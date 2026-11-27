//! `.md` 写状态机（C7/C8）：per-file 代际计数 + 串行写锁 + 未刷盘快照 registry，
//! 以及延迟原子写载体 `PendingMdWrite`。M14 自 mod.rs 拆出。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Prepared atomic `.md` write whose disk flush can be deferred until after DB locks drop.
#[derive(Debug)]
pub struct PendingMdWrite {
    pub(super) path: PathBuf,
    pub(super) content: String,
    /// C8: generation captured while rendering under the DB lock. A flush whose
    /// generation is no longer current is a stale snapshot and is skipped.
    pub(super) generation: u64,
    pub(super) flushed: bool,
}

impl PendingMdWrite {
    /// Atomically write the rendered markdown content to disk.
    ///
    /// C8: the compare-and-write runs under the per-file `.md` serial lock (NOT
    /// the DB lock, so the deferred-flush contract holds). If another writer
    /// committed a newer version in the unlock gap, this snapshot is stale and
    /// is skipped — the newer writer flushes its own snapshot.
    pub fn flush(mut self) -> anyhow::Result<()> {
        let state = md_write_state(&self.path);
        let _serial = crate::util::recover_poison(&state.write_lock);
        let current = state.generation.load(Ordering::Acquire);
        self.flushed = true;
        if current != self.generation {
            clear_unflushed_if_matching(&state, self.generation);
            tracing::info!(
                "skip stale .md snapshot for {} (snapshot generation {}, current {})",
                self.path.display(),
                self.generation,
                current
            );
            return Ok(());
        }
        let content = std::mem::take(&mut self.content);
        let res = crate::util::atomic_write(&self.path, content);
        // W5: 只有写成功才清快照。失败时保留它，read() 仍读到刚提交的内容；
        // 否则 read() 回退旧盘面，下一次 write() 以 stale body 为基把丢失固化。
        if res.is_ok() {
            clear_unflushed_if_matching(&state, self.generation);
        }
        res
    }
}

impl Drop for PendingMdWrite {
    fn drop(&mut self) {
        if !self.flushed {
            let state = md_write_state(&self.path);
            clear_unflushed_if_matching(&state, self.generation);
        }
    }
}

/// C8: per-file generation counter + serial write lock, shared across
/// `BoundedMemory` instances (every request builds a new one) and keyed by the
/// `.md` path so distinct memory dirs / targets never interfere.
#[derive(Default)]
pub(super) struct MdWriteState {
    /// Bumped inside the DB lock by every commit that changes what this file
    /// renders (write/update/remove/reconcile/split/atom-store+evict).
    pub(super) generation: AtomicU64,
    /// Serializes "compare generation + write" against "bump + write" so a
    /// stale snapshot can never land after a newer one.
    pub(super) write_lock: Mutex<()>,
    /// C1:最新的未落盘渲染快照 `(generation, content)`，供解锁刷盘窗口内的
    /// `BoundedMemory::read()` 读取，防止读到旧磁盘文件覆盖刚提交的 L1 原子。
    pub(super) unflushed: Mutex<Option<(u64, String)>>,
}

type MdWriteRegistry = Mutex<std::collections::HashMap<PathBuf, Arc<MdWriteState>>>;

/// C7: REGISTRY 容量上限——超出时驱逐空闲条目（无未刷盘快照且无其他持有者），
/// 防止长期运行下不同 memory_dir/target 路径无限累积。
const MD_WRITE_REGISTRY_CAP: usize = 256;

/// C7: 规范化 registry key，使等价路径（相对/绝对、Windows `\\?\` verbatim 前缀、
/// 大小写别名经 canonicalize）共享同一把锁/代际/快照状态。文件可能尚未创建，
/// canonicalize 失败时回退“规范化父目录 + 文件名”；再失败回退绝对路径。
fn registry_key(path: &Path) -> PathBuf {
    let abs = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    if let Ok(c) = std::fs::canonicalize(&abs) {
        return strip_verbatim_prefix(c);
    }
    if let (Some(parent), Some(name)) = (abs.parent(), abs.file_name()) {
        if let Ok(cp) = std::fs::canonicalize(parent) {
            return strip_verbatim_prefix(cp).join(name);
        }
    }
    abs
}

/// C7: 去掉 Windows canonicalize 返回的 `\\?\` verbatim 前缀（UNC 路径除外），
/// 使规范化结果与 `std::path::absolute` 的普通绝对路径形式一致。非 Windows 无操作。
fn strip_verbatim_prefix(p: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        if let Some(s) = p.to_str() {
            if let Some(rest) = s.strip_prefix(r"\\?\") {
                if !rest.starts_with(r"UNC\") {
                    return PathBuf::from(rest);
                }
            }
        }
    }
    p
}

pub(super) fn md_write_state(path: &Path) -> Arc<MdWriteState> {
    static REGISTRY: std::sync::OnceLock<MdWriteRegistry> = std::sync::OnceLock::new();
    let registry = REGISTRY.get_or_init(|| Mutex::new(std::collections::HashMap::new()));
    let key = registry_key(path);
    let mut map = crate::util::recover_poison(registry);
    // C7: 仅在需要插入新 key 且已达上限时驱逐空闲条目（保留有未刷盘快照或仍被
    // 其他 Arc 持有者使用的状态，避免破坏正在进行的串行写）。
    if map.len() >= MD_WRITE_REGISTRY_CAP && !map.contains_key(&key) {
        map.retain(|_, state| {
            let has_unflushed = crate::util::recover_poison(&state.unflushed).is_some();
            has_unflushed || Arc::strong_count(state) > 1
        });
    }
    map.entry(key).or_default().clone()
}

fn clear_unflushed_if_matching(state: &MdWriteState, generation: u64) {
    let mut guard = crate::util::recover_poison(&state.unflushed);
    if matches!(&*guard, Some((g, _)) if *g == generation) {
        *guard = None;
    }
}

pub(super) fn set_unflushed_snapshot(path: &Path, generation: u64, content: &str) {
    let state = md_write_state(path);
    let mut guard = crate::util::recover_poison(&state.unflushed);
    *guard = Some((generation, content.to_string()));
}

/// Record a committed content change for `path` (call under the DB lock).
pub(super) fn bump_md_generation(path: &Path) {
    md_write_state(path)
        .generation
        .fetch_add(1, Ordering::AcqRel);
}

/// C7: 递增代际并**原子地**记录最新渲染快照 `(gen, content)`（在 DB 锁内、
/// `write_md_serialized` 之前调用）。持有 unflushed 锁跨越 bump，使解锁刷盘窗口
/// 内或写盘失败后的 `read()` 要么看到旧代际+旧快照、要么看到新代际+新快照，
/// 不会回退到陈旧盘面。写成功且代际仍匹配时由 `write_md_serialized` 清除。
pub(super) fn bump_md_generation_with_snapshot(path: &Path, content: &str) {
    let state = md_write_state(path);
    let mut guard = crate::util::recover_poison(&state.unflushed);
    let gen = state.generation.fetch_add(1, Ordering::AcqRel) + 1;
    *guard = Some((gen, content.to_string()));
}

/// Current generation, captured when a snapshot is rendered under the DB lock.
pub(super) fn md_generation(path: &Path) -> u64 {
    md_write_state(path).generation.load(Ordering::Acquire)
}

/// Immediate-write path (write/update/remove): shares the `.md` serial lock with
/// deferred flushes so the two can never interleave.
pub(super) fn write_md_serialized(path: &Path, content: String) -> anyhow::Result<()> {
    let state = md_write_state(path);
    let _serial = crate::util::recover_poison(&state.write_lock);
    let gen_before_write = state.generation.load(Ordering::Acquire);
    let res = crate::util::atomic_write(path, content);
    // W5: 写失败不得清快照（同 PendingMdWrite::flush）。
    // 代际必须在写**之前**取：写后取会把并发写入者刚设的更新快照（g == 新 current）
    // 一并清掉，重新打开 C1 的读间隙。
    if res.is_ok() {
        let mut guard = crate::util::recover_poison(&state.unflushed);
        if matches!(&*guard, Some((g, _)) if *g <= gen_before_write) {
            *guard = None;
        }
    }
    res
}

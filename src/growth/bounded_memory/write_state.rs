//! `.md` 写状态机（C7/C8）：per-file 代际计数 + 串行写锁 + 未刷盘快照 registry，
//! 以及延迟原子写载体 `PendingMdWrite`。M14 自 mod.rs 拆出。

use super::BoundedMemory;
use crate::index::db::Db;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Prepared atomic `.md` write whose disk flush can be deferred until after DB locks drop.
#[derive(Debug)]
pub struct PendingMdWrite {
    pub(super) path: PathBuf,
    pub(super) content: String,
    /// C8: generation captured while rendering under the DB lock. Retained for
    /// the `Drop` mis-clear guard; the deferred flush re-renders current DB state
    /// rather than writing this captured snapshot (F1/F2 freshness).
    pub(super) generation: u64,
    pub(super) flushed: bool,
    /// Render inputs carried so the deferred flush can re-render from the DB
    /// after the DB lock dropped (it no longer writes the captured `content`).
    pub(super) memory_limit: usize,
    pub(super) user_limit: usize,
}

impl PendingMdWrite {
    /// Deferred flush: re-render the CURRENT DB state under the same
    /// `write_lock -> flock` shell as the immediate path, then `atomic_write`.
    ///
    /// Re-rendering (instead of writing the captured snapshot) folds in rows
    /// committed by concurrent writers in the unlock gap (F1/F2): the file
    /// always converges to the live DB set, never a stale snapshot.
    ///
    /// R1: the re-render SELECT is autocommit — no open SQLite WRITE tx is held
    /// when the flock is acquired. R2 exception (gateway flush edge): the shell
    /// takes `write_lock -> flock`, then briefly locks `db` (L_gw) for the SELECT
    /// re-render ONLY; the guard drops BEFORE `atomic_write`, so file IO never
    /// runs under L_gw.
    pub fn flush(mut self, db: &Mutex<Db>) -> anyhow::Result<()> {
        // The shell owns snapshot clearing; mark flushed so `Drop` does not
        // mis-clear the freshly re-rendered snapshot (W5: keep it on write fail).
        self.flushed = true;
        let path = self.path.clone();
        let target = md_target_from_path(&path)?;
        let dir = path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();
        let (memory_limit, user_limit) = (self.memory_limit, self.user_limit);
        write_md_render_locked(&path, || {
            // R2 例外（gateway flush 边）：shell 已持 write_lock+flock，此处短暂取
            // L_gw 仅为 autocommit SELECT 重渲染。rollback_if_in_tx 必需——若上一
            // 持有者 panic 时留有打开的写事务，flock 已在手（R1），必须先回滚残留
            // 事务再做 autocommit SELECT。guard 在闭包返回时 drop（早于 atomic_write）。
            let guard = crate::util::recover_poison(db);
            guard.rollback_if_in_tx();
            let bm = BoundedMemory::new(&dir, &guard, memory_limit, user_limit);
            let (_count, mut pending) = bm.render_md_from_db(target)?;
            pending.flushed = true;
            Ok((pending.generation, std::mem::take(&mut pending.content)))
            // `guard` drops here — BEFORE the shell's atomic_write (R2).
        })
    }
}

/// Reverse of `BoundedMemory::target_file`: infer the target from the `.md` name.
/// The whitelist must stay in sync with `target_file` — only `memory`/`user`
/// targets exist (MEMORY.md / USER.md are the only files this shell renders);
/// any other `.md` name is rejected rather than guessed.
fn md_target_from_path(path: &Path) -> anyhow::Result<&'static str> {
    match path.file_name().and_then(|n| n.to_str()) {
        Some("MEMORY.md") => Ok("memory"),
        Some("USER.md") => Ok("user"),
        _ => anyhow::bail!("cannot infer memory target from {}", path.display()),
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

/// Stable sibling lock path `<file_name>.lck` next to the `.md` target.
///
/// NEVER lock the `.md` target itself: `atomic_write` does temp+rename, swapping
/// the target inode, so a lock on it would not serialize successive writes. The
/// `.lck` sibling is stable across renames. Its name deliberately matches NEITHER
/// the tmp-sweep pattern `.{name}.{pid}.{seq}.tmp` (util/mod.rs) NOR the offload
/// `.offload_*.tmp` shape, so sweepers never delete it. Lock files stay empty and
/// permanent; the OS releases the lock on close/process death (no stale cleanup).
pub(super) fn md_lock_path(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(".lck");
    path.with_file_name(name)
}

/// Acquire the cross-process exclusive `.md` flock (blocking).
///
/// R1: NEVER call this while holding an OPEN SQLite WRITE transaction — all flock
/// acquisitions happen after commit / outside a write tx (the re-render SELECT is
/// autocommit). R2: NEVER acquire this flock while holding L_gw (the gateway
/// `Arc<Mutex<Db>>`); the only permitted cross-edge is the flush shell below
/// (`write_lock -> flock -> brief L_gw autocommit SELECT`, guard dropped BEFORE
/// file IO).
///
/// Uses `std::fs::File::lock` (stable 1.89): Unix `flock(LOCK_EX)`, Windows
/// `LockFileEx(LOCKFILE_EXCLUSIVE_LOCK)`. Open `.read(true).write(true).create(true)`
/// (plain create, NOT create_new; Windows requires read or write access to lock).
/// A FRESH handle is opened per critical section — never reuse a locked handle
/// (re-locking a held handle is unspecified / may deadlock). The lock auto-releases
/// when the returned `File` drops.
pub(super) fn lock_md_file(path: &Path) -> std::io::Result<std::fs::File> {
    let lock_path = md_lock_path(path);
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)?;
    file.lock()?;
    Ok(file)
}

/// `.md` render-write shell shared by the immediate and deferred-flush paths.
///
/// Lock order (R2): in-process `write_lock` FIRST, then the cross-process flock,
/// then the caller's DB SELECT `render`, then `atomic_write` — the file IO runs
/// under both locks but NEVER under L_gw (the render closure must drop any DB
/// guard before returning). `render` returns `(generation, content)`; on a
/// successful write the matching unflushed snapshot is cleared (W5: on failure
/// the snapshot is kept so `read()` still sees the committed content).
///
/// Lock degradation (perf C3): if the `.lck` open/lock fails, warn and continue
/// render+`atomic_write` WITHOUT the flock — the floor is today's in-process C8
/// serialization (exotic filesystems stay writable; WAL already forbids network
/// FS). `WouldBlock` is normal contention and never escapes the blocking
/// `File::lock()`; only genuine errors degrade.
pub(super) fn write_md_render_locked(
    path: &Path,
    render: impl FnOnce() -> anyhow::Result<(u64, String)>,
) -> anyhow::Result<()> {
    let state = md_write_state(path);
    let _serial = crate::util::recover_poison(&state.write_lock);
    let _flock = match lock_md_file(path) {
        Ok(f) => Some(f),
        Err(e) => {
            tracing::warn!(
                ".md flock degraded for {} ({}); rendering under in-process write_lock only",
                path.display(),
                e
            );
            None
        }
    };
    let (generation, content) = render()?;
    let res = crate::util::atomic_write(path, content);
    if res.is_ok() {
        clear_unflushed_if_matching(&state, generation);
    }
    res
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

/// Current generation, captured when a snapshot is rendered under the DB lock.
pub(super) fn md_generation(path: &Path) -> u64 {
    md_write_state(path).generation.load(Ordering::Acquire)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn tmp_dir(tag: &str) -> PathBuf {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "asuna_mdlock_{}_{}_{}_{}",
            tag,
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0),
            seq
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn open_lck(md: &Path) -> std::fs::File {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(md_lock_path(md))
            .unwrap()
    }

    fn would_block(f: &std::fs::File) -> bool {
        matches!(f.try_lock(), Err(std::fs::TryLockError::WouldBlock))
    }

    /// `.lck` is a stable sibling of the `.md` target (never the target itself),
    /// and `lock_md_file` opens read+write+create (Windows lock requirement) and
    /// acquires the exclusive lock.
    #[test]
    fn lock_md_file_uses_stable_sibling_lck() {
        let dir = tmp_dir("sibling");
        let md = dir.join("MEMORY.md");
        assert_eq!(md_lock_path(&md), dir.join("MEMORY.md.lck"));
        let holder = lock_md_file(&md).unwrap();
        assert!(dir.join("MEMORY.md.lck").exists());
        assert!(!md.exists(), "locking must not create the .md target");
        assert!(
            would_block(&open_lck(&md)),
            "second handle must be excluded"
        );
        drop(holder);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Two threads opening SEPARATE handles to the same `.lck` mutually exclude:
    /// the contender's `try_lock` returns `WouldBlock` while the holder lives, and
    /// succeeds after the holder drops (same-process separate handles conflict —
    /// a valid proxy for cross-process flock).
    #[test]
    fn try_lock_mutual_exclusion_across_threads() {
        let dir = tmp_dir("threads");
        let md = dir.join("MEMORY.md");
        let (tx, rx) = std::sync::mpsc::channel();
        let md2 = md.clone();
        let holder = std::thread::spawn(move || {
            let f = lock_md_file(&md2).unwrap();
            tx.send(()).unwrap();
            std::thread::sleep(Duration::from_millis(200));
            drop(f);
        });
        rx.recv().unwrap();
        let contender = open_lck(&md);
        assert!(
            would_block(&contender),
            "contender must block while holder lives"
        );
        holder.join().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match contender.try_lock() {
                Ok(()) => break,
                Err(std::fs::TryLockError::WouldBlock) => {
                    assert!(
                        Instant::now() < deadline,
                        "lock not released after holder drop"
                    );
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(e) => panic!("unexpected try_lock error: {e:?}"),
            }
        }
        drop(contender);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// `atomic_write` (temp+rename) swaps the `.md` inode but the `.lck` sibling is
    /// stable: it survives the rename and the original lock still serializes writes.
    #[test]
    fn atomic_write_rename_keeps_lck_and_lock() {
        let dir = tmp_dir("rename");
        let md = dir.join("MEMORY.md");
        let holder = lock_md_file(&md).unwrap();
        let lck = md_lock_path(&md);
        crate::util::atomic_write(&md, b"v1").unwrap();
        assert!(lck.exists(), ".lck must survive the .md rename");
        assert!(
            would_block(&open_lck(&md)),
            "lock must still serialize after rename"
        );
        crate::util::atomic_write(&md, b"v2").unwrap();
        assert_eq!(std::fs::read(&md).unwrap(), b"v2");
        assert!(lck.exists());
        drop(holder);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The tmp sweeper must NOT delete `.lck` files (name matches neither the
    /// `.{name}.{pid}.{seq}.tmp` shape nor offload `.offload_*.tmp`).
    #[test]
    fn sweep_does_not_delete_lck() {
        let dir = tmp_dir("sweep");
        let md = dir.join("MEMORY.md");
        drop(lock_md_file(&md).unwrap());
        let lck = md_lock_path(&md);
        assert!(lck.exists());
        // An orphan atomic-write tmp file that the sweeper SHOULD remove.
        let orphan = dir.join(".MEMORY.md.999999.0.tmp");
        std::fs::write(&orphan, b"stale").unwrap();
        let removed = crate::util::sweep_orphan_tmp_files(&dir, Duration::ZERO);
        assert_eq!(removed, 1, "orphan tmp must be swept");
        assert!(lck.exists(), ".lck must survive the sweep");
        assert!(!orphan.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Killing a holder PROCESS auto-releases the flock (no stale lock): the OS
    /// closes the dead process's handles, releasing `flock`/`LockFileEx`. A
    /// contender acquires with deadline retry after the kill.
    #[test]
    fn kill_holder_process_auto_releases_lock() {
        let dir = tmp_dir("kill");
        let exe = std::env::current_exe().unwrap();
        let mut child = std::process::Command::new(exe)
            .args(["md_lock_holder_helper", "--ignored", "--test-threads=1"])
            .env("AMS_MD_LOCK_TEST_DIR", &dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let ready = dir.join("ready");
        let deadline = Instant::now() + Duration::from_secs(20);
        while !ready.exists() {
            if Instant::now() > deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("holder process did not become ready");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let md = dir.join("MEMORY.md");
        let contender = open_lck(&md);
        assert!(would_block(&contender), "child process must hold the lock");
        child.kill().unwrap();
        child.wait().unwrap();
        drop(contender);
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let f = open_lck(&md);
            match f.try_lock() {
                Ok(()) => {
                    drop(f);
                    break;
                }
                Err(std::fs::TryLockError::WouldBlock) => {
                    assert!(
                        Instant::now() < deadline,
                        "lock not released after process death"
                    );
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(e) => panic!("unexpected try_lock error: {e:?}"),
            }
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Helper run as a subprocess by `kill_holder_process_auto_releases_lock`:
    /// acquire the `.lck` flock, signal readiness, then hold until killed.
    #[test]
    #[ignore]
    fn md_lock_holder_helper() {
        let dir = PathBuf::from(std::env::var("AMS_MD_LOCK_TEST_DIR").unwrap());
        let _f = lock_md_file(&dir.join("MEMORY.md")).unwrap();
        std::fs::write(dir.join("ready"), b"1").unwrap();
        std::thread::sleep(Duration::from_secs(60));
    }
}

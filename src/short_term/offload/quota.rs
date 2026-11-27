//! refs/ 增长配额（J29）：每任务步数帽 + 跨任务总字节帽与淘汰。
//! M14 自 offload.rs 拆出；NodeId / 校验 / 原子分配 / recall 仍在 offload.rs。

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU64;
use std::time::SystemTime;

/// Per-task step-file cap under `refs/{task_id}/` (J29).
///
/// 200 is sized for a personal agent's working memory: one offload per long
/// tool output, so a normal task consumes well under 10 steps; the cap exists
/// to bound disk use by a client that loops on a single `task_id`. Oldest
/// steps (smallest step number) are evicted beyond it.
pub(super) const MAX_STEPS_PER_TASK: usize = 200;

/// Total byte cap across all `refs/**/*.md` step files (J29).
///
/// 100MB ≈ the whole-disk working set of a personal agent (roughly ten 10MB
/// offloads, far past typical usage). When exceeded, step files are evicted
/// oldest-mtime-first until back under the cap.
pub(super) const MAX_REFS_TOTAL_BYTES: u64 = 100 * 1024 * 1024;

/// Run the refs-wide byte scan only every Nth offload (W28d).
///
/// `evict_refs_total` stats every step file under `refs/`, which is O(all
/// files) on every write. The per-task cap still runs each time; between
/// scans the total may overshoot the byte cap by up to N-1 payloads —
/// acceptable for a best-effort quota.
pub(super) const REFS_TOTAL_SCAN_EVERY_N: u64 = 8;

/// Offload counter driving `REFS_TOTAL_SCAN_EVERY_N`.
pub(super) static OFFLOAD_SCAN_TICK: AtomicU64 = AtomicU64::new(0);

/// Enforce the refs/ growth caps (J29): per-task step count and total bytes.
/// Returns the number of files evicted. Eviction is oldest-first and
/// deterministic: within a task by step number, across tasks by file mtime
/// (path as tie-break). Under concurrent allocation the step order can lag
/// write-completion order by milliseconds (a create_new race loser takes the
/// next number but may finish writing first) — acceptable for a best-effort
/// quota.
///
/// `protect` is the file the caller just wrote; it is never evicted (it still
/// counts toward the byte total). Without this, a single payload larger than
/// the total cap could be evicted immediately after a successful write,
/// returning a dangling node_id. (Unreachable via the gateway's 10MB body
/// limit vs the 100MB cap, but the invariant is cheap to hold.)
///
/// `scan_refs_total` gates the refs-wide byte scan (W28d throttle); the
/// per-task step cap is always enforced.
pub(super) fn enforce_offload_quota(
    task_dir: &Path,
    protect: &Path,
    max_files: usize,
    max_bytes: u64,
    scan_refs_total: bool,
) -> anyhow::Result<usize> {
    let mut removed = evict_task_dir(task_dir, Some(protect), max_files)?;
    if scan_refs_total {
        if let Some(refs_dir) = task_dir.parent() {
            removed += evict_refs_total(refs_dir, Some(protect), max_bytes)?;
        }
    }
    Ok(removed)
}

pub(super) fn parse_step_filename(name: &str) -> Option<usize> {
    name.strip_prefix("step_")?
        .strip_suffix(".md")?
        .parse()
        .ok()
}

const STALE_OFFLOAD_TMP_AGE: std::time::Duration = std::time::Duration::from_secs(300);

fn sweep_if_stale_offload_tmp(path: &Path, name: &str) {
    if !(name.starts_with(".offload_") && name.ends_with(".tmp")) {
        return;
    }
    let Ok(md) = std::fs::symlink_metadata(path) else {
        return;
    };
    if !md.is_file() {
        return;
    }
    let Ok(modified) = md.modified() else {
        return;
    };
    if let Ok(age) = SystemTime::now().duration_since(modified) {
        if age >= STALE_OFFLOAD_TMP_AGE {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Evict `step_*.md` files beyond `max_files` from one task dir, oldest step
/// number first. The `protect`ed path COUNTS toward the cap but is never
/// evicted (if every remaining candidate is protected the dir may stay above
/// the cap — best-effort by design).
pub(super) fn evict_task_dir(
    task_dir: &Path,
    protect: Option<&Path>,
    max_files: usize,
) -> anyhow::Result<usize> {
    let mut steps: Vec<(usize, PathBuf)> = Vec::new();
    for entry in std::fs::read_dir(task_dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let entry_path = entry.path();
        if let Some(step) = parse_step_filename(&name) {
            steps.push((step, entry_path));
        } else {
            sweep_if_stale_offload_tmp(&entry_path, &name);
        }
    }
    if steps.len() <= max_files {
        return Ok(0);
    }
    steps.sort_unstable();
    let mut excess = steps.len() - max_files;
    let mut removed = 0usize;
    for (_, path) in steps {
        if excess == 0 {
            break;
        }
        if protect == Some(path.as_path()) {
            continue;
        }
        match std::fs::remove_file(&path) {
            Ok(()) => {
                removed += 1;
                excess -= 1;
                tracing::info!(
                    "offload quota: evicted {} (task dir > {} step files)",
                    path.display(),
                    max_files
                );
            }
            // Concurrent eviction may have removed it already.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                excess -= 1;
            }
            Err(e) => {
                tracing::warn!(
                    "offload quota: failed to evict {} (continuing cleanup): {}",
                    path.display(),
                    e
                );
                // W14: the file is still there — do NOT consume the eviction
                // budget, or an undeletable oldest step stops eviction early
                // and the per-task cap silently lapses. Keep going to the
                // next (newer) candidate instead.
            }
        }
    }
    Ok(removed)
}

pub(super) fn collect_task_step_files(
    task_dir: &Path,
    protect: Option<&Path>,
    files: &mut Vec<(SystemTime, u64, PathBuf)>,
    protected_size: &mut u64,
) -> anyhow::Result<()> {
    let sub = match std::fs::read_dir(task_dir) {
        Ok(sub) => sub,
        Err(e) => {
            // 维度7 / W14: 不可读任务目录的字节无法计入总配额——静默跳过会低估
            // refs 总量、让配额永不收敛。至少 warn 使运维可见（保守计入需已知
            // 大小，不可读时取不到，故 warn 而非猜测）。
            tracing::warn!(
                "offload quota: cannot read task dir {} (its bytes are excluded from the refs total): {}",
                task_dir.display(),
                e
            );
            return Ok(());
        }
    };
    for f in sub {
        let f = f?;
        let Ok(ft) = f.file_type() else { continue };
        if ft.is_symlink() || !ft.is_file() {
            continue;
        }
        let name = f.file_name();
        let name_str = name.to_string_lossy();
        // 维度7: 与 evict_task_dir/find_next_step 统一用 parse_step_filename（严格
        // 数字）——原 prefix/suffix 宽松判据会把 "step_abc.md" 计入字节配额却不计
        // 步数配额，两套账目漂移。
        if parse_step_filename(&name_str).is_none() {
            sweep_if_stale_offload_tmp(&f.path(), &name_str);
            continue;
        }
        let path = f.path();
        let Ok(fm) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if !fm.is_file() {
            continue;
        }
        if protect == Some(path.as_path()) {
            *protected_size += fm.len();
            continue;
        }
        files.push((
            fm.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            fm.len(),
            path,
        ));
    }
    Ok(())
}

type StepFileInventory = (Vec<(SystemTime, u64, PathBuf)>, u64);

fn collect_all_refs_step_files(
    refs_dir: &Path,
    protect: Option<&Path>,
) -> anyhow::Result<StepFileInventory> {
    let mut files: Vec<(SystemTime, u64, PathBuf)> = Vec::new();
    let mut protected_size: u64 = 0;
    for entry in std::fs::read_dir(refs_dir)? {
        let entry = entry?;
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_symlink() || !ft.is_dir() {
            continue;
        }
        collect_task_step_files(&entry.path(), protect, &mut files, &mut protected_size)?;
    }
    Ok((files, protected_size))
}

fn try_remove_ref_file(
    path: &Path,
    size: u64,
    max_bytes: u64,
    total: &mut u64,
    touched_dirs: &mut std::collections::HashSet<PathBuf>,
) -> bool {
    match std::fs::remove_file(path) {
        Ok(()) => {
            *total = total.saturating_sub(size);
            if let Some(parent) = path.parent() {
                touched_dirs.insert(parent.to_path_buf());
            }
            tracing::info!(
                "offload quota: evicted {} ({} bytes; refs total still > cap {})",
                path.display(),
                size,
                max_bytes
            );
            true
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            *total = total.saturating_sub(size);
            if let Some(parent) = path.parent() {
                touched_dirs.insert(parent.to_path_buf());
            }
            false
        }
        Err(e) => {
            tracing::warn!(
                "offload quota: failed to evict {} (continuing cleanup): {}",
                path.display(),
                e
            );
            false
        }
    }
}

/// Evict `step_*.md` files across all task dirs until total size ≤ `max_bytes`,
/// oldest mtime first. `protect`ed paths count toward the total but are never
/// evicted.
pub(super) fn evict_refs_total(
    refs_dir: &Path,
    protect: Option<&Path>,
    max_bytes: u64,
) -> anyhow::Result<usize> {
    let (mut files, protected_size) = collect_all_refs_step_files(refs_dir, protect)?;
    let mut total: u64 = protected_size + files.iter().map(|(_, s, _)| *s).sum::<u64>();
    if total <= max_bytes {
        return Ok(0);
    }
    // Oldest first; path tie-break makes ordering deterministic when mtimes collide.
    files.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.2.cmp(&b.2)));
    let mut removed = 0usize;
    let mut touched_dirs = std::collections::HashSet::new();
    for (_, size, path) in files {
        if total <= max_bytes {
            break;
        }
        if try_remove_ref_file(&path, size, max_bytes, &mut total, &mut touched_dirs) {
            removed += 1;
        }
    }
    for dir in touched_dirs {
        // Best-effort prune of now-empty task dirs (remove_dir fails safely if non-empty).
        let _ = std::fs::remove_dir(dir);
    }
    Ok(removed)
}

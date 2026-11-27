//! Context offload: store long text to refs/ directory
//!
//! Returns a node_id that can be used to recall the text later.
//! This reduces context window usage for long tool outputs.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

mod quota;

use quota::{
    enforce_offload_quota, parse_step_filename, MAX_REFS_TOTAL_BYTES, MAX_STEPS_PER_TASK,
    OFFLOAD_SCAN_TICK, REFS_TOTAL_SCAN_EVERY_N,
};
// offload_tests.rs reaches the eviction helpers via `use super::*`.
#[cfg(test)]
use quota::{collect_task_step_files, evict_refs_total, evict_task_dir};

/// Process-wide temp-name sequence (C6).
///
/// `pid + nanos` alone collides when two offloads land on the same clock tick
/// (coarse Windows granularity), and `create_new` then fails the whole call
/// with AlreadyExists. The sequence keeps every temp name unique regardless
/// of the clock — same pattern as `util::atomic_write`'s SEQ.
static OFFLOAD_TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Bound on step-number probe attempts during atomic allocation.
///
/// `find_next_step` starts at max+1, so retries only happen when concurrent
/// calls race for the same slot; 1000 consecutive collisions is a clear
/// anomaly and errors out instead of spinning.
const MAX_STEP_ALLOC_ATTEMPTS: usize = 1000;

/// Unique identifier for offloaded content
///
/// Format: "{task_id}/step_{n}"
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeId {
    pub task_id: String,
    pub step: usize,
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/step_{}", self.task_id, self.step)
    }
}

impl NodeId {
    pub fn new(task_id: String, step: usize) -> Self {
        Self { task_id, step }
    }

    /// Parse node_id from string format "{task_id}/step_{n}"
    pub fn parse(s: &str) -> Option<Self> {
        let parts: Vec<&str> = s.rsplitn(2, "/step_").collect();
        if parts.len() != 2 {
            return None;
        }
        let step: usize = parts[0].parse().ok()?;
        let task_id = parts[1].to_string();
        validate_task_id(&task_id).ok()?;
        Some(Self { task_id, step })
    }

    /// Get file path relative to refs_dir
    pub fn file_path(&self, refs_dir: &Path) -> PathBuf {
        refs_dir
            .join(&self.task_id)
            .join(format!("step_{}.md", self.step))
    }
}

/// Invalid `task_id` rejected by validation so HTTP callers can return 400.
#[derive(Debug)]
pub struct InvalidTaskId(pub String);

impl fmt::Display for InvalidTaskId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for InvalidTaskId {}

/// Requested offload node does not exist on disk so HTTP callers can return 404.
#[derive(Debug)]
pub struct NodeNotFound(pub String);

impl fmt::Display for NodeNotFound {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Node not found: {}", self.0)
    }
}

impl std::error::Error for NodeNotFound {}

/// Validate task_id to prevent path traversal and Windows device-name attacks.
///
/// Only allows ASCII alphanumeric characters, underscores, and hyphens (1..=128).
fn validate_task_id(task_id: &str) -> anyhow::Result<()> {
    if task_id.is_empty() {
        return Err(anyhow::Error::new(InvalidTaskId(
            "task_id cannot be empty".to_string(),
        )));
    }
    if task_id.len() > 128 {
        return Err(anyhow::Error::new(InvalidTaskId(
            "task_id too long (max 128 characters)".to_string(),
        )));
    }
    if !task_id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(anyhow::Error::new(InvalidTaskId(
            "task_id contains invalid characters (only ASCII alphanumeric, '_', '-' allowed)"
                .to_string(),
        )));
    }
    if crate::util::is_windows_reserved_device_name(task_id) {
        return Err(anyhow::Error::new(InvalidTaskId(
            "task_id cannot be a reserved device name".to_string(),
        )));
    }
    Ok(())
}

/// Content rejected by the security scan (U10). `offload_text` fails with
/// this typed error so HTTP callers can answer 400 instead of 500; the
/// payload is the scan reason.
#[derive(Debug)]
pub struct ScanRejected(pub String);

impl fmt::Display for ScanRejected {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "content rejected by security scan: {}", self.0)
    }
}

impl std::error::Error for ScanRejected {}

/// Unique sibling temp name for an offload payload (see `OFFLOAD_TEMP_SEQ`).
fn offload_temp_name() -> String {
    let seq = OFFLOAD_TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
    format!(
        ".offload_{}_{}_{}.tmp",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0),
        seq
    )
}

fn write_offload_temp_file(task_dir: &Path, content: &str) -> anyhow::Result<PathBuf> {
    let temp_path = task_dir.join(offload_temp_name());
    let mut tmp_file = match std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temp_path)
    {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(task_dir)?;
            std::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&temp_path)?
        }
        Err(e) => return Err(e.into()),
    };
    if let Err(e) = tmp_file
        .write_all(content.as_bytes())
        .and_then(|_| tmp_file.flush())
    {
        drop(tmp_file);
        let _ = std::fs::remove_file(&temp_path);
        return Err(e.into());
    }
    Ok(temp_path)
}

enum StepBindOutcome {
    Claimed,
    AdvanceStep,
}

fn try_create_step_fallback(
    task_dir: &Path,
    candidate: &Path,
    temp_path: &Path,
    content: &str,
) -> anyhow::Result<StepBindOutcome> {
    match std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(candidate)
    {
        Ok(mut f) => {
            let write_result = f.write_all(content.as_bytes()).and_then(|_| f.flush());
            drop(f);
            if let Err(we) = write_result {
                let _ = std::fs::remove_file(candidate);
                let _ = std::fs::remove_file(temp_path);
                return Err(we.into());
            }
            Ok(StepBindOutcome::Claimed)
        }
        Err(ce) if ce.kind() == std::io::ErrorKind::AlreadyExists => {
            Ok(StepBindOutcome::AdvanceStep)
        }
        Err(ce) if ce.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(task_dir)?;
            Ok(StepBindOutcome::AdvanceStep)
        }
        Err(ce) => {
            let _ = std::fs::remove_file(temp_path);
            Err(ce.into())
        }
    }
}

enum StepBindResult {
    Claimed(usize),
    Retry(usize),
}

fn ensure_temp_file_present(
    task_dir: &Path,
    temp_path: &Path,
    content: &str,
) -> anyhow::Result<()> {
    std::fs::create_dir_all(task_dir)?;
    if !temp_path.exists() {
        if let Err(we) = std::fs::write(temp_path, content.as_bytes()) {
            let _ = std::fs::remove_file(temp_path);
            return Err(we.into());
        }
    }
    Ok(())
}

fn try_bind_step_once(
    refs_dir: &Path,
    task_dir: &Path,
    temp_path: &Path,
    task_id: &str,
    step: usize,
    content: &str,
) -> anyhow::Result<StepBindResult> {
    let candidate = NodeId::new(task_id.to_string(), step).file_path(refs_dir);
    match std::fs::hard_link(temp_path, &candidate) {
        Ok(()) => Ok(StepBindResult::Claimed(step)),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            Ok(StepBindResult::Retry(step.saturating_add(1)))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            ensure_temp_file_present(task_dir, temp_path, content)?;
            Ok(StepBindResult::Retry(step))
        }
        Err(_) => match try_create_step_fallback(task_dir, &candidate, temp_path, content)? {
            StepBindOutcome::Claimed => Ok(StepBindResult::Claimed(step)),
            StepBindOutcome::AdvanceStep => Ok(StepBindResult::Retry(step.saturating_add(1))),
        },
    }
}

fn bind_offload_step(
    refs_dir: &Path,
    task_dir: &Path,
    temp_path: &Path,
    task_id: &str,
    content: &str,
) -> anyhow::Result<usize> {
    let mut step = find_next_step(task_dir)?;
    for _ in 0..MAX_STEP_ALLOC_ATTEMPTS {
        match try_bind_step_once(refs_dir, task_dir, temp_path, task_id, step, content)? {
            StepBindResult::Claimed(claimed) => return Ok(claimed),
            StepBindResult::Retry(next_step) => step = next_step,
        }
    }
    anyhow::bail!(
        "offload: no free step slot for task {} after {} allocation attempts",
        task_id,
        MAX_STEP_ALLOC_ATTEMPTS
    )
}

/// Offload large text content to external storage.
///
/// Writes to `refs_dir/{task_id}/step_{n}.md` where `n` is incremented monotonically.
/// The content is scanned *before* touching the filesystem so rejected text never
/// creates orphan directories or files.
///
/// # Security
///
/// - `task_id` is validated to prevent path traversal (alphanumeric, `_`, `-` only).
/// - U10: `content` is recalled verbatim via `/recall/:node_id`, so text tripping
///   the injection/credential scan is rejected outright (same hard-reject policy as
///   manual memory writes).
///
/// # Concurrency
///
/// Thread-safe for concurrent calls racing on the same `task_id`: the step slot
/// is claimed atomically via `hard_link` / `create_new` (`O_EXCL` / Windows
/// `CREATE_NEW`), so colliding callers advance to free slots without overwriting
/// existing content.
///
/// Returns the `NodeId` that can be used to recall the text.
pub fn offload_text(refs_dir: &Path, task_id: &str, content: &str) -> anyhow::Result<NodeId> {
    validate_task_id(task_id)?;

    let scan = crate::growth::security::scan_content(content);
    if !scan.is_safe() {
        return Err(anyhow::Error::new(ScanRejected(scan.reason())));
    }

    std::fs::create_dir_all(refs_dir)?;
    let task_dir = refs_dir.join(task_id);
    match std::fs::symlink_metadata(&task_dir) {
        Ok(md) if md.is_dir() => {}
        Ok(_) => anyhow::bail!("offload task_dir is not a real directory (symlink or file)"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            match std::fs::create_dir(&task_dir) {
                Ok(()) => {}
                Err(create_err) if create_err.kind() == std::io::ErrorKind::AlreadyExists => {
                    let md = std::fs::symlink_metadata(&task_dir)?;
                    if !md.is_dir() {
                        anyhow::bail!("offload task_dir is not a real directory (symlink or file)");
                    }
                }
                Err(create_err) => return Err(create_err.into()),
            }
        }
        Err(e) => return Err(e.into()),
    }
    if !crate::util::is_contained_in_dir(refs_dir, &task_dir) {
        anyhow::bail!("offload task_dir escapes refs_dir");
    }

    let temp_path = write_offload_temp_file(&task_dir, content)?;
    let step_res = bind_offload_step(refs_dir, &task_dir, &temp_path, task_id, content);
    let _ = std::fs::remove_file(&temp_path);
    let step = step_res?;
    let claimed_path = NodeId::new(task_id.to_string(), step).file_path(refs_dir);

    tracing::debug!(
        "Offloaded {} bytes to {}/step_{}.md",
        content.len(),
        task_id,
        step
    );

    // J29 quota: best-effort eviction after a successful write — a cleanup
    // failure must not fail the offload whose content is already durable.
    // The just-written file is protected from eviction (see enforce_offload_quota).
    let scan_refs_total = OFFLOAD_SCAN_TICK
        .fetch_add(1, Ordering::Relaxed)
        .is_multiple_of(REFS_TOTAL_SCAN_EVERY_N);
    if let Err(e) = enforce_offload_quota(
        &task_dir,
        &claimed_path,
        MAX_STEPS_PER_TASK,
        MAX_REFS_TOTAL_BYTES,
        scan_refs_total,
    ) {
        tracing::warn!("offload quota enforcement failed: {}", e);
    }

    Ok(NodeId::new(task_id.to_string(), step))
}

/// Recall text content by node_id
///
/// Reads from `refs_dir/{task_id}/step_{n}.md`
///
/// # Security
///
/// Validates `node_id.task_id` to prevent path traversal attacks.
pub fn recall_text(refs_dir: &Path, node_id: &NodeId) -> anyhow::Result<String> {
    validate_task_id(&node_id.task_id)?;

    let task_dir = refs_dir.join(&node_id.task_id);
    match std::fs::symlink_metadata(&task_dir) {
        Ok(md) if md.is_dir() => {}
        Ok(_) => return Err(anyhow::Error::new(NodeNotFound(node_id.to_string()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(anyhow::Error::new(NodeNotFound(node_id.to_string())));
        }
        Err(e) => return Err(e.into()),
    }

    let file_path = node_id.file_path(refs_dir);
    // 维度4: read_to_string 会跟随符号链接——本地攻击者把 step_n.md 换成指向
    // refs 外文件的 symlink 即可越权读取。先用 symlink_metadata（不跟随链接）
    // 确认目标是普通文件，symlink/目录等一律按 NodeNotFound 拒。远程不可达，
    // 纵深防御（hard_link 写出的仍是普通文件，正常 recall 不受影响）。
    match std::fs::symlink_metadata(&file_path) {
        Ok(md) if md.is_file() => {}
        Ok(_) => return Err(anyhow::Error::new(NodeNotFound(node_id.to_string()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(anyhow::Error::new(NodeNotFound(node_id.to_string())));
        }
        Err(e) => return Err(e.into()),
    }
    if !crate::util::is_contained_in_dir(refs_dir, &file_path) {
        return Err(anyhow::Error::new(NodeNotFound(node_id.to_string())));
    }
    match std::fs::read_to_string(&file_path) {
        Ok(content) => Ok(content),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Err(anyhow::Error::new(NodeNotFound(node_id.to_string())))
        }
        Err(e) => Err(e.into()),
    }
}

/// Find the next sequential step number in a task directory
fn find_next_step(task_dir: &Path) -> anyhow::Result<usize> {
    let mut max_step = 0;

    for entry in std::fs::read_dir(task_dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();

        if let Some(step) = parse_step_filename(&name) {
            max_step = max_step.max(step);
        }
    }

    max_step
        .checked_add(1)
        .ok_or_else(|| anyhow::anyhow!("offload step counter overflow"))
}

#[cfg(test)]
#[path = "offload_tests.rs"]
mod tests;

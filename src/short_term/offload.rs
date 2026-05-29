//! Context offload: store long text to refs/ directory
//!
//! Returns a node_id that can be used to recall the text later.
//! This reduces context window usage for long tool outputs.

use std::path::{Path, PathBuf};
use serde::{Deserialize, Serialize};
use std::fmt;

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
        Some(Self { task_id, step })
    }

    /// Get file path relative to refs_dir
    pub fn file_path(&self, refs_dir: &Path) -> PathBuf {
        refs_dir.join(&self.task_id).join(format!("step_{}.md", self.step))
    }
}

/// Validate task_id to prevent path traversal attacks
///
/// Only allows alphanumeric characters, underscores, and hyphens.
/// Rejects empty strings, "..", "/", and other path separators.
fn validate_task_id(task_id: &str) -> anyhow::Result<()> {
    if task_id.is_empty() {
        anyhow::bail!("task_id cannot be empty");
    }
    if task_id.len() > 255 {
        anyhow::bail!("task_id too long (max 255 characters)");
    }
    if !task_id
        .chars()
        .all(|c| c.is_alphanumeric() || c == '_' || c == '-')
    {
        anyhow::bail!("task_id contains invalid characters (only alphanumeric, '_', '-' allowed)");
    }
    Ok(())
}

/// Offload text content to refs directory
///
/// Stores content to `refs_dir/{task_id}/step_{n}.md` where n is
/// the next sequential step number for this task_id.
///
/// # Security
///
/// `task_id` is validated to prevent path traversal attacks.
/// Only alphanumeric characters, underscores, and hyphens are allowed.
///
/// # Concurrency
///
/// This function is not thread-safe. Concurrent calls with the same
/// `task_id` may result in step number collisions. For concurrent use,
/// serialize calls per task_id or use external locking.
///
/// Returns the node_id that can be used to recall the text.
pub fn offload_text(
    refs_dir: &Path,
    task_id: &str,
    content: &str,
) -> anyhow::Result<NodeId> {
    validate_task_id(task_id)?;

    let task_dir = refs_dir.join(task_id);
    std::fs::create_dir_all(&task_dir)?;

    // Find next step number by scanning existing files
    let step = find_next_step(&task_dir)?;

    let node_id = NodeId::new(task_id.to_string(), step);
    let file_path = node_id.file_path(refs_dir);

    std::fs::write(&file_path, content)?;

    tracing::debug!(
        "Offloaded {} bytes to {}",
        content.len(),
        file_path.display()
    );

    Ok(node_id)
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

    let file_path = node_id.file_path(refs_dir);

    if !file_path.exists() {
        anyhow::bail!("Node not found: {}", node_id);
    }

    let content = std::fs::read_to_string(&file_path)?;
    Ok(content)
}

/// Find the next sequential step number in a task directory
fn find_next_step(task_dir: &Path) -> anyhow::Result<usize> {
    let mut max_step = 0;

    for entry in std::fs::read_dir(task_dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();

        if name.starts_with("step_") && name.ends_with(".md") {
            let step_str = &name[5..name.len() - 3]; // Remove "step_" and ".md"
            if let Ok(step) = step_str.parse::<usize>() {
                max_step = max_step.max(step);
            }
        }
    }

    Ok(max_step + 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_node_id_parse_and_format() {
        let node = NodeId::parse("task_001/step_3").unwrap();
        assert_eq!(node.task_id, "task_001");
        assert_eq!(node.step, 3);
        assert_eq!(node.to_string(), "task_001/step_3");
    }

    #[test]
    fn test_node_id_parse_invalid() {
        assert!(NodeId::parse("invalid").is_none());
        assert!(NodeId::parse("task/step_abc").is_none());
    }

    #[test]
    fn test_offload_and_recall() {
        let tmp = TempDir::new().unwrap();
        let refs_dir = tmp.path();

        let content = "This is a long tool output that should be offloaded";
        let node_id = offload_text(refs_dir, "task_001", content).unwrap();

        assert_eq!(node_id.task_id, "task_001");
        assert_eq!(node_id.step, 1);

        let recalled = recall_text(refs_dir, &node_id).unwrap();
        assert_eq!(recalled, content);
    }

    #[test]
    fn test_offload_sequential_steps() {
        let tmp = TempDir::new().unwrap();
        let refs_dir = tmp.path();

        let node1 = offload_text(refs_dir, "task_001", "step 1").unwrap();
        let node2 = offload_text(refs_dir, "task_001", "step 2").unwrap();
        let node3 = offload_text(refs_dir, "task_001", "step 3").unwrap();

        assert_eq!(node1.step, 1);
        assert_eq!(node2.step, 2);
        assert_eq!(node3.step, 3);
    }

    #[test]
    fn test_recall_nonexistent() {
        let tmp = TempDir::new().unwrap();
        let refs_dir = tmp.path();

        let node = NodeId::new("task_999".to_string(), 1);
        assert!(recall_text(refs_dir, &node).is_err());
    }

    #[test]
    fn test_validate_task_id_valid() {
        // Valid task IDs
        assert!(validate_task_id("task_001").is_ok());
        assert!(validate_task_id("my-task").is_ok());
        assert!(validate_task_id("Task123").is_ok());
        assert!(validate_task_id("a").is_ok());
        assert!(validate_task_id("task_with-mixed_chars123").is_ok());
    }

    #[test]
    fn test_validate_task_id_empty() {
        assert!(validate_task_id("").is_err());
    }

    #[test]
    fn test_validate_task_id_too_long() {
        let long_id = "a".repeat(256);
        assert!(validate_task_id(&long_id).is_err());

        let max_id = "a".repeat(255);
        assert!(validate_task_id(&max_id).is_ok());
    }

    #[test]
    fn test_validate_task_id_path_traversal() {
        // Path traversal attacks
        assert!(validate_task_id("..").is_err());
        assert!(validate_task_id("../etc").is_err());
        assert!(validate_task_id("task/../../etc").is_err());
        assert!(validate_task_id("/absolute/path").is_err());
        assert!(validate_task_id("task\\windows").is_err());
    }

    #[test]
    fn test_validate_task_id_special_chars() {
        // Special characters
        assert!(validate_task_id("task name").is_err());
        assert!(validate_task_id("task@name").is_err());
        assert!(validate_task_id("task#name").is_err());
        assert!(validate_task_id("task$name").is_err());
        assert!(validate_task_id("task!name").is_err());
        assert!(validate_task_id("task.name").is_err());
    }

    #[test]
    fn test_offload_rejects_path_traversal() {
        let tmp = TempDir::new().unwrap();
        let refs_dir = tmp.path();

        // Attempt path traversal
        assert!(offload_text(refs_dir, "../../../etc", "malicious").is_err());
        assert!(offload_text(refs_dir, "/absolute/path", "malicious").is_err());
        assert!(offload_text(refs_dir, "task/../../etc", "malicious").is_err());
    }

    #[test]
    fn test_recall_rejects_path_traversal() {
        let tmp = TempDir::new().unwrap();
        let refs_dir = tmp.path();

        // Create a node with malicious task_id (bypassing validation by direct construction)
        let malicious_node = NodeId::new("../../../etc".to_string(), 1);
        assert!(recall_text(refs_dir, &malicious_node).is_err());

        let absolute_node = NodeId::new("/etc/passwd".to_string(), 1);
        assert!(recall_text(refs_dir, &absolute_node).is_err());
    }
}

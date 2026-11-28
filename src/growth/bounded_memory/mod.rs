mod format;
mod mutations;
mod reconcile;
mod write_state;

use crate::index::db::Db;
use crate::util::time;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

pub(crate) use format::{extract_body, has_entry_separator, normalize_crlf};
pub use write_state::PendingMdWrite;
// format.rs / reconcile.rs / mutations.rs take these via `super::`.
use write_state::{
    bump_md_generation, md_generation, md_write_state, set_unflushed_snapshot,
    write_md_render_locked,
};
// tests.rs reaches these via `use super::*`.
#[cfg(test)]
use format::ENTRY_SEPARATOR;
#[cfg(test)]
use mutations::superseded_chain;

/// 有界记忆管理器
pub struct BoundedMemory<'a> {
    pub(super) memory_dir: PathBuf,
    pub(super) db: &'a Db,
    pub(super) memory_limit: usize,
    pub(super) user_limit: usize,
    security_scan: bool,
    /// Fraction of MEMORY.md capacity reserved for auto-extracted atoms (default 0.3).
    pub(super) atom_capacity_ratio: f64,
}

/// 溯源验证结果
#[derive(Debug, serde::Serialize)]
pub struct ProvenanceInfo {
    pub target: String,
    pub content: String,
    pub source_session: Option<String>,
    pub confidence: String,
    pub created_at: String,
    pub session_exists: bool,
    pub session_file_path: Option<String>,
}

/// 溯源验证报告
#[derive(Debug, serde::Serialize)]
pub struct ProvenanceReport {
    pub target: String,
    pub total_entries: usize,
    pub verified: usize,
    pub missing_source: usize,
    pub no_source: usize,
    pub entries: Vec<ProvenanceInfo>,
}

/// DB/.md 一致性检查报告
#[derive(Debug, serde::Serialize)]
pub struct ReconcileReport {
    pub target: String,
    pub md_entry_count: usize,
    pub db_entry_count: usize,
    pub only_in_md: Vec<String>,
    pub only_in_db: Vec<String>,
}

/// 多条目行拆分报告
#[derive(Debug, serde::Serialize)]
pub struct SplitReport {
    pub target: String,
    pub bad_rows: usize,
    pub sub_entries_created: usize,
    pub duplicates_skipped: usize,
}

impl<'a> BoundedMemory<'a> {
    pub fn new(memory_dir: &Path, db: &'a Db, memory_limit: usize, user_limit: usize) -> Self {
        Self {
            memory_dir: memory_dir.to_path_buf(),
            db,
            memory_limit,
            user_limit,
            security_scan: true,
            atom_capacity_ratio: 0.3,
        }
    }

    /// 可选关闭安全扫描（用于受信任的内部调用 / 配置覆盖）
    pub fn with_security_scan(mut self, enabled: bool) -> Self {
        self.security_scan = enabled;
        self
    }

    /// 设置 atom 容量占比（MEMORY.md 中分配给自动提取 atoms 的比例）
    pub fn with_atom_capacity_ratio(mut self, ratio: f64) -> Self {
        self.atom_capacity_ratio = if ratio.is_finite() {
            ratio.clamp(0.0, 1.0)
        } else {
            0.3
        };
        self
    }

    /// 严格白名单验证 target，禁止路径穿越
    pub(super) fn target_file(&self, target: &str) -> anyhow::Result<PathBuf> {
        match target {
            "memory" => Ok(self.memory_dir.join("MEMORY.md")),
            "user" => Ok(self.memory_dir.join("USER.md")),
            other => anyhow::bail!("非法 target: {} (仅支持 'memory' / 'user')", other),
        }
    }

    pub(super) fn capacity(&self, target: &str) -> usize {
        match target {
            "memory" => self.memory_limit,
            "user" => self.user_limit,
            _ => self.memory_limit,
        }
    }

    pub(super) fn metadata_header(&self, target: &str, capacity: usize) -> String {
        let label = if target == "user" {
            "ASUNA USER PROFILE"
        } else {
            "ASUNA MEMORY"
        };
        let updated = time::unix_ms_to_iso(time::now_unix_ms());
        format!(
            "<!-- {} | capacity: {} chars | updated: {} -->",
            label, capacity, updated
        )
    }

    pub(super) fn run_scan(&self, content: &str) -> anyhow::Result<()> {
        if !self.security_scan {
            return Ok(());
        }
        let scan = crate::growth::security::scan_content(content);
        if !scan.is_safe() {
            anyhow::bail!("安全扫描未通过: {}", scan.reason());
        }
        Ok(())
    }

    /// 读取实际磁盘 `.md` 内容（供 `reconcile_check` 等需直读落盘状态的路径使用）
    pub(super) fn read_disk(&self, target: &str) -> anyhow::Result<String> {
        let path = self.target_file(target)?;
        if path.exists() {
            Ok(std::fs::read_to_string(path)?)
        } else {
            Ok(String::new())
        }
    }

    /// 读取全文（若存在同代际未刷盘的最新渲染快照则优先返回，避免并发读写间隙读到旧盘面）
    pub fn read(&self, target: &str) -> anyhow::Result<String> {
        let path = self.target_file(target)?;
        let state = md_write_state(&path);
        let current = state.generation.load(Ordering::Acquire);
        {
            let guard = crate::util::recover_poison(&state.unflushed);
            if let Some((gen, content)) = guard.as_ref() {
                if *gen == current {
                    return Ok(content.clone());
                }
            }
        }
        self.read_disk(target)
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod audit_tests;

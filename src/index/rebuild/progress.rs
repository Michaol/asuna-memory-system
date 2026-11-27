use serde::Serialize;
use std::sync::{Arc, Mutex};

/// 重建状态
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum RebuildStatus {
    Idle,
    Running,
    Completed,
    Failed,
}

/// 重建进度（线程安全，跨线程共享）
#[derive(Debug, Serialize)]
pub struct RebuildProgress {
    pub status: RebuildStatus,
    pub sessions_processed: usize,
    pub turns_indexed: usize,
    pub vectors_indexed: usize,
    pub errors: Vec<String>,
    pub started_at: i64,
    pub finished_at: Option<i64>,
}

impl Default for RebuildProgress {
    fn default() -> Self {
        Self {
            status: RebuildStatus::Idle,
            sessions_processed: 0,
            turns_indexed: 0,
            vectors_indexed: 0,
            errors: Vec::new(),
            started_at: 0,
            finished_at: None,
        }
    }
}

pub type SharedProgress = Arc<Mutex<RebuildProgress>>;

pub fn new_shared_progress() -> SharedProgress {
    Arc::new(Mutex::new(RebuildProgress::default()))
}

/// 进度回调类型：(sessions_processed, turns_indexed, 已完成向量数, 向量总数)
/// D1-4: 携带 Phase 1 计数，运行中 sessions_processed/turns_indexed 不再恒 0。
pub(super) type ProgressFn = dyn Fn(usize, usize, usize, usize) + Send + Sync;

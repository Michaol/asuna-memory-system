//! Shared application state for HTTP gateway

use crate::config::Config;
use crate::embedder::LazyEmbedder;
use crate::index::db::Db;
use crate::memory::llm::LlmClient;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use tokio::sync::Semaphore;

/// Maximum concurrent post-session pipeline tasks spawned by `/session/end`.
pub const MAX_CONCURRENT_PIPELINES: usize = 4;

/// Thread-safe application state shared across all HTTP handlers
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    /// Precomputed SHA-256 of `gateway.api_key` — the auth middleware compares
    /// against this instead of rehashing the expected key on every request.
    pub api_key_digest: [u8; 32],
    pub db: Arc<Mutex<Db>>,
    pub embedder: Option<Arc<LazyEmbedder>>,
    /// LLM client for extraction pipeline (None = Lite mode, no extraction)
    pub llm: Option<Arc<LlmClient>>,
    /// Bounds concurrent `/session/end` background pipeline runs.
    pub pipeline_semaphore: Arc<Semaphore>,
    /// Tracks session IDs currently running a background pipeline to deduplicate retries.
    pub active_pipeline_sessions: Arc<Mutex<HashSet<String>>>,
    /// Single-flight guard preventing overlapping L3–L5 consolidation runs.
    pub consolidation_running: Arc<AtomicBool>,
    /// Set when the gateway receives a shutdown signal so background work
    /// (startup vector backfill) stops instead of holding up runtime drop.
    pub shutdown: Arc<AtomicBool>,
}

impl AppState {
    pub fn new(
        config: Config,
        db: Db,
        embedder: Option<LazyEmbedder>,
        llm: Option<LlmClient>,
    ) -> Self {
        Self {
            api_key_digest: Sha256::digest(config.gateway.api_key.trim().as_bytes()).into(),
            config: Arc::new(config),
            db: Arc::new(Mutex::new(db)),
            embedder: embedder.map(Arc::new),
            llm: llm.map(Arc::new),
            pipeline_semaphore: Arc::new(Semaphore::new(MAX_CONCURRENT_PIPELINES)),
            active_pipeline_sessions: Arc::new(Mutex::new(HashSet::new())),
            consolidation_running: Arc::new(AtomicBool::new(false)),
            shutdown: Arc::new(AtomicBool::new(false)),
        }
    }
}

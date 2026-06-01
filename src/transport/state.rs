//! Shared application state for HTTP gateway

use crate::config::Config;
use crate::embedder::LazyEmbedder;
use crate::index::db::Db;
use crate::memory::llm::LlmClient;
use std::sync::{Arc, Mutex};

/// Thread-safe application state shared across all HTTP handlers
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub db: Arc<Mutex<Db>>,
    #[allow(dead_code)] // Will be used in P3 for embedding
    pub embedder: Option<Arc<Mutex<LazyEmbedder>>>,
    /// LLM client for extraction pipeline (None = Lite mode, no extraction)
    pub llm: Option<Arc<LlmClient>>,
}

impl AppState {
    pub fn new(
        config: Config,
        db: Db,
        embedder: Option<LazyEmbedder>,
        llm: Option<LlmClient>,
    ) -> Self {
        Self {
            config: Arc::new(config),
            db: Arc::new(Mutex::new(db)),
            embedder: embedder.map(|e| Arc::new(Mutex::new(e))),
            llm: llm.map(Arc::new),
        }
    }
}

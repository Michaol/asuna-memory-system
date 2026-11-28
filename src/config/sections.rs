use serde::{Deserialize, Serialize};

/// Pipeline configuration for memory extraction (P3)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PipelineConfig {
    /// Enable automatic L1 extraction
    pub enable_extraction: bool,
    /// Minimum session length (in turns) for L1 extraction. The post-session
    /// pipeline skips extraction for sessions with fewer turns than this threshold.
    pub every_n_turns: usize,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            enable_extraction: true,
            every_n_turns: 5,
        }
    }
}

/// Admission configuration for A-MAC scoring (P4)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AdmissionConfig {
    /// Enable admission control
    pub enabled: bool,
    /// Minimum score threshold (0.0-1.0)
    pub threshold: f64,
    /// Weights for 5 dimensions: [utility, novelty, recency, importance, confidence]
    pub weights: [f64; 5],
}

impl Default for AdmissionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            threshold: 0.6,
            weights: [0.3, 0.2, 0.2, 0.2, 0.1],
        }
    }
}

/// Recall configuration for memory retrieval
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RecallConfig {
    /// Token budget for recall context
    pub token_budget: usize,
}

impl Default for RecallConfig {
    fn default() -> Self {
        Self { token_budget: 2000 }
    }
}

/// L2 scenario aggregation configuration (opt-in; requires LLM and embedder).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ScenarioConfig {
    /// Enable L2 scenario aggregation in the post-session pipeline.
    pub enabled: bool,
    /// Cosine similarity threshold for clustering atoms into a scenario.
    pub similarity_threshold: f32,
    /// Minimum atoms in a cluster to form a scenario (singletons are skipped).
    pub min_cluster_size: usize,
    /// Cap on `memory_type='scenario'` rows; oldest are evicted beyond this.
    pub max_scenarios: usize,
}

impl Default for ScenarioConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            similarity_threshold: 0.8,
            min_cluster_size: 2,
            max_scenarios: 50,
        }
    }
}

/// Persona configuration for the L3-L5 consolidation cycle (P5 / S14b / S14c)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PersonaConfig {
    /// Sessions (by `sessions.updated_at`) touched since a layer's last output
    /// before that layer refreshes. 0 disables the consolidation cycle.
    pub trigger_every_n: usize,
}

impl Default for PersonaConfig {
    fn default() -> Self {
        Self {
            trigger_every_n: 10,
        }
    }
}

/// LLM configuration for extraction pipeline
#[derive(Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct LlmConfig {
    /// LLM API base URL (reads from AMS_LLM_BASE_URL or OPENAI_BASE_URL)
    pub base_url: String,
    /// LLM API key (reads from AMS_LLM_API_KEY or OPENAI_API_KEY)
    pub api_key: String,
    /// LLM model name (reads from AMS_LLM_MODEL or OPENAI_MODEL)
    pub model: String,
}

impl std::fmt::Debug for LlmConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LlmConfig")
            .field(
                "base_url",
                &crate::util::sanitize_url_for_log(&self.base_url),
            )
            .field(
                "api_key",
                &if self.api_key.is_empty() {
                    ""
                } else {
                    "[REDACTED]"
                },
            )
            .field("model", &self.model)
            .finish()
    }
}

impl LlmConfig {
    /// Fill empty fields from environment variables (config.json values win over env vars).
    pub fn resolve_env(&mut self) {
        // W1: surrounding whitespace must not reach the URL or the Authorization
        // header (same yardstick as GatewayConfig::resolve_env).
        self.base_url = self.base_url.trim().to_string();
        self.api_key = self.api_key.trim().to_string();
        // F1: model gets the same yardstick — padded/whitespace-only values
        // must not reach the request body (whitespace-only counts as unset).
        self.model = self.model.trim().to_string();
        let custom_base = !self.base_url.is_empty()
            || std::env::var("AMS_LLM_BASE_URL").is_ok_and(|v| !v.trim().is_empty());
        if self.base_url.is_empty() {
            self.base_url = std::env::var("AMS_LLM_BASE_URL")
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
                .or_else(|| {
                    std::env::var("OPENAI_BASE_URL")
                        .ok()
                        .map(|v| v.trim().to_string())
                        .filter(|v| !v.is_empty())
                })
                .unwrap_or_default();
        }
        if self.api_key.is_empty() {
            self.api_key = std::env::var("AMS_LLM_API_KEY")
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
                .or_else(|| {
                    if custom_base {
                        None
                    } else {
                        std::env::var("OPENAI_API_KEY")
                            .ok()
                            .map(|v| v.trim().to_string())
                            .filter(|v| !v.is_empty())
                    }
                })
                .unwrap_or_default();
        }
        if self.model.is_empty() {
            self.model = std::env::var("AMS_LLM_MODEL")
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
                .or_else(|| {
                    std::env::var("OPENAI_MODEL")
                        .ok()
                        .map(|v| v.trim().to_string())
                        .filter(|v| !v.is_empty())
                })
                .unwrap_or_else(|| "deepseek-v3".to_string());
        }
    }
}

/// Conversation archive configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ConversationConfig {
    pub preview_length: usize,
}

impl Default for ConversationConfig {
    fn default() -> Self {
        Self {
            preview_length: 200,
        }
    }
}

/// Growth-memory configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MemoryConfig {
    /// Maximum character count for MEMORY.md (default 2200 ≈ ~550 tokens).
    pub memory_char_limit: usize,
    /// Maximum character count for USER.md (default 1375 ≈ ~344 tokens).
    pub user_char_limit: usize,
    pub security_scan: bool,
    /// Fraction of MEMORY.md capacity reserved for auto-extracted atoms (default 0.3).
    // v4-M: 容器级 #[serde(default)] 已从 Self::default() 取 0.3，字段级
    // default 冗余（删除后缺省值不变）。
    pub atom_capacity_ratio: f64,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            memory_char_limit: 2200,
            user_char_limit: 1375,
            security_scan: true,
            atom_capacity_ratio: 0.3,
        }
    }
}

/// Turn-search configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SearchConfig {
    pub default_top_k: usize,
    pub search_mode: String,
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            default_top_k: 5,
            search_mode: "hybrid".to_string(),
        }
    }
}

/// Embedding backend configuration (1024-dim online API only in v3.0).
#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct EmbeddingConfig {
    pub dimensions: usize,
    pub batch_size: usize,
    /// API base URL for OpenAI-compatible embedding endpoint.
    #[serde(default)]
    pub api_url: String,
    /// API key for the embedding endpoint (reads from AMS_EMBEDDING_API_KEY).
    #[serde(default)]
    pub api_key: String,
    /// Model name for the embedding API (e.g. "qwen3.7-text-embedding-flash").
    #[serde(default)]
    pub api_model: String,
    /// API format: "openai" (default) or "dashscope". Auto-detected when empty.
    #[serde(default)]
    pub api_format: String,
}

impl std::fmt::Debug for EmbeddingConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EmbeddingConfig")
            .field("dimensions", &self.dimensions)
            .field("batch_size", &self.batch_size)
            .field("api_url", &crate::util::sanitize_url_for_log(&self.api_url))
            .field(
                "api_key",
                &if self.api_key.is_empty() {
                    ""
                } else {
                    "[REDACTED]"
                },
            )
            .field("api_model", &self.api_model)
            .field("api_format", &self.api_format)
            .finish()
    }
}

impl Default for EmbeddingConfig {
    fn default() -> Self {
        Self {
            dimensions: crate::embedder::EMBEDDING_DIMENSIONS,
            batch_size: 32,
            api_url: String::new(),
            api_key: String::new(),
            api_model: String::new(),
            api_format: String::new(),
        }
    }
}

impl EmbeddingConfig {
    /// Fill API fields from environment variables and auto-detect provider format/limits.
    pub fn resolve_env(&mut self) {
        // W1: trim before use — a padded URL keeps its whitespace mid-URL once
        // the endpoint path is appended (ApiEmbedder::new trims defensively too).
        self.api_url = self.api_url.trim().to_string();
        self.api_key = self.api_key.trim().to_string();
        // F2: whitespace-only api_format counts as unset so auto-detection
        // still fires (same yardstick as the sibling fields above).
        self.api_format = self.api_format.trim().to_string();
        // NEW-1-config: api_model 同族 trim（纯空白视同未设置；padded 下游 W8 兜住）。
        self.api_model = self.api_model.trim().to_string();
        if self.api_key.is_empty() {
            self.api_key = std::env::var("AMS_EMBEDDING_API_KEY")
                .map(|k| k.trim().to_string())
                .unwrap_or_default();
        }
        let lower_url = self.api_url.to_ascii_lowercase();
        // DashScope endpoints come in two domain families, so detection matches
        // either substring: the legacy shared domain (dashscope.aliyuncs.com, in
        // maintenance since 2026-09) matches "dashscope"; the workspace domain
        // ({WorkspaceId}.{region}.maas.aliyuncs.com) matches "maas.aliyuncs.com".
        let dashscope_url =
            lower_url.contains("dashscope") || lower_url.contains("maas.aliyuncs.com");
        // Auto-detect DashScope native format ONLY for native DashScope endpoints;
        // compatible-mode URLs speak OpenAI `/embeddings` format.
        if self.api_format.is_empty() && dashscope_url && !lower_url.contains("compatible-mode") {
            self.api_format = "dashscope".to_string();
        }
        // DashScope per-request input caps are model-dependent: the
        // qwen3.7-text-embedding family (incl. -flash) accepts 20 texts in the
        // native format; text-embedding-v3/v4 accept 10. Compatible-mode keeps
        // the conservative 10 cap (flash's compatible-mode cap is undocumented).
        // Clamp batch_size DOWN to the cap so a request never exceeds the model's
        // input limit and gets rejected; a smaller configured batch is left as-is.
        // F2-a: api_format 已在顶部 trim（纯空白视同未设置），故此处可直接
        // eq_ignore_ascii_case 比较，无需再 trim。
        let dashscope_format = self.api_format.eq_ignore_ascii_case("dashscope");
        if dashscope_format || dashscope_url {
            let cap = if dashscope_format
                && self
                    .api_model
                    .to_ascii_lowercase()
                    .starts_with("qwen3.7-text-embedding")
            {
                20
            } else {
                10
            };
            if self.batch_size > cap {
                self.batch_size = cap;
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GraphConfig {
    pub enabled: bool,
    pub remind_on_save: bool,
}

impl Default for GraphConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            remind_on_save: true,
        }
    }
}

#[cfg(test)]
#[path = "sections_tests.rs"]
mod tests;

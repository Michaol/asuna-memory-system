use std::path::{Path, PathBuf};
use serde::{Deserialize, Serialize};

/// 模型发现搜索路径优先级
fn model_search_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();

    // Windows 开发环境：支持 ASUNA_DEV_ROOT 环境变量
    #[cfg(windows)]
    {
        if let Ok(dev_root) = std::env::var("ASUNA_DEV_ROOT") {
            paths.push(PathBuf::from(dev_root).join("models/embeddinggemma-300m-q8"));
        }
    }

    // 跨平台便携路径
    paths.push(PathBuf::from("~/.asuna/models/embeddinggemma-300m-q8"));

    paths
}

/// Pipeline configuration for memory extraction (P3)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineConfig {
    /// Enable automatic L1 extraction
    pub enable_extraction: bool,
    /// Extract every N turns
    pub every_n_turns: usize,
    /// Idle timeout before extraction (seconds)
    pub idle_timeout_seconds: u64,
    /// Minimum interval between L2 extractions (seconds)
    pub l2_min_interval_seconds: u64,
    /// Enable warmup period (delay first extraction)
    pub enable_warmup: bool,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            enable_extraction: true,
            every_n_turns: 5,
            idle_timeout_seconds: 600,
            l2_min_interval_seconds: 3600,
            enable_warmup: true,
        }
    }
}

/// Admission configuration for A-MAC scoring (P4)
#[derive(Debug, Clone, Serialize, Deserialize)]
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
pub struct RecallConfig {
    /// Retrieval strategy: "hybrid", "keyword", "vector"
    pub strategy: String,
    /// Maximum results to return
    pub max_results: usize,
    /// Token budget for recall context
    pub token_budget: usize,
    /// Timeout for recall operations (milliseconds)
    pub timeout_ms: u64,
}

impl Default for RecallConfig {
    fn default() -> Self {
        Self {
            strategy: "hybrid".to_string(),
            max_results: 10,
            token_budget: 2000,
            timeout_ms: 5000,
        }
    }
}

/// L2 scenario aggregation configuration (v2.6.1: wired into the pipeline).
/// Scenarios cluster this session's newly-stored atoms by embedding similarity
/// and summarize each cluster via the LLM; the summary is stored as a
/// `memory_type='scenario'` row in bounded_memory so `/recall` L2 surfaces it,
/// plus a human-readable Markdown file under `memory/scenarios/`.
/// Opt-in (default disabled): requires both an LLM and an embedder.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScenarioConfig {
    /// Enable L2 scenario aggregation in the post-session pipeline.
    pub enabled: bool,
    /// Cosine similarity threshold for clustering atoms into a scenario.
    pub similarity_threshold: f32,
    /// Minimum atoms in a cluster to form a scenario (singletons are skipped).
    pub min_cluster_size: usize,
    /// Cap on `memory_type='scenario'` rows; oldest are evicted beyond this.
    /// Prevents unbounded scenario growth (scenarios bypass the atom budget).
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

/// Persona configuration for L3 layer (P5)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersonaConfig {
    /// Update persona every N L1 extractions
    pub trigger_every_n: usize,
}

impl Default for PersonaConfig {
    fn default() -> Self {
        Self { trigger_every_n: 10 }
    }
}

/// Privacy configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrivacyConfig {
    /// L0 retention period (days, 0 = forever)
    pub l0_retention_days: u32,
    /// L1 retention period (days, 0 = forever)
    pub l1_retention_days: u32,
    /// Enable automatic cleanup
    pub auto_cleanup: bool,
}

impl Default for PrivacyConfig {
    fn default() -> Self {
        Self {
            l0_retention_days: 90,
            l1_retention_days: 0,
            auto_cleanup: true,
        }
    }
}

/// LLM configuration for extraction pipeline
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmConfig {
    /// LLM API base URL (reads from AMS_LLM_BASE_URL or OPENAI_BASE_URL)
    pub base_url: String,
    /// LLM API key (reads from AMS_LLM_API_KEY or OPENAI_API_KEY)
    pub api_key: String,
    /// LLM model name (reads from AMS_LLM_MODEL or OPENAI_MODEL)
    pub model: String,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            base_url: String::new(),
            api_key: String::new(),
            model: String::new(),
        }
    }
}

impl LlmConfig {
    /// Fill empty fields from environment variables.
    /// Called after deserialization so that config.json values take precedence
    /// over env vars, but env vars fill in fields omitted from the config.
    pub fn resolve_env(&mut self) {
        if self.base_url.is_empty() {
            self.base_url = std::env::var("AMS_LLM_BASE_URL")
                .or_else(|_| std::env::var("OPENAI_BASE_URL"))
                .unwrap_or_default();
        }
        if self.api_key.is_empty() {
            self.api_key = std::env::var("AMS_LLM_API_KEY")
                .or_else(|_| std::env::var("OPENAI_API_KEY"))
                .unwrap_or_default();
        }
        // Only override model from env if config.json left it at the default empty string.
        // The default "deepseek-v3" in Default::default() is a fallback for when no config
        // file exists; if a user explicitly sets model in config.json, that takes precedence.
        if self.model.is_empty() {
            if let Ok(m) = std::env::var("AMS_LLM_MODEL").or_else(|_| std::env::var("OPENAI_MODEL")) {
                self.model = m;
            } else {
                self.model = "deepseek-v3".to_string();
            }
        }
    }
}

/// Gateway configuration for HTTP API server
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewayConfig {
    /// Enable API key authentication
    pub auth_enabled: bool,
    /// API key for authentication (reads from AMS_GATEWAY_API_KEY)
    pub api_key: String,
    /// Allowed CORS origins (empty = allow all, not recommended for production)
    pub cors_origins: Vec<String>,
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            auth_enabled: false,
            api_key: String::new(),
            cors_origins: vec![],
        }
    }
}

impl GatewayConfig {
    /// Fill empty fields from environment variables.
    pub fn resolve_env(&mut self) {
        if self.api_key.is_empty() {
            self.api_key = std::env::var("AMS_GATEWAY_API_KEY").unwrap_or_default();
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub data_dir: PathBuf,
    pub profile_id: String,

    pub conversation: ConversationConfig,
    pub memory: MemoryConfig,
    pub search: SearchConfig,
    pub embedding: EmbeddingConfig,

    #[serde(default)]
    pub graph: GraphConfig,

    /// 运行时标记：config.json 中是否缺少 graph 段
    #[serde(skip)]
    pub graph_using_defaults: bool,

    /// [M5] 废弃字段，实际 DB 路径由 profile_db_path() 决定。
    /// 保留以兼容旧版 config.json。
    #[serde(default, skip_serializing)]
    #[allow(dead_code)]
    pub db_path: Option<PathBuf>,

    /// 手动指定的模型目录（最高优先级）
    pub model_path: Option<PathBuf>,

    /// P3: 记忆提取管道配置
    #[serde(default)]
    pub pipeline: PipelineConfig,

    /// P4: A-MAC 准入评分配置
    #[serde(default)]
    pub admission: AdmissionConfig,

    /// 召回配置
    #[serde(default)]
    pub recall: RecallConfig,

    /// v2.6.1: L2 scenario aggregation (opt-in)
    #[serde(default)]
    pub scenarios: ScenarioConfig,

    /// P5: 画像配置
    #[serde(default)]
    pub persona: PersonaConfig,

    /// 隐私配置
    #[serde(default)]
    pub privacy: PrivacyConfig,

    /// LLM 配置（用于提取管道）
    #[serde(default)]
    pub llm: LlmConfig,

    /// Gateway 配置（HTTP API 服务器）
    #[serde(default)]
    pub gateway: GatewayConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationConfig {
    pub enabled: bool,
    pub auto_embed: bool,
    pub preview_length: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryConfig {
    pub memory_enabled: bool,
    pub user_profile_enabled: bool,
    /// Maximum character count for MEMORY.md (default 2200 ≈ ~550 tokens).
    /// Enforced per-write; exceeding this rejects new entries.
    pub memory_char_limit: usize,
    /// Maximum character count for USER.md (default 1375 ≈ ~344 tokens).
    /// Kept smaller than memory_char_limit because the user profile is
    /// injected into every conversation context.
    pub user_char_limit: usize,
    pub security_scan: bool,
    /// Fraction of MEMORY.md capacity reserved for auto-extracted atoms (default 0.3).
    /// Manual entries use the remaining capacity. Atoms exceeding their reserved
    /// budget are evicted LRU (oldest first) before new atoms are appended.
    #[serde(default = "default_atom_capacity_ratio")]
    pub atom_capacity_ratio: f64,
}

fn default_atom_capacity_ratio() -> f64 {
    0.3
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchConfig {
    pub default_top_k: usize,
    pub search_mode: String,
    pub fts_enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingConfig {
    pub model_name: String,
    pub dimensions: usize,
    pub batch_size: usize,
    /// API base URL for OpenAI-compatible embedding endpoint (e.g. "https://api.openai.com/v1").
    /// When both `api_url` and `api_model` are set, API backend is used instead of local ONNX.
    #[serde(default)]
    pub api_url: String,
    /// API key for the embedding endpoint (reads from AMS_EMBEDDING_API_KEY).
    /// Optional — some local endpoints (Ollama) don't require auth.
    #[serde(default)]
    pub api_key: String,
    /// Model name for the embedding API (e.g. "text-embedding-3-small").
    /// Must be set together with `api_url` to enable the API backend.
    #[serde(default)]
    pub api_model: String,
    /// API format: "openai" (default) or "dashscope" (DashScope native API).
    /// Auto-detected from api_url if empty (URLs containing "dashscope" use "dashscope").
    #[serde(default)]
    pub api_format: String,
}

impl EmbeddingConfig {
    /// Fill API fields from environment variables if empty.
    /// Auto-detect api_format from api_url when not explicitly set.
    pub fn resolve_env(&mut self) {
        if self.api_key.is_empty() {
            self.api_key = std::env::var("AMS_EMBEDDING_API_KEY").unwrap_or_default();
        }
        // Auto-detect DashScope format from URL
        if self.api_format.is_empty() && !self.api_url.is_empty() {
            if self.api_url.contains("dashscope") {
                self.api_format = "dashscope".to_string();
            }
        }
        // DashScope has a hard limit of 10 inputs per embedding request (HTTP
        // 400 above it). Clamp batch_size so batch embedding (L2 scenario
        // aggregation, rebuild, DB backfill) never exceeds the provider cap even
        // when the user's config uses a larger batch_size (default 32).
        if self.api_format == "dashscope" && self.batch_size > 10 {
            self.batch_size = 10;
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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

impl Default for Config {
    fn default() -> Self {
        let home = dirs_home();
        let data_dir = home.join(".asuna");

        Self {
            data_dir: data_dir.clone(),
            profile_id: "default".to_string(),

            conversation: ConversationConfig {
                enabled: true,
                auto_embed: true,
                preview_length: 200,
            },
            memory: MemoryConfig {
                memory_enabled: true,
                user_profile_enabled: true,
                // 2200 chars ≈ 1100 中文字符 ≈ ~550 tokens (GPT-4 tokenizer)
                // Chosen to fit a single memory file within typical context window
                // budget while leaving room for metadata headers.
                memory_char_limit: 2200,
                // 1375 chars ≈ 687 中文字符 ≈ ~344 tokens
                // Slightly smaller than memory to keep user profile concise
                // for injection into every conversation context.
                user_char_limit: 1375,
                security_scan: true,
                atom_capacity_ratio: 0.3,
            },
            search: SearchConfig {
                default_top_k: 5,
                search_mode: "hybrid".to_string(),
                fts_enabled: true,
            },
            embedding: EmbeddingConfig {
                model_name: "embeddinggemma-300m-q8".to_string(),
                dimensions: 1024,
                batch_size: 32,
                api_url: String::new(),
                api_key: String::new(),
                api_model: String::new(),
                api_format: String::new(),
            },
            graph: GraphConfig::default(),
            graph_using_defaults: false,
            db_path: None,
            model_path: None,
            pipeline: PipelineConfig::default(),
            admission: AdmissionConfig::default(),
            recall: RecallConfig::default(),
            scenarios: ScenarioConfig::default(),
            persona: PersonaConfig::default(),
            privacy: PrivacyConfig::default(),
            llm: LlmConfig::default(),
            gateway: GatewayConfig::default(),
        }
    }
}

impl Config {
    /// 从 JSON 文件加载配置，若不存在则使用默认值
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let mut config = if path.exists() {
            let content = std::fs::read_to_string(path)?;
            // 单次 JSON 解析：先解析为 Value 检查 graph key，再转换为 Config（4.2 fix）
            let raw: serde_json::Value = serde_json::from_str(&content)?;
            let graph_missing = raw
                .as_object()
                .map(|obj| !obj.contains_key("graph"))
                .unwrap_or(true);
            let mut config: Config = serde_json::from_value(raw)?;
            config.graph_using_defaults = graph_missing;
            // 展开 ~ 路径
            config.data_dir = expand_tilde(&config.data_dir);
            // db_path 是废弃字段，忽略其值（实际使用 profile_db_path()）
            config
        } else {
            Self {
                graph_using_defaults: true,
                ..Self::default()
            }
        };

        // Fill env-var-backed fields that were omitted from config.json
        config.llm.resolve_env();
        config.gateway.resolve_env();
        config.embedding.resolve_env();

        Ok(config)
    }

    /// 智能发现模型目录
    pub fn discover_model_dir(&self) -> Option<PathBuf> {
        // 1. 手动指定
        if let Some(ref p) = self.model_path {
            if p.join("model_quantized.onnx").exists() {
                return Some(p.clone());
            }
        }

        // 2. 搜索预设路径
        for p in model_search_paths() {
            let p = expand_tilde(&p);
            if p.join("model_quantized.onnx").exists() {
                return Some(p);
            }
        }

        None
    }

    /// Create an embedder from config. Handles both "local" (ONNX) and "api" providers.
    /// Returns `None` if neither provider is configured or available.
    pub fn create_embedder(&self) -> Option<crate::embedder::LazyEmbedder> {
        let model_dir = self.discover_model_dir();
        crate::embedder::LazyEmbedder::from_config(&self.embedding, model_dir.as_deref())
    }

    /// 获取 profile 对应的数据目录
    pub fn profile_dir(&self) -> PathBuf {
        self.data_dir.join("profiles").join(&self.profile_id)
    }

    /// 获取对话归档目录（按 profile 隔离）
    pub fn conversations_dir(&self) -> PathBuf {
        self.profile_dir().join("conversations")
    }

    /// 获取模型存储目录（model-download 下载目标路径）
    pub fn model_dir(&self) -> PathBuf {
        self.data_dir.join("models").join("embeddinggemma-300m-q8")
    }

    /// 获取成长记忆目录（按 profile 隔离）
    pub fn memory_dir(&self) -> PathBuf {
        self.profile_dir().join("memory")
    }

    /// 获取短期记忆 refs 目录（按 profile 隔离）
    pub fn refs_dir(&self) -> PathBuf {
        self.profile_dir().join("refs")
    }

    /// 获取 profile 对应的数据库路径
    pub fn profile_db_path(&self) -> PathBuf {
        self.profile_dir().join("memory.db")
    }

    /// 确保所有需要的目录存在
    pub fn ensure_dirs(&self) -> anyhow::Result<()> {
        std::fs::create_dir_all(&self.data_dir)?;
        std::fs::create_dir_all(self.profile_dir())?;
        std::fs::create_dir_all(self.conversations_dir())?;
        std::fs::create_dir_all(self.memory_dir())?;
        std::fs::create_dir_all(self.refs_dir())?;
        std::fs::create_dir_all(self.data_dir.join("models"))?;
        Ok(())
    }

    /// 列出所有可用 profile
    pub fn list_profiles(&self) -> Vec<String> {
        let profiles_dir = self.data_dir.join("profiles");
        let mut profiles = Vec::new();
        if let Ok(entries) = std::fs::read_dir(profiles_dir) {
            for entry in entries.flatten() {
                if entry.path().is_dir() {
                    if let Some(name) = entry.file_name().to_str() {
                        profiles.push(name.to_string());
                    }
                }
            }
        }
        profiles.sort();
        profiles
    }
}

fn dirs_home() -> PathBuf {
    std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

pub fn expand_tilde(path: &Path) -> PathBuf {
    let s = path.to_string_lossy();
    if s.starts_with("~") {
        let home = dirs_home();
        home.join(s.strip_prefix("~/").unwrap_or(s.strip_prefix("~").unwrap_or(&s)))
    } else {
        path.to_path_buf()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_graph_missing_detected() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("config.json");
        std::fs::write(
            &p,
            r#"{"data_dir": ".", "profile_id": "default", "conversation": {"enabled": true, "auto_embed": true, "preview_length": 200}, "memory": {"memory_enabled": true, "user_profile_enabled": true, "memory_char_limit": 2200, "user_char_limit": 1375, "security_scan": true}, "search": {"default_top_k": 5, "search_mode": "hybrid", "fts_enabled": true}, "embedding": {"model_name": "test", "dimensions": 768, "batch_size": 32}}"#,
        ).unwrap();
        let config = Config::load(&p).unwrap();
        assert!(config.graph_using_defaults, "should detect missing graph section");
    }

    #[test]
    fn test_graph_present_not_flagged() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("config.json");
        std::fs::write(
            &p,
            r#"{"data_dir": ".", "profile_id": "default", "conversation": {"enabled": true, "auto_embed": true, "preview_length": 200}, "memory": {"memory_enabled": true, "user_profile_enabled": true, "memory_char_limit": 2200, "user_char_limit": 1375, "security_scan": true}, "search": {"default_top_k": 5, "search_mode": "hybrid", "fts_enabled": true}, "embedding": {"model_name": "test", "dimensions": 768, "batch_size": 32}, "graph": {"enabled": false, "remind_on_save": false}}"#,
        ).unwrap();
        let config = Config::load(&p).unwrap();
        assert!(!config.graph_using_defaults, "should not flag when graph section present");
        assert!(!config.graph.enabled);
    }

    #[test]
    fn test_load_nonexistent_uses_defaults() {
        let config = Config::load(Path::new("/nonexistent/path/config.json")).unwrap();
        assert!(config.graph_using_defaults, "nonexistent config should flag defaults");
    }

    #[test]
    fn test_embedding_api_config() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("config.json");
        std::fs::write(
            &p,
            r#"{"data_dir": ".", "profile_id": "default", "conversation": {"enabled": true, "auto_embed": true, "preview_length": 200}, "memory": {"memory_enabled": true, "user_profile_enabled": true, "memory_char_limit": 2200, "user_char_limit": 1375, "security_scan": true}, "search": {"default_top_k": 5, "search_mode": "hybrid", "fts_enabled": true}, "embedding": {"model_name": "test", "dimensions": 768, "batch_size": 10, "api_url": "https://dashscope.aliyuncs.com/compatible-mode/v1", "api_key": "sk-test", "api_model": "text-embedding-v4"}}"#,
        ).unwrap();
        let config = Config::load(&p).unwrap();
        assert_eq!(config.embedding.api_url, "https://dashscope.aliyuncs.com/compatible-mode/v1");
        assert_eq!(config.embedding.api_key, "sk-test");
        assert_eq!(config.embedding.api_model, "text-embedding-v4");
        // Auto-detect DashScope format from URL
        assert_eq!(config.embedding.api_format, "dashscope");
        assert_eq!(config.embedding.batch_size, 10);
    }

    /// v2.6.1 regression: DashScope rejects >10 inputs per embedding request
    /// (HTTP 400). resolve_env must clamp batch_size to 10 when the format is
    /// DashScope, even if the config declares a larger batch_size (default 32) —
    /// else batch embedding (L2 scenario aggregation, rebuild, DB backfill) 400s.
    #[test]
    fn test_dashscope_batch_size_clamped_to_provider_cap() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("config.json");
        std::fs::write(
            &p,
            r#"{"data_dir": ".", "profile_id": "default", "conversation": {"enabled": true, "auto_embed": true, "preview_length": 200}, "memory": {"memory_enabled": true, "user_profile_enabled": true, "memory_char_limit": 2200, "user_char_limit": 1375, "security_scan": true}, "search": {"default_top_k": 5, "search_mode": "hybrid", "fts_enabled": true}, "embedding": {"model_name": "test", "dimensions": 768, "batch_size": 32, "api_url": "https://dashscope.aliyuncs.com/compatible-mode/v1", "api_key": "sk-test", "api_model": "text-embedding-v4"}}"#,
        ).unwrap();
        let config = Config::load(&p).unwrap();
        assert_eq!(config.embedding.api_format, "dashscope");
        assert_eq!(config.embedding.batch_size, 10, "batch_size must clamp to DashScope's 10-input cap");
    }

    #[test]
    fn test_embedding_explicit_format() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("config.json");
        std::fs::write(
            &p,
            r#"{"data_dir": ".", "profile_id": "default", "conversation": {"enabled": true, "auto_embed": true, "preview_length": 200}, "memory": {"memory_enabled": true, "user_profile_enabled": true, "memory_char_limit": 2200, "user_char_limit": 1375, "security_scan": true}, "search": {"default_top_k": 5, "search_mode": "hybrid", "fts_enabled": true}, "embedding": {"model_name": "test", "dimensions": 768, "batch_size": 32, "api_url": "https://dashscope.aliyuncs.com/compatible-mode/v1", "api_key": "sk-test", "api_model": "text-embedding-v4", "api_format": "openai"}}"#,
        ).unwrap();
        let config = Config::load(&p).unwrap();
        // Explicit "openai" overrides auto-detection
        assert_eq!(config.embedding.api_format, "openai");
    }

    #[test]
    fn test_embedding_defaults_no_api() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("config.json");
        std::fs::write(
            &p,
            r#"{"data_dir": ".", "profile_id": "default", "conversation": {"enabled": true, "auto_embed": true, "preview_length": 200}, "memory": {"memory_enabled": true, "user_profile_enabled": true, "memory_char_limit": 2200, "user_char_limit": 1375, "security_scan": true}, "search": {"default_top_k": 5, "search_mode": "hybrid", "fts_enabled": true}, "embedding": {"model_name": "test", "dimensions": 768, "batch_size": 32}}"#,
        ).unwrap();
        let config = Config::load(&p).unwrap();
        assert!(config.embedding.api_url.is_empty());
        assert!(config.embedding.api_model.is_empty());
    }
}

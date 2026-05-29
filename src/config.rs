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
            base_url: std::env::var("AMS_LLM_BASE_URL")
                .or_else(|_| std::env::var("OPENAI_BASE_URL"))
                .unwrap_or_default(),
            api_key: std::env::var("AMS_LLM_API_KEY")
                .or_else(|_| std::env::var("OPENAI_API_KEY"))
                .unwrap_or_default(),
            model: std::env::var("AMS_LLM_MODEL")
                .or_else(|_| std::env::var("OPENAI_MODEL"))
                .unwrap_or_else(|_| "deepseek-v3".to_string()),
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
            api_key: std::env::var("AMS_GATEWAY_API_KEY").unwrap_or_default(),
            cors_origins: vec![],
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
    pub memory_char_limit: usize,
    pub user_char_limit: usize,
    pub security_scan: bool,
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
                memory_char_limit: 2200,
                user_char_limit: 1375,
                security_scan: true,
            },
            search: SearchConfig {
                default_top_k: 5,
                search_mode: "hybrid".to_string(),
                fts_enabled: true,
            },
            embedding: EmbeddingConfig {
                model_name: "embeddinggemma-300m-q8".to_string(),
                dimensions: 768,
                batch_size: 32,
            },
            graph: GraphConfig::default(),
            graph_using_defaults: false,
            db_path: None,
            model_path: None,
            pipeline: PipelineConfig::default(),
            admission: AdmissionConfig::default(),
            recall: RecallConfig::default(),
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
        if path.exists() {
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
            Ok(config)
        } else {
            Ok(Self {
                graph_using_defaults: true,
                ..Self::default()
            })
        }
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
}

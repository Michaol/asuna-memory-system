mod gateway;
mod sections;

pub use gateway::{gateway_bind_addr, validate_gateway_bind, GatewayConfig};
pub use sections::{
    AdmissionConfig, ConversationConfig, EmbeddingConfig, GraphConfig, LlmConfig, MemoryConfig,
    PersonaConfig, PipelineConfig, RecallConfig, ScenarioConfig, SearchConfig,
};

use crate::fact::search::SearchMode;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Top-level configuration (`#[serde(default)]` container: every section and key is optional).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
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
    #[serde(default, skip_serializing)]
    #[allow(dead_code)]
    pub db_path: Option<PathBuf>,

    /// [v3.0.0] 废弃字段：已移除本地嵌入模型，仅保留在线 1024 维嵌入 API。
    #[serde(default, skip_serializing)]
    #[allow(dead_code)]
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

    /// L2 scenario aggregation (opt-in)
    #[serde(default)]
    pub scenarios: ScenarioConfig,

    /// P5: 画像配置
    #[serde(default)]
    pub persona: PersonaConfig,

    /// LLM 配置（用于提取管道）
    #[serde(default)]
    pub llm: LlmConfig,

    /// Gateway 配置（HTTP API 服务器）
    #[serde(default)]
    pub gateway: GatewayConfig,
}

impl Default for Config {
    fn default() -> Self {
        let data_dir = crate::util::dirs_home().join(".asuna");
        Self {
            data_dir,
            profile_id: "default".to_string(),
            conversation: ConversationConfig::default(),
            memory: MemoryConfig::default(),
            search: SearchConfig::default(),
            embedding: EmbeddingConfig::default(),
            graph: GraphConfig::default(),
            graph_using_defaults: false,
            db_path: None,
            model_path: None,
            pipeline: PipelineConfig::default(),
            admission: AdmissionConfig::default(),
            recall: RecallConfig::default(),
            scenarios: ScenarioConfig::default(),
            persona: PersonaConfig::default(),
            llm: LlmConfig::default(),
            gateway: GatewayConfig::default(),
        }
    }
}

fn validate_finite_range(field: &str, value: f64, min: f64, max: f64) -> anyhow::Result<()> {
    if !value.is_finite() || !(min..=max).contains(&value) {
        anyhow::bail!("{field} ({value}) must be finite and within [{min}, {max}]");
    }
    Ok(())
}

impl Config {
    /// 从 JSON 文件加载配置，若不存在则使用默认值
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let mut config = if path.exists() {
            // v4-M: 读/解析错误带上文件路径——裸 "Permission denied" / serde
            // 行号让用户不知道是哪个配置文件出了问题。
            let content = std::fs::read_to_string(path)
                .map_err(|e| anyhow::anyhow!("config file '{}': {e}", path.display()))?;
            let raw: serde_json::Value = serde_json::from_str(&content).map_err(|e| {
                anyhow::anyhow!("config file '{}': invalid JSON: {e}", path.display())
            })?;
            let graph_missing = raw
                .as_object()
                .map(|obj| !obj.contains_key("graph"))
                .unwrap_or(true);
            let mut config: Config = serde_json::from_value(raw)
                .map_err(|e| anyhow::anyhow!("config file '{}': {e}", path.display()))?;
            config.graph_using_defaults = graph_missing;
            let raw_dir = config.data_dir.to_string_lossy();
            let trimmed_dir = raw_dir.trim();
            if trimmed_dir.len() != raw_dir.len() {
                config.data_dir = PathBuf::from(trimmed_dir);
            }
            config.data_dir = expand_tilde(&config.data_dir);
            config
        } else {
            Self {
                graph_using_defaults: true,
                ..Self::default()
            }
        };

        config.llm.resolve_env();
        config.gateway.resolve_env();
        config.embedding.resolve_env();
        config.validate()?;
        Ok(config)
    }

    /// Validate configuration invariants after loading and env resolution.
    pub fn validate(&self) -> anyhow::Result<()> {
        self.validate_core()?;
        self.validate_memory_and_admission()?;
        self.validate_scenarios_and_recall()
    }

    fn validate_core(&self) -> anyhow::Result<()> {
        // W3: 空/纯空白 data_dir 会让 expand_tilde 原样返回 ""，profile_dir 退化为
        // 相对路径 "profiles/default"，create_dir_all("") 又返回 Ok —— 整个数据树
        // 静默落到进程 CWD，换个 cwd 启动就"记忆全丢"。与 profile_id / bind_host
        // 同口径拒绝。
        if self.data_dir.as_os_str().is_empty() || self.data_dir.to_string_lossy().trim().is_empty()
        {
            anyhow::bail!("config.data_dir 不能为空或纯空白（会使数据目录漂移到进程 CWD）");
        }
        if let Err(msg) = crate::util::validate_profile_id(&self.profile_id) {
            anyhow::bail!("config.profile_id '{}': {}", self.profile_id, msg);
        }
        let expected_dim = crate::embedder::EMBEDDING_DIMENSIONS;
        if self.embedding.dimensions != expected_dim {
            anyhow::bail!(
                "config.embedding.dimensions={} 不受支持（v3.0 仅支持 {} 维在线嵌入）。\
                 请将 embedding.dimensions 改为 {}（或删除该字段以使用默认值），然后运行 `asuna-memory rebuild --full` 重建向量索引。",
                self.embedding.dimensions,
                expected_dim,
                expected_dim
            );
        }
        pub const MAX_EMBEDDING_BATCH_SIZE: usize = 1024;
        if self.embedding.batch_size == 0 || self.embedding.batch_size > MAX_EMBEDDING_BATCH_SIZE {
            anyhow::bail!(
                "config.embedding.batch_size must be between 1 and {MAX_EMBEDDING_BATCH_SIZE}"
            );
        }
        if self.conversation.preview_length == 0 {
            anyhow::bail!("config.conversation.preview_length must be >= 1");
        }
        // HTTP `/search`, MCP `search_sessions`, and CLI `search` reject invalid modes up front,
        // so invalid modes must fail at startup instead of waiting for the first search request.
        if let Err(msg) = self.search.search_mode.parse::<SearchMode>() {
            anyhow::bail!("config.search.search_mode: {msg}");
        }
        Ok(())
    }

    fn validate_memory_and_admission(&self) -> anyhow::Result<()> {
        validate_finite_range(
            "config.memory.atom_capacity_ratio",
            self.memory.atom_capacity_ratio,
            0.0,
            1.0,
        )?;
        validate_finite_range(
            "config.admission.threshold",
            self.admission.threshold,
            0.0,
            1.0,
        )?;
        // M6: the sum itself can overflow to +inf even when every weight is finite.
        let weights_sum: f64 = self.admission.weights.iter().sum();
        if self
            .admission
            .weights
            .iter()
            .any(|w| !w.is_finite() || *w < 0.0)
            || !weights_sum.is_finite()
            || weights_sum <= 0.0
        {
            anyhow::bail!(
                "config.admission.weights ({:?}) must contain non-negative finite numbers with a finite sum > 0",
                self.admission.weights
            );
        }
        if self.memory.memory_char_limit == 0 {
            anyhow::bail!("config.memory.memory_char_limit must be >= 1");
        }
        if self.memory.user_char_limit == 0 {
            anyhow::bail!("config.memory.user_char_limit must be >= 1");
        }
        Ok(())
    }

    fn validate_scenarios_and_recall(&self) -> anyhow::Result<()> {
        validate_finite_range(
            "config.scenarios.similarity_threshold",
            f64::from(self.scenarios.similarity_threshold),
            -1.0,
            1.0,
        )?;
        // NEW-2/3/4: 数值字段零/范围检（与 batch_size/preview_length 零检尺标一致）。
        // L2 cluster_atoms 要求 >= 2 才能成簇，min_cluster_size=1 会静默丢掉所有单元素。
        if self.scenarios.min_cluster_size < 2 {
            anyhow::bail!(
                "config.scenarios.min_cluster_size must be >= 2 (singletons cannot form a scenario cluster)"
            );
        }
        if self.scenarios.max_scenarios == 0 {
            anyhow::bail!(
                "config.scenarios.max_scenarios must be >= 1 (0 would evict every scenario each cycle)"
            );
        }
        if self.scenarios.max_scenarios > i64::MAX as usize {
            anyhow::bail!("config.scenarios.max_scenarios exceeds i64::MAX (SQLite LIMIT wrap)");
        }
        if self.recall.token_budget == 0 {
            anyhow::bail!("config.recall.token_budget must be >= 1");
        }
        // v4-M: 零值尺标补齐——top_k=0 恒空结果、char_limit=0 渲染空 .md
        // （与 batch_size/preview_length/token_budget 零检同尺标）。
        if self.search.default_top_k == 0 {
            anyhow::bail!("config.search.default_top_k must be >= 1");
        }
        Ok(())
    }

    /// Create an online API embedder from config.
    /// Returns `None` if `api_url` or `api_model` is unconfigured (keyword-only mode).
    pub fn create_embedder(&self) -> Option<crate::embedder::LazyEmbedder> {
        crate::embedder::LazyEmbedder::from_config(&self.embedding)
    }

    /// 获取 profile 对应的数据目录
    pub fn profile_dir(&self) -> PathBuf {
        self.data_dir.join("profiles").join(&self.profile_id)
    }

    /// 获取对话归档目录（按 profile 隔离）
    pub fn conversations_dir(&self) -> PathBuf {
        self.profile_dir().join("conversations")
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
        // NEW-7 残留：启动清扫上次崩溃遗留的 atomic_write 孤儿 tmp。
        // profile_dir 递归覆盖 conversations/memory/refs 全部写入点；
        // 5 分钟宽限保护并发写者（doctor × gateway）的在途临时文件。
        let removed = crate::util::sweep_orphan_tmp_files(
            &self.profile_dir(),
            std::time::Duration::from_secs(300),
        );
        if removed > 0 {
            tracing::info!(
                "Swept {removed} orphaned atomic_write temp file(s) under {}",
                self.profile_dir().display()
            );
        }
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

pub fn expand_tilde(path: &Path) -> PathBuf {
    let s = path.to_string_lossy();
    if s == "~" {
        crate::util::dirs_home()
    } else if let Some(rest) = s.strip_prefix("~/").or_else(|| s.strip_prefix("~\\")) {
        crate::util::dirs_home().join(rest)
    } else {
        path.to_path_buf()
    }
}

#[cfg(test)]
mod tests;

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

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
///
/// J35/S14d: `idle_timeout_seconds` / `l2_min_interval_seconds` /
/// `enable_warmup` removed — no production reader since P3 design.
/// Container-level `#[serde(default)]`: any subset of keys loads, missing
/// keys take the values below.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PipelineConfig {
    /// Enable automatic L1 extraction
    pub enable_extraction: bool,
    /// Extract every N turns
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
///
/// J35/S14d: `strategy` / `max_results` / `timeout_ms` removed — no
/// production reader (`/recall` picks its top-k from the request with a
/// hardcoded fallback, and retrieval has no timeout knob). `token_budget`
/// stays: it is the default budget applied by the `/recall` handler.
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

/// L2 scenario aggregation configuration (v2.6.1: wired into the pipeline).
/// Scenarios cluster this session's newly-stored atoms by embedding similarity
/// and summarize each cluster via the LLM; the summary is stored as a
/// `memory_type='scenario'` row in bounded_memory so `/recall` L2 surfaces it,
/// plus a human-readable Markdown file under `memory/scenarios/`.
/// Opt-in (default disabled): requires both an LLM and an embedder.
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

/// Persona configuration for the L3-L5 consolidation cycle (P5 / S14b / S14c)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PersonaConfig {
    /// S14b (pipeline Phase 4b), widened by S14c to the whole L3-L5 band:
    /// run one consolidation cycle once at least N sessions (by
    /// `sessions.updated_at`) have been touched since the last persona
    /// write. A fired cycle refreshes `persona.md` (L3), the three
    /// `mental_models/` docs (L4) and the two `intent/` docs (L5) in
    /// sequence — each step independently best-effort — so this value is
    /// the consolidation period for the abstract layers, not just the
    /// persona. 0 = explicitly disabled. Only evaluated when
    /// `scenarios.enabled` — the persona input is the scenario rows.
    pub trigger_every_n: usize,
}

impl Default for PersonaConfig {
    fn default() -> Self {
        Self {
            trigger_every_n: 10,
        }
    }
}

// Privacy configuration (privacy.l0_retention_days / l1_retention_days /
// auto_cleanup) — REMOVED in S14d (J35 re-audit): the whole section never
// had a production reader (L0/L1 retention cleanup was never implemented).
// Old config.json files may still carry the section; serde ignores unknown
// keys, so loading is unaffected.

/// LLM configuration for extraction pipeline
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct LlmConfig {
    /// LLM API base URL (reads from AMS_LLM_BASE_URL or OPENAI_BASE_URL)
    pub base_url: String,
    /// LLM API key (reads from AMS_LLM_API_KEY or OPENAI_API_KEY)
    pub api_key: String,
    /// LLM model name (reads from AMS_LLM_MODEL or OPENAI_MODEL)
    pub model: String,
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
            if let Ok(m) = std::env::var("AMS_LLM_MODEL").or_else(|_| std::env::var("OPENAI_MODEL"))
            {
                self.model = m;
            } else {
                self.model = "deepseek-v3".to_string();
            }
        }
    }
}

/// Gateway configuration for HTTP API server
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct GatewayConfig {
    /// Enable API key authentication
    pub auth_enabled: bool,
    /// API key for authentication (reads from AMS_GATEWAY_API_KEY)
    pub api_key: String,
    /// Allowed CORS origins (empty = allow all, not recommended for production)
    pub cors_origins: Vec<String>,
    /// Interface the gateway binds (U12). "" (the default, also what an absent
    /// key yields) means "unset": resolve_env then fills
    /// AMS_GATEWAY_BIND_HOST, falling back to loopback. A non-loopback bind is
    /// refused at startup unless authentication is enabled — see
    /// [`validate_gateway_bind`].
    #[serde(default)]
    pub bind_host: String,
}

/// Final fallback for `bind_host` when neither config.json nor
/// AMS_GATEWAY_BIND_HOST specifies a host (the "unset" default is "").
fn loopback_bind_host() -> String {
    "127.0.0.1".to_string()
}

impl GatewayConfig {
    /// Fill empty fields from environment variables.
    ///
    /// U11: a non-empty `AMS_GATEWAY_API_KEY` also IMPLIES `auth_enabled =
    /// true`. The env var is the documented way to turn auth on ("Set
    /// AMS_GATEWAY_API_KEY for auth" appeared even in the no-auth startup
    /// warning), yet `auth_enabled` defaults false and used to be the only
    /// real gate — an operator who set just the key got a still-unauthenticated
    /// gateway (fail-open trap). The env key therefore wins over an explicit
    /// config.json `auth_enabled: false` because the override only ever runs in
    /// the secure direction (OFF→ON); to keep auth OFF while a key exists you
    /// must now say so explicitly via `AMS_GATEWAY_AUTH_ENABLED=false`.
    pub fn resolve_env(&mut self) {
        // NB4: trim on assignment — same yardstick as key_from_env below and
        // the startup guard / middleware (`api_key.is_empty()`). An
        // all-whitespace env key is "unset", and a padded key never compares
        // as the configured secret with its whitespace intact.
        if self.api_key.is_empty() {
            self.api_key = std::env::var("AMS_GATEWAY_API_KEY")
                .map(|k| k.trim().to_string())
                .unwrap_or_default();
        }
        // NB8: parse the explicit switch FIRST and defer the env-key
        // implication, so the "enabling authentication" warn only fires when
        // the final state really flips to on (it used to announce enabling for
        // a state AMS_GATEWAY_AUTH_ENABLED=false then suppressed).
        let explicit = match std::env::var("AMS_GATEWAY_AUTH_ENABLED") {
            Ok(v) => match v.trim().to_ascii_lowercase().as_str() {
                "true" | "1" => Some(true),
                "false" | "0" => Some(false),
                "" => None,
                other => {
                    tracing::warn!(
                        "AMS_GATEWAY_AUTH_ENABLED='{other}' is not one of true/false/1/0; ignored"
                    );
                    None
                }
            },
            Err(_) => None,
        };
        if let Some(on) = explicit {
            self.auth_enabled = on;
        }
        // Implication (U11): a non-empty env key wins over a config.json
        // `auth_enabled: false` (secure direction only) — but never over an
        // explicit env switch saying "off".
        let key_from_env = std::env::var("AMS_GATEWAY_API_KEY").is_ok_and(|k| !k.trim().is_empty());
        if key_from_env && !self.auth_enabled {
            if explicit == Some(false) {
                tracing::info!(
                    "AMS_GATEWAY_API_KEY is set but gateway auth stays off: \
                     AMS_GATEWAY_AUTH_ENABLED=false disables it explicitly."
                );
            } else {
                tracing::warn!(
                    "AMS_GATEWAY_API_KEY is set but gateway auth is off; enabling authentication. \
                     Set AMS_GATEWAY_AUTH_ENABLED=false to explicitly keep auth disabled."
                );
                self.auth_enabled = true;
            }
        }
        // U12: explicit config.json value wins; "unset" (absent key/section
        // or the "" default) fills from AMS_GATEWAY_BIND_HOST; a blank or
        // absent env value lands on loopback, never on the all-interfaces
        // default of a socket.
        if self.bind_host.trim().is_empty() {
            self.bind_host = std::env::var("AMS_GATEWAY_BIND_HOST")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(loopback_bind_host);
        }
    }
}

/// U12: startup check for the gateway bind address (pure so the matrix is
/// unit-testable).
///
/// Binding a non-loopback interface without authentication exposes the entire
/// private memory store (readable `/recall`, writable `/capture`) to every
/// host that can reach the port — so it is refused. Loopback addresses
/// (127.0.0.0/8, ::1, localhost) may run unauthenticated. Hostnames cannot be
/// resolved here (pure fn, no DNS), so anything that is not provably loopback
/// — including `0.0.0.0`, `::` and unknown names — requires auth (fail toward
/// the secure direction). `auth_enabled` / `api_key` must be the FINAL
/// post-`resolve_env` state.
pub fn validate_gateway_bind(
    bind_host: &str,
    auth_enabled: bool,
    api_key: &str,
) -> anyhow::Result<()> {
    let host = bind_host.trim();
    if host.is_empty() {
        anyhow::bail!("gateway bind_host is empty");
    }
    if host.contains(char::is_whitespace) {
        anyhow::bail!(
            "gateway bind_host '{bind_host}' contains whitespace; expected a bare host or \
             address (no port)"
        );
    }
    // NB5: an IPv6 literal must be bracket-PAIRED. Unbalanced forms like
    // "[::1" used to be accepted (strip_prefix/strip_suffix each tolerate the
    // missing partner, and the bare IPv6 parsed as loopback) — a typo'd or
    // truncated host must not silently decide the loopback question.
    if host.starts_with('[') != host.ends_with(']') {
        anyhow::bail!(
            "invalid bind_host '{bind_host}': IPv6 literals must be fully bracketed \
             ('[' requires a matching ']')"
        );
    }
    let bare = host.strip_prefix('[').unwrap_or(host);
    let bare = bare.strip_suffix(']').unwrap_or(bare);
    let loopback = match bare.parse::<std::net::IpAddr>() {
        Ok(ip) => ip.is_loopback(),
        Err(_) => {
            if bare.eq_ignore_ascii_case("localhost") {
                true
            } else if host.contains(':') {
                // ':' is only legal in an IPv6 literal, which parsed above or
                // would not reach here without brackets; a bare `a:b` string
                // is almost certainly a host:port pair typed into bind_host.
                anyhow::bail!(
                    "gateway bind_host '{bind_host}' is not a valid address (the port comes \
                     from the CLI/env and must not be part of bind_host)"
                )
            } else {
                // Unresolvable-by-inspection hostname: could bind to any
                // interface, so treat it as non-loopback (require auth).
                false
            }
        }
    };
    if !loopback && !(auth_enabled && !api_key.trim().is_empty()) {
        anyhow::bail!(
            "gateway bind_host '{bind_host}' is not a loopback address; non-loopback binding \
             requires authentication (set AMS_GATEWAY_API_KEY to enable auth, or \
             gateway.auth_enabled=true with a non-empty gateway.api_key)"
        );
    }
    Ok(())
}

/// U12: render the configured host as a `host:port` socket-address string for
/// `TcpListener::bind`, bracketing bare IPv6 literals (a raw `:::8765` fails
/// ToSocketAddrs parsing; `[::]:8765` is the canonical form).
pub fn gateway_bind_addr(bind_host: &str, port: u16) -> String {
    let host = bind_host.trim();
    // NB5-aligned with validate_gateway_bind: a host counts as already
    // bracketed only when the '[' and ']' are paired.
    let bracketed = host.starts_with('[') && host.ends_with(']');
    if host.contains(':') && !bracketed {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

/// Top-level configuration.
///
/// J2/S14d: container-level `#[serde(default)]` — every section and every
/// key is optional. An empty `{}` (or any subset of config.json) loads and
/// missing items take the documented defaults from [`Config::default`] /
/// each sub-struct's `Default`. Unknown keys stay ignored (no
/// `deny_unknown_fields`), which is what makes the S14d field removals
/// backward-compatible for on-disk configs.
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

    /// LLM 配置（用于提取管道）
    #[serde(default)]
    pub llm: LlmConfig,

    /// Gateway 配置（HTTP API 服务器）
    #[serde(default)]
    pub gateway: GatewayConfig,
}

/// Conversation archive configuration.
/// J35/S14d: `enabled` / `auto_embed` removed — no production reader
/// (conversation capture is always on in this build; embedding of turns is
/// gated by whether an embedder exists, not by this flag).
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
/// J35/S14d: `memory_enabled` / `user_profile_enabled` removed — no
/// production reader (both stores are always live; capacity is bounded by
/// the char limits below).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MemoryConfig {
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

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            // 2200 chars ≈ 1100 中文字符 ≈ ~550 tokens (GPT-4 tokenizer)
            // Chosen to fit a single memory file within typical context window
            // budget while leaving room for metadata headers.
            memory_char_limit: 2200,
            // 1375 chars ≈ 687 中文字符 ≈ ~344 tokens
            // Slightly smaller than memory to keep user profile concise
            // for injection into every conversation context.
            user_char_limit: 1375,
            security_scan: true,
            atom_capacity_ratio: default_atom_capacity_ratio(),
        }
    }
}

fn default_atom_capacity_ratio() -> f64 {
    0.3
}

/// Turn-search configuration.
/// J35/S14d: `fts_enabled` removed — no production reader (FTS5 is part of
/// the schema, not a runtime toggle).
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

/// Embedding backend configuration.
/// J35/S14d: `model_name` removed — no production reader (local model dir
/// is discovered by path, API model name lives in `api_model`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct EmbeddingConfig {
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

impl Default for EmbeddingConfig {
    fn default() -> Self {
        Self {
            dimensions: 1024,
            batch_size: 32,
            api_url: String::new(),
            api_key: String::new(),
            api_model: String::new(),
            api_format: String::new(),
        }
    }
}

impl EmbeddingConfig {
    /// Fill API fields from environment variables if empty.
    /// Auto-detect api_format from api_url when not explicitly set.
    pub fn resolve_env(&mut self) {
        if self.api_key.is_empty() {
            self.api_key = std::env::var("AMS_EMBEDDING_API_KEY").unwrap_or_default();
        }
        // Auto-detect DashScope format from URL
        if self.api_format.is_empty()
            && !self.api_url.is_empty()
            && self.api_url.contains("dashscope")
        {
            self.api_format = "dashscope".to_string();
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

impl Default for Config {
    fn default() -> Self {
        let home = dirs_home();
        let data_dir = home.join(".asuna");

        Self {
            data_dir,
            profile_id: "default".to_string(),

            // Section defaults live on each sub-struct (single source of
            // truth for the container-level #[serde(default)] fallbacks).
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
        home.join(
            s.strip_prefix("~/")
                .unwrap_or(s.strip_prefix("~").unwrap_or(&s)),
        )
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
        assert!(
            config.graph_using_defaults,
            "should detect missing graph section"
        );
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
        assert!(
            !config.graph_using_defaults,
            "should not flag when graph section present"
        );
        assert!(!config.graph.enabled);
    }

    #[test]
    fn test_load_nonexistent_uses_defaults() {
        let config = Config::load(Path::new("/nonexistent/path/config.json")).unwrap();
        assert!(
            config.graph_using_defaults,
            "nonexistent config should flag defaults"
        );
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
        assert_eq!(
            config.embedding.api_url,
            "https://dashscope.aliyuncs.com/compatible-mode/v1"
        );
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
        assert_eq!(
            config.embedding.batch_size, 10,
            "batch_size must clamp to DashScope's 10-input cap"
        );
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

    #[test]
    fn test_removed_embedding_keys_ignored() {
        // J35/S14d regression anchor: `embedding.model_name` (no reader) was
        // deleted; a config.json still carrying it must load unchanged
        // (serde ignores unknown keys — no deny_unknown_fields).
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("config.json");
        std::fs::write(
            &p,
            r#"{"embedding": {"model_name": "custom-model", "dimensions": 768}}"#,
        )
        .unwrap();
        let config = Config::load(&p).unwrap();
        assert_eq!(config.embedding.dimensions, 768);
        assert_eq!(config.embedding.batch_size, 32);
    }

    /// J2/S14d: a minimal config.json — every section and key is optional,
    /// container-level `#[serde(default)]` fills documented defaults.
    #[test]
    fn test_empty_config_loads_with_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("config.json");
        std::fs::write(&p, "{}").unwrap();
        let config = Config::load(&p).unwrap();
        assert!(config.data_dir.ends_with(".asuna"));
        assert_eq!(config.profile_id, "default");
        assert_eq!(config.conversation.preview_length, 200);
        assert_eq!(config.memory.memory_char_limit, 2200);
        assert_eq!(config.memory.user_char_limit, 1375);
        assert!(config.memory.security_scan);
        assert!((config.memory.atom_capacity_ratio - 0.3).abs() < f64::EPSILON);
        assert_eq!(config.search.default_top_k, 5);
        assert_eq!(config.search.search_mode, "hybrid");
        assert_eq!(config.embedding.dimensions, 1024);
        assert_eq!(config.embedding.batch_size, 32);
        assert!(config.graph.enabled);
        assert!(config.pipeline.enable_extraction);
        assert_eq!(config.pipeline.every_n_turns, 5);
        assert!(config.admission.enabled);
        assert_eq!(config.recall.token_budget, 2000);
        assert!(!config.scenarios.enabled);
        assert_eq!(config.persona.trigger_every_n, 10);
        // { } has no graph key → the "graph section missing" runtime flag
        // still fires (doctor hint relies on it).
        assert!(config.graph_using_defaults);
    }

    #[test]
    fn test_single_section_subset_loads() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("config.json");
        std::fs::write(&p, r#"{"embedding": {"dimensions": 384}}"#).unwrap();
        let config = Config::load(&p).unwrap();
        assert_eq!(config.embedding.dimensions, 384);
        // sibling keys and unrelated sections take their documented defaults
        assert_eq!(config.embedding.batch_size, 32);
        assert!(config.embedding.api_url.is_empty());
        assert_eq!(config.profile_id, "default");
        assert_eq!(config.recall.token_budget, 2000);
    }

    /// Field-level defaults inside a PRESENT section: keys not mentioned
    /// must come from the section's `Default`, not serde's type defaults
    /// (e.g. `enable_extraction` must stay TRUE, not false).
    #[test]
    fn test_partial_section_keeps_struct_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("config.json");
        std::fs::write(
            &p,
            r#"{"pipeline": {"every_n_turns": 3}, "graph": {"enabled": false}, "memory": {"security_scan": false}}"#,
        )
        .unwrap();
        let config = Config::load(&p).unwrap();
        assert_eq!(config.pipeline.every_n_turns, 3);
        assert!(
            config.pipeline.enable_extraction,
            "bool default must be true, not serde's false"
        );
        assert!(!config.graph.enabled);
        assert!(
            config.graph.remind_on_save,
            "bool default must be true, not serde's false"
        );
        assert!(!config.memory.security_scan);
        assert_eq!(config.memory.memory_char_limit, 2200);
    }

    /// J35/S14d full removal-back-compat anchor: every deleted key
    /// (recall.strategy/max_results/timeout_ms, pipeline.idle_timeout_seconds/
    /// l2_min_interval_seconds/enable_warmup, the whole privacy section,
    /// search.fts_enabled, conversation.enabled/auto_embed,
    /// memory.memory_enabled/user_profile_enabled, embedding.model_name)
    /// must be ignored by an otherwise-valid config.
    #[test]
    fn test_all_removed_keys_still_load() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("config.json");
        std::fs::write(
            &p,
            r#"{
                "data_dir": ".",
                "privacy": {"l0_retention_days": 90, "l1_retention_days": 0, "auto_cleanup": true},
                "recall": {"strategy": "vector", "max_results": 7, "timeout_ms": 111, "token_budget": 1500},
                "pipeline": {"idle_timeout_seconds": 600, "l2_min_interval_seconds": 3600, "enable_warmup": false},
                "search": {"default_top_k": 9, "fts_enabled": false},
                "conversation": {"enabled": false, "auto_embed": false, "preview_length": 50},
                "memory": {"memory_enabled": false, "user_profile_enabled": false},
                "embedding": {"model_name": "old-model", "dimensions": 512}
            }"#,
        )
        .unwrap();
        let config = Config::load(&p).unwrap();
        // surviving keys in the same sections are honored
        assert_eq!(config.recall.token_budget, 1500);
        assert_eq!(config.search.default_top_k, 9);
        assert_eq!(config.conversation.preview_length, 50);
        assert_eq!(config.embedding.dimensions, 512);
        // and the deleted ones left no trace: their old defaults did NOT
        // override the section defaults (warmup false must not disable anything)
        assert!(config.pipeline.enable_extraction);
        assert_eq!(config.pipeline.every_n_turns, 5);
    }

    /// J2: serde-defaulting must not break the documented precedence
    /// config.json explicit > env > built-in default for the env-backed
    /// fields (llm.*; gateway/embedding precedence is covered by the
    /// existing bind-host / api tests below).
    #[test]
    fn test_llm_precedence_config_wins_over_env_wins_over_default() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("config.json");
        let llm_envs: &[(&str, Option<&str>)] = &[
            ("AMS_LLM_BASE_URL", Some("http://from-env")),
            ("AMS_LLM_API_KEY", Some("env-key")),
            ("AMS_LLM_MODEL", Some("env-model")),
        ];
        with_gateway_envs(llm_envs, || {
            // explicit config.json values win over env
            std::fs::write(
                &p,
                r#"{"llm": {"base_url": "http://from-config", "api_key": "cfg-key", "model": "cfg-model"}}"#,
            )
            .unwrap();
            let c = Config::load(&p).unwrap();
            assert_eq!(c.llm.base_url, "http://from-config");
            assert_eq!(c.llm.api_key, "cfg-key");
            assert_eq!(c.llm.model, "cfg-model");
            // section present but fields omitted → serde default ("") lets
            // env fill them
            std::fs::write(&p, r#"{"llm": {}}"#).unwrap();
            let c = Config::load(&p).unwrap();
            assert_eq!(c.llm.base_url, "http://from-env");
            assert_eq!(c.llm.model, "env-model");
        });
        // no config values, no env → resolve_env's built-in model fallback
        let no_envs: &[(&str, Option<&str>)] = &[
            ("AMS_LLM_BASE_URL", None),
            ("AMS_LLM_API_KEY", None),
            ("AMS_LLM_MODEL", None),
        ];
        with_gateway_envs(no_envs, || {
            std::fs::write(&p, "{}").unwrap();
            let c = Config::load(&p).unwrap();
            assert_eq!(c.llm.model, "deepseek-v3");
            assert!(c.llm.base_url.is_empty());
        });
    }

    // ── U11/U12: gateway env resolution + bind validation ──

    /// resolve_env reads process-wide env vars, so these tests serialize on a
    /// mutex and remove every var they touched afterwards (the unsafe blocks
    /// mirror embedder/mod.rs's cross-version set_var style). Other tests in
    /// this file only assert on fields resolve_env never mutates, so a
    /// concurrent Config::load observing a set var cannot change their results.
    static ENV_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn with_gateway_envs<T>(vars: &[(&str, Option<&str>)], f: impl FnOnce() -> T) -> T {
        let _g = ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        for (k, v) in vars {
            match v {
                Some(val) => unsafe { std::env::set_var(k, val) },
                None => unsafe { std::env::remove_var(k) },
            }
        }
        let out = f();
        for (k, _) in vars {
            unsafe { std::env::remove_var(k) };
        }
        out
    }

    const GW_ENVS: &[(&str, Option<&str>)] = &[
        ("AMS_GATEWAY_API_KEY", None),
        ("AMS_GATEWAY_AUTH_ENABLED", None),
        ("AMS_GATEWAY_BIND_HOST", None),
    ];

    #[test]
    fn test_gateway_resolve_env_no_envs() {
        with_gateway_envs(GW_ENVS, || {
            let mut gw = GatewayConfig::default();
            gw.resolve_env();
            assert!(!gw.auth_enabled);
            assert!(gw.api_key.is_empty());
            assert_eq!(gw.bind_host, "127.0.0.1");
        });
    }

    #[test]
    fn test_gateway_resolve_env_key_implies_auth() {
        with_gateway_envs(
            &[
                ("AMS_GATEWAY_API_KEY", Some("s3cret")),
                ("AMS_GATEWAY_AUTH_ENABLED", None),
                ("AMS_GATEWAY_BIND_HOST", None),
            ],
            || {
                let mut gw = GatewayConfig::default();
                gw.resolve_env();
                assert_eq!(gw.api_key, "s3cret");
                assert!(gw.auth_enabled, "env key must imply auth (U11)");
            },
        );
    }

    #[test]
    fn test_gateway_resolve_env_key_overrides_explicit_false() {
        with_gateway_envs(
            &[
                ("AMS_GATEWAY_API_KEY", Some("s3cret")),
                ("AMS_GATEWAY_AUTH_ENABLED", None),
                ("AMS_GATEWAY_BIND_HOST", None),
            ],
            || {
                let mut gw = GatewayConfig {
                    auth_enabled: false,
                    api_key: "from-config".into(),
                    cors_origins: vec![],
                    bind_host: "127.0.0.1".into(),
                };
                gw.resolve_env();
                assert!(
                    gw.auth_enabled,
                    "env key wins over false (secure direction)"
                );
                // config.json key still takes precedence over the env value
                assert_eq!(gw.api_key, "from-config");
            },
        );
    }

    #[test]
    fn test_gateway_resolve_env_auth_enabled_switch() {
        // explicit false keeps auth off even with an env key present
        with_gateway_envs(
            &[
                ("AMS_GATEWAY_API_KEY", Some("s3cret")),
                ("AMS_GATEWAY_AUTH_ENABLED", Some("false")),
                ("AMS_GATEWAY_BIND_HOST", None),
            ],
            || {
                let mut gw = GatewayConfig::default();
                gw.resolve_env();
                assert!(!gw.auth_enabled, "explicit switch outranks the implication");
            },
        );
        // explicit true with no key: auth on (run_gateway then refuses to
        // start without a key — pre-existing fail-closed behavior)
        with_gateway_envs(
            &[
                ("AMS_GATEWAY_API_KEY", None),
                ("AMS_GATEWAY_AUTH_ENABLED", Some("1")),
                ("AMS_GATEWAY_BIND_HOST", None),
            ],
            || {
                let mut gw = GatewayConfig::default();
                gw.resolve_env();
                assert!(gw.auth_enabled);
                assert!(gw.api_key.is_empty());
            },
        );
        // unparseable value: warn + ignore, config state untouched
        with_gateway_envs(
            &[
                ("AMS_GATEWAY_API_KEY", None),
                ("AMS_GATEWAY_AUTH_ENABLED", Some("maybe")),
                ("AMS_GATEWAY_BIND_HOST", None),
            ],
            || {
                let mut gw = GatewayConfig::default();
                gw.resolve_env();
                assert!(!gw.auth_enabled);
            },
        );
        // NB8 matrix: explicit true outranks everything, config value kept off
        // with no key stays off (log order no longer changes these finals)
        with_gateway_envs(
            &[
                ("AMS_GATEWAY_API_KEY", Some("s3cret")),
                ("AMS_GATEWAY_AUTH_ENABLED", Some("true")),
                ("AMS_GATEWAY_BIND_HOST", None),
            ],
            || {
                let mut gw = GatewayConfig::default();
                gw.resolve_env();
                assert!(gw.auth_enabled);
            },
        );
        with_gateway_envs(
            &[
                ("AMS_GATEWAY_API_KEY", None),
                ("AMS_GATEWAY_AUTH_ENABLED", Some("0")),
                ("AMS_GATEWAY_BIND_HOST", None),
            ],
            || {
                let mut gw = GatewayConfig {
                    auth_enabled: true,
                    api_key: "from-config".into(),
                    cors_origins: vec![],
                    bind_host: String::new(),
                };
                gw.resolve_env();
                assert!(!gw.auth_enabled, "explicit env switch outranks config.json");
            },
        );
        // NB4: an all-whitespace env key is "unset" (auth must NOT engage —
        // the startup guard / middleware see the same empty key).
        with_gateway_envs(
            &[
                ("AMS_GATEWAY_API_KEY", Some("   ")),
                ("AMS_GATEWAY_AUTH_ENABLED", None),
                ("AMS_GATEWAY_BIND_HOST", None),
            ],
            || {
                let mut gw = GatewayConfig::default();
                gw.resolve_env();
                assert!(gw.api_key.is_empty(), "whitespace key trims to unset");
                assert!(!gw.auth_enabled, "whitespace-only key must not enable auth");
            },
        );
        // NB4: a padded real key resolves to the trimmed secret (same
        // yardstick as key_from_env) and still implies auth.
        with_gateway_envs(
            &[
                ("AMS_GATEWAY_API_KEY", Some(" s3cret ")),
                ("AMS_GATEWAY_AUTH_ENABLED", None),
                ("AMS_GATEWAY_BIND_HOST", None),
            ],
            || {
                let mut gw = GatewayConfig::default();
                gw.resolve_env();
                assert_eq!(gw.api_key, "s3cret");
                assert!(gw.auth_enabled);
            },
        );
    }

    #[test]
    fn test_gateway_resolve_env_bind_host() {
        with_gateway_envs(
            &[
                ("AMS_GATEWAY_API_KEY", None),
                ("AMS_GATEWAY_AUTH_ENABLED", None),
                ("AMS_GATEWAY_BIND_HOST", Some("0.0.0.0")),
            ],
            || {
                // the "" default (= what Config::load sees when the key or
                // whole section is absent) means "unset", so env fills it…
                let mut gw = GatewayConfig::default();
                gw.resolve_env();
                assert_eq!(gw.bind_host, "0.0.0.0");
                // …but an explicit config.json value wins
                let mut gw = GatewayConfig {
                    bind_host: "127.0.0.5".into(),
                    ..GatewayConfig::default()
                };
                gw.resolve_env();
                assert_eq!(gw.bind_host, "127.0.0.5");
            },
        );
    }

    /// U12's real scenario, end to end through Config::load: the Docker path
    /// ships no config.json (or one without gateway.bind_host), so
    /// AMS_GATEWAY_BIND_HOST must survive to the effective value run_gateway
    /// binds — a hand-built struct state is not enough (the env branch used to
    /// be dead code because every load path pre-filled loopback).
    #[test]
    fn test_gateway_config_load_bind_host_env() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("config.json");
        let load_with_gateway = |gateway_json: Option<&str>| -> Config {
            let mut root = serde_json::json!({
                "data_dir": ".",
                "profile_id": "default",
                "conversation": {"enabled": true, "auto_embed": true, "preview_length": 200},
                "memory": {"memory_enabled": true, "user_profile_enabled": true, "memory_char_limit": 2200, "user_char_limit": 1375, "security_scan": true},
                "search": {"default_top_k": 5, "search_mode": "hybrid", "fts_enabled": true},
                "embedding": {"model_name": "test", "dimensions": 768, "batch_size": 32, "api_url": "https://example.invalid/v1", "api_key": "sk-test", "api_format": "openai"},
            });
            if let Some(g) = gateway_json {
                root["gateway"] = serde_json::from_str(g).unwrap();
            }
            std::fs::write(&p, root.to_string()).unwrap();
            Config::load(&p).unwrap()
        };
        with_gateway_envs(
            &[
                ("AMS_GATEWAY_API_KEY", None),
                ("AMS_GATEWAY_AUTH_ENABLED", None),
                ("AMS_GATEWAY_BIND_HOST", Some("0.0.0.0")),
            ],
            || {
                // gateway section present but bind_host key absent → env wins
                let c = load_with_gateway(Some(
                    r#"{"auth_enabled": true, "api_key": "k", "cors_origins": []}"#,
                ));
                assert_eq!(c.gateway.bind_host, "0.0.0.0");
                // no gateway section at all → env wins
                let c = load_with_gateway(None);
                assert_eq!(c.gateway.bind_host, "0.0.0.0");
                // explicit bind_host in config.json outranks env
                let c = load_with_gateway(Some(
                    r#"{"auth_enabled": true, "api_key": "k", "cors_origins": [], "bind_host": "127.0.0.5"}"#,
                ));
                assert_eq!(c.gateway.bind_host, "127.0.0.5");
            },
        );
        // env unset → loopback fallback on every path
        with_gateway_envs(GW_ENVS, || {
            let c = load_with_gateway(None);
            assert_eq!(c.gateway.bind_host, "127.0.0.1");
            let c = load_with_gateway(Some(
                r#"{"auth_enabled": false, "api_key": "", "cors_origins": []}"#,
            ));
            assert_eq!(c.gateway.bind_host, "127.0.0.1");
        });
    }

    #[test]
    fn test_validate_gateway_bind_loopback_needs_no_auth() {
        for host in [
            "127.0.0.1",
            "127.9.9.9",
            "::1",
            "[::1]",
            "localhost",
            "LOCALhost",
            " 127.0.0.1 ",
        ] {
            validate_gateway_bind(host, false, "")
                .unwrap_or_else(|e| panic!("loopback {host:?} must bind without auth: {e}"));
        }
    }

    #[test]
    fn test_validate_gateway_bind_non_loopback_requires_auth() {
        for host in [
            "0.0.0.0",
            "::",
            "[::]",
            "10.0.0.5",
            "memory.internal.example",
        ] {
            let err = validate_gateway_bind(host, false, "k")
                .err()
                .unwrap_or_else(|| panic!("non-loopback {host:?} without auth must be refused"));
            assert!(
                err.to_string().contains("requires authentication"),
                "host {host:?} → {err}"
            );
            // with auth + key it passes
            validate_gateway_bind(host, true, "k")
                .unwrap_or_else(|e| panic!("non-loopback {host:?} with auth must pass: {e}"));
        }
        // auth "enabled" with an empty key is not authentication
        assert!(validate_gateway_bind("0.0.0.0", true, "").is_err());
        assert!(validate_gateway_bind("0.0.0.0", true, "   ").is_err());
    }

    #[test]
    fn test_validate_gateway_bind_invalid_addresses() {
        assert!(validate_gateway_bind("", false, "").is_err());
        assert!(validate_gateway_bind("   ", false, "").is_err());
        let err = validate_gateway_bind("127.0.0.1:8080", true, "k")
            .expect_err("host:port inside bind_host must be rejected");
        assert!(err.to_string().contains("not a valid address"), "{err}");
        let err = validate_gateway_bind("0.0.0.0:8765", false, "").expect_err("same");
        assert!(err.to_string().contains("not a valid address"), "{err}");
        // NB5: IPv6 brackets must be PAIRED — "[::1" (loopback once stripped)
        // used to slip through and answer the loopback question for a typo.
        let err =
            validate_gateway_bind("[::1", false, "").expect_err("unbalanced '[' must be rejected");
        assert!(err.to_string().contains("invalid bind_host"), "{err}");
        let err =
            validate_gateway_bind("::1]", false, "").expect_err("unbalanced ']' must be rejected");
        assert!(err.to_string().contains("invalid bind_host"), "{err}");
        // paired form stays valid (loopback, no auth needed)
        validate_gateway_bind("[::1]", false, "")
            .unwrap_or_else(|e| panic!("paired IPv6 literal must pass: {e}"));
    }

    #[test]
    fn test_gateway_bind_addr_formatting() {
        assert_eq!(gateway_bind_addr("127.0.0.1", 8765), "127.0.0.1:8765");
        assert_eq!(gateway_bind_addr("localhost", 80), "localhost:80");
        // bare IPv6 must be bracketed, bracketed forms kept verbatim
        assert_eq!(gateway_bind_addr("::1", 8765), "[::1]:8765");
        assert_eq!(gateway_bind_addr("::", 8765), "[::]:8765");
        assert_eq!(gateway_bind_addr("[::1]", 8765), "[::1]:8765");
        assert_eq!(gateway_bind_addr(" 0.0.0.0 ", 8765), "0.0.0.0:8765");
    }
}

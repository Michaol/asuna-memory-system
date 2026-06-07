pub mod onnx;
pub mod tokenizer;
pub mod api;

pub use tokenizer::EmbedTask;

use once_cell::sync::OnceCell;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// ORT 动态库名称（平台特定）
#[cfg(target_os = "windows")]
const ORT_LIB_NAME: &str = "onnxruntime.dll";
#[cfg(target_os = "macos")]
const ORT_LIB_NAME: &str = "libonnxruntime.dylib";
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
const ORT_LIB_NAME: &str = "libonnxruntime.so";

/// 启动时调用：自动发现 ONNX Runtime 动态库并设置 `ORT_DYLIB_PATH`。
///
/// `ort` crate（`load-dynamic` feature）在找不到库时会 **panic**，不是返回 `Result`。
/// 此函数在 `ort::init()` 之前设置 `ORT_DYLIB_PATH` 为绝对路径，
/// 使 `libloading` 能直接找到文件，无需依赖 `LD_LIBRARY_PATH`。
///
/// 搜索顺序：
/// 1. `ORT_DYLIB_PATH` 已设置 → 不操作
/// 2. 可执行文件所在目录（Release tar.gz 解压后 .so 与 binary 同目录）
/// 3. `~/.asuna/lib/`（约定安装位置）
/// 4. 常见系统路径（`/usr/lib`、`/usr/local/lib`）
///
/// 找到后设置 `ORT_DYLIB_PATH=<绝对路径>` 并打印 INFO 日志。
/// 未找到则打印 WARN，后续 `ort::init()` 会 panic（被 `ort_available()` 拦截）。
pub fn init_ort_library_path() {
    // 用户已显式设置，不覆盖
    if std::env::var("ORT_DYLIB_PATH")
        .map(|s| !s.is_empty())
        .unwrap_or(false)
    {
        return;
    }

    let found = search_ort_library();
    if let Some(path) = found {
        // SAFETY: 在 main() 最开始调用，此时无并发线程读取此环境变量。
        // Rust 1.80+ 将 set_var 标记为 unsafe，此处用 unsafe 块保持跨版本兼容。
        unsafe {
            std::env::set_var("ORT_DYLIB_PATH", &path);
        }
        tracing::info!("ORT 动态库自动发现: {}", path.display());
    } else {
        tracing::warn!(
            "ORT 动态库 ({}) 未在已知路径找到。语义搜索不可用。\n\
             修复方式（任选一）：\n\
             1. 将 {} 放到可执行文件同目录\n\
             2. 放到 ~/.asuna/lib/{}\n\
             3. 安装到 /usr/lib 或 /usr/local/lib\n\
             4. 设置 ORT_DYLIB_PATH 环境变量指向 .so 文件路径\n\
             5. 设置 LD_LIBRARY_PATH 包含 .so 所在目录",
            ORT_LIB_NAME, ORT_LIB_NAME, ORT_LIB_NAME,
        );
    }
}

/// 按优先级搜索 ORT 动态库，返回第一个找到的绝对路径
fn search_ort_library() -> Option<PathBuf> {
    // 1. 可执行文件同目录（Release 包解压后 binary 和 .so 在一起）
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let path = dir.join(ORT_LIB_NAME);
            if path.is_file() {
                return Some(path);
            }
        }
    }

    // 2. ~/.asuna/lib/（约定安装位置）
    if let Some(home) = dirs_or_fallback() {
        let path = home.join(".asuna").join("lib").join(ORT_LIB_NAME);
        if path.is_file() {
            return Some(path);
        }
    }

    // 3. 常见系统路径
    for dir in &["/usr/lib", "/usr/local/lib", "/usr/lib64"] {
        let path = Path::new(dir).join(ORT_LIB_NAME);
        if path.is_file() {
            return Some(path);
        }
    }

    None
}

/// 获取用户主目录（不引入额外 crate，使用 HOME 环境变量）
fn dirs_or_fallback() -> Option<PathBuf> {
    std::env::var("HOME").ok().map(PathBuf::from)
}

/// 探测 ONNX Runtime 动态库是否可加载（不触发 ort panic）。
///
/// `ort` crate（`load-dynamic` feature）在找不到 `libonnxruntime.so` 时会 panic，
/// 无法通过 Result 捕获。此函数用 `libloading` 做安全的预探测，
/// 使调用方可优雅降级为关键词搜索，而不是进程崩溃。
///
/// 结果全局缓存（一次探测，终身有效）。
fn ort_available() -> bool {
    static ORT_CHECK: OnceCell<bool> = OnceCell::new();
    *ORT_CHECK.get_or_init(|| {
        // 优先检查 ORT_DYLIB_PATH 环境变量（与 ort crate 行为一致）
        let lib_name = std::env::var("ORT_DYLIB_PATH")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| ORT_LIB_NAME.to_string());
        match unsafe { libloading::Library::new(&lib_name) } {
            Ok(_) => true,
            Err(e) => {
                tracing::warn!(
                    "ONNX Runtime 动态库不可用 ({}): {}。语义搜索将降级为关键词搜索。",
                    lib_name,
                    e
                );
                false
            }
        }
    })
}

/// Internal backend: either local ONNX or remote API
enum Backend {
    Onnx {
        inner: Mutex<Option<onnx::OnnxEmbedder>>,
        model_dir: std::path::PathBuf,
    },
    Api(api::ApiEmbedder),
}

/// Lazy 加载的嵌入器
///
/// Supports two backends:
/// - **local**: ONNX Runtime model (requires model files + libonnxruntime)
/// - **api**: OpenAI-compatible HTTP API (requires api_url + api_model)
///
/// Created via `from_config()` which reads `embedding.provider` from config.
pub struct LazyEmbedder {
    backend: Backend,
    /// Cached load-failure state (only used by Onnx backend)
    load_failed: Mutex<bool>,
    /// Maximum batch size for embedding API calls (read from config)
    batch_size: usize,
    /// Expected output dimension (from config.embedding.dimensions). When set,
    /// embeddings whose length differs are rejected loudly instead of silently
    /// producing vectors that every vec0 insert will reject (empty index).
    expected_dim: Option<usize>,
}

impl LazyEmbedder {
    /// Create from an ONNX model directory (shorthand for local provider).
    /// Kept for backward compatibility with call sites that already have a model path.
    pub fn new(model_dir: &Path) -> Self {
        Self {
            backend: Backend::Onnx {
                inner: Mutex::new(None),
                model_dir: model_dir.to_path_buf(),
            },
            load_failed: Mutex::new(false),
            batch_size: 32,
            expected_dim: None,
        }
    }

    /// Returns the configured batch size for embedding API calls.
    pub fn batch_size(&self) -> usize {
        self.batch_size
    }

    /// Validate an embedding's length against the configured dimension.
    /// Returns an error (rather than silently producing an unindexable vector)
    /// when the model output dimension does not match `config.embedding.dimensions`.
    fn validate_dim(&self, vec: Vec<f32>) -> anyhow::Result<Vec<f32>> {
        if let Some(expected) = self.expected_dim {
            if vec.len() != expected {
                anyhow::bail!(
                    "嵌入维度不匹配：模型输出 {} 维，但 config.embedding.dimensions={}。\
                     请将配置改为 {} 维（并 rebuild --full），或更换与配置匹配的模型。",
                    vec.len(),
                    expected,
                    vec.len()
                );
            }
        }
        Ok(vec)
    }

    /// Create from embedding config. Returns `None` if no backend is available.
    ///
    /// Priority:
    /// 1. **API** — if `api_url` and `api_model` are both set, use the API backend
    /// 2. **Local ONNX** — otherwise, use `model_dir` if available
    /// 3. **None** — if neither is configured, semantic search is disabled
    pub fn from_config(
        config: &crate::config::EmbeddingConfig,
        model_dir: Option<&Path>,
    ) -> Option<Self> {
        // 1. API takes priority when configured
        if !config.api_url.is_empty() && !config.api_model.is_empty() {
            let format = api::ApiFormat::from_str(&config.api_format);
            match api::ApiEmbedder::new(
                &config.api_url,
                &config.api_key,
                &config.api_model,
                config.dimensions,
                format,
            ) {
                Ok(embedder) => {
                    tracing::info!(
                        "Embedding provider: API ({} / {}, format={})",
                        config.api_url,
                        config.api_model,
                        config.api_format
                    );
                    return Some(Self {
                        backend: Backend::Api(embedder),
                        load_failed: Mutex::new(false),
                        batch_size: config.batch_size.max(1),
                        expected_dim: Some(config.dimensions),
                    });
                }
                Err(e) => {
                    tracing::warn!("Failed to create API embedder: {}, falling back to local", e);
                    // Fall through to local ONNX
                }
            }
        }

        // 2. Local ONNX fallback
        match model_dir {
            Some(dir) => {
                let mut embedder = Self::new(dir);
                embedder.batch_size = config.batch_size.max(1);
                embedder.expected_dim = Some(config.dimensions);
                Some(embedder)
            }
            None => {
                tracing::info!("No embedding backend available — semantic search disabled");
                None
            }
        }
    }

    /// Get the ONNX embedder (lazy-loads on first call).
    /// Returns Err if ONNX is not available or backend is not Onnx.
    fn get_onnx_embedder(
        &self,
    ) -> anyhow::Result<std::sync::MutexGuard<'_, Option<onnx::OnnxEmbedder>>> {
        match &self.backend {
            Backend::Onnx { inner, model_dir } => {
                if *self.load_failed.lock().unwrap() {
                    anyhow::bail!("ONNX Runtime 动态库不可用，语义搜索已禁用");
                }
                if !ort_available() {
                    *self.load_failed.lock().unwrap() = true;
                    anyhow::bail!("ONNX Runtime 动态库不可用，语义搜索已禁用");
                }
                let mut guard = inner
                    .lock()
                    .map_err(|e| anyhow::anyhow!("lock poisoned: {}", e))?;
                if guard.is_none() {
                    tracing::info!("首次加载嵌入模型: {}", model_dir.display());
                    *guard = Some(onnx::OnnxEmbedder::new(model_dir)?);
                }
                Ok(guard)
            }
            Backend::Api(_) => anyhow::bail!("当前嵌入后端为 API，不支持 ONNX 操作"),
        }
    }

    /// 生成单个查询向量（搜索路径使用）
    pub fn embed_query(&self, text: &str) -> anyhow::Result<Vec<f32>> {
        let vec = match &self.backend {
            Backend::Onnx { .. } => {
                let mut guard = self.get_onnx_embedder()?;
                guard.as_mut().unwrap().embed(text, EmbedTask::Query)?
            }
            Backend::Api(api) => api.embed(text, true)?,
        };
        self.validate_dim(vec)
    }

    /// 生成单个文档向量（保存对话 / rebuild 路径使用）
    pub fn embed_document(&self, text: &str) -> anyhow::Result<Vec<f32>> {
        let vec = match &self.backend {
            Backend::Onnx { .. } => {
                let mut guard = self.get_onnx_embedder()?;
                guard.as_mut().unwrap().embed(text, EmbedTask::Document)?
            }
            Backend::Api(api) => api.embed(text, false)?,
        };
        self.validate_dim(vec)
    }

    /// 批量生成文档向量
    pub fn embed_documents(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
        let vecs = match &self.backend {
            Backend::Onnx { .. } => {
                let mut guard = self.get_onnx_embedder()?;
                guard.as_mut().unwrap().embed_batch(texts, EmbedTask::Document)?
            }
            Backend::Api(api) => api.embed_batch(texts, false)?,
        };
        vecs.into_iter().map(|v| self.validate_dim(v)).collect()
    }
}

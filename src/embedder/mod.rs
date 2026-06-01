pub mod onnx;
pub mod tokenizer;

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

/// Lazy 加载的嵌入器
pub struct LazyEmbedder {
    inner: Mutex<Option<onnx::OnnxEmbedder>>,
    model_dir: std::path::PathBuf,
    /// 缓存 ort 加载失败状态，避免重复探测
    load_failed: Mutex<bool>,
}

impl LazyEmbedder {
    pub fn new(model_dir: &Path) -> Self {
        Self {
            inner: Mutex::new(None),
            model_dir: model_dir.to_path_buf(),
            load_failed: Mutex::new(false),
        }
    }

    /// 首次调用时加载模型（若 ORT 不可用则返回 Err 并缓存失败状态）
    fn get_embedder(
        &self,
    ) -> anyhow::Result<std::sync::MutexGuard<'_, Option<onnx::OnnxEmbedder>>> {
        // 快速路径：已知失败则直接返回，避免重复探测
        if *self.load_failed.lock().unwrap() {
            anyhow::bail!("ONNX Runtime 动态库不可用，语义搜索已禁用");
        }

        // 预探测 ORT 是否可加载（结果全局缓存）
        if !ort_available() {
            *self.load_failed.lock().unwrap() = true;
            anyhow::bail!("ONNX Runtime 动态库不可用，语义搜索已禁用");
        }

        let mut guard = self
            .inner
            .lock()
            .map_err(|e| anyhow::anyhow!("lock poisoned: {}", e))?;
        if guard.is_none() {
            tracing::info!("首次加载嵌入模型: {}", self.model_dir.display());
            *guard = Some(onnx::OnnxEmbedder::new(&self.model_dir)?);
        }
        Ok(guard)
    }

    /// 生成单个查询向量（搜索路径使用）
    pub fn embed_query(&self, text: &str) -> anyhow::Result<Vec<f32>> {
        let mut guard = self.get_embedder()?;
        guard.as_mut().unwrap().embed(text, EmbedTask::Query)
    }

    /// 生成单个文档向量（保存对话 / rebuild 路径使用）
    pub fn embed_document(&self, text: &str) -> anyhow::Result<Vec<f32>> {
        let mut guard = self.get_embedder()?;
        guard.as_mut().unwrap().embed(text, EmbedTask::Document)
    }

    /// 批量生成文档向量
    pub fn embed_documents(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
        let mut guard = self.get_embedder()?;
        guard.as_mut().unwrap().embed_batch(texts, EmbedTask::Document)
    }
}

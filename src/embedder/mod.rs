pub mod download;
pub mod onnx;
pub mod tokenizer;

use std::sync::Mutex;
use std::path::Path;

/// Lazy 加载的嵌入器
pub struct LazyEmbedder {
    inner: Mutex<Option<onnx::OnnxEmbedder>>,
    model_dir: std::path::PathBuf,
}

impl LazyEmbedder {
    pub fn new(model_dir: &Path) -> Self {
        Self {
            inner: Mutex::new(None),
            model_dir: model_dir.to_path_buf(),
        }
    }

    /// 首次调用时加载模型
    fn get_embedder(&self) -> anyhow::Result<std::sync::MutexGuard<'_, Option<onnx::OnnxEmbedder>>> {
        let mut guard = self.inner.lock().map_err(|e| anyhow::anyhow!("lock poisoned: {}", e))?;
        if guard.is_none() {
            tracing::info!("首次加载嵌入模型: {}", self.model_dir.display());
            *guard = Some(onnx::OnnxEmbedder::new(&self.model_dir)?);
        }
        Ok(guard)
    }

    /// 生成单个文本的嵌入向量
    pub fn embed(&self, text: &str) -> anyhow::Result<Vec<f32>> {
        let mut guard = self.get_embedder()?;
        guard.as_mut().unwrap().embed(text)
    }

    /// 批量生成嵌入向量
    pub fn embed_batch(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
        let mut guard = self.get_embedder()?;
        guard.as_mut().unwrap().embed_batch(texts)
    }

    /// 是否已加载模型
    pub fn is_loaded(&self) -> bool {
        self.inner.lock().map(|g| g.is_some()).unwrap_or(false)
    }
}

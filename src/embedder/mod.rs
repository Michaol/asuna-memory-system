pub mod download;
pub mod onnx;
pub mod tokenizer;

pub use tokenizer::EmbedTask;

use std::path::Path;
use std::sync::Mutex;

/// Lazy 加载的嵌入器
pub struct LazyEmbedder {
    inner: Mutex<Option<onnx::OnnxEmbedder>>,
    model_dir: std::path::PathBuf,
}

#[allow(dead_code)]
impl LazyEmbedder {
    pub fn new(model_dir: &Path) -> Self {
        Self {
            inner: Mutex::new(None),
            model_dir: model_dir.to_path_buf(),
        }
    }

    /// 首次调用时加载模型
    fn get_embedder(
        &self,
    ) -> anyhow::Result<std::sync::MutexGuard<'_, Option<onnx::OnnxEmbedder>>> {
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

    /// 批量生成查询向量
    pub fn embed_queries(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
        let mut guard = self.get_embedder()?;
        guard.as_mut().unwrap().embed_batch(texts, EmbedTask::Query)
    }

    /// 是否已加载模型
    pub fn is_loaded(&self) -> bool {
        self.inner.lock().map(|g| g.is_some()).unwrap_or(false)
    }
}

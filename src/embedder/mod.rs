pub mod api;

pub use api::EmbedApiError;

use std::sync::Arc;

/// Canonical embedding dimensionality enforced across the entire v3 pipeline
/// (API responses, `vec_turns`, and `vec_bounded_memory`).
pub const EMBEDDING_DIMENSIONS: usize = 1024;

/// Lock-free online API embedder wrapper.
///
/// Created via [`LazyEmbedder::from_config`], returning `None` when no online
/// embedding API (`api_url` + `api_model`) is configured (degrading searches to
/// keyword-only mode).
pub struct LazyEmbedder {
    api: api::ApiEmbedder,
    /// Maximum batch size for embedding API calls (read from config)
    batch_size: usize,
    /// Expected output dimension (fixed to `config.embedding.dimensions` = 1024).
    /// Responses whose vector length differs are rejected loudly rather than
    /// failing later on `vec0` insert.
    expected_dim: usize,
}

impl LazyEmbedder {
    /// Create from embedding config. Returns `None` if `api_url` or `api_model`
    /// is empty, or if constructing `ApiEmbedder` fails (keyword-only mode).
    pub fn from_config(config: &crate::config::EmbeddingConfig) -> Option<Self> {
        if config.api_url.is_empty() || config.api_model.is_empty() {
            tracing::info!(
                "No embedding API configured — semantic search disabled (keyword-only mode)"
            );
            return None;
        }

        let format = api::ApiFormat::from_str(&config.api_format);
        // M13: log the canonical parsed format, not the raw config string
        // (which may differ in case/spelling from what from_str resolved).
        let fmt_label = format.as_str();
        let api = match api::ApiEmbedder::new(
            &config.api_url,
            &config.api_key,
            &config.api_model,
            config.dimensions,
            format,
        ) {
            Ok(api) => api,
            Err(e) => {
                tracing::warn!(
                    "Failed to create API embedder: {} — semantic search disabled (keyword-only mode)",
                    e
                );
                return None;
            }
        };

        tracing::info!(
            "Embedding provider: API ({} / {}, format={}, dim={})",
            crate::util::sanitize_url_for_log(&config.api_url),
            config.api_model,
            fmt_label,
            config.dimensions
        );
        Some(Self {
            api,
            batch_size: config.batch_size.max(1),
            expected_dim: config.dimensions,
        })
    }

    /// Returns the configured batch size for embedding API calls.
    pub fn batch_size(&self) -> usize {
        self.batch_size
    }

    /// Validate an embedding's length against the configured dimension.
    fn validate_dim(&self, vec: Vec<f32>) -> anyhow::Result<Vec<f32>> {
        if vec.len() != self.expected_dim {
            anyhow::bail!(
                "嵌入维度不匹配：模型输出 {} 维，但系统要求 {} 维。\
                 请更换输出 1024 维的在线嵌入模型。",
                vec.len(),
                self.expected_dim
            );
        }
        Ok(vec)
    }

    /// 生成单个查询向量（搜索路径使用）
    pub fn embed_query(&self, text: &str) -> anyhow::Result<Vec<f32>> {
        let vec = self.api.embed(text, true)?;
        self.validate_dim(vec)
    }

    /// 生成单个文档向量（保存对话 / rebuild 路径使用）
    pub fn embed_document(&self, text: &str) -> anyhow::Result<Vec<f32>> {
        let vec = self.api.embed(text, false)?;
        self.validate_dim(vec)
    }

    /// 批量生成文档向量
    pub fn embed_documents(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
        let vecs = self.embed_batch_chunked(texts, false)?;
        vecs.into_iter().map(|v| self.validate_dim(v)).collect()
    }

    /// Chunk API batch calls by `batch_size`, preserving input order.
    fn embed_batch_chunked(&self, texts: &[&str], is_query: bool) -> anyhow::Result<Vec<Vec<f32>>> {
        let chunk = self.batch_size.max(1);
        if texts.len() <= chunk {
            return self.api.embed_batch(texts, is_query);
        }
        let mut out = Vec::with_capacity(texts.len());
        for c in texts.chunks(chunk) {
            out.extend(self.api.embed_batch(c, is_query)?);
        }
        Ok(out)
    }
}

// Keep Arc import check clean if downstream uses Option<Arc<LazyEmbedder>>
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Arc<LazyEmbedder>>();
};

/// L2 normalize in-place using `f64` accumulation; rejects `NaN`/`Inf`.
/// Zero vectors remain all zeros and return `Ok(())`.
pub fn l2_normalize(vec: &mut [f32]) -> anyhow::Result<()> {
    if vec.iter().any(|x| !x.is_finite()) {
        anyhow::bail!("embedding contains non-finite value (NaN/Inf)");
    }
    let norm = vec
        .iter()
        .map(|&x| (x as f64) * (x as f64))
        .sum::<f64>()
        .sqrt();
    if norm > 0.0 && norm.is_finite() {
        let inv = 1.0 / norm;
        for x in vec.iter_mut() {
            *x = ((*x as f64) * inv) as f32;
        }
    }
    Ok(())
}

/// Per-vector absmax INT8 quantization for `vec0` `int8[1024]` with `distance_metric=cosine`.
///
/// Scales each vector by `127.0 / max(|v_i|)` so unit-length high-dimensional vectors
/// (whose components average ~`1/sqrt(1024) ≈ 0.03`) preserve full 8-bit angular
/// precision while remaining invariant under cosine distance. Non-finite or
/// all-zero vectors serialize safely to all-zero bytes.
pub fn quantize_to_int8(vec: &[f32]) -> Vec<u8> {
    if vec.is_empty() {
        return Vec::new();
    }
    if vec.iter().any(|v| !v.is_finite()) {
        return vec![0u8; vec.len()];
    }
    let max_abs = vec.iter().fold(0.0f32, |acc, &v| acc.max(v.abs()));
    if max_abs == 0.0 {
        return vec![0u8; vec.len()];
    }
    // M5: scale in f64 — for a subnormal max_abs, `127.0 / max_abs` overflows
    // f32 to +inf and destroys the proportionality of every component.
    let scale = 127.0 / f64::from(max_abs);
    vec.iter()
        .map(|&v| ((f64::from(v) * scale).round().clamp(-127.0, 127.0) as i8) as u8)
        .collect()
}

#[cfg(test)]
mod tests;

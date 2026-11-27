//! Embedding API client supporting OpenAI-compatible and DashScope native formats.
//!
//! ## OpenAI format
//! `POST {api_url}/embeddings` with `{"model","input":["..."],"dimensions":N}`
//! Works with OpenAI, Azure, Ollama, vLLM, LiteLLM, SiliconFlow, DashScope compatible-mode, etc.
//!
//! ## DashScope format
//! `POST {api_url}/services/embeddings/text-embedding/text-embedding`
//! with `{"model","input":{"texts":["..."]},"parameters":{"dimension":N,"text_type":"..."}}`
//! Required by Alibaba DashScope text-embedding-v3/v4 when using the native endpoint.

use crate::util::http_client::{
    append_url_path, build_agent, is_retryable_error, post_json_bytes, read_bounded_body,
    retry_after_delay, sanitize_anyhow_error, HttpStatusError, HttpTimeouts,
};
use serde::{de::DeserializeOwned, Deserialize, Serialize};

/// Maximum response payload accepted from an embedding API (32 MiB).
const MAX_EMBED_RESPONSE_BYTES: usize = 32 * 1024 * 1024;

/// API request/response format
#[derive(Debug, Clone, PartialEq)]
pub enum ApiFormat {
    OpenAI,
    DashScope,
}

impl ApiFormat {
    /// 不可失败解析：未知/空串一律回落 OpenAI（仅 warn），故返回 Self 而非
    /// Result——有意不实现 FromStr（其 Err 关联类型对不可失败解析是误导）。
    /// 保留固有 from_str 名以稳定跨模块调用方（config/tests、main、embedder），
    /// 改名会波及本簇之外的文件。
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Self {
        if s.trim().eq_ignore_ascii_case("dashscope") {
            Self::DashScope
        } else {
            if !s.is_empty() && !s.trim().eq_ignore_ascii_case("openai") {
                tracing::warn!("Unknown embedding api_format '{}', defaulting to openai", s);
            }
            Self::OpenAI
        }
    }

    /// Canonical label for logs/diagnostics — always agrees with `from_str`,
    /// unlike echoing the raw `embedding.api_format` config string.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::OpenAI => "openai",
            Self::DashScope => "dashscope",
        }
    }
}

/// W16: Sanitized error returned by [`ApiEmbedder`] so raw `ureq::Error`
/// URLs (with `userinfo@` / `?query`) are stripped from the `anyhow` error chain
/// while HTTP status remains downcastable for 401/403 backfill abort checks.
#[derive(Debug)]
pub struct EmbedApiError {
    pub status: Option<u16>,
    pub message: String,
}

impl std::fmt::Display for EmbedApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for EmbedApiError {}

/// Embedding API client
pub struct ApiEmbedder {
    endpoint: String,
    api_key: String,
    model: String,
    dimensions: usize,
    format: ApiFormat,
    client: ureq::Agent,
}

// --- OpenAI request/response types ---

// W27d: request fields borrow the caller's strings — no per-item `String`
// clones (or per-request model clone) inside the batch path.
#[derive(Serialize)]
struct OpenAIRequest<'a> {
    model: &'a str,
    input: &'a [&'a str],
    #[serde(skip_serializing_if = "Option::is_none")]
    dimensions: Option<usize>,
}

#[derive(Deserialize, Debug)]
struct OpenAIResponse {
    data: Vec<OpenAIData>,
}

#[derive(Deserialize, Debug)]
struct OpenAIData {
    embedding: Vec<f32>,
    /// OpenAI 规范中的权威位置映射（data[].index）。部分兼容 provider 会省略，
    /// 故为可选；缺失时只能按返回顺序处理（见 `assemble_openai_embeddings`）。
    #[serde(default)]
    index: Option<usize>,
}

// --- DashScope request/response types ---

#[derive(Serialize)]
struct DashScopeRequest<'a> {
    model: &'a str,
    input: DashScopeInput<'a>,
    parameters: DashScopeParameters<'a>,
}

#[derive(Serialize)]
struct DashScopeInput<'a> {
    texts: &'a [&'a str],
}

#[derive(Serialize)]
struct DashScopeParameters<'a> {
    dimension: usize,
    text_type: &'a str,
}

#[derive(Deserialize)]
struct DashScopeResponse {
    output: DashScopeOutput,
}

#[derive(Deserialize)]
struct DashScopeOutput {
    embeddings: Vec<DashScopeEmbedding>,
}

#[derive(Deserialize)]
struct DashScopeEmbedding {
    text_index: usize,
    embedding: Vec<f32>,
}

impl ApiEmbedder {
    pub fn new(
        api_url: &str,
        api_key: &str,
        model: &str,
        dimensions: usize,
        format: ApiFormat,
    ) -> anyhow::Result<Self> {
        // W8: trim at the entry point — trailing whitespace would sit mid-URL
        // after append_url_path and fail every request with a permanent
        // InvalidUrl; a whitespace-padded api_key would likewise sit in the
        // Authorization header value and fail every request with BadHeader.
        // resolve_env already trims config/env values; this covers direct
        // constructors too.
        let api_url = api_url.trim();
        let model = model.trim();
        let api_key = api_key.trim();
        if api_url.is_empty() {
            anyhow::bail!("embedding api_url is empty");
        }
        if model.is_empty() {
            anyhow::bail!("embedding api_model is empty");
        }

        // J28 (parity with the LLM client): a non-https base URL sends the API
        // key + payloads unencrypted. Warn, never reject — Ollama/vLLM local
        // endpoints legitimately use http. Classification is the tested pure
        // helper crate::util::url_scheme_issue.
        if let Some(problem) = crate::util::url_scheme_issue(api_url) {
            tracing::warn!(
                "embedding api_url ({}): {problem}",
                crate::util::sanitize_url_for_log(api_url)
            );
        }

        let endpoint = match format {
            ApiFormat::OpenAI => append_url_path(api_url, "/embeddings"),
            ApiFormat::DashScope => {
                // DashScope native endpoint is at a fixed path under the base URL
                // e.g. https://dashscope.aliyuncs.com/api/v1/services/embeddings/text-embedding/text-embedding
                let (base_part, query_part) = match api_url.split_once('?') {
                    Some((b, q)) => (b.trim_end_matches('/'), Some(q)),
                    None => (api_url.trim_end_matches('/'), None),
                };
                if base_part.ends_with("/text-embedding") {
                    // User provided the full service path already
                    api_url.to_string()
                } else {
                    match query_part {
                        Some(q) => format!(
                            "{base_part}/services/embeddings/text-embedding/text-embedding?{q}"
                        ),
                        None => {
                            format!("{base_part}/services/embeddings/text-embedding/text-embedding")
                        }
                    }
                }
            }
        };

        let client = build_agent(
            &endpoint,
            HttpTimeouts {
                per_read: std::time::Duration::from_secs(30),
                per_write: std::time::Duration::from_secs(10),
                global: std::time::Duration::from_secs(45),
            },
        );

        Ok(Self {
            endpoint,
            api_key: api_key.to_string(),
            model: model.to_string(),
            dimensions,
            format,
            client,
        })
    }

    /// Embed a single text. `is_query` selects the asymmetric DashScope
    /// `text_type` (query vs document); the OpenAI format ignores it.
    pub fn embed(&self, text: &str, is_query: bool) -> anyhow::Result<Vec<f32>> {
        self.embed_batch(&[text], is_query)
            .map(|mut v| v.pop().unwrap_or_default())
    }

    fn wait_before_retry(
        &self,
        attempt: usize,
        max_attempts: usize,
        retry_base_delay_ms: u64,
        last_error: Option<&anyhow::Error>,
    ) {
        let backoff = std::time::Duration::from_millis(retry_base_delay_ms << (attempt - 1));
        let delay = retry_after_delay(last_error, backoff);
        let sanitized_err = last_error.map(|e| self.sanitize_error_message(e));
        tracing::warn!(
            "Retrying embed_batch (attempt {}/{}), waiting {:?} after error: {:?}",
            attempt + 1,
            max_attempts,
            delay,
            sanitized_err
        );
        std::thread::sleep(delay);
    }

    /// Embed a batch of texts with retry logic for network errors.
    /// `is_query` selects the asymmetric DashScope `text_type` (query vs document).
    pub fn embed_batch(&self, texts: &[&str], is_query: bool) -> anyhow::Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(vec![]);
        }

        #[cfg(test)]
        const RETRY_BASE_DELAY_MS: u64 = 10;
        #[cfg(not(test))]
        const RETRY_BASE_DELAY_MS: u64 = 1000;

        let max_attempts = 3;
        let mut last_error: Option<anyhow::Error> = None;

        for attempt in 0..max_attempts {
            if attempt > 0 {
                self.wait_before_retry(
                    attempt,
                    max_attempts,
                    RETRY_BASE_DELAY_MS,
                    last_error.as_ref(),
                );
            }

            let result = match self.format {
                ApiFormat::OpenAI => self.embed_batch_openai(texts),
                ApiFormat::DashScope => self.embed_batch_dashscope(texts, is_query),
            };

            match result {
                Ok(embeddings) => return Ok(embeddings),
                Err(e) if is_retryable_error(&e) => last_error = Some(e),
                Err(e) => return Err(self.to_sanitized_error(e)),
            }
        }

        let e = last_error.unwrap_or_else(|| {
            anyhow::anyhow!("embed_batch failed after {} attempts", max_attempts)
        });
        Err(self.to_sanitized_error(e))
    }

    fn to_sanitized_error(&self, e: anyhow::Error) -> anyhow::Error {
        let status = e
            .downcast_ref::<HttpStatusError>()
            .map(|he| he.status)
            .or_else(|| {
                e.downcast_ref::<ureq::Error>().and_then(|ue| match ue {
                    ureq::Error::StatusCode(code) => Some(*code),
                    _ => None,
                })
            });
        let message = self.sanitize_error_message(&e);
        anyhow::Error::new(EmbedApiError { status, message })
    }

    /// C3 + W16: the endpoint is internal infrastructure and may carry
    /// `userinfo@` or an `?api_key=` query. Delegate URL/credential redaction
    /// before wrapping in `EmbedApiError`.
    fn sanitize_error_message(&self, e: &anyhow::Error) -> String {
        sanitize_anyhow_error(e, &self.endpoint)
    }

    fn embed_batch_openai(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
        let request = OpenAIRequest {
            model: &self.model,
            input: texts,
            dimensions: Some(self.dimensions),
        };

        // M20/W27d: one compact serialization of borrowed fields; the shared
        // post_json_bytes adds Content-Type + Bearer auth and validates status.
        let json = serde_json::to_string(&request)?;
        let response = post_json_bytes(&self.client, &self.endpoint, &self.api_key, &json)?;
        let body: OpenAIResponse = read_json_capped(response)?;

        assemble_openai_embeddings(body.data, texts.len(), self.dimensions)
    }

    fn embed_batch_dashscope(
        &self,
        texts: &[&str],
        is_query: bool,
    ) -> anyhow::Result<Vec<Vec<f32>>> {
        let request = DashScopeRequest {
            model: &self.model,
            input: DashScopeInput { texts },
            parameters: DashScopeParameters {
                dimension: self.dimensions,
                // DashScope v3/v4 are asymmetric: queries must use text_type=query.
                text_type: if is_query { "query" } else { "document" },
            },
        };

        // M20/W27d: 同 embed_batch_openai——一次紧凑序列化 + 共享 post_json_bytes。
        let json = serde_json::to_string(&request)?;
        let response = post_json_bytes(&self.client, &self.endpoint, &self.api_key, &json)?;
        let body: DashScopeResponse = read_json_capped(response)?;

        assemble_dashscope_embeddings(body.output.embeddings, texts.len(), self.dimensions)
    }
}

/// Read JSON response body up to [`MAX_EMBED_RESPONSE_BYTES`], preserving `ureq::Error`
/// directly on stream interruption or timeout so [`is_retryable_error`] can inspect it.
fn read_json_capped<T: DeserializeOwned>(
    response: ureq::http::Response<ureq::Body>,
) -> anyhow::Result<T> {
    let buf = read_bounded_body(response, MAX_EMBED_RESPONSE_BYTES, "embedding API")?;
    serde_json::from_slice(&buf).map_err(anyhow::Error::from)
}

fn extract_indexed_openai_embeddings(
    data: Vec<OpenAIData>,
    expected_len: usize,
) -> anyhow::Result<Vec<Vec<f32>>> {
    let mut slots: Vec<Option<Vec<f32>>> = vec![None; expected_len];
    for (pos, item) in data.into_iter().enumerate() {
        let idx = item.index.ok_or_else(|| {
            anyhow::anyhow!(
                "embedding API mixed indexed and non-indexed items (item {pos} has no index)"
            )
        })?;
        if idx >= expected_len {
            anyhow::bail!(
                "embedding API returned index {idx} out of range for {expected_len} inputs"
            );
        }
        if slots[idx].is_some() {
            anyhow::bail!("embedding API returned duplicate index {idx}");
        }
        slots[idx] = Some(item.embedding);
    }
    slots
        .into_iter()
        .enumerate()
        .map(|(i, s)| s.ok_or_else(|| anyhow::anyhow!("embedding API did not return index {i}")))
        .collect()
}

/// 校验数量 + 按 `index` 重排 + 逐条维度校验 + L2 归一化（纯函数，可离线测试）。
fn assemble_openai_embeddings(
    data: Vec<OpenAIData>,
    expected_len: usize,
    dimensions: usize,
) -> anyhow::Result<Vec<Vec<f32>>> {
    if data.len() != expected_len {
        anyhow::bail!(
            "embedding API returned {} results but sent {} inputs",
            data.len(),
            expected_len
        );
    }

    let mut results: Vec<Vec<f32>> = if data.iter().any(|d| d.index.is_some()) {
        extract_indexed_openai_embeddings(data, expected_len)?
    } else {
        data.into_iter().map(|d| d.embedding).collect()
    };

    for (i, vec) in results.iter_mut().enumerate() {
        if vec.len() != dimensions {
            anyhow::bail!(
                "embedding API returned {} dimensions for item {i} but expected {dimensions}",
                vec.len()
            );
        }
        super::l2_normalize(vec)?;
    }

    Ok(results)
}

/// 校验数量 + `text_index` 构成 `0..expected_len` 完整排列 + 维度校验 + L2 归一化。
fn assemble_dashscope_embeddings(
    embeddings: Vec<DashScopeEmbedding>,
    expected_len: usize,
    dimensions: usize,
) -> anyhow::Result<Vec<Vec<f32>>> {
    if embeddings.len() != expected_len {
        anyhow::bail!(
            "DashScope returned {} embeddings but sent {} inputs",
            embeddings.len(),
            expected_len
        );
    }

    let mut slots: Vec<Option<Vec<f32>>> = vec![None; expected_len];
    for item in embeddings {
        if item.text_index >= expected_len {
            anyhow::bail!(
                "DashScope returned text_index {} out of range for {} inputs",
                item.text_index,
                expected_len
            );
        }
        if slots[item.text_index].is_some() {
            anyhow::bail!(
                "DashScope returned duplicate text_index {}",
                item.text_index
            );
        }
        if item.embedding.len() != dimensions {
            anyhow::bail!(
                "DashScope returned {} dimensions for text_index {} but expected {}",
                item.embedding.len(),
                item.text_index,
                dimensions
            );
        }
        let mut vec = item.embedding;
        super::l2_normalize(&mut vec)?;
        slots[item.text_index] = Some(vec);
    }

    slots
        .into_iter()
        .enumerate()
        .map(|(i, s)| s.ok_or_else(|| anyhow::anyhow!("DashScope did not return text_index {i}")))
        .collect()
}

#[cfg(test)]
#[path = "api_tests.rs"]
mod tests;

//! Embedding API client supporting OpenAI-compatible and DashScope native formats.
//!
//! ## OpenAI format
//! `POST {api_url}/embeddings` with `{"model","input":["..."],"dimensions":N}`
//! Works with OpenAI, Azure, Ollama, vLLM, LiteLLM, SiliconFlow, etc.
//!
//! ## DashScope format
//! `POST {api_url}/services/embeddings/text-embedding/text-embedding`
//! with `{"model","input":{"texts":["..."]},"parameters":{"dimension":N,"text_type":"..."}}`
//! Required by Alibaba DashScope text-embedding-v3/v4 when using the native endpoint.

use serde::{Deserialize, Serialize};

/// API request/response format
#[derive(Debug, Clone, PartialEq)]
pub enum ApiFormat {
    OpenAI,
    DashScope,
}

impl ApiFormat {
    pub fn from_str(s: &str) -> Self {
        match s {
            "dashscope" => Self::DashScope,
            _ => Self::OpenAI,
        }
    }
}

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

#[derive(Serialize)]
struct OpenAIRequest {
    model: String,
    input: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    dimensions: Option<usize>,
}

#[derive(Deserialize)]
struct OpenAIResponse {
    data: Vec<OpenAIData>,
}

#[derive(Deserialize)]
struct OpenAIData {
    embedding: Vec<f32>,
    /// OpenAI 规范中的权威位置映射（data[].index）。部分兼容 provider 会省略，
    /// 故为可选；缺失时只能按返回顺序处理（见 `assemble_openai_embeddings`）。
    #[serde(default)]
    index: Option<usize>,
}

// --- DashScope request/response types ---

#[derive(Serialize)]
struct DashScopeRequest {
    model: String,
    input: DashScopeInput,
    parameters: DashScopeParameters,
}

#[derive(Serialize)]
struct DashScopeInput {
    texts: Vec<String>,
}

#[derive(Serialize)]
struct DashScopeParameters {
    dimension: usize,
    text_type: String,
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
            tracing::warn!("embedding api_url ({api_url}): {problem}");
        }

        let endpoint = match format {
            ApiFormat::OpenAI => api_url.trim_end_matches('/').to_string() + "/embeddings",
            ApiFormat::DashScope => {
                // DashScope native endpoint is at a fixed path under the base URL
                // e.g. https://dashscope.aliyuncs.com/api/v1/services/embeddings/text-embedding/text-embedding
                let base = api_url.trim_end_matches('/');
                if base.ends_with("/text-embedding") {
                    // User provided the full service path already
                    base.to_string()
                } else {
                    base.to_string() + "/services/embeddings/text-embedding/text-embedding"
                }
            }
        };

        let client = ureq::AgentBuilder::new()
            .timeout_read(std::time::Duration::from_secs(30))
            .timeout_write(std::time::Duration::from_secs(10))
            .build();

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

    /// Embed a batch of texts with retry logic for network errors.
    /// `is_query` selects the asymmetric DashScope `text_type` (query vs document).
    pub fn embed_batch(&self, texts: &[&str], is_query: bool) -> anyhow::Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(vec![]);
        }

        // Retry configuration: 3 attempts, backoff sleeps of 1s and 2s before
        // attempts 2 and 3 (a third sleep never happens — attempt 3 failure returns).
        let max_attempts = 3;
        let mut last_error = None;

        for attempt in 0..max_attempts {
            if attempt > 0 {
                let delay_secs = 1 << (attempt - 1); // 1s, 2s
                tracing::warn!(
                    "Retrying embed_batch (attempt {}/{}), waiting {}s after error: {:?}",
                    attempt + 1,
                    max_attempts,
                    delay_secs,
                    last_error.as_ref().map(|e: &anyhow::Error| e.to_string())
                );
                std::thread::sleep(std::time::Duration::from_secs(delay_secs));
            }

            let result = match self.format {
                ApiFormat::OpenAI => self.embed_batch_openai(texts),
                ApiFormat::DashScope => self.embed_batch_dashscope(texts, is_query),
            };

            match result {
                Ok(embeddings) => return Ok(embeddings),
                Err(e) => {
                    // 类型化重试判定（与 memory/llm.rs 同型；5xx 集合更保守，
                    // 仅常见限流/瞬时网关码）：网络错误与瞬时服务端错误重试；
                    // 校验类错误（其余 4xx、本地 JSON/维度）快速失败。
                    if is_retryable_embed_error(&e) {
                        last_error = Some(e);
                        continue; // Retry
                    }
                    return Err(e); // Don't retry validation errors
                }
            }
        }

        // All retries exhausted
        Err(last_error.unwrap_or_else(|| {
            anyhow::anyhow!("embed_batch failed after {} attempts", max_attempts)
        }))
    }

    fn embed_batch_openai(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
        let request = OpenAIRequest {
            model: self.model.clone(),
            input: texts.iter().map(ToString::to_string).collect(),
            dimensions: Some(self.dimensions),
        };

        let mut req = self
            .client
            .post(&self.endpoint)
            .set("Content-Type", "application/json");

        if !self.api_key.is_empty() {
            req = req.set("Authorization", &format!("Bearer {}", self.api_key));
        }

        let response = req.send_json(serde_json::to_value(&request)?)?;
        let body: OpenAIResponse = response.into_json()?;

        assemble_openai_embeddings(body.data, texts.len(), self.dimensions)
    }

    fn embed_batch_dashscope(
        &self,
        texts: &[&str],
        is_query: bool,
    ) -> anyhow::Result<Vec<Vec<f32>>> {
        let request = DashScopeRequest {
            model: self.model.clone(),
            input: DashScopeInput {
                texts: texts.iter().map(ToString::to_string).collect(),
            },
            parameters: DashScopeParameters {
                dimension: self.dimensions,
                // DashScope v3/v4 are asymmetric: queries must use text_type=query.
                text_type: if is_query { "query" } else { "document" }.to_string(),
            },
        };

        let mut req = self
            .client
            .post(&self.endpoint)
            .set("Content-Type", "application/json");

        if !self.api_key.is_empty() {
            req = req.set("Authorization", &format!("Bearer {}", self.api_key));
        }

        let response = req.send_json(serde_json::to_value(&request)?)?;
        let body: DashScopeResponse = response.into_json()?;

        if body.output.embeddings.len() != texts.len() {
            anyhow::bail!(
                "DashScope returned {} embeddings but sent {} inputs",
                body.output.embeddings.len(),
                texts.len()
            );
        }

        // DashScope may return embeddings out of order — sort by text_index
        let mut sorted = body.output.embeddings;
        sorted.sort_by_key(|e| e.text_index);

        let mut results = Vec::with_capacity(sorted.len());
        for (i, item) in sorted.into_iter().enumerate() {
            if item.embedding.len() != self.dimensions {
                anyhow::bail!(
                    "DashScope returned {} dimensions for text_index {} but expected {}",
                    item.embedding.len(),
                    i,
                    self.dimensions
                );
            }
            let mut vec = item.embedding;
            l2_normalize(&mut vec);
            results.push(vec);
        }

        Ok(results)
    }
}

/// 判断嵌入调用错误是否可重试（类型化判定，取代旧的错误字符串 contains 匹配）。
///
/// ureq::Error 经 `?` 转 anyhow::Error 后类型保留，可 downcast 还原：
/// - `Transport(_)`（DNS / 连接失败 / 超时 / IO）→ 重试
/// - `Status(code, _)` 且 code ∈ {429, 500, 502, 503, 504}（限流 / 瞬时服务端错误）→ 重试
/// - 其余 `Status`（400 / 401 / 403 / 404 / 422 等校验类）→ 不重试
///
/// 2xx 响应体读取阶段（`Response::into_json`）的错误以 `std::io::Error` 出现
/// 而非 ureq::Error：读取中途超时 / 连接重置属瞬时故障 → 重试（保持旧字符串
/// 匹配对此场景的行为平价）；JSON 语法错误等 `InvalidData` → 不重试。
/// 其余非 ureq/io 错误（维度校验等本地错误）→ 不重试。
fn is_retryable_embed_error(e: &anyhow::Error) -> bool {
    if let Some(ue) = e.downcast_ref::<ureq::Error>() {
        return match ue {
            ureq::Error::Transport(_) => true,
            ureq::Error::Status(code, _) => matches!(*code, 429 | 500 | 502 | 503 | 504),
        };
    }
    if let Some(io) = e.downcast_ref::<std::io::Error>() {
        return matches!(
            io.kind(),
            std::io::ErrorKind::TimedOut
                | std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::ConnectionAborted
                | std::io::ErrorKind::UnexpectedEof
        );
    }
    false
}

/// 校验数量 + 按 index 重排 + 逐条维度校验 + L2 归一化（纯函数，可离线测试）。
///
/// 映射规则（修复 U6：OpenAI 兼容端点乱序返回导致向量静默错配）：
/// - 全部 `index` 为 None：按 API 返回顺序处理（兼容不规范 provider，保持 v2.6 前行为）；
/// - 任一 `index` 为 Some：要求全部为 Some 且恰构成 0..expected_len 的排列，
///   越界 / 重复 / 缺失均报错，向量按 index 归位（对齐 DashScope 的 text_index 处理）。
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
        let mut slots: Vec<Option<Vec<f32>>> = (0..expected_len).map(|_| None).collect();
        for (pos, item) in data.into_iter().enumerate() {
            let idx = match item.index {
                Some(i) => i,
                None => anyhow::bail!(
                    "embedding API mixed indexed and non-indexed items (item {pos} has no index)"
                ),
            };
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
        // 条数相符 + 无重复 + 无越界 ⇒ 必为完整排列；此分支仅为防御（逻辑上不可达）
        slots
            .into_iter()
            .enumerate()
            .map(|(i, s)| {
                s.ok_or_else(|| anyhow::anyhow!("embedding API did not return index {i}"))
            })
            .collect::<anyhow::Result<Vec<_>>>()?
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
        l2_normalize(vec);
    }

    Ok(results)
}

/// L2 normalize in-place (idempotent if already unit-length)
fn l2_normalize(vec: &mut [f32]) {
    let norm: f32 = vec.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        let inv = 1.0 / norm;
        for x in vec.iter_mut() {
            *x *= inv;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(index: Option<usize>, embedding: Vec<f32>) -> OpenAIData {
        OpenAIData { index, embedding }
    }

    /// 由字符串构造 ureq::Response，组装 Error::Status（ureq 2.x 无公开 Status 构造器，
    /// FromStr for Response 是官方推荐的测试构造方式）。
    fn status_error(code: u16) -> ureq::Error {
        let raw = format!("HTTP/1.1 {code} Test\r\n\r\n");
        let resp: ureq::Response = raw.parse().expect("test response should parse");
        ureq::Error::Status(code, resp)
    }

    // --- U6: assemble_openai_embeddings ---

    #[test]
    fn test_assemble_indexed_ascending() {
        let data = vec![item(Some(0), vec![2.0, 0.0]), item(Some(1), vec![0.0, 4.0])];
        let out = assemble_openai_embeddings(data, 2, 2).unwrap();
        assert_eq!(out, vec![vec![1.0, 0.0], vec![0.0, 1.0]]);
    }

    /// 乱序返回时按 index 归位（而非按返回顺序）——U6 的核心回归测试。
    #[test]
    fn test_assemble_indexed_out_of_order_remapped() {
        let data = vec![
            item(Some(2), vec![0.0, 0.0]),
            item(Some(0), vec![3.0, 0.0]),
            item(Some(1), vec![0.0, 5.0]),
        ];
        let out = assemble_openai_embeddings(data, 3, 2).unwrap();
        assert_eq!(
            out,
            vec![vec![1.0, 0.0], vec![0.0, 1.0], vec![0.0, 0.0]],
            "向量必须按 index 归位，而非返回顺序"
        );
    }

    #[test]
    fn test_assemble_all_none_preserves_return_order() {
        let data = vec![item(None, vec![0.0, 3.0]), item(None, vec![4.0, 0.0])];
        let out = assemble_openai_embeddings(data, 2, 2).unwrap();
        assert_eq!(out, vec![vec![0.0, 1.0], vec![1.0, 0.0]]);
    }

    #[test]
    fn test_assemble_duplicate_index_errors() {
        let data = vec![item(Some(0), vec![1.0, 0.0]), item(Some(0), vec![0.0, 1.0])];
        let err = assemble_openai_embeddings(data, 2, 2).unwrap_err();
        assert!(err.to_string().contains("duplicate index"));
    }

    #[test]
    fn test_assemble_out_of_range_index_errors() {
        let data = vec![item(Some(5), vec![1.0, 0.0])];
        let err = assemble_openai_embeddings(data, 1, 2).unwrap_err();
        assert!(err.to_string().contains("out of range"));
    }

    #[test]
    fn test_assemble_missing_index_mixed_errors() {
        let data = vec![item(Some(0), vec![1.0, 0.0]), item(None, vec![0.0, 1.0])];
        let err = assemble_openai_embeddings(data, 2, 2).unwrap_err();
        assert!(err.to_string().contains("no index"));
    }

    #[test]
    fn test_assemble_count_mismatch_errors() {
        let data = vec![item(Some(0), vec![1.0, 0.0])];
        let err = assemble_openai_embeddings(data, 2, 2).unwrap_err();
        assert!(err.to_string().contains("results but sent"));
    }

    #[test]
    fn test_assemble_dimension_mismatch_errors() {
        let data = vec![item(Some(0), vec![1.0, 0.0, 3.0])];
        let err = assemble_openai_embeddings(data, 1, 2).unwrap_err();
        assert!(err.to_string().contains("dimensions for item 0"));
    }

    // --- J16: is_retryable_embed_error ---

    /// `?` 转 anyhow 后 downcast 仍能还原 ureq::Error——重试判定的前提。
    #[test]
    fn test_anyhow_downcast_preserves_ureq_error() {
        let e: anyhow::Error = status_error(429).into();
        assert!(matches!(
            e.downcast_ref::<ureq::Error>(),
            Some(ureq::Error::Status(429, _))
        ));
    }

    /// Transport 错误构造：请求一个非法 URL。ureq 在发起任何网络 I/O 之前就
    /// URL 解析失败并返回 Error::Transport(InvalidUrl)——完全离线确定性。
    /// （ureq 2.12.1 的 Transport 字段为私有、无公开构造器，故经此路径取得真实实例。）
    #[test]
    fn test_retryable_transport_error() {
        let err = ureq::get("broken/url").call().unwrap_err();
        assert!(matches!(err, ureq::Error::Transport(_)));
        let e: anyhow::Error = err.into();
        assert!(is_retryable_embed_error(&e), "Transport 错误应重试");
    }

    #[test]
    fn test_retryable_status_codes() {
        for code in [429u16, 500, 502, 503, 504] {
            let e: anyhow::Error = status_error(code).into();
            assert!(is_retryable_embed_error(&e), "{code} 应可重试");
        }
    }

    #[test]
    fn test_non_retryable_status_codes() {
        for code in [400u16, 401, 403, 404, 422] {
            let e: anyhow::Error = status_error(code).into();
            assert!(
                !is_retryable_embed_error(&e),
                "{code} 属校验类错误，不应重试"
            );
        }
    }

    #[test]
    fn test_non_ureq_local_error_not_retryable() {
        let e: anyhow::Error = anyhow::anyhow!("维度校验失败（本地错误）");
        assert!(!is_retryable_embed_error(&e), "非 ureq 本地错误不应重试");
    }

    /// 2xx 响应体读取阶段的瞬时 io 错误应重试（与旧字符串匹配行为平价）；
    /// JSON 语法类 InvalidData 不重试。
    #[test]
    fn test_retryable_io_error_kinds() {
        use std::io::ErrorKind;
        for kind in [
            ErrorKind::TimedOut,
            ErrorKind::ConnectionReset,
            ErrorKind::ConnectionAborted,
            ErrorKind::UnexpectedEof,
        ] {
            let e: anyhow::Error = anyhow::Error::new(std::io::Error::new(kind, "body stalled"));
            assert!(is_retryable_embed_error(&e), "{kind:?} 应重试");
        }
        let e: anyhow::Error =
            anyhow::Error::new(std::io::Error::new(ErrorKind::InvalidData, "bad json"));
        assert!(
            !is_retryable_embed_error(&e),
            "InvalidData（JSON 解析失败）不应重试"
        );
    }
}

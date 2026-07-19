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

        let endpoint = match format {
            ApiFormat::OpenAI => {
                api_url.trim_end_matches('/').to_string() + "/embeddings"
            }
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

        // Retry configuration: 3 attempts with exponential backoff (1s, 2s, 4s)
        let max_attempts = 3;
        let mut last_error = None;

        for attempt in 0..max_attempts {
            if attempt > 0 {
                let delay_secs = 1 << (attempt - 1); // 1, 2, 4 seconds
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
                    let error_str = e.to_string();
                    // Only retry on network errors (connection reset, timeout, etc.)
                    // Don't retry on API errors (4xx/5xx), validation errors, etc.
                    let is_network_error = error_str.contains("Connection reset")
                        || error_str.contains("Connection refused")
                        || error_str.contains("Timed out")
                        || error_str.contains("Network error")
                        || error_str.contains("os error");

                    if is_network_error {
                        last_error = Some(e);
                        continue; // Retry
                    } else {
                        return Err(e); // Don't retry API errors
                    }
                }
            }
        }

        // All retries exhausted
        Err(last_error.unwrap_or_else(|| anyhow::anyhow!("embed_batch failed after {} attempts", max_attempts)))
    }

    fn embed_batch_openai(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
        let request = OpenAIRequest {
            model: self.model.clone(),
            input: texts.iter().map(|s| s.to_string()).collect(),
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

        if body.data.len() != texts.len() {
            anyhow::bail!(
                "embedding API returned {} results but sent {} inputs",
                body.data.len(),
                texts.len()
            );
        }

        let mut results = Vec::with_capacity(body.data.len());
        for (i, item) in body.data.into_iter().enumerate() {
            if item.embedding.len() != self.dimensions {
                anyhow::bail!(
                    "embedding API returned {} dimensions for item {} but expected {}",
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

    fn embed_batch_dashscope(&self, texts: &[&str], is_query: bool) -> anyhow::Result<Vec<Vec<f32>>> {
        let request = DashScopeRequest {
            model: self.model.clone(),
            input: DashScopeInput {
                texts: texts.iter().map(|s| s.to_string()).collect(),
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

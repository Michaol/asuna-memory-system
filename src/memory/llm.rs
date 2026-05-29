//! LLM API client for memory extraction pipeline
//!
//! Uses OpenAI-compatible chat/completions API via ureq.
//! Reads configuration from environment variables.

use serde::{Deserialize, Serialize};
use std::io::Read;

/// LLM client for extraction pipeline
pub struct LlmClient {
    base_url: String,
    api_key: String,
    model: String,
    agent: ureq::Agent,
}

#[derive(Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<ChatMessage>,
    temperature: f64,
}

#[derive(Serialize)]
struct ChatMessage {
    role: String,
    content: String,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Choice {
    message: ResponseMessage,
}

#[derive(Deserialize)]
struct ResponseMessage {
    content: Option<String>,
}

impl LlmClient {
    /// Create a new LLM client with explicit parameters (for testing)
    pub fn new(base_url: &str, api_key: &str, model: &str) -> Self {
        Self {
            base_url: base_url.to_string(),
            api_key: api_key.to_string(),
            model: model.to_string(),
            agent: ureq::AgentBuilder::new()
                .timeout(std::time::Duration::from_secs(60))
                .build(),
        }
    }

    /// Create a new LLM client from environment variables.
    /// Returns None if required env vars are missing (Lite mode).
    pub fn from_env() -> Option<Self> {
        let base_url = std::env::var("AMS_LLM_BASE_URL")
            .or_else(|_| std::env::var("OPENAI_BASE_URL"))
            .ok()?;
        let api_key = std::env::var("AMS_LLM_API_KEY")
            .or_else(|_| std::env::var("OPENAI_API_KEY"))
            .ok()?;
        let model = std::env::var("AMS_LLM_MODEL")
            .or_else(|_| std::env::var("OPENAI_MODEL"))
            .unwrap_or_else(|_| "deepseek-v3".to_string());

        if base_url.is_empty() || api_key.is_empty() {
            return None;
        }

        Some(Self {
            base_url,
            api_key,
            model,
            agent: ureq::AgentBuilder::new()
                .timeout(std::time::Duration::from_secs(60))
                .build(),
        })
    }

    /// Check if the client is configured
    pub fn is_available(&self) -> bool {
        !self.base_url.is_empty() && !self.api_key.is_empty()
    }

    /// Send a chat completion request with retry logic
    pub fn chat(&self, system: &str, user: &str) -> anyhow::Result<String> {
        let url = format!(
            "{}/chat/completions",
            self.base_url.trim_end_matches('/')
        );

        let request = ChatRequest {
            model: self.model.clone(),
            messages: vec![
                ChatMessage {
                    role: "system".to_string(),
                    content: system.to_string(),
                },
                ChatMessage {
                    role: "user".to_string(),
                    content: user.to_string(),
                },
            ],
            temperature: 0.3,
        };

        let request_json = serde_json::to_string(&request)?;

        // Retry logic with exponential backoff
        let max_retries = 3;
        let base_delay = std::time::Duration::from_secs(1);

        for attempt in 0..max_retries {
            match self
                .agent
                .post(&url)
                .set("Authorization", &format!("Bearer {}", self.api_key))
                .set("Content-Type", "application/json")
                .send_string(&request_json)
            {
                Ok(response) => {
                    let mut response_text = String::new();
                    response.into_reader().read_to_string(&mut response_text)?;
                    let response: ChatResponse = serde_json::from_str(&response_text)?;

                    return response
                        .choices
                        .first()
                        .and_then(|c| c.message.content.as_deref())
                        .map(|s: &str| s.to_string())
                        .ok_or_else(|| anyhow::anyhow!("LLM returned empty response"));
                }
                Err(e) => {
                    // Check if error is retryable
                    let is_retryable = match &e {
                        ureq::Error::Transport(_) => true,
                        ureq::Error::Status(code, _) => {
                            // Retry on 5xx errors and 429 (rate limit)
                            matches!(*code, 429 | 500..=599)
                        }
                    };

                    if !is_retryable || attempt == max_retries - 1 {
                        return Err(anyhow::anyhow!("LLM API call failed: {}", e));
                    }

                    // Exponential backoff: 1s, 2s, 4s
                    let delay = base_delay * 2u32.pow(attempt as u32);
                    tracing::warn!(
                        "LLM API call failed (attempt {}/{}), retrying in {:?}: {}",
                        attempt + 1,
                        max_retries,
                        delay,
                        e
                    );
                    std::thread::sleep(delay);
                }
            }
        }

        Err(anyhow::anyhow!(
            "LLM API call failed after {} retries",
            max_retries
        ))
    }

    /// Send a chat completion and parse JSON response
    pub fn chat_json<T: serde::de::DeserializeOwned>(
        &self,
        system: &str,
        user: &str,
    ) -> anyhow::Result<T> {
        let content = self.chat(system, user)?;
        let json_str = extract_json_from_response(&content);
        serde_json::from_str(&json_str).map_err(|e| {
            anyhow::anyhow!(
                "Failed to parse LLM JSON response: {} — raw: {}",
                e,
                content
            )
        })
    }
}

/// Extract JSON from LLM response (handles markdown code blocks)
fn extract_json_from_response(content: &str) -> String {
    let trimmed = content.trim();

    // Try to extract from ```json code block
    if let Some(start) = trimmed.find("```json") {
        let after = &trimmed[start + 7..];
        if let Some(end) = after.find("```") {
            return after[..end].trim().to_string();
        }
    }

    // Try to extract from ``` code block
    if let Some(start) = trimmed.find("```") {
        let after = &trimmed[start + 3..];
        if let Some(end) = after.find("```") {
            let candidate = after[..end].trim();
            if candidate.starts_with('[') || candidate.starts_with('{') {
                return candidate.to_string();
            }
        }
    }

    // Raw JSON
    trimmed.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_json_from_markdown_block() {
        let input = "```json\n{\"key\": \"value\"}\n```";
        assert_eq!(
            extract_json_from_response(input),
            "{\"key\": \"value\"}"
        );
    }

    #[test]
    fn test_extract_json_from_code_block() {
        let input = "```\n[{\"key\": \"value\"}]\n```";
        assert_eq!(
            extract_json_from_response(input),
            "[{\"key\": \"value\"}]"
        );
    }

    #[test]
    fn test_extract_raw_json() {
        let input = "{\"key\": \"value\"}";
        assert_eq!(extract_json_from_response(input), "{\"key\": \"value\"}");
    }

    #[test]
    fn test_extract_json_array() {
        let input = "[1, 2, 3]";
        assert_eq!(extract_json_from_response(input), "[1, 2, 3]");
    }
}

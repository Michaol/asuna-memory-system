//! LLM API client for memory extraction pipeline
//!
//! Uses OpenAI-compatible chat/completions API via ureq.
//! Configured via `crate::config::LlmConfig` (`LlmClient::from_config`).

use crate::util::http_client::{
    append_url_path, build_agent, is_retryable_error, post_json_bytes, read_bounded_body,
    retry_after_delay, sanitize_anyhow_error, HttpTimeouts,
};
use serde::{Deserialize, Serialize};

/// Maximum response payload accepted from an LLM API (8 MiB).
const MAX_LLM_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

fn build_http_agent(base_url: &str) -> ureq::Agent {
    build_agent(
        base_url,
        HttpTimeouts {
            per_read: std::time::Duration::from_secs(60),
            per_write: std::time::Duration::from_secs(15),
            global: std::time::Duration::from_secs(90),
        },
    )
}

/// LLM client for extraction pipeline
#[derive(Clone)]
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
    #[cfg(test)]
    pub fn new(base_url: &str, api_key: &str, model: &str) -> Self {
        // W15: trim at the entry point, same ruler as ApiEmbedder::new (W8).
        let base_url = base_url.trim();
        Self {
            base_url: base_url.to_string(),
            api_key: api_key.trim().to_string(),
            model: model.trim().to_string(),
            agent: build_http_agent(base_url),
        }
    }

    /// Create from an already-resolved LlmConfig.
    ///
    /// env 兜底不在这里：`Config::load` 已调 `LlmConfig::resolve_env`（读
    /// AMS_LLM_* / OPENAI_* 并 trim），本函数只查空 + 构造。
    /// Returns None if base_url or api_key is empty.
    pub fn from_config(cfg: &crate::config::LlmConfig) -> Option<Self> {
        // W15: trim consistently with `new` — resolve_env already trims, this
        // covers direct/programmatic config values too.
        let base_url = cfg.base_url.trim();
        let api_key = cfg.api_key.trim();
        let model = cfg.model.trim();
        if base_url.is_empty() || api_key.is_empty() {
            return None;
        }
        // J28: warn (never reject — local dev endpoints legitimately use http)
        // when the configured base URL would send the API key + conversation
        // content in cleartext or under an unexpected scheme.
        warn_on_base_url_scheme("llm.base_url", base_url);
        // model 默认值（"deepseek-v3"）只在 resolve_env 一处兜底：它保证 model
        // 非空，此处再写一遍默认值既是重复硬编码也是不可达分支。
        Some(Self {
            base_url: base_url.to_string(),
            api_key: api_key.to_string(),
            model: model.to_string(),
            agent: build_http_agent(base_url),
        })
    }
}

fn read_chat_response_content(
    response: ureq::http::Response<ureq::Body>,
) -> Result<String, anyhow::Error> {
    let buf = read_bounded_body(response, MAX_LLM_RESPONSE_BYTES, "LLM API")?;
    let response: ChatResponse = serde_json::from_slice(&buf).map_err(|e| {
        anyhow::anyhow!(
            "LLM API response envelope parse failed ({} bytes): {}",
            buf.len(),
            e
        )
    })?;
    response
        .choices
        .first()
        .and_then(|c| c.message.content.as_deref())
        .map(ToString::to_string)
        .ok_or_else(|| anyhow::anyhow!("LLM returned empty response"))
}

enum ChatAttempt {
    Ok(String),
    Fatal(anyhow::Error),
    Retry(anyhow::Error),
}

impl LlmClient {
    fn attempt_chat(&self, url: &str, request_json: &str, is_last: bool) -> ChatAttempt {
        // M12a: shared POST + status validation; transport and status errors
        // both stay downcastable in the anyhow chain for is_retryable_error.
        let ok_resp = match post_json_bytes(&self.agent, url, &self.api_key, request_json) {
            Ok(r) => r,
            Err(e) => {
                if !is_retryable_error(&e) || is_last {
                    return ChatAttempt::Fatal(anyhow::anyhow!(
                        "LLM API call failed: {}",
                        sanitize_llm_error_for_log(&e, url)
                    ));
                }
                return ChatAttempt::Retry(e);
            }
        };

        match read_chat_response_content(ok_resp) {
            Ok(text) => ChatAttempt::Ok(text),
            Err(err) => {
                // W21b: every fatal surface is sanitized + truncated — raw
                // endpoint URLs or unbounded response bodies must never leak.
                if !is_retryable_error(&err) || is_last {
                    return ChatAttempt::Fatal(anyhow::anyhow!(
                        "LLM API response read failed: {}",
                        sanitize_llm_error_for_log(&err, url)
                    ));
                }
                ChatAttempt::Retry(err)
            }
        }
    }

    /// Send a chat completion request with retry logic
    pub fn chat(&self, system: &str, user: &str) -> anyhow::Result<String> {
        let url = append_url_path(&self.base_url, "/chat/completions");

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

        #[cfg(test)]
        const RETRY_BASE_DELAY_MS: u64 = 10;
        #[cfg(not(test))]
        const RETRY_BASE_DELAY_MS: u64 = 1000;

        let max_retries = 3u32;
        let base_delay = std::time::Duration::from_millis(RETRY_BASE_DELAY_MS);

        for attempt in 0..max_retries {
            let err = match self.attempt_chat(&url, &request_json, attempt == max_retries - 1) {
                ChatAttempt::Ok(text) => return Ok(text),
                ChatAttempt::Fatal(e) => return Err(e),
                ChatAttempt::Retry(e) => e,
            };
            let backoff = base_delay * 2u32.pow(attempt);
            let delay = retry_after_delay(Some(&err), backoff);
            tracing::warn!(
                "LLM API call failed (attempt {}/{}), retrying in {:?}: {}",
                attempt + 1,
                max_retries,
                delay,
                sanitize_llm_error_for_log(&err, &url)
            );
            std::thread::sleep(delay);
        }
        Err(anyhow::anyhow!("LLM API call failed after retries"))
    }

    /// Send a chat completion and parse JSON response
    pub fn chat_json<T: serde::de::DeserializeOwned>(
        &self,
        system: &str,
        user: &str,
    ) -> anyhow::Result<T> {
        let content = self.chat(system, user)?;
        parse_json_response(&content)
    }
}

/// Extract and deserialize JSON from raw LLM text (supports markdown code blocks).
pub(crate) fn parse_json_response<T: serde::de::DeserializeOwned>(
    content: &str,
) -> anyhow::Result<T> {
    let trimmed = content.trim();
    // W9: 围栏解析失败必须**落回**下面的平衡扫描循环。模型可能先输出一个"思考"
    // 围栏再输出真载荷；extract_fenced_block 只看首字符是否 '{'/'['，不校验能否
    // 反序列化为 T，旧实现锁定首个围栏即 return → 整会话 L1 抽取失败。
    let mut offset = 0usize;
    if let Some((inner, after_fence)) = extract_fenced_block_with_end(trimmed) {
        if let Ok(parsed) = serde_json::from_str(inner) {
            return Ok(parsed);
        }
        // F3: 失败围栏必须**整体跳过**。从 0 重扫会钻进它内部，把嵌套的示例/占位
        // 对象（如系统提示里的 {"atoms":[{"content":"fact text",…}]}）当成真载荷
        // 静默入库，而真正的第二个围栏永远到不了。
        offset = after_fence;
    }
    for _ in 0..MAX_JSON_CANDIDATE_SCANS {
        let Some((start, end)) = find_balanced_json_span(trimmed, offset) else {
            break;
        };
        let Some(span) = trimmed.get(start..=end) else {
            break;
        };
        if let Ok(parsed) = serde_json::from_str::<T>(span) {
            return Ok(parsed);
        }
        offset = next_json_scan_offset(span, start, end);
    }

    let json_str = extract_json_from_response(content);
    serde_json::from_str(&json_str).map_err(|e| format_json_parse_error(e, content))
}

fn format_json_parse_error(e: serde_json::Error, content: &str) -> anyhow::Error {
    // J28 + C2: redact credentials before truncating and logging raw LLM text.
    let redacted = crate::growth::security::redact_credentials(content);
    anyhow::anyhow!(
        "Failed to parse LLM JSON response: {} — raw: {}",
        e,
        crate::util::truncate_for_log(&redacted)
    )
}

/// B9: 重试日志与致命错误的脱敏口径（J28 + C2）——URI/credential redaction
/// delegated to `util::http_client`, then truncated (W21b).
fn sanitize_llm_error_for_log(err: &anyhow::Error, url: &str) -> String {
    crate::util::truncate_for_log(&sanitize_anyhow_error(err, url))
}

/// Emit the base-URL scheme warning for the LLM client at `LlmClient::from_config`
/// (`LlmClient::new` is the test constructor and deliberately stays quiet).
/// Classification itself is [`crate::util::url_scheme_issue`] (unit-tested there).
fn warn_on_base_url_scheme(source: &str, url: &str) {
    if let Some(problem) = crate::util::url_scheme_issue(url) {
        let safe_url = crate::util::sanitize_url_for_log(url);
        tracing::warn!("LLM base_url ({source} = {safe_url}): {problem}");
    }
}

/// W18: 按行取首个**闭合**围栏里形如 JSON 的内容。语言标签一律忽略
/// （json / JSON / Json 大小写同效），多围栏不再跨段截断；未闭合或
/// 内容非 JSON 的围栏跳过，继续找下一个。
fn extract_fenced_block(text: &str) -> Option<&str> {
    extract_fenced_block_with_end(text).map(|(inner, _)| inner)
}

/// 同 `extract_fenced_block`，另返回**闭合围栏之后**的字节偏移，供调用方跳过整个
/// 失败围栏继续扫描（F3：从 0 重扫会钻进失败围栏内部，把嵌套的示例/占位对象当成
/// 真载荷）。
fn extract_fenced_block_with_end(text: &str) -> Option<(&str, usize)> {
    let mut content_start: Option<usize> = None;
    let mut offset = 0usize;
    for line in text.split_inclusive('\n') {
        if line.trim().starts_with("```") {
            if let Some(start) = content_start {
                let inner = text.get(start..offset).map(str::trim).unwrap_or("");
                if inner.starts_with('{') || inner.starts_with('[') {
                    return Some((inner, offset + line.len()));
                }
                content_start = None;
            } else {
                content_start = Some(offset + line.len());
            }
        }
        offset += line.len();
    }
    None
}

const MAX_JSON_CANDIDATE_SCANS: usize = 64;

fn advance_json_string(b: u8, in_string: &mut bool, escaped: &mut bool) {
    if *escaped {
        *escaped = false;
    } else if b == b'\\' {
        *escaped = true;
    } else if b == b'"' {
        *in_string = false;
    }
}

fn find_balanced_json_span(text: &str, from: usize) -> Option<(usize, usize)> {
    let tail = text.get(from..)?;
    let rel_start = tail
        .as_bytes()
        .iter()
        .position(|&b| b == b'{' || b == b'[')?;
    let start = from + rel_start;
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (i, &b) in text.as_bytes().iter().enumerate().skip(start) {
        if in_string {
            advance_json_string(b, &mut in_string, &mut escaped);
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' | b'[' => depth += 1,
            b'}' | b']' => {
                depth -= 1;
                if depth == 0 {
                    return Some((start, i));
                }
            }
            _ => {}
        }
    }
    None
}

fn next_json_scan_offset(span: &str, start: usize, end: usize) -> usize {
    if serde_json::from_str::<serde_json::Value>(span).is_ok() {
        end + 1
    } else {
        start + 1
    }
}

/// W18 兜底：prose 包裹的裸 JSON——扫描最外层平衡 `{...}` / `[...]` 跨度，
/// 跳过非 JSON 的前缀方/花括号（如 `Note [see section A]: {...}`），优先返回首个有效 JSON。
fn extract_balanced_json(text: &str) -> Option<&str> {
    let mut offset = 0usize;
    let mut first_span: Option<&str> = None;
    for _ in 0..MAX_JSON_CANDIDATE_SCANS {
        let Some((start, end)) = find_balanced_json_span(text, offset) else {
            break;
        };
        let Some(candidate) = text.get(start..=end) else {
            break;
        };
        if first_span.is_none() {
            first_span = Some(candidate);
        }
        if serde_json::from_str::<serde_json::Value>(candidate).is_ok() {
            return Some(candidate);
        }
        offset = start + 1;
    }
    first_span
}

/// Extract JSON from LLM response (handles markdown code blocks)
fn extract_json_from_response(content: &str) -> String {
    let trimmed = content.trim();

    if let Some(inner) = extract_fenced_block(trimmed) {
        return inner.to_string();
    }

    if let Some(span) = extract_balanced_json(trimmed) {
        return span.to_string();
    }

    // Raw JSON (or unparseable text — serde reports the failure downstream)
    trimmed.to_string()
}

/// Shared offline-deterministic local OpenAI-compatible stub server for unit tests.
/// Returns `(base_url, request_counter)`.
#[cfg(test)]
pub(crate) fn spawn_test_llm_stub(
    responses: Vec<&'static str>,
) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
    use std::io::{Read, Write};
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    let calls = Arc::new(AtomicUsize::new(0));
    let calls_ref = calls.clone();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for (i, stream) in listener.incoming().enumerate() {
            let Some(&content) = responses.get(i) else {
                break;
            };
            let Ok(mut stream) = stream else { continue };
            calls_ref.fetch_add(1, Ordering::SeqCst);
            let mut buf = Vec::new();
            let mut byte = [0u8; 1];
            while !buf.ends_with(b"\r\n\r\n") {
                match stream.read(&mut byte) {
                    Ok(1) => buf.push(byte[0]),
                    _ => break,
                }
            }
            let len: usize = String::from_utf8_lossy(&buf)
                .to_ascii_lowercase()
                .lines()
                .find_map(|l| {
                    l.strip_prefix("content-length:")
                        .and_then(|v| v.trim().parse().ok())
                })
                .unwrap_or(0);
            let mut body = vec![0u8; len];
            if len > 0 {
                let _ = stream.read_exact(&mut body);
            }
            let json = serde_json::json!({
                "choices": [{ "message": { "role": "assistant", "content": content } }]
            })
            .to_string();
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                json.len(),
                json
            );
            let _ = stream.write_all(resp.as_bytes());
            let _ = stream.flush();
        }
    });
    (format!("http://{}/v1", addr), calls)
}

#[cfg(test)]
#[path = "llm_tests.rs"]
mod tests;

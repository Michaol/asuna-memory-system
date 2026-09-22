pub mod text;
pub mod time;

/// Truncate a possibly-large payload (e.g. a raw LLM response) for log and
/// error surfaces (J28: `chat_json` parse failures used to embed the whole
/// response — potentially the entire conversation — into every log line).
///
/// Character-based, never splits a UTF-8 multi-byte sequence.
pub fn truncate_for_log(s: &str) -> String {
    const MAX_CHARS: usize = 200;
    let total = s.chars().count();
    if total <= MAX_CHARS {
        return s.to_string();
    }
    let cut: String = s.chars().take(MAX_CHARS).collect();
    format!("{} (truncated, {} chars total)", cut, total)
}

/// Classify a configured API base URL's scheme (J28). Returns a human-readable
/// problem description when the URL is not https, `None` when it is.
///
/// Deliberately NOT a rejection: plain `http://` to a local dev endpoint
/// (Ollama, vLLM on localhost) is legitimate, and a non-http(s) scheme is
/// mostly a typo whose request will fail anyway — a startup warning is the
/// proportionate response for both. Callers pass non-empty URLs only.
pub fn url_scheme_issue(url: &str) -> Option<&'static str> {
    let lower = url.trim().to_ascii_lowercase();
    if lower.starts_with("https://") {
        None
    } else if lower.starts_with("http://") {
        Some("plain http: the API key and request payloads travel unencrypted; prefer https unless this is a trusted local endpoint")
    } else {
        Some("scheme is neither http nor https; requests will likely fail")
    }
}


/// Recover a poisoned mutex by logging and taking the inner guard.
///
/// Safe for SQLite connections (statement-atomic) and small plain values
/// (lazy-load flags). Shared by the HTTP gateway and the post-session pipeline.
pub fn recover_poison<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| {
        tracing::error!(
            "Mutex poisoned (a task panicked while holding it); recovering guard: {}",
            e
        );
        e.into_inner()
    })
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_truncate_for_log_under_limit_unchanged() {
        assert_eq!(truncate_for_log(""), "");
        assert_eq!(truncate_for_log("short"), "short");
        let exactly_200 = "中".repeat(200);
        assert_eq!(truncate_for_log(&exactly_200), exactly_200);
    }

    #[test]
    fn test_truncate_for_log_cuts_and_marks() {
        let long = "a".repeat(500);
        let out = truncate_for_log(&long);
        assert!(out.starts_with(&"a".repeat(200)));
        assert!(out.contains("(truncated, 500 chars total)"));
        // cut boundary is at 200 chars: the remainder of the input is gone
        assert!(!out.contains(&"a".repeat(201)));
    }

    #[test]
    fn test_truncate_for_log_multibyte_safe() {
        // 201 CJK chars would be byte-cut at an index inside a 3-byte char
        // by a s[..200] implementation; chars().take(200) must not panic.
        let long = "记忆".repeat(120); // 240 chars
        let out = truncate_for_log(&long);
        assert!(out.chars().count() < long.chars().count());
        assert!(out.contains("truncated"));
        assert!(out.starts_with("记忆"));
    }

    #[test]
    fn test_url_scheme_issue_matrix() {
        assert!(url_scheme_issue("https://api.openai.com/v1").is_none());
        assert!(url_scheme_issue("HTTPS://example.com").is_none());
        assert!(url_scheme_issue("  https://x  ").is_none());
        assert!(url_scheme_issue("http://localhost:11434/v1").is_some());
        assert!(url_scheme_issue("HTTP://127.0.0.1:9/v1").is_some());
        assert!(url_scheme_issue("ftp://files.example.com").is_some());
        assert!(url_scheme_issue("api.example.com/v1").is_some());
    }
}

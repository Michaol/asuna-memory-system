use crate::util::http_client::{
    check_http_status, is_retryable_error, is_retryable_status, is_retryable_ureq_error,
    retry_after_delay,
};
use serde::Deserialize;

use super::{
    extract_json_from_response, parse_json_response, sanitize_llm_error_for_log, LlmClient,
};

#[test]
fn test_extract_json_from_markdown_block() {
    let input = "```json\n{\"key\": \"value\"}\n```";
    assert_eq!(extract_json_from_response(input), "{\"key\": \"value\"}");
}

#[test]
fn test_extract_json_from_code_block() {
    let input = "```\n[{\"key\": \"value\"}]\n```";
    assert_eq!(extract_json_from_response(input), "[{\"key\": \"value\"}]");
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

/// W18: 多围栏取首个闭合围栏——旧 split_once + rfind 围栏符会跨段截断，
/// 把第一个围栏的开到最后一个围栏的闭全部当成 JSON。
#[test]
fn test_extract_json_first_closed_fence_not_cross_section() {
    let input = "前导说明\n```json\n{\"a\": 1}\n```\n后记\n```\n{\"b\": 2}\n```";
    assert_eq!(extract_json_from_response(input), "{\"a\": 1}");
}

/// W9 回归：首个围栏是"思考"载荷、真载荷在第二个围栏时，parse_json_response
/// 必须落回平衡扫描而不是整批失败。旧实现锁定首个 JSON 形状围栏即 return，
/// 不校验能否反序列化为 T → 整会话 L1 抽取失败。
#[test]
fn test_parse_json_response_falls_through_reasoning_fence() {
    #[derive(serde::Deserialize)]
    struct Payload {
        atoms: Vec<String>,
    }
    let raw = "```json\n{\"reasoning\": \"先想一想\"}\n```\n```json\n{\"atoms\": [\"a\"]}\n```";
    let parsed: Payload = parse_json_response(raw).expect("第二个围栏的真载荷必须被解析");
    assert_eq!(parsed.atoms, vec!["a".to_string()]);
}

/// W18: 语言标签大小写无关（大写 JSON 标签此前落通用分支）。
#[test]
fn test_extract_json_uppercase_json_tag() {
    let input = "```JSON\n{\"key\": \"value\"}\n```";
    assert_eq!(extract_json_from_response(input), "{\"key\": \"value\"}");
}

/// W18: prose 包裹裸 JSON → 最外层平衡跨度兜底；字符串内的括号与转义引号
/// 不得被当成结构括号。
#[test]
fn test_extract_json_prose_wrapped_balanced_fallback() {
    let input = "Sure! Here it is: {\"a\": [1, 2], \"b\": \"} not a close\"} hope it helps";
    let want = "{\"a\": [1, 2], \"b\": \"} not a close\"}";
    assert_eq!(extract_json_from_response(input), want);
    let escaped = "prefix {\"a\": \"he said \\\"hi\\\" }\"} suffix";
    let want_escaped = "{\"a\": \"he said \\\"hi\\\" }\"}";
    assert_eq!(extract_json_from_response(escaped), want_escaped);
    assert_eq!(
        extract_json_from_response("result [1, 2, 3] — done"),
        "[1, 2, 3]"
    );
}

/// W18: 未闭合围栏不得吞到文本末尾——回退平衡提取。
#[test]
fn test_extract_json_unclosed_fence_falls_back() {
    let input = "```json\n{\"a\": 1}\n";
    assert_eq!(extract_json_from_response(input), "{\"a\": 1}");
}

/// C5: 返回错误不得回显原始 endpoint URL 里的 userinfo（内网主机保留可诊断性）。
/// 127.0.0.1:9 discard 端口连接必拒——离线确定性，同仓库既有降级测试姿势。
#[test]
fn test_chat_error_sanitizes_endpoint_credentials() {
    let llm = LlmClient::new("http://user:secret@127.0.0.1:9/v1", "k", "m");
    let err = llm.chat("sys", "user").unwrap_err().to_string();
    assert!(err.contains("LLM API call failed"), "{err}");
    assert!(
        !err.contains("secret"),
        "endpoint userinfo must not leak: {err}"
    );
}

/// C2: ureq 会把大写主机名与默认 :80/:443 端口规范化，单独 replace(url) 会失配；
/// sanitize_llm_error 必须提取并脱敏 ureq 内部实际 URL，且 parse_json_response
/// 必须先 redact_credentials 再截断。
#[test]
fn test_c2_sanitize_llm_error_normalized_url_and_raw_response_redaction() {
    let llm = LlmClient::new("http://user:secret@LOCALHOST:80/v1", "k", "m");
    let raw_url = "http://user:secret@LOCALHOST:80/v1/chat/completions";
    let err: anyhow::Error = ureq::Error::BadUri(format!("{raw_url} is missing host")).into();
    let msg = sanitize_llm_error_for_log(&err, raw_url);
    assert!(
        !msg.contains("secret"),
        "sanitized msg leaked secret: {msg}"
    );
    let _ = llm;

    let poisoned_json = format!("not json {}sk-proj-{} end", "", "a".repeat(28));
    let parse_err = parse_json_response::<serde_json::Value>(&poisoned_json)
        .unwrap_err()
        .to_string();
    assert!(
        !parse_err.contains(&"a".repeat(28)),
        "parse_json_response leaked credential: {parse_err}"
    );
    assert!(parse_err.contains("[REDACTED]"), "{parse_err}");
}

/// B9: retry_after_delay 取 max(退避, Retry-After 头值) 并封顶（cfg(test)
/// CAP=50ms）；无头/非 Status 错误原样返回退避（镜像 embedder/api.rs M21 测试）。
#[test]
fn test_retry_after_delay_respects_header_with_cap() {
    let backoff = std::time::Duration::from_millis(10);
    let cap = std::time::Duration::from_millis(50);
    assert_eq!(retry_after_delay(None, backoff), backoff);

    // 429 带 Retry-After: 7（秒）→ 封顶 CAP（> backoff）
    let resp = ureq::http::Response::builder()
        .status(429)
        .header("Retry-After", "7")
        .body(ureq::Body::builder().data(""))
        .unwrap();
    let e: anyhow::Error = check_http_status(resp).unwrap_err().into();
    assert_eq!(retry_after_delay(Some(&e), backoff), cap);

    // Retry-After: 0 → max(backoff, 0) = backoff
    let resp0 = ureq::http::Response::builder()
        .status(429)
        .header("Retry-After", "0")
        .body(ureq::Body::builder().data(""))
        .unwrap();
    let e0: anyhow::Error = check_http_status(resp0).unwrap_err().into();
    assert_eq!(retry_after_delay(Some(&e0), backoff), backoff);

    // 无 Retry-After 头 → 原样退避
    let nohdr = ureq::http::Response::builder()
        .status(503)
        .body(ureq::Body::builder().data(""))
        .unwrap();
    let e_nohdr: anyhow::Error = check_http_status(nohdr).unwrap_err().into();
    assert_eq!(retry_after_delay(Some(&e_nohdr), backoff), backoff);
}

/// B9 端到端：首次 429（带 Retry-After: 1）→ 重试 → 第二次 200 成功。
/// cfg(test) CAP=50ms 封顶头值，测试快速且离线确定性。
#[test]
fn test_chat_retries_after_429_with_retry_after() {
    use std::io::{Read, Write};
    use std::sync::atomic::{AtomicUsize, Ordering};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let calls = std::sync::Arc::new(AtomicUsize::new(0));
    let calls_ref = calls.clone();
    std::thread::spawn(move || {
        for (i, stream) in listener.incoming().enumerate() {
            let Ok(mut stream) = stream else { continue };
            calls_ref.fetch_add(1, Ordering::SeqCst);
            // 读请求头 + body（同 spawn_test_llm_stub 姿势）。
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
            let resp = if i == 0 {
                "HTTP/1.1 429 Mock\r\nRetry-After: 1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()
            } else {
                let json = serde_json::json!({
                    "choices": [{ "message": { "role": "assistant", "content": "ok" } }]
                })
                .to_string();
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    json.len(),
                    json
                )
            };
            let _ = stream.write_all(resp.as_bytes());
            let _ = stream.flush();
            if i >= 1 {
                break;
            }
        }
    });
    let llm = LlmClient::new(&format!("http://{addr}/v1"), "k", "m");
    let out = llm.chat("sys", "user").expect("429 后重试应成功");
    assert_eq!(out, "ok");
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[test]
fn test_retryable_status_codes_narrowed_to_transient_set() {
    for code in [429u16, 500, 502, 503, 504] {
        assert!(is_retryable_ureq_error(&ureq::Error::StatusCode(code)));
        assert!(is_retryable_status(code));
    }
    assert!(!is_retryable_ureq_error(&ureq::Error::StatusCode(501)));
    assert!(!is_retryable_status(501));
}

#[test]
fn test_extract_and_parse_json_skips_prose_bracket_notes() {
    #[derive(Debug, Deserialize, PartialEq, Eq)]
    struct Kv {
        key: String,
    }

    let prose_note = "Note [see section A]: {\"key\": \"value\"}";
    assert_eq!(
        extract_json_from_response(prose_note),
        "{\"key\": \"value\"}"
    );
    assert_eq!(
        parse_json_response::<Kv>(prose_note).unwrap(),
        Kv {
            key: "value".to_string()
        }
    );

    let numbered_note = "Note [1]: {\"key\": \"value\"}";
    assert_eq!(
        parse_json_response::<Kv>(numbered_note).unwrap(),
        Kv {
            key: "value".to_string()
        }
    );
}

#[test]
fn test_llm_timeout_recv_body_is_retryable() {
    let err = ureq::Error::Timeout(ureq::Timeout::RecvBody);
    assert!(is_retryable_ureq_error(&err));
    let wrapped: anyhow::Error = err.into();
    assert!(is_retryable_error(&wrapped));
}

#[test]
fn test_llm_bad_uri_error_is_redacted() {
    let err: anyhow::Error =
        ureq::Error::BadUri("http://user:secret@localhost/v1 is missing host".into()).into();
    let msg = sanitize_llm_error_for_log(&err, "https://api.example/v1/chat/completions");
    assert!(!msg.contains("user:secret"), "leaked userinfo: {msg}");
    assert!(!msg.contains("secret"), "leaked secret: {msg}");

    let https_err: anyhow::Error =
        ureq::Error::RequireHttpsOnly("http://user:secret@localhost/v1".into()).into();
    let msg2 = sanitize_llm_error_for_log(&https_err, "https://api.example/v1/chat/completions");
    assert!(!msg2.contains("user:secret"), "leaked userinfo: {msg2}");
    assert!(!msg2.contains("secret"), "leaked secret: {msg2}");
}

/// W15: 入口 trim 尺标与 ApiEmbedder::new（W8）一致——尾随空白不得进入
/// URL/Authorization 头/model 字段。
#[test]
fn test_new_trims_url_key_model() {
    let llm = LlmClient::new("  http://proxy.example/v1/  ", "  sk-test  ", "  m  ");
    assert_eq!(llm.base_url, "http://proxy.example/v1/");
    assert_eq!(llm.api_key, "sk-test");
    assert_eq!(llm.model, "m");
}

/// W15: from_config 同样 trim；纯空白 base_url/api_key 视同未设置。
#[test]
fn test_from_config_trims_and_rejects_blank() {
    let cfg = crate::config::LlmConfig {
        base_url: "  http://proxy.example/v1  ".to_string(),
        api_key: "  sk-test  ".to_string(),
        model: "  m  ".to_string(),
    };
    let llm = LlmClient::from_config(&cfg).expect("trimmed config must build");
    assert_eq!(llm.base_url, "http://proxy.example/v1");
    assert_eq!(llm.api_key, "sk-test");
    assert_eq!(llm.model, "m");

    let blank = crate::config::LlmConfig {
        base_url: "   ".to_string(),
        api_key: "sk-test".to_string(),
        model: "m".to_string(),
    };
    assert!(LlmClient::from_config(&blank).is_none());
}

/// W15/M12a: base_url 尾随斜杠不得拼出 `//chat/completions`；base 上的
/// `?query` 必须保留在追加路径之后（共享 append_url_path）。
#[test]
fn test_chat_url_joins_slashes_and_query() {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let target = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let target_ref = target.clone();
    std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let mut buf = Vec::new();
            let mut byte = [0u8; 1];
            while !buf.ends_with(b"\r\n\r\n") {
                match stream.read(&mut byte) {
                    Ok(1) => buf.push(byte[0]),
                    _ => break,
                }
            }
            let head = String::from_utf8_lossy(&buf).to_string();
            *target_ref.lock().unwrap() = head.lines().next().unwrap_or("").to_string();
            let len: usize = head
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
                "choices": [{ "message": { "role": "assistant", "content": "ok" } }]
            })
            .to_string();
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                json.len(),
                json
            );
            let _ = stream.write_all(resp.as_bytes());
            let _ = stream.flush();
        }
    });
    let llm = LlmClient::new(&format!("http://{addr}/v1/?k=v"), "k", "m");
    assert_eq!(llm.chat("sys", "user").unwrap(), "ok");
    assert_eq!(
        *target.lock().unwrap(),
        "POST /v1/chat/completions?k=v HTTP/1.1"
    );
}

/// W15/M12a: 尾随斜杠 base 端到端仍可命中 stub（无 query 臂）。
#[test]
fn test_chat_trailing_slash_base_url() {
    let (base, _calls) = super::spawn_test_llm_stub(vec!["ok"]);
    let llm = LlmClient::new(&format!("{base}/"), "k", "m");
    assert_eq!(llm.chat("s", "u").unwrap(), "ok");
}

/// W15: JSON 扫描边界回归——字符串内的转义引号/花括号/方括号、结尾转义反斜杠、
/// 围栏内外的 trailing commentary 都不得破坏平衡跨度提取。
#[test]
fn test_parse_json_response_escaped_and_commentary_edges() {
    #[derive(Debug, Deserialize, PartialEq)]
    struct Kv {
        key: String,
    }

    let fenced = "```json\n{\"key\": \"say \\\"hi\\\" { } [ ]\"}\n```\ntrailing commentary";
    assert_eq!(
        parse_json_response::<Kv>(fenced).unwrap(),
        Kv {
            key: "say \"hi\" { } [ ]".to_string()
        }
    );

    let bare_commentary = "{\"key\": \"value\"}\nHope this helps!";
    assert_eq!(
        parse_json_response::<Kv>(bare_commentary).unwrap(),
        Kv {
            key: "value".to_string()
        }
    );

    let escaped_backslash = "prefix {\"key\": \"C:\\\\\"} suffix";
    assert_eq!(
        parse_json_response::<Kv>(escaped_backslash).unwrap(),
        Kv {
            key: "C:\\".to_string()
        }
    );

    let both_sides = "Sure! {\"key\": \"value\"} — done";
    assert_eq!(
        parse_json_response::<Kv>(both_sides).unwrap(),
        Kv {
            key: "value".to_string()
        }
    );
}

/// W21b: 200 + 不可解析 body → 致命错误必须脱敏（credential 不回显）且
/// 非瞬时错误不重试（只打一次请求）。
#[test]
fn test_chat_read_error_sanitized_fatal_no_retry() {
    use std::io::{Read, Write};
    use std::sync::atomic::{AtomicUsize, Ordering};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let calls = std::sync::Arc::new(AtomicUsize::new(0));
    let calls_ref = calls.clone();
    let poisoned = format!("not json sk-proj-{} {}", "a".repeat(28), "x".repeat(500));
    std::thread::spawn(move || {
        for stream in listener.incoming() {
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
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                poisoned.len(),
                poisoned
            );
            let _ = stream.write_all(resp.as_bytes());
            let _ = stream.flush();
        }
    });
    let llm = LlmClient::new(&format!("http://{addr}/v1"), "k", "m");
    let err = llm.chat("s", "u").unwrap_err().to_string();
    assert!(err.contains("LLM API response read failed"), "{err}");
    assert!(!err.contains(&"a".repeat(28)), "credential leaked: {err}");
    assert!(!err.contains(&"x".repeat(500)), "raw body leaked: {err}");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "permanent parse error must not retry"
    );
}

/// W21b: 超长错误文本经 sanitize_llm_error_for_log 必须先脱敏再截断。
#[test]
fn test_sanitize_llm_error_for_log_truncates_long_errors() {
    let url = "http://user:secret@proxy.example/v1/chat/completions";
    let long: anyhow::Error = anyhow::anyhow!("boom {url} {}", "x".repeat(400));
    let msg = sanitize_llm_error_for_log(&long, url);
    assert!(!msg.contains("user:secret"), "leaked userinfo: {msg}");
    assert!(msg.contains("truncated"), "not truncated: {msg}");
}

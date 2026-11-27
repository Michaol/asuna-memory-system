use super::{
    assemble_dashscope_embeddings, assemble_openai_embeddings, read_json_capped, ApiEmbedder,
    ApiFormat, DashScopeEmbedding, EmbedApiError, OpenAIData, OpenAIResponse,
    MAX_EMBED_RESPONSE_BYTES,
};
use crate::util::http_client::{
    check_http_status, is_loopback_url, is_retryable_error, retry_after_delay, HttpStatusError,
};

fn item(index: Option<usize>, embedding: Vec<f32>) -> OpenAIData {
    OpenAIData { index, embedding }
}

fn ds_item(text_index: usize, embedding: Vec<f32>) -> DashScopeEmbedding {
    DashScopeEmbedding {
        text_index,
        embedding,
    }
}

/// 组装 ureq::Error::StatusCode 测试错误
fn status_error(code: u16) -> ureq::Error {
    ureq::Error::StatusCode(code)
}

// --- OpenAI assembly ---

#[test]
fn test_assemble_indexed_ascending() {
    let data = vec![item(Some(0), vec![2.0, 0.0]), item(Some(1), vec![0.0, 4.0])];
    let out = assemble_openai_embeddings(data, 2, 2).unwrap();
    assert_eq!(out, vec![vec![1.0, 0.0], vec![0.0, 1.0]]);
}

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

#[test]
fn test_assemble_openai_rejects_non_finite() {
    let data = vec![item(Some(0), vec![f32::NAN, 1.0])];
    assert!(assemble_openai_embeddings(data, 1, 2).is_err());

    let data_inf = vec![item(Some(0), vec![f32::INFINITY, 0.0])];
    assert!(assemble_openai_embeddings(data_inf, 1, 2).is_err());
}

// --- DashScope assembly ---

#[test]
fn test_assemble_dashscope_ascending_and_out_of_order() {
    let asc = vec![ds_item(0, vec![3.0, 0.0]), ds_item(1, vec![0.0, 4.0])];
    let out = assemble_dashscope_embeddings(asc, 2, 2).unwrap();
    assert_eq!(out, vec![vec![1.0, 0.0], vec![0.0, 1.0]]);

    let ooo = vec![
        ds_item(2, vec![0.0, 0.0]),
        ds_item(0, vec![2.0, 0.0]),
        ds_item(1, vec![0.0, 5.0]),
    ];
    let out_ooo = assemble_dashscope_embeddings(ooo, 3, 2).unwrap();
    assert_eq!(
        out_ooo,
        vec![vec![1.0, 0.0], vec![0.0, 1.0], vec![0.0, 0.0]]
    );
}

#[test]
fn test_assemble_dashscope_rejects_duplicate_and_out_of_range() {
    let dup = vec![ds_item(0, vec![1.0, 0.0]), ds_item(0, vec![0.0, 1.0])];
    let err = assemble_dashscope_embeddings(dup, 2, 2).unwrap_err();
    assert!(err.to_string().contains("duplicate text_index"));

    let oor = vec![ds_item(3, vec![1.0, 0.0])];
    let err = assemble_dashscope_embeddings(oor, 1, 2).unwrap_err();
    assert!(err.to_string().contains("out of range"));
}

#[test]
fn test_assemble_dashscope_count_dim_and_nan_errors() {
    let mismatch = vec![ds_item(0, vec![1.0, 0.0])];
    assert!(assemble_dashscope_embeddings(mismatch, 2, 2)
        .unwrap_err()
        .to_string()
        .contains("returned 1 embeddings"));

    let bad_dim = vec![ds_item(0, vec![1.0, 0.0, 2.0])];
    assert!(assemble_dashscope_embeddings(bad_dim, 1, 2)
        .unwrap_err()
        .to_string()
        .contains("dimensions for text_index 0"));

    let nan_vec = vec![ds_item(0, vec![f32::NAN, 0.0])];
    assert!(assemble_dashscope_embeddings(nan_vec, 1, 2).is_err());
}

// --- Retry error classification ---

#[test]
fn test_anyhow_downcast_preserves_ureq_error() {
    let e: anyhow::Error = status_error(429).into();
    assert!(matches!(
        e.downcast_ref::<ureq::Error>(),
        Some(ureq::Error::StatusCode(429))
    ));
}

#[test]
fn test_non_retryable_invalid_url_transport_error() {
    let err = ureq::get("broken/url").call().unwrap_err();
    assert!(
        matches!(err, ureq::Error::BadUri(_) | ureq::Error::Http(_)),
        "got: {err:?}"
    );
    let e: anyhow::Error = err.into();
    assert!(
        !is_retryable_error(&e),
        "BadUri/Http 属永久性配置错误，不应重试"
    );

    let err2 = ureq::get("/relative/only").call().unwrap_err();
    assert!(matches!(err2, ureq::Error::BadUri(_)), "got: {err2:?}");
    let e2: anyhow::Error = err2.into();
    assert!(
        !is_retryable_error(&e2),
        "BadUri 属永久性配置错误，不应重试"
    );
}

#[test]
fn test_retryable_connection_failed_transport_error() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);

    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_connect(Some(std::time::Duration::from_millis(200)))
        .proxy(None)
        .build()
        .into();
    let err = agent
        .post(&format!("http://{addr}/embeddings"))
        .send_empty()
        .unwrap_err();
    assert!(
        matches!(
            err,
            ureq::Error::Io(_) | ureq::Error::ConnectionFailed | ureq::Error::Timeout(_)
        ),
        "got: {err:?}"
    );
    let e: anyhow::Error = err.into();
    assert!(
        is_retryable_error(&e),
        "ConnectionFailed 瞬时连接失败应重试: {e:?}"
    );
}

#[test]
fn test_is_loopback_url_matrix() {
    for url in [
        "http://127.0.0.1:11434/v1",
        "http://127.0.0.2:8080/v1",
        "http://localhost:8000/v1",
        "http://LOCALHOST/v1",
        "http://[::1]:8000/v1",
        "http://user:pass@127.0.0.1:9/v1",
    ] {
        assert!(is_loopback_url(url), "expected loopback: {url}");
    }
    for url in [
        "https://api.openai.com/v1",
        "http://localhost.evil.com/v1",
        "broken/url",
    ] {
        assert!(!is_loopback_url(url), "expected non-loopback: {url}");
    }
}

#[test]
fn test_retryable_status_codes() {
    for code in [429u16, 500, 502, 503, 504] {
        let e: anyhow::Error = status_error(code).into();
        assert!(is_retryable_error(&e), "{code} 应可重试");
        let he: anyhow::Error = HttpStatusError {
            status: code,
            retry_after_secs: None,
        }
        .into();
        assert!(is_retryable_error(&he), "{code} HttpStatusError 应可重试");
    }
}

#[test]
fn test_non_retryable_status_codes() {
    for code in [400u16, 401, 403, 404, 422] {
        let e: anyhow::Error = status_error(code).into();
        assert!(!is_retryable_error(&e), "{code} 属校验类错误，不应重试");
        let he: anyhow::Error = HttpStatusError {
            status: code,
            retry_after_secs: None,
        }
        .into();
        assert!(
            !is_retryable_error(&he),
            "{code} HttpStatusError 属校验类错误，不应重试"
        );
    }
}

#[test]
fn test_non_ureq_local_error_not_retryable() {
    let e: anyhow::Error = anyhow::anyhow!("维度校验失败（本地错误）");
    assert!(!is_retryable_error(&e), "非 ureq 本地错误不应重试");
}

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
        assert!(is_retryable_error(&e), "{kind:?} 应重试");
    }
    let e: anyhow::Error =
        anyhow::Error::new(std::io::Error::new(ErrorKind::InvalidData, "bad json"));
    assert!(
        !is_retryable_error(&e),
        "InvalidData（JSON 解析失败）不应重试"
    );
}

// --- Constructor normalization (W8) + canonical format label (M13) ---

/// W8: trailing whitespace used to land mid-URL after append_url_path,
/// making every request fail with a non-retryable InvalidUrl.
#[test]
fn test_new_trims_api_url_and_model() {
    let openai = ApiEmbedder::new(
        " https://proxy.example/v1/ ",
        "sk-test",
        " text-embedding-v4 ",
        1024,
        ApiFormat::OpenAI,
    )
    .unwrap();
    assert_eq!(openai.endpoint, "https://proxy.example/v1/embeddings");
    assert_eq!(openai.model, "text-embedding-v4");

    let dashscope = ApiEmbedder::new(
        "https://dashscope.aliyuncs.com/api/v1 ",
        "sk-test",
        "text-embedding-v4",
        1024,
        ApiFormat::DashScope,
    )
    .unwrap();
    assert_eq!(
        dashscope.endpoint,
        "https://dashscope.aliyuncs.com/api/v1/services/embeddings/text-embedding/text-embedding"
    );

    // whitespace-only values are "unset", not a usable endpoint/model
    let blank_url = ApiEmbedder::new("  ", "sk-test", "m", 1024, ApiFormat::OpenAI);
    assert!(blank_url.is_err());
    let blank_model = ApiEmbedder::new("https://x", "sk-test", " ", 1024, ApiFormat::OpenAI);
    assert!(blank_model.is_err());
}

/// endpoint 构造分支：DashScope base 已含 /text-embedding → 原样保留；带
/// ?query 的 base → 后缀插在 query 前（append_url_path Some 臂 / DashScope
/// query 臂）。既有 test_new_trims 只覆盖无 query 的 plain base。
#[test]
fn test_new_endpoint_query_and_full_path_branches() {
    // DashScope 完整服务路径原样保留（不再追加后缀）
    let full = "https://x/api/v1/services/embeddings/text-embedding/text-embedding";
    let ds_full = ApiEmbedder::new(full, "k", "m", 1024, ApiFormat::DashScope).unwrap();
    assert_eq!(ds_full.endpoint, full);
    // DashScope base 带 query → 后缀插在 ? 前
    let ds_q =
        ApiEmbedder::new("https://x/api/v1?k=v", "k", "m", 1024, ApiFormat::DashScope).unwrap();
    assert_eq!(
        ds_q.endpoint,
        "https://x/api/v1/services/embeddings/text-embedding/text-embedding?k=v"
    );
    // OpenAI base 带 query → append_url_path Some 臂
    let oa_q = ApiEmbedder::new("https://x/v1?k=v", "k", "m", 1024, ApiFormat::OpenAI).unwrap();
    assert_eq!(oa_q.endpoint, "https://x/v1/embeddings?k=v");
}

/// M13: the diagnostic label must be the canonical parsed format, so a
/// mixed-case or unknown config string cannot mislabel the startup log.
#[test]
fn test_api_format_as_str_round_trips_from_str() {
    assert_eq!(ApiFormat::OpenAI.as_str(), "openai");
    assert_eq!(ApiFormat::DashScope.as_str(), "dashscope");
    for raw in ["", "openai", "OpenAI", "dashscope", " DashScope ", "bogus"] {
        let parsed = ApiFormat::from_str(raw);
        assert_eq!(ApiFormat::from_str(parsed.as_str()), parsed, "raw={raw:?}");
    }
}

// --- Returned-error endpoint redaction (C3) ---

/// Offline stand-in for the embedding API: answers every request with
/// `status`, so the permanent-error return path is observable without a
/// network.
fn spawn_status_mock(status: u16) -> std::net::SocketAddr {
    use std::io::{BufRead, BufReader, Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut len = 0usize;
            let mut line = String::new();
            loop {
                line.clear();
                let n = reader.read_line(&mut line).unwrap_or(0);
                if n == 0 || line.trim_end().is_empty() {
                    break;
                }
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap_or(0);
                }
            }
            let mut body = vec![0u8; len];
            let _ = reader.read_exact(&mut body);
            let resp =
                format!("HTTP/1.1 {status} Mock\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            let _ = stream.write_all(resp.as_bytes());
            let _ = stream.flush();
        }
    });
    addr
}

/// C3 + W16: a permanent (non-retryable) status error must not surface the
/// raw endpoint — neither in `Display` nor in Debug/`chain()` — while HTTP
/// status stays downcastable via `EmbedApiError` for 401/403 backfill abort.
#[test]
fn test_status_error_hides_endpoint_and_keeps_ureq_cause() {
    let addr = spawn_status_mock(401);
    let url = format!("http://user:pass@{addr}/v1?api_key=secret");
    let emb = ApiEmbedder::new(&url, "sk-test", "m", 2, ApiFormat::OpenAI).unwrap();
    let err = emb.embed_batch(&["hi"], false).unwrap_err();
    let msg = err.to_string();
    let debug = format!("{err:?}");
    let alt = format!("{err:#}");
    for text in [&msg, &debug, &alt] {
        assert!(!text.contains(&emb.endpoint), "raw endpoint leaked: {text}");
        assert!(!text.contains("user:pass"), "userinfo leaked: {text}");
        assert!(
            !text.contains("api_key=secret"),
            "query token leaked: {text}"
        );
    }
    assert!(msg.contains("status code 401"), "diagnostic lost: {msg}");
    let permanent = err.chain().any(|c| {
        c.downcast_ref::<EmbedApiError>()
            .is_some_and(|e| matches!(e.status, Some(401 | 403)))
    });
    assert!(
        permanent,
        "W16: EmbedApiError status must stay downcastable"
    );
}

/// C3 + W16: the retries-exhausted return path redacts in both Display and
/// Debug/chain formats — raw Transport URLs must not remain in error chains.
#[test]
fn test_retries_exhausted_error_hides_endpoint() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let url = format!("http://token@{addr}/v1?api_key=secret");
    let emb = ApiEmbedder::new(&url, "sk-test", "m", 2, ApiFormat::OpenAI).unwrap();
    let err = emb.embed_batch(&["hi"], false).unwrap_err();
    for text in [err.to_string(), format!("{err:?}"), format!("{err:#}")] {
        assert!(!text.contains(&emb.endpoint), "raw endpoint leaked: {text}");
        assert!(!text.contains("token@"), "userinfo leaked: {text}");
        assert!(!text.contains("secret"), "query token leaked: {text}");
    }
}

/// M21: retry_after_delay 取 max(退避, Retry-After 头值) 并封顶（cfg(test)
/// CAP=50ms）；无头/非 Status 错误原样返回退避。
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
    let e_nohdr: anyhow::Error = status_error(503).into();
    assert_eq!(retry_after_delay(Some(&e_nohdr), backoff), backoff);
}

/// W8: api_key 尾随空白必须在入口 trim，否则 Authorization 头值含空白致每
/// 请求 BadHeader（config 路径已 trim，此处覆盖直接构造）。
#[test]
fn test_new_trims_api_key() {
    let emb = ApiEmbedder::new(
        "https://proxy.example/v1",
        "  sk-test  ",
        "m",
        1024,
        ApiFormat::OpenAI,
    )
    .unwrap();
    assert_eq!(emb.api_key, "sk-test");
}

/// 离线 mock：首次请求回 429（带 Retry-After），其后回 200 + 有效 OpenAI
/// 嵌入响应（axis0 单位向量）。验证 embed_batch 重试后成功。
fn spawn_retry_then_success_mock(dim: usize) -> std::net::SocketAddr {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::sync::atomic::{AtomicUsize, Ordering};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let calls = std::sync::Arc::new(AtomicUsize::new(0));
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut len = 0usize;
            let mut line = String::new();
            loop {
                line.clear();
                let n = reader.read_line(&mut line).unwrap_or(0);
                if n == 0 || line.trim_end().is_empty() {
                    break;
                }
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap_or(0);
                }
            }
            let mut body = vec![0u8; len];
            let _ = reader.read_exact(&mut body);
            let n = calls.fetch_add(1, Ordering::SeqCst);
            let resp = if n == 0 {
                "HTTP/1.1 429 Mock\r\nRetry-After: 1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    .to_string()
            } else {
                let mut v = vec![0.0f32; dim];
                v[0] = 1.0;
                let vec_json = serde_json::to_string(&v).unwrap();
                let payload = format!("{{\"data\":[{{\"embedding\":{vec_json},\"index\":0}}]}}");
                format!(
                    "HTTP/1.1 200 Mock\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    payload.len(),
                    payload
                )
            };
            let _ = stream.write_all(resp.as_bytes());
            let _ = stream.flush();
        }
    });
    addr
}

/// 测试缺口：embed_batch 首次 429（瞬时）→ 重试 → 第二次 200 成功。
/// 同时覆盖 M21 Retry-After 路径（cfg(test) 封顶 50ms，测试快速）。
#[test]
fn test_embed_batch_retries_then_succeeds() {
    let addr = spawn_retry_then_success_mock(2);
    let url = format!("http://{addr}/v1");
    let emb = ApiEmbedder::new(&url, "sk-test", "m", 2, ApiFormat::OpenAI).unwrap();
    let out = emb.embed_batch(&["hi"], false).expect("429 后重试应成功");
    assert_eq!(out.len(), 1);
    assert_eq!(out[0], vec![1.0, 0.0]);
}

/// 测试缺口：read_json_capped 拒绝超 32MiB 响应（防 OOM）。构造 32MiB+1
/// 字节流式 body，读取封顶触发 BodyExceedsLimit → bail。
#[test]
fn test_read_json_capped_rejects_oversized() {
    use std::io::Read;

    let oversize = (MAX_EMBED_RESPONSE_BYTES + 1) as u64;
    let body = ureq::Body::builder().reader(std::io::repeat(b'a').take(oversize));
    let resp = ureq::http::Response::builder()
        .status(200)
        .body(body)
        .unwrap();
    let err = read_json_capped::<OpenAIResponse>(resp).unwrap_err();
    assert!(err.to_string().contains("exceeded"), "got: {err}");
}

#[test]
fn test_timeout_recv_body_is_retryable() {
    let e: anyhow::Error = ureq::Error::Timeout(ureq::Timeout::RecvBody).into();
    assert!(is_retryable_error(&e), "Timeout::RecvBody 应被判定为可重试");
}

#[test]
fn test_bad_uri_error_is_redacted() {
    let emb = ApiEmbedder::new(
        "https://proxy.example/v1",
        "sk-test",
        "m",
        2,
        ApiFormat::OpenAI,
    )
    .unwrap();
    let e: anyhow::Error =
        ureq::Error::BadUri("http://user:secret@localhost/v1 is missing host".into()).into();
    let msg = emb.sanitize_error_message(&e);
    assert!(!msg.contains("user:secret"), "leaked userinfo: {msg}");
    assert!(!msg.contains("secret"), "leaked secret: {msg}");

    let e_https: anyhow::Error =
        ureq::Error::RequireHttpsOnly("http://user:secret@localhost/v1".into()).into();
    let msg2 = emb.sanitize_error_message(&e_https);
    assert!(!msg2.contains("user:secret"), "leaked userinfo: {msg2}");
    assert!(!msg2.contains("secret"), "leaked secret: {msg2}");
}

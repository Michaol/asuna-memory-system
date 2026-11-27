use super::{
    append_url_path, build_agent, check_http_status, extract_ureq_error_uri, is_loopback_url,
    is_retryable_error, is_retryable_io_error, is_retryable_status, is_retryable_ureq_error,
    is_socks_proxy_value, parse_retry_after_secs, post_json_bytes, read_bounded_body,
    retry_after_delay, sanitize_anyhow_error, sanitize_ureq_error, HttpStatusError, HttpTimeouts,
};
use std::io::{Error as IoError, ErrorKind, Read, Write};
use std::net::TcpListener;
use std::time::Duration;

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

/// W15: 首尾空白、FQDN 尾点（`localhost.`）、IPv4-mapped IPv6 都判环回；
/// `localhost.evil.com` / 非环回映射地址不得误判。
#[test]
fn test_is_loopback_url_w15_edges() {
    for url in [
        "  http://localhost:8000/v1  ",
        "http://localhost.:8000/v1",
        "http://LOCALHOST./v1",
        "http://[::ffff:127.0.0.1]:8000/v1",
        "http://[::ffff:7f00:1]/v1",
        "http://127.0.0.1.:8000/v1",
    ] {
        assert!(is_loopback_url(url), "expected loopback: {url}");
    }
    for url in [
        "http://localhost.evil.com/v1",
        "http://[::ffff:8.8.8.8]/v1",
        "http://notlocalhost/v1",
        "http://127.0.0.1.evil.com/v1",
    ] {
        assert!(!is_loopback_url(url), "expected non-loopback: {url}");
    }
}

/// W3: SOCKS scheme 检测大小写无关、容忍首尾空白；非 socks scheme 不命中。
#[test]
fn test_socks_proxy_value_detection() {
    for v in [
        "socks5://h:1080",
        "SOCKS4A://h",
        "  socks5h://user:pass@h  ",
        "Socks://h",
        "socks4://h",
    ] {
        assert!(is_socks_proxy_value(v), "expected SOCKS: {v}");
    }
    for v in ["http://h", "https://h", "", "   ", "xsocks5://h"] {
        assert!(!is_socks_proxy_value(v), "expected non-SOCKS: {v}");
    }
}

/// W3: build_agent 必须给 DNS 解析设上限（timeout_resolve），且常规/环回
/// target 都能干净构造（无 panic、无代理泄漏到环回）。
#[test]
fn test_build_agent_bounds_resolve_and_builds_cleanly() {
    let timeouts = HttpTimeouts {
        per_read: Duration::from_secs(5),
        per_write: Duration::from_secs(5),
        global: Duration::from_secs(10),
    };
    let agent = build_agent("https://api.example/v1", timeouts);
    let cfg = agent.config();
    assert!(
        cfg.timeouts().resolve.is_some(),
        "DNS resolve must be bounded"
    );
    assert_eq!(cfg.timeouts().resolve, cfg.timeouts().connect);
    assert_eq!(cfg.max_redirects(), 0);
    assert!(!cfg.http_status_as_error());

    let local = build_agent("http://127.0.0.1:11434/v1", timeouts);
    assert!(local.config().proxy().is_none());
}

/// W4: 永久性本地 IO 错误（InvalidData/InvalidInput/PermissionDenied/NotFound）
/// 经 ureq::Error::Io 包裹后同样不得重试；瞬时 kind 保持可重试。
#[test]
fn test_w4_permanent_io_errors_not_retryable() {
    for kind in [
        ErrorKind::InvalidData,
        ErrorKind::InvalidInput,
        ErrorKind::PermissionDenied,
        ErrorKind::NotFound,
    ] {
        let io = IoError::new(kind, "permanent local failure");
        assert!(!is_retryable_io_error(&io), "{kind:?}");
        assert!(
            !is_retryable_ureq_error(&ureq::Error::Io(io)),
            "{kind:?} wrapped in ureq::Error::Io must not retry"
        );
    }
    for kind in [
        ErrorKind::TimedOut,
        ErrorKind::ConnectionReset,
        ErrorKind::ConnectionAborted,
        ErrorKind::ConnectionRefused,
        ErrorKind::UnexpectedEof,
    ] {
        let io = IoError::new(kind, "transient");
        assert!(is_retryable_io_error(&io), "{kind:?}");
        assert!(is_retryable_ureq_error(&ureq::Error::Io(io)), "{kind:?}");
    }
}

/// W4: CONNECT 代理回 403/407 属永久拒绝，不重试；503/无响应保持瞬时；
/// StatusCode(403|407) 同样不重试。
#[test]
fn test_w4_proxy_403_407_not_retryable() {
    for reason in [
        "proxy server responded 403/Forbidden",
        "proxy server responded 407/Proxy Authentication Required",
    ] {
        assert!(
            !is_retryable_ureq_error(&ureq::Error::ConnectProxyFailed(reason.into())),
            "{reason}"
        );
    }
    for reason in [
        "proxy server responded 503/Service Unavailable",
        "proxy server did not respond",
    ] {
        assert!(
            is_retryable_ureq_error(&ureq::Error::ConnectProxyFailed(reason.into())),
            "{reason}"
        );
    }
    for code in [403u16, 407] {
        assert!(!is_retryable_ureq_error(&ureq::Error::StatusCode(code)));
    }
}

/// M12a: 共享 append_url_path——尾随斜杠折叠、?query 保持在追加路径之后。
#[test]
fn test_append_url_path_matrix() {
    assert_eq!(
        append_url_path("https://h/v1", "/embeddings"),
        "https://h/v1/embeddings"
    );
    assert_eq!(
        append_url_path("https://h/v1///", "/embeddings"),
        "https://h/v1/embeddings"
    );
    assert_eq!(
        append_url_path("https://h/v1?k=v", "/embeddings"),
        "https://h/v1/embeddings?k=v"
    );
    assert_eq!(
        append_url_path("https://h/v1/?k=v", "/chat/completions"),
        "https://h/v1/chat/completions?k=v"
    );
    assert_eq!(append_url_path("https://h", "/x"), "https://h/x");
}

/// 离线 mock：捕获请求头并按给定 status 应答（带 Retry-After: 3）。
fn spawn_header_capture_mock(
    status: u16,
) -> (
    std::net::SocketAddr,
    std::sync::Arc<std::sync::Mutex<Vec<String>>>,
) {
    use std::io::{BufRead, BufReader};
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let headers = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let h = headers.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut lines = Vec::new();
            let mut line = String::new();
            let mut len = 0usize;
            loop {
                line.clear();
                let n = reader.read_line(&mut line).unwrap_or(0);
                if n == 0 || line.trim_end().is_empty() {
                    break;
                }
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap_or(0);
                }
                lines.push(line.trim_end().to_string());
            }
            let mut body = vec![0u8; len];
            let _ = reader.read_exact(&mut body);
            h.lock().unwrap().extend(lines);
            let resp = format!(
                "HTTP/1.1 {status} Mock\r\nRetry-After: 3\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            let _ = stream.write_all(resp.as_bytes());
            let _ = stream.flush();
        }
    });
    (addr, headers)
}

/// M12a: post_json_bytes 补 Bearer/Content-Type 头、非 2xx 转 HttpStatusError
/// （保留 Retry-After）；空 api_key 不发 Authorization；2xx 原样放行。
#[test]
fn test_post_json_bytes_auth_headers_and_status_error() {
    let timeouts = HttpTimeouts {
        per_read: Duration::from_secs(5),
        per_write: Duration::from_secs(5),
        global: Duration::from_secs(10),
    };
    let (addr, headers) = spawn_header_capture_mock(429);
    let agent = build_agent(&format!("http://{addr}/v1"), timeouts);
    let err = post_json_bytes(
        &agent,
        &format!("http://{addr}/v1/embeddings"),
        "sk-test",
        "{}",
    )
    .unwrap_err();
    let he = err
        .downcast_ref::<HttpStatusError>()
        .expect("HttpStatusError");
    assert_eq!(he.status, 429);
    assert_eq!(he.retry_after_secs, Some(3));
    let hdrs = headers.lock().unwrap().join("\n").to_ascii_lowercase();
    assert!(hdrs.contains("authorization: bearer sk-test"), "{hdrs}");
    assert!(hdrs.contains("content-type: application/json"), "{hdrs}");

    let (addr2, headers2) = spawn_header_capture_mock(200);
    let agent2 = build_agent(&format!("http://{addr2}/v1"), timeouts);
    let resp =
        post_json_bytes(&agent2, &format!("http://{addr2}/v1/embeddings"), "", "{}").unwrap();
    assert_eq!(resp.status(), 200);
    let hdrs2 = headers2.lock().unwrap().join("\n").to_ascii_lowercase();
    assert!(!hdrs2.contains("authorization"), "{hdrs2}");
}

#[test]
fn test_check_http_status_and_parse_retry_after_secs() {
    let ok_resp = ureq::http::Response::builder()
        .status(200)
        .body(ureq::Body::builder().data("ok"))
        .unwrap();
    assert!(check_http_status(ok_resp).is_ok());

    let err_resp = ureq::http::Response::builder()
        .status(429)
        .header("Retry-After", " 12 ")
        .body(ureq::Body::builder().data(""))
        .unwrap();
    assert_eq!(parse_retry_after_secs(err_resp.headers()), Some(12));
    let he = check_http_status(err_resp).unwrap_err();
    assert_eq!(he.status, 429);
    assert_eq!(he.retry_after_secs, Some(12));
    assert!(he.to_string().contains("HTTP status code 429"));

    let bad_hdr_resp = ureq::http::Response::builder()
        .status(503)
        .header("Retry-After", "Wed, 21 Oct 2015 07:28:00 GMT")
        .body(ureq::Body::builder().data(""))
        .unwrap();
    let he_bad = check_http_status(bad_hdr_resp).unwrap_err();
    assert_eq!(he_bad.status, 503);
    assert_eq!(he_bad.retry_after_secs, None);
}

#[test]
fn test_retry_after_delay_clamping() {
    let backoff = Duration::from_millis(10);
    let cap = Duration::from_millis(50);
    assert_eq!(retry_after_delay(None, backoff), backoff);

    let resp = ureq::http::Response::builder()
        .status(429)
        .header("Retry-After", "7")
        .body(ureq::Body::builder().data(""))
        .unwrap();
    let e: anyhow::Error = check_http_status(resp).unwrap_err().into();
    assert_eq!(retry_after_delay(Some(&e), backoff), cap);

    let zero_resp = ureq::http::Response::builder()
        .status(429)
        .header("Retry-After", "0")
        .body(ureq::Body::builder().data(""))
        .unwrap();
    let e0: anyhow::Error = check_http_status(zero_resp).unwrap_err().into();
    assert_eq!(retry_after_delay(Some(&e0), backoff), backoff);

    let non_http: anyhow::Error = anyhow::anyhow!("local error");
    assert_eq!(retry_after_delay(Some(&non_http), backoff), backoff);
}

#[test]
fn test_retryability_classification_matrix() {
    for code in [429u16, 500, 502, 503, 504] {
        assert!(is_retryable_status(code));
        assert!(is_retryable_ureq_error(&ureq::Error::StatusCode(code)));
        let e: anyhow::Error = HttpStatusError {
            status: code,
            retry_after_secs: None,
        }
        .into();
        assert!(is_retryable_error(&e), "{code} should be retryable");
    }
    for code in [400u16, 401, 403, 404, 422, 501] {
        assert!(!is_retryable_status(code));
        assert!(!is_retryable_ureq_error(&ureq::Error::StatusCode(code)));
        let e: anyhow::Error = HttpStatusError {
            status: code,
            retry_after_secs: None,
        }
        .into();
        assert!(!is_retryable_error(&e), "{code} should not be retryable");
    }

    let recv_timeout: anyhow::Error = ureq::Error::Timeout(ureq::Timeout::RecvBody).into();
    assert!(is_retryable_error(&recv_timeout));

    let bad_uri: anyhow::Error = ureq::Error::BadUri("broken/url".into()).into();
    assert!(!is_retryable_error(&bad_uri));

    for kind in [
        ErrorKind::TimedOut,
        ErrorKind::ConnectionReset,
        ErrorKind::ConnectionAborted,
        ErrorKind::UnexpectedEof,
    ] {
        let e: anyhow::Error = IoError::new(kind, "transient").into();
        assert!(is_retryable_error(&e));
    }
    let bad_data: anyhow::Error = IoError::new(ErrorKind::InvalidData, "bad json").into();
    assert!(!is_retryable_error(&bad_data));
}

#[test]
fn test_read_bounded_body_within_and_over_limit() {
    let ok_resp = ureq::http::Response::builder()
        .status(200)
        .body(ureq::Body::builder().data(b"hello".as_slice()))
        .unwrap();
    let bytes = read_bounded_body(ok_resp, 16, "test API").unwrap();
    assert_eq!(bytes, b"hello");

    let over_resp = ureq::http::Response::builder()
        .status(200)
        .body(ureq::Body::builder().reader(std::io::repeat(b'x').take(17)))
        .unwrap();
    let err = read_bounded_body(over_resp, 16, "test API").unwrap_err();
    assert_eq!(err.to_string(), "test API response exceeded 16 bytes limit");
}

#[test]
fn test_error_sanitization_strips_uri_and_credentials() {
    let raw_url = "http://user:secret@LOCALHOST:80/v1?api_key=querysecret";
    let bad_uri = ureq::Error::BadUri(format!("{raw_url} is missing host"));
    assert_eq!(extract_ureq_error_uri(&bad_uri), Some(raw_url));
    let sanitized = sanitize_ureq_error(&bad_uri, raw_url);
    assert!(!sanitized.contains("user:secret"), "{sanitized}");
    assert!(!sanitized.contains("querysecret"), "{sanitized}");

    let https_err: anyhow::Error =
        ureq::Error::RequireHttpsOnly("http://user:secret@localhost/v1?api_key=querysecret".into())
            .into();
    let sanitized_https = sanitize_anyhow_error(&https_err, "https://proxy.example/v1");
    assert!(
        !sanitized_https.contains("user:secret"),
        "{sanitized_https}"
    );
    assert!(
        !sanitized_https.contains("querysecret"),
        "{sanitized_https}"
    );

    let any_err: anyhow::Error =
        anyhow::anyhow!("call to {raw_url} failed bearer sk-proj-{}", "a".repeat(28));
    let sanitized_any = sanitize_anyhow_error(&any_err, raw_url);
    assert!(!sanitized_any.contains("user:secret"), "{sanitized_any}");
    assert!(!sanitized_any.contains("querysecret"), "{sanitized_any}");
    assert!(!sanitized_any.contains(&"a".repeat(28)), "{sanitized_any}");
    assert!(sanitized_any.contains("[REDACTED]"), "{sanitized_any}");
}

#[test]
fn test_build_agent_preserves_http_error_status_on_loopback() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let mut buf = [0u8; 256];
            let _ = stream.read(&mut buf);
            let _ = stream.write_all(
                b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            );
            let _ = stream.flush();
        }
    });

    let url = format!("http://{addr}/health");
    let agent = build_agent(
        &url,
        HttpTimeouts {
            per_read: Duration::from_secs(5),
            per_write: Duration::from_secs(5),
            global: Duration::from_secs(10),
        },
    );
    let resp = agent.get(&url).call().unwrap();
    let he = check_http_status(resp).unwrap_err();
    assert_eq!(he.status, 401);
}

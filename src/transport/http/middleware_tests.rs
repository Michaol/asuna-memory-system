use super::build_cors_layer;
use crate::test_support::WarnLog;

fn http_agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .proxy(None)
        .build()
        .into()
}

fn cors_warns(origins: Vec<String>) -> Vec<String> {
    let mut cfg = crate::config::Config::default();
    cfg.gateway.cors_origins = origins;
    let log = WarnLog::default();
    crate::test_support::with_warn_default(log.clone(), || {
        let _ = build_cors_layer(&cfg.gateway);
    });
    // S1488/E0597：guard 命名为局部变量（声明在 log 之后，逆序落域先于 log 释放），
    // 尾表达式 guard.clone() 为 owned——直接返回 log.0.lock().unwrap().clone() 会让
    // MutexGuard 临时值活过 log 落域（E0597）。
    let guard = log.0.lock().unwrap();
    guard.clone()
}

/// F13: a non-empty cors_origins whose entries are all blank or all
/// invalid filters down to an empty allow-list — that must warn, while a
/// usable list must not.
#[test]
fn test_cors_empty_allow_list_warns() {
    let blank = cors_warns(vec!["   ".to_string(), "\t".to_string()]);
    assert!(
        blank.iter().any(|m| m.contains("blank or invalid")),
        "all-blank cors_origins must warn, got: {blank:?}"
    );

    let invalid = cors_warns(vec!["http://bad\norigin".to_string()]);
    assert!(
        invalid.iter().any(|m| m.contains("blank or invalid")),
        "all-invalid cors_origins must warn, got: {invalid:?}"
    );

    let valid = cors_warns(vec!["http://localhost:3000".to_string()]);
    assert!(
        !valid.iter().any(|m| m.contains("blank or invalid")),
        "a usable allow-list must not warn, got: {valid:?}"
    );
}

/// F13: fail-closed behavior is unchanged — with every entry blank, no
/// origin (even localhost) gets an Access-Control-Allow-Origin header.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_cors_all_blank_origins_stay_fail_closed() {
    use axum::{routing::get, Router};
    use tokio::net::TcpListener;

    async fn ping() -> &'static str {
        "pong"
    }
    let mut cfg = crate::config::Config::default();
    cfg.gateway.cors_origins = vec!["   ".to_string()];
    // Build under a scoped subscriber so the "blank or invalid" warn callsite is
    // first registered with a dispatcher present (Interest::sometimes). Without
    // one, tracing-core caches Interest::never for the callsite permanently
    // (set_global_default/with_default don't rebuild it), which would silently
    // suppress the warn asserted by test_cors_empty_allow_list_warns.
    let cors =
        crate::test_support::with_warn_default(crate::test_support::WarnLog::default(), || {
            build_cors_layer(&cfg.gateway)
        });
    let app = Router::new().route("/health", get(ping)).layer(cors);

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let agent: ureq::Agent = ureq::Agent::config_builder().proxy(None).build().into();
    for origin in ["http://localhost:3000", "http://evil.example"] {
        let resp = agent
            .get(&format!("http://{addr}/health"))
            .header("Origin", origin)
            .call()
            .unwrap();
        assert!(
            resp.headers().get("access-control-allow-origin").is_none(),
            "blank allow-list must reject origin {origin}"
        );
    }
}

/// C1: preflight must explicitly authorize the browser's auth/content headers.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_cors_preflight_explicit_auth_headers() {
    use axum::{routing::post, Router};
    use tokio::net::TcpListener;

    async fn ok() -> &'static str {
        "ok"
    }
    for origins in [
        vec![],
        vec!["https://app.example".to_string()],
        vec!["*".to_string()],
    ] {
        let mut cfg = crate::config::Config::default();
        cfg.gateway.cors_origins = origins.clone();
        let app = Router::new()
            .route("/capture", post(ok))
            .layer(build_cors_layer(&cfg.gateway));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let origin = if origins.is_empty() {
            "http://localhost:3000"
        } else {
            "https://app.example"
        };
        let resp = http_agent()
            .options(&format!("http://{addr}/capture"))
            .header("Origin", origin)
            .header("Access-Control-Request-Method", "POST")
            .header("Access-Control-Request-Headers", "authorization")
            .call()
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200);
        let allowed = resp
            .headers()
            .get("access-control-allow-headers")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        for expected in ["authorization", "content-type", "x-api-key"] {
            assert!(
                allowed
                    .split(',')
                    .any(|v| v.trim().eq_ignore_ascii_case(expected)),
                "missing {expected} in {allowed:?}"
            );
        }
        assert_ne!(allowed, "*");
    }
}

/// M2: existing mixed-case JSON errors pass through; rewritten errors cannot
/// retain a stale Transfer-Encoding alongside the new Content-Length.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_json_error_body_content_type_and_transfer_encoding() {
    use axum::{body::Body, response::Response, routing::get, Router};
    use tokio::net::TcpListener;

    let app = Router::new()
        .route(
            "/json",
            get(|| async {
                Response::builder()
                    .status(axum::http::StatusCode::UNPROCESSABLE_ENTITY)
                    .header("Content-Type", "Application/JSON; charset=utf-8")
                    .header("X-Contract", "preserved")
                    .body(Body::from(r#"{"error":"already-json"}"#))
                    .unwrap()
            }),
        )
        .route(
            "/rewrite",
            get(|| async {
                Response::builder()
                    .status(axum::http::StatusCode::BAD_REQUEST)
                    .header("Content-Type", "text/plain")
                    .header("Transfer-Encoding", "chunked")
                    .body(Body::from("plain failure"))
                    .unwrap()
            }),
        )
        .layer(axum::middleware::from_fn(super::json_error_body));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let agent = http_agent();

    let mut json = agent.get(&format!("http://{addr}/json")).call().unwrap();
    assert_eq!(json.status().as_u16(), 422);
    assert_eq!(
        json.headers()
            .get("x-contract")
            .and_then(|v| v.to_str().ok()),
        Some("preserved")
    );
    assert_eq!(
        json.body_mut().read_to_string().unwrap(),
        r#"{"error":"already-json"}"#
    );

    let mut rewritten = agent.get(&format!("http://{addr}/rewrite")).call().unwrap();
    assert_eq!(rewritten.status().as_u16(), 400);
    assert!(rewritten.headers().get("transfer-encoding").is_none());
    let body = rewritten.body_mut().read_to_string().unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&body).unwrap()["error"],
        "plain failure"
    );
    assert_eq!(
        rewritten
            .headers()
            .get("content-length")
            .and_then(|v| v.to_str().ok()),
        Some(body.len().to_string().as_str())
    );
}

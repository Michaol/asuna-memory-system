use super::error::{ApiError, ErrorResponse};
use crate::transport::state::AppState;
use axum::{
    body::Body,
    extract::State,
    http::{header, HeaderMap, HeaderName, Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Json, Response},
};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tower_http::cors::{AllowOrigin, Any, CorsLayer};

/// Map a handler panic into the gateway's `{"error": ...}` JSON contract.
pub(super) fn panic_to_response(_payload: Box<dyn std::any::Any + Send>) -> Response {
    // The panic payload can embed user content or secrets; the response body
    // is already generic, so keep the log generic too (CatchPanicLayer's
    // custom hook receives only the payload, not the panic location).
    tracing::error!("Gateway handler panicked (payload redacted)");
    ApiError::internal().into_response()
}

/// W1.2: tower-http's TimeoutLayer answers 408 with an empty body and
/// RequestBodyLimitLayer answers 413 with text/plain — both bypass the
/// gateway's `{"error": ...}` JSON contract. Rewrite by status: no handler
/// produces 408/413 itself, so every such response comes from those layers
/// (or axum's DefaultBodyLimit rejection, also non-JSON). Must be layered
/// outside Timeout/RequestBodyLimit to see their responses.
pub(super) async fn json_error_body(request: Request<Body>, next: Next) -> Response {
    // W4.3: remember the path before `next.run` consumes the request.
    let is_capture = request.uri().path() == "/capture";
    let response = next.run(request).await;
    match response.status() {
        StatusCode::REQUEST_TIMEOUT => {
            if is_capture {
                // W4.3: spawn_blocking is not cancellable — the persist task
                // detached by this timeout may still complete, so an Append
                // retry without `batch_id` can duplicate turns. Clients should
                // send an optional `batch_id` for idempotent retries or query before retrying.
                tracing::warn!(
                    "/capture timed out; the detached blocking write may still complete — pass `batch_id` for idempotent retries or query before retrying"
                );
            }
            (
                StatusCode::REQUEST_TIMEOUT,
                Json(ErrorResponse {
                    error: "request timeout".to_string(),
                }),
            )
                .into_response()
        }
        StatusCode::PAYLOAD_TOO_LARGE => (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(ErrorResponse {
                error: "payload too large".to_string(),
            }),
        )
            .into_response(),
        // W21: 其余错误响应若不是 JSON（axum 提取器拒绝的 400、未知路由 404、
        // 方法不匹配 405、415 等，body 为 text/plain 或空），统一改写进 JSON 错误
        // 契约——否则按 JSON 解析每个错误体的客户端会在这些可达路径上崩掉。
        status if status.is_client_error() || status.is_server_error() => {
            let is_json = super::require_json(response.headers()).is_ok();
            if is_json {
                return response;
            }
            let (mut parts, body) = response.into_parts();
            let bytes = axum::body::to_bytes(body, 64 * 1024)
                .await
                .unwrap_or_default();
            let text = String::from_utf8_lossy(&bytes).trim().to_string();
            let error = if text.is_empty() {
                status.canonical_reason().unwrap_or("error").to_string()
            } else {
                text
            };
            // M2: 必须保留原响应头——重建响应会丢掉 405 的 Allow（RFC 9110 §15.5.6
            // 要求 MUST 生成）等头。只替换 content-type / content-length。
            let payload = serde_json::to_vec(&ErrorResponse { error })
                .unwrap_or_else(|_| b"{\"error\":\"internal server error\"}".to_vec());
            parts.headers.insert(
                axum::http::header::CONTENT_TYPE,
                axum::http::HeaderValue::from_static("application/json"),
            );
            parts.headers.insert(
                axum::http::header::CONTENT_LENGTH,
                axum::http::HeaderValue::from(payload.len()),
            );
            parts.headers.remove(axum::http::header::TRANSFER_ENCODING);
            Response::from_parts(parts, Body::from(payload))
        }
        _ => response,
    }
}

fn is_valid_optional_port(suffix: &str) -> bool {
    suffix.is_empty()
        || suffix
            .strip_prefix(':')
            .is_some_and(|p| !p.is_empty() && p.parse::<u16>().is_ok())
}

/// Returns true if an `Origin` header value points strictly at localhost/loopback.
pub(super) fn is_localhost_origin(origin: &str) -> bool {
    let Some((scheme, rest)) = origin.split_once("://") else {
        return false;
    };
    if !(scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https")) {
        return false;
    }
    if rest.is_empty()
        || rest.contains(['/', '?', '#', '@'])
        || rest.chars().any(char::is_whitespace)
    {
        return false;
    }
    if let Some(stripped) = rest.strip_prefix('[') {
        let Some((host, after)) = stripped.split_once(']') else {
            return false;
        };
        // Bracketed hosts are IPv6 literals; is_loopback accepts every
        // spelling of ::1 (e.g. 0:0:0:0:0:0:0:1), not just the compressed form.
        return host
            .parse::<std::net::Ipv6Addr>()
            .is_ok_and(|ip| ip.is_loopback())
            && is_valid_optional_port(after);
    }
    let (host, port_ok) = match rest.split_once(':') {
        Some((h, p)) => (h, !p.is_empty() && p.parse::<u16>().is_ok()),
        None => (rest, true),
    };
    if !port_ok {
        return false;
    }
    // Parse instead of literal compare: the whole 127.0.0.0/8 range is loopback.
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

fn allowed_cors_headers() -> [HeaderName; 3] {
    [
        header::AUTHORIZATION,
        header::CONTENT_TYPE,
        HeaderName::from_static("x-api-key"),
    ]
}

fn build_localhost_cors_layer(auth_enabled: bool) -> CorsLayer {
    if auth_enabled {
        // Auth on + wildcard CORS means a key leaked into any browser page
        // could be used by any site — default to localhost like the
        // no-auth path instead of Any.
        tracing::warn!(
            "Gateway CORS restricted to localhost origins (auth enabled). \
             Set gateway.cors_origins to allow specific web origins."
        );
    } else {
        tracing::warn!(
            "Gateway running without auth; CORS restricted to localhost origins. \
             Set AMS_GATEWAY_API_KEY (which also enables authentication), or \
             gateway.auth_enabled=true with gateway.api_key in config.json; \
             set gateway.cors_origins to allow specific web origins."
        );
    }
    CorsLayer::new()
        .allow_origin(AllowOrigin::predicate(|origin, _parts| {
            origin.to_str().map(is_localhost_origin).unwrap_or(false)
        }))
        .allow_methods(Any)
        .allow_headers(allowed_cors_headers())
}

fn parse_cors_origins(cors_origins: &[String]) -> Vec<axum::http::HeaderValue> {
    // M4: trim before parsing — a padded origin is a legal HeaderValue but can
    // never match the browser's Origin header (silent fail-closed rejection).
    cors_origins
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        // M11: scheme/host 大小写无关，浏览器 Origin 恒小写、tower-http 逐字节比较
        // ——混合大小写配置项此前是合法 HeaderValue（不告警）却永不匹配，静默
        // fail-closed 拒绝。Origin 不含 path，整体小写是安全的。
        .map(|s| s.to_ascii_lowercase())
        .filter_map(|s| match s.parse() {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::warn!("Ignoring invalid gateway.cors_origins entry {:?}: {}", s, e);
                None
            }
        })
        .collect()
}

pub(super) fn build_cors_layer(gw: &crate::config::GatewayConfig) -> CorsLayer {
    if gw.cors_origins.is_empty() {
        return build_localhost_cors_layer(gw.auth_enabled);
    }
    if gw.cors_origins.iter().any(|o| o.trim() == "*") {
        tracing::warn!("Gateway CORS configured with wildcard '*'; allowing any origin.");
        return CorsLayer::new()
            .allow_origin(Any)
            .allow_methods(Any)
            .allow_headers(allowed_cors_headers());
    }
    let origins = parse_cors_origins(&gw.cors_origins);
    // F13: an all-blank/all-invalid list silently became `allow_origin([])`
    // (reject every origin). Keep that fail-closed behavior, but diagnose it.
    if origins.is_empty() {
        tracing::warn!(
            "Gateway CORS: all gateway.cors_origins entries are blank or invalid; \
             no origins will be allowed (fail-closed). Fix gateway.cors_origins."
        );
    }
    CorsLayer::new()
        .allow_origin(origins)
        .allow_methods(Any)
        .allow_headers(allowed_cors_headers())
}

/// Authentication middleware that validates API key via SHA-256 constant-time digest compare.
pub(super) async fn auth_middleware(
    State(state): State<AppState>,
    headers: HeaderMap,
    request: Request<Body>,
    next: Next,
) -> Response {
    if request.uri().path() == "/health" {
        return next.run(request).await;
    }

    let bearer_token = headers
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(|v| v.trim().split_once(' '))
        .and_then(|(scheme, tok)| {
            let t = tok.trim();
            if scheme.eq_ignore_ascii_case("bearer") && !t.is_empty() {
                Some(t)
            } else {
                None
            }
        });
    let api_key = bearer_token.or_else(|| {
        headers
            .get("X-API-Key")
            .and_then(|h| h.to_str().ok())
            .map(str::trim)
            .filter(|s| !s.is_empty())
    });

    match api_key {
        Some(key) => {
            let provided_digest = Sha256::digest(key.as_bytes());
            if bool::from(provided_digest.as_slice().ct_eq(&state.api_key_digest)) {
                return next.run(request).await;
            }
            tracing::warn!("Invalid API key provided");
            (
                StatusCode::UNAUTHORIZED,
                [(header::WWW_AUTHENTICATE, "Bearer")],
                Json(ErrorResponse {
                    error: "Invalid API key".to_string(),
                }),
            )
                .into_response()
        }
        None => {
            tracing::warn!("No API key provided");
            (
                StatusCode::UNAUTHORIZED,
                [(header::WWW_AUTHENTICATE, "Bearer")],
                Json(ErrorResponse {
                    error: "API key required".to_string(),
                }),
            )
                .into_response()
        }
    }
}

#[cfg(test)]
#[path = "middleware_tests.rs"]
mod tests;

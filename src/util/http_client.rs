//! Shared outbound HTTP client utilities built on `ureq 3`.
//!
//! Unifies agent construction (timeouts, no-redirect, loopback proxy bypass),
//! status validation, `Retry-After` backoff calculation, bounded body reads,
//! retry error classification, and credential-safe URL/error sanitization for
//! `embedder::api` and `memory::llm`.

use std::net::IpAddr;
use std::time::Duration;

/// Per-request timeout policy passed to [`build_agent`].
#[derive(Debug, Clone, Copy)]
pub struct HttpTimeouts {
    pub per_read: Duration,
    pub per_write: Duration,
    pub global: Duration,
}

/// Typed HTTP error produced by [`check_http_status`] when a response returns a
/// non-2xx status code with `.http_status_as_error(false)`.
#[derive(Debug)]
pub struct HttpStatusError {
    pub status: u16,
    pub retry_after_secs: Option<u64>,
}

impl std::fmt::Display for HttpStatusError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HTTP status code {}", self.status)
    }
}

impl std::error::Error for HttpStatusError {}

/// Returns `true` when `url` targets a loopback host (`localhost`, `127.0.0.0/8`, `::1`).
/// Used to bypass ambient `HTTP_PROXY`/`HTTPS_PROXY` env vars for local endpoints
/// (Ollama, vLLM, local test stubs) even when `NO_PROXY` is unset.
pub fn is_loopback_url(url: &str) -> bool {
    let Ok(uri) = url.trim().parse::<ureq::http::Uri>() else {
        return false;
    };
    let Some(host) = uri.host() else {
        return false;
    };
    let bare = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    // W15: a trailing FQDN dot ("localhost.") names the same host.
    let bare = bare.strip_suffix('.').unwrap_or(bare);
    if bare.eq_ignore_ascii_case("localhost") {
        return true;
    }
    match bare.parse::<IpAddr>() {
        Ok(IpAddr::V4(v4)) => v4.is_loopback(),
        // W15: IPv4-mapped IPv6 (`::ffff:127.0.0.1`) is loopback iff the mapped
        // IPv4 is; `Ipv6Addr::is_loopback` alone only recognizes `::1`.
        Ok(IpAddr::V6(v6)) => {
            v6.is_loopback() || v6.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback())
        }
        Err(_) => false,
    }
}

/// W3: `ureq` is built without the `socks-proxy` feature, so a SOCKS proxy
/// picked up from the environment is silently ignored (requests connect
/// directly). Detect `socks*` schemes case-insensitively, tolerating
/// surrounding whitespace.
fn is_socks_proxy_value(value: &str) -> bool {
    value.trim().to_ascii_lowercase().starts_with("socks")
}

// Windows env lookup is case-insensitive, so the lowercase aliases would only
// duplicate the uppercase warnings there; on Unix they are distinct variables.
#[cfg(windows)]
const PROXY_ENV_NAMES: &[&str] = &["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY"];
#[cfg(not(windows))]
const PROXY_ENV_NAMES: &[&str] = &[
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "http_proxy",
    "https_proxy",
    "all_proxy",
];

fn warn_on_socks_proxy_env() {
    for name in PROXY_ENV_NAMES {
        if let Ok(value) = std::env::var(name) {
            if is_socks_proxy_value(&value) {
                tracing::warn!(
                    "{name} configures a SOCKS proxy ({}), but this build of ureq has no \
                     `socks-proxy` feature: the proxy is ignored and requests connect directly",
                    crate::util::sanitize_url_for_log(&value)
                );
            }
        }
    }
}

/// Construct a hardened `ureq::Agent` with explicit timeouts, zero redirects,
/// status codes preserved on the response, and proxy disabled for loopback targets.
pub fn build_agent(target_url: &str, timeouts: HttpTimeouts) -> ureq::Agent {
    #[cfg(test)]
    const CONNECT_TIMEOUT: Duration = Duration::from_millis(250);
    #[cfg(not(test))]
    const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

    warn_on_socks_proxy_env();
    let mut builder = ureq::Agent::config_builder()
        .timeout_connect(Some(CONNECT_TIMEOUT))
        // W3: bound local DNS resolution too — without this a hung resolver
        // stalls the request past every other timeout.
        .timeout_resolve(Some(CONNECT_TIMEOUT))
        .timeout_per_read(Some(timeouts.per_read))
        .timeout_per_write(Some(timeouts.per_write))
        .timeout_global(Some(timeouts.global))
        .max_redirects(0)
        .http_status_as_error(false);
    if is_loopback_url(target_url) {
        builder = builder.proxy(None);
    }
    builder.build().into()
}

/// Extract integer delta-seconds from a `Retry-After` response header.
pub fn parse_retry_after_secs(headers: &ureq::http::HeaderMap) -> Option<u64> {
    headers
        .get(ureq::http::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
}

/// Convert non-2xx HTTP responses into [`HttpStatusError`] while capturing `Retry-After`.
pub fn check_http_status(
    response: ureq::http::Response<ureq::Body>,
) -> Result<ureq::http::Response<ureq::Body>, HttpStatusError> {
    if response.status().is_success() {
        Ok(response)
    } else {
        let status = response.status().as_u16();
        let retry_after_secs = parse_retry_after_secs(response.headers());
        Err(HttpStatusError {
            status,
            retry_after_secs,
        })
    }
}

/// Canonical retryable HTTP status codes (`429`, `500`, `502`, `503`, `504`).
pub fn is_retryable_status(code: u16) -> bool {
    matches!(code, 429 | 500 | 502 | 503 | 504)
}

/// Extract the status code from ureq's `ConnectProxyFailed` reason string
/// (`"proxy server responded 403/Forbidden"`); `None` for non-status failures
/// such as `"proxy server did not respond"`.
fn connect_proxy_failed_status(reason: &str) -> Option<u16> {
    reason
        .strip_prefix("proxy server responded ")?
        .split('/')
        .next()?
        .parse()
        .ok()
}

/// Classify transient `ureq::Error` variants eligible for retry.
pub fn is_retryable_ureq_error(e: &ureq::Error) -> bool {
    match e {
        ureq::Error::Timeout(_) | ureq::Error::HostNotFound | ureq::Error::ConnectionFailed => true,
        // W4: permanent local IO errors (InvalidData/InvalidInput/…) must not
        // be retried — delegate to the io-kind classifier instead of blanket true.
        ureq::Error::Io(io) => is_retryable_io_error(io),
        // W4: a proxy that answered 403/407 rejects the tunnel permanently;
        // other CONNECT failures (no response, 5xx) stay transient.
        ureq::Error::ConnectProxyFailed(reason) => {
            !matches!(connect_proxy_failed_status(reason), Some(403 | 407))
        }
        ureq::Error::StatusCode(code) => is_retryable_status(*code),
        _ => false,
    }
}

/// Classify transient `std::io::Error` kinds eligible for retry during body reads
/// and connection setup. Permanent local kinds (`InvalidData`, `InvalidInput`,
/// `PermissionDenied`, `NotFound`, …) are never retried (W4). `ConnectionRefused`
/// mirrors `ureq::Error::ConnectionFailed`: the peer service may come up later.
pub fn is_retryable_io_error(io: &std::io::Error) -> bool {
    matches!(
        io.kind(),
        std::io::ErrorKind::TimedOut
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::ConnectionRefused
            | std::io::ErrorKind::UnexpectedEof
    )
}

/// Unified retryability classifier across [`HttpStatusError`], [`ureq::Error`],
/// and [`std::io::Error`].
pub fn is_retryable_error(e: &anyhow::Error) -> bool {
    if let Some(he) = e.downcast_ref::<HttpStatusError>() {
        return is_retryable_status(he.status);
    }
    if let Some(ue) = e.downcast_ref::<ureq::Error>() {
        return is_retryable_ureq_error(ue);
    }
    if let Some(io) = e.downcast_ref::<std::io::Error>() {
        return is_retryable_io_error(io);
    }
    false
}

/// Respect `Retry-After` (delta-seconds only), clamped between `backoff` and `CAP`.
pub fn retry_after_delay(last_error: Option<&anyhow::Error>, backoff: Duration) -> Duration {
    #[cfg(test)]
    const CAP: Duration = Duration::from_millis(50);
    #[cfg(not(test))]
    const CAP: Duration = Duration::from_secs(30);

    if let Some(he) = last_error.and_then(|err| err.downcast_ref::<HttpStatusError>()) {
        if let Some(secs) = he.retry_after_secs {
            let header_delay = Duration::from_secs(secs).min(CAP);
            return backoff.max(header_delay).min(CAP);
        }
    }
    backoff
}

/// Read an HTTP response body up to `max_bytes`, preserving underlying `ureq::Error`
/// on stream timeout/interruption so [`is_retryable_error`] can classify it.
pub fn read_bounded_body(
    mut response: ureq::http::Response<ureq::Body>,
    max_bytes: usize,
    label: &str,
) -> anyhow::Result<Vec<u8>> {
    match response
        .body_mut()
        .with_config()
        .limit(max_bytes as u64)
        .read_to_vec()
    {
        Ok(buf) => Ok(buf),
        Err(ureq::Error::BodyExceedsLimit(_)) => {
            anyhow::bail!("{label} response exceeded {max_bytes} bytes limit");
        }
        Err(e) => Err(e.into()),
    }
}

/// M12a: append `suffix` (a leading-`/` path) to `base`, collapsing trailing
/// slashes and keeping any `?query` after the appended path. Shared by
/// `embedder::api` and `memory::llm` endpoint construction.
pub fn append_url_path(base: &str, suffix: &str) -> String {
    match base.split_once('?') {
        Some((base, query)) => format!("{}{}?{}", base.trim_end_matches('/'), suffix, query),
        None => format!("{}{}", base.trim_end_matches('/'), suffix),
    }
}

/// M12a: POST a compact JSON body with optional Bearer auth, then validate the
/// status. Non-2xx becomes [`HttpStatusError`] (downcastable for retry/backfill
/// classification); transport errors stay `ureq::Error` in the anyhow chain.
pub fn post_json_bytes(
    agent: &ureq::Agent,
    url: &str,
    api_key: &str,
    json_body: &str,
) -> anyhow::Result<ureq::http::Response<ureq::Body>> {
    let mut req = agent.post(url).header("Content-Type", "application/json");
    if !api_key.is_empty() {
        req = req.header("Authorization", &format!("Bearer {api_key}"));
    }
    Ok(check_http_status(req.send(json_body)?)?)
}

/// Extract embedded URI substrings formatted into `ureq::Error` messages.
pub fn extract_ureq_error_uri(ue: &ureq::Error) -> Option<&str> {
    match ue {
        ureq::Error::BadUri(s) => s.split_whitespace().next(),
        ureq::Error::RequireHttpsOnly(uri) => Some(uri.as_str()),
        _ => None,
    }
}

/// Strip raw, URI-normalized, and `ureq`-printed endpoint URLs plus credential
/// tokens from an error message string.
pub fn sanitize_error_text(raw_msg: &str, url: &str, printed_url: Option<&str>) -> String {
    let normalized_url = url.parse::<ureq::http::Uri>().ok().map(|u| u.to_string());
    let mut msg = raw_msg.to_string();
    for candidate in [Some(url), normalized_url.as_deref(), printed_url]
        .into_iter()
        .flatten()
    {
        if !candidate.is_empty() {
            msg = msg.replace(candidate, &crate::util::sanitize_url_for_log(candidate));
        }
    }
    crate::growth::security::redact_credentials(&msg)
}

/// Sanitize a `ureq::Error` against `url` and embedded credentials.
pub fn sanitize_ureq_error(e: &ureq::Error, url: &str) -> String {
    sanitize_error_text(&e.to_string(), url, extract_ureq_error_uri(e))
}

/// Sanitize an `anyhow::Error` against `url` (including any wrapped `ureq::Error` URI).
pub fn sanitize_anyhow_error(err: &anyhow::Error, url: &str) -> String {
    let printed_url = err
        .downcast_ref::<ureq::Error>()
        .and_then(extract_ureq_error_uri);
    sanitize_error_text(&err.to_string(), url, printed_url)
}

#[cfg(test)]
#[path = "http_client_tests.rs"]
mod tests;

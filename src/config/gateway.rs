use serde::{Deserialize, Serialize};

/// Gateway configuration for HTTP API server
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct GatewayConfig {
    /// Enable API key authentication
    pub auth_enabled: bool,
    /// API key for authentication (reads from AMS_GATEWAY_API_KEY)
    pub api_key: String,
    /// Allowed CORS origins. When empty and auth is disabled, restricted to localhost origins.
    pub cors_origins: Vec<String>,
    /// Interface the gateway binds ("" resolves to AMS_GATEWAY_BIND_HOST, then loopback).
    #[serde(default)]
    pub bind_host: String,
}

impl std::fmt::Debug for GatewayConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GatewayConfig")
            .field("auth_enabled", &self.auth_enabled)
            .field(
                "api_key",
                &if self.api_key.is_empty() {
                    ""
                } else {
                    "[REDACTED]"
                },
            )
            .field("cors_origins", &self.cors_origins)
            .field("bind_host", &self.bind_host)
            .finish()
    }
}

fn loopback_bind_host() -> String {
    "127.0.0.1".to_string()
}

fn parse_auth_enabled_env() -> Option<bool> {
    let raw = std::env::var("AMS_GATEWAY_AUTH_ENABLED").ok()?;
    match raw.trim().to_ascii_lowercase().as_str() {
        "true" | "1" => Some(true),
        "false" | "0" => Some(false),
        "" => None,
        other => {
            tracing::warn!(
                "AMS_GATEWAY_AUTH_ENABLED='{other}' is not one of true/false/1/0; ignored"
            );
            None
        }
    }
}

fn resolve_bind_host_env(bind_host: &mut String) {
    // v4-M: 与 api_key/base_url 同尺标——config 值与采纳的 env 值都 trim
    // （下游 validate_gateway_bind/gateway_bind_addr 本就 trim，统一口径）。
    *bind_host = bind_host.trim().to_string();
    if bind_host.is_empty() {
        *bind_host = std::env::var("AMS_GATEWAY_BIND_HOST")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(loopback_bind_host);
    }
}

impl GatewayConfig {
    /// Fill empty fields from environment variables.
    ///
    /// A non-empty `AMS_GATEWAY_API_KEY` implies `auth_enabled = true` unless
    /// `AMS_GATEWAY_AUTH_ENABLED` explicitly opts out with `false`/`0`.
    pub fn resolve_env(&mut self) {
        self.api_key = self.api_key.trim().to_string();
        let env_key = std::env::var("AMS_GATEWAY_API_KEY")
            .ok()
            .map(|k| k.trim().to_string())
            .filter(|k| !k.is_empty());
        if self.api_key.is_empty() {
            if let Some(ref k) = env_key {
                self.api_key = k.clone();
            }
        }
        let explicit = parse_auth_enabled_env();
        if let Some(on) = explicit {
            self.auth_enabled = on;
        }
        let key_from_env = env_key.is_some();
        if key_from_env && !self.auth_enabled {
            if explicit == Some(false) {
                tracing::info!(
                    "AMS_GATEWAY_API_KEY is set but gateway auth stays off: \
                     AMS_GATEWAY_AUTH_ENABLED=false disables it explicitly."
                );
            } else {
                tracing::warn!(
                    "AMS_GATEWAY_API_KEY is set but gateway auth is off; enabling authentication. \
                     Set AMS_GATEWAY_AUTH_ENABLED=false to explicitly keep auth disabled."
                );
                self.auth_enabled = true;
            }
        }
        resolve_bind_host_env(&mut self.bind_host);
    }
}

/// Startup check for the gateway bind address: refuses non-loopback binding
/// unless authentication is enabled with a non-empty key.
pub fn validate_gateway_bind(
    bind_host: &str,
    auth_enabled: bool,
    api_key: &str,
) -> anyhow::Result<()> {
    let host = bind_host.trim();
    if host.is_empty() {
        anyhow::bail!("gateway bind_host is empty");
    }
    if host.contains(char::is_whitespace) {
        anyhow::bail!(
            "gateway bind_host '{bind_host}' contains whitespace; expected a bare host or \
             address (no port)"
        );
    }
    if host.starts_with('[') != host.ends_with(']') {
        anyhow::bail!(
            "invalid bind_host '{bind_host}': IPv6 literals must be fully bracketed \
             ('[' requires a matching ']')"
        );
    }
    let bare = host.strip_prefix('[').unwrap_or(host);
    let bare = bare.strip_suffix(']').unwrap_or(bare);
    // W7: brackets are IPv6-literal syntax only — "[localhost]" / "[127.0.0.1]"
    // are not addresses and must not answer the loopback question.
    if host.starts_with('[') && bare.parse::<std::net::Ipv6Addr>().is_err() {
        anyhow::bail!(
            "invalid bind_host '{bind_host}': bracketed hosts must contain a bare IPv6 \
             literal (e.g. '[::1]')"
        );
    }
    let loopback = match bare.parse::<std::net::IpAddr>() {
        Ok(ip) => ip.is_loopback(),
        Err(_) => {
            if bare.eq_ignore_ascii_case("localhost") {
                true
            } else if host.contains(':') {
                anyhow::bail!(
                    "gateway bind_host '{bind_host}' is not a valid address (the port comes \
                     from the CLI/env and must not be part of bind_host)"
                )
            } else {
                false
            }
        }
    };
    if !loopback && !(auth_enabled && !api_key.trim().is_empty()) {
        anyhow::bail!(
            "gateway bind_host '{bind_host}' is not a loopback address; non-loopback binding \
             requires authentication (set AMS_GATEWAY_API_KEY to enable auth, or \
             gateway.auth_enabled=true with a non-empty gateway.api_key)"
        );
    }
    Ok(())
}

/// Render the configured host as a `host:port` socket address string, bracketing
/// bare IPv6 literals.
pub fn gateway_bind_addr(bind_host: &str, port: u16) -> String {
    let host = bind_host.trim();
    let bracketed = host.starts_with('[') && host.ends_with(']');
    if host.contains(':') && !bracketed {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

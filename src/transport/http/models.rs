use super::error::ApiError;
use crate::fact::search::SearchMode;
use crate::transport::state::AppState;
use axum::{
    body::{Body, Bytes},
    extract::FromRequest,
    http::Request,
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use std::str::FromStr;

/// JSON request body extractor that rejects a wrong media type before buffering.
#[derive(Debug)]
pub(super) struct JsonBytes(pub(super) Bytes);

impl FromRequest<AppState> for JsonBytes {
    type Rejection = Response;

    async fn from_request(req: Request<Body>, state: &AppState) -> Result<Self, Self::Rejection> {
        super::require_json(req.headers()).map_err(IntoResponse::into_response)?;
        let path = req.uri().path();
        if matches!(path, "/graph/assert" | "/graph/neighbors") && !state.config.graph.enabled {
            return Err(ApiError::forbidden("graph disabled in config").into_response());
        }
        if path == "/session/end" && state.shutdown.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(ApiError::service_unavailable(
                "gateway is shutting down; session end is unavailable",
            )
            .into_response());
        }
        Bytes::from_request(req, state)
            .await
            .map(Self)
            .map_err(IntoResponse::into_response)
    }
}

#[derive(Deserialize)]
pub(super) struct CaptureRequest {
    pub(super) session_id: String,
    pub(super) turns: Vec<serde_json::Value>,
    /// A1/W4.3: 可选幂等键——同 (session_id, batch_id) 的超时重试不重复追加
    /// turn（向后兼容：缺省 None 时行为与旧版一致）。
    #[serde(default)]
    pub(super) batch_id: Option<String>,
}

#[derive(Serialize)]
pub(super) struct CaptureResponse {
    pub(super) status: String,
    pub(super) turns_saved: usize,
}

#[derive(Deserialize)]
pub(super) struct RecallRequest {
    pub(super) query: String,
    pub(super) top_k: Option<usize>,
    pub(super) max_tokens: Option<usize>,
    pub(super) after: Option<String>,
    pub(super) before: Option<String>,
    pub(super) last_days: Option<i64>,
}

#[derive(Serialize, Debug)]
pub(super) struct RecallResponse {
    pub(super) memories: Vec<serde_json::Value>,
    pub(super) context: String,
    pub(super) truncated: bool,
}

#[derive(Deserialize)]
pub(super) struct SearchRequest {
    #[serde(default)]
    pub(super) query: String,
    pub(super) mode: Option<String>,
    pub(super) top_k: Option<usize>,
    pub(super) role: Option<String>,
    pub(super) after: Option<String>,
    pub(super) before: Option<String>,
    pub(super) last_days: Option<i64>,
    pub(super) entity: Option<String>,
    pub(super) max_hops: Option<u32>,
    pub(super) relation_filter: Option<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
pub(super) enum ConfidenceInput {
    Number(f64),
    String(String),
}

#[derive(Deserialize)]
pub(super) struct GraphAssertRequest {
    pub(super) subject: String,
    pub(super) predicate: String,
    pub(super) object: String,
    pub(super) confidence: Option<ConfidenceInput>,
}

pub(super) fn parse_confidence(input: Option<&ConfidenceInput>) -> Result<f64, ApiError> {
    let parsed = match input {
        None => return Ok(0.5),
        Some(ConfidenceInput::Number(c)) => *c,
        Some(ConfidenceInput::String(s)) => s
            .parse::<f64>()
            .map_err(|_| ApiError::bad_request(format!("invalid confidence value: {}", s)))?,
    };
    if !parsed.is_finite() || !(0.0..=1.0).contains(&parsed) {
        return Err(ApiError::bad_request(format!(
            "confidence must be between 0.0 and 1.0, got {}",
            parsed
        )));
    }
    Ok(parsed)
}

#[derive(Serialize)]
pub(super) struct HealthResponse {
    pub(super) status: String,
    pub(super) version: String,
}

#[derive(Serialize)]
pub(super) struct StatsResponse {
    pub(super) sessions: i64,
    pub(super) turns: i64,
    pub(super) vectors: i64,
    pub(super) entities: i64,
    pub(super) relations: i64,
}

#[derive(Deserialize)]
pub(super) struct OffloadRequest {
    pub(super) task_id: String,
    pub(super) content: String,
}

#[derive(Serialize, Debug)]
pub(super) struct OffloadResponse {
    pub(super) node_id: String,
    pub(super) bytes_stored: usize,
}

#[derive(Serialize)]
pub(super) struct RecallNodeResponse {
    pub(super) node_id: String,
    pub(super) content: String,
}

/// Parse the optional time window via `util::time::resolve_window`.
pub(super) fn parse_time_window(
    after: Option<&str>,
    before: Option<&str>,
    last_days: Option<i64>,
) -> Result<(Option<i64>, Option<i64>), ApiError> {
    crate::util::time::resolve_window(after, before, last_days)
        .map_err(|e| ApiError::bad_request(e.to_string()))
}

pub(super) fn parse_search_mode(mode: Option<&str>) -> Result<SearchMode, ApiError> {
    match mode {
        None => Ok(SearchMode::Hybrid),
        // SearchMode::from_str already names the allowed modes — surface its
        // message instead of a hand-written approximation that can drift.
        Some(s) => SearchMode::from_str(s).map_err(ApiError::bad_request),
    }
}

/// Shared top_k default/cap for `/recall` and `/search`.
pub(super) const DEFAULT_TOP_K: usize = 10;
pub(super) const MAX_TOP_K: usize = 50;
/// Query length cap shared by `/recall` and `/search` (chars, not bytes).
pub(super) const MAX_QUERY_CHARS: usize = 10_000;
/// Entity name cap shared by `/search` multi-hop and `/graph/neighbors`.
pub(super) const MAX_ENTITY_CHARS: usize = 1000;

pub(super) fn session_id_error(session_id: &str) -> Option<&'static str> {
    crate::util::validate_session_id(session_id).err()
}

/// Return true after inspecting at most `max + 1` characters.
pub(super) fn exceeds_char_limit(s: &str, max: usize) -> bool {
    s.chars().nth(max).is_some()
}

/// Shared query length guard for `/recall` and `/search` (identical 400 text).
pub(super) fn validate_query_length(query: &str) -> Result<(), ApiError> {
    if exceeds_char_limit(query, MAX_QUERY_CHARS) {
        return Err(ApiError::bad_request(format!(
            "Query too long (max {} characters)",
            MAX_QUERY_CHARS
        )));
    }
    Ok(())
}

/// Shared entity length guard for `/search` multi-hop and `/graph/neighbors`.
pub(super) fn validate_entity_length(entity: &str) -> Result<(), ApiError> {
    if exceeds_char_limit(entity, MAX_ENTITY_CHARS) {
        return Err(ApiError::bad_request(format!(
            "Entity name too long (max {} characters)",
            MAX_ENTITY_CHARS
        )));
    }
    Ok(())
}

/// Bound graph relation filters without scanning an arbitrarily long string.
pub(super) fn validate_relation_length(field: &str, relation: &str) -> Result<(), ApiError> {
    if exceeds_char_limit(relation, MAX_ENTITY_CHARS) {
        return Err(ApiError::bad_request(format!(
            "{} too long (max {} characters)",
            field, MAX_ENTITY_CHARS
        )));
    }
    Ok(())
}

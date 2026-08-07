//! HTTP REST gateway for AMS
//!
//! Provides REST API endpoints for Hermes and other HTTP clients.
//!
//! ## Endpoints
//!
//! | Endpoint | Method | Description |
//! |----------|--------|-------------|
//! | `/health` | GET | Health check |
//! | `/stats` | GET | Database statistics |
//! | `/capture` | POST | Save conversation turns |
//! | `/recall` | POST | Progressive disclosure retrieval (L3→L2→L1→L0) |
//! | `/recall/:node_id` | GET | Recall offloaded text |
//! | `/search` | POST | Text or multi-hop graph search |
//! | `/persona` | GET | Read user persona |
//! | `/offload` | POST | Store long text to refs/ directory |
//! | `/graph/assert` | POST | Write entity-relation triples |
//! | `/graph/neighbors` | POST | Query N-hop neighbors |
//! | `/session/end` | POST | Record session end timestamp |

use crate::transport::state::AppState;
use axum::{
    body::Body,
    extract::{Path, State},
    http::{HeaderMap, Request, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Json, Response},
    routing::{get, post},
    Router,
};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use std::sync::MutexGuard;
use subtle::ConstantTimeEq;
use tokio::net::TcpListener;
use tower_http::cors::{AllowOrigin, Any, CorsLayer};
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::trace::TraceLayer;

/// Helper to acquire the database lock with a consistent error response
fn acquire_db(
    state: &AppState,
) -> Result<MutexGuard<'_, crate::index::db::Db>, (StatusCode, Json<ErrorResponse>)> {
    state.db.lock().map_err(|_e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: "Failed to acquire database lock".to_string(),
            }),
        )
    })
}

/// Returns true if an `Origin` header value points at localhost (any port).
///
/// Used as the default CORS policy when the gateway runs without auth and no
/// explicit `cors_origins` are configured — local web UIs work, but a public
/// site the user visits cannot cross-origin read the private memory store.
fn is_localhost_origin(origin: &str) -> bool {
    let rest = match origin.split_once("://") {
        Some((scheme, r)) if scheme == "http" || scheme == "https" => r,
        _ => return false,
    };
    // Host is everything before an optional ":port" (handle bracketed IPv6 `[::1]`).
    let host = if let Some(stripped) = rest.strip_prefix('[') {
        match stripped.split_once(']') {
            Some((h, _)) => h,
            None => return false,
        }
    } else {
        rest.split(':').next().unwrap_or("")
    };
    matches!(host, "localhost" | "127.0.0.1" | "::1")
}

/// Start the HTTP gateway server
pub async fn run_gateway(
    config: crate::config::Config,
    db: crate::index::db::Db,
    embedder: Option<crate::embedder::LazyEmbedder>,
    llm: Option<crate::memory::llm::LlmClient>,
    port: u16,
) -> anyhow::Result<()> {
    // Backfill vec_bounded_memory if atoms lack vector embeddings
    if let Some(ref emb) = embedder {
        if let Err(e) = db.maybe_backfill_bounded_memory_vec(emb) {
            tracing::warn!("vec_bounded_memory backfill skipped: {}", e);
        }
    }

    let state = AppState::new(config, db, embedder, llm);

    // CORS configuration
    let cors = if state.config.gateway.cors_origins.is_empty() {
        if state.config.gateway.auth_enabled {
            // Auth is required, so any origin is acceptable (caller must present a key).
            tracing::warn!("Gateway CORS allows any origin (auth enabled). Set gateway.cors_origins to narrow it.");
            CorsLayer::new()
                .allow_origin(Any)
                .allow_methods(Any)
                .allow_headers(Any)
        } else {
            // Auth OFF + no explicit origins: do NOT open to all origins, or any
            // website the user visits could cross-origin fetch private memory from
            // 127.0.0.1 and read it. Restrict to localhost origins (any port).
            tracing::warn!(
                "Gateway running without auth; CORS restricted to localhost origins. \
                 Set AMS_GATEWAY_API_KEY for auth, or gateway.cors_origins to allow specific web origins."
            );
            CorsLayer::new()
                .allow_origin(AllowOrigin::predicate(|origin, _parts| {
                    origin.to_str().map(is_localhost_origin).unwrap_or(false)
                }))
                .allow_methods(Any)
                .allow_headers(Any)
        }
    } else {
        // Parse allowed origins from config
        let origins: Vec<axum::http::HeaderValue> = state
            .config
            .gateway
            .cors_origins
            .iter()
            .filter_map(|s| s.parse().ok())
            .collect();

        CorsLayer::new()
            .allow_origin(origins)
            .allow_methods(Any)
            .allow_headers(Any)
    };

    // Build router with optional authentication
    let mut app = Router::new()
        .route("/health", get(health))
        .route("/stats", get(stats))
        .route("/capture", post(capture))
        .route("/recall", post(recall))
        .route("/recall/:node_id", get(recall_by_node))
        .route("/search", post(search))
        .route("/persona", get(persona))
        .route("/offload", post(offload))
        .route("/graph/assert", post(graph_assert))
        .route("/graph/neighbors", post(graph_neighbors))
        .route("/session/end", post(session_end));

    // Add authentication middleware if enabled
    if state.config.gateway.auth_enabled {
        if state.config.gateway.api_key.is_empty() {
            anyhow::bail!("Gateway auth is enabled but no API key is configured. Set AMS_GATEWAY_API_KEY environment variable.");
        }
        tracing::info!("Gateway authentication enabled");
        app = app.layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ));
    }

    let app = app
        .layer(TraceLayer::new_for_http())
        .layer(RequestBodyLimitLayer::new(10 * 1024 * 1024)) // 10MB request size limit
        .layer(cors)
        .with_state(state);

    let addr = format!("127.0.0.1:{}", port);
    let listener = TcpListener::bind(&addr).await?;
    let actual_addr = listener.local_addr()?;

    tracing::info!("AMS Gateway listening on http://{}", actual_addr);
    println!("AMS Gateway listening on http://{}", actual_addr);

    axum::serve(listener, app).await?;
    Ok(())
}

/// Authentication middleware that validates API key
async fn auth_middleware(
    State(state): State<AppState>,
    headers: HeaderMap,
    request: Request<Body>,
    next: Next,
) -> Response {
    // Skip auth for health check endpoint
    if request.uri().path() == "/health" {
        return next.run(request).await;
    }

    // Extract API key from Authorization header or X-API-Key header
    let api_key = headers
        .get("Authorization")
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .or_else(|| {
            headers
                .get("X-API-Key")
                .and_then(|h| h.to_str().ok())
        });

    match api_key {
        Some(key)
            if key.len() == state.config.gateway.api_key.len()
                && key
                    .as_bytes()
                    .ct_eq(state.config.gateway.api_key.as_bytes())
                    .into() =>
        {
            // Valid API key (constant-time comparison to prevent timing attacks)
            next.run(request).await
        }
        Some(_) => {
            // Invalid API key
            tracing::warn!("Invalid API key provided");
            (
                StatusCode::UNAUTHORIZED,
                Json(ErrorResponse {
                    error: "Invalid API key".to_string(),
                }),
            )
                .into_response()
        }
        None => {
            // No API key provided
            tracing::warn!("No API key provided");
            (
                StatusCode::UNAUTHORIZED,
                Json(ErrorResponse {
                    error: "API key required".to_string(),
                }),
            )
                .into_response()
        }
    }
}

// ============ Request/Response types ============

#[derive(Deserialize)]
struct CaptureRequest {
    session_id: String,
    turns: Vec<serde_json::Value>,
}

#[derive(Serialize)]
struct CaptureResponse {
    status: String,
    turns_saved: usize,
}

#[derive(Deserialize)]
struct RecallRequest {
    query: String,
    top_k: Option<usize>,
    // v2.6: per-request token budget override (defaults to config recall.token_budget)
    max_tokens: Option<usize>,
    // v2.6: time-range filters for L1 (created_at) and L0 (timestamp_ms),
    // semantics identical to /search (v2.5.1). L3 persona / L2 scenarios are
    // evergreen layers and stay unfiltered by design.
    after: Option<String>,
    before: Option<String>,
    last_days: Option<i64>,
}

#[derive(Serialize, Debug)]
struct RecallResponse {
    memories: Vec<serde_json::Value>,
    context: String,
    /// v2.6: true when the token budget dropped at least one memory
    truncated: bool,
}

#[derive(Deserialize)]
struct SearchRequest {
    query: String,
    mode: Option<String>,
    top_k: Option<usize>,
    // Turn-search filters (parity with the MCP search_sessions tool)
    role: Option<String>,
    after: Option<String>,
    before: Option<String>,
    last_days: Option<i64>,
    // P8: Multi-hop query parameters
    entity: Option<String>,
    max_hops: Option<u32>,
    relation_filter: Option<String>,
}

#[derive(Deserialize)]
struct GraphAssertRequest {
    subject: String,
    predicate: String,
    object: String,
    confidence: Option<String>,
}

#[derive(Deserialize)]
struct GraphNeighborsRequest {
    entity: String,
    hops: Option<usize>,
    direction: Option<String>,
    // P8: Add relation_kind filtering
    relation_kind: Option<String>,
}

#[derive(Serialize)]
struct HealthResponse {
    status: String,
    version: String,
}

#[derive(Serialize)]
struct StatsResponse {
    sessions: i64,
    turns: i64,
    vectors: i64,
    entities: i64,
    relations: i64,
}

#[derive(Serialize, Debug)]
struct ErrorResponse {
    error: String,
}

// v2.6: type aliases to keep extracted helpers' signatures readable
// (clippy::type_complexity on nested tuple/generic returns).
type HttpError = (StatusCode, Json<ErrorResponse>);
type TurnRecord = (i64, String, Option<Vec<f32>>);

// ============ Short-term memory types ============

#[derive(Deserialize)]
struct OffloadRequest {
    task_id: String,
    content: String,
}

#[derive(Serialize)]
struct OffloadResponse {
    node_id: String,
    bytes_stored: usize,
}

#[derive(Serialize)]
struct RecallNodeResponse {
    node_id: String,
    content: String,
}

// ============ Handlers ============

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
    })
}

async fn stats(
    State(state): State<AppState>,
) -> Result<Json<StatsResponse>, (StatusCode, Json<ErrorResponse>)> {
    // Batch all stats queries in a single transaction to minimize lock holding time
    let db = acquire_db(&state)?;

    let conn = db.conn();

    // Use a single transaction for all queries
    let tx = conn.unchecked_transaction().map_err(|_e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: "Failed to start transaction".to_string(),
            }),
        )
    })?;

    // Surface query failures as 500 instead of reporting a misleading 0
    // (these are core schema tables; a failure means real corruption, not "0 rows").
    let count = |sql: &str| -> Result<i64, (StatusCode, Json<ErrorResponse>)> {
        tx.query_row(sql, [], |r| r.get::<_, i64>(0)).map_err(|e| {
            tracing::warn!("stats query failed ({}): {}", sql, e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    error: format!("stats query failed: {}", e),
                }),
            )
        })
    };

    let sessions: i64 = count("SELECT COUNT(*) FROM sessions")?;
    let turns: i64 = count("SELECT COUNT(*) FROM turns")?;
    let vectors: i64 = count("SELECT COUNT(*) FROM vec_turns_rowids")?;
    let entities: i64 = count("SELECT COUNT(*) FROM entities")?;
    let relations: i64 = count("SELECT COUNT(*) FROM relations")?;

    // Commit transaction (read-only, but good practice)
    let _ = tx.commit();

    Ok(Json(StatsResponse {
        sessions,
        turns,
        vectors,
        entities,
        relations,
    }))
}

/// Parse a turn timestamp value — supports both epoch ms (i64) and ISO 8601 string.
/// Returns `default` if neither format is parseable.
fn parse_timestamp(v: &serde_json::Value, default: i64) -> i64 {
    v.as_i64().or_else(|| {
        v.as_str()
            .and_then(|s| crate::util::time::ts_to_unix_ms(s).ok())
    }).unwrap_or(default)
}

/// Append turn lines to a JSONL file. Creates the file with header if it doesn't exist,
/// otherwise appends lines only. Ensures parent directory exists.
fn append_jsonl_turns(
    jsonl_path: &std::path::Path,
    header: &crate::fact::conversation::SessionHeader,
    turns: &[crate::fact::conversation::Turn],
) -> anyhow::Result<()> {
    use std::io::Write;

    // Ensure parent directory exists (W2 fix)
    if let Some(parent) = jsonl_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(jsonl_path)?;

    // Check file size AFTER opening (avoids TOCTOU race between exists() and open())
    if file.metadata().map(|m| m.len() == 0).unwrap_or(true) {
        // Empty or new file: write header line first
        writeln!(file, "{}", serde_json::to_string(header)?)?;
    }

    for turn in turns {
        writeln!(file, "{}", serde_json::to_string(turn)?)?;
    }

    Ok(())
}

/// Validate /capture input: non-empty session_id, non-empty turns array, and
/// every turn must be an object with 'role' and 'content' fields.
fn validate_capture_request(
    req: &CaptureRequest,
) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    if req.session_id.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse { error: "session_id is required".into() }),
        ));
    }
    if req.turns.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse { error: "turns array cannot be empty".into() }),
        ));
    }
    for (i, turn) in req.turns.iter().enumerate() {
        let Some(obj) = turn.as_object() else {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse { error: format!("turn[{}] must be an object", i) }),
            ));
        };
        if !obj.contains_key("role") || !obj.contains_key("content") {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse {
                    error: format!("turn[{}] must have 'role' and 'content' fields", i),
                }),
            ));
        }
    }
    Ok(())
}

/// Insert the session row (new session) or bump the existing one inside the
/// capture transaction.
fn upsert_session(
    tx: &rusqlite::Transaction<'_>,
    session_id: &str,
    session_start_ts: Option<i64>,
    first_ts: i64,
    turn_count: i64,
    now: i64,
) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    if session_start_ts.is_none() {
        tx.execute(
            "INSERT INTO sessions (session_id, start_ts, file_path, turn_count, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
            params![
                session_id,
                first_ts,
                format!("gateway://{}", session_id),
                turn_count,
                now,
            ],
        ).map_err(|e| (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse { error: format!("insert session: {}", e) }),
        ))?;
    } else {
        tx.execute(
            "UPDATE sessions SET turn_count = turn_count + ?1, updated_at = ?2 WHERE session_id = ?3",
            params![turn_count, now, session_id],
        ).map_err(|e| (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse { error: format!("update session: {}", e) }),
        ))?;
    }
    Ok(())
}

/// Insert all turns of a capture request and return (turn_id, content,
/// embedding) records for the later embedding/JSONL steps. Embeddings were
/// pre-computed outside the DB lock.
fn insert_capture_turns(
    tx: &rusqlite::Transaction<'_>,
    session_id: &str,
    turns: &[serde_json::Value],
    turn_embeddings: &[Option<Vec<f32>>],
    now: i64,
    max_seq: i64,
    preview_length: usize,
) -> Result<Vec<TurnRecord>, HttpError> {
    let mut turn_records: Vec<TurnRecord> = Vec::with_capacity(turns.len());

    for (i, turn_val) in turns.iter().enumerate() {
        let obj = turn_val.as_object().unwrap();
        let role = obj.get("role").and_then(|v| v.as_str()).unwrap_or("");
        let content = obj.get("content").and_then(|v| v.as_str()).unwrap_or("");
        let timestamp_ms = obj.get("timestamp")
            .map(|v| parse_timestamp(v, now))
            .unwrap_or(now);

        let preview: String = content.chars().take(preview_length).collect();
        let char_count = content.chars().count() as i64;
        let seq = max_seq + (i as i64) + 1;

        tx.execute(
            "INSERT INTO turns (session_id, seq, timestamp_ms, role, preview, char_count)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![session_id, seq, timestamp_ms, role, preview, char_count],
        ).map_err(|e| (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse { error: format!("insert turn[{}]: {}", i, e) }),
        ))?;

        let turn_id = tx.last_insert_rowid();

        // Embedding was pre-computed above (outside the lock/transaction).
        let embedding = turn_embeddings[i].clone();

        turn_records.push((turn_id, content.to_string(), embedding));
    }

    Ok(turn_records)
}

/// Store pre-computed turn embeddings in vec_turns (M1: log failures instead
/// of silent discard).
fn index_turn_embeddings(
    tx: &rusqlite::Transaction<'_>,
    turn_records: &[(i64, String, Option<Vec<f32>>)],
) {
    for (turn_id, _content, embedding) in turn_records {
        if let Some(emb) = embedding {
            // vec_turns is int8[dim]; must quantize + wrap in vec_int8() (matching
            // VectorStore::insert). Writing raw f32 le-bytes here produced a
            // 4×-sized blob that vec0 rejected, so /capture turns were never indexed.
            let embedding_bytes = crate::embedder::onnx::quantize_to_int8(emb);
            if let Err(e) = tx.execute(
                "INSERT INTO vec_turns (rowid, embedding) VALUES (?1, vec_int8(?2))",
                params![*turn_id, embedding_bytes],
            ) {
                tracing::warn!("vec_turns insert failed for turn {}: {}", turn_id, e);
            }
        }
    }
}

/// Best-effort JSONL archival of captured turns (non-transactional).
#[allow(clippy::too_many_arguments)] // extraction boundary from the capture handler
fn archive_session_jsonl(
    config: &crate::config::Config,
    session_id: &str,
    session_start_ts: Option<i64>,
    first_ts: i64,
    now: i64,
    max_seq: i64,
    turns: &[serde_json::Value],
    turn_records: &[(i64, String, Option<Vec<f32>>)],
) {
    let start_iso = crate::util::time::unix_ms_to_iso(
        session_start_ts.unwrap_or(first_ts),
    );
    let header = crate::fact::conversation::SessionHeader {
        v: 1,
        header_type: "session_header".to_string(),
        session_id: session_id.to_string(),
        start_time: start_iso,
        profile_id: config.profile_id.clone(),
        source: Some("gateway".to_string()),
        agent_model: None,
        title: None,
        tags: vec![],
    };

    if let Ok(jsonl_path) = crate::fact::conversation::compute_session_path(
        &config.conversations_dir(), &header,
    ) {
        let jsonl_turns: Vec<crate::fact::conversation::Turn> = turn_records.iter().enumerate()
            .map(|(i, (_id, content, _emb))| {
                let obj = turns[i].as_object().unwrap();
                let ts_ms = obj.get("timestamp")
                    .map(|v| parse_timestamp(v, now))
                    .unwrap_or(now);
                let seq_num = u32::try_from(max_seq + (i as i64) + 1)
                    .unwrap_or(u32::MAX);
                crate::fact::conversation::Turn {
                    ts: crate::util::time::unix_ms_to_iso(ts_ms),
                    seq: seq_num,
                    role: obj.get("role").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    content: content.clone(),
                    metadata: None,
                }
            })
            .collect();

        if let Err(e) = append_jsonl_turns(&jsonl_path, &header, &jsonl_turns) {
            // W3: log JSONL failures instead of silent discard
            tracing::warn!("JSONL append failed for session {}: {}", session_id, e);
        }
    }
}

async fn capture(
    State(state): State<AppState>,
    Json(req): Json<CaptureRequest>,
) -> Result<Json<CaptureResponse>, (StatusCode, Json<ErrorResponse>)> {
    // ── Validate input ──────────────────────────────────────────
    validate_capture_request(&req)?;

    let now = chrono::Utc::now().timestamp_millis();
    let preview_length = state.config.conversation.preview_length;

    // Pre-compute embeddings BEFORE taking the DB lock + transaction (M1): the
    // blocking embedding call (HTTP for the API backend) must not hold the global
    // DB lock or an open write transaction across the network round-trip.
    let turn_contents: Vec<String> = req.turns.iter()
        .map(|t| t.as_object()
            .and_then(|o| o.get("content"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string())
        .collect();
    let turn_embeddings: Vec<Option<Vec<f32>>> = {
        let embedder_guard = state.embedder.as_ref().and_then(|emb| emb.lock().ok());
        match embedder_guard {
            Some(guard) => turn_contents.iter().map(|c| guard.embed_document(c).ok()).collect(),
            None => vec![None; turn_contents.len()],
        }
    };

    let db = acquire_db(&state)?;
    let conn = db.conn();

    // Parse first turn timestamp (W8: uses shared helper)
    let first_ts = req.turns.iter()
        .filter_map(|t| t.as_object())
        .filter_map(|o| o.get("timestamp"))
        .map(|v| parse_timestamp(v, now))
        .next()
        .unwrap_or(now);

    // Check if session already exists — determines start_ts for JSONL path
    let session_start_ts: Option<i64> = conn.query_row(
        "SELECT start_ts FROM sessions WHERE session_id = ?1",
        params![req.session_id],
        |r| r.get(0),
    ).ok();

    // ── Transaction: session + turns + embeddings (W1: atomicity) ──
    let tx = conn.unchecked_transaction().map_err(|e| (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ErrorResponse { error: format!("begin transaction: {}", e) }),
    ))?;

    upsert_session(
        &tx,
        &req.session_id,
        session_start_ts,
        first_ts,
        req.turns.len() as i64,
        now,
    )?;

    let max_seq: i64 = tx.query_row(
        "SELECT COALESCE(MAX(seq), 0) FROM turns WHERE session_id = ?1",
        params![req.session_id],
        |r| r.get(0),
    ).unwrap_or(0);

    // Insert turns (embeddings were pre-computed above, outside the DB lock)
    let turn_records = insert_capture_turns(
        &tx,
        &req.session_id,
        &req.turns,
        &turn_embeddings,
        now,
        max_seq,
        preview_length,
    )?;

    // Store embeddings in vec_turns (M1: log failures instead of silent discard)
    index_turn_embeddings(&tx, &turn_records);

    // Commit transaction — all or nothing (W1)
    tx.commit().map_err(|e| (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ErrorResponse { error: format!("commit transaction: {}", e) }),
    ))?;

    // ── JSONL archival (best-effort, non-transactional) ─────────
    // W6: use append mode instead of read-all + write-all
    // W2: append_jsonl_turns ensures parent directory exists
    archive_session_jsonl(
        &state.config,
        &req.session_id,
        session_start_ts,
        first_ts,
        now,
        max_seq,
        &req.turns,
        &turn_records,
    );

    Ok(Json(CaptureResponse {
        status: "ok".to_string(),
        turns_saved: req.turns.len(),
    }))
}

// ── recall helpers (v2.6: extracted to keep cognitive complexity <= 15) ──

/// Parse the optional time window. Semantics identical to /search (v2.5.1):
/// malformed timestamps return 400, never a silently widened window;
/// last_days clamps to [0, 36500] and overrides `after`.
fn parse_time_window(
    after: Option<&str>,
    before: Option<&str>,
    last_days: Option<i64>,
) -> Result<(Option<i64>, Option<i64>), HttpError> {
    let parse_ts = |label: &str, s: &str| -> Result<i64, HttpError> {
        crate::util::time::ts_to_unix_ms(s).map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse {
                    error: format!("invalid {}: {}", label, e),
                }),
            )
        })
    };
    let after_ms = match after {
        Some(s) => Some(parse_ts("after", s)?),
        None => None,
    };
    let before_ms = match before {
        Some(s) => Some(parse_ts("before", s)?),
        None => None,
    };
    let effective_after = if let Some(days) = last_days {
        let days = days.clamp(0, 36_500);
        Some(crate::util::time::now_unix_ms() - days * crate::util::time::MS_PER_DAY)
    } else {
        after_ms
    };
    Ok((effective_after, before_ms))
}

/// L3 persona: bounded_memory target='user', fall back to USER.md.
fn recall_persona(db: &crate::index::db::Db, config: &crate::config::Config) -> Vec<serde_json::Value> {
    let mut out = Vec::new();
    let mut persona_found = false;
    let row = db.conn().query_row(
        "SELECT content FROM bounded_memory WHERE target = 'user' AND content IS NOT NULL AND content != '' ORDER BY updated_at DESC LIMIT 1",
        [],
        |row| row.get::<_, String>(0),
    );
    match row {
        Ok(persona) if !persona.trim().is_empty() => {
            out.push(serde_json::json!({ "layer": "L3", "type": "persona", "content": persona }));
            persona_found = true;
        }
        Ok(_) => {}
        Err(rusqlite::Error::QueryReturnedNoRows) => {}
        Err(e) => tracing::warn!("recall L3 bounded_memory query error: {}", e),
    }
    if !persona_found {
        let user_md_path = config.memory_dir().join("USER.md");
        if user_md_path.exists() {
            if let Ok(persona) = std::fs::read_to_string(&user_md_path) {
                let trimmed = persona.trim().to_string();
                if !trimmed.is_empty() {
                    out.push(serde_json::json!({ "layer": "L3", "type": "persona", "content": trimmed }));
                }
            }
        }
    }
    out
}

/// L2 scenarios (memory_type='scenario'), ordered by updated_at, capped at top_k.
fn recall_scenarios(db: &crate::index::db::Db, top_k: usize) -> Vec<serde_json::Value> {
    let mut out = Vec::new();
    let rows = match db.conn().prepare(
        "SELECT content FROM bounded_memory WHERE COALESCE(memory_type, 'manual') = 'scenario' ORDER BY updated_at DESC LIMIT ?1"
    ) {
        Ok(mut stmt) => match stmt.query_map([top_k as i64], |row| row.get::<_, String>(0)) {
            Ok(scenarios) => scenarios.filter_map(|r| r.ok()).collect::<Vec<_>>(),
            Err(e) => { tracing::warn!("recall L2 scenario query error (skipping L2 layer): {}", e); Vec::new() }
        },
        Err(e) => { tracing::warn!("recall L2 scenario prepare error (skipping L2 layer): {}", e); Vec::new() }
    };
    for content in rows {
        out.push(serde_json::json!({ "layer": "L2", "type": "scenario", "content": content }));
    }
    out
}

/// L1 atoms via FTS, with optional created_at window before LIMIT.
fn recall_atoms(
    db: &crate::index::db::Db,
    fts_query: &str,
    after: Option<i64>,
    before: Option<i64>,
    top_k: usize,
) -> Vec<serde_json::Value> {
    let mut out = Vec::new();
    let mut sql = String::from(
        "SELECT bm.content,
                CASE bm.confidence WHEN 'high' THEN 1.0 WHEN 'medium' THEN 0.5 ELSE 0.25 END,
                COALESCE(bm.memory_type, 'manual'),
                bm.created_at
         FROM bounded_memory bm
         JOIN bounded_memory_fts fts ON bm.id = fts.rowid
         WHERE bounded_memory_fts MATCH ?1",
    );
    let mut next = 2usize;
    let mut time_binds: Vec<i64> = Vec::new();
    if let Some(a) = after {
        sql.push_str(&format!(" AND bm.created_at >= ?{}", next));
        next += 1;
        time_binds.push(a);
    }
    if let Some(b) = before {
        sql.push_str(&format!(" AND bm.created_at <= ?{}", next));
        next += 1;
        time_binds.push(b);
    }
    sql.push_str(" ORDER BY CASE bm.confidence WHEN 'high' THEN 1.0 WHEN 'medium' THEN 0.5 ELSE 0.25 END DESC, bm.updated_at DESC");
    sql.push_str(&format!(" LIMIT ?{}", next));

    let mut binds: Vec<Box<dyn rusqlite::types::ToSql>> = vec![Box::new(fts_query.to_string())];
    for t in &time_binds {
        binds.push(Box::new(*t));
    }
    binds.push(Box::new(top_k as i64));
    let refs: Vec<&dyn rusqlite::types::ToSql> = binds.iter().map(|b| b.as_ref()).collect();

    let rows = match db.conn().prepare(&sql) {
        Ok(mut stmt) => match stmt.query_map(refs.as_slice(), |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?, row.get::<_, String>(2)?, row.get::<_, i64>(3)?))
        }) {
            Ok(atoms) => atoms.filter_map(|r| r.ok()).collect::<Vec<_>>(),
            Err(e) => { tracing::warn!("recall L1 FTS query error (skipping L1 layer): {}", e); Vec::new() }
        },
        Err(e) => { tracing::warn!("recall L1 FTS prepare error (skipping L1 layer): {}", e); Vec::new() }
    };
    for (content, confidence, memory_type, created_at) in rows {
        out.push(serde_json::json!({
            "layer": "L1", "type": memory_type, "content": content,
            "confidence": confidence, "created_at": created_at,
            "ordered_by": "confidence+recency"
        }));
    }
    out
}

/// L0 recent turns via LIKE, with optional timestamp_ms window before LIMIT.
fn recall_turns(
    db: &crate::index::db::Db,
    search_pattern: &str,
    after: Option<i64>,
    before: Option<i64>,
    top_k: usize,
) -> Result<Vec<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let mut out = Vec::new();
    let mut sql = String::from("SELECT role, preview, timestamp_ms FROM turns WHERE preview LIKE ?1 ESCAPE '\\'");
    let mut next = 2usize;
    let mut time_binds: Vec<i64> = Vec::new();
    if let Some(a) = after {
        sql.push_str(&format!(" AND timestamp_ms >= ?{}", next));
        next += 1;
        time_binds.push(a);
    }
    if let Some(b) = before {
        sql.push_str(&format!(" AND timestamp_ms <= ?{}", next));
        next += 1;
        time_binds.push(b);
    }
    sql.push_str(&format!(" ORDER BY timestamp_ms DESC LIMIT ?{}", next));

    let mut binds: Vec<Box<dyn rusqlite::types::ToSql>> = vec![Box::new(search_pattern.to_string())];
    for t in &time_binds {
        binds.push(Box::new(*t));
    }
    binds.push(Box::new(top_k as i64));
    let refs: Vec<&dyn rusqlite::types::ToSql> = binds.iter().map(|b| b.as_ref()).collect();

    let mut stmt = db.conn().prepare(&sql).map_err(|e| {
        (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorResponse { error: format!("prepare turns query: {}", e) }))
    })?;
    let turns = stmt.query_map(refs.as_slice(), |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, i64>(2)?))
    }).map_err(|e| {
        (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorResponse { error: format!("query turns: {}", e) }))
    })?;
    for turn in turns {
        match turn {
            Ok((role, content, timestamp)) => out.push(serde_json::json!({
                "layer": "L0", "type": "turn", "role": role, "content": content, "timestamp": timestamp
            })),
            Err(e) => tracing::warn!("recall L0 turn row parse error: {}", e),
        }
    }
    Ok(out)
}

/// v2.6 token budget: greedy prefix cut in layer order. The first item that
/// does not fit is dropped whole (never truncated); returns whether anything
/// was dropped. (Hindsight _filter_by_token_budget parity.)
fn apply_token_budget(memories: &mut Vec<serde_json::Value>, budget: usize) -> bool {
    let mut used = 0usize;
    let mut keep = memories.len();
    for (i, m) in memories.iter().enumerate() {
        let est = m
            .get("content")
            .and_then(|v| v.as_str())
            .map(crate::util::text::estimate_tokens)
            .unwrap_or(0);
        if used + est > budget {
            keep = i;
            break;
        }
        used += est;
    }
    let truncated = keep < memories.len();
    memories.truncate(keep);
    truncated
}

/// Rebuild the context string from surviving memories. L0 turns are excluded
/// (same as v2.5.3).
fn rebuild_context(memories: &[serde_json::Value]) -> String {
    memories
        .iter()
        .filter_map(|m| {
            let layer = m.get("layer")?.as_str()?;
            let content = m.get("content")?.as_str()?;
            Some(match layer {
                "L3" => format!("[Persona] {}", content),
                "L2" => format!("[Scenario] {}", content),
                "L1" => format!(
                    "[{}] {}",
                    m.get("type").and_then(|t| t.as_str()).unwrap_or("atom"),
                    content
                ),
                _ => return None,
            })
        })
        .collect::<Vec<_>>()
        .join("\n")
}

async fn recall(
    State(state): State<AppState>,
    Json(req): Json<RecallRequest>,
) -> Result<Json<RecallResponse>, (StatusCode, Json<ErrorResponse>)> {
    // Validate input
    if req.query.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse { error: "query cannot be empty".to_string() }),
        ));
    }
    if req.query.len() > 10000 {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse { error: "Query too long (max 10000 characters)".to_string() }),
        ));
    }

    let top_k = req.top_k.unwrap_or(10).min(50); // Cap at 50
    let (effective_after, before_ms) =
        parse_time_window(req.after.as_deref(), req.before.as_deref(), req.last_days)?;
    let db = acquire_db(&state)?;

    // Progressive disclosure: L3 -> L2 -> L1 -> L0
    let mut memories = Vec::new();
    memories.extend(recall_persona(&db, &state.config));
    memories.extend(recall_scenarios(&db, top_k));
    let fts_query = format!("\"{}\"", req.query.replace('"', "\"\""));
    memories.extend(recall_atoms(&db, &fts_query, effective_after, before_ms, top_k));
    let search_pattern = format!("%{}%", escape_like(&req.query));
    memories.extend(recall_turns(&db, &search_pattern, effective_after, before_ms, top_k)?);

    let budget = req.max_tokens.unwrap_or(state.config.recall.token_budget);
    let truncated = apply_token_budget(&mut memories, budget);
    let context = rebuild_context(&memories);

    Ok(Json(RecallResponse {
        memories,
        context,
        truncated,
    }))
}

/// Multi-hop graph search (entity-centric). Validation is done by the caller.
fn search_multi_hop(
    state: &AppState,
    entity: &str,
    max_hops: u32,
    relation_filter: Option<&str>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let db = acquire_db(state)?;
    let atom_ids = crate::memory::graph_integration::multi_hop_query(&db, entity, max_hops, relation_filter)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorResponse { error: format!("multi-hop query failed: {}", e) })))?;
    let atoms = if atom_ids.is_empty() {
        Vec::new()
    } else {
        batch_fetch_atoms(&db, &atom_ids)?
    };
    Ok(Json(serde_json::json!({
        "results": atoms,
        "query_type": "multi_hop",
        "entity": entity,
        "max_hops": max_hops,
        "status": "ok"
    })))
}

/// Fetch atom details for a list of ids in a single batched query (avoid N+1).
fn batch_fetch_atoms(
    db: &crate::index::db::Db,
    atom_ids: &[i64],
) -> Result<Vec<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let placeholders: Vec<String> = (1..=atom_ids.len()).map(|i| format!("?{}", i)).collect();
    let in_clause = placeholders.join(", ");
    let sql = format!(
        "SELECT id, content, COALESCE(memory_type, 'manual'),
                CASE confidence WHEN 'high' THEN 1.0 WHEN 'medium' THEN 0.5 ELSE 0.25 END,
                created_at
         FROM bounded_memory WHERE id IN ({})
         ORDER BY CASE confidence WHEN 'high' THEN 1.0 WHEN 'medium' THEN 0.5 ELSE 0.25 END DESC",
        in_clause
    );
    let mut stmt = db.conn().prepare(&sql).map_err(|e| {
        (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorResponse { error: format!("prepare batch query: {}", e) }))
    })?;
    let params: Vec<Box<dyn rusqlite::types::ToSql>> = atom_ids
        .iter()
        .map(|id| Box::new(*id) as Box<dyn rusqlite::types::ToSql>)
        .collect();
    let param_refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
    let rows = stmt.query_map(param_refs.as_slice(), |row| {
        Ok(serde_json::json!({
            "id": row.get::<_, i64>(0)?,
            "content": row.get::<_, String>(1)?,
            "memory_type": row.get::<_, String>(2)?,
            "confidence_score": row.get::<_, f64>(3)?,
            "created_at": row.get::<_, i64>(4)?
        }))
    }).map_err(|e| {
        (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorResponse { error: format!("batch query execution: {}", e) }))
    })?;
    Ok(rows.filter_map(|r| r.ok()).collect())
}

/// Convert search results to JSON with v2.6 score transparency (per-source
/// `scores` components summing to `score`).
fn search_results_to_json(results: &[crate::fact::search::SearchResult]) -> Vec<serde_json::Value> {
    results.iter().map(|r| {
        let mut obj = serde_json::json!({
            "turn_id": r.turn_id,
            "score": r.score,
            "preview": r.preview,
            "session_id": r.session_id,
            "timestamp_ms": r.timestamp_ms,
            "role": r.role,
        });
        if let Some(ref breakdown) = r.scores {
            obj["scores"] = serde_json::to_value(breakdown).unwrap_or(serde_json::Value::Null);
        }
        obj
    }).collect()
}

async fn search(
    State(state): State<AppState>,
    Json(req): Json<SearchRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    // Input validation
    if req.entity.is_none() && req.query.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse { error: "query cannot be empty for text search".to_string() }),
        ));
    }
    if req.query.len() > 10000 {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse { error: "Query too long (max 10000 characters)".to_string() }),
        ));
    }

    // P8: Multi-hop graph queries
    if let Some(entity) = req.entity {
        if entity.len() > 1000 {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse { error: "Entity name too long (max 1000 characters)".to_string() }),
            ));
        }
        let max_hops = req.max_hops.unwrap_or(2);
        if max_hops > 10 {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse { error: "max_hops too large (max 10)".to_string() }),
            ));
        }
        return search_multi_hop(&state, &entity, max_hops, req.relation_filter.as_deref());
    }

    // Traditional text search - delegate to existing search logic
    let db = acquire_db(&state)?;

    // Determine search mode
    let search_mode = match req.mode.as_deref() {
        Some("semantic") => crate::fact::search::SearchMode::Semantic,
        Some("keyword") => crate::fact::search::SearchMode::Keyword,
        _ => crate::fact::search::SearchMode::Hybrid, // Default to hybrid
    };

    let top_k = req.top_k.unwrap_or(10).min(50); // Cap at 50

    // Reuse parse_time_window (parity with /recall and /search v2.5.1)
    let (effective_after, before_ms) =
        parse_time_window(req.after.as_deref(), req.before.as_deref(), req.last_days)?;

    let params = crate::fact::search::SearchParams {
        query: req.query.clone(),
        search_mode,
        top_k,
        after_ms: effective_after,
        before_ms,
        role: req.role.clone(),
    };

    // Get embedder if available
    let embedder = state.embedder.as_ref().and_then(|e| e.lock().ok());

    let results = crate::fact::search::search_sessions(&db, embedder.as_deref(), &params)
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    error: format!("search failed: {}", e),
                }),
            )
        })?;

    let results_json = search_results_to_json(&results);

    Ok(Json(serde_json::json!({
        "results": results_json,
        "query_type": "text",
        "count": results_json.len(),
        "status": "ok"
    })))
}

async fn persona(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let config = state.config.as_ref();
    let memory_dir = config.memory_dir();

    // Priority 1: Read USER.md (the canonical user profile file)
    let user_md_path = memory_dir.join("USER.md");
    if user_md_path.exists() {
        if let Ok(content) = std::fs::read_to_string(&user_md_path) {
            if !content.trim().is_empty() {
                return Ok(Json(serde_json::json!({
                    "persona": content,
                    "status": "ok",
                    "source": "USER.md"
                })));
            }
        }
    }

    // Priority 2: Read persona.md (legacy path)
    let persona_path = memory_dir.join("persona.md");
    if persona_path.exists() {
        if let Ok(content) = std::fs::read_to_string(&persona_path) {
            if !content.trim().is_empty() {
                return Ok(Json(serde_json::json!({
                    "persona": content,
                    "status": "ok",
                    "source": "persona.md"
                })));
            }
        }
    }

    // Priority 3: Fallback to bounded_memory table (target='user')
    let db = acquire_db(&state)?;
    if let Ok(content) = db.conn().query_row(
        "SELECT content FROM bounded_memory WHERE target = 'user' ORDER BY updated_at DESC LIMIT 1",
        [],
        |row| row.get::<_, String>(0),
    ) {
        if !content.trim().is_empty() {
            return Ok(Json(serde_json::json!({
                "persona": content,
                "status": "ok",
                "source": "bounded_memory"
            })));
        }
    }

    Ok(Json(serde_json::json!({
        "persona": null,
        "status": "not_found"
    })))
}

async fn graph_assert(
    State(state): State<AppState>,
    Json(req): Json<GraphAssertRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    // Validate input
    if req.subject.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: "subject cannot be empty".to_string(),
            }),
        ));
    }

    if req.predicate.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: "predicate cannot be empty".to_string(),
            }),
        ));
    }

    if req.object.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: "object cannot be empty".to_string(),
            }),
        ));
    }

    // Validate lengths
    if req.subject.len() > 1000 || req.predicate.len() > 1000 || req.object.len() > 1000 {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: "subject, predicate, and object must be <= 1000 characters".to_string(),
            }),
        ));
    }

    // Validate confidence if provided
    let confidence = if let Some(conf_str) = &req.confidence {
        match conf_str.parse::<f64>() {
            Ok(c) if (0.0..=1.0).contains(&c) => c,
            Ok(c) => {
                return Err((
                    StatusCode::BAD_REQUEST,
                    Json(ErrorResponse {
                        error: format!("confidence must be between 0.0 and 1.0, got {}", c),
                    }),
                ));
            }
            Err(_) => {
                return Err((
                    StatusCode::BAD_REQUEST,
                    Json(ErrorResponse {
                        error: format!("invalid confidence value: {}", conf_str),
                    }),
                ));
            }
        }
    } else {
        0.5 // Default confidence
    };

    let db = acquire_db(&state)?;

    // Canonicalize entity names
    let subject_canonical = crate::graph::canonical::canonicalize(&req.subject);
    let object_canonical = crate::graph::canonical::canonicalize(&req.object);

    let now = crate::util::time::now_unix_ms();

    // Begin transaction
    let tx = db.conn().unchecked_transaction().map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("begin transaction: {}", e),
            }),
        )
    })?;

    // Insert or update subject entity
    db.conn().execute(
        "INSERT INTO entities (canonical, name, entity_type, first_seen, last_seen)
         VALUES (?1, ?2, 'unknown', ?3, ?3)
         ON CONFLICT(canonical) DO UPDATE SET last_seen = ?3",
        params![subject_canonical, req.subject, now],
    ).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("insert subject entity: {}", e),
            }),
        )
    })?;

    // Insert or update object entity
    db.conn().execute(
        "INSERT INTO entities (canonical, name, entity_type, first_seen, last_seen)
         VALUES (?1, ?2, 'unknown', ?3, ?3)
         ON CONFLICT(canonical) DO UPDATE SET last_seen = ?3",
        params![object_canonical, req.object, now],
    ).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("insert object entity: {}", e),
            }),
        )
    })?;

    // Insert or update relation
    db.conn().execute(
        "INSERT INTO relations (src_canonical, rel_type, dst_canonical, confidence, source_turn, relation_kind, created_at)
         VALUES (?1, ?2, ?3, ?4, NULL, 'asserted', ?5)
         ON CONFLICT(src_canonical, rel_type, dst_canonical) DO UPDATE SET
         confidence = MAX(relations.confidence, ?4),
         created_at = ?5",
        params![subject_canonical, req.predicate, object_canonical, confidence, now],
    ).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("insert relation: {}", e),
            }),
        )
    })?;

    tx.commit().map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("commit transaction: {}", e),
            }),
        )
    })?;

    Ok(Json(serde_json::json!({
        "status": "ok",
        "subject": subject_canonical,
        "predicate": req.predicate,
        "object": object_canonical,
        "confidence": confidence,
    })))
}

async fn graph_neighbors(
    State(state): State<AppState>,
    Json(req): Json<GraphNeighborsRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    // Input validation
    if req.entity.len() > 1000 {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: "Entity name too long (max 1000 characters)".to_string(),
            }),
        ));
    }

    let hops = req.hops.unwrap_or(1);
    if hops > 10 {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: "hops too large (max 10)".to_string(),
            }),
        ));
    }

    let db = acquire_db(&state)?;

    let canonical = crate::graph::canonical::canonicalize(&req.entity);
    let direction = req.direction.as_deref().unwrap_or("both");
    let relation_kind = req.relation_kind.as_deref();
    let max_results = hops * 10;

    // Build query based on direction
    // For "both" direction: use CTE to apply LIMIT to each direction separately,
    // then UNION the results. This prevents LIMIT from skewing toward out-direction.
    let sql = match direction {
        "out" => {
            if relation_kind.is_some() {
                "SELECT dst_canonical, rel_type, confidence, relation_kind
                     FROM relations
                     WHERE src_canonical = ?1 AND relation_kind = ?2
                     LIMIT ?3".to_string()
            } else {
                "SELECT dst_canonical, rel_type, confidence, relation_kind
                     FROM relations
                     WHERE src_canonical = ?1
                     LIMIT ?2".to_string()
            }
        }
        "in" => {
            if relation_kind.is_some() {
                "SELECT src_canonical, rel_type, confidence, relation_kind
                     FROM relations
                     WHERE dst_canonical = ?1 AND relation_kind = ?2
                     LIMIT ?3".to_string()
            } else {
                "SELECT src_canonical, rel_type, confidence, relation_kind
                     FROM relations
                     WHERE dst_canonical = ?1
                     LIMIT ?2".to_string()
            }
        }
        _ => {
            // both directions — wrap each SELECT in a subquery with its own LIMIT
            // to prevent LIMIT from applying to the entire UNION (which skews results)
            let half_limit = max_results / 2 + 1;
            if relation_kind.is_some() {
                format!(
                    "SELECT * FROM (
                        SELECT dst_canonical, rel_type, confidence, relation_kind
                        FROM relations WHERE src_canonical = ?1 AND relation_kind = ?2 LIMIT {half}
                    ) UNION SELECT * FROM (
                        SELECT src_canonical, rel_type, confidence, relation_kind
                        FROM relations WHERE dst_canonical = ?1 AND relation_kind = ?2 LIMIT {half}
                    ) LIMIT ?3",
                    half = half_limit
                )
            } else {
                format!(
                    "SELECT * FROM (
                        SELECT dst_canonical, rel_type, confidence, relation_kind
                        FROM relations WHERE src_canonical = ?1 LIMIT {half}
                    ) UNION SELECT * FROM (
                        SELECT src_canonical, rel_type, confidence, relation_kind
                        FROM relations WHERE dst_canonical = ?1 LIMIT {half}
                    ) LIMIT ?2",
                    half = half_limit
                )
            }
        }
    };

    let mut stmt = db.conn().prepare(&sql).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("prepare query: {}", e),
            }),
        )
    })?;

    let neighbors: Vec<serde_json::Value> = if let Some(kind) = relation_kind {
        stmt.query_map(
            rusqlite::params![canonical, kind, (hops * 10) as i64],
            |row| {
                Ok(serde_json::json!({
                    "entity": row.get::<_, String>(0)?,
                    "relation": row.get::<_, String>(1)?,
                    "confidence": row.get::<_, f64>(2)?,
                    "relation_kind": row.get::<_, String>(3)?
                }))
            },
        )
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    error: format!("query execution: {}", e),
                }),
            )
        })?
        .filter_map(|r| r.ok())
        .collect()
    } else {
        stmt.query_map(rusqlite::params![canonical, (hops * 10) as i64], |row| {
            Ok(serde_json::json!({
                "entity": row.get::<_, String>(0)?,
                "relation": row.get::<_, String>(1)?,
                "confidence": row.get::<_, f64>(2)?,
                "relation_kind": row.get::<_, String>(3)?
            }))
        })
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    error: format!("query execution: {}", e),
                }),
            )
        })?
        .filter_map(|r| r.ok())
        .collect()
    };

    Ok(Json(serde_json::json!({
        "entity": req.entity,
        "canonical": canonical,
        "neighbors": neighbors,
        "count": neighbors.len(),
        "status": "ok"
    })))
}

async fn session_end(
    State(state): State<AppState>,
    Json(req): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    // Extract session_id from request
    let session_id = req
        .get("session_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse {
                    error: "session_id is required".to_string(),
                }),
            )
        })?;

    if session_id.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: "session_id cannot be empty".to_string(),
            }),
        ));
    }

    let db = acquire_db(&state)?;

    // Verify session exists (using correct schema column: session_id, not id)
    let session_exists: bool = db.conn().query_row(
        "SELECT COUNT(*) > 0 FROM sessions WHERE session_id = ?1",
        params![session_id],
        |row| row.get(0),
    ).unwrap_or(false);

    if !session_exists {
        return Err((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: format!("session not found: {}", session_id),
            }),
        ));
    }

    // Log session end event.
    tracing::info!("Session end recorded for session_id={}", session_id);

    // Update session end timestamp (using correct schema column: session_id)
    let now = crate::util::time::now_unix_ms();
    db.conn().execute(
        "UPDATE sessions SET end_ts = ?1, updated_at = ?1 WHERE session_id = ?2",
        params![now, session_id],
    ).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("update session end_ts: {}", e),
            }),
        )
    })?;

    // Spawn post-session pipeline (L1 extraction + graph integration)
    // Runs as a blocking task so LLM/DB calls don't starve the tokio runtime.
    if let Some(ref llm) = state.llm {
        let sid = session_id.to_string();
        let db_clone = state.db.clone();
        let llm_clone = llm.clone();
        let emb_clone = state.embedder.clone();
        let cfg_clone = state.config.clone();
        tokio::task::spawn_blocking(move || {
            crate::transport::pipeline::run_pipeline(
                db_clone, llm_clone, emb_clone, cfg_clone, sid,
            );
        });
    }

    Ok(Json(serde_json::json!({
        "status": "ok",
        "session_id": session_id,
        "end_ts": now,
        "pipeline": if state.llm.is_some() { "spawned" } else { "skipped (no LLM configured)" },
    })))
}

// ============ Short-term memory handlers ============

async fn offload(
    State(state): State<AppState>,
    Json(req): Json<OffloadRequest>,
) -> Result<Json<OffloadResponse>, (StatusCode, Json<ErrorResponse>)> {
    let refs_dir = state.config.refs_dir();
    let bytes = req.content.len();

    let node_id = crate::short_term::offload_text(&refs_dir, &req.task_id, &req.content)
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    error: format!("offload failed: {}", e),
                }),
            )
        })?;

    Ok(Json(OffloadResponse {
        node_id: node_id.to_string(),
        bytes_stored: bytes,
    }))
}

async fn recall_by_node(
    State(state): State<AppState>,
    Path(node_id_str): Path<String>,
) -> Result<Json<RecallNodeResponse>, (StatusCode, Json<ErrorResponse>)> {
    let node_id = crate::short_term::NodeId::parse(&node_id_str).ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: format!("invalid node_id format: {}", node_id_str),
            }),
        )
    })?;

    let refs_dir = state.config.refs_dir();
    let content = crate::short_term::recall_text(&refs_dir, &node_id).map_err(|e| {
        (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: format!("recall failed: {}", e),
            }),
        )
    })?;

    Ok(Json(RecallNodeResponse {
        node_id: node_id.to_string(),
        content,
    }))
}

// ============ Utility functions ============

/// Escape SQLite LIKE wildcards (%, _, \) using \ as ESCAPE character
fn escape_like(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' | '%' | '_' => {
                out.push('\\');
                out.push(c);
            }
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{is_localhost_origin, recall, RecallRequest, SearchRequest};

    #[test]
    fn test_search_request_accepts_role_and_time_filters() {
        // Regression: SearchRequest previously lacked role/after/before/last_days,
        // so serde silently dropped them and /search ignored the filters.
        let req: SearchRequest = serde_json::from_str(
            r#"{"query":"q","mode":"keyword","top_k":10,"role":"assistant",
                "after":"2026-01-01T00:00:00Z","before":"2026-02-01T00:00:00Z","last_days":7}"#,
        )
        .unwrap();
        assert_eq!(req.role.as_deref(), Some("assistant"));
        assert_eq!(req.after.as_deref(), Some("2026-01-01T00:00:00Z"));
        assert_eq!(req.before.as_deref(), Some("2026-02-01T00:00:00Z"));
        assert_eq!(req.last_days, Some(7));
    }

    #[test]
    fn test_is_localhost_origin() {
        // Allowed: localhost / loopback, any port, http or https
        assert!(is_localhost_origin("http://localhost"));
        assert!(is_localhost_origin("http://localhost:3000"));
        assert!(is_localhost_origin("https://localhost:8080"));
        assert!(is_localhost_origin("http://127.0.0.1:5173"));
        assert!(is_localhost_origin("http://[::1]:9000"));

        // Blocked: public sites (the drive-by read vector) and non-loopback hosts
        assert!(!is_localhost_origin("https://evil.com"));
        assert!(!is_localhost_origin("http://localhost.evil.com"));
        assert!(!is_localhost_origin("http://127.0.0.1.evil.com"));
        assert!(!is_localhost_origin("http://10.0.0.5:3000"));
        assert!(!is_localhost_origin("file://localhost"));
        assert!(!is_localhost_origin("null"));
        assert!(!is_localhost_origin(""));
    }

    #[test]
    fn test_recall_request_accepts_max_tokens_and_legacy_shape() {
        // Hermes sends only {query, top_k} — must keep deserializing.
        let req: RecallRequest = serde_json::from_str(r#"{"query":"q"}"#).unwrap();
        assert!(req.max_tokens.is_none());
        assert!(req.top_k.is_none());
        assert!(req.after.is_none());
        assert!(req.before.is_none());
        assert!(req.last_days.is_none());
        let req: RecallRequest = serde_json::from_str(
            r#"{"query":"q","top_k":5,"max_tokens":500,
                "after":"2026-01-01T00:00:00Z","before":"2026-02-01T00:00:00Z","last_days":7}"#,
        )
        .unwrap();
        assert_eq!(req.top_k, Some(5));
        assert_eq!(req.max_tokens, Some(500));
        assert_eq!(req.after.as_deref(), Some("2026-01-01T00:00:00Z"));
        assert_eq!(req.before.as_deref(), Some("2026-02-01T00:00:00Z"));
        assert_eq!(req.last_days, Some(7));
    }

    #[tokio::test]
    async fn test_recall_token_budget_greedy_cut() {
        use crate::config::Config;
        use crate::index::db::Db;
        use crate::transport::state::AppState;
        use crate::util::text::estimate_tokens;
        use axum::{extract::State, Json};

        let db = Db::open_memory().unwrap();
        db.init_schema().unwrap();
        let state = AppState::new(Config::default(), db, None, None);

        // Seed: 1 persona (L3) + 5 atoms (L1) + 3 turns (L0), all containing 测试
        {
            let d = state.db.lock().unwrap();
            let conn = d.conn();
            conn.execute(
                "INSERT INTO bounded_memory (target, content, created_at, updated_at, memory_type) \
                 VALUES ('user', '用户画像测试', 1000, 1000, 'manual')",
                [],
            )
            .unwrap();
            for i in 0..5 {
                conn.execute(
                    "INSERT INTO bounded_memory (target, content, created_at, updated_at, memory_type) \
                     VALUES ('memory', ?1, 1000, 1000, 'atom')",
                    rusqlite::params![format!("测试内容编号{}", i)],
                )
                .unwrap();
            }
            conn.execute(
                "INSERT INTO sessions (session_id, start_ts, file_path, created_at, updated_at) \
                 VALUES ('s1', 1000, 'f.jsonl', 1000, 1000)",
                [],
            )
            .unwrap();
            for i in 0..3 {
                conn.execute(
                    "INSERT INTO turns (session_id, seq, timestamp_ms, role, preview) \
                     VALUES ('s1', ?1, ?2, 'user', ?3)",
                    rusqlite::params![i as i64, 2000 + i as i64, format!("测试对话{}", i)],
                )
                .unwrap();
            }
        }

        // Default budget (2000): everything fits → byte-parity with v2.5.3 behavior
        let resp = recall(
            State(state.clone()),
            Json(RecallRequest {
                query: "测试".into(),
                top_k: Some(10),
                after: None,
                before: None,
                last_days: None,
                max_tokens: None,
            }),
        )
        .await
        .unwrap()
        .0;
        assert!(!resp.truncated);
        // 1 persona (L3) + 6 L1 FTS matches (5 atoms + the persona row itself:
        // the L1 query has no target filter — pre-existing v2.5.3 behavior) + 3 turns
        assert_eq!(resp.memories.len(), 10);
        // Pin the context rebuild format, not just the count
        assert!(resp.context.starts_with("[Persona] 用户画像测试"));

        // Tight budget: persona (6 tokens) fits, first atom (7 tokens) does not →
        // exactly 1 memory kept, cut starts at index 1. Pins greedy direction:
        // a reversed/unordered cut would fail this.
        let resp = recall(
            State(state.clone()),
            Json(RecallRequest {
                query: "测试".into(),
                top_k: Some(10),
                after: None,
                before: None,
                last_days: None,
                max_tokens: Some(8),
            }),
        )
        .await
        .unwrap()
        .0;
        assert!(resp.truncated);
        assert_eq!(resp.memories.len(), 1);
        assert_eq!(resp.memories[0]["layer"], "L3");
        let used: usize = resp
            .memories
            .iter()
            .filter_map(|m| m.get("content").and_then(|v| v.as_str()))
            .map(estimate_tokens)
            .sum();
        assert!(used <= 8);

        // Boundary semantics: budget == first item size keeps it (drop, never
        // truncate); budget below the first item yields an empty result.
        let resp = recall(
            State(state.clone()),
            Json(RecallRequest {
                query: "测试".into(),
                top_k: Some(10),
                after: None,
                before: None,
                last_days: None,
                max_tokens: Some(6),
            }),
        )
        .await
        .unwrap()
        .0;
        assert!(resp.truncated);
        assert_eq!(resp.memories.len(), 1); // persona fits exactly (6 tokens)
        let resp = recall(
            State(state.clone()),
            Json(RecallRequest {
                query: "测试".into(),
                top_k: Some(10),
                after: None,
                before: None,
                last_days: None,
                max_tokens: Some(5),
            }),
        )
        .await
        .unwrap()
        .0;
        assert!(resp.truncated);
        assert!(resp.memories.is_empty()); // first item already overflows

        // Tight budget (middle case): kept sum stays within budget
        let resp = recall(
            State(state.clone()),
            Json(RecallRequest {
                query: "测试".into(),
                top_k: Some(10),
                after: None,
                before: None,
                last_days: None,
                max_tokens: Some(15),
            }),
        )
        .await
        .unwrap()
        .0;
        assert!(resp.truncated);
        assert!(resp.memories.len() < 10);
        let used: usize = resp
            .memories
            .iter()
            .filter_map(|m| m.get("content").and_then(|v| v.as_str()))
            .map(estimate_tokens)
            .sum();
        assert!(used <= 15);
        // Layer order preserved (greedy prefix): kept memories are the first N
        let layers: Vec<&str> = resp
            .memories
            .iter()
            .filter_map(|m| m.get("layer").and_then(|v| v.as_str()))
            .collect();
        let mut sorted = layers.clone();
        sorted.sort_by_key(|l| match *l {
            "L3" => 0,
            "L2" => 1,
            "L1" => 2,
            _ => 3,
        });
        assert_eq!(layers, sorted);
        // context rebuilt from kept memories only; L0 turns excluded as before
        for m in &resp.memories {
            let content = m["content"].as_str().unwrap();
            if m["layer"] == "L0" {
                assert!(!resp.context.contains(content));
            } else {
                assert!(resp.context.contains(content));
            }
        }
    }

    /// Seed a recall fixture: 1 persona + 2 atoms (old/new created_at) +
    /// 2 turns (old/new timestamp_ms), all matching 测试.
    fn seed_time_filter_fixture(state: &crate::transport::state::AppState) {
        let d = state.db.lock().unwrap();
        let conn = d.conn();
        conn.execute(
            "INSERT INTO bounded_memory (target, content, created_at, updated_at, memory_type) \
             VALUES ('user', '用户画像测试', 500, 500, 'manual')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO bounded_memory (target, content, created_at, updated_at, memory_type) \
             VALUES ('memory', '旧记忆测试', 1000, 1000, 'atom')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO bounded_memory (target, content, created_at, updated_at, memory_type) \
             VALUES ('memory', '新记忆测试', 9000, 9000, 'atom')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions (session_id, start_ts, file_path, created_at, updated_at) \
             VALUES ('s1', 500, 'f.jsonl', 500, 500)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO turns (session_id, seq, timestamp_ms, role, preview) \
             VALUES ('s1', 1, 1500, 'user', '旧对话测试')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO turns (session_id, seq, timestamp_ms, role, preview) \
             VALUES ('s1', 2, 9500, 'user', '新对话测试')",
            [],
        )
        .unwrap();
    }

    #[tokio::test]
    async fn test_recall_time_filter_windows_layers() {
        use crate::config::Config;
        use crate::index::db::Db;
        use crate::transport::state::AppState;
        use axum::{extract::State, Json};

        let db = Db::open_memory().unwrap();
        db.init_schema().unwrap();
        let state = AppState::new(Config::default(), db, None, None);
        seed_time_filter_fixture(&state);

        // Window [5000, 10000]: keeps the new atom + new turn only; persona (L3)
        // is evergreen and stays; old atom/turn excluded.
        let resp = recall(
            State(state.clone()),
            Json(RecallRequest {
                query: "测试".into(),
                top_k: Some(10),
                max_tokens: None,
                after: Some("1970-01-01T00:00:05Z".into()), // 5000 ms
                before: Some("1970-01-01T00:00:10Z".into()), // 10000 ms
                last_days: None,
            }),
        )
        .await
        .unwrap()
        .0;
        // Cross-feature pin: time-filter reduction is NOT a budget truncation
        assert!(!resp.truncated);

        let l1: Vec<&str> = resp
            .memories
            .iter()
            .filter(|m| m["layer"] == "L1")
            .filter_map(|m| m["content"].as_str())
            .collect();
        assert_eq!(l1, vec!["新记忆测试"]);
        let l0: Vec<&str> = resp
            .memories
            .iter()
            .filter(|m| m["layer"] == "L0")
            .filter_map(|m| m["content"].as_str())
            .collect();
        assert_eq!(l0, vec!["新对话测试"]);
        // L3 persona unfiltered by design; L1 items carry the additive created_at key
        assert!(resp.memories.iter().any(|m| m["layer"] == "L3"));
        let new_atom = resp
            .memories
            .iter()
            .find(|m| m["layer"] == "L1")
            .unwrap();
        assert_eq!(new_atom["created_at"], 9000);

        // before-only combination (placeholder numbering must still line up)
        let resp = recall(
            State(state.clone()),
            Json(RecallRequest {
                query: "测试".into(),
                top_k: Some(10),
                max_tokens: None,
                after: None,
                before: Some("1970-01-01T00:00:02Z".into()), // 2000 ms
                last_days: None,
            }),
        )
        .await
        .unwrap()
        .0;
        let l1: Vec<&str> = resp
            .memories
            .iter()
            .filter(|m| m["layer"] == "L1")
            .filter_map(|m| m["content"].as_str())
            .collect();
        // persona row (created_at 500) + old atom (1000) fit; new atom (9000) cut
        assert_eq!(l1, vec!["旧记忆测试", "用户画像测试"]);

        // No filters → parity: all four matching rows returned (persona shows up
        // in L1 too — pre-existing no-target-filter behavior).
        let resp = recall(
            State(state.clone()),
            Json(RecallRequest {
                query: "测试".into(),
                top_k: Some(10),
                max_tokens: None,
                after: None,
                before: None,
                last_days: None,
            }),
        )
        .await
        .unwrap()
        .0;
        // 1 L3 persona + 3 L1 FTS matches (persona row + 2 atoms; L1 has no
        // target filter — pre-existing) + 2 L0 turns
        assert_eq!(resp.memories.len(), 6);
    }

    #[tokio::test]
    async fn test_recall_time_filter_last_days_overrides_after() {
        use crate::config::Config;
        use crate::index::db::Db;
        use crate::transport::state::AppState;
        use axum::{extract::State, Json};

        let db = Db::open_memory().unwrap();
        db.init_schema().unwrap();
        let state = AppState::new(Config::default(), db, None, None);
        seed_time_filter_fixture(&state);

        // last_days=7 → window starts now-7d; all fixture rows are 1970-epoch,
        // so L1/L0 all drop out. A stale `after` (epoch 0) must be OVERRIDDEN —
        // if it leaked through, old rows would reappear.
        let resp = recall(
            State(state.clone()),
            Json(RecallRequest {
                query: "测试".into(),
                top_k: Some(10),
                max_tokens: None,
                after: Some("1970-01-01T00:00:00Z".into()),
                before: None,
                last_days: Some(7),
            }),
        )
        .await
        .unwrap()
        .0;
        // Exactly the L3 persona remains; assert count so a regression that
        // also drops the persona can't pass the .all() vacuously.
        assert_eq!(resp.memories.len(), 1);
        assert!(
            resp.memories.iter().all(|m| m["layer"] == "L3"),
            "last_days must override after; got non-L3 rows: {:?}",
            resp.memories
        );
        // Negative / oversized values clamp the same way as /search
        let resp = recall(
            State(state.clone()),
            Json(RecallRequest {
                query: "测试".into(),
                top_k: Some(10),
                max_tokens: None,
                after: None,
                before: None,
                last_days: Some(-5), // clamps to 0 → window starts now → same result
            }),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(resp.memories.len(), 1); // vacuous-guard: persona only
        assert!(resp.memories.iter().all(|m| m["layer"] == "L3"));

        // Clamp is really pinned: seed a FUTURE-dated turn. Without the clamp,
        // last_days=-5 would start the window at now+5d and exclude it; with the
        // clamp to 0 (start=now) it is kept.
        let now = crate::util::time::now_unix_ms();
        {
            let d = state.db.lock().unwrap();
            d.conn()
                .execute(
                    "INSERT INTO turns (session_id, seq, timestamp_ms, role, preview) \
                     VALUES ('s1', 9, ?1, 'user', '未来对话测试')",
                    rusqlite::params![now + 86_400_000],
                )
                .unwrap();
        }
        let resp = recall(
            State(state.clone()),
            Json(RecallRequest {
                query: "测试".into(),
                top_k: Some(10),
                max_tokens: None,
                after: None,
                before: None,
                last_days: Some(-5),
            }),
        )
        .await
        .unwrap()
        .0;
        assert!(
            resp.memories
                .iter()
                .any(|m| m["layer"] == "L0" && m["content"] == "未来对话测试"),
            "clamped window (start=now) must keep the future-dated turn; unclamped \
             (start=now+5d) would drop it"
        );
    }

    #[tokio::test]
    async fn test_recall_time_filter_error_semantics() {
        use crate::config::Config;
        use crate::index::db::Db;
        use crate::transport::state::AppState;
        use axum::{extract::State, Json};

        let db = Db::open_memory().unwrap();
        db.init_schema().unwrap();
        let state = AppState::new(Config::default(), db, None, None);
        seed_time_filter_fixture(&state);

        // Malformed after → 400, never a silently widened window (/search parity)
        let err = recall(
            State(state.clone()),
            Json(RecallRequest {
                query: "测试".into(),
                top_k: Some(10),
                max_tokens: None,
                after: Some("yesterday".into()),
                before: None,
                last_days: None,
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(err.0, axum::http::StatusCode::BAD_REQUEST);
        assert!(err.1.0.error.contains("invalid after"));

        // Malformed before → same 400 contract
        let err = recall(
            State(state.clone()),
            Json(RecallRequest {
                query: "测试".into(),
                top_k: Some(10),
                max_tokens: None,
                after: None,
                before: Some("not-a-date".into()),
                last_days: None,
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(err.0, axum::http::StatusCode::BAD_REQUEST);
        assert!(err.1.0.error.contains("invalid before"));
    }
}

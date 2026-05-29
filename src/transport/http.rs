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
use tower_http::cors::{Any, CorsLayer};
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

/// Start the HTTP gateway server
pub async fn run_gateway(
    config: crate::config::Config,
    db: crate::index::db::Db,
    embedder: Option<crate::embedder::LazyEmbedder>,
    port: u16,
) -> anyhow::Result<()> {
    let state = AppState::new(config, db, embedder);

    // CORS configuration
    let cors = if state.config.gateway.cors_origins.is_empty() {
        // WARNING: Allowing any origin is insecure for production deployments
        if !state.config.gateway.auth_enabled {
            tracing::error!(
                "SECURITY WARNING: Gateway is running without authentication AND with CORS open to all origins. \
                 This is dangerous for production. Set AMS_GATEWAY_API_KEY and/or configure gateway.cors_origins."
            );
        } else {
            tracing::warn!("Gateway CORS configured to allow any origin. Set gateway.cors_origins for production.");
        }
        CorsLayer::new()
            .allow_origin(Any)
            .allow_methods(Any)
            .allow_headers(Any)
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
}

#[derive(Serialize)]
struct RecallResponse {
    memories: Vec<serde_json::Value>,
    context: String,
}

#[derive(Deserialize)]
struct SearchRequest {
    query: String,
    mode: Option<String>,
    top_k: Option<usize>,
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

#[derive(Serialize)]
struct ErrorResponse {
    error: String,
}

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

    let sessions: i64 = tx
        .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))
        .unwrap_or_else(|e| { tracing::warn!("stats sessions query failed: {}", e); 0 });

    let turns: i64 = tx
        .query_row("SELECT COUNT(*) FROM turns", [], |r| r.get(0))
        .unwrap_or_else(|e| { tracing::warn!("stats turns query failed: {}", e); 0 });

    let vectors: i64 = tx
        .query_row("SELECT COUNT(*) FROM vec_turns_rowids", [], |r| r.get(0))
        .unwrap_or_else(|e| { tracing::warn!("stats vectors query failed: {}", e); 0 });

    let entities: i64 = tx
        .query_row("SELECT COUNT(*) FROM entities", [], |r| r.get(0))
        .unwrap_or_else(|e| { tracing::warn!("stats entities query failed: {}", e); 0 });

    let relations: i64 = tx
        .query_row("SELECT COUNT(*) FROM relations", [], |r| r.get(0))
        .unwrap_or_else(|e| { tracing::warn!("stats relations query failed: {}", e); 0 });

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

async fn capture(
    State(state): State<AppState>,
    Json(req): Json<CaptureRequest>,
) -> Result<Json<CaptureResponse>, (StatusCode, Json<ErrorResponse>)> {
    // Validate input
    if req.session_id.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: "session_id is required".to_string(),
            }),
        ));
    }

    if req.turns.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: "turns array cannot be empty".to_string(),
            }),
        ));
    }

    // Validate turns structure
    for (i, turn) in req.turns.iter().enumerate() {
        if !turn.is_object() {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse {
                    error: format!("turn[{}] must be an object", i),
                }),
            ));
        }

        let obj = turn.as_object().unwrap();
        if !obj.contains_key("role") || !obj.contains_key("content") {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse {
                    error: format!("turn[{}] must have 'role' and 'content' fields", i),
                }),
            ));
        }
    }

    // Store turns in database with transaction
    let db = acquire_db(&state)?;

    let conn = db.conn();
    let tx = conn.unchecked_transaction().map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("begin transaction: {}", e),
            }),
        )
    })?;

    let now = chrono::Utc::now().timestamp_millis();

    // Determine first turn timestamp for session start_ts
    let first_ts = req.turns.iter()
        .filter_map(|t| t.as_object())
        .filter_map(|o| o.get("timestamp"))
        .filter_map(|v| v.as_i64())
        .next()
        .unwrap_or(now);

    // Use session_id as a virtual file_path (required NOT NULL column)
    let file_path = format!("gateway://{}", req.session_id);

    // Insert or update session (using correct schema columns)
    conn.execute(
        "INSERT INTO sessions (session_id, start_ts, file_path, turn_count, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?5)
         ON CONFLICT(session_id) DO UPDATE SET
           turn_count = sessions.turn_count + ?4,
           updated_at = ?5",
        params![req.session_id, first_ts, file_path, req.turns.len(), now],
    ).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("insert session: {}", e),
            }),
        )
    })?;

    let mut turns_saved = 0;

    // Get next seq number for this session
    let max_seq: i64 = conn.query_row(
        "SELECT COALESCE(MAX(seq), 0) FROM turns WHERE session_id = ?1",
        params![req.session_id],
        |r| r.get(0),
    ).unwrap_or(0);

    // Insert turns (using correct schema columns: seq, timestamp_ms, preview, char_count)
    for (i, turn) in req.turns.iter().enumerate() {
        let obj = turn.as_object().unwrap();
        let role = obj.get("role").and_then(|v| v.as_str()).unwrap_or("");
        let content = obj.get("content").and_then(|v| v.as_str()).unwrap_or("");
        let timestamp_ms = obj
            .get("timestamp")
            .and_then(|v| v.as_i64())
            .unwrap_or(now);

        // Truncate content for preview (matching conversation.rs behavior)
        let preview: String = content.chars().take(500).collect();
        let char_count = content.chars().count() as i64;
        let seq = max_seq + (i as i64) + 1;

        conn.execute(
            "INSERT INTO turns (session_id, seq, timestamp_ms, role, preview, char_count)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![req.session_id, seq, timestamp_ms, role, preview, char_count],
        ).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    error: format!("insert turn: {}", e),
                }),
            )
        })?;

        turns_saved += 1;
    }

    tx.commit().map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("commit transaction: {}", e),
            }),
        )
    })?;

    Ok(Json(CaptureResponse {
        status: "ok".to_string(),
        turns_saved,
    }))
}

async fn recall(
    State(state): State<AppState>,
    Json(req): Json<RecallRequest>,
) -> Result<Json<RecallResponse>, (StatusCode, Json<ErrorResponse>)> {
    // Validate input
    if req.query.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: "query cannot be empty".to_string(),
            }),
        ));
    }

    if req.query.len() > 10000 {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: "Query too long (max 10000 characters)".to_string(),
            }),
        ));
    }

    let top_k = req.top_k.unwrap_or(10).min(50); // Cap at 50

    let db = acquire_db(&state)?;

    // Progressive disclosure: L3 -> L2 -> L1 -> L0
    let mut memories = Vec::new();
    let mut context_parts = Vec::new();

    // L3: Persona (user profile)
    if let Ok(persona) = db.conn().query_row(
        "SELECT content FROM bounded_memory WHERE target = 'user' ORDER BY updated_at DESC LIMIT 1",
        [],
        |row| row.get::<_, String>(0),
    ) {
        memories.push(serde_json::json!({
            "layer": "L3",
            "type": "persona",
            "content": persona,
        }));
        context_parts.push(format!("[Persona] {}", persona));
    }

    // L2: Scenarios (recent scenarios)
    // Use COALESCE for memory_type as defense-in-depth for partial migrations
    match db.conn().prepare(
        "SELECT content FROM bounded_memory WHERE COALESCE(memory_type, 'manual') = 'scenario'
         ORDER BY updated_at DESC LIMIT ?1"
    ) {
        Ok(mut stmt) => {
            match stmt.query_map([top_k as i64], |row| {
                row.get::<_, String>(0)
            }) {
                Ok(scenarios) => {
                    for scenario in scenarios {
                        match scenario {
                            Ok(content) => {
                                memories.push(serde_json::json!({
                                    "layer": "L2",
                                    "type": "scenario",
                                    "content": content,
                                }));
                                context_parts.push(format!("[Scenario] {}", content));
                            }
                            Err(e) => tracing::warn!("recall L2 scenario row parse error: {}", e),
                        }
                    }
                }
                Err(e) => tracing::warn!("recall L2 scenario query error (skipping L2 layer): {}", e),
            }
        }
        Err(e) => tracing::warn!("recall L2 scenario prepare error (skipping L2 layer): {}", e),
    }

    // L1: Atoms (search by FTS)
    // Sanitize FTS5 query: wrap in double quotes to treat as literal phrase,
    // preventing FTS5 operator injection (NEAR, NOT, AND, OR, *, etc.)
    let fts_query = format!("\"{}\"", req.query.replace('"', "\"\""));
    let tokenized_fts = crate::util::text::tokenize_chinese(&fts_query);

    // Use COALESCE for confidence_score as defense-in-depth: if the column
    // is missing (partial migration), fall back to 1.0 instead of 500.
    match db.conn().prepare(
        "SELECT bm.content, COALESCE(bm.confidence_score, 1.0), COALESCE(bm.memory_type, 'manual')
         FROM bounded_memory bm
         JOIN bounded_memory_fts fts ON bm.id = fts.rowid
         WHERE bounded_memory_fts MATCH ?1
         ORDER BY COALESCE(bm.confidence_score, 1.0) DESC, bm.updated_at DESC
         LIMIT ?2"
    ) {
        Ok(mut stmt) => {
            let atoms = stmt.query_map(params![tokenized_fts, top_k as i64], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, f64>(1)?,
                    row.get::<_, String>(2)?,
                ))
            });

            match atoms {
                Ok(atoms) => {
                    for atom in atoms {
                        match atom {
                            Ok((content, confidence, memory_type)) => {
                                memories.push(serde_json::json!({
                                    "layer": "L1",
                                    "type": memory_type,
                                    "content": content,
                                    "confidence": confidence,
                                }));
                                context_parts.push(format!("[{}] {}", memory_type, content));
                            }
                            Err(e) => tracing::warn!("recall L1 atom row parse error: {}", e),
                        }
                    }
                }
                Err(e) => tracing::warn!("recall L1 FTS query error (skipping L1 layer): {}", e),
            }
        }
        Err(e) => tracing::warn!("recall L1 FTS prepare error (skipping L1 layer): {}", e),
    }

    // L0: Recent conversation turns
    // Use correct schema columns: preview (not content), timestamp_ms (not timestamp)
    // Escape LIKE wildcards to prevent user input from matching unintended rows
    let escaped_query = escape_like(&req.query);
    let search_pattern = format!("%{}%", escaped_query);
    let mut stmt = db.conn().prepare(
        "SELECT role, preview, timestamp_ms FROM turns
         WHERE preview LIKE ?1 ESCAPE '\\'
         ORDER BY timestamp_ms DESC LIMIT ?2"
    ).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("prepare turns query: {}", e),
            }),
        )
    })?;

    let turns = stmt.query_map(params![search_pattern, top_k as i64], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
        ))
    }).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("query turns: {}", e),
            }),
        )
    })?;

    for turn in turns {
        match turn {
            Ok((role, content, timestamp)) => {
                memories.push(serde_json::json!({
                    "layer": "L0",
                    "type": "turn",
                    "role": role,
                    "content": content,
                    "timestamp": timestamp,
                }));
            }
            Err(e) => tracing::warn!("recall L0 turn row parse error: {}", e),
        }
    }

    let context = context_parts.join("\n");

    Ok(Json(RecallResponse {
        memories,
        context,
    }))
}

async fn search(
    State(state): State<AppState>,
    Json(req): Json<SearchRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    // Input validation
    if req.entity.is_none() && req.query.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: "query cannot be empty for text search".to_string(),
            }),
        ));
    }

    if req.query.len() > 10000 {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: "Query too long (max 10000 characters)".to_string(),
            }),
        ));
    }

    // P8: Support multi-hop queries
    if let Some(entity) = req.entity {
        // Validate entity length
        if entity.len() > 1000 {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse {
                    error: "Entity name too long (max 1000 characters)".to_string(),
                }),
            ));
        }

        // Validate max_hops to prevent DoS
        let max_hops = req.max_hops.unwrap_or(2);
        if max_hops > 10 {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse {
                    error: "max_hops too large (max 10)".to_string(),
                }),
            ));
        }

        let db = acquire_db(&state)?;

        let relation_filter = req.relation_filter.as_deref();

        let atom_ids = crate::memory::graph_integration::multi_hop_query(
            &db,
            &entity,
            max_hops,
            relation_filter,
        )
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    error: format!("multi-hop query failed: {}", e),
                }),
            )
        })?;

        // Fetch atom details in a single batch query (avoid N+1)
        let atoms = if atom_ids.is_empty() {
            Vec::new()
        } else {
            // Build parameterized IN clause: "id IN (?1, ?2, ...)"
            let placeholders: Vec<String> = (1..=atom_ids.len())
                .map(|i| format!("?{}", i))
                .collect();
            let in_clause = placeholders.join(", ");
            let sql = format!(
                "SELECT id, content, COALESCE(memory_type, 'manual'), COALESCE(confidence_score, 1.0), created_at
                 FROM bounded_memory WHERE id IN ({}) ORDER BY COALESCE(confidence_score, 1.0) DESC",
                in_clause
            );
            let mut stmt = db.conn().prepare(&sql).map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ErrorResponse {
                        error: format!("prepare batch query: {}", e),
                    }),
                )
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
            })
            .map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ErrorResponse {
                        error: format!("batch query execution: {}", e),
                    }),
                )
            })?;

            rows.filter_map(|r| r.ok()).collect()
        };

        return Ok(Json(serde_json::json!({
            "results": atoms,
            "query_type": "multi_hop",
            "entity": entity,
            "max_hops": max_hops,
            "status": "ok"
        })));
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

    let params = crate::fact::search::SearchParams {
        query: req.query.clone(),
        search_mode,
        top_k,
        after_ms: None,
        before_ms: None,
        role: None,
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

    // Convert results to JSON
    let results_json: Vec<serde_json::Value> = results
        .iter()
        .map(|r| {
            serde_json::json!({
                "turn_id": r.turn_id,
                "score": r.score,
                "preview": r.preview,
                "session_id": r.session_id,
                "timestamp_ms": r.timestamp_ms,
                "role": r.role,
            })
        })
        .collect();

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
    let persona_path = config.memory_dir().join("persona.md");

    if persona_path.exists() {
        let content = std::fs::read_to_string(&persona_path).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    error: format!("read persona: {}", e),
                }),
            )
        })?;
        Ok(Json(serde_json::json!({
            "persona": content,
            "status": "ok"
        })))
    } else {
        Ok(Json(serde_json::json!({
            "persona": null,
            "status": "not_found"
        })))
    }
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
            rusqlite::params![canonical, kind, hops * 10],
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
        stmt.query_map(rusqlite::params![canonical, hops * 10], |row| {
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
    // Note: Async aggregation (L1 extraction, L2 scenario aggregation) is not yet
    // implemented. The session end timestamp is recorded for future pipeline use.
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

    Ok(Json(serde_json::json!({
        "status": "ok",
        "session_id": session_id,
        "end_ts": now,
        "message": "Session end timestamp recorded. Async aggregation pipeline not yet implemented.",
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

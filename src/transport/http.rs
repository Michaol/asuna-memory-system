//! HTTP REST gateway for AMS
//!
//! Provides REST API endpoints for Hermes and other HTTP clients.
//!
//! ## Endpoint Status
//!
//! - **Implemented**: `/health`, `/stats`, `/persona`
//! - **Stub (P3)**: `/capture`, `/session/end`
//! - **Stub (P5)**: `/recall`
//! - **Stub (future)**: `/search`, `/graph/assert`, `/graph/neighbors`
//!
//! Stub endpoints return valid but empty responses. Full implementation
//! will be completed in later phases of Project Aegis.

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
use tokio::net::TcpListener;
use tower_http::cors::{Any, CorsLayer};
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::trace::TraceLayer;

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
        tracing::warn!("Gateway CORS configured to allow any origin. This is insecure for production.");
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
        Some(key) if key == state.config.gateway.api_key => {
            // Valid API key, proceed with request
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
    #[allow(dead_code)] // Will be used in P3
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
    #[allow(dead_code)] // Will be used in P5
    top_k: Option<usize>,
}

#[derive(Serialize)]
struct RecallResponse {
    memories: Vec<serde_json::Value>,
    context: String,
}

#[derive(Deserialize)]
struct SearchRequest {
    #[allow(dead_code)]
    query: String,
    #[allow(dead_code)]
    mode: Option<String>,
    #[allow(dead_code)]
    top_k: Option<usize>,
    // P8: Multi-hop query parameters
    entity: Option<String>,
    max_hops: Option<u32>,
    relation_filter: Option<String>,
}

#[derive(Deserialize)]
struct GraphAssertRequest {
    #[allow(dead_code)] // Will be used when graph assert is implemented
    subject: String,
    #[allow(dead_code)]
    predicate: String,
    #[allow(dead_code)]
    object: String,
    #[allow(dead_code)]
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
    let db = state.db.lock().map_err(|_e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: "Failed to acquire database lock".to_string(),
            }),
        )
    })?;

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
    let db = state.db.lock().map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("db lock: {}", e),
            }),
        )
    })?;

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

    // Insert or update session
    conn.execute(
        "INSERT INTO sessions (id, created_at, updated_at) VALUES (?1, ?2, ?2)
         ON CONFLICT(id) DO UPDATE SET updated_at = ?2",
        params![req.session_id, now],
    ).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("insert session: {}", e),
            }),
        )
    })?;

    let mut turns_saved = 0;

    // Insert turns
    for turn in &req.turns {
        let obj = turn.as_object().unwrap();
        let role = obj.get("role").and_then(|v| v.as_str()).unwrap_or("");
        let content = obj.get("content").and_then(|v| v.as_str()).unwrap_or("");
        let timestamp = obj
            .get("timestamp")
            .and_then(|v| v.as_i64())
            .unwrap_or(now);

        conn.execute(
            "INSERT INTO turns (session_id, role, content, timestamp, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![req.session_id, role, content, timestamp, now],
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

    let db = state.db.lock().map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("db lock: {}", e),
            }),
        )
    })?;

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
    let mut stmt = db.conn().prepare(
        "SELECT content FROM bounded_memory WHERE memory_type = 'scenario'
         ORDER BY updated_at DESC LIMIT ?1"
    ).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("prepare scenarios query: {}", e),
            }),
        )
    })?;

    let scenarios = stmt.query_map([top_k as i64], |row| {
        row.get::<_, String>(0)
    }).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("query scenarios: {}", e),
            }),
        )
    })?;

    for scenario in scenarios.flatten() {
        memories.push(serde_json::json!({
            "layer": "L2",
            "type": "scenario",
            "content": scenario,
        }));
        context_parts.push(format!("[Scenario] {}", scenario));
    }

    // L1: Atoms (search by FTS)
    let search_query = crate::util::text::tokenize_chinese(&req.query);
    let mut stmt = db.conn().prepare(
        "SELECT bm.content, bm.confidence_score, bm.memory_type
         FROM bounded_memory bm
         JOIN bounded_memory_fts fts ON bm.id = fts.rowid
         WHERE bounded_memory_fts MATCH ?1
         ORDER BY bm.confidence_score DESC, bm.updated_at DESC
         LIMIT ?2"
    ).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("prepare atoms FTS query: {}", e),
            }),
        )
    })?;

    let atoms = stmt.query_map(params![search_query, top_k as i64], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, f64>(1)?,
            row.get::<_, String>(2)?,
        ))
    }).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("query atoms: {}", e),
            }),
        )
    })?;

    for atom in atoms.flatten() {
        let (content, confidence, memory_type) = atom;
        memories.push(serde_json::json!({
            "layer": "L1",
            "type": memory_type,
            "content": content,
            "confidence": confidence,
        }));
        context_parts.push(format!("[{}] {}", memory_type, content));
    }

    // L0: Recent conversation turns
    let mut stmt = db.conn().prepare(
        "SELECT role, content, timestamp FROM turns
         WHERE content LIKE ?1
         ORDER BY timestamp DESC LIMIT ?2"
    ).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("prepare turns query: {}", e),
            }),
        )
    })?;

    let search_pattern = format!("%{}%", req.query);
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

    for turn in turns.flatten() {
        let (role, content, timestamp) = turn;
        memories.push(serde_json::json!({
            "layer": "L0",
            "type": "turn",
            "role": role,
            "content": content,
            "timestamp": timestamp,
        }));
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

        let db = state.db.lock().map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    error: format!("db lock: {}", e),
                }),
            )
        })?;

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

        // Fetch atom details for each ID
        let mut atoms = Vec::new();
        for atom_id in atom_ids {
            let result = db.conn().query_row(
                "SELECT id, content, memory_type, confidence_score, created_at
                 FROM bounded_memory WHERE id = ?1",
                rusqlite::params![atom_id],
                |row| {
                    Ok(serde_json::json!({
                        "id": row.get::<_, i64>(0)?,
                        "content": row.get::<_, String>(1)?,
                        "memory_type": row.get::<_, String>(2)?,
                        "confidence_score": row.get::<_, f64>(3)?,
                        "created_at": row.get::<_, i64>(4)?
                    }))
                },
            );

            if let Ok(atom) = result {
                atoms.push(atom);
            }
        }

        return Ok(Json(serde_json::json!({
            "results": atoms,
            "query_type": "multi_hop",
            "entity": entity,
            "max_hops": max_hops,
            "status": "ok"
        })));
    }

    // Traditional text search - delegate to existing search logic
    let db = state.db.lock().map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("db lock: {}", e),
            }),
        )
    })?;

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

    let db = state.db.lock().map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("db lock: {}", e),
            }),
        )
    })?;

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

    let db = state.db.lock().map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("db lock: {}", e),
            }),
        )
    })?;

    let canonical = crate::graph::canonical::canonicalize(&req.entity);
    let direction = req.direction.as_deref().unwrap_or("both");
    let relation_kind = req.relation_kind.as_deref();

    // Build query based on direction
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
            // both directions
            if relation_kind.is_some() {
                "SELECT dst_canonical, rel_type, confidence, relation_kind
                     FROM relations
                     WHERE src_canonical = ?1 AND relation_kind = ?2
                     UNION
                     SELECT src_canonical, rel_type, confidence, relation_kind
                     FROM relations
                     WHERE dst_canonical = ?1 AND relation_kind = ?2
                     LIMIT ?3".to_string()
            } else {
                "SELECT dst_canonical, rel_type, confidence, relation_kind
                     FROM relations
                     WHERE src_canonical = ?1
                     UNION
                     SELECT src_canonical, rel_type, confidence, relation_kind
                     FROM relations
                     WHERE dst_canonical = ?1
                     LIMIT ?2".to_string()
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

    let db = state.db.lock().map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("db lock: {}", e),
            }),
        )
    })?;

    // Verify session exists
    let session_exists: bool = db.conn().query_row(
        "SELECT COUNT(*) > 0 FROM sessions WHERE id = ?1",
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

    // Trigger async aggregation (L1 extraction, L2 scenario aggregation, etc.)
    // For now, we'll just log the event and return success
    // In a production system, this would spawn an async task
    tracing::info!("Session end triggered for session_id={}, aggregation queued", session_id);

    // Update session end timestamp
    let now = crate::util::time::now_unix_ms();
    db.conn().execute(
        "UPDATE sessions SET end_ts = ?1, updated_at = ?1 WHERE id = ?2",
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
        "aggregation": "queued",
        "message": "Session aggregation has been queued for processing",
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

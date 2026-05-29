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
    extract::{Path, State},
    http::StatusCode,
    response::Json,
    routing::{get, post},
    Router,
};
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tower_http::cors::{Any, CorsLayer};

/// Start the HTTP gateway server
pub async fn run_gateway(
    config: crate::config::Config,
    db: crate::index::db::Db,
    embedder: Option<crate::embedder::LazyEmbedder>,
    port: u16,
) -> anyhow::Result<()> {
    let state = AppState::new(config, db, embedder);

    // CORS for local-only deployment (gateway binds to 127.0.0.1)
    // WARNING: If gateway is exposed to network in the future, restrict CORS origins
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    let app = Router::new()
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
        .route("/session/end", post(session_end))
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
    #[allow(dead_code)] // Will be used when search is implemented
    query: String,
    #[allow(dead_code)]
    mode: Option<String>,
    #[allow(dead_code)]
    top_k: Option<usize>,
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
    #[allow(dead_code)] // Will be used when graph neighbors is implemented
    entity: String,
    #[allow(dead_code)]
    hops: Option<usize>,
    #[allow(dead_code)]
    direction: Option<String>,
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
    // NOTE: Holds Mutex<Db> lock for 5 queries. Acceptable for single-user local deployment.
    // If multi-user concurrency is needed, consider batching queries or using try_lock.
    let db = state.db.lock().map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("db lock: {}", e),
            }),
        )
    })?;

    let sessions: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))
        .unwrap_or_else(|e| { tracing::warn!("stats sessions query failed: {}", e); 0 });

    let turns: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM turns", [], |r| r.get(0))
        .unwrap_or_else(|e| { tracing::warn!("stats turns query failed: {}", e); 0 });

    let vectors: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM vec_turns_rowids", [], |r| r.get(0))
        .unwrap_or_else(|e| { tracing::warn!("stats vectors query failed: {}", e); 0 });

    let entities: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM entities", [], |r| r.get(0))
        .unwrap_or_else(|e| { tracing::warn!("stats entities query failed: {}", e); 0 });

    let relations: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM relations", [], |r| r.get(0))
        .unwrap_or_else(|e| { tracing::warn!("stats relations query failed: {}", e); 0 });

    Ok(Json(StatsResponse {
        sessions,
        turns,
        vectors,
        entities,
        relations,
    }))
}

async fn capture(
    State(_state): State<AppState>,
    Json(req): Json<CaptureRequest>,
) -> Result<Json<CaptureResponse>, (StatusCode, Json<ErrorResponse>)> {
    // TODO: Implement capture logic (will be done in P3)
    // For now, just acknowledge the request
    Ok(Json(CaptureResponse {
        status: "ok".to_string(),
        turns_saved: req.turns.len(),
    }))
}

async fn recall(
    State(_state): State<AppState>,
    Json(req): Json<RecallRequest>,
) -> Result<Json<RecallResponse>, (StatusCode, Json<ErrorResponse>)> {
    // TODO: Implement recall with progressive disclosure (will be done in P5)
    // For now, return empty results
    Ok(Json(RecallResponse {
        memories: vec![],
        context: format!("Query: {}", req.query),
    }))
}

async fn search(
    State(_state): State<AppState>,
    Json(_req): Json<SearchRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    // TODO: Implement search (will delegate to existing search logic)
    Ok(Json(serde_json::json!({
        "results": [],
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
    State(_state): State<AppState>,
    Json(_req): Json<GraphAssertRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    // TODO: Implement graph assert (will delegate to existing graph logic)
    Ok(Json(serde_json::json!({
        "status": "ok"
    })))
}

async fn graph_neighbors(
    State(_state): State<AppState>,
    Json(_req): Json<GraphNeighborsRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    // TODO: Implement graph neighbors (will delegate to existing graph logic)
    Ok(Json(serde_json::json!({
        "neighbors": [],
        "status": "ok"
    })))
}

async fn session_end(
    State(_state): State<AppState>,
    Json(_req): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    // TODO: Implement session end (will trigger async aggregation in P3)
    Ok(Json(serde_json::json!({
        "status": "ok"
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

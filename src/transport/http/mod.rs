//! HTTP REST gateway for AMS.
//!
//! Endpoints:
//! `/health`, `/stats`, `/capture`, `/recall`, `/recall/{*node_id}`, `/search`,
//! `/persona`, `/offload`, `/graph/assert`, `/graph/neighbors`, `/session/end`.

mod backfill;
mod capture;
mod error;
mod middleware;
mod models;
mod ops_handlers;
mod recall;
mod search;

use backfill::spawn_startup_vec_backfill;
use capture::capture as capture_route;
#[cfg(test)]
use capture::{capture, parse_turn_timestamp, validate_capture_request, MAX_TURNS_PER_BATCH};
use error::ApiError;
#[cfg(test)]
use middleware::is_localhost_origin;
use middleware::{auth_middleware, build_cors_layer, json_error_body, panic_to_response};
#[cfg(test)]
use models::session_id_error;
#[cfg(test)]
use models::{
    CaptureRequest, ConfidenceInput, GraphAssertRequest, JsonBytes, RecallRequest, SearchRequest,
};
use ops_handlers::{
    graph_assert as graph_assert_route, graph_neighbors as graph_neighbors_route, health, offload,
    persona as persona_route, recall_by_node as recall_by_node_route,
    session_end as session_end_route, stats,
};
#[cfg(test)]
use ops_handlers::{
    graph_assert, graph_neighbors, persona, recall_by_node, session_end, try_spawn_pipeline,
};
#[cfg(test)]
use recall::recall;
use recall::recall as recall_route;
use search::search as search_route;
#[cfg(test)]
use search::{batch_fetch_atoms, search};

use crate::transport::state::AppState;
use axum::{
    extract::DefaultBodyLimit,
    middleware as axum_middleware,
    routing::{get, post},
    Router,
};
use std::sync::MutexGuard;
use tokio::net::TcpListener;
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::TraceLayer;

const REQUEST_TIMEOUT_SECS: u64 = 60;

/// Shared request body cap: the axum extractor limit (DefaultBodyLimit) and
/// the tower-http streaming limit (RequestBodyLimitLayer) must stay in sync.
const MAX_REQUEST_BODY_BYTES: usize = 10 * 1024 * 1024;

/// Acquire the database lock with poison recovery and automatic uncommitted
/// transaction rollback.
fn acquire_db(state: &AppState) -> Result<MutexGuard<'_, crate::index::db::Db>, ApiError> {
    let guard = crate::util::recover_poison(&state.db);
    guard.rollback_if_in_tx();
    Ok(guard)
}

fn ensure_graph_enabled(state: &AppState) -> Result<(), ApiError> {
    if !state.config.graph.enabled {
        return Err(ApiError::forbidden("graph disabled in config"));
    }
    Ok(())
}

async fn run_blocking<F, T>(op_name: &'static str, f: F) -> Result<T, ApiError>
where
    F: FnOnce() -> Result<T, ApiError> + Send + 'static,
    T: Send + 'static,
{
    match tokio::task::spawn_blocking(f).await {
        Ok(res) => res,
        Err(e) => {
            tracing::error!("{} blocking task failed: {}", op_name, e);
            Err(ApiError::internal())
        }
    }
}

/// Shared content-type gate for JSON write endpoints (R1): CSRF posture
/// requires application/json; charset parameters are stripped and the
/// comparison is case-insensitive.
fn require_json(headers: &axum::http::HeaderMap) -> Result<(), ApiError> {
    let content_type = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let mime = content_type.split(';').next().unwrap_or("").trim();
    if !mime.eq_ignore_ascii_case("application/json") {
        return Err(ApiError::unsupported_media_type(
            "expected Content-Type: application/json",
        ));
    }
    Ok(())
}

async fn embed_search_query(
    state: &AppState,
    needs_vec: bool,
    is_semantic: bool,
    query: String,
) -> Result<Option<Vec<f32>>, ApiError> {
    let (true, Some(emb)) = (needs_vec, state.embedder.clone()) else {
        return Ok(None);
    };
    match tokio::task::spawn_blocking(move || emb.embed_query(&query)).await {
        Ok(Ok(v)) => Ok(Some(v)),
        Ok(Err(e)) => {
            if is_semantic {
                // C3: the raw embedder error carries the internal endpoint
                // URL — log it server-side, never echo it into the 500 body.
                tracing::error!(
                    target: "asuna_memory::transport::http",
                    "semantic search embedding failed: {}",
                    e
                );
                return Err(ApiError::internal());
            }
            tracing::warn!(
                target: "asuna_memory::transport::http",
                "hybrid: 语义搜索失败，降级为仅关键词: {}",
                e
            );
            Ok(None)
        }
        Err(e) => {
            tracing::error!(
                target: "asuna_memory::transport::http",
                "search embed task failed: {}",
                e
            );
            Err(ApiError::internal())
        }
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(e) = tokio::signal::ctrl_c().await {
            tracing::error!("failed to install Ctrl+C handler: {}", e);
            std::future::pending::<()>().await;
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(e) => {
                tracing::error!("failed to install SIGTERM handler: {}", e);
                std::future::pending::<()>().await;
            }
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    tracing::info!("Shutdown signal received, draining gateway...");
}

fn build_router(state: &AppState) -> Router<AppState> {
    let mut app = Router::new()
        .route("/health", get(health))
        .route("/stats", get(stats))
        .route("/capture", post(capture_route))
        .route("/recall", post(recall_route))
        .route("/recall/{*node_id}", get(recall_by_node_route))
        .route("/search", post(search_route))
        .route("/persona", get(persona_route))
        .route("/offload", post(offload))
        .route("/graph/assert", post(graph_assert_route))
        .route("/graph/neighbors", post(graph_neighbors_route))
        .route("/session/end", post(session_end_route));

    if state.config.gateway.auth_enabled {
        tracing::info!("Gateway authentication enabled");
        app = app.layer(axum_middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ));
    }
    app
}

/// Re-check the *actual* bound address: a `localhost` bind_host can resolve
/// to a non-loopback address (hosts-file surprise) that validate_gateway_bind's
/// literal check cannot see.
fn ensure_actual_bind_allowed(
    addr: std::net::SocketAddr,
    auth_enabled: bool,
    api_key: &str,
) -> anyhow::Result<()> {
    if !addr.ip().is_loopback() && !(auth_enabled && !api_key.trim().is_empty()) {
        anyhow::bail!(
            "refusing to expose non-loopback address {} without authentication",
            addr
        );
    }
    Ok(())
}

/// Start the HTTP gateway server.
pub async fn run_gateway(
    mut config: crate::config::Config,
    db: crate::index::db::Db,
    embedder: Option<crate::embedder::LazyEmbedder>,
    llm: Option<crate::memory::llm::LlmClient>,
    port: u16,
) -> anyhow::Result<()> {
    config.gateway.api_key = config.gateway.api_key.trim().to_string();
    crate::config::validate_gateway_bind(
        &config.gateway.bind_host,
        config.gateway.auth_enabled,
        &config.gateway.api_key,
    )?;
    let bind_addr = crate::config::gateway_bind_addr(&config.gateway.bind_host, port);

    if config.gateway.auth_enabled && config.gateway.api_key.is_empty() {
        anyhow::bail!("Gateway auth is enabled but no API key is configured. Set AMS_GATEWAY_API_KEY environment variable.");
    }

    let state = AppState::new(config, db, embedder, llm);
    let cors = build_cors_layer(&state.config.gateway);

    let app = build_router(&state)
        .layer(DefaultBodyLimit::max(MAX_REQUEST_BODY_BYTES))
        .layer(TimeoutLayer::with_status_code(
            axum::http::StatusCode::REQUEST_TIMEOUT,
            std::time::Duration::from_secs(REQUEST_TIMEOUT_SECS),
        ))
        .layer(RequestBodyLimitLayer::new(MAX_REQUEST_BODY_BYTES))
        // Trace outside the limit/timeout layers so 413/408 rejections are
        // traced too (they never reach an inner TraceLayer).
        .layer(TraceLayer::new_for_http())
        // W1.2/W21: 把 tower 层与提取器的非 JSON 错误体统一改写进 JSON 错误契约。
        // 必须在 Timeout/RequestBodyLimit **外层**才能看到它们的响应；cors 与
        // CatchPanic 在其外（见下方 M9），故本层不是最外层。
        .layer(axum_middleware::from_fn(json_error_body))
        // M9: CatchPanic 必须在 cors **内层**——否则 handler panic 的 500 在最外层
        // 生成，CORS 层已解绕，浏览器侧只看到不透明 CORS 失败，读不到 JSON 错误体。
        .layer(CatchPanicLayer::custom(panic_to_response))
        .layer(cors)
        .with_state(state.clone());

    let listener = TcpListener::bind(&bind_addr).await?;
    let actual_addr = listener.local_addr()?;
    ensure_actual_bind_allowed(
        actual_addr,
        state.config.gateway.auth_enabled,
        &state.config.gateway.api_key,
    )?;

    tracing::info!("AMS Gateway listening on http://{}", actual_addr);
    println!("AMS Gateway listening on http://{}", actual_addr);

    // W28b: raise the flag as soon as the signal arrives (before draining
    // finishes) so the background backfill stops instead of holding up
    // runtime drop until the container's grace period expires.
    let shutdown_flag = state.shutdown.clone();
    let pipeline_semaphore = state.pipeline_semaphore.clone();
    let backfill_task = spawn_startup_vec_backfill(state);

    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            shutdown_flag.store(true, std::sync::atomic::Ordering::SeqCst);
        })
        .await?;

    if let Some(task) = backfill_task {
        if let Err(e) = task.await {
            tracing::error!("vec_bounded_memory backfill join failed: {}", e);
        }
    }

    // W20: /session/end 已回 "spawned" 的 pipeline 是 detached task——serve 返回后
    // runtime 会被 shutdown_timeout 丢弃，该会话记忆永久丢失且无重触发路径。
    // 这里等在途 pipeline 交还全部许可，让承诺过的工作真正完成。
    //
    // **必须在 serve 返回之后**等，不能放进 signal future：axum 官方语义是
    // "graceful shutdown when the provided future completes"——signal future 未完成
    // 前服务器不会开始关闭、仍在接受新请求，等待反而推迟排空并撑爆容器 grace 期。
    // 预算 25s + main.rs 的 shutdown_timeout(10s) ≈ 35s，需容器 grace ≥ 40s
    // （compose 已设 stop_grace_period: 40s；docker run 需 --stop-timeout 40）。
    let drained = tokio::time::timeout(
        std::time::Duration::from_secs(25),
        pipeline_semaphore
            .acquire_many_owned(crate::transport::state::MAX_CONCURRENT_PIPELINES as u32),
    )
    .await;
    if matches!(drained, Ok(Ok(_))) {
        tracing::info!("graceful shutdown: 在途 post-session pipeline 已全部完成");
    } else {
        tracing::warn!(
            "graceful shutdown: 等待在途 pipeline 超时（25s），未完成的工作将被放弃；\
             受影响会话需重新 POST /session/end"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests;

use super::error::ApiError;
use super::models::{
    exceeds_char_limit, parse_confidence, session_id_error, validate_entity_length,
    validate_relation_length, GraphAssertRequest, HealthResponse, JsonBytes, OffloadRequest,
    OffloadResponse, RecallNodeResponse, StatsResponse, MAX_ENTITY_CHARS,
};
use super::{acquire_db, ensure_graph_enabled, run_blocking};
use crate::transport::state::AppState;
use axum::{
    extract::{Path, State},
    response::Json,
};
use rusqlite::params;
use std::sync::Mutex;

/// Mirrors the private `graph::query::neighbors::MAX_LIMIT` clamp (200): the REST
/// gate rejects out-of-range limits with 400 like `hops`, instead of silently
/// clamping (0 used to become 1).
const MAX_NEIGHBOR_LIMIT: u32 = 200;

async fn read_persona_file(
    path: &std::path::Path,
    source: &'static str,
) -> Option<serde_json::Value> {
    // W3.2: read directly (no try_exists+read TOCTOU) and distinguish
    // NotFound (normal fallback) from real IO faults (warn, then fall back)
    // so permission/disk failures stay observable instead of silently
    // masquerading as "file absent".
    let content = match tokio::fs::read_to_string(path).await {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            tracing::warn!(
                "persona file {} unreadable ({}); falling back",
                path.display(),
                e
            );
            return None;
        }
    };
    let body = crate::growth::bounded_memory::extract_body(&content);
    if body.is_empty() {
        return None;
    }
    Some(serde_json::json!({
        "persona": content,
        "status": "ok",
        "source": source
    }))
}

fn persona_db_fallback(state: &AppState) -> Result<Option<String>, ApiError> {
    let db = acquire_db(state)?;
    let mut stmt = db
        .conn()
        .prepare(
            "SELECT content FROM bounded_memory WHERE target = 'user' \
             AND NOT EXISTS (SELECT 1 FROM bounded_memory s WHERE s.supersedes_id = bounded_memory.id) \
             ORDER BY updated_at DESC",
        )
        .map_err(|e| {
            tracing::error!("persona fallback prepare failed: {}", e);
            ApiError::internal()
        })?;
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|e| {
            tracing::error!("persona fallback query failed: {}", e);
            ApiError::internal()
        })?;
    for row in rows {
        let content = row.map_err(|e| {
            tracing::error!("persona fallback row read failed: {}", e);
            ApiError::internal()
        })?;
        if !content.trim().is_empty() {
            return Ok(Some(content));
        }
    }
    Ok(None)
}

/// Priority chain: USER.md -> persona.md -> bounded_memory.
pub(super) async fn persona(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let memory_dir = state.config.memory_dir();

    if let Some(resp) = read_persona_file(&memory_dir.join("USER.md"), "USER.md").await {
        return Ok(Json(resp));
    }
    if let Some(resp) = read_persona_file(&memory_dir.join("persona.md"), "persona.md").await {
        return Ok(Json(resp));
    }

    if let Some(content) =
        run_blocking("persona_fallback", move || persona_db_fallback(&state)).await?
    {
        return Ok(Json(serde_json::json!({
            "persona": content,
            "status": "ok",
            "source": "bounded_memory"
        })));
    }

    Ok(Json(serde_json::json!({
        "persona": null,
        "status": "not_found"
    })))
}

fn validate_graph_assert_fields(req: &GraphAssertRequest) -> Result<f64, ApiError> {
    if req.subject.trim().is_empty() {
        return Err(ApiError::bad_request("subject cannot be empty"));
    }
    if req.predicate.trim().is_empty() {
        return Err(ApiError::bad_request("predicate cannot be empty"));
    }
    if req.object.trim().is_empty() {
        return Err(ApiError::bad_request("object cannot be empty"));
    }
    if exceeds_char_limit(&req.subject, MAX_ENTITY_CHARS)
        || exceeds_char_limit(&req.predicate, MAX_ENTITY_CHARS)
        || exceeds_char_limit(&req.object, MAX_ENTITY_CHARS)
    {
        return Err(ApiError::bad_request(format!(
            "subject, predicate, and object must be <= {} characters",
            MAX_ENTITY_CHARS
        )));
    }
    if let Err(reason) = crate::growth::security::scan_fields(&[
        ("subject", req.subject.as_str()),
        ("predicate", req.predicate.as_str()),
        ("object", req.object.as_str()),
    ]) {
        return Err(ApiError::bad_request(reason));
    }
    parse_confidence(req.confidence.as_ref())
}

pub(super) async fn graph_assert(
    State(state): State<AppState>,
    JsonBytes(body): JsonBytes,
) -> Result<Json<serde_json::Value>, ApiError> {
    ensure_graph_enabled(&state)?;

    run_blocking("graph_assert", move || {
        let req: GraphAssertRequest = serde_json::from_slice(&body)
            .map_err(|e| ApiError::bad_request(format!("invalid JSON body: {}", e)))?;
        let confidence = validate_graph_assert_fields(&req)?;

        let predicate = req.predicate.trim().to_string();
        let triple = crate::graph::TripleInput {
            src: req.subject,
            rel: req.predicate,
            dst: req.object,
            src_type: None,
            dst_type: None,
            confidence: Some(confidence),
            source_turn: None,
        };

        let db = acquire_db(&state)?;
        let subject_canonical =
            crate::graph::query::resolve_entity_canonical(db.conn(), &triple.src).map_err(|e| {
                tracing::error!("graph_assert subject resolution failed: {}", e);
                ApiError::internal()
            })?;
        let object_canonical =
            crate::graph::query::resolve_entity_canonical(db.conn(), &triple.dst).map_err(|e| {
                tracing::error!("graph_assert object resolution failed: {}", e);
                ApiError::internal()
            })?;
        crate::graph::assert_triples(&db, &[triple]).map_err(|e| {
            tracing::error!("graph_assert failed: {}", e);
            ApiError::internal()
        })?;

        Ok(Json(serde_json::json!({
            "status": "ok",
            "subject": subject_canonical,
            "predicate": predicate,
            "object": object_canonical,
            "confidence": confidence,
        })))
    })
    .await
}

pub(super) async fn graph_neighbors(
    State(state): State<AppState>,
    JsonBytes(body): JsonBytes,
) -> Result<Json<serde_json::Value>, ApiError> {
    ensure_graph_enabled(&state)?;

    run_blocking("graph_neighbors", move || {
        let q: crate::graph::NeighborQuery = serde_json::from_slice(&body)
            .map_err(|e| ApiError::bad_request(format!("invalid JSON body: {}", e)))?;
        // 空/纯空白 entity 与 /search multi-hop 同契约（400），长度校验共用。
        if q.entity.trim().is_empty() {
            return Err(ApiError::bad_request("entity cannot be empty"));
        }
        validate_entity_length(&q.entity)?;
        if let Some(ref relation) = q.rel_type {
            validate_relation_length("rel_type", relation)?;
        }
        if !(1..=MAX_NEIGHBOR_LIMIT).contains(&q.limit) {
            return Err(ApiError::bad_request(format!(
                "limit must be in 1..={}, got {}",
                MAX_NEIGHBOR_LIMIT, q.limit
            )));
        }
        if !(1..=crate::graph::query::MAX_HOPS).contains(&q.hops) {
            return Err(ApiError::bad_request(format!(
                "hops must be in 1..={}, got {}",
                crate::graph::query::MAX_HOPS,
                q.hops
            )));
        }

        let entity = q.entity.clone();
        let db = acquire_db(&state)?;
        // H2: canonical 回显必须用**实际解析到的**节点键，而不是再 canonicalize
        // 一次——session 键是无损的，二次折叠会回显一个可能不存在的节点名。
        let canonical = crate::graph::query::resolve_entity_canonical(db.conn(), &q.entity)
            .map_err(|e| {
                tracing::error!("graph_neighbors entity resolution failed: {}", e);
                ApiError::internal()
            })?;
        // W17: 节点帽命中时结果是部分集，必须透出 truncated（口径同 /search 多跳）。
        let (neighbors, truncated) = crate::graph::query::neighbors_with_truncation(&db, &q)
            .map_err(|e| {
                tracing::error!("graph_neighbors failed: {}", e);
                ApiError::internal()
            })?;

        let count = neighbors.len();
        Ok(Json(serde_json::json!({
            "entity": entity,
            "canonical": canonical,
            "neighbors": neighbors,
            "count": count,
            "truncated": truncated,
            "status": "ok"
        })))
    })
    .await
}

struct SessionFlightGuard {
    active: std::sync::Arc<Mutex<std::collections::HashSet<String>>>,
    session_id: String,
}

impl Drop for SessionFlightGuard {
    fn drop(&mut self) {
        let mut active = crate::util::recover_poison(&self.active);
        active.remove(&self.session_id);
    }
}

fn session_end_db_update(state: &AppState, session_id: &str, now: i64) -> Result<i64, ApiError> {
    let db = acquire_db(state)?;
    let session_exists: bool = db
        .conn()
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sessions WHERE session_id = ?1)",
            params![session_id],
            |row| row.get(0),
        )
        .map_err(|e| {
            tracing::error!("session_end existence check failed: {}", e);
            ApiError::internal()
        })?;
    if !session_exists {
        return Err(ApiError::not_found(format!(
            "session not found: {}",
            session_id
        )));
    }
    tracing::info!("Session end recorded for session_id={}", session_id);
    let stored_end_ts: i64 = db
        .conn()
        .query_row(
            // M10: 重复/重试调用不得把已记录的 end_ts 前移；updated_at 仍刷新。
            // M4: RETURNING 取**实际落库值**——响应回显 now 会与库中值分歧
            // （重试场景，以及 Overwrite 已写入 turn 派生 end_ts 的场景）。
            "UPDATE sessions SET end_ts = COALESCE(end_ts, ?1), updated_at = ?1
             WHERE session_id = ?2 RETURNING end_ts",
            params![now, session_id],
            |r| r.get(0),
        )
        .map_err(|e| {
            // RETURNING 只返回被直接修改的行；WHERE 无匹配即零行（SQLite 不报错）。
            // 存在性检查与此处之间会话被并发删除时，应回 404 而非 500。
            if matches!(e, rusqlite::Error::QueryReturnedNoRows) {
                return ApiError::not_found(format!("session not found: {}", session_id));
            }
            tracing::error!("update session end_ts failed: {}", e);
            ApiError::internal()
        })?;
    Ok(stored_end_ts)
}

pub(super) fn try_spawn_pipeline(
    state: &AppState,
    session_id: &str,
) -> Result<&'static str, ApiError> {
    if state.shutdown.load(std::sync::atomic::Ordering::SeqCst) {
        return Err(ApiError::service_unavailable(
            "gateway is shutting down; session end is unavailable",
        ));
    }
    let Some(ref llm) = state.llm else {
        return Ok("skipped (no LLM configured)");
    };
    let mut active = crate::util::recover_poison(&state.active_pipeline_sessions);
    if active.contains(session_id) {
        tracing::info!(
            "Post-session pipeline already running for session_id={}",
            session_id
        );
        return Ok("already_running");
    }
    let Ok(permit) = std::sync::Arc::clone(&state.pipeline_semaphore).try_acquire_owned() else {
        tracing::warn!(
            "Post-session pipeline concurrency limit reached; skipping session_id={}",
            session_id
        );
        return Ok("skipped (busy)");
    };
    active.insert(session_id.to_string());
    drop(active);

    let guard = SessionFlightGuard {
        active: state.active_pipeline_sessions.clone(),
        session_id: session_id.to_string(),
    };
    let sid = session_id.to_string();
    let db_clone = state.db.clone();
    let llm_clone = llm.clone();
    let emb_clone = state.embedder.clone();
    let cfg_clone = state.config.clone();
    let consolidation_flag = Some(state.consolidation_running.clone());
    let shutdown = state.shutdown.clone();
    tokio::spawn(async move {
        let _permit = permit;
        let result = tokio::task::spawn_blocking(move || {
            let _flight = guard;
            crate::service::pipeline::run_pipeline(
                db_clone,
                llm_clone,
                emb_clone,
                cfg_clone,
                sid,
                consolidation_flag,
                shutdown,
            );
        })
        .await;
        if let Err(e) = result {
            tracing::error!(
                "post-session pipeline task failed (panic or cancellation): {}",
                e
            );
        }
    });
    Ok("spawned")
}

pub(super) async fn session_end(
    State(state): State<AppState>,
    JsonBytes(body): JsonBytes,
) -> Result<Json<serde_json::Value>, ApiError> {
    if state.shutdown.load(std::sync::atomic::Ordering::SeqCst) {
        return Err(ApiError::service_unavailable(
            "gateway is shutting down; session end is unavailable",
        ));
    }
    // W5.1: content-type 由 JsonBytes 在缓冲前校验；blocking 池内完成 serde 与 DB 更新。
    let state_for_db = state.clone();
    let (session_id, now) =
        run_blocking("session_end", move || -> Result<(String, i64), ApiError> {
            let req: serde_json::Value = serde_json::from_slice(&body)
                .map_err(|e| ApiError::bad_request(format!("invalid JSON body: {}", e)))?;
            let session_id = req
                .get("session_id")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| ApiError::bad_request("session_id is required"))?;
            if let Some(error) = session_id_error(session_id) {
                return Err(ApiError::bad_request(error));
            }
            let now = crate::util::time::now_unix_ms();
            let stored_end_ts = session_end_db_update(&state_for_db, session_id, now)?;
            Ok((session_id.to_string(), stored_end_ts))
        })
        .await?;

    let pipeline_status = try_spawn_pipeline(&state, &session_id)?;

    Ok(Json(serde_json::json!({
        "status": "ok",
        "session_id": session_id,
        "end_ts": now,
        "pipeline": pipeline_status,
    })))
}

pub(super) async fn offload(
    State(state): State<AppState>,
    JsonBytes(body): JsonBytes,
) -> Result<Json<OffloadResponse>, ApiError> {
    // W5.1: content 合法可近 10MB——JsonBytes 缓冲后，serde 与磁盘写入合并为单次 blocking 调度。
    let refs_dir = state.config.refs_dir();
    let resp = run_blocking("offload", move || -> Result<OffloadResponse, ApiError> {
        let req: OffloadRequest = serde_json::from_slice(&body)
            .map_err(|e| ApiError::bad_request(format!("invalid JSON body: {}", e)))?;
        let bytes_stored = req.content.len();
        let node_id = crate::short_term::offload_text(&refs_dir, &req.task_id, &req.content)
            .map_err(|e| {
                if e.downcast_ref::<crate::short_term::ScanRejected>()
                    .is_some()
                    || e.downcast_ref::<crate::short_term::InvalidTaskId>()
                        .is_some()
                {
                    ApiError::bad_request(format!("offload failed: {}", e))
                } else {
                    tracing::error!("offload failed: {}", e);
                    ApiError::internal()
                }
            })?;
        Ok(OffloadResponse {
            node_id: node_id.to_string(),
            bytes_stored,
        })
    })
    .await?;

    Ok(Json(resp))
}

pub(super) async fn recall_by_node(
    State(state): State<AppState>,
    Path(node_id_str): Path<String>,
) -> Result<Json<RecallNodeResponse>, ApiError> {
    let node_id = crate::short_term::NodeId::parse(&node_id_str)
        .ok_or_else(|| ApiError::bad_request(format!("invalid node_id format: {}", node_id_str)))?;

    let refs_dir = state.config.refs_dir();
    let node_for_read = node_id.clone();
    let result = tokio::task::spawn_blocking(move || {
        crate::short_term::recall_text(&refs_dir, &node_for_read)
    })
    .await;
    let content = match result {
        Err(e) => {
            tracing::error!("recall_by_node task failed: {}", e);
            return Err(ApiError::internal());
        }
        Ok(Err(e)) => {
            if e.downcast_ref::<crate::short_term::InvalidTaskId>()
                .is_some()
            {
                return Err(ApiError::bad_request(format!("recall failed: {}", e)));
            }
            if e.downcast_ref::<crate::short_term::NodeNotFound>()
                .is_some()
            {
                return Err(ApiError::not_found(format!("recall failed: {}", e)));
            }
            tracing::error!("recall_by_node failed: {}", e);
            return Err(ApiError::internal());
        }
        Ok(Ok(content)) => content,
    };

    Ok(Json(RecallNodeResponse {
        node_id: node_id.to_string(),
        content,
    }))
}

pub(super) async fn health() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
    })
}

fn stats_blocking(state: &AppState) -> Result<StatsResponse, ApiError> {
    let db = acquire_db(state)?;
    let conn = db.conn();

    let tx = conn.unchecked_transaction().map_err(|e| {
        tracing::error!(
            target: "asuna_memory::transport::http::ops_handlers",
            "stats failed to start transaction: {}",
            e
        );
        ApiError::internal()
    })?;

    let count = |sql: &str| -> Result<i64, ApiError> {
        tx.query_row(sql, [], |r| r.get::<_, i64>(0)).map_err(|e| {
            tracing::error!(
                target: "asuna_memory::transport::http::ops_handlers",
                "stats query failed ({}): {}",
                sql,
                e
            );
            ApiError::internal()
        })
    };

    let sessions: i64 = count("SELECT COUNT(*) FROM sessions")?;
    let turns: i64 = count("SELECT COUNT(*) FROM turns")?;
    let vectors: i64 = count("SELECT COUNT(*) FROM vec_turns_rowids")?;
    let entities: i64 = count("SELECT COUNT(*) FROM entities")?;
    let relations: i64 = count("SELECT COUNT(*) FROM relations")?;

    tx.commit().map_err(|e| {
        tracing::error!(
            target: "asuna_memory::transport::http::ops_handlers",
            "stats commit failed: {}",
            e
        );
        ApiError::internal()
    })?;

    Ok(StatsResponse {
        sessions,
        turns,
        vectors,
        entities,
        relations,
    })
}

pub(super) async fn stats(State(state): State<AppState>) -> Result<Json<StatsResponse>, ApiError> {
    run_blocking("stats", move || stats_blocking(&state))
        .await
        .map(Json)
}

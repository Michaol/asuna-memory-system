use super::error::ApiError;
use super::models::{
    parse_search_mode, parse_time_window, validate_entity_length, validate_query_length,
    validate_relation_length, JsonBytes, SearchRequest, DEFAULT_TOP_K, MAX_TOP_K,
};
use super::{acquire_db, embed_search_query, ensure_graph_enabled, run_blocking};
use crate::transport::state::AppState;
use axum::{extract::State, response::Json};

/// Multi-hop hop bounds: default 2; the cap mirrors the private
/// MAX_HOPS_LIMIT inside graph_integration::multi_hop_query.
const DEFAULT_MAX_HOPS: u32 = 2;
const MAX_SEARCH_HOPS: u32 = 10;

fn search_multi_hop(
    state: &AppState,
    entity: &str,
    max_hops: u32,
    relation_filter: Option<&str>,
    top_k: usize,
) -> Result<Json<serde_json::Value>, ApiError> {
    let db = acquire_db(state)?;
    // A2/W5.4: 截断标志透出响应体——触达节点帽时原子集不完整，调用方不再只见 "ok"。
    let (atom_ids, truncated) = crate::memory::graph_integration::multi_hop_query_with_truncation(
        &db,
        entity,
        max_hops,
        relation_filter,
    )
    .map_err(|e| {
        tracing::error!(
            target: "asuna_memory::transport::http::search",
            "multi-hop query failed: {}",
            e
        );
        ApiError::internal()
    })?;
    let atoms = if atom_ids.is_empty() {
        Vec::new()
    } else {
        let mut atoms = batch_fetch_atoms(&db, &atom_ids)?;
        atoms.truncate(top_k);
        atoms
    };
    Ok(Json(serde_json::json!({
        "results": atoms,
        "query_type": "multi_hop",
        "entity": entity,
        "max_hops": max_hops,
        "truncated": truncated,
        "status": "ok"
    })))
}

pub(super) fn batch_fetch_atoms(
    db: &crate::index::db::Db,
    atom_ids: &[i64],
) -> Result<Vec<serde_json::Value>, ApiError> {
    if atom_ids.is_empty() {
        return Ok(Vec::new());
    }

    let mut all_atoms: Vec<(f64, serde_json::Value)> = Vec::new();
    crate::index::sql_util::for_each_in_chunk::<i64, ApiError, _>(atom_ids, |chunk, in_clause| {
        let sql = format!(
                "SELECT bm.id, bm.content, COALESCE(bm.memory_type, 'manual'),
                        CASE bm.confidence WHEN 'high' THEN 1.0 WHEN 'medium' THEN 0.5 ELSE 0.25 END,
                        bm.created_at
                 FROM bounded_memory bm
                 WHERE bm.id IN ({})
                   AND NOT EXISTS (SELECT 1 FROM bounded_memory s WHERE s.supersedes_id = bm.id)",
                in_clause
            );
        let mut stmt = db.conn().prepare(&sql).map_err(|e| {
            tracing::error!(
                target: "asuna_memory::transport::http::search",
                "batch_fetch_atoms prepare failed: {}",
                e
            );
            ApiError::internal()
        })?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(chunk), |row| {
                let score = row.get::<_, f64>(3)?;
                Ok((
                    score,
                    serde_json::json!({
                        "id": row.get::<_, i64>(0)?,
                        "content": row.get::<_, String>(1)?,
                        "memory_type": row.get::<_, String>(2)?,
                        "confidence_score": score,
                        "created_at": row.get::<_, i64>(4)?
                    }),
                ))
            })
            .map_err(|e| {
                tracing::error!(
                    target: "asuna_memory::transport::http::search",
                    "batch_fetch_atoms query failed: {}",
                    e
                );
                ApiError::internal()
            })?;
        for row in rows {
            let item = row.map_err(|e| {
                tracing::error!(
                    target: "asuna_memory::transport::http::search",
                    "batch_fetch_atoms row decode failed: {}",
                    e
                );
                ApiError::internal()
            })?;
            all_atoms.push(item);
        }
        Ok(())
    })?;
    all_atoms.sort_by(|a, b| b.0.total_cmp(&a.0));
    Ok(all_atoms.into_iter().map(|(_, v)| v).collect())
}

fn search_results_to_json(results: &[crate::fact::search::SearchResult]) -> Vec<serde_json::Value> {
    results
        .iter()
        .map(|r| {
            let mut obj = serde_json::json!({
                "turn_id": r.turn_id,
                "score": r.score,
                "preview": r.preview,
                "session_id": r.session_id,
                "timestamp_ms": r.timestamp_ms,
                "role": r.role,
            });
            if let Some(ref breakdown) = r.scores {
                match serde_json::to_value(breakdown) {
                    Ok(v) => obj["scores"] = v,
                    Err(e) => {
                        tracing::warn!(
                            target: "asuna_memory::transport::http::search",
                            "search result scores serialization failed: {}",
                            e
                        );
                    }
                }
            }
            obj
        })
        .collect()
}

fn search_text_blocking(
    state: &AppState,
    query_vec: Option<Vec<f32>>,
    params: &crate::fact::search::SearchParams,
) -> Result<serde_json::Value, ApiError> {
    let db = acquire_db(state)?;
    let results =
        crate::fact::search::search_sessions_with_vec(&db, query_vec, params).map_err(|e| {
            let msg = e.to_string();
            if msg.contains("语义搜索需要嵌入引擎") {
                ApiError::internal_with_message(format!("search failed: {}", msg))
            } else {
                tracing::error!(
                    target: "asuna_memory::transport::http::search",
                    "search failed: {}",
                    e
                );
                ApiError::internal()
            }
        })?;

    let results_json = search_results_to_json(&results);
    Ok(serde_json::json!({
        "results": results_json,
        "query_type": "text",
        "count": results_json.len(),
        "status": "ok"
    }))
}

async fn handle_multi_hop_search(
    state: AppState,
    entity: String,
    max_hops_opt: Option<u32>,
    relation_filter: Option<String>,
    top_k: usize,
) -> Result<Json<serde_json::Value>, ApiError> {
    ensure_graph_enabled(&state)?;
    if entity.trim().is_empty() {
        return Err(ApiError::bad_request("entity cannot be empty"));
    }
    validate_entity_length(&entity)?;
    if let Some(ref relation) = relation_filter {
        validate_relation_length("relation_filter", relation)?;
    }
    let max_hops = max_hops_opt.unwrap_or(DEFAULT_MAX_HOPS);
    if !(1..=MAX_SEARCH_HOPS).contains(&max_hops) {
        return Err(ApiError::bad_request(format!(
            "max_hops must be between 1 and {}",
            MAX_SEARCH_HOPS
        )));
    }
    if top_k == 0 {
        return Ok(Json(serde_json::json!({
            "results": [],
            "query_type": "multi_hop",
            "entity": entity,
            "max_hops": max_hops,
            "truncated": false,
            "status": "ok"
        })));
    }
    run_blocking("search_multi_hop", move || {
        search_multi_hop(&state, &entity, max_hops, relation_filter.as_deref(), top_k)
    })
    .await
}

pub(super) async fn search(
    State(state): State<AppState>,
    JsonBytes(body): JsonBytes,
) -> Result<Json<serde_json::Value>, ApiError> {
    // W5.1: JsonBytes 在缓冲前检查 content-type，blocking 池内 serde。
    // entity 优先/校验/早退逻辑保持原序。
    let req: SearchRequest = run_blocking(
        "search_prepare",
        move || -> Result<SearchRequest, ApiError> {
            serde_json::from_slice(&body)
                .map_err(|e| ApiError::bad_request(format!("invalid JSON body: {}", e)))
        },
    )
    .await?;

    let top_k = req.top_k.unwrap_or(DEFAULT_TOP_K).min(MAX_TOP_K);

    // entity 优先走 multi-hop；纯空白 entity 视同未提供，回落 text search
    // （合法 query 不应因空白 entity 被 400）。
    if let Some(entity) = req.entity.filter(|e| !e.trim().is_empty()) {
        if !req.query.trim().is_empty() {
            tracing::warn!(
                target: "asuna_memory::transport::http::search",
                "/search: entity and query both provided; query is ignored in multi-hop mode"
            );
        }
        return handle_multi_hop_search(state, entity, req.max_hops, req.relation_filter, top_k)
            .await;
    }

    if req.query.trim().is_empty() {
        return Err(ApiError::bad_request(
            "query cannot be empty for text search",
        ));
    }
    validate_query_length(&req.query)?;

    let search_mode = parse_search_mode(req.mode.as_deref())?;
    // role 白名单与 /capture 对齐（非法值此前静默 200 空结果）。
    if let Some(ref role) = req.role {
        if !crate::util::VALID_ROLES.contains(&role.as_str()) {
            return Err(ApiError::bad_request(format!(
                "invalid role '{}' (allowed: {:?})",
                role,
                crate::util::VALID_ROLES
            )));
        }
    }
    let (effective_after, before_ms) =
        parse_time_window(req.after.as_deref(), req.before.as_deref(), req.last_days)?;

    if top_k == 0 {
        return Ok(Json(serde_json::json!({
            "results": [],
            "query_type": "text",
            "count": 0,
            "status": "ok"
        })));
    }

    let params = crate::fact::search::SearchParams {
        query: req.query,
        search_mode,
        top_k,
        after_ms: effective_after,
        before_ms,
        role: req.role,
    };

    let needs_vec = matches!(
        params.search_mode,
        crate::fact::search::SearchMode::Semantic | crate::fact::search::SearchMode::Hybrid
    );
    let is_semantic = matches!(
        params.search_mode,
        crate::fact::search::SearchMode::Semantic
    );
    // M10a: preserve the semantic error contract without acquiring the DB mutex.
    if is_semantic && state.embedder.is_none() {
        return Err(ApiError::internal_with_message(
            "search failed: 语义搜索需要嵌入引擎",
        ));
    }
    let query_vec =
        embed_search_query(&state, needs_vec, is_semantic, params.query.clone()).await?;

    run_blocking("search_text", move || {
        search_text_blocking(&state, query_vec, &params)
    })
    .await
    .map(Json)
}

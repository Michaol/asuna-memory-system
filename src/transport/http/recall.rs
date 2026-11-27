use super::error::ApiError;
use super::models::{
    parse_time_window, validate_query_length, JsonBytes, RecallRequest, RecallResponse,
    DEFAULT_TOP_K, MAX_TOP_K,
};
use super::{acquire_db, embed_search_query, run_blocking};
use crate::transport::state::AppState;
use axum::{extract::State, response::Json};

pub(super) async fn recall(
    State(state): State<AppState>,
    JsonBytes(body): JsonBytes,
) -> Result<Json<RecallResponse>, ApiError> {
    // W5.1: JSON 反序列化移出 tokio worker——JsonBytes 在缓冲前检查 content-type，
    // 随后在 blocking 池内 serde 解析（校验/top_k=0 早退/时间窗逻辑保持原序）。
    let req: RecallRequest = run_blocking(
        "recall_prepare",
        move || -> Result<RecallRequest, ApiError> {
            serde_json::from_slice(&body)
                .map_err(|e| ApiError::bad_request(format!("invalid JSON body: {}", e)))
        },
    )
    .await?;

    if req.query.trim().is_empty() {
        return Err(ApiError::bad_request("query cannot be empty"));
    }
    validate_query_length(&req.query)?;

    let top_k = req.top_k.unwrap_or(DEFAULT_TOP_K).min(MAX_TOP_K);
    let (effective_after, before_ms) =
        parse_time_window(req.after.as_deref(), req.before.as_deref(), req.last_days)?;
    // W2.1: max_tokens 上界帽防下游乘法溢出（retrieval 侧另有 saturating_mul 纵深）。
    let max_tokens = req.max_tokens.map(|v| v.min(1_000_000));
    // W2.2/M10c: 空结果请求在时间窗校验后早退，不做文件/DB/嵌入工作。
    if top_k == 0 || max_tokens == Some(0) {
        return Ok(Json(RecallResponse {
            memories: Vec::new(),
            context: String::new(),
            truncated: false,
        }));
    }
    let query = req.query;
    let query_vec = embed_search_query(&state, true, false, query.clone()).await?;

    run_blocking("recall", move || {
        let memory_dir = state.config.memory_dir();
        let preloaded = crate::memory::retrieval::preload_recall_docs(&memory_dir);
        let db = acquire_db(&state)?;
        let engine = crate::memory::retrieval::RetrievalEngine::new(
            &db,
            &memory_dir,
            state.config.recall.token_budget,
        )
        .with_preloaded_docs(preloaded);
        let outcome = engine
            .recall_with_vec(
                &query,
                query_vec.as_deref(),
                top_k,
                effective_after,
                before_ms,
                max_tokens,
            )
            .map_err(|e| {
                tracing::error!(
                    target: "asuna_memory::transport::http::recall",
                    "recall failed: {}",
                    e
                );
                ApiError::internal()
            })?;

        Ok(Json(RecallResponse {
            memories: outcome.memories,
            context: outcome.context,
            truncated: outcome.truncated,
        }))
    })
    .await
}

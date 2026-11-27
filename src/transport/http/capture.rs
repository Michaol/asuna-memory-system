use super::error::ApiError;
use super::models::{session_id_error, CaptureRequest, CaptureResponse, JsonBytes};
use super::{acquire_db, run_blocking};
use crate::transport::state::AppState;
use axum::{extract::State, response::Json};
use rusqlite::params;

pub(super) const MAX_TURNS_PER_BATCH: usize = 1000;

/// Parse a turn timestamp value into epoch milliseconds (`0..1e12` -> seconds*1000,
/// `1e12..1e15` -> milliseconds, ISO 8601 string -> parsed).
pub(super) fn parse_turn_timestamp(v: &serde_json::Value) -> Result<i64, String> {
    const SECONDS_CUTOFF: i64 = 1_000_000_000_000; // 1e12
    const MS_UPPER_BOUND: i64 = 1_000_000_000_000_000; // 1e15
    if let Some(n) = v.as_i64() {
        if n < 0 {
            return Err(format!("timestamp {n} is negative"));
        }
        if n >= MS_UPPER_BOUND {
            return Err(format!(
                "timestamp {n} is outside the plausible epoch-ms band"
            ));
        }
        if n < SECONDS_CUTOFF {
            tracing::warn!(
                target: "asuna_memory::transport::http::capture",
                "turn timestamp {} looks like epoch seconds (<1e12); converting to ms (x1000)",
                n
            );
            return Ok(n * 1000);
        }
        return Ok(n);
    }
    if let Some(s) = v.as_str() {
        return crate::util::time::ts_to_unix_ms(s)
            .map_err(|e| format!("unparseable timestamp string: {}", e));
    }
    Err("timestamp must be an epoch-ms integer or an ISO 8601 string".to_string())
}

fn validate_turn_entry(i: usize, turn: &serde_json::Value) -> Result<Option<i64>, ApiError> {
    let Some(obj) = turn.as_object() else {
        return Err(ApiError::bad_request(format!(
            "turn[{}] must be an object",
            i
        )));
    };
    if !obj.contains_key("role") || !obj.contains_key("content") {
        return Err(ApiError::bad_request(format!(
            "turn[{}] must have 'role' and 'content' fields",
            i
        )));
    }
    let role = obj
        .get("role")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| ApiError::bad_request(format!("turn[{}] 'role' must be a string", i)))?;
    if !crate::util::VALID_ROLES.contains(&role) {
        return Err(ApiError::bad_request(format!(
            "turn[{}] invalid role '{}' (allowed: {:?})",
            i,
            role,
            crate::util::VALID_ROLES
        )));
    }
    if !obj.get("content").is_some_and(serde_json::Value::is_string) {
        return Err(ApiError::bad_request(format!(
            "turn[{}] 'content' must be a string",
            i
        )));
    }
    obj.get("timestamp")
        .map(|ts| {
            parse_turn_timestamp(ts)
                .map_err(|e| ApiError::bad_request(format!("turn[{}] {}", i, e)))
        })
        .transpose()
}

/// A1/W4.3: batch_id 幂等键校验——尺标参考 validate_session_id（非空、≤255
/// 字符、无控制字符），但 batch_id 非路径组件，不限制分隔符/':'（客户端可用
/// UUID 或任意标识）。仅在字段存在（Some）时校验；None 走旧无幂等路径。
fn validate_batch_id(batch_id: &str) -> Result<(), ApiError> {
    if batch_id.trim().is_empty() {
        return Err(ApiError::bad_request("batch_id cannot be empty"));
    }
    if super::models::exceeds_char_limit(batch_id, 255) {
        return Err(ApiError::bad_request(
            "batch_id too long (max 255 characters)",
        ));
    }
    if batch_id.chars().any(char::is_control) {
        return Err(ApiError::bad_request(
            "batch_id must not contain control characters",
        ));
    }
    Ok(())
}

pub(super) fn validate_capture_request(req: &CaptureRequest) -> Result<Vec<Option<i64>>, ApiError> {
    if let Some(error) = session_id_error(&req.session_id) {
        return Err(ApiError::bad_request(error));
    }
    if let Some(ref batch_id) = req.batch_id {
        validate_batch_id(batch_id)?;
    }
    if req.turns.is_empty() {
        return Err(ApiError::bad_request("turns array cannot be empty"));
    }
    if req.turns.len() > MAX_TURNS_PER_BATCH {
        return Err(ApiError::bad_request(format!(
            "too many turns in one batch (max {})",
            MAX_TURNS_PER_BATCH
        )));
    }
    req.turns
        .iter()
        .enumerate()
        .map(|(i, turn)| validate_turn_entry(i, turn))
        .collect()
}

async fn embed_capture_turns(
    embedder: Option<std::sync::Arc<crate::embedder::LazyEmbedder>>,
    turn_contents: &[String],
) -> Option<Vec<Vec<f32>>> {
    let emb = embedder?;
    let contents = turn_contents.to_vec();
    match tokio::task::spawn_blocking(move || {
        let texts: Vec<&str> = contents.iter().map(String::as_str).collect();
        match emb.embed_documents(&texts) {
            Ok(vecs) => {
                // embed_documents guarantees one vector per text (the API
                // layer bails on count mismatch) — a defensive branch here
                // would be unreachable.
                debug_assert_eq!(vecs.len(), texts.len());
                Some(vecs)
            }
            Err(e) => {
                tracing::warn!(
                    target: "asuna_memory::transport::http::capture",
                    "capture: embedding batch failed ({}); storing {} turns without vectors",
                    e,
                    texts.len()
                );
                None
            }
        }
    })
    .await
    {
        Ok(vecs) => vecs,
        Err(e) => {
            tracing::error!(
                target: "asuna_memory::transport::http::capture",
                "capture: embedding task failed: {}",
                e
            );
            None
        }
    }
}

fn build_capture_turns(
    req_turns: &[serde_json::Value],
    timestamps: &[Option<i64>],
    now: i64,
) -> Result<Vec<crate::fact::conversation::Turn>, ApiError> {
    let mut turns = Vec::with_capacity(req_turns.len());
    for (i, (turn_val, timestamp_ms)) in req_turns.iter().zip(timestamps).enumerate() {
        let Some(obj) = turn_val.as_object() else {
            tracing::error!(
                target: "asuna_memory::transport::http::capture",
                "capture: turn[{}] is not an object (internal invariant)",
                i
            );
            return Err(ApiError::internal());
        };
        let role = obj
            .get("role")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        let content = obj
            .get("content")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        turns.push(crate::fact::conversation::Turn {
            ts: crate::util::time::unix_ms_to_iso(timestamp_ms.unwrap_or(now)),
            seq: 0,
            role: role.to_string(),
            content: content.to_string(),
            metadata: None,
        });
    }
    Ok(turns)
}

fn lookup_capture_dedup(
    state: &AppState,
    session_id: &str,
    batch_id: &str,
) -> Result<Option<usize>, ApiError> {
    use rusqlite::OptionalExtension;
    let db = acquire_db(state)?;
    let saved: Option<i64> = db
        .conn()
        .query_row(
            "SELECT turns_saved FROM capture_dedup WHERE session_id = ?1 AND batch_id = ?2",
            params![session_id, batch_id],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| {
            tracing::error!(
                target: "asuna_memory::transport::http::capture",
                "capture dedup lookup failed: {}",
                e
            );
            ApiError::internal()
        })?;
    Ok(saved.map(|n| n.max(0) as usize))
}

fn capture_persist_blocking(
    state: &AppState,
    session_id: &str,
    batch_id: Option<&str>,
    first_ts: i64,
    turns: &[crate::fact::conversation::Turn],
    flagged_turns: &[(usize, crate::growth::security::ScanResult)],
    embeddings: Option<&[Vec<f32>]>,
) -> Result<usize, ApiError> {
    let db = acquire_db(state)?;
    let conn = db.conn();

    let session_start_ts: Option<i64> = {
        use rusqlite::OptionalExtension;
        conn.query_row(
            "SELECT start_ts FROM sessions WHERE session_id = ?1",
            params![session_id],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| {
            tracing::error!(
                target: "asuna_memory::transport::http::capture",
                "capture session lookup failed: {}",
                e
            );
            ApiError::internal()
        })?
    };

    let header = crate::fact::conversation::SessionHeader {
        v: 1,
        header_type: "session_header".to_string(),
        session_id: session_id.to_string(),
        start_time: crate::util::time::unix_ms_to_iso(session_start_ts.unwrap_or(first_ts)),
        profile_id: state.config.profile_id.clone(),
        source: Some("gateway".to_string()),
        agent_model: None,
        title: None,
        tags: vec![],
    };

    let conv_dir = state.config.conversations_dir();
    let store = crate::fact::session_store::SessionStore::new(&conv_dir, &db)
        .with_preview_length(state.config.conversation.preview_length);
    let (stats, dedup_hit) = store
        .save_append_idempotent(&header, turns, embeddings, batch_id)
        .map_err(|e| {
            tracing::error!(
                target: "asuna_memory::transport::http::capture",
                "capture persist failed: {}",
                e
            );
            ApiError::internal()
        })?;

    if dedup_hit {
        if let Some(bid) = batch_id {
            tracing::info!(
                target: "asuna_memory::transport::http::capture",
                "capture: batch_id={} 重复（session_id={}），幂等返回 turns_saved={}",
                bid,
                session_id,
                stats.turns_saved
            );
        }
        return Ok(stats.turns_saved);
    }

    for (i, scan) in flagged_turns {
        if let Some(turn) = turns.get(*i) {
            crate::growth::audit::record_unsafe_turn(&db, session_id, *i, &turn.content, scan);
        }
    }
    Ok(stats.turns_saved)
}

enum CapturePrepareOutcome {
    Duplicate(usize),
    Ready(PreparedCapture),
}

/// CPU stage of `/capture` (W28a): timestamp parsing, turn building, preview
/// truncation and the security scan of every turn's full content. Kept in one
/// blocking-pool closure — a 10MB batch must not monopolize a tokio worker.
struct PreparedCapture {
    session_id: String,
    batch_id: Option<String>,
    first_ts: i64,
    turns: Vec<crate::fact::conversation::Turn>,
    preview_contents: Vec<String>,
    flagged_turns: Vec<(usize, crate::growth::security::ScanResult)>,
}

fn prepare_capture(
    req: &CaptureRequest,
    preview_len: usize,
    timestamps: &[Option<i64>],
) -> Result<PreparedCapture, ApiError> {
    let now = chrono::Utc::now().timestamp_millis();
    let first_ts = timestamps.iter().flatten().next().copied().unwrap_or(now);

    let turns = build_capture_turns(&req.turns, timestamps, now)?;
    let preview_contents: Vec<String> = turns
        .iter()
        .map(|t| t.content.chars().take(preview_len).collect())
        .collect();
    let flagged_turns: Vec<(usize, crate::growth::security::ScanResult)> = turns
        .iter()
        .enumerate()
        .filter_map(|(i, t)| {
            let scan = crate::growth::security::scan_content(&t.content);
            (!scan.is_safe()).then_some((i, scan))
        })
        .collect();
    Ok(PreparedCapture {
        session_id: req.session_id.clone(),
        batch_id: req.batch_id.clone(),
        first_ts,
        turns,
        preview_contents,
        flagged_turns,
    })
}

pub(super) async fn capture(
    State(state): State<AppState>,
    JsonBytes(body): JsonBytes,
) -> Result<Json<CaptureResponse>, ApiError> {
    // R1: JsonBytes 在缓冲前检查 content-type；随后在 blocking 池内 serde
    // 解析 + 校验 + 查重/准备（10MB body 解析不再独占 worker）。

    let preview_len = state.config.conversation.preview_length.max(1);
    let state_prep = state.clone();
    let PreparedCapture {
        session_id,
        batch_id,
        first_ts,
        turns,
        preview_contents,
        flagged_turns,
    } = match run_blocking("capture_prepare", move || {
        let req: CaptureRequest = serde_json::from_slice(&body)
            .map_err(|e| ApiError::bad_request(format!("invalid JSON body: {}", e)))?;
        let timestamps = validate_capture_request(&req)?;
        if let Some(ref bid) = req.batch_id {
            if let Some(saved) = lookup_capture_dedup(&state_prep, &req.session_id, bid)? {
                tracing::info!(
                    target: "asuna_memory::transport::http::capture",
                    "capture: batch_id={} 命中预查去重（session_id={}），跳过扫描与嵌入",
                    bid,
                    req.session_id
                );
                return Ok(CapturePrepareOutcome::Duplicate(saved));
            }
        }
        Ok(CapturePrepareOutcome::Ready(prepare_capture(
            &req,
            preview_len,
            &timestamps,
        )?))
    })
    .await?
    {
        CapturePrepareOutcome::Duplicate(saved) => {
            return Ok(Json(CaptureResponse {
                status: "ok".to_string(),
                turns_saved: saved,
            }));
        }
        CapturePrepareOutcome::Ready(p) => p,
    };
    let embeddings = embed_capture_turns(state.embedder.clone(), &preview_contents).await;

    // A1: turns_saved 由 persist 返回——无 batch_id/新键为本批条数，重复键幂等
    // 返回首存值（不追加 turn）。
    let turns_saved = run_blocking("capture", move || {
        capture_persist_blocking(
            &state,
            &session_id,
            batch_id.as_deref(),
            first_ts,
            &turns,
            &flagged_turns,
            embeddings.as_deref(),
        )
    })
    .await?;

    Ok(Json(CaptureResponse {
        status: "ok".to_string(),
        turns_saved,
    }))
}

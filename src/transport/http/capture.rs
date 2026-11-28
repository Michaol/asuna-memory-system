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

/// Best-effort PRE-check of the `(session_id, batch_id)` dedup key: if the batch
/// already committed, return its `turns_saved` so the handler can skip the costly
/// scan/embed stage entirely. This is only a fast path — retry safety itself comes
/// from `save_append_idempotent` committing the dedup row in the SAME tx as the
/// turns (see `MAX_CAPTURE_BUSY_RETRIES`). `None` = no prior commit; `.max(0)`
/// defends against a corrupt negative count.
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

/// Finite BUSY/LOCKED retries for `/capture` persist (perf H1/C1). Each attempt
/// already waits up to `busy_timeout = 5000ms` inside SQLite, so 1 initial + 2
/// retries ≈ 15s worst case — well within `REQUEST_TIMEOUT_SECS = 60`. No extra
/// backoff sleep is added between attempts: the busy_timeout wait IS the backoff,
/// and sleeping longer would only eat into the request budget. Append is
/// idempotent (the `capture_dedup` row commits in the SAME tx as turns/JSONL), so
/// a retry after a failed attempt cannot duplicate turns.
const MAX_CAPTURE_BUSY_RETRIES: usize = 2;

/// SQLITE_BUSY / SQLITE_LOCKED (including all extended codes) are transient
/// contention: a concurrent writer (rebuild/migration/another process) may hold
/// the write lock past `busy_timeout`. rusqlite maps every extended BUSY/LOCKED
/// code to the primary `ErrorCode` (`ffi::Error::new` masks `& 0xff`), so matching
/// the primary code covers BUSY_RECOVERY/BUSY_SNAPSHOT/LOCKED_SHAREDCACHE etc.
fn is_sqlite_busy(e: &anyhow::Error) -> bool {
    e.chain().any(|cause| {
        cause.downcast_ref::<rusqlite::Error>().is_some_and(|se| {
            matches!(
                se.sqlite_error_code(),
                Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
            )
        })
    })
}

/// Run the idempotent Append with finite BUSY/LOCKED retry; map exhaustion to
/// 503 (retryable) rather than 500. Non-contention errors stay 500 (internal).
/// Lock discipline: the attempt closure must re-acquire L_gw per attempt and
/// drop it before returning, so the ~15s worst-case retry window never holds
/// the gateway mutex (other handlers / the `.md` flush can interleave).
fn save_append_with_busy_retry<F, T>(mut attempt: F) -> Result<T, ApiError>
where
    F: FnMut() -> anyhow::Result<T>,
{
    let mut retries = 0usize;
    loop {
        match attempt() {
            Ok(v) => return Ok(v),
            Err(e) => {
                // Not contention → a genuine internal fault; retrying cannot help.
                if !is_sqlite_busy(&e) {
                    tracing::error!(
                        target: "asuna_memory::transport::http::capture",
                        "capture persist failed: {}",
                        e
                    );
                    return Err(ApiError::internal());
                }
                // Transient contention with budget left: retry. The next attempt's
                // busy_timeout supplies the wait, so no separate sleep is needed.
                if retries < MAX_CAPTURE_BUSY_RETRIES {
                    retries += 1;
                    tracing::warn!(
                        target: "asuna_memory::transport::http::capture",
                        "capture persist BUSY/LOCKED, retry {}/{}: {}",
                        retries,
                        MAX_CAPTURE_BUSY_RETRIES,
                        e
                    );
                    continue;
                }
                // Budget exhausted: 503 (not 500) tells the client the request is
                // safe to retry — batch_id dedup guards against duplicate turns.
                tracing::error!(
                    target: "asuna_memory::transport::http::capture",
                    "capture persist still BUSY/LOCKED after {} retries: {}",
                    retries,
                    e
                );
                return Err(ApiError::service_unavailable(
                    "database busy after retries; the request is safe to retry \
                     (batch_id dedup guards against duplicate turns)",
                ));
            }
        }
    }
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
    // session_start_ts 预查只需短暂持锁：guard 在块末即 drop，绝不跨下面的
    // BUSY 重试循环（重试期间持 L_gw 会让全部 handler 与 .md flush 排队 ~15s）。
    let session_start_ts: Option<i64> = {
        let db = acquire_db(state)?;
        use rusqlite::OptionalExtension;
        db.conn()
            .query_row(
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
    // L_gw 逐 attempt 重新获取、attempt 之间 drop（perf H1/C1）：BUSY 重试最坏
    // ~15s 的 SQLite busy 等待不得横跨网关互斥锁。每次 attempt 自带完整事务
    // （run_in_transaction 失败即 drop 回滚），无打开的写事务跨 attempt；
    // acquire_db 的 rollback_if_in_tx 再兜底清除残留事务。
    let (stats, dedup_hit) = save_append_with_busy_retry(|| {
        let db = acquire_db(state)
            .map_err(|e| anyhow::anyhow!("capture db lock acquire failed ({})", e.status()))?;
        let store = crate::fact::session_store::SessionStore::new(&conv_dir, &db)
            .with_preview_length(state.config.conversation.preview_length);
        store.save_append_idempotent(&header, turns, embeddings, batch_id)
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

    // 审计写入同样只在短暂持锁内进行；无 flagged turn 时不取锁。
    if !flagged_turns.is_empty() {
        let db = acquire_db(state)?;
        for (i, scan) in flagged_turns {
            if let Some(turn) = turns.get(*i) {
                crate::growth::audit::record_unsafe_turn(&db, session_id, *i, &turn.content, scan);
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn sqlite_err(code: i32) -> anyhow::Error {
        anyhow::Error::new(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(code),
            Some("mock sqlite failure".to_string()),
        ))
    }

    /// Extended BUSY/LOCKED codes all fold to the primary ErrorCode, so the
    /// classifier recognises them (and only them) as transient contention.
    #[test]
    fn is_sqlite_busy_covers_extended_codes() {
        // SQLITE_BUSY=5, BUSY_RECOVERY=261, BUSY_SNAPSHOT=517, BUSY_TIMEOUT=773
        for code in [5, 261, 517, 773] {
            assert!(is_sqlite_busy(&sqlite_err(code)), "BUSY extended {code}");
        }
        // SQLITE_LOCKED=6, LOCKED_SHAREDCACHE=262
        for code in [6, 262] {
            assert!(is_sqlite_busy(&sqlite_err(code)), "LOCKED extended {code}");
        }
        // SQLITE_CONSTRAINT=19 is not contention; a non-rusqlite error is not either.
        assert!(!is_sqlite_busy(&sqlite_err(19)));
        assert!(!is_sqlite_busy(&anyhow::anyhow!("plain io failure")));
        // Wrapped in an anyhow context chain still detected.
        assert!(is_sqlite_busy(&sqlite_err(5).context("outer")));
    }

    /// BUSY twice then success: retried (3 attempts) and returns Ok.
    #[test]
    fn busy_retry_succeeds_after_transient_contention() {
        let calls = Cell::new(0);
        let res = save_append_with_busy_retry(|| {
            let n = calls.get() + 1;
            calls.set(n);
            if n <= 2 {
                Err(sqlite_err(5))
            } else {
                Ok(42usize)
            }
        });
        assert_eq!(res.unwrap(), 42);
        assert_eq!(calls.get(), 3, "1 initial + 2 retries");
    }

    /// Persistent BUSY exhausts retries and maps to 503 (retryable), not 500.
    #[test]
    fn persistent_busy_maps_to_service_unavailable() {
        let calls = Cell::new(0);
        let err = save_append_with_busy_retry(|| {
            calls.set(calls.get() + 1);
            Err::<usize, _>(sqlite_err(517))
        })
        .unwrap_err();
        assert_eq!(err.status(), axum::http::StatusCode::SERVICE_UNAVAILABLE);
        assert!(err.message().contains("retry"));
        assert_eq!(calls.get(), MAX_CAPTURE_BUSY_RETRIES + 1);
    }

    /// A non-contention error is NOT retried and stays 500 (internal).
    #[test]
    fn non_busy_error_is_not_retried() {
        let calls = Cell::new(0);
        let err = save_append_with_busy_retry(|| {
            calls.set(calls.get() + 1);
            Err::<usize, _>(sqlite_err(19))
        })
        .unwrap_err();
        assert_eq!(err.status(), axum::http::StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(calls.get(), 1, "non-BUSY must not retry");
    }

    /// First-attempt success makes exactly one call.
    #[test]
    fn immediate_success_single_attempt() {
        let calls = Cell::new(0);
        let res = save_append_with_busy_retry(|| {
            calls.set(calls.get() + 1);
            Ok(7usize)
        });
        assert_eq!(res.unwrap(), 7);
        assert_eq!(calls.get(), 1);
    }
}

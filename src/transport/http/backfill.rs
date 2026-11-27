//! Startup vector backfill (W28b) — lifecycle work, not HTTP plumbing.
//! Moved out of `http/mod.rs` so the gateway module stays focused on route
//! assembly, middleware stack and shutdown.

use crate::index::db::{
    is_permanent_backfill_error, run_backfill_chunks_with_control, BackfillChunkOutcome,
};
use crate::transport::state::AppState;

#[cfg(test)]
pub(super) fn is_permanent_backfill_error_for_test(e: &anyhow::Error) -> bool {
    is_permanent_backfill_error(e)
}

fn backfill_one_chunk(
    state: &AppState,
    emb: &crate::embedder::LazyEmbedder,
    chunk: &[(i64, String)],
) -> BackfillChunkOutcome {
    let texts: Vec<&str> = chunk.iter().map(|(_, c)| c.as_str()).collect();
    let embeddings = match emb.embed_documents(&texts) {
        Ok(embs) => embs,
        Err(e) => {
            tracing::warn!("vec_bounded_memory backfill: batch embed failed: {}", e);
            return if is_permanent_backfill_error(&e) {
                BackfillChunkOutcome::Permanent
            } else {
                BackfillChunkOutcome::Failed
            };
        }
    };
    let db = crate::util::recover_poison(&state.db);
    db.rollback_if_in_tx();
    if let Err(e) = db.insert_backfill_vec_chunk(chunk, &embeddings) {
        tracing::warn!("vec_bounded_memory backfill: insert failed: {}", e);
        return BackfillChunkOutcome::Failed;
    }
    BackfillChunkOutcome::Done
}

fn fetch_pending_backfill(state: &AppState) -> Option<Vec<(i64, String)>> {
    let db = crate::util::recover_poison(&state.db);
    db.rollback_if_in_tx();
    match db.prepare_bounded_memory_vec_backfill() {
        Ok(items) => Some(items),
        Err(e) => {
            tracing::warn!("vec_bounded_memory backfill skipped: {}", e);
            None
        }
    }
}

pub(super) fn run_startup_vec_backfill_blocking(state: &AppState) {
    let Some(ref emb) = state.embedder else {
        return;
    };
    let Some(pending) = fetch_pending_backfill(state) else {
        return;
    };
    if pending.is_empty() {
        return;
    }
    tracing::info!(
        "Backfilling vec_bounded_memory in background: {} atoms need embeddings...",
        pending.len()
    );
    let batch_size = emb.batch_size().max(1);
    if let Err(e) = run_backfill_chunks_with_control(
        &pending,
        batch_size,
        Some(state.shutdown.as_ref()),
        |chunk| Ok(backfill_one_chunk(state, emb, chunk)),
    ) {
        tracing::warn!("vec_bounded_memory backfill failed: {}", e);
    }
}

pub(super) fn spawn_startup_vec_backfill(state: AppState) -> Option<tokio::task::JoinHandle<()>> {
    state.embedder.as_ref()?;
    Some(tokio::spawn(async move {
        if let Err(e) = tokio::task::spawn_blocking(move || {
            run_startup_vec_backfill_blocking(&state);
        })
        .await
        {
            tracing::error!("vec_bounded_memory backfill task failed: {}", e);
        }
    }))
}

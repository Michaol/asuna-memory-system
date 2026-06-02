//! Post-session pipeline: L1 atom extraction → graph integration
//!
//! Triggered asynchronously from `/session/end` via `tokio::task::spawn_blocking`.
//! All operations (LLM calls, DB queries) are synchronous, so the entire pipeline
//! runs as a blocking task to avoid starving the tokio runtime.

use crate::config::Config;
use crate::embedder::LazyEmbedder;
use crate::index::db::Db;
use crate::memory::graph_integration::integrate_atom_with_graph;
use crate::memory::l1::{L1Extractor, TurnContent};
use crate::memory::llm::LlmClient;
use std::sync::{Arc, Mutex};

/// Run the post-session extraction + graph pipeline.
///
/// Designed to run inside `tokio::task::spawn_blocking`. Steps:
/// 1. Read session turns from DB
/// 2. Extract atomic facts via LLM (L1Extractor)
/// 3. Store atoms with embedding + admission scoring
/// 4. Integrate atoms into the knowledge graph
///
/// Failures are logged but never propagated — the pipeline is best-effort.
pub fn run_pipeline(
    db: Arc<Mutex<Db>>,
    llm: Arc<LlmClient>,
    embedder: Option<Arc<Mutex<LazyEmbedder>>>,
    config: Arc<Config>,
    session_id: String,
) {
    // Gate: pipeline and graph must both be enabled
    if !config.pipeline.enable_extraction {
        tracing::debug!("Pipeline disabled, skip session {}", session_id);
        return;
    }
    if !config.graph.enabled {
        tracing::debug!("Graph disabled, skip session {}", session_id);
        return;
    }

    // Acquire locks for the entire pipeline duration.
    // L1Extractor borrows &Db and &LazyEmbedder, so we must hold both locks
    // while the extractor is alive.
    let db_guard = match db.lock() {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!("Pipeline: DB lock failed for session {}: {}", session_id, e);
            return;
        }
    };

    let embedder_guard = embedder.as_ref().and_then(|e| e.lock().ok());
    let embedder_ref: Option<&LazyEmbedder> = embedder_guard.as_deref();

    // 1. Read session turns
    let (turns, turn_ids) = {
        let mut stmt = match db_guard.conn().prepare(
            "SELECT id, role, preview FROM turns WHERE session_id = ?1 ORDER BY seq",
        ) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("Pipeline: turn query failed for {}: {}", session_id, e);
                return;
            }
        };

        let rows: Vec<(i64, String, String)> = stmt
            .query_map(rusqlite::params![session_id], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
            .unwrap_or_default();

        let turn_ids: Vec<i64> = rows.iter().map(|(id, _, _)| *id).collect();
        let turns: Vec<TurnContent> = rows
            .into_iter()
            .map(|(_, role, content)| TurnContent { role, content })
            .collect();

        (turns, turn_ids)
    };

    // Gate: skip short sessions
    if turns.len() < config.pipeline.every_n_turns {
        tracing::debug!(
            "Pipeline: session {} too short ({} turns < {}), skipping",
            session_id,
            turns.len(),
            config.pipeline.every_n_turns
        );
        return;
    }

    // 2. Build extractor and extract atoms via LLM
    let db_ref: &Db = &db_guard;
    let bounded_memory = crate::growth::bounded_memory::BoundedMemory::new(
        &config.memory_dir(),
        db_ref,
        config.memory.memory_char_limit,
        config.memory.user_char_limit,
    )
    .with_atom_capacity_ratio(config.memory.atom_capacity_ratio);

    let extractor = if config.admission.enabled {
        L1Extractor::with_admission(db_ref, &llm, embedder_ref, &config.admission)
            .with_growth(bounded_memory)
    } else {
        L1Extractor::new(db_ref, &llm, embedder_ref)
            .with_growth(bounded_memory)
    };

    let atoms = match extractor.extract_from_turns(&turns) {
        Ok(a) if !a.is_empty() => a,
        Ok(_) => {
            tracing::debug!("Pipeline: no atoms extracted for session {}", session_id);
            return;
        }
        Err(e) => {
            tracing::warn!("Pipeline: L1 extraction failed for {}: {}", session_id, e);
            return;
        }
    };

    tracing::info!(
        "Pipeline: extracted {} atoms from session {} ({} turns)",
        atoms.len(),
        session_id,
        turns.len()
    );

    // 3. Store atoms (embedding + admission + dedup + write to bounded_memory)
    let atom_ids = match extractor.store_atoms(&atoms, &turn_ids) {
        Ok(ids) => ids,
        Err(e) => {
            tracing::warn!("Pipeline: store_atoms failed for {}: {}", session_id, e);
            return;
        }
    };

    // 4. Graph integration — create entities + from_session + mentions relations
    let mut graph_count = 0u32;
    for (i, atom) in atoms.iter().enumerate() {
        if i >= atom_ids.len() {
            break;
        }

        // Filter entity names: skip entries shorter than 2 chars (likely noise)
        let entities: Vec<String> = atom
            .entities
            .iter()
            .filter(|e| e.chars().count() >= 2)
            .cloned()
            .collect();

        match integrate_atom_with_graph(
            &db_guard,
            atom_ids[i],
            &atom.content,
            None,              // supersedes_id (store_atoms handles this internally)
            Some(&session_id), // source session
            &entities,         // extracted_entities (LLM-based from atom prompt)
            &[],               // similar_atom_ids (future: vector similarity)
        ) {
            Ok(_) => graph_count += 1,
            Err(e) => {
                tracing::debug!(
                    "Pipeline: graph integration failed for atom {}: {}",
                    atom_ids[i],
                    e
                );
            }
        }
    }

    drop(embedder_guard);
    drop(db_guard);

    tracing::info!(
        "Pipeline complete for session {}: {} atoms stored, {} graph integrations",
        session_id,
        atom_ids.len(),
        graph_count
    );
}

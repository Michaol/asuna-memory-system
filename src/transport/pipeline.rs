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

    // ── Phase 1: read session turns under the DB lock, then RELEASE it ──
    // The lock must not be held across the (slow, network-bound) LLM extraction
    // below, or every other gateway request would block for the LLM's duration.
    let (turns, turn_ids) = {
        let db_guard = match db.lock() {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!("Pipeline: DB lock failed for session {}: {}", session_id, e);
                return;
            }
        };

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
        // db_guard dropped here — lock released for the LLM call below
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

    // ── Phase 2: extract atoms via LLM WITHOUT holding any lock ──
    let atoms = match crate::memory::l1::extract_atoms(&llm, &turns) {
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

    // ── Phase 3: re-acquire locks for the DB-writing steps (store + graph) ──
    let db_guard = match db.lock() {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!("Pipeline: DB lock failed for session {}: {}", session_id, e);
            return;
        }
    };

    let embedder_guard = embedder.as_ref().and_then(|e| e.lock().ok());
    let embedder_ref: Option<&LazyEmbedder> = embedder_guard.as_deref();

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

    // Store atoms (embedding + admission + dedup + write to bounded_memory)
    let atom_ids = match extractor.store_atoms(&atoms, &turn_ids) {
        Ok(ids) => ids,
        Err(e) => {
            tracing::warn!("Pipeline: store_atoms failed for {}: {}", session_id, e);
            return;
        }
    };

    // 4. Graph integration — create entities + from_session + mentions relations
    let graph_count = integrate_session_atoms(&db_guard, &atoms, &atom_ids, &session_id);

    drop(embedder_guard);
    drop(db_guard);

    tracing::info!(
        "Pipeline complete for session {}: {} atoms stored, {} graph integrations",
        session_id,
        atom_ids.len(),
        graph_count
    );

    // ── Phase 4 (v2.6.1): L2 scenario aggregation (opt-in, best-effort) ──
    if config.scenarios.enabled && !atom_ids.is_empty() {
        run_l2_aggregation(
            db.clone(),
            llm.clone(),
            embedder.clone(),
            config.clone(),
            &atom_ids,
            &session_id,
        );
    }
}



/// Graph integration for this session's stored atoms (extracted from
/// run_pipeline to keep cognitive complexity ≤ 15). Creates entities +
/// from_session + mentions relations per atom; entity names shorter than 2
/// chars are filtered as noise.
fn integrate_session_atoms(
    db: &Db,
    atoms: &[crate::memory::l1::Atom],
    atom_ids: &[i64],
    session_id: &str,
) -> u32 {
    let mut count = 0u32;
    for (i, atom) in atoms.iter().enumerate() {
        if i >= atom_ids.len() {
            break;
        }
        let entities: Vec<String> = atom
            .entities
            .iter()
            .filter(|e| e.chars().count() >= 2)
            .cloned()
            .collect();
        match integrate_atom_with_graph(
            db,
            atom_ids[i],
            &atom.content,
            None,
            Some(session_id),
            &entities,
            &[],
        ) {
            Ok(_) => count += 1,
            Err(e) => tracing::debug!(
                "Pipeline: graph integration failed for atom {}: {}",
                atom_ids[i],
                e
            ),
        }
    }
    count
}
/// L2 scenario aggregation: cluster this session's newly-stored atoms by
/// embedding similarity, summarize each cluster via the LLM, and store each
/// summary as a `memory_type='scenario'` row (so `/recall` L2 surfaces it)
/// plus a human-readable Markdown file under `memory/scenarios/`.
///
/// Lock discipline mirrors `run_pipeline`: read under the DB lock → release
/// for the (slow) embed + LLM calls → re-acquire the DB lock to write.
/// Best-effort: any failure is logged and never propagates.
fn run_l2_aggregation(
    db: Arc<Mutex<Db>>,
    llm: Arc<LlmClient>,
    embedder: Option<Arc<Mutex<LazyEmbedder>>>,
    config: Arc<Config>,
    atom_ids: &[i64],
    session_id: &str,
) {
    if atom_ids.is_empty() {
        return;
    }
    // 1. Re-fetch (id, content) for the stored atoms under the DB lock.
    let stored = fetch_stored_atoms(&db, atom_ids, session_id);
    if stored.is_empty() {
        return;
    }
    // 2. Re-embed (store_atoms computed embeddings internally but doesn't return
    //    them; re-embed is the clean isolated path). Skips if no embedder / too
    //    few non-zero embeddings to cluster.
    let atoms_with_emb = match reembed_for_clustering(
        embedder.as_ref(),
        &stored,
        config.scenarios.min_cluster_size,
        session_id,
    ) {
        Some(a) => a,
        None => return,
    };
    // 3. Aggregate (LLM) — no DB lock held.
    let scenarios_dir = config.memory_dir().join("scenarios");
    let aggregator = crate::memory::scenario::ScenarioAggregator::new(&llm, &scenarios_dir, &config.pipeline);
    let scenarios = match aggregator.aggregate(
        &atoms_with_emb,
        config.scenarios.similarity_threshold,
        config.scenarios.min_cluster_size,
    ) {
        Ok(s) if !s.is_empty() => s,
        Ok(_) => {
            tracing::debug!("L2: no scenarios formed for {}", session_id);
            return;
        }
        Err(e) => {
            tracing::warn!("L2: aggregate failed for {}: {}", session_id, e);
            return;
        }
    };
    // 4+5. Write scenario .md files (no DB lock) + scenario rows (transaction,
    //      dedup, cap-evict) under the DB lock.
    let (written, skipped_dup) = write_scenarios(&db, &config, &aggregator, &scenarios, session_id);
    tracing::info!(
        "Pipeline L2: {} scenarios stored, {} duplicates skipped for session {}",
        written,
        skipped_dup,
        session_id
    );
}

/// Re-fetch (id, content) for the stored atoms under the DB lock.
fn fetch_stored_atoms(
    db: &Arc<Mutex<Db>>,
    atom_ids: &[i64],
    session_id: &str,
) -> Vec<(i64, String)> {
    if atom_ids.is_empty() {
        return Vec::new();
    }
    let db_guard = match db.lock() {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!("L2: DB lock failed for {}: {}", session_id, e);
            return Vec::new();
        }
    };
    let placeholders: Vec<String> = (1..=atom_ids.len()).map(|i| format!("?{}", i)).collect();
    let sql = format!(
        "SELECT id, content FROM bounded_memory WHERE id IN ({})",
        placeholders.join(", ")
    );
    let mut stmt = match db_guard.conn().prepare(&sql) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("L2: prepare fetch failed for {}: {}", session_id, e);
            return Vec::new();
        }
    };
    let params: Vec<Box<dyn rusqlite::types::ToSql>> = atom_ids
        .iter()
        .map(|id| Box::new(*id) as Box<dyn rusqlite::types::ToSql>)
        .collect();
    let refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
    stmt.query_map(refs.as_slice(), |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
    })
    .ok()
    .map(|rows| rows.filter_map(|r| r.ok()).collect())
    .unwrap_or_default()
}

/// Re-embed the stored atoms' content (as documents, not queries, for
/// clustering parity). Returns None if no embedder is available, the embed
/// fails, or too few non-zero embeddings remain to form a cluster.
fn reembed_for_clustering(
    embedder: Option<&Arc<Mutex<LazyEmbedder>>>,
    stored: &[(i64, String)],
    min_cluster: usize,
    session_id: &str,
) -> Option<Vec<(i64, String, Vec<f32>)>> {
    let embedder_guard = embedder.and_then(|e| e.lock().ok())?;
    let contents: Vec<&str> = stored.iter().map(|(_, c)| c.as_str()).collect();
    let embeddings = match embedder_guard.embed_documents(&contents) {
        Ok(e) => e,
        Err(e) => {
            tracing::debug!("L2: embed failed for {}: {}", session_id, e);
            return None;
        }
    };
    let out: Vec<(i64, String, Vec<f32>)> = stored
        .iter()
        .zip(embeddings.iter())
        .filter(|(_, e)| e.iter().any(|&v| v != 0.0))
        .map(|((id, c), e)| (*id, c.clone(), e.clone()))
        .collect();
    if out.len() < min_cluster {
        tracing::debug!("L2: too few embedded atoms ({}) for {}", out.len(), session_id);
        return None;
    }
    Some(out)
}

/// Write scenario .md files (no DB lock — file I/O is independent of the
/// recall source rows) + scenario rows under the DB lock in one transaction
/// (dedup by summary, cap-evict oldest beyond `max_scenarios`). Returns
/// (written, duplicates_skipped).
fn write_scenarios(
    db: &Arc<Mutex<Db>>,
    config: &Config,
    aggregator: &crate::memory::scenario::ScenarioAggregator,
    scenarios: &[crate::memory::scenario::Scenario],
    session_id: &str,
) -> (usize, usize) {
    for s in scenarios {
        if let Err(e) = aggregator.save_scenario(s) {
            tracing::debug!("L2: scenario .md save failed: {}", e);
        }
    }
    let db_guard = match db.lock() {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!("L2: DB lock (write) failed for {}: {}", session_id, e);
            return (0, 0);
        }
    };
    let tx = match db_guard.conn().unchecked_transaction() {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!("L2: tx begin failed for {}: {}", session_id, e);
            return (0, 0);
        }
    };
    let mut written = 0usize;
    let mut skipped_dup = 0usize;
    for s in scenarios {
        let exists: bool = tx
            .query_row(
                "SELECT 1 FROM bounded_memory WHERE target='memory' AND memory_type='scenario' AND content = ?1 LIMIT 1",
                rusqlite::params![s.summary],
                |_| Ok(()),
            )
            .is_ok();
        if exists {
            skipped_dup += 1;
            continue;
        }
        match tx.execute(
            "INSERT INTO bounded_memory (target, content, created_at, updated_at, confidence, memory_type, source_session) \
             VALUES ('memory', ?1, ?2, ?2, 'medium', 'scenario', ?3)",
            rusqlite::params![s.summary, s.created_at, session_id],
        ) {
            Ok(_) => written += 1,
            Err(e) => tracing::warn!("L2: scenario row insert failed: {}", e),
        }
    }
    // Cap: evict oldest scenario rows beyond max_scenarios (scenarios bypass
    // the atom budget, so without this they grow unbounded and squeeze atoms).
    let cap = config.scenarios.max_scenarios as i64;
    if let Err(e) = tx.execute(
        "DELETE FROM bounded_memory WHERE target='memory' AND memory_type='scenario' AND id NOT IN \
         (SELECT id FROM bounded_memory WHERE target='memory' AND memory_type='scenario' \
          ORDER BY updated_at DESC LIMIT ?1)",
        rusqlite::params![cap],
    ) {
        tracing::debug!("L2: scenario cap-evict failed: {}", e);
    }
    if let Err(e) = tx.commit() {
        tracing::warn!("L2: tx commit failed for {}: {}", session_id, e);
    }
    (written, skipped_dup)
}

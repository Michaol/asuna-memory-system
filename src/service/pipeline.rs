//! Post-session pipeline: L1 atom extraction → graph integration
//!
//! Triggered asynchronously from `/session/end` via `tokio::task::spawn_blocking`.
//! All operations (LLM calls, DB queries) are synchronous, so the entire pipeline
//! runs as a blocking task to avoid starving the tokio runtime.

use crate::config::Config;
use crate::embedder::LazyEmbedder;
use crate::index::db::Db;
use crate::memory::admission::AdmissionScorer;
use crate::memory::graph_integration::integrate_atom_with_graph;
use crate::memory::l1::{L1Extractor, StoredAtom, TurnContent};
use crate::memory::llm::LlmClient;
use crate::transport::http::recover_poison;
use std::sync::{Arc, Mutex};

/// Run the post-session extraction + graph pipeline.
///
/// Designed to run inside `tokio::task::spawn_blocking`. Steps:
/// 1. Read session turns from DB
/// 2. Extract atomic facts via LLM (L1Extractor)
/// 3. Store atoms with embedding + admission scoring — split into 3a
///    prepare (short DB lock) / 3b embed (embedder mutex only) + score
///    (no lock at all) / 3c commit (short DB lock) so no gateway handler
///    is blocked behind this batch's embedding / admission-LLM calls
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
        // U19: recover a poisoned DB mutex (self-heal, see http::acquire_db's
        // statement-atomicity argument) instead of stranding every future
        // pipeline run on a permanent warn-and-skip.
        let db_guard = match db.lock() {
            Ok(d) => d,
            Err(e) => {
                tracing::error!(
                    "Pipeline: DB lock poisoned for session {}, recovering: {}",
                    session_id,
                    e
                );
                e.into_inner()
            }
        };

        let mut stmt = match db_guard
            .conn()
            .prepare("SELECT id, role, preview FROM turns WHERE session_id = ?1 ORDER BY seq")
        {
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

    // ── Phase 3: store atoms — split into short-lock / lock-free / short-lock ──
    // (C3/C12/U14/U25) The old code held the GLOBAL DB mutex across
    // store_atoms' whole network stage (per-atom embedding + per-atom
    // admission LLM calls), blocking every other gateway handler for the
    // batch's duration. The StorePlan stages mirror the release-before-
    // network discipline Phase 1 and run_l2_aggregation already use:
    //   3a short DB lock     → snapshot read
    //   3b embedder mutex    → embedding only (network)
    //      NO lock           → admission scoring (per-atom LLM, NB2/NB9:
    //                          never hold the embedder mutex across it)
    //   3c short DB lock     → transactional insert + graph integration
    // Lock scopes here do not overlap (3b holds only the embedder mutex,
    // 3a/3c only the DB mutex), so the global db→embedder lock order is
    // trivially respected.

    // 3a: snapshot existing contents/embeddings under the DB lock, then
    // RELEASE it (StorePlan borrows the atoms slice, never the Db).
    let mut plan = {
        let db_guard = match db.lock() {
            Ok(d) => d,
            Err(e) => {
                tracing::error!(
                    "Pipeline: DB lock poisoned for session {}, recovering: {}",
                    session_id,
                    e
                );
                e.into_inner()
            }
        };
        let db_ref: &Db = &db_guard;
        let extractor = L1Extractor::new(db_ref, &llm, None);
        match extractor.prepare_store(&atoms, &turn_ids) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!("Pipeline: prepare_store failed for {}: {}", session_id, e);
                return;
            }
        }
        // db_guard dropped here — lock released for the network stage below
    };

    // 3b: embedding WITHOUT the DB lock, then admission scoring with NO lock
    // at all. The embedder mutex (shared with /capture and /search) covers
    // ONLY the embedding call — admission scoring is per-atom LLM network
    // work (NB2/NB9: it must not queue other handlers' embeds behind it).
    // A batch- or per-atom embedder failure degrades to skipping single
    // atoms (C13) inside execute_embed, not to losing the whole session's
    // L1 memory.
    let embed_result = {
        let embedder_guard = embedder.as_ref().map(|e| recover_poison(e));
        plan.execute_embed(embedder_guard.as_deref())
        // embedder_guard dropped at the end of this block — scoring runs lock-free
    };
    if let Err(e) = embed_result {
        tracing::warn!("Pipeline: embed stage failed for {}: {}", session_id, e);
        return;
    }
    let admission_scorer: Option<AdmissionScorer<'_>> = if config.admission.enabled {
        Some(AdmissionScorer::new(&config.admission, Some(&llm)))
    } else {
        None
    };
    if let Err(e) = plan.execute_score(admission_scorer.as_ref()) {
        tracing::warn!(
            "Pipeline: admission scoring failed for {}: {}",
            session_id,
            e
        );
        return;
    }

    // 3c: re-acquire the DB lock ONLY for the transactional writes
    // (insert + audits + graph). commit_store re-runs the exact-text guard
    // against the live table to close the race window 3b opened.
    let db_guard = match db.lock() {
        Ok(d) => d,
        Err(e) => {
            tracing::error!(
                "Pipeline: DB lock poisoned for session {}, recovering: {}",
                session_id,
                e
            );
            e.into_inner()
        }
    };

    let db_ref: &Db = &db_guard;
    let bounded_memory = crate::growth::bounded_memory::BoundedMemory::new(
        &config.memory_dir(),
        db_ref,
        config.memory.memory_char_limit,
        config.memory.user_char_limit,
    )
    .with_atom_capacity_ratio(config.memory.atom_capacity_ratio);

    // Admission scoring already happened in 3b; the commit path is DB-only
    // work, so this extractor needs neither the embedder nor the scorer.
    let extractor = L1Extractor::new(db_ref, &llm, None).with_growth(bounded_memory);

    // Store atoms (dedup + write to bounded_memory).
    // `stored` only contains atoms that actually reached bounded_memory; the
    // id list must be derived from it (never positional pairing with `atoms`).
    let stored = match extractor.commit_store(&mut plan) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("Pipeline: commit_store failed for {}: {}", session_id, e);
            return;
        }
    };
    let atom_ids: Vec<i64> = stored.iter().map(|s| s.id).collect();

    // 4. Graph integration — create entities + from_session + mentions relations
    let graph_count = integrate_session_atoms(&db_guard, &atoms, &stored, &session_id);

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
/// run_pipeline to keep cognitive complexity ≤ 15). Per StoredAtom: create
/// entities + from_session + mentions relations and, for conflict
/// replacements, the supersedes edge; entity names shorter than 2 chars are
/// filtered as noise. Atoms are paired with their input batch entry via
/// `StoredAtom.source_index` — positional pairing would mis-attribute every
/// entry after a skipped (deduplicated/rejected) atom.
fn integrate_session_atoms(
    db: &Db,
    atoms: &[crate::memory::l1::Atom],
    stored: &[StoredAtom],
    session_id: &str,
) -> u32 {
    let mut count = 0u32;
    for s in stored {
        let Some(atom) = atoms.get(s.source_index) else {
            tracing::warn!(
                "Pipeline: stored atom {} has source_index {} outside the {}-atom batch; skipping graph integration",
                s.id,
                s.source_index,
                atoms.len()
            );
            continue;
        };
        let entities: Vec<String> = atom
            .entities
            .iter()
            .filter(|e| e.chars().count() >= 2)
            .cloned()
            .collect();
        match integrate_atom_with_graph(
            db,
            s.id,
            &atom.content,
            s.supersedes_id,
            Some(session_id),
            &entities,
            &[],
        ) {
            Ok(_) => count += 1,
            Err(e) => tracing::debug!(
                "Pipeline: graph integration failed for atom {}: {}",
                s.id,
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
    let aggregator =
        crate::memory::scenario::ScenarioAggregator::new(&llm, &scenarios_dir, &config.pipeline);
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
    // 4+5. Write scenario rows (transaction, dedup, cap-evict) under the DB
    //      lock, then sync the .md mirrors outside it (J12).
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
            tracing::error!("L2: DB lock poisoned for {}, recovering: {}", session_id, e);
            e.into_inner()
        }
    };
    let placeholders: Vec<String> = (1..=atom_ids.len()).map(|i| format!("?{}", i)).collect();
    // C14-b: a row stored earlier in this batch may already be superseded by a
    // later one (same-batch conflict chain) — superseded facts must not be
    // aggregated into an L2 scenario, same as every other recall surface.
    let sql = format!(
        "SELECT bm.id, bm.content FROM bounded_memory bm
         WHERE bm.id IN ({})
           AND NOT EXISTS (SELECT 1 FROM bounded_memory s WHERE s.supersedes_id = bm.id)",
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
    let embedder_guard = embedder.map(|e| recover_poison(e))?;
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
        tracing::debug!(
            "L2: too few embedded atoms ({}) for {}",
            out.len(),
            session_id
        );
        return None;
    }
    Some(out)
}

/// Write scenario rows under the DB lock in one transaction (dedup by
/// summary, cap-evict oldest beyond `max_scenarios`), then synchronize the
/// human-readable `.md` mirrors outside the lock.
///
/// J12: mirrors are written only for rows that were actually inserted (a
/// duplicate summary no longer drops a file), filenames
/// `{created_at}_{db_id}.md` map 1:1 back to the DB row, and cap-evicted
/// rows have their mirror file removed (best-effort: failures warn but
/// never fail the write). Returns (written, duplicates_skipped).
fn write_scenarios(
    db: &Arc<Mutex<Db>>,
    config: &Config,
    aggregator: &crate::memory::scenario::ScenarioAggregator,
    scenarios: &[crate::memory::scenario::Scenario],
    session_id: &str,
) -> (usize, usize) {
    let db_guard = match db.lock() {
        Ok(d) => d,
        Err(e) => {
            tracing::error!(
                "L2: DB lock (write) poisoned for {}, recovering: {}",
                session_id,
                e
            );
            e.into_inner()
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
    // (index into `scenarios`, DB row id) — mirror files are written after
    // the commit, keyed by the id so cap-eviction can delete them again.
    let mut inserted: Vec<(usize, i64)> = Vec::new();
    for (i, s) in scenarios.iter().enumerate() {
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
            Ok(_) => {
                written += 1;
                inserted.push((i, tx.last_insert_rowid()));
            }
            Err(e) => tracing::warn!("L2: scenario row insert failed: {}", e),
        }
    }
    // Cap: evict oldest scenario rows beyond max_scenarios (scenarios bypass
    // the atom budget, so without this they grow unbounded and squeeze atoms).
    // The doomed rows are SELECTed first (id, created_at) so their mirror
    // files can be removed in lock-step once the DELETE succeeded (J12).
    let cap = config.scenarios.max_scenarios as i64;
    let evict_predicate = "target='memory' AND memory_type='scenario' AND id NOT IN \
         (SELECT id FROM bounded_memory WHERE target='memory' AND memory_type='scenario' \
          ORDER BY updated_at DESC LIMIT ?1)"
        .to_string();
    let mut select_failed = false;
    let mut evicted: Vec<(i64, i64)> = match tx.prepare(&format!(
        "SELECT id, created_at FROM bounded_memory WHERE {}",
        evict_predicate
    )) {
        Ok(mut stmt) => match stmt.query_map(rusqlite::params![cap], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
        }) {
            Ok(rows) => rows.filter_map(|r| r.ok()).collect(),
            Err(e) => {
                tracing::debug!("L2: scenario cap-evict select failed: {}", e);
                select_failed = true;
                Vec::new()
            }
        },
        Err(e) => {
            tracing::debug!("L2: scenario cap-evict select prepare failed: {}", e);
            select_failed = true;
            Vec::new()
        }
    };
    if select_failed {
        // Doomed rows could not be enumerated, so mirror deletion cannot be
        // paired. Still enforce the cap with a blind DELETE (the pre-J12
        // behavior) — losing mirror pairing is better than letting rows grow
        // unbounded; warn so orphaned mirrors are diagnosable.
        match tx.execute(
            &format!("DELETE FROM bounded_memory WHERE {}", evict_predicate),
            rusqlite::params![cap],
        ) {
            Ok(_) => tracing::warn!(
                "L2: scenario cap-evict ran blind (doomed-row SELECT failed); evicted mirrors may orphan"
            ),
            Err(e) => tracing::debug!("L2: scenario cap-evict failed: {}", e),
        }
    } else if !evicted.is_empty() {
        match tx.execute(
            &format!("DELETE FROM bounded_memory WHERE {}", evict_predicate),
            rusqlite::params![cap],
        ) {
            Ok(_) => {}
            Err(e) => {
                tracing::debug!("L2: scenario cap-evict failed: {}", e);
                // Rows survived the DELETE attempt — keep their mirrors.
                evicted.clear();
            }
        }
    }
    let committed = match tx.commit() {
        Ok(()) => true,
        Err(e) => {
            tracing::warn!("L2: tx commit failed for {}: {}", session_id, e);
            false
        }
    };
    // Release the DB lock before any file I/O (the pre-J12 code wrote files
    // before acquiring the lock; the same "no file I/O under lock" discipline
    // is kept, just after the rows exist).
    drop(db_guard);

    if committed {
        for (i, row_id) in &inserted {
            if let Err(e) = aggregator.save_scenario(&scenarios[*i], *row_id) {
                tracing::debug!("L2: scenario .md save failed: {}", e);
            }
        }
        let scenarios_dir = config.memory_dir().join("scenarios");
        for (row_id, created_at) in &evicted {
            let path = scenarios_dir.join(format!("{}_{}.md", created_at, row_id));
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                // Already-missing mirror (or a legacy `{created_at}_{title}.md`
                // from before J12, which this scheme cannot name): nothing to do.
                Err(ref e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => tracing::warn!(
                    "L2: scenario .md evict failed for {}: {}",
                    path.display(),
                    e
                ),
            }
        }
    } else {
        // The commit rolled everything back: writing mirrors for `inserted`
        // would create orphan files (exactly the class J12 removes) and
        // deleting mirrors for `evicted` would destroy files whose rows are
        // still alive. Skip the whole file phase.
        tracing::debug!("L2: skipping .md mirror sync (tx rolled back)");
    }
    (written, skipped_dup)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::canonical::canonicalize;

    fn atom(content: &str, entities: &[&str]) -> crate::memory::l1::Atom {
        crate::memory::l1::Atom {
            content: content.to_string(),
            atom_type: "fact".to_string(),
            confidence: 0.9,
            entities: entities.iter().map(|e| e.to_string()).collect(),
        }
    }

    fn insert_atom_row(db: &Db, content: &str) -> i64 {
        db.conn()
            .execute(
                "INSERT INTO bounded_memory (target, content, created_at, updated_at, confidence, memory_type)
                 VALUES ('memory', ?1, 0, 0, 'medium', 'atom')",
                rusqlite::params![content],
            )
            .unwrap();
        db.conn().last_insert_rowid()
    }

    fn atom_entity_name(db: &Db, atom_id: i64) -> String {
        db.conn()
            .query_row(
                "SELECT name FROM entities WHERE canonical = ?1",
                rusqlite::params![canonicalize(&format!("atom_{}", atom_id))],
                |r| r.get(0),
            )
            .unwrap()
    }

    fn mention_targets(db: &Db, atom_id: i64) -> Vec<String> {
        let mut stmt = db
            .conn()
            .prepare(
                "SELECT dst_canonical FROM relations
                 WHERE src_canonical = ?1 AND rel_type = 'mentions'
                 ORDER BY dst_canonical",
            )
            .unwrap();
        stmt.query_map(
            rusqlite::params![canonicalize(&format!("atom_{}", atom_id))],
            |r| r.get::<_, String>(0),
        )
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
    }

    /// Regression (graph index misalignment): store_atoms returns only the
    /// atoms that were actually stored, so a skipped middle atom shifted the
    /// old positional `atoms[i]` ↔ `atom_ids[i]` pairing — B's content/entities
    /// silently hung off C's bounded_memory row. Integration must pair via
    /// `StoredAtom.source_index`, and (J40) must forward `supersedes_id` so the
    /// supersedes edge is finally created on the production path.
    #[test]
    fn integrate_session_atoms_pairs_by_source_index_and_supersedes() {
        let db = Db::open_memory().unwrap();
        db.init_schema().unwrap();

        let atoms = vec![
            atom("Atom alpha content", &["AlphaEntity"]),
            atom("Atom beta content", &["BetaEntity"]),
            atom("Atom gamma content", &["GammaEntity"]),
        ];

        // B was skipped by store_atoms: only A and C have bounded_memory rows.
        let id_a = insert_atom_row(&db, &atoms[0].content);
        let id_c = insert_atom_row(&db, &atoms[2].content);
        let stored = vec![
            StoredAtom {
                source_index: 0,
                id: id_a,
                supersedes_id: None,
            },
            StoredAtom {
                source_index: 2,
                id: id_c,
                supersedes_id: Some(id_a),
            },
        ];

        let count = integrate_session_atoms(&db, &atoms, &stored, "sess-42");
        assert_eq!(count, 2);

        // Canonical atom entities carry the PAIRED atom's content.
        assert_eq!(atom_entity_name(&db, id_a), "Atom alpha content");
        assert_eq!(atom_entity_name(&db, id_c), "Atom gamma content");

        // mentions edges attach to the right atoms; the skipped atom's entity
        // must not be linked to any stored atom.
        assert_eq!(
            mention_targets(&db, id_a),
            vec![canonicalize("AlphaEntity")]
        );
        assert_eq!(
            mention_targets(&db, id_c),
            vec![canonicalize("GammaEntity")]
        );
        let beta_mentions: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM relations WHERE rel_type = 'mentions' AND dst_canonical = ?1",
                rusqlite::params![canonicalize("BetaEntity")],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            beta_mentions, 0,
            "skipped atom's entities must stay unlinked"
        );

        // J40: supersedes edge atom_c → atom_a driven by StoredAtom.supersedes_id.
        let supersedes: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM relations
                 WHERE rel_type = 'supersedes' AND src_canonical = ?1 AND dst_canonical = ?2",
                rusqlite::params![
                    canonicalize(&format!("atom_{}", id_c)),
                    canonicalize(&format!("atom_{}", id_a))
                ],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(supersedes, 1);

        // from_session edges exist for both stored atoms.
        for id in [id_a, id_c] {
            let fs: i64 = db
                .conn()
                .query_row(
                    "SELECT COUNT(*) FROM relations
                     WHERE rel_type = 'from_session' AND src_canonical = ?1 AND dst_canonical = ?2",
                    rusqlite::params![
                        canonicalize(&format!("atom_{}", id)),
                        canonicalize("session_sess-42")
                    ],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(fs, 1);
        }
    }

    /// Defensive: an out-of-range source_index must warn and skip, never panic.
    #[test]
    fn integrate_session_atoms_skips_out_of_range_source_index() {
        let db = Db::open_memory().unwrap();
        db.init_schema().unwrap();

        let a = atom("Only atom content", &["OnlyEntity"]);
        let id_a = insert_atom_row(&db, &a.content);
        let stored = vec![StoredAtom {
            source_index: 9,
            id: id_a,
            supersedes_id: None,
        }];

        let count = integrate_session_atoms(&db, std::slice::from_ref(&a), &stored, "s1");
        assert_eq!(count, 0);
    }

    /// Defensive: when the superseded atom's row AND entity are both gone
    /// (e.g. capacity-evicted before integration ran), the supersedes edge is
    /// skipped best-effort — it must not FK-fail and roll back the new atom's
    /// whole graph integration.
    #[test]
    fn integrate_atom_survives_missing_supersedes_target() {
        let db = Db::open_memory().unwrap();
        db.init_schema().unwrap();

        let a = atom("Replacement atom content", &["ReplacementEntity"]);
        let id_a = insert_atom_row(&db, &a.content);
        // id 4242 has no bounded_memory row and no entity: stale supersedes ref.
        let stored = vec![StoredAtom {
            source_index: 0,
            id: id_a,
            supersedes_id: Some(4242),
        }];

        let count = integrate_session_atoms(&db, std::slice::from_ref(&a), &stored, "s2");
        assert_eq!(count, 1, "integration must succeed despite stale target");

        // The atom's own entity + mentions + from_session were still created.
        assert_eq!(atom_entity_name(&db, id_a), "Replacement atom content");
        assert_eq!(
            mention_targets(&db, id_a),
            vec![canonicalize("ReplacementEntity")]
        );
        // The dangling supersedes edge was skipped, not inserted.
        let supersedes: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM relations WHERE rel_type = 'supersedes'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(supersedes, 0);
    }

    /// C14-b: the L2 scenario surface reads atoms back via fetch_stored_atoms;
    /// a row superseded (same-batch conflict chain) must not be clustered into
    /// a scenario summary alongside its replacement.
    #[test]
    fn fetch_stored_atoms_excludes_superseded() {
        let db = std::sync::Arc::new(std::sync::Mutex::new(Db::open_memory().unwrap()));
        db.lock().unwrap().init_schema().unwrap();

        let old_id = {
            let d = db.lock().unwrap();
            insert_atom_row(&d, "old scenario fact")
        };
        let new_id = {
            let d = db.lock().unwrap();
            d.conn()
                .execute(
                    "INSERT INTO bounded_memory (target, content, created_at, updated_at, confidence, memory_type, supersedes_id) \
                     VALUES ('memory', 'new scenario fact', 0, 0, 'medium', 'atom', ?1)",
                    rusqlite::params![old_id],
                )
                .unwrap();
            d.conn().last_insert_rowid()
        };

        let ids = vec![old_id, new_id, 999_999];
        let fetched = fetch_stored_atoms(&db, &ids, "sess-x");
        let got: Vec<i64> = fetched.iter().map(|(id, _)| *id).collect();
        assert_eq!(
            got,
            vec![new_id],
            "superseded row filtered, ghost id absent"
        );
    }

    // ── J12: L2 mirror-file lifecycle follows the DB rows ──

    /// In-memory DB + config whose memory_dir points into `tmp`
    /// (`max_scenarios` drives the cap-evict under test).
    fn l2_write_fixture(tmp: &tempfile::TempDir, max_scenarios: usize) -> (Arc<Mutex<Db>>, Config) {
        let db = Arc::new(Mutex::new(Db::open_memory().unwrap()));
        db.lock().unwrap().init_schema().unwrap();
        let config = Config {
            data_dir: tmp.path().to_path_buf(),
            scenarios: crate::config::ScenarioConfig {
                max_scenarios,
                ..Default::default()
            },
            ..Config::default()
        };
        (db, config)
    }

    fn scenario_with(summary: &str, created_at: i64) -> crate::memory::scenario::Scenario {
        crate::memory::scenario::Scenario {
            title: format!("标题: {}", summary),
            atom_ids: vec![1, 2],
            summary: summary.to_string(),
            created_at,
            updated_at: created_at,
        }
    }

    fn scenario_files(dir: &std::path::Path) -> Vec<String> {
        if !dir.exists() {
            return Vec::new();
        }
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// J12(1): a duplicate summary is skipped BEFORE the mirror write — the
    /// file must never be dropped for a row that was not inserted. The name
    /// maps 1:1 to the row id (J12(2)).
    #[test]
    fn write_scenarios_dedup_writes_single_mirror() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (db, config) = l2_write_fixture(&tmp, 50);
        let llm = Arc::new(LlmClient::new("test", "test", "test"));
        let dir = config.memory_dir().join("scenarios");
        let aggregator =
            crate::memory::scenario::ScenarioAggregator::new(&llm, &dir, &config.pipeline);

        let s = scenario_with("重复的场景摘要", 1000);
        let (written, dup) = write_scenarios(&db, &config, &aggregator, &[s.clone(), s], "s1");
        assert_eq!((written, dup), (1, 1));
        let files = scenario_files(&dir);
        assert_eq!(
            files.len(),
            1,
            "duplicate summary must not drop a second .md"
        );

        let row_id: i64 = db
            .lock()
            .unwrap()
            .conn()
            .query_row(
                "SELECT id FROM bounded_memory WHERE memory_type='scenario'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(files[0], format!("1000_{}.md", row_id));
    }

    /// J12(3): cap-eviction deletes the mirror file together with the row —
    /// the directory can no longer grow unboundedly.
    #[test]
    fn write_scenarios_cap_evict_removes_mirror_files() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (db, config) = l2_write_fixture(&tmp, 1);
        let llm = Arc::new(LlmClient::new("test", "test", "test"));
        let dir = config.memory_dir().join("scenarios");
        let aggregator =
            crate::memory::scenario::ScenarioAggregator::new(&llm, &dir, &config.pipeline);

        let s1 = scenario_with("场景一", 1000);
        write_scenarios(&db, &config, &aggregator, std::slice::from_ref(&s1), "sA");
        assert_eq!(scenario_files(&dir).len(), 1);

        let s2 = scenario_with("场景二", 2000);
        let (written, dup) = write_scenarios(&db, &config, &aggregator, &[s2], "sB");
        assert_eq!((written, dup), (1, 0));

        let files = scenario_files(&dir);
        assert_eq!(
            files.len(),
            1,
            "evicted row's mirror must disappear with its DB row"
        );
        assert!(
            files[0].starts_with("2000_"),
            "kept mirror must belong to the newest row: {:?}",
            files
        );
        let rows: i64 = db
            .lock()
            .unwrap()
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM bounded_memory WHERE memory_type='scenario'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(rows, 1);
    }

    /// J12(3, orphan-cleanup path): a missing mirror at eviction time (never
    /// written, or deleted externally) is tolerated — no panic, no failure.
    #[test]
    fn write_scenarios_evict_tolerates_missing_mirror() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (db, config) = l2_write_fixture(&tmp, 1);
        let llm = Arc::new(LlmClient::new("test", "test", "test"));
        let dir = config.memory_dir().join("scenarios");
        let aggregator =
            crate::memory::scenario::ScenarioAggregator::new(&llm, &dir, &config.pipeline);

        let s1 = scenario_with("场景一", 1000);
        write_scenarios(&db, &config, &aggregator, std::slice::from_ref(&s1), "sA");
        let orphan = scenario_files(&dir).remove(0);
        std::fs::remove_file(dir.join(&orphan)).unwrap();

        let s2 = scenario_with("场景二", 2000);
        let (written, dup) = write_scenarios(&db, &config, &aggregator, &[s2], "sB");
        assert_eq!((written, dup), (1, 0));
        assert_eq!(scenario_files(&dir).len(), 1);
    }
}

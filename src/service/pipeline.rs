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
/// 5. Phase 4 (`scenarios.enabled`): L2 scenario aggregation over the
///    session's stored atoms
/// 6. Phase 4b (`scenarios.enabled` + `persona.trigger_every_n > 0`):
///    L3-L5 consolidation refresh (persona / mental models / intents)
///
/// Note: `graph.enabled` (checked before step 1) gates the WHOLE pipeline,
/// not just step 4 — disabling the graph also stops L1 extraction, L2 and
/// the L3-L5 refresh.
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

    // ── Phase 4b (S14b/S14c): L3-L5 consolidation refresh (opt-in, best-effort) ──
    // Deliberately not gated on this session producing atoms or L2 output:
    // the trigger counts sessions touched since the last persona, so even a
    // session that yielded nothing still advances toward the threshold.
    if config.scenarios.enabled && config.persona.trigger_every_n > 0 {
        run_consolidation(db.clone(), llm.clone(), config.clone(), &session_id, &turns);
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

// ════════════════════════════════════════════════════════════════════════
// S14b/S14c: L3-L5 consolidation (Phase 4b)
//
// Pure file surface by design: persona (L3) is written ONLY to
// `memory/persona.md`, the mental models (L4) ONLY to
// `memory/mental_models/*.md` and the intent predictions (L5) ONLY to
// `memory/intent/*.md`. None of them ever writes a `bounded_memory` row —
// the user face belongs to the manual-entry mechanism (USER.md reconcile +
// user_char_limit budget), and double-writing there would fight those
// invariants. Consumers read these files through `/recall` (L3 via the
// fallback chain, L4/L5 via the fresh-document layers; memory/retrieval.rs)
// and the `/persona` endpoint (L3, transport/http.rs).
//
// One trigger gates the whole cycle (persona.trigger_every_n sessions since
// the last persona write — see the PersonaConfig docs); inside a cycle the
// steps run in order persona → L4 → L5, each independently best-effort: a
// step's failure warns and the following steps still attempt their refresh.
// ════════════════════════════════════════════════════════════════════════

/// How many most-recent `memory_type='scenario'` rows feed the L3 persona
/// generator. A fixed constant (no config field): one line per scenario in
/// the prompt, and the DB cap (`scenarios.max_scenarios`, default 50) keeps
/// 20 well within any provider's context budget. Newest-first because a
/// user persona is about who they are NOW.
const PERSONA_INPUT_SCENARIOS: i64 = 20;

/// How many most-recent non-superseded `memory_type='atom'` rows feed the
/// L4/L5 generators. A fixed constant (no config field), mirroring
/// `PERSONA_INPUT_SCENARIOS`. Note each generator renders only a prefix of
/// the list it is given (L4's context takes the first 20 entries, L5's the
/// first 15), so 30 keeps the freshest atoms available across the shared
/// input (the digest + rows) without an unbounded prompt; the surplus never
/// reaches a prompt.
const CONSOLIDATION_INPUT_ATOMS: i64 = 30;

/// Trigger predicate (pure, unit-tested): run the L3-L5 consolidation cycle
/// once at least `trigger_every_n` sessions have been touched since the last
/// persona write. `trigger_every_n <= 0` disables the whole cycle — 0 is the
/// documented "off" value of `PersonaConfig::trigger_every_n`.
fn persona_due(sessions_since: i64, trigger_every_n: i64) -> bool {
    trigger_every_n > 0 && sessions_since >= trigger_every_n
}

/// Narrow read view of `persona.md` frontmatter (S14a write format,
/// serde_yaml): only `updated_at` matters to the trigger.
#[derive(serde::Deserialize)]
struct PersonaTimestamp {
    updated_at: i64,
}

/// Narrow read view of a scenario mirror's frontmatter: only `title`.
#[derive(serde::Deserialize)]
struct MirrorTitle {
    title: String,
}

/// Extract the YAML frontmatter between a leading "---" line and the next
/// line-boundary "---". Line-based on purpose: a plain `split("---")` would
/// silently truncate a YAML scalar containing "---" (e.g. `title: a---b`
/// parses as title "a" instead of erroring) — NB5 of the S14b review.
fn frontmatter_block(content: &str) -> Option<&str> {
    let rest = content.strip_prefix("---\n")?;
    let end = rest.find("\n---")?;
    Some(&rest[..end])
}

/// "When was the persona last generated" (unix ms), the window boundary for
/// the session count. Priority: frontmatter `updated_at` → file mtime
/// (hand-written or corrupt file) → 0 when the file is missing/unstatable
/// (= "never generated", so every session counts toward the trigger).
fn last_persona_ts(persona_path: &std::path::Path) -> i64 {
    if let Ok(content) = std::fs::read_to_string(persona_path) {
        // S14a write format "---\n<yaml>\n---\n<body>".
        if let Some(fm) = frontmatter_block(&content) {
            if let Ok(ts) = serde_yaml::from_str::<PersonaTimestamp>(fm.trim()) {
                return ts.updated_at;
            }
        }
    }
    persona_path
        .metadata()
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Best-effort scenario title from its `{created_at}_{db_id}.md` mirror.
/// `None` (missing file / unparsable frontmatter) → the caller truncates
/// the row content instead; a title is prompt sugar, never worth a failure.
fn mirror_scenario_title(
    scenarios_dir: &std::path::Path,
    created_at: i64,
    db_id: i64,
) -> Option<String> {
    let content =
        std::fs::read_to_string(scenarios_dir.join(format!("{}_{}.md", created_at, db_id))).ok()?;
    serde_yaml::from_str::<MirrorTitle>(frontmatter_block(&content)?.trim())
        .ok()
        .map(|t| t.title)
}

/// The L3 generation input: the `PERSONA_INPUT_SCENARIOS` most-recent
/// scenario rows under a SHORT DB lock, then — lock released — each row's
/// title resolved from its human-readable mirror (missing/corrupt → the
/// first 30 chars of the row content). Empty when no scenario rows exist.
fn persona_inputs(
    db: &Arc<Mutex<Db>>,
    scenarios_dir: &std::path::Path,
    session_id: &str,
) -> Vec<crate::memory::scenario::Scenario> {
    let rows: Vec<(i64, String, i64, i64)> = {
        let db_guard = match db.lock() {
            Ok(d) => d,
            Err(e) => {
                tracing::error!("L3: DB lock poisoned for {}, recovering: {}", session_id, e);
                e.into_inner()
            }
        };
        let out = db_guard
            .conn()
            .prepare(
                "SELECT id, content, created_at, updated_at FROM bounded_memory \
             WHERE memory_type = 'scenario' ORDER BY updated_at DESC LIMIT ?1",
            )
            .and_then(|mut stmt| {
                stmt.query_map(rusqlite::params![PERSONA_INPUT_SCENARIOS], |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, i64>(3)?,
                    ))
                })
                .map(|rows| rows.filter_map(|r| r.ok()).collect())
            });
        // db_guard dropped at block end — mirror file I/O below is lock-free
        match out {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    "L3: persona input scenario query failed for {}: {}",
                    session_id,
                    e
                );
                return Vec::new();
            }
        }
    };
    rows.into_iter()
        .map(|(id, content, created_at, updated_at)| {
            // char-based (not byte) truncation — CJK-safe, same yardstick
            // as every other length gate in this crate.
            let title = mirror_scenario_title(scenarios_dir, created_at, id)
                .unwrap_or_else(|| content.chars().take(30).collect());
            crate::memory::scenario::Scenario {
                title,
                // Mirrors do carry atom_ids, but PersonaGenerator::generate
                // consumes title + summary only — no reason to parse more.
                atom_ids: Vec::new(),
                summary: content,
                created_at,
                updated_at,
            }
        })
        .collect()
}

/// The L4/L5 generation input: the `CONSOLIDATION_INPUT_ATOMS` most-recent
/// atom rows under a SHORT DB lock (the same NOT EXISTS supersede predicate
/// the recall/L2 surfaces use — contradicted facts must not feed a cognitive
/// abstraction either), mapped into `Scenario`-shaped entries because that
/// is what the generators' `build_context` renders (`- title: summary`);
/// the timestamps have no consumer on this path. Empty when no live atom
/// rows exist (the caller skips L4/L5 — not a failure).
fn consolidation_inputs(
    db: &Arc<Mutex<Db>>,
    session_id: &str,
) -> Vec<crate::memory::scenario::Scenario> {
    let contents: Vec<String> = {
        let db_guard = match db.lock() {
            Ok(d) => d,
            Err(e) => {
                tracing::error!(
                    "L4/L5: DB lock poisoned for {}, recovering: {}",
                    session_id,
                    e
                );
                e.into_inner()
            }
        };
        let out = db_guard
            .conn()
            .prepare(
                "SELECT bm.content FROM bounded_memory bm
                 WHERE bm.memory_type = 'atom'
                   AND NOT EXISTS (SELECT 1 FROM bounded_memory s WHERE s.supersedes_id = bm.id)
                 ORDER BY bm.updated_at DESC LIMIT ?1",
            )
            .and_then(|mut stmt| {
                stmt.query_map(rusqlite::params![CONSOLIDATION_INPUT_ATOMS], |r| {
                    r.get::<_, String>(0)
                })
                .map(|rows| rows.filter_map(|r| r.ok()).collect())
            });
        // db_guard dropped at block end — the LLM calls below run lock-free
        match out {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("L4/L5: atom input query failed for {}: {}", session_id, e);
                return Vec::new();
            }
        }
    };
    contents
        .into_iter()
        .map(|content| crate::memory::scenario::Scenario {
            // Same char-based (CJK-safe) title fallback as persona_inputs.
            title: content.chars().take(30).collect(),
            atom_ids: Vec::new(),
            summary: content,
            created_at: 0,
            updated_at: 0,
        })
        .collect()
}

/// One-line digest of THIS session's turns, prepended to the L4/L5 input:
/// the atom rows already contain this session's extractions (Phase 3 commits
/// before this phase runs), the digest adds the raw dialogue voice the
/// abstraction lost. Last 8 turns, `role: preview` joined by " | ", capped —
/// prompt sugar, so blank input yields `None` rather than an empty entry.
fn session_digest(
    session_id: &str,
    turns: &[TurnContent],
) -> Option<crate::memory::scenario::Scenario> {
    if turns.is_empty() {
        return None;
    }
    let recent = &turns[turns.len().saturating_sub(8)..];
    let joined = recent
        .iter()
        .map(|t| format!("{}: {}", t.role, t.content))
        .collect::<Vec<_>>()
        .join(" | ");
    Some(crate::memory::scenario::Scenario {
        title: format!("current session {}", session_id),
        atom_ids: Vec::new(),
        summary: joined.chars().take(400).collect(),
        created_at: 0,
        updated_at: 0,
    })
}

/// L3 step of the consolidation cycle (S14b). Returns the freshly generated
/// persona (handed to L4 as context) only when generation succeeded — a
/// save-only failure still returns Some (the in-memory persona is valid even
/// if its file mirror did not land). Any failure warns: the cycle continues.
fn refresh_l3_persona(
    db: &Arc<Mutex<Db>>,
    llm: &Arc<LlmClient>,
    memory_dir: &std::path::Path,
    session_id: &str,
) -> Option<crate::memory::persona::Persona> {
    let scenarios = persona_inputs(db, &memory_dir.join("scenarios"), session_id);
    if scenarios.is_empty() {
        // A brand-new installation (sessions but no scenarios yet) — skip,
        // not a failure: nothing warns until L2 starts producing rows.
        tracing::debug!(
            "L3: no scenario rows for persona generation, skipping ({})",
            session_id
        );
        return None;
    }
    // LLM generation + persona.md write — NO DB lock held.
    let generator = crate::memory::persona::PersonaGenerator::new(llm, memory_dir);
    match generator.generate(&scenarios) {
        Ok(persona) => {
            match generator.save_persona(&persona) {
                Ok(path) => tracing::info!(
                    "Pipeline L3: persona regenerated from {} scenarios → {} (session {})",
                    scenarios.len(),
                    path.display(),
                    session_id
                ),
                Err(e) => tracing::warn!("L3: persona.md save failed for {}: {}", session_id, e),
            }
            Some(persona)
        }
        Err(e) => {
            tracing::warn!("L3: persona generation failed for {}: {}", session_id, e);
            None
        }
    }
}

/// L4 step (S14c): three generate → save pairs, each independently
/// best-effort — one document's failure never skips the other two.
fn refresh_l4_mental_models(
    generator: &crate::memory::mental_model::MentalModelGenerator,
    inputs: &[crate::memory::scenario::Scenario],
    persona: Option<&crate::memory::persona::Persona>,
    session_id: &str,
) {
    match generator.generate_workflow_patterns(inputs, persona) {
        Ok(w) => match generator.save_workflow_patterns(&w) {
            Ok(_) => tracing::info!("Pipeline L4: workflow patterns refreshed ({})", session_id),
            Err(e) => tracing::warn!("L4: workflow-patterns.md save failed: {}", e),
        },
        Err(e) => tracing::warn!("L4: workflow patterns generation failed: {}", e),
    }
    match generator.generate_decision_framework(inputs, persona) {
        Ok(d) => match generator.save_decision_framework(&d) {
            Ok(_) => tracing::info!("Pipeline L4: decision framework refreshed ({})", session_id),
            Err(e) => tracing::warn!("L4: decision-framework.md save failed: {}", e),
        },
        Err(e) => tracing::warn!("L4: decision framework generation failed: {}", e),
    }
    match generator.generate_communication_style(inputs, persona) {
        Ok(c) => match generator.save_communication_style(&c) {
            Ok(_) => tracing::info!(
                "Pipeline L4: communication style refreshed ({})",
                session_id
            ),
            Err(e) => tracing::warn!("L4: communication-style.md save failed: {}", e),
        },
        Err(e) => tracing::warn!("L4: communication style generation failed: {}", e),
    }
}

/// L5 step (S14c): both predictions independently best-effort. Runs
/// regardless of how much of L4 succeeded — its context reads whatever L4
/// documents are on disk (generation does not gate on freshness; the
/// consumption side does).
fn refresh_l5_intents(
    predictor: &crate::memory::intent_prediction::IntentPredictor,
    generator: &crate::memory::mental_model::MentalModelGenerator,
    inputs: &[crate::memory::scenario::Scenario],
    session_id: &str,
) {
    match predictor.predict_likely_topics(inputs, generator) {
        Ok(t) => match predictor.save_likely_topics(&t) {
            Ok(_) => tracing::info!("Pipeline L5: likely topics refreshed ({})", session_id),
            Err(e) => tracing::warn!("L5: likely-next-topics.md save failed: {}", e),
        },
        Err(e) => tracing::warn!("L5: likely topics prediction failed: {}", e),
    }
    match predictor.predict_anticipated_needs(inputs, generator) {
        Ok(n) => match predictor.save_anticipated_needs(&n) {
            Ok(_) => tracing::info!("Pipeline L5: anticipated needs refreshed ({})", session_id),
            Err(e) => tracing::warn!("L5: anticipated-needs.md save failed: {}", e),
        },
        Err(e) => tracing::warn!("L5: anticipated needs prediction failed: {}", e),
    }
}

/// L3-L5 consolidation refresh (S14b persona + S14c mental models and
/// intent predictions; opt-in, best-effort). EACH LAYER has its own anchor
/// (S14c NB2/NB4): a layer refreshes once `persona.trigger_every_n`
/// sessions were touched since that layer's own newest output — L3 anchors
/// on persona.md (frontmatter → mtime → 0), L4/L5 on the newest mtime in
/// their document dirs. A layer that persistently fails (or whose inputs
/// are absent) only re-attempts ITSELF; it no longer drags the other
/// layers into a 5-6-call LLM cycle every session. Within a cycle the
/// order stays L3 → L4 → L5 (L4's output feeds L5's context) and each step
/// is independent best-effort.
///
/// Lock discipline mirrors `run_l2_aggregation`: the session counts and the
/// input fetches take short DB locks; every LLM call and every file write
/// runs with NO lock held. The whole phase never fails the pipeline.
fn run_consolidation(
    db: Arc<Mutex<Db>>,
    llm: Arc<LlmClient>,
    config: Arc<Config>,
    session_id: &str,
    turns: &[TurnContent],
) {
    let memory_dir = config.memory_dir();
    let trigger_every_n = i64::try_from(config.persona.trigger_every_n).unwrap_or(i64::MAX);

    let l3_ts = last_persona_ts(&memory_dir.join("persona.md"));
    let l4_ts = last_dir_mtime(&crate::memory::mental_model::docs_dir(&memory_dir));
    let l5_ts = last_dir_mtime(&crate::memory::intent_prediction::docs_dir(&memory_dir));

    // 1. Sessions touched since each layer's anchor — one COUNT pass, one
    //    short DB lock.
    let (since_l3, since_l4, since_l5) = {
        let db_guard = match db.lock() {
            Ok(d) => d,
            Err(e) => {
                tracing::error!(
                    "L3-L5: DB lock poisoned for {}, recovering: {}",
                    session_id,
                    e
                );
                e.into_inner()
            }
        };
        let result = db_guard.conn().query_row(
            "SELECT COALESCE(SUM(updated_at > ?1), 0), \
                    COALESCE(SUM(updated_at > ?2), 0), \
                    COALESCE(SUM(updated_at > ?3), 0) \
             FROM sessions",
            rusqlite::params![l3_ts, l4_ts, l5_ts],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            },
        );
        // db_guard dropped at block end — the LLM calls must not run under it
        match result {
            Ok(counts) => counts,
            Err(e) => {
                tracing::warn!("L3-L5: session-count failed for {}: {}", session_id, e);
                return;
            }
        }
    };
    let l3_due = persona_due(since_l3, trigger_every_n);
    let l4_due = persona_due(since_l4, trigger_every_n);
    let l5_due = persona_due(since_l5, trigger_every_n);
    if !l3_due && !l4_due && !l5_due {
        return;
    }

    // 2. L3 (persona.md). Its failure or input-less skip must not gate L4/L5.
    let persona = if l3_due {
        refresh_l3_persona(&db, &llm, &memory_dir, session_id)
    } else {
        None
    };

    if !l4_due && !l5_due {
        return;
    }

    // 3. L4 + L5 share the atom-row input (plus this session's digest).
    let mut inputs = consolidation_inputs(&db, session_id);
    if inputs.is_empty() {
        tracing::debug!(
            "L4/L5: no live atom rows for consolidation input, skipping ({})",
            session_id
        );
        return;
    }
    if let Some(digest) = session_digest(session_id, turns) {
        inputs.insert(0, digest);
    }
    // LLM generation + document writes for L4/L5 — NO DB lock held. The L5
    // context reads the L4 docs back off disk via the generator's loaders,
    // so construct it once and share it (same memory_dir contract) even when
    // only one of the two layers is due.
    let mm =
        crate::memory::mental_model::MentalModelGenerator::new(llm.clone(), memory_dir.clone());
    if l4_due {
        refresh_l4_mental_models(&mm, &inputs, persona.as_ref(), session_id);
    }
    if l5_due {
        let predictor =
            crate::memory::intent_prediction::IntentPredictor::new(llm.clone(), memory_dir);
        refresh_l5_intents(&predictor, &mm, &inputs, session_id);
    }
}

/// Newest file mtime (unix ms) in a directory; 0 when missing, empty or
/// unstatable (= "never generated", so every session counts toward the
/// layer's trigger). Anchor for the L4/L5 consolidation cycles (S14c).
fn last_dir_mtime(dir: &std::path::Path) -> i64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|e| e.metadata().ok())
        .filter(std::fs::Metadata::is_file)
        .filter_map(|m| m.modified().ok())
        .filter_map(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .max()
        .unwrap_or(0)
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

    // ── S14b: L3 persona refresh ──

    #[test]
    fn last_dir_mtime_zero_for_missing_and_max_of_files() {
        let tmp = tempfile::TempDir::new().unwrap();
        assert_eq!(last_dir_mtime(&tmp.path().join("nope")), 0, "missing dir");
        let dir = tmp.path().join("docs");
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(last_dir_mtime(&dir), 0, "empty dir");
        let f1 = std::fs::File::create(dir.join("a.md")).unwrap();
        f1.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_millis(1000))
            .unwrap();
        let f2 = std::fs::File::create(dir.join("b.md")).unwrap();
        f2.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_millis(5000))
            .unwrap();
        assert_eq!(last_dir_mtime(&dir), 5000, "max of file mtimes");
    }

    #[test]
    fn persona_due_boundaries() {
        assert!(persona_due(5, 5), "exact threshold fires");
        assert!(!persona_due(4, 5), "one short waits");
        assert!(!persona_due(9, 0), "trigger_every_n=0 is explicitly off");
        assert!(!persona_due(0, 0));
        assert!(!persona_due(3, -1), "negative guard");
        assert!(!persona_due(0, 1), "no sessions, no fire");
    }

    #[test]
    fn last_persona_ts_frontmatter_then_mtime_then_zero() {
        use crate::memory::persona::{Persona, PersonaGenerator};
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("persona.md");
        let llm = LlmClient::new("test", "test", "test");
        let gen = PersonaGenerator::new(&llm, tmp.path());

        // (1) S14a write format: frontmatter updated_at wins (body/times are
        // irrelevant to the trigger, only updated_at is parsed).
        gen.save_persona(&Persona {
            preferences: "p".into(),
            identity: "i".into(),
            workflow: "w".into(),
            tech_stack: "t".into(),
            communication_style: "c".into(),
            created_at: 1000,
            updated_at: 2500,
            supersedes_id: None,
        })
        .unwrap();
        assert_eq!(last_persona_ts(&path), 2500);

        // (2) hand-written file without frontmatter → mtime fallback (fresh
        // write: within a minute of now_unix_ms, ms resolution).
        std::fs::write(&path, "# 手写的画像，没有 frontmatter\n").unwrap();
        let ts = last_persona_ts(&path);
        let now = crate::util::time::now_unix_ms();
        assert!(
            (now - ts).abs() < 60_000,
            "mtime fallback: {ts} vs now {now}"
        );

        // (3) missing file → 0 (= never generated).
        std::fs::remove_file(&path).unwrap();
        assert_eq!(last_persona_ts(&path), 0);
    }

    #[test]
    fn persona_inputs_prefers_mirror_title_and_truncates_without_it() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db = Arc::new(Mutex::new(Db::open_memory().unwrap()));
        db.lock().unwrap().init_schema().unwrap();
        let scenarios_dir = tmp.path().join("scenarios");
        let llm = LlmClient::new("test", "test", "test");
        let pipeline_cfg = crate::config::PipelineConfig::default();
        let aggregator =
            crate::memory::scenario::ScenarioAggregator::new(&llm, &scenarios_dir, &pipeline_cfg);

        // Row A: newest (updated 2000), WITH a mirror whose title contains
        // ": " (the serde_yaml round-trip case from S14a).
        let content_a = "甲场景摘要".to_string();
        let id_a = {
            let d = db.lock().unwrap();
            d.conn()
                .execute(
                    "INSERT INTO bounded_memory (target, content, created_at, updated_at, memory_type) \
                     VALUES ('memory', ?1, 1000, 2000, 'scenario')",
                    rusqlite::params![content_a],
                )
                .unwrap();
            d.conn().last_insert_rowid()
        };
        aggregator
            .save_scenario(
                &crate::memory::scenario::Scenario {
                    title: "带冒号: 的标题".into(),
                    atom_ids: vec![1, 2],
                    summary: content_a.clone(),
                    created_at: 1000,
                    updated_at: 2000,
                },
                id_a,
            )
            .unwrap();

        // Row B: older (updated 1000), NO mirror → title = first 30 chars.
        let content_b =
            "一二三四五六七八九十一二三四五六七八九十一二三四五六七八九十一二三四五六七八九十"; // 40 chars
        {
            let d = db.lock().unwrap();
            d.conn()
                .execute(
                    "INSERT INTO bounded_memory (target, content, created_at, updated_at, memory_type) \
                     VALUES ('memory', ?1, 1500, 1000, 'scenario')",
                    rusqlite::params![content_b],
                )
                .unwrap();
        }

        let inputs = persona_inputs(&db, &scenarios_dir, "s1");
        assert_eq!(inputs.len(), 2);
        assert_eq!(
            inputs[0].title, "带冒号: 的标题",
            "mirror frontmatter title, newest row first"
        );
        assert_eq!(inputs[0].summary, content_a);
        assert_eq!(inputs[0].created_at, 1000);
        assert_eq!(
            inputs[1].title,
            content_b.chars().take(30).collect::<String>(),
            "char-based (CJK-safe) truncation without mirror"
        );
        assert_eq!(
            inputs[1].atom_ids,
            Vec::<i64>::new(),
            "persona input carries no atom_ids"
        );
    }

    /// NB5 regression: a mirror title scalar containing "---" must parse in
    /// FULL — the old `split("---").nth(1)` silently truncated `title: a---b`
    /// to "a"; frontmatter_block extracts by line boundary instead.
    #[test]
    fn mirror_title_with_embedded_dashes_parses_fully() {
        let tmp = tempfile::TempDir::new().unwrap();
        let llm = LlmClient::new("test", "test", "test");
        let pipeline_cfg = crate::config::PipelineConfig::default();
        let aggregator =
            crate::memory::scenario::ScenarioAggregator::new(&llm, tmp.path(), &pipeline_cfg);
        aggregator
            .save_scenario(
                &crate::memory::scenario::Scenario {
                    title: "部署---生产环境流程".into(),
                    atom_ids: vec![1],
                    summary: "摘要".into(),
                    created_at: 1000,
                    updated_at: 2000,
                },
                9,
            )
            .unwrap();
        assert_eq!(
            mirror_scenario_title(tmp.path(), 1000, 9).as_deref(),
            Some("部署---生产环境流程")
        );
    }

    /// S14b degradation contract (S14c extended: the cycle is now L3-L5 and
    /// atom rows feed L4/L5, so the phases keep zero atom rows to stay in
    /// the persona-only shape): with the LLM at an unreachable endpoint
    /// (127.0.0.1 discard port — connection refused, offline deterministic;
    /// ureq retries 3× with backoff, so phase B costs ~3s, same precedent as
    /// session_store's embedder-degradation test) the refresh must
    /// warn-and-return: no panic, no persona.md. A scenario-less session
    /// must skip even before the LLM call. The manual persona.md write then
    /// verifies the `/recall` consumption end-to-end.
    #[test]
    fn run_consolidation_degrades_on_unreachable_llm_and_recall_reads_persona_md() {
        use crate::memory::persona::{Persona, PersonaGenerator};
        use crate::memory::retrieval::RetrievalEngine;

        let tmp = tempfile::TempDir::new().unwrap();
        let config = Config {
            data_dir: tmp.path().to_path_buf(),
            scenarios: crate::config::ScenarioConfig {
                enabled: true,
                ..Default::default()
            },
            persona: crate::config::PersonaConfig { trigger_every_n: 1 },
            ..Config::default()
        };
        config.ensure_dirs().unwrap();
        let memory_dir = config.memory_dir();
        let persona_path = memory_dir.join("persona.md");

        let db = Arc::new(Mutex::new(Db::open_memory().unwrap()));
        db.lock().unwrap().init_schema().unwrap();
        db.lock()
            .unwrap()
            .conn()
            .execute(
                "INSERT INTO sessions (session_id, start_ts, file_path, created_at, updated_at) \
                 VALUES ('s1', 1000, 'f.jsonl', 1000, 1000)",
                [],
            )
            .unwrap();

        // Unreachable LLM: any attempted call fails after ~3s of retries.
        let llm = Arc::new(LlmClient::new("http://127.0.0.1:9/v1", "k", "test-model"));

        // Phase A: due (1 session, no persona yet) but NO scenario rows →
        // debug-skip before any LLM call, persona.md untouched.
        run_consolidation(db.clone(), llm.clone(), Arc::new(config.clone()), "s1", &[]);
        assert!(
            !persona_path.exists(),
            "no scenarios → skip must not create persona.md"
        );

        // Phase B: add a scenario row + mirror → due fires the LLM call,
        // which fails; the refresh must degrade silently (test completing =
        // no panic) and leave no persona.md behind. No atom rows exist yet,
        // so the L4/L5 steps debug-skip on empty input (S14c).
        {
            let d = db.lock().unwrap();
            d.conn()
                .execute(
                    "INSERT INTO bounded_memory (target, content, created_at, updated_at, memory_type) \
                     VALUES ('memory', '用户在调试 Rust 所有权', 1000, 1000, 'scenario')",
                    [],
                )
                .unwrap();
            let id = d.conn().last_insert_rowid();
            let dir = memory_dir.join("scenarios");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join(format!("1000_{}.md", id)),
                "---\ntitle: Rust 调试\natom_ids: [1]\ncreated_at: 1000\nupdated_at: 1000\n---\n\n摘要\n",
            )
            .unwrap();
        }
        run_consolidation(db.clone(), llm.clone(), Arc::new(config.clone()), "s1", &[]);
        assert!(
            !persona_path.exists(),
            "failed LLM generation must leave no persona.md"
        );

        // Phase C: hand-written persona.md (bypassing the LLM) → the /recall
        // L3 chain surfaces it (no DB target='user' row in this fixture),
        // and the trigger now sees updated_at as the window boundary.
        let gen = PersonaGenerator::new(&llm, &memory_dir);
        gen.save_persona(&Persona {
            preferences: "标记画像偏好".into(),
            identity: "i".into(),
            workflow: "w".into(),
            tech_stack: "t".into(),
            communication_style: "c".into(),
            created_at: 1000,
            updated_at: 5000,
            supersedes_id: None,
        })
        .unwrap();
        assert_eq!(last_persona_ts(&persona_path), 5000);
        {
            let d = db.lock().unwrap();
            let engine = RetrievalEngine::new(&d, &memory_dir, 2000);
            let outcome = engine.recall("无关查询词", 10, None, None, None).unwrap();
            assert!(
                outcome.memories.iter().any(|m| {
                    m["layer"] == "L3"
                        && m["type"] == "persona"
                        && m["content"]
                            .as_str()
                            .is_some_and(|c| c.contains("标记画像偏好"))
                }),
                "/recall must consume the generated persona.md, got {:?}",
                outcome.memories
            );
        }
    }

    // ── S14c: L4/L5 join the consolidation cycle ──

    /// Fixture config: consolidation cycle on (scenarios gate + trigger 1),
    /// memory_dir inside `tmp`.
    fn consolidation_fixture(tmp: &tempfile::TempDir) -> Config {
        let config = Config {
            data_dir: tmp.path().to_path_buf(),
            scenarios: crate::config::ScenarioConfig {
                enabled: true,
                ..Default::default()
            },
            persona: crate::config::PersonaConfig { trigger_every_n: 1 },
            ..Config::default()
        };
        config.ensure_dirs().unwrap();
        config
    }

    /// In-memory DB with one updated session (trigger_every_n=1 → due; the
    /// persona window starts at 0 = "never generated").
    fn consolidation_db_with_session() -> Arc<Mutex<Db>> {
        let db = Arc::new(Mutex::new(Db::open_memory().unwrap()));
        db.lock().unwrap().init_schema().unwrap();
        db.lock()
            .unwrap()
            .conn()
            .execute(
                "INSERT INTO sessions (session_id, start_ts, file_path, created_at, updated_at) \
                 VALUES ('s1', 1000, 'f.jsonl', 1000, 1000)",
                [],
            )
            .unwrap();
        db
    }

    fn insert_row(db: &Arc<Mutex<Db>>, content: &str, memory_type: &str, updated_at: i64) -> i64 {
        let d = db.lock().unwrap();
        d.conn()
            .execute(
                "INSERT INTO bounded_memory (target, content, created_at, updated_at, memory_type) \
                 VALUES ('memory', ?1, ?2, ?2, ?3)",
                rusqlite::params![content, updated_at, memory_type],
            )
            .unwrap();
        d.conn().last_insert_rowid()
    }

    /// Sorted `*.md` file names of a directory ("" listing when absent).
    fn md_files(dir: &std::path::Path) -> Vec<String> {
        match std::fs::read_dir(dir) {
            Ok(entries) => {
                let mut names: Vec<String> = entries
                    .filter_map(|e| e.ok())
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect();
                names.sort();
                names
            }
            Err(_) => Vec::new(),
        }
    }

    /// Offline-deterministic LLM stub (S14c): a thread answering exactly
    /// `responses.len()` POSTs on an ephemeral 127.0.0.1 port, in call
    /// order, with OpenAI-shaped envelopes carrying the given content
    /// literals. The consolidation cycle is single-threaded, so the call
    /// sequence is deterministic: persona → workflow → decision →
    /// communication → topics → needs.
    fn spawn_llm_stub(responses: Vec<&'static str>) -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for (i, stream) in listener.incoming().enumerate() {
                let Some(&content) = responses.get(i) else {
                    break;
                };
                let Ok(mut stream) = stream else { continue };
                // Drain the request (headers to \r\n\r\n, then exactly
                // Content-Length body bytes) before answering.
                let mut buf: Vec<u8> = Vec::new();
                let mut byte = [0u8; 1];
                while !buf.ends_with(b"\r\n\r\n") {
                    match stream.read(&mut byte) {
                        Ok(1) => buf.push(byte[0]),
                        _ => break,
                    }
                }
                let headers = String::from_utf8_lossy(&buf).to_ascii_lowercase();
                let len: usize = headers
                    .lines()
                    .find_map(|l| {
                        l.strip_prefix("content-length:")
                            .and_then(|v| v.trim().parse().ok())
                    })
                    .unwrap_or(0);
                let mut body = vec![0u8; len];
                if len > 0 {
                    let _ = stream.read_exact(&mut body);
                }
                let json = serde_json::json!({
                    "choices": [{ "message": { "role": "assistant", "content": content } }]
                })
                .to_string();
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    json.len(),
                    json
                );
                let _ = stream.write_all(resp.as_bytes());
                let _ = stream.flush();
            }
        });
        format!("http://{}/v1", addr)
    }

    /// 7a (independence, mixed responses): L3's generation fails to parse
    /// while the workflow-patterns call succeeds and the other two L4 calls
    /// fail — L3's failure must not skip L4, one document's failure must not
    /// skip the other L4 documents, and L5 (whose calls succeed) must run
    /// regardless. The end state — persona.md absent, exactly one L4 doc,
    /// both L5 docs — pins all three invariants at once.
    #[test]
    fn run_consolidation_steps_are_independent_mixed_llm_responses() {
        use crate::memory::mental_model::{
            load_decision_framework_from, load_workflow_patterns_from,
        };
        let bad = "not a json array or object";
        let arr = "[\"甲模式\", \"乙模式\", \"丙模式\"]";
        // persona(bad) → workflow(arr) → decision(bad) → comm(bad) →
        // topics(arr) → needs(arr)
        let url = spawn_llm_stub(vec![bad, arr, bad, bad, arr, arr]);

        let tmp = tempfile::TempDir::new().unwrap();
        let config = consolidation_fixture(&tmp);
        let memory_dir = config.memory_dir();
        let db = consolidation_db_with_session();
        insert_row(&db, "用户调试 Rust 的场景", "scenario", 1000);
        insert_row(&db, "用户偏好小步提交", "atom", 1000);
        let llm = Arc::new(LlmClient::new(&url, "k", "test-model"));

        run_consolidation(db.clone(), llm, Arc::new(config), "s1", &[]);

        assert!(
            !memory_dir.join("persona.md").exists(),
            "failed persona generation must write nothing"
        );
        assert_eq!(
            md_files(&memory_dir.join("mental_models")),
            vec!["workflow-patterns.md".to_string()],
            "decision/comm failures must not skip workflow, and must not write junk"
        );
        assert_eq!(
            md_files(&memory_dir.join("intent")),
            vec![
                "anticipated-needs.md".to_string(),
                "likely-next-topics.md".to_string()
            ],
            "L5 runs even though L4 was only partially generated"
        );
        // The written doc is readable by the /recall L4 loader with the stub
        // payload — generation and consumption agree on the file format.
        let wf = load_workflow_patterns_from(&memory_dir)
            .unwrap()
            .expect("workflow doc");
        assert_eq!(wf.patterns, vec!["甲模式", "乙模式", "丙模式"]);
        assert!(load_decision_framework_from(&memory_dir).unwrap().is_none());
    }

    /// 7a (all-degrade): unreachable LLM (127.0.0.1:9) with scenario AND
    /// atom rows in place — every one of the six LLM calls of a full cycle
    /// fails; no panic, no persona.md, and neither the mental_models/ nor
    /// the intent/ directory is ever created (saves only run on success).
    /// Runtime ≈ 6 × ureq's ~3s retry backoff (~18s), same offline pattern
    /// as the S14b degradation test above.
    #[test]
    fn run_consolidation_degrades_to_no_files_when_llm_unreachable() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config = consolidation_fixture(&tmp);
        let memory_dir = config.memory_dir();
        let db = consolidation_db_with_session();
        insert_row(&db, "用户调试 Rust 的场景", "scenario", 1000);
        insert_row(&db, "用户偏好小步提交", "atom", 1000);
        let llm = Arc::new(LlmClient::new("http://127.0.0.1:9/v1", "k", "test-model"));
        let turns = vec![TurnContent {
            role: "user".to_string(),
            content: "继续之前的重构".to_string(),
        }];

        run_consolidation(db, llm, Arc::new(config), "s1", &turns);

        assert!(!memory_dir.join("persona.md").exists());
        assert!(!memory_dir.join("mental_models").exists());
        assert!(!memory_dir.join("intent").exists());
    }

    /// L4/L5 input: live (non-superseded) atom rows only, newest first,
    /// scenario rows never included, title = first 30 chars (CJK-safe).
    #[test]
    fn consolidation_inputs_excludes_superseded_atoms_newest_first() {
        let db = consolidation_db_with_session();
        let old_id = insert_row(&db, "被取代的旧事实", "atom", 500);
        {
            let d = db.lock().unwrap();
            d.conn()
                .execute(
                    "INSERT INTO bounded_memory (target, content, created_at, updated_at, memory_type, supersedes_id) \
                     VALUES ('memory', '取代旧事实的行', 3000, 3000, 'atom', ?1)",
                    rusqlite::params![old_id],
                )
                .unwrap();
        }
        let long_cjk = "一二三四五六七八九十一二三四五六七八九十一二三四五六七八九十一二三四"; // 34 chars
        insert_row(&db, long_cjk, "atom", 1500);
        insert_row(&db, "最新的原子事实", "atom", 2000);
        insert_row(&db, "场景不该进来", "scenario", 4000);

        let inputs = consolidation_inputs(&db, "s1");
        let summaries: Vec<&str> = inputs.iter().map(|s| s.summary.as_str()).collect();
        assert_eq!(
            summaries,
            vec!["取代旧事实的行", "最新的原子事实", long_cjk],
            "superseded atom filtered, scenario row excluded, newest first: {:?}",
            summaries
        );
        assert_eq!(
            inputs[2].title,
            long_cjk.chars().take(30).collect::<String>(),
            "char-based (CJK-safe) title fallback"
        );
    }

    /// The digest entry: absent without turns, last-8 window, 400-char cap.
    #[test]
    fn session_digest_windows_and_caps() {
        assert!(session_digest("s1", &[]).is_none());
        let turn = |i: usize| TurnContent {
            role: "user".to_string(),
            content: format!("回合内容{}", i),
        };
        let many: Vec<TurnContent> = (1..=12).map(turn).collect();
        let d = session_digest("s1", &many).unwrap();
        assert_eq!(d.title, "current session s1");
        assert!(d.summary.contains("回合内容5"));
        assert!(d.summary.contains("回合内容12"));
        assert!(
            !d.summary.contains("回合内容4"),
            "only the last 8 turns: {}",
            d.summary
        );
        let long = vec![TurnContent {
            role: "user".to_string(),
            content: "长".to_string() + &"话".repeat(500),
        }];
        let d = session_digest("s1", &long).unwrap();
        assert_eq!(d.summary.chars().count(), 400);
    }
}

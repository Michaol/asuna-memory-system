//! Post-session pipeline: L1 atom extraction -> graph integration -> L2/L3-L5 consolidation.
//!
//! Designed to run inside `tokio::task::spawn_blocking`. All operations
//! (LLM calls, DB queries) are synchronous.

mod consolidation;
mod l2;

use crate::config::Config;
use crate::embedder::LazyEmbedder;
use crate::index::db::Db;
use crate::memory::admission::AdmissionScorer;
use crate::memory::l1::{L1Extractor, StoredAtom, TurnContent};
use crate::memory::llm::LlmClient;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

const PIPELINE_TURN_MAX_CHARS: usize = 4000;

/// 单字符实体（英文 "a"/"I"、中文单字）判别度低，入图易产生虚假边——
/// 图谱集成的启发式质量门槛，最小 2 字符。
const MIN_GRAPH_ENTITY_CHARS: usize = 2;

type Phase1TurnRows = Vec<(i64, i64, String, String)>;

pub(super) fn lock_db(db: &Mutex<Db>) -> MutexGuard<'_, Db> {
    let guard = crate::util::recover_poison(db);
    guard.rollback_if_in_tx();
    guard
}

pub(super) struct AtomicFlagGuard<'a>(pub(super) &'a AtomicBool);

impl Drop for AtomicFlagGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// Flush the deferred `.md` snapshot AFTER the DB guard dropped (no DB lock held
/// across file IO). Multi-writer: `PendingMdWrite::flush` re-renders the CURRENT
/// DB state under the same per-file `write_lock -> flock` shell as the immediate
/// path, folding in rows concurrent writers committed in the unlock gap (F1/F2).
/// The shell briefly locks `db` (L_gw) for the SELECT re-render ONLY and drops the
/// guard BEFORE `atomic_write` (R2 gateway-flush exception); the flock is never
/// taken while an open SQLite WRITE tx is held (R1).
///
/// In-process serialization: up to `MAX_CONCURRENT_PIPELINES` (=4, transport/
/// state.rs) pipelines may reach here concurrently; the shared per-file
/// `write_lock` serializes them BEFORE they open flock handles. Consolidation's
/// single-flight flag does NOT cover this L1 flush path — the write_lock+flock
/// shell is what makes concurrent L1 flushes safe.
fn flush_pending_md(
    pending_md: Option<crate::growth::bounded_memory::PendingMdWrite>,
    db: &Mutex<Db>,
) {
    if let Some(pending) = pending_md {
        if let Err(e) = pending.flush(db) {
            tracing::error!(
                "store_atoms: failed to sync atoms to MEMORY.md (DB and .md have diverged; run `asuna-memory doctor --fix` to repair): {}",
                e
            );
        }
    }
}

/// Run the post-session extraction + graph + consolidation pipeline (best-effort).
///
/// Note: `graph.enabled` gates the whole pipeline (along with `pipeline.enable_extraction`).
pub fn run_pipeline(
    db: Arc<Mutex<Db>>,
    llm: Arc<LlmClient>,
    embedder: Option<Arc<LazyEmbedder>>,
    config: Arc<Config>,
    session_id: String,
    consolidation_running: Option<Arc<AtomicBool>>,
    shutdown: Arc<AtomicBool>,
) {
    if shutdown.load(Ordering::SeqCst) {
        tracing::info!("Pipeline cancelled before start for session {}", session_id);
        return;
    }
    if !pipeline_enabled(&config, &session_id) {
        return;
    }

    let Some((turns, turn_ids)) = load_qualified_session_turns(&db, &config, &session_id) else {
        return;
    };
    if shutdown.load(Ordering::SeqCst) {
        return;
    }
    let Some(atoms) = extract_session_atoms(&llm, &session_id, &turns) else {
        return;
    };
    if shutdown.load(Ordering::SeqCst) {
        return;
    }
    let Some(l1) = commit_l1_stage(
        &db,
        &llm,
        embedder.as_deref(),
        &config,
        &session_id,
        &atoms,
        &turn_ids,
    ) else {
        return;
    };

    tracing::info!(
        "Pipeline complete for session {}: {} atoms stored, {} graph integrations",
        session_id,
        l1.stored_count,
        l1.graph_count
    );

    // 门控（for_ai §8.3）：L2-L5 非开箱即跑——scenarios.enabled 默认 false
    // 同时关闭 L2 聚合与 L3-L5 consolidation；persona.trigger_every_n=0 另行
    // 显式关闭 L3-L5 的 due 判定（默认 10）。文档侧措辞由文档簇同步。
    if shutdown.load(Ordering::SeqCst) {
        return;
    }
    if config.scenarios.enabled && !l1.l2_atoms.is_empty() {
        l2::run_l2_aggregation(&db, &llm, &config, l1.l2_atoms, &session_id);
    }
    if shutdown.load(Ordering::SeqCst) {
        return;
    }
    if config.scenarios.enabled && config.persona.trigger_every_n > 0 {
        consolidation::run_consolidation(
            db,
            llm,
            config,
            &session_id,
            &turns,
            consolidation_running.as_deref(),
        );
    }
}

fn pipeline_enabled(config: &Config, session_id: &str) -> bool {
    if !config.pipeline.enable_extraction {
        tracing::debug!("Pipeline disabled, skip session {}", session_id);
        return false;
    }
    if !config.graph.enabled {
        tracing::debug!("Graph disabled, skip session {}", session_id);
        return false;
    }
    true
}

fn load_qualified_session_turns(
    db: &Arc<Mutex<Db>>,
    config: &Config,
    session_id: &str,
) -> Option<(Vec<TurnContent>, Vec<i64>)> {
    let (file_path, db_rows) = fetch_phase1_turns(db, session_id)?;
    let (turns, turn_ids) = load_session_turns(
        &config.conversations_dir(),
        session_id,
        file_path.as_deref(),
        db_rows,
    );
    if turns.len() < config.pipeline.every_n_turns {
        tracing::debug!(
            "Pipeline: session {} too short ({} turns < {}), skipping",
            session_id,
            turns.len(),
            config.pipeline.every_n_turns
        );
        return None;
    }
    Some((turns, turn_ids))
}

fn extract_session_atoms(
    llm: &LlmClient,
    session_id: &str,
    turns: &[TurnContent],
) -> Option<Vec<crate::memory::l1::Atom>> {
    match crate::memory::l1::extract_atoms(llm, turns) {
        Ok(atoms) if !atoms.is_empty() => {
            tracing::info!(
                "Pipeline: extracted {} atoms from session {} ({} turns)",
                atoms.len(),
                session_id,
                turns.len()
            );
            Some(atoms)
        }
        Ok(_) => {
            tracing::debug!("Pipeline: no atoms extracted for session {}", session_id);
            None
        }
        Err(e) => {
            tracing::warn!("Pipeline: L1 extraction failed for {}: {}", session_id, e);
            None
        }
    }
}

struct CommittedL1 {
    stored_count: usize,
    graph_count: u32,
    l2_atoms: Vec<(i64, String, Vec<f32>)>,
}

fn commit_l1_stage(
    db: &Arc<Mutex<Db>>,
    llm: &LlmClient,
    embedder: Option<&LazyEmbedder>,
    config: &Config,
    session_id: &str,
    atoms: &[crate::memory::l1::Atom],
    turn_ids: &[i64],
) -> Option<CommittedL1> {
    let mut plan = {
        let db_guard = lock_db(db);
        L1Extractor::new(&db_guard)
            .prepare_store(atoms, turn_ids)
            .map_err(|e| tracing::warn!("Pipeline: prepare_store failed for {}: {}", session_id, e))
            .ok()?
    };

    if let Err(e) = plan.execute_embed(embedder) {
        tracing::warn!("Pipeline: embed stage failed for {}: {}", session_id, e);
        return None;
    }
    let admission_scorer = config
        .admission
        .enabled
        .then(|| AdmissionScorer::new(&config.admission, Some(llm)));
    if let Err(e) = plan.execute_score(admission_scorer.as_ref()) {
        tracing::warn!(
            "Pipeline: admission scoring failed for {}: {}",
            session_id,
            e
        );
        return None;
    }

    let (stored_count, graph_count, l2_atoms, pending_md) = {
        let db_guard = lock_db(db);
        let bounded_memory = crate::growth::bounded_memory::BoundedMemory::new(
            &config.memory_dir(),
            &db_guard,
            config.memory.memory_char_limit,
            config.memory.user_char_limit,
        )
        .with_atom_capacity_ratio(config.memory.atom_capacity_ratio);

        let extractor = L1Extractor::new(&db_guard).with_growth(bounded_memory);
        let (stored, pending_md) = extractor
            .commit_store_deferred_md(&mut plan)
            .map_err(|e| tracing::warn!("Pipeline: commit_store failed for {}: {}", session_id, e))
            .ok()?;
        let stored_count = stored.len();
        // W17: commit 内的容量驱逐可能已删掉刚入库的原子——按存活 id 过滤后再
        // 集成/聚类，否则图谱重插孤儿实体、L2 与 scenario frontmatter 带悬空 atom_ids。
        let stored = retain_live_stored(&db_guard, stored);
        let graph_count = integrate_session_atoms(&db_guard, atoms, &stored, session_id);
        let l2_atoms = filter_l2_atoms(atoms, stored);
        (stored_count, graph_count, l2_atoms, pending_md)
    };
    flush_pending_md(pending_md, db);
    Some(CommittedL1 {
        stored_count,
        graph_count,
        l2_atoms,
    })
}

fn lookup_session_file_path(db: &Db, session_id: &str) -> Option<String> {
    use rusqlite::OptionalExtension;
    match db
        .conn()
        .query_row(
            "SELECT file_path FROM sessions WHERE session_id = ?1",
            rusqlite::params![session_id],
            |row| row.get::<_, String>(0),
        )
        .optional()
    {
        Ok(fp) => fp,
        Err(e) => {
            tracing::warn!(
                "Pipeline: session file_path lookup failed for {}: {}",
                session_id,
                e
            );
            None
        }
    }
}

fn query_phase1_turn_rows(
    db: &Db,
    session_id: &str,
) -> Result<Phase1TurnRows, (&'static str, rusqlite::Error)> {
    let mut stmt = db
        .conn()
        .prepare(
            "SELECT id, seq, role, COALESCE(preview, '') FROM turns WHERE session_id = ?1 ORDER BY seq",
        )
        .map_err(|e| ("turn query", e))?;
    let mapped = stmt
        .query_map(rusqlite::params![session_id], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })
        .map_err(|e| ("turn query", e))?;
    mapped
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| ("turn decode", e))
}

fn collect_phase1_turn_rows(db: &Db, session_id: &str) -> Option<Phase1TurnRows> {
    match query_phase1_turn_rows(db, session_id) {
        Ok(rows) => Some(rows),
        Err((stage, e)) => {
            tracing::warn!("Pipeline: {} failed for {}: {}", stage, session_id, e);
            None
        }
    }
}

fn fetch_phase1_turns(
    db: &Arc<Mutex<Db>>,
    session_id: &str,
) -> Option<(Option<String>, Phase1TurnRows)> {
    let db_guard = lock_db(db);
    let file_path = lookup_session_file_path(&db_guard, session_id);
    let rows = collect_phase1_turn_rows(&db_guard, session_id)?;
    Some((file_path, rows))
}

fn load_jsonl_turn_contents(
    conversations_dir: &std::path::Path,
    session_id: &str,
    file_path: Option<&str>,
) -> std::collections::HashMap<i64, String> {
    let Some(fp) = file_path.map(str::trim).filter(|p| !p.is_empty()) else {
        return std::collections::HashMap::new();
    };
    if fp.starts_with("gateway://") || std::path::Path::new(fp).is_absolute() {
        return std::collections::HashMap::new();
    }
    let candidate = conversations_dir.join(fp);
    if !crate::util::is_contained_in_dir(conversations_dir, &candidate) {
        return std::collections::HashMap::new();
    }
    let (header, jsonl_turns) = match crate::index::conversation::read_session(&candidate) {
        Ok(v) => v,
        Err(e) => {
            // 真相源不可读必须可观测：静默回落 DB preview 会掩盖 JSONL
            // 损坏/权限故障（与 header mismatch 分支同口径 warn）。
            tracing::warn!(
                "Pipeline: JSONL read failed for {} ({}), falling back to DB preview",
                session_id,
                e
            );
            return std::collections::HashMap::new();
        }
    };
    if header.session_id != session_id {
        tracing::warn!(
            "Pipeline: JSONL session_id mismatch for {} (got {}), falling back to DB preview",
            session_id,
            header.session_id
        );
        return std::collections::HashMap::new();
    }
    jsonl_turns
        .into_iter()
        .map(|t| {
            let capped = if t.content.chars().count() > PIPELINE_TURN_MAX_CHARS {
                t.content.chars().take(PIPELINE_TURN_MAX_CHARS).collect()
            } else {
                t.content
            };
            (t.seq as i64, capped)
        })
        .collect()
}

fn load_session_turns(
    conversations_dir: &std::path::Path,
    session_id: &str,
    file_path: Option<&str>,
    db_rows: Phase1TurnRows,
) -> (Vec<TurnContent>, Vec<i64>) {
    let jsonl_map = load_jsonl_turn_contents(conversations_dir, session_id, file_path);
    let mut turn_ids = Vec::with_capacity(db_rows.len());
    let mut turns = Vec::with_capacity(db_rows.len());
    for (id, seq, role, preview) in db_rows {
        turn_ids.push(id);
        let content = jsonl_map
            .get(&seq)
            .filter(|c| !c.trim().is_empty())
            .cloned()
            .unwrap_or(preview);
        turns.push(TurnContent { role, content });
    }
    (turns, turn_ids)
}

/// W17: keep only the stored atoms whose `bounded_memory` row survived the
/// capacity eviction that runs inside `commit_store_deferred_md`.
/// F11: a failed liveness check (prepare, query or decode) fails closed —
/// return an empty Vec so graph integration/L2 skip this round rather than
/// re-insert orphans while the DB is unhealthy.
fn retain_live_stored(db: &Db, stored: Vec<StoredAtom>) -> Vec<StoredAtom> {
    if stored.is_empty() {
        return stored;
    }
    // W19: 按 SQL_IN_CHUNK_SIZE (500) 分块（口径同 l2.rs）。抽取批**没有**条数帽
    // （唯一上界是 8 MiB LLM 响应 ≈ 13 万原子），单条 IN 列表可超过 SQLite 变量上限
    // 32766 → prepare 失败 → 整会话图谱集成/L2 静默跳过。
    let mut live_ids: std::collections::HashSet<i64> = std::collections::HashSet::new();
    let res = crate::index::sql_util::for_each_in_chunk(
        &stored,
        |chunk, placeholders| -> Result<(), ()> {
            let sql = format!("SELECT id FROM bounded_memory WHERE id IN ({placeholders})");
            let mut stmt = db.conn().prepare(&sql).map_err(|e| {
                tracing::warn!(
                    "Pipeline: live-atom check prepare failed ({e}); skipping graph integration/L2 this round"
                );
            })?;
            // E0597 守卫：rows 先独立绑定，collect 的 match 直接作尾表达式
            // （结果 owned，不借 stmt；MappedRows 临时值不得活过 stmt 落域）。
            let rows = stmt
                .query_map(rusqlite::params_from_iter(chunk.iter().map(|s| &s.id)), |r| {
                    r.get::<_, i64>(0)
                })
                .map_err(|e| {
                    tracing::warn!(
                        "Pipeline: live-atom check failed ({e}); skipping graph integration/L2 this round"
                    );
                })?;
            let ids: Vec<i64> = rows.collect::<rusqlite::Result<_>>().map_err(|e| {
                tracing::warn!(
                    "Pipeline: live-atom check decode failed ({e}); skipping graph integration/L2 this round"
                );
            })?;
            live_ids.extend(ids);
            Ok(())
        },
    );
    if res.is_err() {
        return Vec::new();
    }
    stored
        .into_iter()
        .filter(|s| live_ids.contains(&s.id))
        .collect()
}

/// Filter stored atoms for L2 clustering, excluding any atom superseded within
/// the same batch or missing a valid non-zero embedding.
fn filter_l2_atoms(
    atoms: &[crate::memory::l1::Atom],
    stored: Vec<StoredAtom>,
) -> Vec<(i64, String, Vec<f32>)> {
    let superseded_ids: std::collections::HashSet<i64> =
        stored.iter().filter_map(|s| s.supersedes_id).collect();
    stored
        .into_iter()
        .filter(|s| !superseded_ids.contains(&s.id))
        // W2.4 同族 / M13a：统一走 is_invalid_vector（任一分量 inf/NaN 即整向量拒，
        // 再要求非全零），与 atom 入库/召回守卫同一判据，不再内联重复。
        .filter(|s| !crate::index::vector::is_invalid_vector(&s.embedding))
        .filter_map(|s| {
            let atom = atoms.get(s.source_index)?;
            Some((s.id, atom.content.clone(), s.embedding))
        })
        .collect()
}

fn integrate_session_atoms(
    db: &Db,
    atoms: &[crate::memory::l1::Atom],
    stored: &[StoredAtom],
    session_id: &str,
) -> u32 {
    if stored.is_empty() {
        return 0;
    }
    let tx = match db.immediate_tx() {
        Ok(tx) => tx,
        Err(e) => {
            tracing::warn!(
                "Pipeline: failed to begin graph integration tx for {}: {}",
                session_id,
                e
            );
            return 0;
        }
    };
    let count = stored
        .iter()
        .filter(|s| integrate_single_atom(&tx, atoms, s, session_id))
        .count() as u32;
    if let Err(e) = tx.commit() {
        tracing::warn!(
            "Pipeline: graph integration tx commit failed for {}: {}",
            session_id,
            e
        );
        return 0;
    }
    count
}

fn integrate_single_atom(
    tx: &rusqlite::Transaction<'_>,
    atoms: &[crate::memory::l1::Atom],
    s: &StoredAtom,
    session_id: &str,
) -> bool {
    let Some(atom) = atoms.get(s.source_index) else {
        tracing::warn!(
            "Pipeline: stored atom {} has source_index {} outside the {}-atom batch; skipping graph integration",
            s.id,
            s.source_index,
            atoms.len()
        );
        return false;
    };
    let entities: Vec<String> = atom
        .entities
        .iter()
        .filter(|e| e.chars().count() >= MIN_GRAPH_ENTITY_CHARS)
        .cloned()
        .collect();
    match crate::memory::graph_integration::integrate_atom_with_graph_on_conn(
        tx,
        s.id,
        &atom.content,
        s.supersedes_id,
        Some(session_id),
        &entities,
        &[],
    ) {
        Ok(_) => true,
        Err(e) => {
            tracing::warn!(
                "Pipeline: graph integration failed for atom {}: {}",
                s.id,
                e
            );
            false
        }
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod fail_closed_tests;

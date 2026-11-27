use super::lock_db;
use crate::config::Config;
use crate::index::db::Db;
use crate::memory::llm::LlmClient;
use std::sync::{Arc, Mutex};

/// L2 scenario aggregation: cluster this session's newly-stored atoms using
/// the embeddings already computed in Phase 3b, summarize via LLM, and store
/// each summary as a `memory_type='scenario'` row + Markdown mirror.
pub(super) fn run_l2_aggregation(
    db: &Arc<Mutex<Db>>,
    llm: &Arc<LlmClient>,
    config: &Config,
    atoms_with_emb: Vec<(i64, String, Vec<f32>)>,
    session_id: &str,
) {
    if atoms_with_emb.len() < config.scenarios.min_cluster_size {
        tracing::debug!(
            "L2: too few embedded atoms ({}) for {}",
            atoms_with_emb.len(),
            session_id
        );
        return;
    }
    let scenarios_dir = config.memory_dir().join("scenarios");
    let aggregator = crate::memory::scenario::ScenarioAggregator::new(llm, &scenarios_dir);
    let Some(scenarios) = try_aggregate_scenarios(&aggregator, &atoms_with_emb, config, session_id)
    else {
        return;
    };
    let (written, skipped_dup) = write_scenarios(db, config, &aggregator, &scenarios, session_id);
    tracing::info!(
        "Pipeline L2: {} scenarios stored, {} duplicates skipped for session {}",
        written,
        skipped_dup,
        session_id
    );
}

fn try_aggregate_scenarios(
    aggregator: &crate::memory::scenario::ScenarioAggregator,
    atoms_with_emb: &[(i64, String, Vec<f32>)],
    config: &Config,
    session_id: &str,
) -> Option<Vec<crate::memory::scenario::Scenario>> {
    match aggregator.aggregate(
        atoms_with_emb,
        config.scenarios.similarity_threshold,
        config.scenarios.min_cluster_size,
    ) {
        Ok(s) if !s.is_empty() => Some(s),
        Ok(_) => {
            tracing::debug!("L2: no scenarios formed for {}", session_id);
            None
        }
        Err(e) => {
            tracing::warn!("L2: aggregate failed for {}: {}", session_id, e);
            None
        }
    }
}

fn scenario_passes_security_scan(s: &crate::memory::scenario::Scenario) -> bool {
    let title_scan = crate::growth::security::scan_content(&s.title);
    let summary_scan = crate::growth::security::scan_content(&s.summary);
    if title_scan.is_safe() && summary_scan.is_safe() {
        return true;
    }
    let reason = if !title_scan.is_safe() {
        title_scan.reason()
    } else {
        summary_scan.reason()
    };
    tracing::warn!(
        "L2: skipping scenario flagged by security scan ({}): {}",
        reason,
        crate::util::truncate_for_log(&crate::growth::security::redact_credentials(&s.title))
    );
    false
}

fn select_doomed_scenarios(
    tx: &rusqlite::Transaction<'_>,
    select_sql: &str,
    cap: i64,
) -> Option<Vec<(i64, i64)>> {
    let mut stmt = match tx.prepare(select_sql) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("L2: scenario cap-evict select prepare failed: {}", e);
            return None;
        }
    };
    // S1488/E0597：stmt 必须保持独立绑定，query_map 的 match 不能放进块尾
    // 表达式位（MappedRows 临时值会活过 stmt 的落域）。拆两段：rows 先绑定，
    // collect 的 match 直接作尾表达式（结果owned，不借 stmt）。
    let rows = match stmt.query_map(rusqlite::params![cap], |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
    }) {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!("L2: scenario cap-evict select failed: {}", e);
            return None;
        }
    };
    // W26: 行解码失败必须整批放弃——丢几行 doomed 再按同谓词全删，
    // 被丢那些行的 .md 镜像就成孤儿。
    match rows.collect::<rusqlite::Result<Vec<_>>>() {
        Ok(doomed) => Some(doomed),
        Err(e) => {
            tracing::warn!("L2: scenario cap-evict select decode failed: {}", e);
            None
        }
    }
}

fn evict_excess_scenarios(tx: &rusqlite::Transaction<'_>, cap: i64) -> Vec<(i64, i64)> {
    let evict_predicate = "target='memory' AND memory_type='scenario' AND id NOT IN \
         (SELECT id FROM bounded_memory WHERE target='memory' AND memory_type='scenario' \
          ORDER BY updated_at DESC, id DESC LIMIT ?1)";
    let select_sql = format!("SELECT id, created_at FROM bounded_memory WHERE {evict_predicate}");
    let delete_sql = format!("DELETE FROM bounded_memory WHERE {evict_predicate}");

    let Some(evicted) = select_doomed_scenarios(tx, &select_sql, cap) else {
        // W26: 枚举不到 doomed 行就不删——宁可本轮超帽（下轮重试），也不盲删
        // 留下没有 DB 行的 .md 镜像孤儿。
        tracing::warn!(
            "L2: scenario cap-evict skipped (doomed-row SELECT failed); cap retried next cycle"
        );
        return Vec::new();
    };
    if evicted.is_empty() {
        return evicted;
    }
    if let Err(e) = tx.execute(&delete_sql, rusqlite::params![cap]) {
        tracing::warn!("L2: scenario cap-evict failed: {}", e);
        return Vec::new();
    }
    evicted
}

/// W14: 串行化锁外场景 `.md` 镜像写/删，防止两轮并发提交倒序覆写或删重。
static SCENARIO_MIRROR_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn live_inserted_scenario_ids(
    db: &Arc<Mutex<Db>>,
    inserted: &[(usize, i64)],
) -> std::collections::HashSet<i64> {
    let mut live: std::collections::HashSet<i64> =
        std::collections::HashSet::with_capacity(inserted.len());
    if inserted.is_empty() {
        return live;
    }
    let db_guard = lock_db(db);
    let conn = db_guard.conn();
    let _: Result<(), std::convert::Infallible> = crate::index::sql_util::for_each_in_chunk(
        inserted,
        |chunk, placeholders| {
            let sql = format!("SELECT id FROM bounded_memory WHERE id IN ({placeholders})");
            match conn.prepare(&sql).and_then(|mut s| {
                s.query_map(
                    rusqlite::params_from_iter(chunk.iter().map(|(_, id)| id)),
                    |r| r.get::<_, i64>(0),
                )?
                .collect::<rusqlite::Result<Vec<i64>>>()
            }) {
                Ok(ids) => live.extend(ids),
                Err(e) => {
                    // W21c: fail-closed——复查失败时不得把未验证的 id 当存活
                    // （旧实现乐观全收，会为已驱逐行写孤儿 .md）。宁漏镜像
                    // （DB 行仍在，下轮/doctor 可补），不写无 DB 行的孤儿文件；
                    // 与 retain_live_stored 的 F11 整批放弃口径一致。
                    tracing::warn!(
                        "L2: failed to re-check scenario row liveness ({}); skipping mirrors for {} unverified rows (fail-closed)",
                        e,
                        chunk.len()
                    );
                }
            }
            Ok(())
        },
    );
    live
}

fn save_live_scenario_mirrors(
    aggregator: &crate::memory::scenario::ScenarioAggregator,
    scenarios: &[crate::memory::scenario::Scenario],
    inserted: &[(usize, i64)],
    live_ids: &std::collections::HashSet<i64>,
) {
    for (i, row_id) in inserted {
        if !live_ids.contains(row_id) {
            tracing::debug!(
                "L2: skipping .md save for row {} (already evicted before mirror sync)",
                row_id
            );
            continue;
        }
        if let Err(e) = aggregator.save_scenario(&scenarios[*i], *row_id) {
            // scenario 镜像无 doctor --fix 修复路径：DB/.md 漂移在默认
            // info 级必须可见（debug 级运维看不到），升 warn。
            tracing::warn!(
                "L2: scenario .md save failed for row {} (DB/.md diverged): {}",
                row_id,
                e
            );
        }
    }
}

fn remove_evicted_scenario_mirrors(scenarios_dir: &std::path::Path, evicted: &[(i64, i64)]) {
    for (row_id, created_at) in evicted {
        let path = scenarios_dir.join(format!("{}_{}.md", created_at, row_id));
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(ref e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!(
                "L2: scenario .md evict failed for {}: {}",
                path.display(),
                e
            ),
        }
    }
}

pub(super) fn sync_scenario_mirrors(
    db: &Arc<Mutex<Db>>,
    aggregator: &crate::memory::scenario::ScenarioAggregator,
    scenarios: &[crate::memory::scenario::Scenario],
    scenarios_dir: &std::path::Path,
    inserted: &[(usize, i64)],
    evicted: &[(i64, i64)],
) {
    let _mirror_guard = crate::util::recover_poison(&SCENARIO_MIRROR_LOCK);
    let live_ids = live_inserted_scenario_ids(db, inserted);
    save_live_scenario_mirrors(aggregator, scenarios, inserted, &live_ids);
    remove_evicted_scenario_mirrors(scenarios_dir, evicted);
}

enum ScenarioInsertOutcome {
    Inserted(i64),
    Duplicate,
    Skipped,
}

fn try_insert_scenario(
    tx: &rusqlite::Transaction<'_>,
    s: &crate::memory::scenario::Scenario,
    session_id: &str,
) -> ScenarioInsertOutcome {
    // W21: 纯空白 summary 会落空 scenario 行 + 空镜像；trim 后去重/入库同口径。
    let summary = s.summary.trim();
    if summary.is_empty() {
        tracing::warn!("L2: skipping scenario with blank summary");
        return ScenarioInsertOutcome::Skipped;
    }
    if !scenario_passes_security_scan(s) {
        return ScenarioInsertOutcome::Skipped;
    }
    use rusqlite::OptionalExtension;
    let exists = match tx
        .query_row(
            "SELECT 1 FROM bounded_memory WHERE target='memory' AND memory_type='scenario' AND content = ?1 LIMIT 1",
            rusqlite::params![summary],
            |_| Ok(()),
        )
        .optional()
    {
        Ok(opt) => opt.is_some(),
        Err(e) => {
            tracing::warn!("L2: scenario dedup check failed: {}", e);
            return ScenarioInsertOutcome::Skipped;
        }
    };
    if exists {
        return ScenarioInsertOutcome::Duplicate;
    }
    match tx.execute(
        "INSERT INTO bounded_memory (target, content, created_at, updated_at, confidence, memory_type, source_session) \
         VALUES ('memory', ?1, ?2, ?2, 'medium', 'scenario', ?3)",
        rusqlite::params![summary, s.created_at, session_id],
    ) {
        Ok(_) => ScenarioInsertOutcome::Inserted(tx.last_insert_rowid()),
        Err(e) => {
            tracing::warn!("L2: scenario row insert failed: {}", e);
            ScenarioInsertOutcome::Skipped
        }
    }
}

struct ScenariosTxOutcome {
    written: usize,
    skipped_dup: usize,
    committed: bool,
    inserted: Vec<(usize, i64)>,
    evicted: Vec<(i64, i64)>,
}

fn write_scenarios_tx(
    db: &Arc<Mutex<Db>>,
    max_scenarios: i64,
    scenarios: &[crate::memory::scenario::Scenario],
    session_id: &str,
) -> Option<ScenariosTxOutcome> {
    let db_guard = lock_db(db);
    let tx = match db_guard.immediate_tx() {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!("L2: tx begin failed for {}: {}", session_id, e);
            return None;
        }
    };
    let mut written = 0usize;
    let mut skipped_dup = 0usize;
    let mut inserted: Vec<(usize, i64)> = Vec::new();
    for (i, s) in scenarios.iter().enumerate() {
        match try_insert_scenario(&tx, s, session_id) {
            ScenarioInsertOutcome::Inserted(row_id) => {
                written += 1;
                inserted.push((i, row_id));
            }
            ScenarioInsertOutcome::Duplicate => skipped_dup += 1,
            ScenarioInsertOutcome::Skipped => {}
        }
    }
    let evicted = evict_excess_scenarios(&tx, max_scenarios);
    let committed = match tx.commit() {
        Ok(()) => true,
        Err(e) => {
            tracing::warn!("L2: tx commit failed for {}: {}", session_id, e);
            false
        }
    };
    Some(ScenariosTxOutcome {
        written,
        skipped_dup,
        committed,
        inserted,
        evicted,
    })
}

/// Write scenario rows under the DB lock in one transaction (dedup by summary,
/// cap-evict oldest beyond `max_scenarios`), then synchronize `.md` mirrors outside the lock.
pub(super) fn write_scenarios(
    db: &Arc<Mutex<Db>>,
    config: &Config,
    aggregator: &crate::memory::scenario::ScenarioAggregator,
    scenarios: &[crate::memory::scenario::Scenario],
    session_id: &str,
) -> (usize, usize) {
    let Some(outcome) = write_scenarios_tx(
        db,
        config.scenarios.max_scenarios as i64,
        scenarios,
        session_id,
    ) else {
        return (0, 0);
    };

    if outcome.committed {
        let scenarios_dir = config.memory_dir().join("scenarios");
        sync_scenario_mirrors(
            db,
            aggregator,
            scenarios,
            &scenarios_dir,
            &outcome.inserted,
            &outcome.evicted,
        );
        (outcome.written, outcome.skipped_dup)
    } else {
        tracing::debug!("L2: skipping .md mirror sync (tx rolled back)");
        (0, outcome.skipped_dup)
    }
}

use super::{Atom, L1Extractor, StoredAtom};
use crate::growth::bounded_memory::normalize_crlf;

mod embed;
mod store;

pub(super) use embed::StorePlan;
#[cfg(test)]
pub(super) use store::{confidence_level_order, should_supersede};

/// Char budget for the admission conversation context.
pub(super) const ADMISSION_CONTEXT_CHAR_BUDGET: usize = 1500;

/// Cap on source turns read for the admission context (avoids huge IN lists).
pub(super) const ADMISSION_CONTEXT_MAX_TURNS: usize = 64;

impl<'a> L1Extractor<'a> {
    /// Store atoms with dedup and conflict detection (test compatibility wrapper).
    #[cfg(test)]
    pub fn store_atoms(
        &self,
        atoms: &[Atom],
        source_turn_ids: &[i64],
    ) -> anyhow::Result<Vec<StoredAtom>> {
        let mut plan = self.prepare_store(atoms, source_turn_ids)?;
        plan.execute_embed(self.embedder)?;
        plan.execute_score(None)?;
        self.commit_store(&mut plan)
    }

    /// Stage 1 (short DB lock): snapshot everything the store needs so the
    /// network-bound stage that follows can run without holding the DB mutex.
    pub fn prepare_store<'b>(
        &self,
        atoms: &'b [Atom],
        source_turn_ids: &[i64],
    ) -> anyhow::Result<StorePlan<'b>> {
        let turn_ids_json = serde_json::to_string(source_turn_ids)?;

        let existing_contents = self.load_existing_contents()?;

        let existing = self.load_existing_embeddings(0)?;
        // W20: 快照水位（id AUTOINCREMENT 单调）——commit 事务内据此补载
        // 锁外窗口里并发管线新插入的活原子向量。
        let snapshot_max_id = existing.iter().map(|(id, _)| *id).max().unwrap_or(0);

        // W20: 新近度按最新 source turn 计（first() 取的是最旧 turn，低估新近度）。
        let turn_timestamp_ms = if let Some(&turn_id) = source_turn_ids.last() {
            self.db
                .conn()
                .query_row(
                    "SELECT timestamp_ms FROM turns WHERE id = ?1",
                    [turn_id],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap_or_else(|e| {
                    // 回退必须留痕：坏 ts/已删 turn 会让新近度塌缩到 now，
                    // 无日志则不可观测（W2.6 同族）。
                    tracing::warn!(
                        "prepare_store: source turn {} timestamp lookup failed ({}); recency anchored to now",
                        turn_id,
                        e
                    );
                    chrono::Utc::now().timestamp_millis()
                })
        } else {
            chrono::Utc::now().timestamp_millis()
        };

        let turn_previews = self.load_turn_previews(source_turn_ids)?;
        let conversation_context =
            format_admission_context(&turn_previews, atoms.len(), source_turn_ids.len());

        Ok(StorePlan {
            entries: atoms.iter().enumerate().collect(),
            existing_contents,
            existing,
            snapshot_max_id,
            turn_ids_json,
            turn_timestamp_ms,
            conversation_context,
            embedding_dim: self.db.dimensions(),
            pending_audits: Vec::new(),
            embedded: Vec::new(),
            planned: Vec::new(),
        })
    }

    /// Stage 3 compatibility wrapper that flushes `MEMORY.md` synchronously.
    #[cfg(test)]
    pub fn commit_store<'b>(&self, plan: &mut StorePlan<'b>) -> anyhow::Result<Vec<StoredAtom>> {
        let (stored, pending_md) = self.commit_store_deferred_md(plan)?;
        if let Some(pending) = pending_md {
            if let Err(e) = pending.flush() {
                tracing::error!(
                    "store_atoms: failed to sync atoms to MEMORY.md (DB and .md have diverged; run `asuna-memory doctor --fix` to repair): {}",
                    e
                );
            }
        }
        Ok(stored)
    }

    /// Commit planned atoms to DB and run capacity eviction, returning a deferred
    /// `.md` write so callers holding a DB mutex can flush to disk after unlocking.
    pub fn commit_store_deferred_md<'b>(
        &self,
        plan: &mut StorePlan<'b>,
    ) -> anyhow::Result<(
        Vec<StoredAtom>,
        Option<crate::growth::bounded_memory::PendingMdWrite>,
    )> {
        if !plan.embedded.is_empty() && plan.planned.is_empty() {
            tracing::warn!(
                "commit_store called with {} embedded but unscored atoms — was execute_score skipped? Nothing will be stored",
                plan.embedded.len()
            );
        }

        // W9: 将重复筛选、未落库审计与活快照裁剪全部放进同一个 IMMEDIATE 事务，
        // 消除锁外校验→锁内写入之间的 TOCTOU 窗口。
        let tx = self.db.immediate_tx()?;
        let planned = std::mem::take(&mut plan.planned);
        let kept = self.filter_non_duplicate_planned(planned)?;
        self.flush_pending_audits(&mut plan.pending_audits);
        self.retain_live_atom_snapshots(&mut plan.existing)?;

        // W20: 补载 prepare 快照之后（锁外窗口）并发插入的活原子向量，
        // 否则 check_dedup 看不到它们 → 语义去重漏判并发近重复。
        let fresh = self.load_existing_embeddings(plan.snapshot_max_id)?;
        plan.existing.extend(fresh);
        let stored = self.store_planned(kept, &mut plan.existing, &plan.turn_ids_json)?;
        tx.commit()?;

        // W19: 本批未落库任何新原子（全部被跳过/去重）时不触发增长层渲染与驱逐。
        let pending_md = if stored.is_empty() {
            None
        } else {
            self.prepare_growth_layer()
        };
        Ok((stored, pending_md))
    }

    fn filter_non_duplicate_planned<'b>(
        &self,
        planned: Vec<(usize, &'b Atom, Vec<f32>)>,
    ) -> anyhow::Result<Vec<(usize, &'b Atom, Vec<f32>)>> {
        let current = self.load_existing_contents()?;

        let mut kept = Vec::with_capacity(planned.len());
        for (source_index, atom, embedding) in planned {
            // W2.3: 与 load_existing_contents 同口径（CRLF→LF 后比对）。
            let normalized = normalize_crlf(&atom.content);
            let trimmed = normalized.trim();
            if !trimmed.is_empty() && current.contains(trimmed) {
                tracing::debug!(
                    "Skipping atom duplicated during the lock-free window: {}",
                    log_safe_content(&atom.content)
                );
                if let Err(e) = crate::growth::audit::log_action(
                    self.db,
                    "duplicate_skip",
                    "memory",
                    &crate::growth::security::redact_credentials(&atom.content),
                    None,
                ) {
                    tracing::warn!("failed to audit duplicate_skip: {}", e);
                }
                continue;
            }
            kept.push((source_index, atom, embedding));
        }
        Ok(kept)
    }

    fn flush_pending_audits(&self, pending_audits: &mut Vec<(String, String)>) {
        for (action, detail) in pending_audits.drain(..) {
            if let Err(e) =
                crate::growth::audit::log_action(self.db, &action, "memory", &detail, None)
            {
                tracing::warn!("failed to audit {}: {}", action, e);
            }
        }
    }

    pub(super) fn retain_live_atom_snapshots(
        &self,
        existing: &mut Vec<(i64, Vec<f32>)>,
    ) -> anyhow::Result<()> {
        // W26: 复查失败必须上抛——吞错会把活 id 当死 id 剔出快照，
        // 语义去重随之漏判（健康主路径不触发，CORRUPT/IOERR 时才显形）。
        let live_ids: std::collections::HashSet<i64> = self
            .db
            .conn()
            .prepare(
                "SELECT bm.id FROM bounded_memory bm
                 INNER JOIN vec_bounded_memory vec ON bm.id = vec.id
                 WHERE COALESCE(bm.memory_type, 'manual') = 'atom'
                   AND NOT EXISTS (SELECT 1 FROM bounded_memory s WHERE s.supersedes_id = bm.id)",
            )?
            .query_map([], |row| row.get::<_, i64>(0))?
            .collect::<rusqlite::Result<_>>()?;
        existing.retain(|(id, _)| live_ids.contains(id));
        Ok(())
    }

    /// B8: 代际 bump 是 `prepare_sync_atoms_to_md` 的**首个**操作（growth/
    /// bounded_memory/format.rs，先于一切可错步骤；其前置 `target_file("memory")`
    /// 对字面量 "memory" 恒 Ok）——因此即使本函数返回 None（atom-store tx 已提交
    /// 但 sync 失败），代际也已前进，其他写者的旧 .md 快照 flush 时因代际落后
    /// 被跳过，不会覆盖新状态。唯一不 bump 的路径是 `bounded_memory == None`：
    /// 仅测试构造如此，生产 commit_l1_stage 恒 `.with_growth(...)`（service/
    /// pipeline）。回归钉：l1/tests.rs test_prepare_sync_failure_still_bumps_generation。
    fn prepare_growth_layer(&self) -> Option<crate::growth::bounded_memory::PendingMdWrite> {
        let bm = self.bounded_memory.as_ref()?;
        match bm.prepare_sync_atoms_to_md() {
            Ok((evicted, pending)) => {
                if evicted > 0 {
                    tracing::info!("store_atoms: evicted {} atoms from MEMORY.md", evicted);
                }
                Some(pending)
            }
            Err(e) => {
                tracing::error!(
                    "store_atoms: failed to sync atoms to MEMORY.md (DB and .md have diverged; run `asuna-memory doctor --fix` to repair): {}",
                    e
                );
                None
            }
        }
    }

    fn load_turn_previews(&self, source_turn_ids: &[i64]) -> anyhow::Result<Vec<(String, String)>> {
        if source_turn_ids.is_empty() {
            return Ok(Vec::new());
        }
        let ids = if source_turn_ids.len() > ADMISSION_CONTEXT_MAX_TURNS {
            &source_turn_ids[source_turn_ids.len() - ADMISSION_CONTEXT_MAX_TURNS..]
        } else {
            source_turn_ids
        };
        let placeholders = crate::index::sql_util::numbered_placeholders(ids.len());
        let sql = format!(
            "SELECT role, COALESCE(preview, '') FROM turns WHERE id IN ({placeholders}) ORDER BY seq"
        );
        let mut stmt = self.db.conn().prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(ids), |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        Ok(rows
            .filter_map(|row| match row {
                Ok(preview) => Some(preview),
                Err(e) => {
                    tracing::warn!(
                        "admission context turn row read error (skipping row): {}",
                        e
                    );
                    None
                }
            })
            .collect())
    }

    /// Load live atom vectors with `id > after_id` (0 = the full snapshot).
    fn load_existing_embeddings(&self, after_id: i64) -> anyhow::Result<Vec<(i64, Vec<f32>)>> {
        let conn = self.db.conn();
        let mut stmt = conn.prepare(
            "SELECT bm.id, vec.embedding
             FROM bounded_memory bm
             INNER JOIN vec_bounded_memory vec ON bm.id = vec.id
             WHERE COALESCE(bm.memory_type, 'manual') = 'atom'
               AND bm.id > ?1
               AND NOT EXISTS (SELECT 1 FROM bounded_memory s WHERE s.supersedes_id = bm.id)",
        )?;

        let embeddings = stmt.query_map(rusqlite::params![after_id], |row| {
            let id: i64 = row.get(0)?;
            let embedding_bytes: Vec<u8> = row.get(1)?;
            let embedding: Vec<f32> = embedding_bytes
                .iter()
                .map(|&b| (b as i8) as f32 / 127.0)
                .collect();
            Ok((id, embedding))
        })?;

        let mut result = Vec::new();
        for embedding in embeddings {
            result.push(embedding?);
        }

        tracing::debug!("Loaded {} existing L1 atom embeddings", result.len());
        Ok(result)
    }

    /// Trimmed contents of every live (non-superseded) `bounded_memory` row —
    /// the exact-text duplicate guard snapshot. Shared by `prepare_store` and
    /// the commit-time re-check so the two queries can never drift apart.
    fn load_existing_contents(&self) -> anyhow::Result<std::collections::HashSet<String>> {
        let mut stmt = self.db.conn().prepare(
            "SELECT bm.content FROM bounded_memory bm
             WHERE NOT EXISTS (
                 SELECT 1 FROM bounded_memory s WHERE s.supersedes_id = bm.id
             )",
        )?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut contents = std::collections::HashSet::new();
        for row in rows {
            match row {
                Ok(c) => {
                    contents.insert(normalize_crlf(&c).trim().to_string());
                }
                // 读不出的行不得静默消失于去重快照（漏判重复）——至少 warn，
                // 与 recall 各层逐行降级同口径。
                Err(e) => {
                    tracing::warn!("existing-contents row read error (skipping row): {}", e)
                }
            }
        }
        Ok(contents)
    }
}

/// Store-path logs share the warn/audit sanitization posture: redact
/// credentials, then cap length (atom content is conversation-derived and
/// may carry secrets — raw echoes in info/debug were the odd ones out).
pub(super) fn log_safe_content(content: &str) -> String {
    crate::util::truncate_for_log(&crate::growth::security::redact_credentials(content))
}

pub(super) fn format_admission_context(
    turn_previews: &[(String, String)],
    atoms_len: usize,
    source_turn_ids_len: usize,
) -> String {
    if turn_previews.is_empty() {
        return format!(
            "Processing {} atoms from {} turns",
            atoms_len, source_turn_ids_len
        );
    }
    // 迭代中按预算截断：不再先 join 全部（≤64 turn）预览成大中间串再
    // take——中间串至多 budget + 一行（输出与 join-then-take 逐字一致）。
    let mut out = String::new();
    for (role, preview) in turn_previews {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&format!("[{}] {}", role, preview));
        if out.chars().count() >= ADMISSION_CONTEXT_CHAR_BUDGET {
            break;
        }
    }
    out.chars().take(ADMISSION_CONTEXT_CHAR_BUDGET).collect()
}

use super::budget::cap_persona_content;
use super::cache::read_persona_fallback;
use super::{RetrievalEngine, CONSOLIDATION_FRESHNESS_MS, CONSOLIDATION_RENDER_CAP};
use crate::memory::intent_prediction::{load_anticipated_needs_from, load_likely_topics_from};
use crate::memory::mental_model::{
    load_communication_style_from, load_decision_framework_from, load_workflow_patterns_from,
};
use std::path::Path;

impl<'a> RetrievalEngine<'a> {
    #[cfg(test)]
    pub(super) fn recall_persona(&self) -> Vec<serde_json::Value> {
        self.recall_persona_with_budget(None)
    }

    /// L3 persona: all live DB `target='user'` rows (concatenated, newest first)
    /// → `persona.md` → `USER.md`, protected against budget starvation.
    pub(super) fn recall_persona_with_budget(
        &self,
        budget: Option<usize>,
    ) -> Vec<serde_json::Value> {
        let mut out = Vec::new();
        if let Some(combined) = self.load_db_persona_rows() {
            let capped = cap_persona_content(combined, budget);
            out.push(serde_json::json!({ "layer": "L3", "type": "persona", "content": capped }));
            return out;
        }
        // W28c: 有预载就用预载的文件回退（锁外已读），否则才在锁内同步读。
        let fallback = match self.preloaded_docs {
            Some(ref docs) => docs.persona_fallback.clone(),
            None => read_persona_fallback(&self.persona_md_path, &self.user_md_path),
        };
        if let Some(content) = fallback {
            let capped = cap_persona_content(content, budget);
            out.push(serde_json::json!({ "layer": "L3", "type": "persona", "content": capped }));
        }
        out
    }

    fn load_db_persona_rows(&self) -> Option<String> {
        let mut stmt = match self.db.conn().prepare(
            "SELECT bm.content FROM bounded_memory bm
             WHERE bm.target = 'user'
               AND bm.content IS NOT NULL AND bm.content != ''
               AND NOT EXISTS (SELECT 1 FROM bounded_memory s WHERE s.supersedes_id = bm.id)
             ORDER BY bm.updated_at DESC",
        ) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    target: "asuna_memory::memory::retrieval",
                    "recall L3 bounded_memory prepare error: {}",
                    e
                );
                return None;
            }
        };
        let parts: Vec<String> = match stmt.query_map([], |row| row.get::<_, String>(0)) {
            Ok(rows) => rows
                .filter_map(|row| match row {
                    Ok(s) => Some(s),
                    Err(e) => {
                        tracing::warn!(
                            target: "asuna_memory::memory::retrieval",
                            "recall L3 persona row read error (skipping row): {}",
                            e
                        );
                        None
                    }
                })
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
            Err(e) => {
                tracing::warn!(
                    target: "asuna_memory::memory::retrieval",
                    "recall L3 bounded_memory query error: {}",
                    e
                );
                return None;
            }
        };
        (!parts.is_empty()).then(|| parts.join("\n"))
    }

    /// L4 mental models (S14c): the three `mental_models/` documents, fresh
    /// ones only, rendered compact. Like every other layer: a read failure
    /// warns and the document is skipped — `/recall` never fails over them.
    pub(super) fn recall_mental_models(&self) -> Vec<serde_json::Value> {
        recall_mental_models_from(&self.memory_dir).0
    }

    /// L5 intent predictions (S14c): the two `intent/` documents, fresh
    /// ones only. Same degrade-to-skip semantics as L4.
    pub(super) fn recall_intents(&self) -> Vec<serde_json::Value> {
        recall_intents_from(&self.memory_dir).0
    }

    /// L2 scenarios (`memory_type='scenario'`), filtered by optional time window and capped at `top_k`.
    pub(super) fn recall_scenarios(
        &self,
        after: Option<i64>,
        before: Option<i64>,
        top_k: usize,
    ) -> Vec<serde_json::Value> {
        if top_k == 0 {
            return Vec::new();
        }
        // M6b: target='memory' 与写侧（l2.rs 恒写 'memory'）及其余读取面
        // （recall_atoms FTS/vec 两腿）同口径，排除任何非 memory 目标的杂散行。
        let mut sql = String::from(
            "SELECT content, updated_at FROM bounded_memory
             WHERE target = 'memory'
               AND COALESCE(memory_type, 'manual') = 'scenario'
               AND NOT EXISTS (SELECT 1 FROM bounded_memory s WHERE s.supersedes_id = bounded_memory.id)",
        );
        let mut binds: Vec<i64> = Vec::new();
        let mut next = 1usize;
        if let Some(a) = after {
            sql.push_str(&format!(" AND updated_at >= ?{}", next));
            next += 1;
            binds.push(a);
        }
        if let Some(b) = before {
            sql.push_str(&format!(" AND updated_at <= ?{}", next));
            next += 1;
            binds.push(b);
        }
        sql.push_str(&format!(" ORDER BY updated_at DESC LIMIT ?{}", next));
        binds.push(top_k as i64);

        let refs: Vec<&dyn rusqlite::types::ToSql> = binds
            .iter()
            .map(|v| v as &dyn rusqlite::types::ToSql)
            .collect();
        let rows = match self.db.conn().prepare(&sql) {
            Ok(mut stmt) => match stmt.query_map(refs.as_slice(), |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            }) {
                Ok(scenarios) => scenarios
                    .filter_map(|row| match row {
                        Ok(s) => Some(s),
                        Err(e) => {
                            tracing::warn!(
                                target: "asuna_memory::memory::retrieval",
                                "recall L2 scenario row read error (skipping row): {}",
                                e
                            );
                            None
                        }
                    })
                    .collect::<Vec<_>>(),
                Err(e) => {
                    tracing::warn!(
                        target: "asuna_memory::memory::retrieval",
                        "recall L2 scenario query error (skipping L2 layer): {}",
                        e
                    );
                    Vec::new()
                }
            },
            Err(e) => {
                tracing::warn!(
                    target: "asuna_memory::memory::retrieval",
                    "recall L2 scenario prepare error (skipping L2 layer): {}",
                    e
                );
                Vec::new()
            }
        };
        rows.into_iter()
            .map(|(content, updated_at)| {
                serde_json::json!({
                    "layer": "L2",
                    "type": "scenario",
                    "content": content,
                    "updated_at": updated_at
                })
            })
            .collect()
    }
}

/// C13 / W13: 返回 `(items, earliest_expiry_ms, had_io_error)`——最早过期时刻供 preload
/// 缓存判定何时必须重读重 gate，并标记是否发生了可观测 I/O 错误（非 `NotFound`）。
pub(super) fn recall_mental_models_from(dir: &Path) -> (Vec<serde_json::Value>, i64, bool) {
    let mut out = Vec::new();
    let mut expires = i64::MAX;
    let mut had_error = false;
    for (doc_type, title, loaded) in [
        (
            "workflow_patterns",
            "Workflow Patterns",
            load_workflow_patterns_from(dir).map(|d| d.map(|d| (d.patterns, d.updated_at))),
        ),
        (
            "decision_framework",
            "Decision Framework",
            load_decision_framework_from(dir).map(|d| d.map(|d| (d.criteria, d.updated_at))),
        ),
        (
            "communication_style",
            "Communication Style",
            load_communication_style_from(dir).map(|d| d.map(|d| (d.preferences, d.updated_at))),
        ),
    ] {
        let (opt_exp, err) = push_consolidation_doc(&mut out, "L4", doc_type, title, loaded);
        had_error |= err;
        if let Some(exp) = opt_exp {
            expires = expires.min(exp);
        }
    }
    (out, expires, had_error)
}

/// C13 / W13: 同 [`recall_mental_models_from`] 的 `(items, 最早过期, had_error)` 口径。
pub(super) fn recall_intents_from(dir: &Path) -> (Vec<serde_json::Value>, i64, bool) {
    let mut out = Vec::new();
    let mut expires = i64::MAX;
    let mut had_error = false;
    for (doc_type, title, loaded) in [
        (
            "likely_topics",
            "Likely Next Topics",
            load_likely_topics_from(dir).map(|d| d.map(|d| (d.topics, d.updated_at))),
        ),
        (
            "anticipated_needs",
            "Anticipated Needs",
            load_anticipated_needs_from(dir).map(|d| d.map(|d| (d.needs, d.updated_at))),
        ),
    ] {
        let (opt_exp, err) = push_consolidation_doc(&mut out, "L5", doc_type, title, loaded);
        had_error |= err;
        if let Some(exp) = opt_exp {
            expires = expires.min(exp);
        }
    }
    (out, expires, had_error)
}

/// Shared gate/render/push for one L4/L5 consolidation document: absent,
/// unreadable (warn), item-less or stale documents contribute nothing — the
/// recall chain never fails over them (same posture as the L2/L1 layers).
/// Returns `(earliest_expiry_ms, had_error)`.
fn push_consolidation_doc(
    memories: &mut Vec<serde_json::Value>,
    layer: &str,
    doc_type: &str,
    title: &str,
    loaded: anyhow::Result<Option<(Vec<String>, i64)>>,
) -> (Option<i64>, bool) {
    let (items, updated_at) = match loaded {
        Ok(Some(doc)) => doc,
        Ok(None) => return (None, false),
        Err(e) => {
            tracing::warn!(
                target: "asuna_memory::memory::retrieval",
                "recall {} {} load error (skipping document): {}",
                layer,
                doc_type,
                e
            );
            return (None, true);
        }
    };
    if items.is_empty() {
        return (None, false);
    }
    if !consolidation_fresh(updated_at) {
        tracing::debug!(
            target: "asuna_memory::memory::retrieval",
            "recall {} {} stale (updated_at={}s), skipping",
            layer,
            doc_type,
            updated_at
        );
        return (None, false);
    }
    memories.push(serde_json::json!({
        "layer": layer, "type": doc_type,
        "content": render_consolidation_doc(title, &items),
    }));
    // C13: 过期时刻 = updated_ms + FRESHNESS。对 future stamp（时钟偏移）同样
    // 保守成立：偏移容忍期内恒 fresh，越过 updated_ms 后按正常年龄规则继续
    // fresh 满 FRESHNESS——期间任何时刻重 gate 都不会比缓存更宽。
    (
        Some(
            updated_at
                .saturating_mul(1000)
                .saturating_add(CONSOLIDATION_FRESHNESS_MS),
        ),
        false,
    )
}

/// Freshness gate ([`CONSOLIDATION_FRESHNESS_MS`]). `updated_at` is unix
/// SECONDS (the `Updated:` line / file mtime — see
/// `mental_model::load_list_md`), scaled to ms here. `0` — the loaders'
/// "cannot date this document" value (S14a: pre-J9 files, unstatable
/// mtime) — counts as NOT fresh: a document that cannot tell when it was
/// written cannot claim to be current. Future stamps (clock skew) read as
/// fresh only within `FUTURE_SKEW_TOLERANCE_MS` (24h); beyond that they are
/// stale, so a hand-edited "Updated:" line cannot bypass the expiry forever.
pub(super) fn consolidation_fresh(updated_at_secs: i64) -> bool {
    if updated_at_secs <= 0 {
        return false;
    }
    let updated_ms = updated_at_secs.saturating_mul(1000);
    let now_ms = crate::util::time::now_unix_ms();
    // Future stamps: tolerate small clock skew (24h); beyond that treat the
    // document as stale rather than eternally fresh — a hand-edited or
    // skew-written "Updated:" line must not bypass the expiry gate forever
    // (S14c NB3; asymmetric-with-0 handling closed).
    const FUTURE_SKEW_TOLERANCE_MS: i64 = 24 * 60 * 60 * 1000;
    if updated_ms > now_ms {
        return updated_ms - now_ms <= FUTURE_SKEW_TOLERANCE_MS;
    }
    now_ms - updated_ms <= CONSOLIDATION_FRESHNESS_MS
}

/// Compact one-line render of a list document (`Title: a; b; …`), capped at
/// [`CONSOLIDATION_RENDER_CAP`] chars (ellipsis included; keeps the item
/// small enough for the greedy budget to treat L4/L5 as cheap top-of-chain
/// context rather than anchors that crowd out L2/L1).
pub(super) fn render_consolidation_doc(title: &str, items: &[String]) -> String {
    let mut out = String::from(title);
    out.push_str(": ");
    out.push_str(&items.join("; "));
    if out.chars().count() > CONSOLIDATION_RENDER_CAP {
        out = out.chars().take(CONSOLIDATION_RENDER_CAP - 1).collect();
        out.push('…');
    }
    out
}

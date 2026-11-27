//! L1 三段式 store 的中间阶段载体 `StorePlan`：精确去重/安全扫描门 + 批量嵌入
//! （逐原子回退）+ A-MAC 准入打分。M14 自 plan.rs 拆出；不借用 Db。

use super::log_safe_content;
use crate::embedder::LazyEmbedder;
use crate::growth::bounded_memory::normalize_crlf;
use crate::memory::admission::AdmissionScorer;
use crate::memory::l1::Atom;

/// Middle stages of the three-stage L1 store (no DB handle borrowed).
pub struct StorePlan<'b> {
    pub(super) entries: Vec<(usize, &'b Atom)>,
    pub(super) existing_contents: std::collections::HashSet<String>,
    pub(in crate::memory::l1) existing: Vec<(i64, Vec<f32>)>,
    /// Highest live atom id captured by `prepare_store` (W20 reload watermark).
    pub(super) snapshot_max_id: i64,
    pub(super) turn_ids_json: String,
    pub(in crate::memory::l1) turn_timestamp_ms: i64,
    pub(in crate::memory::l1) conversation_context: String,
    pub(super) embedding_dim: usize,
    pub(super) pending_audits: Vec<(String, String)>,
    pub(in crate::memory::l1) embedded: Vec<(usize, &'b Atom, Vec<f32>)>,
    pub(in crate::memory::l1) planned: Vec<(usize, &'b Atom, Vec<f32>)>,
}

impl<'b> StorePlan<'b> {
    fn screen_atom_for_embed(&mut self, atom: &'b Atom) -> bool {
        // W2.3: 精确去重按 CRLF→LF 归一后的形态登记（入库同形）。
        let normalized = normalize_crlf(&atom.content);
        let trimmed = normalized.trim();
        if trimmed.is_empty() {
            return false;
        }
        // W21: L1 走直接 SQL 入库，绕过 write() 的分隔符守卫
        if crate::growth::bounded_memory::has_entry_separator(&atom.content) {
            let redacted = crate::growth::security::redact_credentials(&atom.content);
            tracing::warn!(
                "Skipping atom containing the MEMORY.md entry separator: {}",
                crate::util::truncate_for_log(&redacted)
            );
            self.pending_audits
                .push(("separator_skip".to_string(), redacted));
            return false;
        }
        // C5: 先扫描后登记 existing_contents
        let scan = crate::growth::security::scan_content(&atom.content);
        if !scan.is_safe() {
            let redacted = crate::growth::security::redact_credentials(&atom.content);
            tracing::warn!(
                "Skipping atom flagged by security scan ({}): {}",
                scan.reason(),
                crate::util::truncate_for_log(&redacted)
            );
            self.pending_audits
                .push(("security_scan_skip".to_string(), redacted));
            return false;
        }
        if !self.existing_contents.insert(trimmed.to_string()) {
            tracing::debug!(
                "Skipping exact-duplicate atom: {}",
                log_safe_content(&atom.content)
            );
            self.pending_audits.push((
                "duplicate_skip".to_string(),
                crate::growth::security::redact_credentials(&atom.content),
            ));
            return false;
        }
        true
    }

    /// Stage 2a (no DB lock; only stage that may run under the embedder mutex):
    /// run exact-text and security-scan gates, then batch-embed with per-atom fallback.
    pub fn execute_embed(&mut self, embedder: Option<&LazyEmbedder>) -> anyhow::Result<()> {
        let entries = std::mem::take(&mut self.entries);
        let mut survivors: Vec<(usize, &'b Atom)> = Vec::new();
        for &(source_index, atom) in &entries {
            if self.screen_atom_for_embed(atom) {
                survivors.push((source_index, atom));
            }
        }
        self.entries = entries;

        let embedded = self.embed_survivors(embedder, &survivors);
        self.embedded = embedded;
        Ok(())
    }

    fn embed_survivors(
        &self,
        embedder: Option<&LazyEmbedder>,
        survivors: &[(usize, &'b Atom)],
    ) -> Vec<(usize, &'b Atom, Vec<f32>)> {
        if survivors.is_empty() {
            return Vec::new();
        }
        let Some(embedder) = embedder else {
            tracing::warn!(
                "No embedder available, returning zero vector (dim={})",
                self.embedding_dim
            );
            return survivors
                .iter()
                .map(|&(source_index, atom)| (source_index, atom, vec![0.0; self.embedding_dim]))
                .collect();
        };
        let texts: Vec<&str> = survivors.iter().map(|(_, a)| a.content.as_str()).collect();
        match embedder.embed_documents(&texts) {
            Ok(vecs) if vecs.len() == texts.len() => survivors
                .iter()
                .copied()
                .zip(vecs)
                .map(|((source_index, atom), embedding)| (source_index, atom, embedding))
                .collect(),
            Ok(vecs) => {
                tracing::warn!(
                    "Embedding batch returned {} vectors for {} atoms; falling back to per-atom embedding",
                    vecs.len(),
                    texts.len()
                );
                embed_survivors_per_atom(embedder, survivors, self.embedding_dim)
            }
            Err(e) => {
                tracing::warn!(
                    "Embedding batch failed ({}); falling back to per-atom embedding",
                    e
                );
                embed_survivors_per_atom(embedder, survivors, self.embedding_dim)
            }
        }
    }

    /// Stage 2b (no locks at all): A-MAC admission scoring of `execute_embed` output.
    pub fn execute_score(&mut self, admission: Option<&AdmissionScorer<'_>>) -> anyhow::Result<()> {
        let mut embedded = std::mem::take(&mut self.embedded);
        if let Some(scorer) = admission {
            retain_admitted_atoms(
                &mut embedded,
                &self.existing,
                &self.conversation_context,
                self.turn_timestamp_ms,
                scorer,
            );
        }

        self.planned = embedded;
        Ok(())
    }
}

fn retain_admitted_atoms(
    embedded: &mut Vec<(usize, &Atom, Vec<f32>)>,
    existing: &[(i64, Vec<f32>)],
    conversation_context: &str,
    turn_timestamp_ms: i64,
    scorer: &AdmissionScorer<'_>,
) {
    let existing_refs: Vec<&[f32]> = existing.iter().map(|(_, e)| e.as_slice()).collect();
    let inputs: Vec<crate::memory::admission::BatchScoreInput<'_>> = embedded
        .iter()
        .map(|(_, atom, emb)| crate::memory::admission::BatchScoreInput {
            content: &atom.content,
            atom_type: &atom.atom_type,
            embedding: emb.as_slice(),
        })
        .collect();
    let outcomes = scorer.score_batch(
        &inputs,
        &existing_refs,
        conversation_context,
        turn_timestamp_ms,
    );
    if outcomes.len() != embedded.len() {
        // fail-closed：无对应 outcome 的原子被丢弃，但必须留痕
        tracing::warn!(
            "admission returned {} outcomes for {} embedded atoms; the unmatched tail is dropped (fail-closed)",
            outcomes.len(),
            embedded.len()
        );
    }
    let keep: Vec<bool> = embedded
        .iter()
        .zip(outcomes)
        .map(|((_, atom, _), outcome)| outcome_is_admitted(atom, outcome, scorer.threshold()))
        .collect();
    let mut keep_iter = keep.into_iter();
    embedded.retain(|_| keep_iter.next().unwrap_or(false));
}

fn outcome_is_admitted(
    atom: &Atom,
    outcome: anyhow::Result<crate::memory::admission::AdmissionScore>,
    threshold: f64,
) -> bool {
    match outcome {
        Ok(result) if result.admitted => true,
        Ok(result) => {
            tracing::info!(
                "Atom rejected by admission (score={:.2}, threshold={:.2}): {}",
                result.score,
                threshold,
                log_safe_content(&atom.content)
            );
            false
        }
        Err(e) => {
            tracing::warn!(
                "Skipping atom after admission scoring failure ({}): {}",
                e,
                log_safe_content(&atom.content)
            );
            false
        }
    }
}

/// C2: 逐原子回退路径。嵌入失败**不得丢弃原子**——与"未配置嵌入器"分支同姿势
/// 落零向量：`index_atom_vector` 会跳过零/非有限向量不写索引，但 bounded_memory
/// 行照常入库，启动时 `maybe_backfill_bounded_memory_vec` 以当前标度重嵌入。
/// 这正是 for_ai.md §3 承诺的 "degrades to vectorless persistence"。旧实现直接
/// 丢弃，嵌入 API 短暂不可达即永久丢失该会话全部 L1 记忆（无自动重抽取路径）。
fn embed_survivors_per_atom<'b>(
    embedder: &LazyEmbedder,
    survivors: &[(usize, &'b Atom)],
    dim: usize,
) -> Vec<(usize, &'b Atom, Vec<f32>)> {
    let mut embedded = Vec::with_capacity(survivors.len());
    let mut degraded = 0usize;
    for &(source_index, atom) in survivors {
        match embedder.embed_document(&atom.content) {
            Ok(embedding) => embedded.push((source_index, atom, embedding)),
            Err(e) => {
                degraded += 1;
                tracing::warn!(
                    "Atom embedding failed ({e}); persisting vectorless for later backfill: {}",
                    log_safe_content(&atom.content)
                );
                embedded.push((source_index, atom, vec![0.0; dim]));
            }
        }
    }
    if degraded > 0 {
        tracing::warn!(
            "{degraded}/{} atoms persisted vectorless after embedding failure; \
             startup backfill will re-embed them",
            survivors.len()
        );
    }
    embedded
}

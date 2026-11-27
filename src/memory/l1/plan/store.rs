//! L1 落库写面：语义去重（KNN + 漂移界认证）、冲突/共存判定与原子入库。
//! M14 自 plan.rs 拆出；prepare/commit 编排与快照加载仍在 plan.rs。

use super::log_safe_content;
use crate::embedder::quantize_to_int8;
use crate::growth::bounded_memory::normalize_crlf;
use crate::memory::dedup::{check_dedup, cosine_similarity, DedupResult};
use crate::memory::l1::{Atom, L1Extractor, StoredAtom};

/// W26c: KNN 候选宽帽。vec0 以**量化查询**排序，与精确 f32 cosine 在近并列处
/// 可能有微小排序扰动——取宽候选集后用原查询精确复算，再以漂移界认证
/// （见 dedup_via_knn）；认证不成立时回退全快照精确扫描，判定逐字不变。
const DEDUP_KNN_CANDIDATES: i64 = 32;

/// W26c: 认证余量——覆盖 f32 逐分量累加（check_dedup 与 vec0 距离两侧）的
/// 最坏舍入误差（n=1024 时 ~n·u 量级，u=f32 单元舍入），取 1e-3 留足裕度。
const DEDUP_CERT_EPSILON: f64 = 1e-3;

/// W26c: KNN 原始行 `(id, distance, 去量化向量, live)`，按距离升序。
type DedupKnnRow = (i64, f64, Vec<f32>, bool);

impl<'a> L1Extractor<'a> {
    fn store_conflict_or_coexist(
        &self,
        atom: &Atom,
        embedding: &[f32],
        existing_id: i64,
        similarity: f32,
        existing: &mut Vec<(i64, Vec<f32>)>,
        turn_ids_json: &str,
    ) -> anyhow::Result<(i64, Option<i64>)> {
        let superseding =
            self.conflict_may_supersede(sanitize_atom_confidence(atom.confidence), existing_id)?;
        if superseding {
            let new_id = self.store_conflicting_atom(
                atom,
                embedding,
                existing_id,
                similarity,
                existing,
                turn_ids_json,
            )?;
            return Ok((new_id, Some(existing_id)));
        }
        tracing::info!(
            "Conflict not superseded (confidence gate or vanished row; cosine={:.3}, new_confidence={:.2}): storing coexisting atom (existing_id={}): {}",
            similarity,
            atom.confidence,
            existing_id,
            log_safe_content(&atom.content)
        );
        let new_id = self.store_unique_atom(atom, embedding, existing, turn_ids_json)?;
        Ok((new_id, None))
    }

    fn audit_semantic_duplicate_skip(&self, content: &str, existing_id: i64) {
        tracing::debug!(
            "Skipping duplicate atom (existing_id={}): {}",
            existing_id,
            log_safe_content(content)
        );
        if let Err(e) = crate::growth::audit::log_action(
            self.db,
            "duplicate_skip",
            "memory",
            &crate::growth::security::redact_credentials(content),
            None,
        ) {
            tracing::warn!("failed to audit duplicate_skip: {}", e);
        }
    }

    pub(super) fn store_planned(
        &self,
        planned: Vec<(usize, &Atom, Vec<f32>)>,
        existing: &mut Vec<(i64, Vec<f32>)>,
        turn_ids_json: &str,
    ) -> anyhow::Result<Vec<StoredAtom>> {
        let mut stored = Vec::new();
        for (source_index, atom, embedding) in planned {
            match self.dedup_via_knn(&embedding, existing)? {
                DedupResult::Duplicate { existing_id } => {
                    self.audit_semantic_duplicate_skip(&atom.content, existing_id);
                }
                DedupResult::Conflict {
                    existing_id,
                    similarity,
                } => {
                    let (id, supersedes_id) = self.store_conflict_or_coexist(
                        atom,
                        &embedding,
                        existing_id,
                        similarity,
                        existing,
                        turn_ids_json,
                    )?;
                    stored.push(StoredAtom {
                        source_index,
                        id,
                        supersedes_id,
                        embedding,
                    });
                }
                DedupResult::Unique => {
                    let id = self.store_unique_atom(atom, &embedding, existing, turn_ids_json)?;
                    stored.push(StoredAtom {
                        source_index,
                        id,
                        supersedes_id: None,
                        embedding,
                    });
                }
            }
        }
        Ok(stored)
    }

    /// W26c: 以 `vec_bounded_memory` KNN 取代逐原子全量暴力余弦扫描。
    /// 精确性契约（与旧 check_dedup(existing) 逐字同判）：
    /// 1. 候选用**原 f32 查询 × 去量化存储向量**经同一 `cosine_similarity` /
    ///    同一 0.95/0.80 阈值复算（KNN 距离只用于取候选，不参与判定）；
    /// 2. 存活原子过滤与 superseded 排除与 load_existing_embeddings 同谓词；
    /// 3. 量化查询排序扰动由漂移界 δ 认证：帽外任一行的精确 cosine ≤
    ///    第 K 行量化 cosine + δ。best_sim 越过该界 → argmax 必在候选内；
    ///    界 ≤ 0.80 → 帽外无行能进冲突带（Unique 认证）。两者皆不成立 →
    ///    回退维护中的全快照 `existing` 精确扫描（旧路径，零额外读盘）。
    ///
    /// 事务内 KNN 可见同批已插入/退索引的向量（与 existing 快照同步）。
    fn dedup_via_knn(
        &self,
        embedding: &[f32],
        existing: &[(i64, Vec<f32>)],
    ) -> anyhow::Result<DedupResult> {
        // 无效向量（维度不符/全零/非有限）在 check_dedup 下 cosine 恒 0 →
        // Unique；不发 KNN（零查询的 vec0 距离无定义）。M13a 同判据。
        if embedding.len() != self.db.dimensions()
            || crate::index::vector::is_invalid_vector(embedding)
        {
            return Ok(DedupResult::Unique);
        }
        let q_bytes = quantize_to_int8(embedding);
        let rows = self.load_dedup_knn_rows(&q_bytes)?;
        // 穷尽：vec 表总行数 < K → 候选集（过滤后）即全体活原子，结果精确。
        if (rows.len() as i64) < DEDUP_KNN_CANDIDATES {
            let pairs = live_knn_pairs(&rows);
            return Ok(check_dedup(embedding, &pairs));
        }
        let Some(&(_, kth_distance, _, _)) = rows.last() else {
            return Ok(check_dedup(embedding, existing));
        };
        if !kth_distance.is_finite() {
            return Ok(check_dedup(embedding, existing));
        }
        let pairs = live_knn_pairs(&rows);
        let best = check_dedup(embedding, &pairs);
        let best_sim = pairs
            .iter()
            .map(|(_, v)| cosine_similarity(embedding, v))
            .fold(0.0f32, f32::max);
        let margin = (1.0 - kth_distance)
            + quantization_drift_bound(embedding, &q_bytes)
            + DEDUP_CERT_EPSILON;
        if f64::from(best_sim) > margin || margin <= 0.80 {
            return Ok(best);
        }
        Ok(check_dedup(embedding, existing))
    }

    /// W26c: KNN 原始行（含被过滤行——穷尽判定与第 K 行距离必须基于**未过滤**
    /// 的帽集，否则近邻全是非 atom/被取代行时会把空候选误当穷尽）。
    /// 返回 `(id, distance, 去量化向量, live)`，按距离升序。
    fn load_dedup_knn_rows(&self, q_bytes: &[u8]) -> anyhow::Result<Vec<DedupKnnRow>> {
        let mut stmt = self.db.conn().prepare(
            "WITH knn AS (
                 SELECT id, distance
                 FROM vec_bounded_memory
                 WHERE embedding MATCH vec_int8(?1)
                   AND k = ?2
             )
             SELECT knn.id, knn.distance, vec.embedding,
                    (COALESCE(bm.memory_type, 'manual') = 'atom'
                     AND NOT EXISTS (
                         SELECT 1 FROM bounded_memory s WHERE s.supersedes_id = bm.id
                     )) AS live
             FROM knn
             JOIN vec_bounded_memory vec ON vec.id = knn.id
             LEFT JOIN bounded_memory bm ON bm.id = knn.id
             ORDER BY knn.distance ASC",
        )?;
        let rows = stmt.query_map(rusqlite::params![q_bytes, DEDUP_KNN_CANDIDATES], |row| {
            let id: i64 = row.get(0)?;
            let distance: f64 = row.get(1)?;
            let embedding_bytes: Vec<u8> = row.get(2)?;
            let live: i64 = row.get(3)?;
            let embedding: Vec<f32> = embedding_bytes
                .iter()
                .map(|&b| (b as i8) as f32 / 127.0)
                .collect();
            Ok((id, distance, embedding, live != 0))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub(in crate::memory::l1) fn conflict_may_supersede(
        &self,
        new_confidence: f64,
        existing_id: i64,
    ) -> anyhow::Result<bool> {
        match self.db.conn().query_row(
            "SELECT confidence FROM bounded_memory WHERE id = ?1",
            rusqlite::params![existing_id],
            |r| r.get::<_, Option<String>>(0),
        ) {
            Ok(level) => Ok(should_supersede(
                new_confidence,
                level.as_deref().unwrap_or(""),
            )),
            Err(rusqlite::Error::QueryReturnedNoRows) => {
                tracing::debug!(
                    "Conflict against already-deleted row id={}: storing as unique (no chain)",
                    existing_id
                );
                Ok(false)
            }
            Err(e) => Err(e.into()),
        }
    }

    fn store_conflicting_atom(
        &self,
        atom: &Atom,
        embedding: &[f32],
        existing_id: i64,
        similarity: f32,
        existing: &mut Vec<(i64, Vec<f32>)>,
        turn_ids_json: &str,
    ) -> anyhow::Result<i64> {
        let conf = sanitize_atom_confidence(atom.confidence);
        // W2.3: L1 直插绕过 BoundedMemory::write，入库前自行 CRLF→LF 归一。
        let content = normalize_crlf(&atom.content);
        let new_id = crate::memory::chain::create_superseding(
            self.db,
            "memory",
            &content,
            "atom",
            conf,
            Some(turn_ids_json),
            existing_id,
        )?;
        tracing::info!(
            "Supersedes chain created: new_id={} superseded_id={} cosine={:.3}: {}",
            new_id,
            existing_id,
            similarity,
            log_safe_content(&atom.content)
        );

        self.db.conn().execute(
            "DELETE FROM vec_bounded_memory WHERE id = ?1",
            rusqlite::params![existing_id],
        )?;
        existing.retain(|(id, _)| *id != existing_id);

        self.index_atom_vector(new_id, embedding, existing)?;
        Ok(new_id)
    }

    fn store_unique_atom(
        &self,
        atom: &Atom,
        embedding: &[f32],
        existing: &mut Vec<(i64, Vec<f32>)>,
        turn_ids_json: &str,
    ) -> anyhow::Result<i64> {
        let now = crate::util::time::now_unix_ms();
        let conf = sanitize_atom_confidence(atom.confidence);
        self.db.conn().execute(
            "INSERT INTO bounded_memory
             (target, content, created_at, updated_at, confidence, confidence_score,
              memory_type, source_turn_ids)
             VALUES ('memory', ?1, ?2, ?2, ?3, ?4, 'atom', ?5)",
            rusqlite::params![
                normalize_crlf(&atom.content),
                now,
                crate::memory::confidence_text(conf),
                conf,
                turn_ids_json,
            ],
        )?;
        let id = self.db.conn().last_insert_rowid();

        self.index_atom_vector(id, embedding, existing)?;
        tracing::info!(
            "Stored unique atom (id={}): {}",
            id,
            log_safe_content(&atom.content)
        );
        Ok(id)
    }

    /// Index a finite, non-zero, dimension-matching embedding into
    /// `vec_bounded_memory` and update the in-batch snapshot. Zero, non-finite
    /// or wrong-dimension vectors are skipped so undefined cosine distance
    /// never corrupts KNN ordering (and a malformed embedder output cannot
    /// FK-veto the whole batch via the vec0 dimension check).
    fn index_atom_vector(
        &self,
        id: i64,
        embedding: &[f32],
        existing: &mut Vec<(i64, Vec<f32>)>,
    ) -> anyhow::Result<()> {
        // W2.4 同族 / M13a：NaN/Inf 分量与全零向量一样会毒化 cosine KNN——
        // 统一走 is_invalid_vector（与召回/聚类守卫同判据；旧 `any(v != 0.0)`
        // 对 NaN 恒真放行）。维度不符同样跳过（vec0 会硬拒，不得回滚整批）。
        if embedding.len() == self.db.dimensions()
            && !crate::index::vector::is_invalid_vector(embedding)
        {
            let embedding_bytes = quantize_to_int8(embedding);
            self.db.conn().execute(
                "INSERT INTO vec_bounded_memory (id, embedding) VALUES (?1, vec_int8(?2))",
                rusqlite::params![id, embedding_bytes],
            )?;
            existing.push((id, embedding.to_vec()));
        }
        Ok(())
    }
}

/// W26c: 候选中的存活原子对，按 id 升序——与全快照扫描的平局取首（最小 id）
/// 口径一致（check_dedup 严格 `>` 保留先见者）。
fn live_knn_pairs(rows: &[DedupKnnRow]) -> Vec<(i64, Vec<f32>)> {
    let mut pairs: Vec<(i64, Vec<f32>)> = rows
        .iter()
        .filter(|(_, _, _, live)| *live)
        .map(|(id, _, v, _)| (*id, v.clone()))
        .collect();
    pairs.sort_by_key(|(id, _)| *id);
    pairs
}

/// W26c: 量化漂移界 δ = ||unit(query) - unit(dequant(q_bytes))||₂。
/// 对任一单位存储向量 s：|cos_exact(query,s) - cos_knn(query,s)| =
/// |(unit(query)-unit(q_int8))·s| ≤ δ（Cauchy-Schwarz）。cosine 尺度无关，
/// 故 absmax 量化标度与 /127 去量化不影响方向。f64 累加避免界本身被舍入侵蚀。
fn quantization_drift_bound(query: &[f32], q_bytes: &[u8]) -> f64 {
    let norm = |v: &[f64]| v.iter().map(|x| x * x).sum::<f64>().sqrt();
    let q: Vec<f64> = query.iter().map(|&x| f64::from(x)).collect();
    let d: Vec<f64> = q_bytes
        .iter()
        .map(|&b| f64::from(b as i8) / 127.0)
        .collect();
    let (nq, nd) = (norm(&q), norm(&d));
    if nq == 0.0 || nd == 0.0 {
        // 理论上不可达（无效向量已早退）；保守上界 2 → 认证必失败 → 全扫描。
        return 2.0;
    }
    q.iter()
        .zip(&d)
        .map(|(a, b)| (a / nq - b / nd).powi(2))
        .sum::<f64>()
        .sqrt()
}

pub(in crate::memory::l1) fn confidence_level_order(level: &str) -> u8 {
    match level.trim().to_ascii_lowercase().as_str() {
        "high" => 3,
        "medium" => 2,
        "low" => 1,
        _ => 0,
    }
}

pub(in crate::memory::l1) fn should_supersede(new_confidence: f64, old_level: &str) -> bool {
    confidence_level_order(crate::memory::confidence_text(new_confidence))
        >= confidence_level_order(old_level)
}

fn sanitize_atom_confidence(raw: f64) -> f64 {
    if raw.is_finite() {
        raw.clamp(0.0, 1.0)
    } else {
        0.5
    }
}

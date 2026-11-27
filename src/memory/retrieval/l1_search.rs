use super::RetrievalEngine;
use crate::util::rrf::RrfAccumulator;
use crate::util::text::{build_safe_fts_or_query, build_safe_fts_query};

/// Maximum cosine distance allowed for L1 vector KNN candidates.
const L1_VEC_MAX_COSINE_DISTANCE: f64 = 0.75;

#[derive(Clone)]
struct AtomRow {
    id: i64,
    content: String,
    confidence: f64,
    memory_type: String,
    created_at: i64,
    updated_at: i64,
}

impl<'a> RetrievalEngine<'a> {
    /// L1 atoms: hybrid FTS5 (`bounded_memory_fts`) + `vec_bounded_memory` KNN fused via RRF.
    pub(super) fn recall_atoms(
        &self,
        query: &str,
        query_vec: Option<&[f32]>,
        after: Option<i64>,
        before: Option<i64>,
        top_k: usize,
    ) -> Vec<serde_json::Value> {
        if top_k == 0 {
            return Vec::new();
        }
        let candidate_k = top_k.saturating_mul(2).clamp(20, 100).max(top_k);
        let fts_limit = if query_vec.is_some() {
            candidate_k
        } else {
            top_k
        };
        let fts_rows = self.recall_atoms_fts(query, after, before, fts_limit);
        let vec_rows = match query_vec {
            Some(qv) => self.recall_atoms_vec(qv, after, before, candidate_k),
            None => Vec::new(),
        };
        fuse_l1_atoms(fts_rows, vec_rows, top_k)
    }

    fn recall_atoms_fts(
        &self,
        query: &str,
        after: Option<i64>,
        before: Option<i64>,
        limit: usize,
    ) -> Vec<AtomRow> {
        let Some(and_expr) = build_safe_fts_query(query) else {
            return Vec::new();
        };
        let rows = self.run_atoms_fts_query(&and_expr, after, before, limit);
        if !rows.is_empty() {
            return rows;
        }
        if let Some(or_expr) = build_safe_fts_or_query(query) {
            return self.run_atoms_fts_query(&or_expr, after, before, limit);
        }
        Vec::new()
    }

    fn run_atoms_fts_query(
        &self,
        fts_query: &str,
        after: Option<i64>,
        before: Option<i64>,
        limit: usize,
    ) -> Vec<AtomRow> {
        let mut sql = String::from(
            "SELECT bm.id, bm.content,
                    CASE bm.confidence WHEN 'high' THEN 1.0 WHEN 'medium' THEN 0.5 ELSE 0.25 END,
                    COALESCE(bm.memory_type, 'manual'),
                    bm.created_at,
                    bm.updated_at
             FROM bounded_memory bm
             JOIN bounded_memory_fts fts ON bm.id = fts.rowid
             WHERE bounded_memory_fts MATCH ?1
               AND bm.target = 'memory'
               AND COALESCE(bm.memory_type, 'manual') != 'scenario'
               AND NOT EXISTS (SELECT 1 FROM bounded_memory s WHERE s.supersedes_id = bm.id)",
        );
        let mut next = 2usize;
        let mut time_binds: Vec<i64> = Vec::new();
        if let Some(a) = after {
            sql.push_str(&format!(" AND bm.created_at >= ?{}", next));
            next += 1;
            time_binds.push(a);
        }
        if let Some(b) = before {
            sql.push_str(&format!(" AND bm.created_at <= ?{}", next));
            next += 1;
            time_binds.push(b);
        }
        sql.push_str(" ORDER BY CASE bm.confidence WHEN 'high' THEN 1.0 WHEN 'medium' THEN 0.5 ELSE 0.25 END DESC, bm.updated_at DESC");
        sql.push_str(&format!(" LIMIT ?{}", next));

        let mut binds: Vec<Box<dyn rusqlite::types::ToSql>> = vec![Box::new(fts_query.to_string())];
        for t in &time_binds {
            binds.push(Box::new(*t));
        }
        binds.push(Box::new(limit as i64));
        let refs: Vec<&dyn rusqlite::types::ToSql> = binds.iter().map(AsRef::as_ref).collect();

        match self.db.conn().prepare(&sql) {
            Ok(mut stmt) => match stmt.query_map(refs.as_slice(), map_atom_row) {
                Ok(atoms) => atoms
                    .filter_map(|row| match row {
                        Ok(atom) => Some(atom),
                        Err(e) => {
                            tracing::warn!(
                                target: "asuna_memory::memory::retrieval",
                                "recall L1 FTS row read error (skipping row): {}",
                                e
                            );
                            None
                        }
                    })
                    .collect(),
                Err(e) => {
                    tracing::warn!(
                        target: "asuna_memory::memory::retrieval",
                        "recall L1 FTS query error (skipping L1 FTS): {}",
                        e
                    );
                    Vec::new()
                }
            },
            Err(e) => {
                tracing::warn!(
                    target: "asuna_memory::memory::retrieval",
                    "recall L1 FTS prepare error (skipping L1 FTS): {}",
                    e
                );
                Vec::new()
            }
        }
    }

    pub(super) fn is_valid_recall_vec(&self, query_vec: &[f32]) -> bool {
        let expected_dim = self.db.dimensions();
        // M13a: 有限性/非全零判据统一走 is_invalid_vector（与入库守卫同款）。
        if query_vec.len() == expected_dim && !crate::index::vector::is_invalid_vector(query_vec) {
            return true;
        }
        tracing::warn!(
            target: "asuna_memory::memory::retrieval",
            "recall L1 vector skipped: invalid query vector (len={}, expected={})",
            query_vec.len(),
            expected_dim
        );
        false
    }

    fn recall_atoms_vec(
        &self,
        query_vec: &[f32],
        after: Option<i64>,
        before: Option<i64>,
        limit: usize,
    ) -> Vec<AtomRow> {
        if !self.is_valid_recall_vec(query_vec) {
            return Vec::new();
        }
        let q_bytes = crate::embedder::quantize_to_int8(query_vec);
        let knn_k = limit.saturating_mul(3).clamp(30, 200).max(limit);
        let (sql, time_binds) = build_vec_atoms_sql(after, before);

        let mut binds: Vec<Box<dyn rusqlite::types::ToSql>> =
            vec![Box::new(q_bytes), Box::new(knn_k as i64)];
        for t in time_binds {
            binds.push(Box::new(t));
        }
        let refs: Vec<&dyn rusqlite::types::ToSql> = binds.iter().map(AsRef::as_ref).collect();

        let mut stmt = match self.db.conn().prepare(&sql) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    target: "asuna_memory::memory::retrieval",
                    "recall L1 vector prepare error (degrading to FTS): {}",
                    e
                );
                return Vec::new();
            }
        };
        let rows = match stmt.query_map(refs.as_slice(), |row| {
            let atom = map_atom_row(row)?;
            let dist: f64 = row.get(6)?;
            Ok((atom, dist))
        }) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(
                    target: "asuna_memory::memory::retrieval",
                    "recall L1 vector query error (degrading to FTS): {}",
                    e
                );
                return Vec::new();
            }
        };
        rows.filter_map(|row| match row {
            Ok(hit) => Some(hit),
            Err(e) => {
                tracing::warn!(
                    target: "asuna_memory::memory::retrieval",
                    "recall L1 vector row read error (skipping row): {}",
                    e
                );
                None
            }
        })
        .filter(|(_, dist)| dist.is_finite() && *dist <= L1_VEC_MAX_COSINE_DISTANCE)
        .take(limit)
        .map(|(atom, _)| atom)
        .collect()
    }
}

/// SQL filter assembly for the L1 vector KNN leg, kept outside the engine
/// impl to isolate the bind-index arithmetic from `recall_atoms_vec`.
pub(super) fn build_vec_atoms_sql(after: Option<i64>, before: Option<i64>) -> (String, Vec<i64>) {
    let mut sql = String::from(
        "WITH knn AS (
             SELECT id, distance
             FROM vec_bounded_memory
             WHERE embedding MATCH vec_int8(?1)
               AND k = ?2
         )
         SELECT bm.id, bm.content,
                CASE bm.confidence WHEN 'high' THEN 1.0 WHEN 'medium' THEN 0.5 ELSE 0.25 END,
                COALESCE(bm.memory_type, 'manual'),
                bm.created_at,
                bm.updated_at,
                knn.distance
         FROM knn
         JOIN bounded_memory bm ON bm.id = knn.id
         WHERE bm.target = 'memory'
           AND COALESCE(bm.memory_type, 'manual') != 'scenario'
           AND NOT EXISTS (SELECT 1 FROM bounded_memory s WHERE s.supersedes_id = bm.id)",
    );
    let mut next = 3usize;
    let mut time_binds: Vec<i64> = Vec::new();
    if let Some(a) = after {
        sql.push_str(&format!(" AND bm.created_at >= ?{}", next));
        next += 1;
        time_binds.push(a);
    }
    if let Some(b) = before {
        sql.push_str(&format!(" AND bm.created_at <= ?{}", next));
        // Symmetric with the after branch (latent off-by-one guard): advance
        // the counter even though before is currently the last bind.
        next += 1;
        time_binds.push(b);
    }
    // before is the last bind today; explicitly consume the symmetric counter
    // so the increment above is not a dead store (unused_assignments).
    let _ = next;
    sql.push_str(" ORDER BY knn.distance ASC");
    (sql, time_binds)
}

fn map_atom_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<AtomRow> {
    Ok(AtomRow {
        id: row.get(0)?,
        content: row.get(1)?,
        confidence: row.get(2)?,
        memory_type: row.get(3)?,
        created_at: row.get(4)?,
        updated_at: row.get(5)?,
    })
}

fn fuse_l1_atoms(
    mut fts_rows: Vec<AtomRow>,
    vec_rows: Vec<AtomRow>,
    top_k: usize,
) -> Vec<serde_json::Value> {
    if vec_rows.is_empty() {
        fts_rows.truncate(top_k);
        return fts_rows
            .into_iter()
            .map(|r| atom_to_json(&r, "confidence+recency"))
            .collect();
    }

    let mut acc = RrfAccumulator::<i64, AtomRow, 2>::with_default_k();
    acc.add_leg(0, fts_rows.into_iter().map(|row| (row.id, row)));
    acc.add_leg(1, vec_rows.into_iter().map(|row| (row.id, row)));

    acc.into_ranked_by(|a, b| {
        b.value
            .confidence
            .total_cmp(&a.value.confidence)
            .then_with(|| b.value.updated_at.cmp(&a.value.updated_at))
            .then_with(|| b.key.cmp(&a.key))
    })
    .into_iter()
    .take(top_k)
    .map(|hit| atom_to_json(&hit.value, "hybrid_rrf"))
    .collect()
}

fn atom_to_json(row: &AtomRow, ordered_by: &str) -> serde_json::Value {
    serde_json::json!({
        "layer": "L1",
        "type": row.memory_type,
        "content": row.content,
        "confidence": row.confidence,
        "created_at": row.created_at,
        "ordered_by": ordered_by
    })
}

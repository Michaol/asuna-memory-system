use super::RetrievalEngine;
use crate::util::text::{build_safe_fts_or_query, build_safe_fts_query, escape_like};

struct TurnsQuerySpec<'a> {
    sql_prefix: &'a str,
    ts_col: &'a str,
    order_clause: &'a str,
}

impl<'a> RetrievalEngine<'a> {
    /// L0 recent turns: indexed `turns_fts` search with punctuation-only `LIKE` fallback.
    ///
    /// A4: degrade-to-skip like every other layer — a prepare/query failure
    /// warns and contributes no turns; `/recall` never fails over L0.
    pub(super) fn recall_turns(
        &self,
        query: &str,
        after: Option<i64>,
        before: Option<i64>,
        top_k: usize,
    ) -> Vec<serde_json::Value> {
        if top_k == 0 {
            return Vec::new();
        }
        // M6b: 空/纯空白查询没有 FTS 词项，LIKE 回退会退化成 '%%' 全表扫描
        // （或匹配任意空白串）——直接返回空。/recall 入口已 400 拒绝空查询，
        // 此处是引擎层防御，与 L1 空查询返回空同口径。
        if query.trim().is_empty() {
            return Vec::new();
        }
        if let Some(and_expr) = build_safe_fts_query(query) {
            let rows = self.run_turns_fts_query(&and_expr, after, before, top_k);
            if !rows.is_empty() {
                return rows;
            }
            if let Some(or_expr) = build_safe_fts_or_query(query) {
                return self.run_turns_fts_query(&or_expr, after, before, top_k);
            }
            return rows;
        }
        let search_pattern = format!("%{}%", escape_like(query));
        self.recall_turns_like(&search_pattern, after, before, top_k)
    }

    fn run_turns_fts_query(
        &self,
        fts_expr: &str,
        after: Option<i64>,
        before: Option<i64>,
        top_k: usize,
    ) -> Vec<serde_json::Value> {
        self.exec_turns_query(
            TurnsQuerySpec {
                sql_prefix: "SELECT t.role, COALESCE(t.preview, ''), t.timestamp_ms
                             FROM turns_fts f
                             JOIN turns t ON f.rowid = t.id
                             WHERE turns_fts MATCH ?1",
                ts_col: "t.timestamp_ms",
                order_clause: "ORDER BY rank, t.timestamp_ms DESC",
            },
            fts_expr,
            after,
            before,
            top_k,
        )
    }

    fn recall_turns_like(
        &self,
        search_pattern: &str,
        after: Option<i64>,
        before: Option<i64>,
        top_k: usize,
    ) -> Vec<serde_json::Value> {
        self.exec_turns_query(
            TurnsQuerySpec {
                // COALESCE mirrors the FTS leg's SELECT shape (NULL preview
                // can never match LIKE, so this is symmetry, not a behavior fix).
                sql_prefix: "SELECT role, COALESCE(preview, ''), timestamp_ms FROM turns WHERE preview LIKE ?1 ESCAPE '\\'",
                ts_col: "timestamp_ms",
                order_clause: "ORDER BY timestamp_ms DESC",
            },
            search_pattern,
            after,
            before,
            top_k,
        )
    }

    fn build_turns_sql_and_binds(
        spec: &TurnsQuerySpec<'_>,
        match_arg: &str,
        after: Option<i64>,
        before: Option<i64>,
        top_k: usize,
    ) -> (String, Vec<Box<dyn rusqlite::types::ToSql>>) {
        let mut sql = String::from(spec.sql_prefix);
        let mut next = 2usize;
        let mut binds: Vec<Box<dyn rusqlite::types::ToSql>> = vec![Box::new(match_arg.to_string())];
        if let Some(a) = after {
            sql.push_str(&format!(" AND {} >= ?{}", spec.ts_col, next));
            next += 1;
            binds.push(Box::new(a));
        }
        if let Some(b) = before {
            sql.push_str(&format!(" AND {} <= ?{}", spec.ts_col, next));
            next += 1;
            binds.push(Box::new(b));
        }
        sql.push_str(&format!(" {} LIMIT ?{}", spec.order_clause, next));
        binds.push(Box::new(top_k as i64));
        (sql, binds)
    }

    fn exec_turns_query(
        &self,
        spec: TurnsQuerySpec<'_>,
        match_arg: &str,
        after: Option<i64>,
        before: Option<i64>,
        top_k: usize,
    ) -> Vec<serde_json::Value> {
        let (sql, binds) = Self::build_turns_sql_and_binds(&spec, match_arg, after, before, top_k);
        let refs: Vec<&dyn rusqlite::types::ToSql> = binds.iter().map(AsRef::as_ref).collect();

        // A4: prepare/query 失败 warn + 跳过 L0
        let mut stmt = match self.db.conn().prepare(&sql) {
            Ok(stmt) => stmt,
            Err(e) => {
                tracing::warn!(
                    target: "asuna_memory::memory::retrieval",
                    "recall L0 turns prepare error (skipping L0 layer): {}",
                    e
                );
                return Vec::new();
            }
        };
        let turns = match stmt.query_map(refs.as_slice(), |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
            ))
        }) {
            Ok(turns) => turns,
            Err(e) => {
                tracing::warn!(
                    target: "asuna_memory::memory::retrieval",
                    "recall L0 turns query error (skipping L0 layer): {}",
                    e
                );
                return Vec::new();
            }
        };
        collect_l0_turn_rows(turns)
    }
}

fn collect_l0_turn_rows(
    turns: rusqlite::MappedRows<
        '_,
        impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<(String, String, i64)>,
    >,
) -> Vec<serde_json::Value> {
    let mut out = Vec::new();
    for turn in turns {
        match turn {
            Ok((role, content, timestamp)) => out.push(serde_json::json!({
                "layer": "L0", "type": "turn", "role": role, "content": content, "timestamp": timestamp
            })),
            Err(e) => tracing::warn!(
                target: "asuna_memory::memory::retrieval",
                "recall L0 turn row parse error: {}",
                e
            ),
        }
    }
    out
}

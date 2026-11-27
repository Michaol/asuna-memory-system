use crate::index::db::Db;

/// FTS5 全文检索
pub struct FtsStore<'a> {
    db: &'a Db,
}

/// FTS5 搜索结果
#[derive(Debug)]
pub struct FtsResult {
    pub turn_id: i64,
    pub rank: f64,
    pub preview: String,
    pub session_id: String,
    pub timestamp_ms: i64,
    pub role: String,
}

impl<'a> FtsStore<'a> {
    pub fn new(db: &'a Db) -> Self {
        Self { db }
    }

    /// FTS5 搜索（仅测试使用；生产路径走 search_turns_with_filters，
    /// 见 fact/search.rs keyword_search——search_with_time_filter 同为测试入口）
    #[cfg(test)]
    pub fn search(&self, query: &str, top_k: usize) -> anyhow::Result<Vec<FtsResult>> {
        self.search_with_time_filter(query, None, None, top_k)
    }

    fn run_fts_query(
        &self,
        fts_expr: &str,
        after_ms: Option<i64>,
        before_ms: Option<i64>,
        role: Option<&str>,
        top_k: usize,
    ) -> anyhow::Result<Vec<FtsResult>> {
        let mut sql = String::from(
            "SELECT t.id, rank, COALESCE(t.preview, ''), t.session_id, t.timestamp_ms, t.role
             FROM turns_fts f
             JOIN turns t ON f.rowid = t.id
             WHERE turns_fts MATCH ?1",
        );

        let mut next = 2usize;
        let mut params: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(fts_expr.to_string())];
        if let Some(after) = after_ms {
            sql.push_str(&format!(" AND t.timestamp_ms >= ?{}", next));
            next += 1;
            params.push(Box::new(after));
        }
        if let Some(before) = before_ms {
            sql.push_str(&format!(" AND t.timestamp_ms <= ?{}", next));
            next += 1;
            params.push(Box::new(before));
        }
        if let Some(r) = role {
            sql.push_str(&format!(" AND t.role = ?{}", next));
            next += 1;
            params.push(Box::new(r.to_string()));
        }
        sql.push_str(&format!(" ORDER BY rank LIMIT ?{}", next));
        params.push(Box::new(i64::try_from(top_k).unwrap_or(i64::MAX)));

        let mut stmt = self.db.conn().prepare(&sql)?;
        let param_refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(Box::as_ref).collect();

        let rows = stmt.query_map(param_refs.as_slice(), |row| {
            Ok(FtsResult {
                turn_id: row.get(0)?,
                rank: row.get(1)?,
                preview: row.get(2)?,
                session_id: row.get(3)?,
                timestamp_ms: row.get(4)?,
                role: row.get(5)?,
            })
        })?;

        let mut results = Vec::new();
        for row in rows {
            results.push(row?);
        }
        Ok(results)
    }

    /// FTS5 搜索 + 时间范围过滤（多词安全检索：优先隐式 AND，零命中且多词时降级 OR）
    #[cfg(test)]
    pub fn search_with_time_filter(
        &self,
        query: &str,
        after_ms: Option<i64>,
        before_ms: Option<i64>,
        top_k: usize,
    ) -> anyhow::Result<Vec<FtsResult>> {
        self.search_turns_with_filters(query, after_ms, before_ms, None, top_k)
    }

    /// FTS5 搜索 + 时间范围及可选 role 过滤（SQL 侧完成 JOIN 与过滤，避免二次回表）。
    pub fn search_turns_with_filters(
        &self,
        query: &str,
        after_ms: Option<i64>,
        before_ms: Option<i64>,
        role: Option<&str>,
        top_k: usize,
    ) -> anyhow::Result<Vec<FtsResult>> {
        if top_k == 0 {
            return Ok(Vec::new());
        }
        let Some(and_expr) = crate::util::text::build_safe_fts_query(query) else {
            return Ok(Vec::new());
        };
        let results = self.run_fts_query(&and_expr, after_ms, before_ms, role, top_k)?;
        if results.is_empty() {
            if let Some(or_expr) = crate::util::text::build_safe_fts_or_query(query) {
                return self.run_fts_query(&or_expr, after_ms, before_ms, role, top_k);
            }
        }
        Ok(results)
    }
}

#[cfg(test)]
#[path = "fts_tests.rs"]
mod tests;

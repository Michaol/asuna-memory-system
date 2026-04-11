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
}

impl<'a> FtsStore<'a> {
    pub fn new(db: &'a Db) -> Self {
        Self { db }
    }

    /// FTS5 搜索
    #[allow(dead_code)]
    pub fn search(&self, query: &str, top_k: usize) -> anyhow::Result<Vec<FtsResult>> {
        let mut stmt = self.db.conn().prepare(
            "SELECT t.id, rank, t.preview
             FROM turns_fts f
             JOIN turns t ON f.rowid = t.id
             WHERE turns_fts MATCH ?1
             ORDER BY rank
             LIMIT ?2",
        )?;

        let rows = stmt.query_map(rusqlite::params![query, top_k as i64], |row| {
            Ok(FtsResult {
                turn_id: row.get(0)?,
                rank: row.get(1)?,
                preview: row.get(2)?,
            })
        })?;

        let mut results = Vec::new();
        for row in rows {
            results.push(row?);
        }
        Ok(results)
    }

    /// FTS5 搜索 + 时间范围过滤
    pub fn search_with_time_filter(
        &self,
        query: &str,
        after_ms: Option<i64>,
        before_ms: Option<i64>,
        top_k: usize,
    ) -> anyhow::Result<Vec<FtsResult>> {
        let mut sql = String::from(
            "SELECT t.id, rank, t.preview
             FROM turns_fts f
             JOIN turns t ON f.rowid = t.id
             WHERE turns_fts MATCH ?1",
        );

        if after_ms.is_some() {
            sql.push_str(" AND t.timestamp_ms >= ?2");
        }
        if before_ms.is_some() {
            let param_idx = if after_ms.is_some() { 3 } else { 2 };
            sql.push_str(&format!(" AND t.timestamp_ms <= ?{}", param_idx));
        }

        sql.push_str(" ORDER BY rank LIMIT ?");
        let limit_idx = if after_ms.is_some() && before_ms.is_some() {
            4
        } else if after_ms.is_some() || before_ms.is_some() {
            3
        } else {
            2
        };
        sql.push_str(&limit_idx.to_string());

        let mut stmt = self.db.conn().prepare(&sql)?;

        let tokenized_query = crate::util::text::tokenize_chinese(query);
        let mut params: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(tokenized_query)];
        if let Some(after) = after_ms {
            params.push(Box::new(after));
        }
        if let Some(before) = before_ms {
            params.push(Box::new(before));
        }
        params.push(Box::new(top_k as i64));

        let param_refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|p| p.as_ref()).collect();

        let rows = stmt.query_map(param_refs.as_slice(), |row| {
            Ok(FtsResult {
                turn_id: row.get(0)?,
                rank: row.get(1)?,
                preview: row.get(2)?,
            })
        })?;

        let mut results = Vec::new();
        for row in rows {
            results.push(row?);
        }
        Ok(results)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup_test_data(db: &Db) {
        db.conn()
            .execute(
                "INSERT INTO sessions (session_id, start_ts, file_path, created_at, updated_at)
             VALUES ('s1', 0, 'test.jsonl', 0, 0)",
                [],
            )
            .unwrap();

        db.conn()
            .execute_batch(
                "INSERT INTO turns (id, session_id, seq, timestamp_ms, role, preview) VALUES
             (1, 's1', 1, 1000, 'user', 'Rust is a systems programming language'),
             (2, 's1', 2, 2000, 'assistant', 'Python is great for data science'),
             (3, 's1', 3, 3000, 'user', 'I love Rust for its memory safety');",
            )
            .unwrap();
    }

    #[test]
    fn test_fts_search() {
        let db = Db::open_memory().unwrap();
        db.init_schema().unwrap();
        setup_test_data(&db);

        let store = FtsStore::new(&db);
        let results = store.search("Rust", 10).unwrap();
        assert!(
            results.len() >= 2,
            "should find at least 2 results for 'Rust'"
        );
    }

    #[test]
    fn test_fts_search_with_time_filter() {
        let db = Db::open_memory().unwrap();
        db.init_schema().unwrap();
        setup_test_data(&db);

        let store = FtsStore::new(&db);
        // 只搜索 1500ms 之后的内容
        let results = store
            .search_with_time_filter("Rust", Some(1500), None, 10)
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].turn_id, 3);
    }
}

use std::collections::HashMap;
use crate::index::db::Db;
use crate::index::fts::FtsStore;
use crate::index::vector::VectorStore;
use crate::embedder::LazyEmbedder;

/// 搜索参数
pub struct SearchParams {
    pub query: String,
    pub search_mode: SearchMode,
    pub top_k: usize,
    pub after_ms: Option<i64>,
    pub before_ms: Option<i64>,
    pub role: Option<String>,
}

#[derive(Debug, Clone)]
pub enum SearchMode {
    Semantic,
    Keyword,
    Hybrid,
}

/// 搜索结果
#[derive(Debug, serde::Serialize)]
pub struct SearchResult {
    pub turn_id: i64,
    pub score: f64,
    pub preview: String,
    pub session_id: String,
    pub timestamp_ms: i64,
    pub role: String,
}

/// 执行搜索
pub fn search_sessions(
    db: &Db,
    embedder: Option<&LazyEmbedder>,
    params: &SearchParams,
) -> anyhow::Result<Vec<SearchResult>> {
    match params.search_mode {
        SearchMode::Keyword => keyword_search(db, params),
        SearchMode::Semantic => semantic_search(db, embedder, params),
        SearchMode::Hybrid => hybrid_search(db, embedder, params),
    }
}

/// 关键词搜索
fn keyword_search(db: &Db, params: &SearchParams) -> anyhow::Result<Vec<SearchResult>> {
    let fts = FtsStore::new(db);
    let fts_results = fts.search_with_time_filter(
        &params.query,
        params.after_ms,
        params.before_ms,
        params.top_k,
    )?;

    let mut results = Vec::new();
    for r in fts_results {
        if let Some(ref role_filter) = params.role {
            let actual_role: Option<String> = db.conn().query_row(
                "SELECT role FROM turns WHERE id = ?1",
                rusqlite::params![r.turn_id],
                |row| row.get(0),
            ).ok();
            if actual_role.as_deref() != Some(role_filter.as_str()) {
                continue;
            }
        }
        if let Some(info) = get_turn_context(db, r.turn_id)? {
            results.push(SearchResult {
                turn_id: r.turn_id,
                score: -r.rank, // FTS5 rank is negative, invert for consistent "higher is better"
                preview: r.preview,
                session_id: info.0,
                timestamp_ms: info.1,
                role: info.2,
            });
        }
    }
    Ok(results)
}

/// 语义搜索
fn semantic_search(
    db: &Db,
    embedder: Option<&LazyEmbedder>,
    params: &SearchParams,
) -> anyhow::Result<Vec<SearchResult>> {
    let embedder = embedder.ok_or_else(|| anyhow::anyhow!("语义搜索需要嵌入引擎"))?;
    let query_vec = embedder.embed(&params.query)?;

    let vec_store = VectorStore::new(db);
    let vec_results = vec_store.search(&query_vec, params.top_k)?;

    let mut results = Vec::new();
    for (turn_id, distance) in vec_results {
        if let Some(info) = get_turn_context(db, turn_id)? {
            // 时间过滤
            if let Some(after) = params.after_ms {
                if info.1 < after { continue; }
            }
            if let Some(before) = params.before_ms {
                if info.1 > before { continue; }
            }
            // Role 过滤
            if let Some(ref role_filter) = params.role {
                if info.2 != *role_filter { continue; }
            }

            results.push(SearchResult {
                turn_id,
                score: 1.0 - distance as f64, // 余弦距离 → 相似度
                preview: get_preview(db, turn_id)?,
                session_id: info.0,
                timestamp_ms: info.1,
                role: info.2,
            });
        }
    }
    Ok(results)
}

/// Hybrid 搜索（RRF 融合）
fn hybrid_search(
    db: &Db,
    embedder: Option<&LazyEmbedder>,
    params: &SearchParams,
) -> anyhow::Result<Vec<SearchResult>> {
    let k = 60.0; // RRF 常数

    // 语义搜索结果
    let semantic_results = if let Some(emb) = embedder {
        semantic_search(db, Some(emb), params).unwrap_or_default()
    } else {
        vec![]
    };

    // 关键词搜索结果
    let keyword_results = keyword_search(db, params).unwrap_or_default();

    // RRF 融合
    let mut scores: HashMap<i64, f64> = HashMap::new();
    let mut preview_map: HashMap<i64, String> = HashMap::new();
    let mut context_map: HashMap<i64, (String, i64, String)> = HashMap::new();

    for (rank, r) in semantic_results.iter().enumerate() {
        let rrf_score = 1.0 / (k + rank as f64 + 1.0);
        *scores.entry(r.turn_id).or_insert(0.0) += rrf_score;
        preview_map.entry(r.turn_id).or_insert_with(|| r.preview.clone());
        context_map.entry(r.turn_id).or_insert_with(|| (r.session_id.clone(), r.timestamp_ms, r.role.clone()));
    }

    for (rank, r) in keyword_results.iter().enumerate() {
        let rrf_score = 1.0 / (k + rank as f64 + 1.0);
        *scores.entry(r.turn_id).or_insert(0.0) += rrf_score;
        preview_map.entry(r.turn_id).or_insert_with(|| r.preview.clone());
        context_map.entry(r.turn_id).or_insert_with(|| (r.session_id.clone(), r.timestamp_ms, r.role.clone()));
    }

    // 按分数排序
    let mut sorted: Vec<(i64, f64)> = scores.into_iter().collect();
    sorted.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    let results: Vec<SearchResult> = sorted.into_iter()
        .take(params.top_k)
        .filter_map(|(turn_id, score)| {
            let preview = preview_map.remove(&turn_id)?;
            let (session_id, timestamp_ms, role) = context_map.remove(&turn_id)?;
            Some(SearchResult {
                turn_id,
                score,
                preview,
                session_id,
                timestamp_ms,
                role,
            })
        })
        .collect();

    Ok(results)
}

/// 获取 turn 的上下文信息
fn get_turn_context(db: &Db, turn_id: i64) -> anyhow::Result<Option<(String, i64, String)>> {
    let result = db.conn().query_row(
        "SELECT t.session_id, t.timestamp_ms, t.role
         FROM turns t WHERE t.id = ?1",
        rusqlite::params![turn_id],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?, row.get::<_, String>(2)?)),
    );
    match result {
        Ok(info) => Ok(Some(info)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn get_preview(db: &Db, turn_id: i64) -> anyhow::Result<String> {
    let preview: String = db.conn().query_row(
        "SELECT preview FROM turns WHERE id = ?1",
        rusqlite::params![turn_id],
        |row| row.get(0),
    )?;
    Ok(preview)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup_test_data(db: &Db) {
        db.conn().execute(
            "INSERT INTO sessions (session_id, start_ts, file_path, created_at, updated_at)
             VALUES ('s1', 0, 'test.jsonl', 0, 0)",
            [],
        ).unwrap();

        db.conn().execute_batch(
            "INSERT INTO turns (id, session_id, seq, timestamp_ms, role, preview) VALUES
             (1, 's1', 1, 1000, 'user', 'Rust is a systems programming language'),
             (2, 's1', 2, 2000, 'assistant', 'Python is great for data science'),
             (3, 's1', 3, 3000, 'user', 'I love Rust for its memory safety');"
        ).unwrap();
    }

    #[test]
    fn test_keyword_search() {
        let db = Db::open_memory().unwrap();
        db.init_schema().unwrap();
        setup_test_data(&db);

        let params = SearchParams {
            query: "Rust".to_string(),
            search_mode: SearchMode::Keyword,
            top_k: 5,
            after_ms: None,
            before_ms: None,
            role: None,
        };

        let results = search_sessions(&db, None, &params).unwrap();
        assert!(results.len() >= 2);
    }

    #[test]
    fn test_hybrid_without_embedder() {
        let db = Db::open_memory().unwrap();
        db.init_schema().unwrap();
        setup_test_data(&db);

        let params = SearchParams {
            query: "Rust".to_string(),
            search_mode: SearchMode::Hybrid,
            top_k: 5,
            after_ms: None,
            before_ms: None,
            role: None,
        };

        // 没有 embedder 时，hybrid 退化为 keyword
        let results = search_sessions(&db, None, &params).unwrap();
        assert!(results.len() >= 2);
    }

    #[test]
    fn test_time_range_filter() {
        let db = Db::open_memory().unwrap();
        db.init_schema().unwrap();
        setup_test_data(&db);

        let params = SearchParams {
            query: "Rust".to_string(),
            search_mode: SearchMode::Keyword,
            top_k: 5,
            after_ms: Some(1500),
            before_ms: None,
            role: None,
        };

        let results = search_sessions(&db, None, &params).unwrap();
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn test_role_filter() {
        let db = Db::open_memory().unwrap();
        db.init_schema().unwrap();
        setup_test_data(&db);

        let params = SearchParams {
            query: "Rust".to_string(),
            search_mode: SearchMode::Keyword,
            top_k: 5,
            after_ms: None,
            before_ms: None,
            role: Some("assistant".to_string()),
        };

        let results = search_sessions(&db, None, &params).unwrap();
        assert_eq!(results.len(), 0); // assistant 回复里没有 "Rust"
    }
}

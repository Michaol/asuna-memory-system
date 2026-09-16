use crate::embedder::LazyEmbedder;
use crate::index::db::Db;
use crate::index::fts::FtsStore;
use crate::index::vector::VectorStore;
use std::collections::HashMap;

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

/// Per-source score components (v2.6 score transparency).
/// Makes ranking inspectable: which source contributed what to `score`.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct ScoreBreakdown {
    /// Semantic (vector cosine similarity) or semantic RRF contribution
    #[serde(skip_serializing_if = "Option::is_none")]
    pub semantic: Option<f64>,
    /// Keyword (FTS5 rank inverted) or keyword RRF contribution
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keyword: Option<f64>,
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
    /// v2.6: score components; omitted only when no breakdown applies
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scores: Option<ScoreBreakdown>,
}

/// 执行搜索（自带 embed_query 网络调用的便捷入口）
///
/// 锁纪律（U15）：semantic/hybrid 模式在本函数内部调用 `embed_query`
/// （同步网络 I/O + 重试退避）。持有与其他流量共享的 std::sync::Mutex
/// （如网关的全局 DB 锁）的调用方 MUST 改用
/// [`search_sessions_with_vec`]，在拿锁之前自行预计算查询向量，
/// 否则锁会横跨网络往返。CLI `cmd_search` 与 MCP `search_sessions`
/// 工具继续用本签名：它们的连接不与任何并发网关锁共享（MCP 单线程、
/// CLI 独立进程，均无此问题），无锁内网络调用可避免。
pub fn search_sessions(
    db: &Db,
    embedder: Option<&LazyEmbedder>,
    params: &SearchParams,
) -> anyhow::Result<Vec<SearchResult>> {
    match params.search_mode {
        SearchMode::Keyword => keyword_search(db, params),
        SearchMode::Semantic => {
            let embedder = embedder.ok_or_else(|| anyhow::anyhow!("语义搜索需要嵌入引擎"))?;
            let query_vec = embedder.embed_query(&params.query)?;
            search_sessions_with_vec(db, Some(query_vec), params)
        }
        SearchMode::Hybrid => {
            // 降级语义与旧 hybrid_search 的 embedder 分支逐一相同：
            // 无嵌入引擎 → 静默 keyword-only；embed 失败 → warn + keyword-only。
            let query_vec = match embedder {
                Some(e) => match e.embed_query(&params.query) {
                    Ok(v) => Some(v),
                    Err(err) => {
                        tracing::warn!("hybrid: 语义搜索失败，降级为仅关键词: {}", err);
                        None
                    }
                },
                None => None,
            };
            search_sessions_with_vec(db, query_vec, params)
        }
    }
}

/// [`search_sessions`] 的预计算向量变体（U15）：semantic/hybrid 使用传入的
/// 查询向量，函数内部零网络调用，可安全地在 DB 锁内运行。
///
/// `query_vec` 应为调用方在锁外 `embedder.embed_query(params.query)` 的结果。
/// 传 `None` 即关闭语义来源，行为与旧入口逐一对应：semantic 模式返回
/// "语义搜索需要嵌入引擎" 错误；hybrid 模式走既有的 keyword-only 降级。
/// 向量维度与 `vec_turns` 不符时由 vec0 报错，错误照常上抛。
pub fn search_sessions_with_vec(
    db: &Db,
    query_vec: Option<Vec<f32>>,
    params: &SearchParams,
) -> anyhow::Result<Vec<SearchResult>> {
    match params.search_mode {
        SearchMode::Keyword => keyword_search(db, params),
        SearchMode::Semantic => {
            let query_vec = query_vec.ok_or_else(|| anyhow::anyhow!("语义搜索需要嵌入引擎"))?;
            semantic_search(db, &query_vec, params)
        }
        SearchMode::Hybrid => hybrid_search(db, query_vec, params),
    }
}

/// 关键词搜索
fn keyword_search(db: &Db, params: &SearchParams) -> anyhow::Result<Vec<SearchResult>> {
    let fts = FtsStore::new(db);
    // Over-fetch when a role filter will be applied in Rust (role is not pushed
    // into SQL), otherwise post-filtering could under-return below top_k.
    let fetch_k = if params.role.is_some() {
        params.top_k.saturating_mul(5).min(200)
    } else {
        params.top_k
    };
    let fts_results =
        fts.search_with_time_filter(&params.query, params.after_ms, params.before_ms, fetch_k)?;

    if fts_results.is_empty() {
        return Ok(Vec::new());
    }

    // Batch fetch all turn contexts in a single query (avoid N+1)
    let turn_ids: Vec<i64> = fts_results.iter().map(|r| r.turn_id).collect();
    let contexts = batch_get_turn_context(db, &turn_ids)?;

    let mut results = Vec::new();
    for r in fts_results {
        let info = match contexts.get(&r.turn_id) {
            Some(info) => info,
            None => continue,
        };

        // Apply role filter
        if let Some(ref role_filter) = params.role {
            if info.2 != *role_filter {
                continue;
            }
        }

        results.push(SearchResult {
            turn_id: r.turn_id,
            score: -r.rank, // FTS5 rank is negative, invert for consistent "higher is better"
            preview: r.preview,
            session_id: info.0.clone(),
            timestamp_ms: info.1,
            role: info.2.clone(),
            scores: Some(ScoreBreakdown {
                semantic: None,
                keyword: Some(-r.rank),
            }),
        });
    }
    results.truncate(params.top_k);
    Ok(results)
}

/// 语义搜索（预计算向量版）：不再内部 embed，可安全在锁内运行。
fn semantic_search(
    db: &Db,
    query_vec: &[f32],
    params: &SearchParams,
) -> anyhow::Result<Vec<SearchResult>> {
    let vec_store = VectorStore::new(db);
    // Over-fetch when time/role filters are applied in Rust after the KNN LIMIT,
    // otherwise post-filtering could under-return below top_k.
    let fetch_k =
        if params.role.is_some() || params.after_ms.is_some() || params.before_ms.is_some() {
            params.top_k.saturating_mul(5).min(200)
        } else {
            params.top_k
        };
    let vec_results = vec_store.search(query_vec, fetch_k)?;

    if vec_results.is_empty() {
        return Ok(Vec::new());
    }

    // Batch fetch all turn contexts + previews in two queries (avoid N+1)
    let turn_ids: Vec<i64> = vec_results.iter().map(|(id, _)| *id).collect();
    let contexts = batch_get_turn_context(db, &turn_ids)?;
    let previews = batch_get_previews(db, &turn_ids)?;

    let mut results = Vec::new();
    for (turn_id, distance) in vec_results {
        if let Some(result) = semantic_result(params, &contexts, &previews, turn_id, distance) {
            results.push(result);
        }
    }
    results.truncate(params.top_k);
    Ok(results)
}

/// 构建单条语义搜索结果：上下文缺失或被时间/Role 过滤时返回 None。
fn semantic_result(
    params: &SearchParams,
    contexts: &HashMap<i64, (String, i64, String)>,
    previews: &HashMap<i64, String>,
    turn_id: i64,
    distance: f32,
) -> Option<SearchResult> {
    let info = contexts.get(&turn_id)?;

    // 时间过滤
    if let Some(after) = params.after_ms {
        if info.1 < after {
            return None;
        }
    }
    if let Some(before) = params.before_ms {
        if info.1 > before {
            return None;
        }
    }
    // Role 过滤
    if let Some(ref role_filter) = params.role {
        if info.2 != *role_filter {
            return None;
        }
    }

    let preview = previews.get(&turn_id).cloned().unwrap_or_default();
    Some(SearchResult {
        turn_id,
        score: 1.0 - distance as f64, // 余弦距离 → 相似度
        preview,
        session_id: info.0.clone(),
        timestamp_ms: info.1,
        role: info.2.clone(),
        scores: Some(ScoreBreakdown {
            semantic: Some(1.0 - distance as f64),
            keyword: None,
        }),
    })
}

/// Hybrid 搜索（RRF 融合，语义侧使用预计算向量）
fn hybrid_search(
    db: &Db,
    query_vec: Option<Vec<f32>>,
    params: &SearchParams,
) -> anyhow::Result<Vec<SearchResult>> {
    let k = 60.0; // RRF 常数

    // 语义搜索结果（query_vec 为 None 时静默退化为仅关键词——旧 embedder=None
    // 分支的语义；向量已就绪后 KNN 失败仍 warn + 降级，与旧行为一致）
    let semantic_results = match &query_vec {
        Some(qv) => semantic_search(db, qv, params).unwrap_or_else(|e| {
            tracing::warn!("hybrid: 语义搜索失败，降级为仅关键词: {}", e);
            vec![]
        }),
        None => vec![],
    };

    // 关键词搜索结果
    let keyword_results = keyword_search(db, params).unwrap_or_else(|e| {
        tracing::warn!("hybrid: 关键词搜索失败: {}", e);
        vec![]
    });

    // RRF 融合
    let mut scores: HashMap<i64, f64> = HashMap::new();
    // v2.6 score transparency: per-source RRF contributions
    let mut semantic_contrib: HashMap<i64, f64> = HashMap::new();
    let mut keyword_contrib: HashMap<i64, f64> = HashMap::new();
    let mut preview_map: HashMap<i64, String> = HashMap::new();
    let mut context_map: HashMap<i64, (String, i64, String)> = HashMap::new();

    for (rank, r) in semantic_results.iter().enumerate() {
        let rrf_score = 1.0 / (k + rank as f64 + 1.0);
        *scores.entry(r.turn_id).or_insert(0.0) += rrf_score;
        *semantic_contrib.entry(r.turn_id).or_insert(0.0) += rrf_score;
        preview_map
            .entry(r.turn_id)
            .or_insert_with(|| r.preview.clone());
        context_map
            .entry(r.turn_id)
            .or_insert_with(|| (r.session_id.clone(), r.timestamp_ms, r.role.clone()));
    }

    for (rank, r) in keyword_results.iter().enumerate() {
        let rrf_score = 1.0 / (k + rank as f64 + 1.0);
        *scores.entry(r.turn_id).or_insert(0.0) += rrf_score;
        *keyword_contrib.entry(r.turn_id).or_insert(0.0) += rrf_score;
        preview_map
            .entry(r.turn_id)
            .or_insert_with(|| r.preview.clone());
        context_map
            .entry(r.turn_id)
            .or_insert_with(|| (r.session_id.clone(), r.timestamp_ms, r.role.clone()));
    }

    // 按分数排序
    let mut sorted: Vec<(i64, f64)> = scores.into_iter().collect();
    sorted.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    let results: Vec<SearchResult> = sorted
        .into_iter()
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
                scores: Some(ScoreBreakdown {
                    semantic: semantic_contrib.get(&turn_id).copied(),
                    keyword: keyword_contrib.get(&turn_id).copied(),
                }),
            })
        })
        .collect();

    Ok(results)
}

/// Batch fetch turn contexts to avoid N+1 queries.
/// Returns a map from turn_id to (session_id, timestamp_ms, role).
fn batch_get_turn_context(
    db: &Db,
    turn_ids: &[i64],
) -> anyhow::Result<HashMap<i64, (String, i64, String)>> {
    if turn_ids.is_empty() {
        return Ok(HashMap::new());
    }

    // Build parameterized IN clause
    let placeholders: Vec<String> = (1..=turn_ids.len()).map(|i| format!("?{}", i)).collect();
    let sql = format!(
        "SELECT id, session_id, timestamp_ms, role FROM turns WHERE id IN ({})",
        placeholders.join(", ")
    );

    let mut stmt = db.conn().prepare(&sql)?;
    let params: Vec<Box<dyn rusqlite::types::ToSql>> = turn_ids
        .iter()
        .map(|id| Box::new(*id) as Box<dyn rusqlite::types::ToSql>)
        .collect();
    let param_refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();

    let mut map = HashMap::new();
    let rows = stmt.query_map(param_refs.as_slice(), |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, String>(3)?,
        ))
    })?;

    for row in rows {
        let (id, session_id, timestamp_ms, role) = row?;
        map.insert(id, (session_id, timestamp_ms, role));
    }

    Ok(map)
}

/// Batch fetch turn previews to avoid N+1 queries.
/// Returns a map from turn_id to preview text.
fn batch_get_previews(db: &Db, turn_ids: &[i64]) -> anyhow::Result<HashMap<i64, String>> {
    if turn_ids.is_empty() {
        return Ok(HashMap::new());
    }

    let placeholders: Vec<String> = (1..=turn_ids.len()).map(|i| format!("?{}", i)).collect();
    let sql = format!(
        "SELECT id, preview FROM turns WHERE id IN ({})",
        placeholders.join(", ")
    );

    let mut stmt = db.conn().prepare(&sql)?;
    let params: Vec<Box<dyn rusqlite::types::ToSql>> = turn_ids
        .iter()
        .map(|id| Box::new(*id) as Box<dyn rusqlite::types::ToSql>)
        .collect();
    let param_refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();

    let mut map = HashMap::new();
    let rows = stmt.query_map(param_refs.as_slice(), |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
    })?;

    for row in rows {
        let (id, preview) = row?;
        map.insert(id, preview);
    }

    Ok(map)
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

    /// v2.6 score transparency: keyword mode exposes the keyword component only,
    /// and it equals the top-level score.
    #[test]
    fn test_score_breakdown_keyword() {
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
        assert!(!results.is_empty());
        for r in &results {
            let bd = r
                .scores
                .as_ref()
                .expect("keyword results carry a breakdown");
            assert!(
                bd.semantic.is_none(),
                "keyword mode has no semantic component"
            );
            assert_eq!(bd.keyword, Some(r.score), "keyword component equals score");
        }
    }

    /// Hybrid without embedder degrades to keyword-only RRF: the breakdown must
    /// carry keyword only, and the components must sum back to the fused score.
    #[test]
    fn test_score_breakdown_hybrid_without_embedder() {
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

        let results = search_sessions(&db, None, &params).unwrap();
        assert!(!results.is_empty());
        for r in &results {
            let bd = r.scores.as_ref().expect("hybrid results carry a breakdown");
            assert!(
                bd.semantic.is_none(),
                "no embedder → no semantic contribution"
            );
            let sum = bd.semantic.unwrap_or(0.0) + bd.keyword.unwrap_or(0.0);
            assert!(
                (sum - r.score).abs() < 1e-12,
                "components must sum to the fused score: {:?} vs {}",
                bd,
                r.score
            );
        }
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

    /// Role filter must not under-return: when more than top_k matching-role turns
    /// exist (interleaved with distractor turns that rank among them), the filtered
    /// keyword search should still return a full top_k.
    #[test]
    fn test_role_filter_does_not_under_return() {
        let db = Db::open_memory().unwrap();
        db.init_schema().unwrap();
        db.conn()
            .execute(
                "INSERT INTO sessions (session_id, start_ts, file_path, created_at, updated_at)
             VALUES ('s1', 0, 'test.jsonl', 0, 0)",
                [],
            )
            .unwrap();
        // 6 user + 6 assistant turns all matching "Rust"
        for i in 0..6 {
            db.conn()
                .execute(
                    "INSERT INTO turns (session_id, seq, timestamp_ms, role, preview)
                 VALUES ('s1', ?1, ?2, 'user', 'Rust topic')",
                    rusqlite::params![i * 2 + 1, (i * 2 + 1) as i64],
                )
                .unwrap();
            db.conn()
                .execute(
                    "INSERT INTO turns (session_id, seq, timestamp_ms, role, preview)
                 VALUES ('s1', ?1, ?2, 'assistant', 'Rust reply')",
                    rusqlite::params![i * 2 + 2, (i * 2 + 2) as i64],
                )
                .unwrap();
        }

        let params = SearchParams {
            query: "Rust".to_string(),
            search_mode: SearchMode::Keyword,
            top_k: 3,
            after_ms: None,
            before_ms: None,
            role: Some("user".to_string()),
        };

        let results = search_sessions(&db, None, &params).unwrap();
        assert_eq!(
            results.len(),
            3,
            "should return full top_k when enough matching-role turns exist"
        );
        assert!(results.iter().all(|r| r.role == "user"));
    }

    // ── U15: search_sessions_with_vec（预计算向量入口，无内部网络调用） ──

    /// 注入轴向量：turn 1 → [1,0,..]，turn 3 → [0,1,..]（正交），
    /// 量化往返对 0/1 分量精确，KNN 排序可断言。
    fn insert_axis_vecs(db: &Db) {
        let store = VectorStore::new(db);
        let mut v1 = vec![0.0f32; db.dimensions()];
        v1[0] = 1.0;
        store.insert(1, &v1).unwrap();
        let mut v3 = vec![0.0f32; db.dimensions()];
        v3[1] = 1.0;
        store.insert(3, &v3).unwrap();
    }

    fn axis_query(db: &Db, axis: usize) -> Vec<f32> {
        let mut q = vec![0.0f32; db.dimensions()];
        q[axis] = 1.0;
        q
    }

    /// with_vec 的 semantic 路径直接使用注入向量（无需 embedder）：
    /// 与轴 0 向量同向的 turn 1 排第一且余弦距离 ≈ 0（score ≈ 1.0）。
    #[test]
    fn test_with_vec_semantic_uses_injected_vector() {
        let db = Db::open_memory().unwrap();
        db.init_schema().unwrap();
        setup_test_data(&db);
        insert_axis_vecs(&db);

        let params = SearchParams {
            query: "Rust".to_string(),
            search_mode: SearchMode::Semantic,
            top_k: 5,
            after_ms: None,
            before_ms: None,
            role: None,
        };
        let results = search_sessions_with_vec(&db, Some(axis_query(&db, 0)), &params).unwrap();
        assert_eq!(results.len(), 2, "两个已索引向量都参与 KNN");
        assert_eq!(results[0].turn_id, 1, "同方向量的 turn 排第一");
        assert!(
            (results[0].score - 1.0).abs() < 0.01,
            "同向余弦相似度 ≈ 1.0, got {}",
            results[0].score
        );
        assert_eq!(results[1].turn_id, 3);
        assert!(results[1].score < results[0].score);
        let bd = results[0].scores.as_ref().unwrap();
        assert!(bd.semantic.is_some() && bd.keyword.is_none());
    }

    /// with_vec semantic 模式 + query_vec=None → 保持原错误消息
    /// "语义搜索需要嵌入引擎"（包装与直接入口一致）。
    #[test]
    fn test_with_vec_semantic_without_vector_errors() {
        let db = Db::open_memory().unwrap();
        db.init_schema().unwrap();
        setup_test_data(&db);

        let params = SearchParams {
            query: "Rust".to_string(),
            search_mode: SearchMode::Semantic,
            top_k: 5,
            after_ms: None,
            before_ms: None,
            role: None,
        };
        let err = search_sessions_with_vec(&db, None, &params).unwrap_err();
        assert!(err.to_string().contains("语义搜索需要嵌入引擎"));
        // 包装（无 embedder）产生同一消息
        let err2 = search_sessions(&db, None, &params).unwrap_err();
        assert_eq!(err.to_string(), err2.to_string());
    }

    /// with_vec hybrid + query_vec=None → 既有 keyword-only 降级路径：
    /// 命中含 "Rust" 的 turn，且 breakdown 无 semantic 分量。
    #[test]
    fn test_with_vec_hybrid_none_degrades_to_keyword() {
        let db = Db::open_memory().unwrap();
        db.init_schema().unwrap();
        setup_test_data(&db);
        // 即使向量存在，None 向量也不参与——只走关键词
        insert_axis_vecs(&db);

        let params = SearchParams {
            query: "Rust".to_string(),
            search_mode: SearchMode::Hybrid,
            top_k: 5,
            after_ms: None,
            before_ms: None,
            role: None,
        };
        let results = search_sessions_with_vec(&db, None, &params).unwrap();
        assert!(results.len() >= 2, "keyword-only 降级仍须命中 Rust turns");
        for r in &results {
            let bd = r.scores.as_ref().unwrap();
            assert!(bd.semantic.is_none(), "无向量不得有 semantic 分量");
        }
    }

    /// with_vec hybrid + Some(向量) → 语义与关键词两路 RRF 融合：
    /// 同时命中的 turn 1 两项分量齐备（排名第一不断言——BM25 名次可致 RRF 并列）。
    #[test]
    fn test_with_vec_hybrid_fuses_injected_vector() {
        let db = Db::open_memory().unwrap();
        db.init_schema().unwrap();
        setup_test_data(&db);
        insert_axis_vecs(&db);

        let params = SearchParams {
            query: "Rust".to_string(),
            search_mode: SearchMode::Hybrid,
            top_k: 5,
            after_ms: None,
            before_ms: None,
            role: None,
        };
        let results = search_sessions_with_vec(&db, Some(axis_query(&db, 0)), &params).unwrap();
        assert!(!results.is_empty());
        // turn 1 同时被语义（同向量）与关键词（"Rust"）命中 → 双贡献。
        // 不断言其排名第一：keyword 侧 BM25 名次可致 RRF 总分并列。
        let t1 = results
            .iter()
            .find(|r| r.turn_id == 1)
            .expect("turn 1 must survive RRF fusion");
        let bd = t1.scores.as_ref().unwrap();
        assert!(
            bd.semantic.is_some() && bd.keyword.is_some(),
            "semantic+keyword 双贡献：{:?}",
            bd
        );
        let sum = bd.semantic.unwrap() + bd.keyword.unwrap();
        assert!((sum - t1.score).abs() < 1e-12);
    }
}

use crate::embedder::LazyEmbedder;
use crate::index::db::Db;
use crate::index::fts::FtsStore;
use crate::index::vector::VectorStore;
use crate::util::rrf::{RrfAccumulator, DEFAULT_RRF_K};
use std::collections::HashMap;

/// 语义腿带时间/role 过滤时的 over-fetch 倍率与上限：KNN LIMIT 在 Rust 侧过滤
/// 之前执行，不多取会因过滤掉部分候选而欠返回（低于 top_k）。×5 为经验值，
/// 封顶 200 防大 top_k 放大 KNN 开销（审查 v4 维度7）。
const SEMANTIC_OVERFETCH_FACTOR: usize = 5;
const SEMANTIC_OVERFETCH_CAP: usize = 200;

/// Hybrid 单路候选池倍率与上下限：RRF 融合需比 top_k 更宽的候选池以容纳两路
/// 并集；×3、clamp[20,100] 为经验值（审查 v4 维度7）。
const HYBRID_CANDIDATE_FACTOR: usize = 3;
const HYBRID_CANDIDATE_MIN: usize = 20;
const HYBRID_CANDIDATE_MAX: usize = 100;

/// 搜索参数
pub struct SearchParams {
    pub query: String,
    pub search_mode: SearchMode,
    pub top_k: usize,
    pub after_ms: Option<i64>,
    pub before_ms: Option<i64>,
    pub role: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchMode {
    Semantic,
    Keyword,
    Hybrid,
}

impl std::str::FromStr for SearchMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "hybrid" => Ok(Self::Hybrid),
            "semantic" => Ok(Self::Semantic),
            "keyword" => Ok(Self::Keyword),
            other => Err(format!(
                "invalid search mode '{}': must be one of 'hybrid', 'semantic', 'keyword'",
                other
            )),
        }
    }
}

impl SearchMode {
    /// Parse CLI `--mode` flag values, preserving `vector`/`fts` aliases and
    /// defaulting unknown strings to `Hybrid` for CLI backward compatibility.
    pub fn from_cli_str(s: &str) -> Self {
        match s {
            "semantic" | "vector" => Self::Semantic,
            "keyword" | "fts" => Self::Keyword,
            _ => Self::Hybrid,
        }
    }
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
    if params.top_k == 0 {
        return Ok(Vec::new());
    }
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
    if params.top_k == 0 {
        return Ok(Vec::new());
    }
    match params.search_mode {
        SearchMode::Keyword => keyword_search(db, params),
        SearchMode::Semantic => {
            let query_vec = query_vec.ok_or_else(|| anyhow::anyhow!("语义搜索需要嵌入引擎"))?;
            semantic_search(db, &query_vec, params)
        }
        SearchMode::Hybrid => hybrid_search(db, query_vec, params),
    }
}

#[derive(Clone)]
struct TurnMeta {
    session_id: String,
    timestamp_ms: i64,
    role: String,
    preview: String,
}

/// 关键词搜索
fn keyword_search(db: &Db, params: &SearchParams) -> anyhow::Result<Vec<SearchResult>> {
    if params.top_k == 0 {
        return Ok(Vec::new());
    }
    let fts = FtsStore::new(db);
    let fts_results = fts.search_turns_with_filters(
        &params.query,
        params.after_ms,
        params.before_ms,
        params.role.as_deref(),
        params.top_k,
    )?;

    let results = fts_results
        .into_iter()
        .map(|r| SearchResult {
            turn_id: r.turn_id,
            score: -r.rank, // FTS5 rank is negative, invert for consistent "higher is better"
            preview: r.preview,
            session_id: r.session_id,
            timestamp_ms: r.timestamp_ms,
            role: r.role,
            scores: Some(ScoreBreakdown {
                semantic: None,
                keyword: Some(-r.rank),
            }),
        })
        .collect();
    Ok(results)
}

/// 语义搜索（预计算向量版）：不再内部 embed，可安全在锁内运行。
fn semantic_search(
    db: &Db,
    query_vec: &[f32],
    params: &SearchParams,
) -> anyhow::Result<Vec<SearchResult>> {
    if params.top_k == 0 {
        return Ok(Vec::new());
    }
    let vec_store = VectorStore::new(db);
    // Over-fetch when time/role filters are applied in Rust after the KNN LIMIT,
    // otherwise post-filtering could under-return below top_k.
    let fetch_k =
        if params.role.is_some() || params.after_ms.is_some() || params.before_ms.is_some() {
            let overfetch = params.top_k.saturating_mul(SEMANTIC_OVERFETCH_FACTOR);
            overfetch.min(SEMANTIC_OVERFETCH_CAP).max(params.top_k)
        } else {
            params.top_k
        };
    let vec_results = vec_store.search(query_vec, fetch_k)?;

    if vec_results.is_empty() {
        return Ok(Vec::new());
    }

    // Single chunked query for session context + preview (replaces two separate IN queries)
    let turn_ids: Vec<i64> = vec_results.iter().map(|(id, _)| *id).collect();
    let turns = batch_get_turns(db, &turn_ids)?;

    let mut results = Vec::new();
    for (turn_id, distance) in vec_results {
        if let Some(result) = semantic_result(params, &turns, turn_id, distance) {
            results.push(result);
        }
    }
    results.truncate(params.top_k);
    Ok(results)
}

/// 构建单条语义搜索结果：上下文缺失或被时间/Role 过滤时返回 None。
fn semantic_result(
    params: &SearchParams,
    turns: &HashMap<i64, TurnMeta>,
    turn_id: i64,
    distance: f32,
) -> Option<SearchResult> {
    let meta = turns.get(&turn_id)?;

    if params
        .after_ms
        .is_some_and(|after| meta.timestamp_ms < after)
    {
        return None;
    }
    if params
        .before_ms
        .is_some_and(|before| meta.timestamp_ms > before)
    {
        return None;
    }
    if params.role.as_deref().is_some_and(|role| meta.role != role) {
        return None;
    }

    Some(SearchResult {
        turn_id,
        score: 1.0 - distance as f64, // 余弦距离 → 相似度
        preview: meta.preview.clone(),
        session_id: meta.session_id.clone(),
        timestamp_ms: meta.timestamp_ms,
        role: meta.role.clone(),
        scores: Some(ScoreBreakdown {
            semantic: Some(1.0 - distance as f64),
            keyword: None,
        }),
    })
}

fn turn_meta_from_result(r: SearchResult) -> (i64, TurnMeta) {
    (
        r.turn_id,
        TurnMeta {
            session_id: r.session_id,
            timestamp_ms: r.timestamp_ms,
            role: r.role,
            preview: r.preview,
        },
    )
}

fn resolve_hybrid_legs(
    sem_res: Option<anyhow::Result<Vec<SearchResult>>>,
    kw_res: anyhow::Result<Vec<SearchResult>>,
) -> anyhow::Result<(Vec<SearchResult>, Vec<SearchResult>)> {
    match (sem_res, kw_res) {
        (None, Err(k_err)) => Err(k_err),
        (Some(Err(s_err)), Err(k_err)) => anyhow::bail!(
            "hybrid search failed on both legs: semantic=({s_err}); keyword=({k_err})"
        ),
        (Some(Err(s_err)), Ok(kw)) => {
            tracing::warn!("hybrid: 语义搜索失败，降级为仅关键词: {}", s_err);
            Ok((Vec::new(), kw))
        }
        (Some(Ok(sem)), Err(k_err)) => {
            tracing::warn!("hybrid: 关键词搜索失败，降级为仅语义: {}", k_err);
            Ok((sem, Vec::new()))
        }
        (sem, Ok(kw)) => Ok((sem.transpose()?.unwrap_or_default(), kw)),
    }
}

/// Hybrid 搜索（RRF 融合，扩大单路候选池并保证确定性并列排序）
fn hybrid_search(
    db: &Db,
    query_vec: Option<Vec<f32>>,
    params: &SearchParams,
) -> anyhow::Result<Vec<SearchResult>> {
    if params.top_k == 0 {
        return Ok(Vec::new());
    }
    let candidate_k = params
        .top_k
        .saturating_mul(HYBRID_CANDIDATE_FACTOR)
        .clamp(HYBRID_CANDIDATE_MIN, HYBRID_CANDIDATE_MAX)
        .max(params.top_k);
    let candidate_params = SearchParams {
        query: params.query.clone(),
        search_mode: params.search_mode.clone(),
        top_k: candidate_k,
        after_ms: params.after_ms,
        before_ms: params.before_ms,
        role: params.role.clone(),
    };

    let sem_res = query_vec
        .as_deref()
        .map(|qv| semantic_search(db, qv, &candidate_params));
    let kw_res = keyword_search(db, &candidate_params);
    let (semantic_results, keyword_results) = resolve_hybrid_legs(sem_res, kw_res)?;

    let mut acc = RrfAccumulator::<i64, TurnMeta, 2>::new(DEFAULT_RRF_K);
    acc.add_leg(0, semantic_results.into_iter().map(turn_meta_from_result));
    acc.add_leg(1, keyword_results.into_iter().map(turn_meta_from_result));
    let ranked = acc.into_ranked_by(|a, b| {
        b.value
            .timestamp_ms
            .cmp(&a.value.timestamp_ms)
            .then_with(|| b.key.cmp(&a.key))
    });
    Ok(ranked
        .into_iter()
        .take(params.top_k)
        .map(|hit| SearchResult {
            turn_id: hit.key,
            score: hit.score,
            preview: hit.value.preview,
            session_id: hit.value.session_id,
            timestamp_ms: hit.value.timestamp_ms,
            role: hit.value.role,
            scores: Some(ScoreBreakdown {
                semantic: hit.leg_scores[0],
                keyword: hit.leg_scores[1],
            }),
        })
        .collect())
}

/// Single chunked query fetching `(session_id, timestamp_ms, role, preview)` for turn IDs.
fn batch_get_turns(db: &Db, turn_ids: &[i64]) -> anyhow::Result<HashMap<i64, TurnMeta>> {
    if turn_ids.is_empty() {
        return Ok(HashMap::new());
    }

    let mut map = HashMap::with_capacity(turn_ids.len());
    crate::index::sql_util::for_each_in_chunk(
        turn_ids,
        |chunk, placeholders| -> anyhow::Result<()> {
            let sql = format!(
            "SELECT id, session_id, timestamp_ms, role, COALESCE(preview, '') FROM turns WHERE id IN ({placeholders})"
        );
            let mut stmt = db.conn().prepare(&sql)?;
            let rows = stmt.query_map(rusqlite::params_from_iter(chunk), |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    TurnMeta {
                        session_id: row.get(1)?,
                        timestamp_ms: row.get(2)?,
                        role: row.get(3)?,
                        preview: row.get(4)?,
                    },
                ))
            })?;
            for row in rows {
                let (id, meta) = row?;
                map.insert(id, meta);
            }
            Ok(())
        },
    )?;
    Ok(map)
}

#[cfg(test)]
#[path = "search_tests.rs"]
mod tests;

//! 图谱读路径：N-hop 邻居（有界 BFS + 索引点查）。
//! M14 自 query.rs 拆出；共享 wire 类型与实体键解析仍在 query.rs。

use super::{resolve_entity_canonical, Direction, Neighbor, NeighborQuery, MAX_HOPS};
use crate::index::db::Db;
use std::collections::{HashMap, HashSet};

/// 结果帽（不是有效性约束）：越界值静默 clamp 到 1..=200，**不报错**——
/// limit=0 因此返回 1 条而非空集。与 hops 的 400 校验尺标不同是有意的：
/// limit 只表达"最多要几条"，调用方（REST handler）若要拒绝 0 需自行预校验。
const MAX_LIMIT: u32 = 200;
const MAX_NEIGHBOR_VISITED: usize = 10_000;

/// W25: 邻居累积器——首访才入 Vec，避免稠密图下同一邻居被多条边重复返回时
/// 重复串堆积（回答 limit=200 的查询可物化数十 MB）。去重不改变结果集，且保留
/// 首次出现顺序（配合 ORDER BY，在**单个 chunk 与单个方向内**是字典序；跨 chunk
/// 仍按 frontier 顺序拼接，故触帽时的子集不是全局字典序前缀）。
/// 收成一个参数也避免 query_directed_edges 超过 Sonar S107 的 7 参数上限。
struct NeighborAccum<'a> {
    out: &'a mut Vec<String>,
    seen: &'a mut HashSet<String>,
}

impl NeighborAccum<'_> {
    fn push(&mut self, nbr: String) {
        if self.seen.insert(nbr.clone()) {
            self.out.push(nbr);
        }
    }
}

fn query_directed_edges(
    conn: &rusqlite::Connection,
    placeholders: &str,
    select_col: &str,
    filter_col: &str,
    params: &[&dyn rusqlite::ToSql],
    rel_bind: Option<usize>,
    acc: &mut NeighborAccum<'_>,
) -> anyhow::Result<()> {
    // W17: 必须 ORDER BY——节点帽会在整层中途 break，无排序时 discovered 子集
    // 取决于 SQLite 扫描顺序，事后排序无法恢复真正的字典序前缀（选择契约是
    // "最小距离上字典序最前的 limit 个"）。
    let sql = match rel_bind {
        Some(idx) => format!(
            "SELECT {select_col} FROM relations WHERE {filter_col} IN ({placeholders}) AND rel_type = ?{idx} AND relation_kind IN ('asserted', 'derived') ORDER BY {select_col}"
        ),
        None => format!(
            "SELECT {select_col} FROM relations WHERE {filter_col} IN ({placeholders}) AND relation_kind IN ('asserted', 'derived') ORDER BY {select_col}"
        ),
    };
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params, |r| r.get::<_, String>(0))?;
    for r in rows {
        acc.push(r?);
    }
    Ok(())
}

fn query_adjacent_batch(
    conn: &rusqlite::Connection,
    nodes: &[String],
    direction: Direction,
    rel_type: Option<&str>,
) -> anyhow::Result<Vec<String>> {
    let mut nbrs = Vec::new();
    let mut seen = HashSet::new();
    crate::index::sql_util::for_each_in_chunk(
        nodes,
        |chunk, placeholders| -> anyhow::Result<()> {
            let rt_val = rel_type.unwrap_or_default();
            let mut params: Vec<&dyn rusqlite::ToSql> =
                chunk.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
            let rel_bind = rel_type.map(|_| {
                params.push(&rt_val);
                chunk.len() + 1
            });
            // relation_kind 白名单：与 path / multi_hop_query 统一（策略见 mod.rs）。
            if matches!(direction, Direction::Out | Direction::Both) {
                query_directed_edges(
                    conn,
                    placeholders,
                    "dst_canonical",
                    "src_canonical",
                    params.as_slice(),
                    rel_bind,
                    &mut NeighborAccum {
                        out: &mut nbrs,
                        seen: &mut seen,
                    },
                )?;
            }
            if matches!(direction, Direction::In | Direction::Both) {
                query_directed_edges(
                    conn,
                    placeholders,
                    "src_canonical",
                    "dst_canonical",
                    params.as_slice(),
                    rel_bind,
                    &mut NeighborAccum {
                        out: &mut nbrs,
                        seen: &mut seen,
                    },
                )?;
            }
            Ok(())
        },
    )?;
    Ok(nbrs)
}

fn hydrate_neighbors(
    conn: &rusqlite::Connection,
    distances: &HashMap<String, u32>,
) -> anyhow::Result<Vec<Neighbor>> {
    let keys: Vec<&String> = distances.keys().collect();
    let mut out = Vec::with_capacity(keys.len());
    crate::index::sql_util::for_each_in_chunk(
        &keys,
        |chunk, placeholders| -> anyhow::Result<()> {
            let sql = format!(
            "SELECT canonical, name, entity_type FROM entities WHERE canonical IN ({placeholders})"
        );
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(rusqlite::params_from_iter(chunk.iter().copied()), |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })?;
            for row in rows {
                let (canonical, name, entity_type) = row?;
                if let Some(&distance) = distances.get(&canonical) {
                    out.push(Neighbor {
                        canonical,
                        name,
                        entity_type,
                        distance,
                    });
                }
            }
            Ok(())
        },
    )?;
    Ok(out)
}

fn collect_neighbor_distances(
    conn: &rusqlite::Connection,
    start_canon: String,
    q: &NeighborQuery,
    rel_filter: Option<&str>,
    limit: usize,
) -> anyhow::Result<(HashMap<String, u32>, bool)> {
    let mut visited: HashSet<String> = HashSet::new();
    visited.insert(start_canon.clone());
    let mut distances: HashMap<String, u32> = HashMap::new();
    let mut frontier: Vec<String> = vec![start_canon];

    let mut truncated = false;
    for hop in 1..=q.hops {
        if frontier.is_empty() {
            break;
        }
        let mut next_frontier = Vec::new();
        for nbr in query_adjacent_batch(conn, &frontier, q.direction, rel_filter)? {
            if !visited.insert(nbr.clone()) {
                continue;
            }
            // W17: cap+1 探测——第 MAX+1 个**新**节点才证明帽外还有结果。
            // 旧实现 `>= cap` 即置 truncated：恰好触帽（最后一个邻居入集）
            // 或帽满但后续批次全是已访节点时误报截断。探到的多余节点不入
            // distances/frontier（结果集精确截到 cap）。
            if visited.len() > MAX_NEIGHBOR_VISITED {
                visited.remove(&nbr);
                truncated = true;
                break;
            }
            distances.insert(nbr.clone(), hop);
            next_frontier.push(nbr);
        }
        frontier = next_frontier;
        if truncated {
            break;
        }
        // 满 limit 早停：必须在整层结束后判。
        if distances.len() >= limit {
            break;
        }
    }
    Ok((distances, truncated))
}

/// 查询某实体的 N-hop 邻居（兼容包装，丢弃截断标志；生产路径请用
/// [`neighbors_with_truncation`]，口径同 multi_hop_query 的 cfg(test) 包装）。
#[cfg(test)]
pub fn neighbors(db: &Db, q: &NeighborQuery) -> anyhow::Result<Vec<Neighbor>> {
    Ok(neighbors_with_truncation(db, q)?.0)
}

/// W17: 带截断标志的变体——节点帽命中时返回的是部分集，调用方必须透出
/// `truncated`，否则调用方会把不完整的邻居集当完整答案（口径同 multi-hop 的
/// `multi_hop_query_with_truncation`）。
pub fn neighbors_with_truncation(
    db: &Db,
    q: &NeighborQuery,
) -> anyhow::Result<(Vec<Neighbor>, bool)> {
    if !(1..=MAX_HOPS).contains(&q.hops) {
        anyhow::bail!("hops must be in 1..={}, got {}", MAX_HOPS, q.hops);
    }
    // 结果帽 clamp（limit=0 → 1，不报错）：语义见 MAX_LIMIT 文档。
    let limit = q.limit.clamp(1, MAX_LIMIT) as usize;
    let canon = resolve_entity_canonical(db.conn(), &q.entity)?;
    if canon.is_empty() {
        return Ok((Vec::new(), false));
    }

    // M7 对称：写侧 rel_type 已 trim，查询过滤同样 trim（空白变体匹配规范化行；
    // 纯空白过滤视同无过滤）。
    let rel_filter = q
        .rel_type
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());

    let conn = db.conn();
    let (distances, truncated) = collect_neighbor_distances(conn, canon, q, rel_filter, limit)?;
    if distances.is_empty() {
        return Ok((Vec::new(), truncated));
    }

    // W28e: 先按 (distance, canonical) 排序截断到 limit，再 hydrate 选中节点。
    let mut ranked: Vec<(&String, u32)> = distances.iter().map(|(k, &d)| (k, d)).collect();
    ranked.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(b.0)));
    ranked.truncate(limit);
    let selected: HashMap<String, u32> = ranked.into_iter().map(|(k, d)| (k.clone(), d)).collect();
    let mut out = hydrate_neighbors(conn, &selected)?;
    out.sort_by(|a, b| {
        a.distance
            .cmp(&b.distance)
            .then_with(|| a.canonical.cmp(&b.canonical))
    });
    Ok((out, truncated))
}

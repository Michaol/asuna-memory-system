//! 图谱读路径：邻居 + 路径（基于 SQL JOIN / 递归 CTE）。

use crate::graph::canonical::canonicalize;
use crate::index::db::Db;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    Out,
    In,
    #[default]
    Both,
}

#[derive(Debug, Deserialize)]
pub struct NeighborQuery {
    pub entity: String,
    #[serde(default)]
    pub rel_type: Option<String>,
    #[serde(default)]
    pub direction: Direction,
    #[serde(default = "default_hops")]
    pub hops: u32,
    #[serde(default = "default_limit")]
    pub limit: u32,
}

fn default_hops() -> u32 {
    1
}
fn default_limit() -> u32 {
    50
}

#[derive(Debug, Serialize)]
pub struct Neighbor {
    pub canonical: String,
    pub name: String,
    pub entity_type: String,
    pub distance: u32,
}

const MAX_HOPS: u32 = 5;
const MAX_LIMIT: u32 = 200;

/// 查询某实体的 N-hop 邻居。
///
/// - `direction = Out`: 沿出边遍历（src → dst）
/// - `direction = In`:  沿入边遍历（dst ← src）
/// - `direction = Both`: 双向遍历（无向图）
///
/// `hops` 限制在 1..=5，`limit` 限制在 1..=200。
/// 递归 CTE 使用 `UNION`（去重）而非 `UNION ALL`，保证有环图能终止。
pub fn neighbors(db: &Db, q: &NeighborQuery) -> anyhow::Result<Vec<Neighbor>> {
    if !(1..=MAX_HOPS).contains(&q.hops) {
        anyhow::bail!("hops must be in 1..={}, got {}", MAX_HOPS, q.hops);
    }
    let limit = q.limit.clamp(1, MAX_LIMIT);
    let canon = canonicalize(&q.entity);
    if canon.is_empty() {
        return Ok(Vec::new());
    }

    // 根据方向选择 JOIN 子句（在 CTE 递归步骤中决定下一跳节点）
    let recurse_step = match q.direction {
        Direction::Out => {
            "JOIN relations r ON r.src_canonical = visited.canonical
             JOIN entities e ON e.canonical = r.dst_canonical"
        }
        Direction::In => {
            "JOIN relations r ON r.dst_canonical = visited.canonical
             JOIN entities e ON e.canonical = r.src_canonical"
        }
        Direction::Both => {
            "JOIN relations r
                ON r.src_canonical = visited.canonical OR r.dst_canonical = visited.canonical
             JOIN entities e ON e.canonical = CASE
                WHEN r.src_canonical = visited.canonical THEN r.dst_canonical
                ELSE r.src_canonical
             END"
        }
    };

    let rel_filter_clause = if q.rel_type.is_some() {
        " AND r.rel_type = ?"
    } else {
        ""
    };

    let sql = format!(
        "WITH RECURSIVE visited(canonical, distance) AS (
            SELECT ?, 0
            UNION
            SELECT e.canonical, visited.distance + 1
            FROM visited
            {recurse_step}
            WHERE visited.distance < ?
              {rel_filter_clause}
        )
        SELECT DISTINCT v.canonical, e.name, e.entity_type, v.distance
        FROM visited v
        JOIN entities e ON e.canonical = v.canonical
        WHERE v.distance > 0 AND v.canonical <> ?
        ORDER BY v.distance, v.canonical
        LIMIT ?"
    );

    let conn = db.conn();
    let mut stmt = conn.prepare(&sql)?;

    // 参数顺序：seed canonical, hops, [rel_type?], seed canonical (排除), limit
    let mut params: Vec<Box<dyn rusqlite::ToSql>> =
        vec![Box::new(canon.clone()), Box::new(q.hops as i64)];
    if let Some(rt) = &q.rel_type {
        params.push(Box::new(rt.clone()));
    }
    params.push(Box::new(canon.clone()));
    params.push(Box::new(limit as i64));

    let param_refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|p| p.as_ref()).collect();

    let rows = stmt.query_map(param_refs.as_slice(), |row| {
        Ok(Neighbor {
            canonical: row.get(0)?,
            name: row.get(1)?,
            entity_type: row.get(2)?,
            distance: row.get::<_, i64>(3)? as u32,
        })
    })?;

    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

#[derive(Debug, Serialize)]
pub struct PathResult {
    pub found: bool,
    pub length: u32,
    pub path: Vec<PathStep>,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
#[allow(dead_code)] // v1.3.0 path() returns empty Vec; variants reserved for v1.3.1 polish
pub enum PathStep {
    Entity { canonical: String, name: String },
    Edge { rel_type: String },
}

const MAX_PATH_HOPS: u32 = 10;

/// 在两节点间寻找最短路径（无向遍历）。
///
/// 使用 BFS 风格的递归 CTE，按 distance 升序枚举从 src 可达的节点；
/// 找到 dst 即返回最短距离。
///
/// v1.3.0 仅返回 `found` 和 `length`；详细路径节点的序列化（`path` 字段）
/// 留作 v1.3.1 polish——`found`/`length` 已覆盖 agent 主要使用场景。
///
/// `max_hops` 限制在 1..=10。
pub fn path(db: &Db, src: &str, dst: &str, max_hops: u32) -> anyhow::Result<PathResult> {
    if !(1..=MAX_PATH_HOPS).contains(&max_hops) {
        anyhow::bail!("max_hops must be in 1..={}, got {}", MAX_PATH_HOPS, max_hops);
    }
    let src_c = canonicalize(src);
    let dst_c = canonicalize(dst);

    // Empty canonical after normalization → not found
    if src_c.is_empty() || dst_c.is_empty() {
        return Ok(PathResult {
            found: false,
            length: 0,
            path: Vec::new(),
        });
    }

    // src == dst → trivially "found" at distance 0
    if src_c == dst_c {
        return Ok(PathResult {
            found: true,
            length: 0,
            path: Vec::new(),
        });
    }

    // BFS via recursive CTE: walk undirected edges, take MIN distance to dst
    let sql = "
        WITH RECURSIVE bfs(node, distance) AS (
            SELECT ?, 0
            UNION
            SELECT
                CASE
                    WHEN r.src_canonical = bfs.node THEN r.dst_canonical
                    ELSE r.src_canonical
                END,
                bfs.distance + 1
            FROM bfs
            JOIN relations r
              ON r.src_canonical = bfs.node OR r.dst_canonical = bfs.node
            WHERE bfs.distance < ?
        )
        SELECT MIN(distance) FROM bfs WHERE node = ?
    ";

    let conn = db.conn();
    let row: Option<i64> = conn
        .query_row(
            sql,
            rusqlite::params![src_c, max_hops as i64, dst_c],
            |r| r.get(0),
        )
        .ok()
        .flatten();

    match row {
        Some(len) if len > 0 => Ok(PathResult {
            found: true,
            length: len as u32,
            path: Vec::new(),
        }),
        _ => Ok(PathResult {
            found: false,
            length: 0,
            path: Vec::new(),
        }),
    }
}

/// 返回 `turn_ids` 中**未被任何 `relations.source_turn` 引用**的子集。
///
/// `save_session` 用此构造 `graph_pending.turn_ids` 软提示——告诉 agent
/// 这些 turn 还没有对应的图谱断言。空输入返回空 Vec；保留输入顺序。
pub fn pending_turn_ids(db: &Db, turn_ids: &[i64]) -> anyhow::Result<Vec<i64>> {
    if turn_ids.is_empty() {
        return Ok(Vec::new());
    }

    // 动态构造 IN (?, ?, ...) 占位符
    let placeholders: String = (0..turn_ids.len())
        .map(|_| "?")
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "SELECT DISTINCT source_turn FROM relations
         WHERE source_turn IN ({placeholders}) AND source_turn IS NOT NULL"
    );

    let conn = db.conn();
    let mut stmt = conn.prepare(&sql)?;
    let params: Vec<&dyn rusqlite::ToSql> =
        turn_ids.iter().map(|t| t as &dyn rusqlite::ToSql).collect();
    let referenced: std::collections::HashSet<i64> = stmt
        .query_map(params.as_slice(), |row| row.get(0))?
        .collect::<Result<_, _>>()?;

    Ok(turn_ids
        .iter()
        .filter(|t| !referenced.contains(t))
        .copied()
        .collect())
}

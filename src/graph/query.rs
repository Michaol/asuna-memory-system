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

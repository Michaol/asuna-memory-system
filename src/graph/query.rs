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
        SELECT v.canonical, e.name, e.entity_type, MIN(v.distance) AS distance
        FROM visited v
        JOIN entities e ON e.canonical = v.canonical
        WHERE v.distance > 0 AND v.canonical <> ?
        GROUP BY v.canonical, e.name, e.entity_type
        ORDER BY distance, v.canonical
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

/// 路径元素：实体节点和关系边交替出现
/// 序列：[Entity, Edge, Entity, Edge, ..., Entity]
/// 元素数 = 2*length + 1
#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum PathStep {
    Entity { canonical: String, name: String },
    Edge { rel_type: String },
}

const MAX_PATH_HOPS: u32 = 10;

// 路径序列化用的不可见 ASCII 控制字符分隔符（US = Unit Separator）。
// agent 不会在合法 entity 名 / rel_type 里使用此字符；如果发生（极罕见），路径
// 解析会失败但 found/length 仍正确（解析回退到空路径）。
const PATH_SEP: &str = "\x1F";

/// 在两节点间寻找最短路径（无向遍历）。
///
/// 使用 BFS 递归 CTE，按 distance 升序枚举从 src 可达的节点；找到 dst 即返回最短距离。
/// 路径携带：每个 BFS 行额外存储 `path_str`（用 \x1F 分隔的 canonical/rel_type 序列），
/// 命中 dst 后 split 还原 `Vec<PathStep>`。
///
/// 返回结构：`{found, length, path}`，其中 path 是 [Entity, Edge, Entity, Edge, ..., Entity]
/// 交替序列，共 `2 * length + 1` 个元素。src==dst 时 path 为空 Vec。
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

    // BFS 携带路径字符串：每个节点累积形如 "src\x1Frel1\x1Fmid\x1Frel2\x1Fdst" 的序列
    // 排除已访问节点（用 LIKE pattern 避免环）。
    let sql = "
        WITH RECURSIVE bfs(node, distance, path_str) AS (
            SELECT ?, 0, ?
            UNION ALL
            SELECT
                CASE
                    WHEN r.src_canonical = bfs.node THEN r.dst_canonical
                    ELSE r.src_canonical
                END,
                bfs.distance + 1,
                bfs.path_str || ? || r.rel_type || ? ||
                CASE
                    WHEN r.src_canonical = bfs.node THEN r.dst_canonical
                    ELSE r.src_canonical
                END
            FROM bfs
            JOIN relations r
              ON r.src_canonical = bfs.node OR r.dst_canonical = bfs.node
            WHERE bfs.distance < ?
              AND instr(bfs.path_str || ?,
                        ? || (CASE WHEN r.src_canonical = bfs.node THEN r.dst_canonical
                                   ELSE r.src_canonical END) || ?) = 0
        )
        SELECT distance, path_str FROM bfs
        WHERE node = ?
        ORDER BY distance ASC
        LIMIT 1
    ";

    let conn = db.conn();
    let result: Option<(i64, String)> = conn
        .query_row(
            sql,
            rusqlite::params![
                src_c,
                src_c.clone(),
                PATH_SEP,
                PATH_SEP,
                max_hops as i64,
                PATH_SEP,
                PATH_SEP,
                PATH_SEP,
                dst_c,
            ],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
        )
        .ok();

    match result {
        Some((len, path_str)) if len > 0 => {
            let path = parse_path_str(&path_str, conn);
            Ok(PathResult {
                found: true,
                length: len as u32,
                path,
            })
        }
        _ => Ok(PathResult {
            found: false,
            length: 0,
            path: Vec::new(),
        }),
    }
}

/// 把 BFS 携带的 path_str 解析为 [Entity, Edge, Entity, Edge, ..., Entity] 序列。
///
/// 输入格式：`"canonical1\x1Frel_type1\x1Fcanonical2\x1Frel_type2\x1F...\x1FcanonicalN"`
/// 即奇数位（0,2,4...）是 entity canonical，偶数位（1,3,5...）是 rel_type。
///
/// Entity 的 `name` 字段从 entities 表查询；查不到时回退为 canonical。
///
/// 解析失败（如 path_str 含意外内容）时返回空 Vec；调用方应仍能凭 found/length 处理。
fn parse_path_str(path_str: &str, conn: &rusqlite::Connection) -> Vec<PathStep> {
    let parts: Vec<&str> = path_str.split(PATH_SEP).collect();
    if parts.is_empty() || parts.len().is_multiple_of(2) {
        // 序列长度必为奇数（实体-边-实体-边-...-实体）
        return Vec::new();
    }
    let mut out = Vec::with_capacity(parts.len());
    for (i, part) in parts.iter().enumerate() {
        if i % 2 == 0 {
            // entity：查 name 回填，失败回退到 canonical
            let canonical = (*part).to_string();
            let name = conn
                .query_row(
                    "SELECT name FROM entities WHERE canonical = ?1",
                    rusqlite::params![canonical],
                    |r| r.get::<_, String>(0),
                )
                .unwrap_or_else(|_| canonical.clone());
            out.push(PathStep::Entity { canonical, name });
        } else {
            out.push(PathStep::Edge {
                rel_type: (*part).to_string(),
            });
        }
    }
    out
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

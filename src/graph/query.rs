//! 图谱读路径：邻居 + 路径（有界 BFS + 索引点查）。
//!
//! M14 拆分：邻居 BFS 在 query/neighbors.rs，最短路径 BFS 在 query/path.rs。
//! 本文件保留共享 wire 类型、实体键解析与 pending_turn_ids，并原样再导出两个
//! 子模块的公开项——`crate::graph::query::*` 的既有路径逐字不变。

use crate::graph::canonical::canonicalize;
use crate::index::db::Db;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

mod neighbors;
mod path;

#[cfg(test)]
pub use neighbors::neighbors;
pub use neighbors::neighbors_with_truncation;
pub use path::{path, path_with_max_results, PathResult, PathStep};
// query_unit_tests.rs 经 `use super::*` 取 reconstruct_path / HashMap。
#[cfg(test)]
use path::reconstruct_path;
#[cfg(test)]
use std::collections::HashMap;

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

/// 邻居查询的跳数上限（REST handler 预校验与 neighbors 内部校验共用，
/// 保证"校验类错误 → 400、内部错误 → 500"的映射不漂移）。
/// 为什么是 5 而 path 是 10：见 mod.rs 的"跳数上限"策略说明。
pub const MAX_HOPS: u32 = 5;

/// W14: 实体键解析。session 实体键改用**无损**原始 id（`session_<id>`，不做
/// 大小写/空白折叠）——`validate_session_id` 允许大写与空格，而 sessions.session_id
/// 是 BINARY 主键，折叠会把 "Sprint-1" 与 "sprint-1" 两个不同会话合成一个图节点，
/// 多跳/邻居检索跨会话混合记忆。解析顺序：**先精确匹配、再回退规范化**——若先试
/// canonicalize，当大小写两行并存时会解析到错误的那个会话。历史行按规范化形存储、
/// 调用方传原始名（如 "Alice" → "alice"）时精确匹配落空，回退命中，向后兼容。
/// 非 `session_` 前缀的实体一律规范化，行为与既有完全一致。
pub fn resolve_entity_canonical(
    conn: &rusqlite::Connection,
    entity: &str,
) -> anyhow::Result<String> {
    let canon = canonicalize(entity);
    // 只有 session 键是无损写入的（link_from_session_edge）；其余实体一律规范化，
    // 保持既有行为不变。
    if !entity.starts_with("session_") {
        return Ok(canon);
    }
    // M11c: 旧 `canon.is_empty()` 早退分支不可达——能走到这里的 entity 必以
    // "session_" 开头，canonicalize（trim/lowercase/折空白）保留该前缀，canon 恒非空。
    let hit = |key: &str| -> rusqlite::Result<bool> {
        conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM entities WHERE canonical = ?1)",
            rusqlite::params![key],
            |r| r.get(0),
        )
    };
    // 先精确匹配、再回退规范化：session 键是无损原始 id，若先试 canonicalize，
    // 当 "session_Sprint-1" 与 "session_sprint-1" 两行并存时会解析到错误的那个会话。
    // 历史行按规范化形存储、调用方传原始名（如 "Alice" → "alice"）时精确匹配落空，
    // 回退命中，向后兼容。
    if hit(entity)? {
        Ok(entity.to_string())
    } else {
        Ok(canon)
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

    let conn = db.conn();
    let mut referenced: HashSet<i64> = HashSet::new();
    crate::index::sql_util::for_each_in_chunk(
        turn_ids,
        |chunk, placeholders| -> anyhow::Result<()> {
            let sql = format!(
                "SELECT DISTINCT source_turn FROM relations
                 WHERE source_turn IN ({placeholders}) AND source_turn IS NOT NULL"
            );
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(rusqlite::params_from_iter(chunk), |row| {
                row.get::<_, i64>(0)
            })?;
            for r in rows {
                referenced.insert(r?);
            }
            Ok(())
        },
    )?;

    Ok(turn_ids
        .iter()
        .filter(|t| !referenced.contains(t))
        .copied()
        .collect())
}

#[cfg(test)]
#[path = "query_unit_tests.rs"]
mod tests;

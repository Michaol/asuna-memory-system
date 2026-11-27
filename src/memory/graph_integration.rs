//! Graph integration for memory atoms
//!
//! When L1 atoms are stored, automatically create graph entities and relations:
//! - Each atom gets a corresponding entity (with memory_atom_id)
//! - `mentions` relations: atom -> extracted entities
//! - `supersedes` relations: new_atom -> old_atom (for conflicts)
//! - `from_session` relations: atom -> session
//! - `related_to` relations: atom -> similar_atom (high vector similarity)
//!
//! 这是图谱的**派生层**（`relation_kind='derived'`）：断言层由 agent 显式写入
//! （graph/store.rs），分层与读路径可见性策略见 graph/mod.rs。
//!
//! 本模块的 entity 写入一律 `INSERT OR IGNORE`，重复时**不刷新 last_seen**
//! （断言层 store.rs 的 upsert 会刷新）。last_seen 目前是 dormant 列（全仓只写
//! 不读），故不为它给每个原子的派生热路径多加一次 UPDATE。

use crate::graph::canonical::canonicalize;
use crate::index::db::Db;
use crate::util::time;
use anyhow::Result;
use rusqlite::params;
use rusqlite::Connection;
use std::collections::HashSet;

/// 派生边的固定置信度——不是 LLM 自评分，而是按"边的证据强度"分档的写侧常量：
/// - supersedes / from_session：由 DB 的 supersedes_id / 会话归属直接确定 → 1.0
/// - mentions：实体名来自 L1 的 LLM 抽取，可能误抽 → 0.8
/// - related_to：仅凭向量相似度超阈值的启发式关联，证据最弱 → 0.7
const CONFIDENCE_SUPERSEDES: f64 = 1.0;
const CONFIDENCE_FROM_SESSION: f64 = 1.0;
const CONFIDENCE_MENTIONS: f64 = 0.8;
const CONFIDENCE_RELATED_TO: f64 = 0.7;

/// 多跳遍历的 visited 节点上限（成本帽，与 graph::query 的 MAX_NEIGHBOR_VISITED
/// 同量级）。触达即 warn 并返回已收集的部分原子集——W5.4：此前静默 break，
/// 调用方无从感知结果不完整。
const MAX_VISITED_NODES: usize = 10_000;

/// Result of graph integration for a single atom
#[derive(Debug, Clone)]
pub struct GraphIntegrationResult {
    #[cfg(test)]
    pub mentions_created: u32,
    #[cfg(test)]
    pub supersedes_created: u32,
    #[cfg(test)]
    pub from_session_created: bool,
    #[cfg(test)]
    pub related_to_created: u32,
}

/// Integrate an L1 atom with the knowledge graph
///
/// Creates an entity for the atom and derives relations based on:
/// - Extracted entities from atom content
/// - Supersedes chain (if atom.supersedes_id is set)
/// - Source session
/// - Vector similarity to other atoms
fn link_derived_mentions(
    conn: &Connection,
    atom_canonical: &str,
    atom_id: i64,
    extracted_entities: &[String],
    now: i64,
) -> Result<u32> {
    let mut created = 0u32;
    for entity_name in extracted_entities {
        let scan = crate::growth::security::scan_content(entity_name);
        if !scan.is_safe() {
            let redacted = crate::growth::security::redact_credentials(entity_name);
            tracing::debug!(
                "Skipping unsafe graph mention {:?} for atom {}: {}",
                crate::util::truncate_for_log(&redacted),
                atom_id,
                scan.reason()
            );
            continue;
        }
        let entity_canonical = canonicalize(entity_name);
        if entity_canonical.is_empty() {
            continue;
        }
        // W24: 保留派生命名空间。抽取出的同名实体（entity_type='extracted'、
        // memory_atom_id 为 NULL）会抢占 `atom_<id>` / `session_*`：驱逐时
        // `DELETE … WHERE memory_atom_id` 删不到抢占者，而 INSERT OR IGNORE 又让
        // 真正的原子实体永远建不起来 → 该原子在多跳检索中永久不可见，还会写出
        // 指向抢占者的 supersedes 边。从源头拒绝比事后探测更彻底。
        if is_reserved_derived_key(&entity_canonical) {
            tracing::debug!(
                "Skipping graph mention {:?}: reserved derived namespace",
                crate::util::truncate_for_log(entity_name)
            );
            continue;
        }
        conn.execute(
            "INSERT OR IGNORE INTO entities (canonical, name, entity_type, first_seen, last_seen)
             VALUES (?1, ?2, 'extracted', ?3, ?3)",
            params![entity_canonical, entity_name, now],
        )?;
        let inserted = conn.execute(
            "INSERT OR IGNORE INTO relations (src_canonical, rel_type, dst_canonical, confidence, source_turn, relation_kind, created_at)
             VALUES (?1, 'mentions', ?2, ?3, NULL, 'derived', ?4)",
            params![atom_canonical, entity_canonical, CONFIDENCE_MENTIONS, now],
        )?;
        if inserted > 0 {
            created += 1;
        }
    }
    Ok(created)
}

fn ensure_atom_entity_exists(conn: &Connection, canonical: &str, atom_id: i64) -> Result<bool> {
    use rusqlite::OptionalExtension;
    conn.execute(
        "INSERT OR IGNORE INTO entities (canonical, name, entity_type, first_seen, last_seen, memory_atom_id)
         SELECT ?1, content, 'memory_atom', created_at, created_at, id
         FROM bounded_memory WHERE id = ?2",
        params![canonical, atom_id],
    )?;
    Ok(conn
        .query_row(
            // W24: 探针必须确认该实体行**确实属于这个原子**。只查 bounded_memory
            // 不够：原子存活但命名空间已被 extracted 实体抢占时，INSERT OR IGNORE
            // 被忽略，边会指向 memory_atom_id 为 NULL 的抢占者（多跳取不到原子）。
            // 查 (canonical, memory_atom_id) 同时覆盖"原子已驱逐"与"被抢占"两种情形，
            // 任一不满足即返回 false，调用方跳过建边（fail-safe，不写幻影边）。
            "SELECT 1 FROM entities WHERE canonical = ?1 AND memory_atom_id = ?2",
            params![canonical, atom_id],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// W24: 派生实体键命名空间（`atom_<id>` / `session_<id>`）不得被抽取名抢占。
fn is_reserved_derived_key(canonical: &str) -> bool {
    if let Some(rest) = canonical.strip_prefix("atom_") {
        return !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit());
    }
    canonical.starts_with("session_")
}

#[cfg(test)]
pub fn integrate_atom_with_graph(
    db: &Db,
    atom_id: i64,
    atom_content: &str,
    supersedes_id: Option<i64>,
    session_id: Option<&str>,
    extracted_entities: &[String],
    similar_atom_ids: &[i64],
) -> Result<GraphIntegrationResult> {
    let tx = db.immediate_tx()?;
    let result = integrate_atom_with_graph_on_conn(
        &tx,
        atom_id,
        atom_content,
        supersedes_id,
        session_id,
        extracted_entities,
        similar_atom_ids,
    )?;
    tx.commit()?;
    Ok(result)
}

/// Inner worker for [`integrate_atom_with_graph`] operating on an active
/// SQLite connection/transaction so batch callers can share one transaction.
fn link_supersedes_edge(
    conn: &Connection,
    atom_canonical: &str,
    supersedes_id: Option<i64>,
    now: i64,
) -> Result<u32> {
    let Some(old_atom_id) = supersedes_id else {
        return Ok(0);
    };
    let old_atom_canonical = canonicalize(&format!("atom_{}", old_atom_id));
    if !ensure_atom_entity_exists(conn, &old_atom_canonical, old_atom_id)? {
        tracing::debug!(
            "Skipping supersedes edge {} -> {}: target entity no longer exists",
            atom_canonical,
            old_atom_canonical
        );
        return Ok(0);
    }
    let inserted = conn.execute(
        "INSERT OR IGNORE INTO relations (src_canonical, rel_type, dst_canonical, confidence, source_turn, relation_kind, created_at)
         VALUES (?1, 'supersedes', ?2, ?3, NULL, 'derived', ?4)",
        params![atom_canonical, old_atom_canonical, CONFIDENCE_SUPERSEDES, now],
    )?;
    Ok(u32::from(inserted > 0))
}

fn link_from_session_edge(
    conn: &Connection,
    atom_canonical: &str,
    session_id: Option<&str>,
    now: i64,
) -> Result<bool> {
    let Some(session_id) = session_id else {
        return Ok(false);
    };
    // W14: session 实体键用**无损**原始 id，不经 canonicalize——否则 "Sprint-1"
    // 与 "sprint-1"（两个不同会话，sessions.session_id 为 BINARY 主键）会折叠成
    // 同一节点，多跳/邻居检索跨会话混合记忆。读侧经 resolve_entity_canonical
    // 兼容按规范化名查询。
    let session_canonical = format!("session_{session_id}");
    conn.execute(
        "INSERT OR IGNORE INTO entities (canonical, name, entity_type, first_seen, last_seen)
         VALUES (?1, ?2, 'session', ?3, ?3)",
        params![session_canonical, session_id, now],
    )?;
    let inserted = conn.execute(
        "INSERT OR IGNORE INTO relations (src_canonical, rel_type, dst_canonical, confidence, source_turn, relation_kind, created_at)
         VALUES (?1, 'from_session', ?2, ?3, NULL, 'derived', ?4)",
        params![atom_canonical, session_canonical, CONFIDENCE_FROM_SESSION, now],
    )?;
    Ok(inserted > 0)
}

fn link_related_atom_edges(
    conn: &Connection,
    atom_canonical: &str,
    similar_atom_ids: &[i64],
    now: i64,
) -> Result<u32> {
    let mut created = 0u32;
    for &similar_id in similar_atom_ids {
        let similar_canonical = canonicalize(&format!("atom_{}", similar_id));
        if !ensure_atom_entity_exists(conn, &similar_canonical, similar_id)? {
            continue;
        }
        // 双向建边：related_to 是无向语义，两个方向都写才能从任一端遍历到。
        let inserted1 = conn.execute(
            "INSERT OR IGNORE INTO relations (src_canonical, rel_type, dst_canonical, confidence, source_turn, relation_kind, created_at)
             VALUES (?1, 'related_to', ?2, ?3, NULL, 'derived', ?4)",
            params![atom_canonical, similar_canonical, CONFIDENCE_RELATED_TO, now],
        )?;
        let inserted2 = conn.execute(
            "INSERT OR IGNORE INTO relations (src_canonical, rel_type, dst_canonical, confidence, source_turn, relation_kind, created_at)
             VALUES (?1, 'related_to', ?2, ?3, NULL, 'derived', ?4)",
            params![similar_canonical, atom_canonical, CONFIDENCE_RELATED_TO, now],
        )?;
        if inserted1 > 0 || inserted2 > 0 {
            created += 1;
        }
    }
    Ok(created)
}

pub(crate) fn integrate_atom_with_graph_on_conn(
    conn: &Connection,
    atom_id: i64,
    atom_content: &str,
    supersedes_id: Option<i64>,
    session_id: Option<&str>,
    extracted_entities: &[String],
    similar_atom_ids: &[i64],
) -> Result<GraphIntegrationResult> {
    let now = time::now_unix_ms();

    // 1. Create entity for this atom
    let atom_canonical = canonicalize(&format!("atom_{}", atom_id));
    conn.execute(
        "INSERT OR IGNORE INTO entities (canonical, name, entity_type, first_seen, last_seen, memory_atom_id)
         VALUES (?1, ?2, 'memory_atom', ?3, ?3, ?4)",
        params![atom_canonical, atom_content, now, atom_id],
    )?;

    let mentions_created =
        link_derived_mentions(conn, &atom_canonical, atom_id, extracted_entities, now)?;
    let supersedes_created = link_supersedes_edge(conn, &atom_canonical, supersedes_id, now)?;
    let from_session_created = link_from_session_edge(conn, &atom_canonical, session_id, now)?;
    let related_to_created = link_related_atom_edges(conn, &atom_canonical, similar_atom_ids, now)?;

    tracing::debug!(
        "Graph integration done for atom {}: mentions={} supersedes={} from_session={} related_to={}",
        atom_id,
        mentions_created,
        supersedes_created,
        from_session_created,
        related_to_created
    );

    Ok(GraphIntegrationResult {
        #[cfg(test)]
        mentions_created,
        #[cfg(test)]
        supersedes_created,
        #[cfg(test)]
        from_session_created,
        #[cfg(test)]
        related_to_created,
    })
}

/// Multi-hop query: find all atoms connected to a given entity
///
/// Traverses the graph up to `max_hops` hops from the starting entity,
/// collecting all memory atoms encountered along the way.
///
/// W5.4：触达 MAX_VISITED_NODES 时返回的是**部分**原子集，并 warn 明示
/// "结果可能不完整"。需要在响应体透出截断标志的调用方（transport）改用
/// [`multi_hop_query_with_truncation`]；本函数保持 `Vec<i64>` 签名兼容。
// A2: transport 改调 with_truncation 后，本兼容包装仅剩本模块自测调用方，
// 故 cfg(test) 门控（生产无调用方，无需 dead_code 抑制）。
#[cfg(test)]
pub fn multi_hop_query(
    db: &Db,
    start_entity: &str,
    max_hops: u32,
    relation_filter: Option<&str>,
) -> Result<Vec<i64>> {
    multi_hop_query_with_truncation(db, start_entity, max_hops, relation_filter)
        .map(|(ids, _truncated)| ids)
}

/// A2/W5.4：`multi_hop_query`（cfg(test) 兼容包装）的截断标志变体——返回 `(atom_ids, truncated)`。
/// `truncated == true` 表示遍历触达 MAX_VISITED_NODES 节点帽、原子集**不完整**
/// （服务端同时 warn）；transport 据此在 /search 响应体加 `truncated` 字段，
/// 不再只靠日志可感知。语义/排序/dedup 与 multi_hop_query 完全一致。
pub fn multi_hop_query_with_truncation(
    db: &Db,
    start_entity: &str,
    max_hops: u32,
    relation_filter: Option<&str>,
) -> Result<(Vec<i64>, bool)> {
    multi_hop_query_with_node_cap(
        db,
        start_entity,
        max_hops,
        relation_filter,
        MAX_VISITED_NODES,
    )
}

/// [`multi_hop_query_with_truncation`] 的可注入节点帽变体（同
/// `graph::query::path_with_max_results` 姿势）：生产恒走 MAX_VISITED_NODES，
/// 测试用小帽确定性地覆盖截断分支。
fn multi_hop_query_with_node_cap(
    db: &Db,
    start_entity: &str,
    max_hops: u32,
    relation_filter: Option<&str>,
    node_cap: usize,
) -> Result<(Vec<i64>, bool)> {
    validate_max_hops(max_hops)?;

    // M7 对称：写侧 rel_type 已 trim，关系过滤同样 trim（纯空白视同无过滤）。
    let relation_filter = relation_filter.map(str::trim).filter(|s| !s.is_empty());

    let start_canonical = crate::graph::query::resolve_entity_canonical(db.conn(), start_entity)?;
    if start_canonical.is_empty() {
        return Ok((Vec::new(), false));
    }

    let conn = db.conn();
    let mut walk = MultiHopWalk {
        visited: HashSet::new(),
        atom_ids: Vec::new(),
        seen_atoms: HashSet::new(),
        node_cap,
    };
    let mut current_frontier = vec![start_canonical.clone()];
    let mut truncated = false;

    for hop in 0..max_hops {
        if current_frontier.is_empty() {
            break;
        }
        if walk.visited.len() >= node_cap {
            truncated = true;
            break;
        }
        let expand_next = hop + 1 < max_hops;
        let (next_frontier, hit_cap) =
            walk.expand_hop(conn, &current_frontier, relation_filter, expand_next)?;
        if hit_cap {
            truncated = true;
            break;
        }
        current_frontier = next_frontier;
    }

    if truncated {
        // W5.4：静默 break 会让稠密图的多跳 /search 缺结果而调用方只见 "ok"。
        // 至少让日志侧可感知（诚实截断，同 path 的展开帽 warn 姿势）。
        tracing::warn!(
            "multi_hop 触达节点上限 node_cap={}（start={} hops={}）：结果可能不完整",
            node_cap,
            crate::util::truncate_for_log(&start_canonical),
            max_hops
        );
    }

    // Deduplicate atom IDs
    let mut atom_ids = walk.atom_ids;
    atom_ids.sort_unstable();
    atom_ids.dedup();

    Ok((atom_ids, truncated))
}

/// BFS 遍历累积态（拆出以压低 `multi_hop_query_with_node_cap` 认知复杂度）。
struct MultiHopWalk {
    visited: HashSet<String>,
    atom_ids: Vec<i64>,
    /// W25: 累积期去重——稠密图下同一 atom 被多条边重复返回，原实现逐行 push
    /// （1 万节点 × 千条边 ≈ 千万次 push / 数十 MB）后才 sort+dedup。
    seen_atoms: HashSet<i64>,
    node_cap: usize,
}

impl MultiHopWalk {
    /// 推进单跳：按 `SQL_IN_CHUNK_SIZE` 分批拉取邻接并吸收；返回 `(下一跳 frontier, 是否触达节点帽)`。
    fn expand_hop(
        &mut self,
        conn: &Connection,
        frontier: &[String],
        relation_filter: Option<&str>,
        expand_next: bool,
    ) -> Result<(Vec<String>, bool)> {
        let mut next_frontier = Vec::new();
        let mut seen_frontier = HashSet::new();

        for chunk in frontier.chunks(crate::index::sql_util::SQL_IN_CHUNK_SIZE) {
            let remaining = self.node_cap.saturating_sub(self.visited.len());
            if remaining == 0 {
                return Ok((next_frontier, true));
            }
            let slice = &chunk[..chunk.len().min(remaining)];
            let adj = query_neighbor_canonicals_batch(conn, slice, relation_filter)?;
            let next = expand_next.then_some((&mut next_frontier, &mut seen_frontier));
            // W16: 只吸收 slice——adj 只覆盖 slice，遍历整个 chunk 会把超出预算的
            // 节点登记进 visited 却因 adj.get() 为 None 永不展开，其原子记忆被静默
            // 丢弃且 truncated 仍为 false。slice 被截短本身即截断，必须上报。
            let shortened = slice.len() < chunk.len();
            if self.absorb_chunk(slice, &adj, next) || shortened {
                return Ok((next_frontier, true));
            }
        }
        // 帽恰好在本跳最后一个节点触达、且**仍有下一跳待展开**时才算截断。
        // next_frontier 为空说明该深度已无剩余工作，结果完整，不得误报 truncated。
        let truncated = self.visited.len() >= self.node_cap && !next_frontier.is_empty();
        Ok((next_frontier, truncated))
    }

    /// 吸收一个 frontier 批次：登记 visited 并委托 `absorb_neighbors`。返回 `true` = 触达节点帽（截断）。
    fn absorb_chunk(
        &mut self,
        chunk: &[String],
        adj: &GraphAdjacencyMap,
        mut next: Option<(&mut Vec<String>, &mut HashSet<String>)>,
    ) -> bool {
        for canonical in chunk {
            if self.visited.len() >= self.node_cap {
                return true;
            }
            if !self.visited.insert(canonical.clone()) {
                continue;
            }
            let Some(neighbors) = adj.get(canonical) else {
                continue;
            };
            self.absorb_neighbors(neighbors, next.as_mut().map(|(f, s)| (&mut **f, &mut **s)));
        }
        false
    }

    fn absorb_neighbors(
        &mut self,
        neighbors: &[(String, Option<i64>)],
        mut next: Option<(&mut Vec<String>, &mut HashSet<String>)>,
    ) {
        for (neighbor, atom_id) in neighbors {
            if let Some(id) = atom_id {
                if self.seen_atoms.insert(*id) {
                    self.atom_ids.push(*id);
                }
            }
            if let Some((frontier, seen)) = next.as_mut() {
                if !self.visited.contains(neighbor) && seen.insert(neighbor.clone()) {
                    frontier.push(neighbor.clone());
                }
            }
        }
    }
}

/// Validate max_hops to prevent DoS
///
/// 上限 10（而 neighbors 是 5）的理由见 graph/mod.rs 的"跳数上限"策略说明。
fn validate_max_hops(max_hops: u32) -> Result<()> {
    const MAX_HOPS_LIMIT: u32 = 10;
    if max_hops > MAX_HOPS_LIMIT {
        return Err(anyhow::anyhow!(
            "max_hops ({}) exceeds maximum allowed ({})",
            max_hops,
            MAX_HOPS_LIMIT
        ));
    }
    Ok(())
}

type GraphAdjacencyMap = std::collections::HashMap<String, Vec<(String, Option<i64>)>>;

/// W18: 按 `<= SQL_IN_CHUNK_SIZE` 批次拉取 frontier 节点的邻接实体及 `memory_atom_id`，
/// 消除 `multi_hop_query` 的每节点 N+1 SQL 往返并按 `(origin, neighbor)` 确定性排序。
fn query_neighbor_canonicals_batch(
    conn: &Connection,
    canonicals: &[String],
    relation_filter: Option<&str>,
) -> Result<GraphAdjacencyMap> {
    let mut map: GraphAdjacencyMap = std::collections::HashMap::with_capacity(canonicals.len());
    if canonicals.is_empty() {
        return Ok(map);
    }
    let placeholders = crate::index::sql_util::numbered_placeholders(canonicals.len());
    let rel_bind = canonicals.len() + 1;
    let sql = if relation_filter.is_some() {
        format!(
            "SELECT n.origin, n.neighbor, e.memory_atom_id
             FROM (
                 SELECT src_canonical AS origin, dst_canonical AS neighbor FROM relations
                 WHERE src_canonical IN ({placeholders}) AND rel_type = ?{rel_bind} AND relation_kind IN ('asserted', 'derived')
                 UNION
                 SELECT dst_canonical AS origin, src_canonical AS neighbor FROM relations
                 WHERE dst_canonical IN ({placeholders}) AND rel_type = ?{rel_bind} AND relation_kind IN ('asserted', 'derived')
             ) n
             LEFT JOIN entities e ON e.canonical = n.neighbor
             ORDER BY n.origin, n.neighbor"
        )
    } else {
        format!(
            "SELECT n.origin, n.neighbor, e.memory_atom_id
             FROM (
                 SELECT src_canonical AS origin, dst_canonical AS neighbor FROM relations
                 WHERE src_canonical IN ({placeholders}) AND relation_kind IN ('asserted', 'derived')
                 UNION
                 SELECT dst_canonical AS origin, src_canonical AS neighbor FROM relations
                 WHERE dst_canonical IN ({placeholders}) AND relation_kind IN ('asserted', 'derived')
             ) n
             LEFT JOIN entities e ON e.canonical = n.neighbor
             ORDER BY n.origin, n.neighbor"
        )
    };
    let mut params: Vec<&dyn rusqlite::ToSql> = canonicals
        .iter()
        .map(|s| s as &dyn rusqlite::ToSql)
        .collect();
    if let Some(ref rel) = relation_filter {
        params.push(rel);
    }
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params.as_slice(), |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<i64>>(2)?,
        ))
    })?;
    for row in rows {
        let (origin, neighbor, atom_id) = row?;
        map.entry(origin).or_default().push((neighbor, atom_id));
    }
    Ok(map)
}

#[cfg(test)]
#[path = "graph_integration_tests.rs"]
mod tests;

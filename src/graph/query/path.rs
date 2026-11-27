//! 图谱读路径：最短路径（无向有界 BFS）。
//! M14 自 query.rs 拆出；实体键解析仍在 query.rs。

use super::resolve_entity_canonical;
use crate::index::db::Db;
use serde::Serialize;
use std::collections::{HashMap, HashSet};

#[derive(Debug, Serialize)]
pub struct PathResult {
    pub found: bool,
    pub length: u32,
    pub path: Vec<PathStep>,
    /// W23: 展开帽命中导致搜索被截断。`found:false` 因此可能是"路径存在但没搜到"，
    /// 调用方必须能区分二者，否则把截断当成"不存在"的事实上报。
    pub truncated: bool,
}

/// 路径元素：实体节点和关系边交替出现
/// 序列：[Entity, Edge, Entity, Edge, ..., Entity]
/// 元素数 = 2*length + 1（**length ≥ 1 时**）；length=0 有两种情形：
/// src==dst（found=true，path 为空 Vec，不含起点实体）与未找到（found=false）。
#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum PathStep {
    Entity { canonical: String, name: String },
    Edge { rel_type: String },
}

/// 最短路径的跳数上限。为什么是 10 而 neighbors 是 5：见 mod.rs 的"跳数上限"
/// 策略说明（路径要在稀疏图里连通两端，半径换连通性，成本由展开帽兜住）。
const MAX_PATH_HOPS: u32 = 10;

/// Hard cap on node expansions during shortest-path BFS (J17).
const MAX_PATH_RESULTS: usize = 5000;

/// 在两节点间寻找最短路径（无向有界 BFS）。
///
/// 返回结构：`{found, length, path}`，其中 path 是 [Entity, Edge, Entity, Edge, ..., Entity]
/// 交替序列，共 `2 * length + 1` 个元素。src==dst 时 path 为空 Vec。
pub fn path(db: &Db, src: &str, dst: &str, max_hops: u32) -> anyhow::Result<PathResult> {
    path_with_max_results(db, src, dst, max_hops, MAX_PATH_RESULTS)
}

type AdjacencyMap = HashMap<String, Vec<(String, String)>>;

struct PathBfsState {
    dst: String,
    max_hops: u32,
    max_expansions: usize,
    expansions: usize,
    visited: HashSet<String>,
    parent: HashMap<String, (String, String)>,
}

impl PathBfsState {
    fn new(src: &str, dst: &str, max_hops: u32, max_expansions: usize) -> Self {
        let mut visited = HashSet::new();
        visited.insert(src.to_string());
        Self {
            dst: dst.to_string(),
            max_hops,
            max_expansions,
            expansions: 0,
            visited,
            parent: HashMap::new(),
        }
    }
}

fn append_incident_edges(
    conn: &rusqlite::Connection,
    sql: &str,
    chunk: &[String],
    out: &mut AdjacencyMap,
) -> anyhow::Result<()> {
    let mut stmt = conn.prepare(sql)?;
    let params: Vec<&dyn rusqlite::ToSql> =
        chunk.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
    let rows = stmt.query_map(params.as_slice(), |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
        ))
    })?;
    for row in rows {
        let (owner, neighbor, rel_type) = row?;
        out.entry(owner).or_default().push((neighbor, rel_type));
    }
    Ok(())
}

/// W17: 按 `<= SQL_IN_CHUNK_SIZE` 节点批次拉取出边/入边，保留与原单点
/// `ORDER BY rel_type, dst_canonical` + `ORDER BY rel_type, src_canonical` 一致的邻接顺序。
fn query_incident_edges_batch(
    conn: &rusqlite::Connection,
    nodes: &[String],
) -> anyhow::Result<AdjacencyMap> {
    let mut edges: AdjacencyMap = HashMap::with_capacity(nodes.len());
    if nodes.is_empty() {
        return Ok(edges);
    }
    let placeholders = crate::index::sql_util::numbered_placeholders(nodes.len());
    let out_sql = format!(
        "SELECT src_canonical, dst_canonical, rel_type FROM relations \
         WHERE src_canonical IN ({placeholders}) AND relation_kind IN ('asserted', 'derived') \
         ORDER BY src_canonical, rel_type, dst_canonical"
    );
    let in_sql = format!(
        "SELECT dst_canonical, src_canonical, rel_type FROM relations \
         WHERE dst_canonical IN ({placeholders}) AND relation_kind IN ('asserted', 'derived') \
         ORDER BY dst_canonical, rel_type, src_canonical"
    );
    append_incident_edges(conn, &out_sql, nodes, &mut edges)?;
    append_incident_edges(conn, &in_sql, nodes, &mut edges)?;
    Ok(edges)
}

enum LayerExpandOutcome {
    Found { length: u32 },
    Truncated,
    Continue(Vec<String>),
}

fn expand_path_layer(
    conn: &rusqlite::Connection,
    frontier: &[String],
    dist: u32,
    state: &mut PathBfsState,
) -> anyhow::Result<LayerExpandOutcome> {
    let collect_next = dist + 1 < state.max_hops;
    let mut next_frontier = Vec::new();
    for chunk in frontier.chunks(crate::index::sql_util::SQL_IN_CHUNK_SIZE) {
        if let Some(outcome) =
            expand_path_chunk(conn, chunk, dist, collect_next, state, &mut next_frontier)?
        {
            return Ok(outcome);
        }
    }
    Ok(LayerExpandOutcome::Continue(next_frontier))
}

fn expand_path_chunk(
    conn: &rusqlite::Connection,
    chunk: &[String],
    dist: u32,
    collect_next: bool,
    state: &mut PathBfsState,
    next_frontier: &mut Vec<String>,
) -> anyhow::Result<Option<LayerExpandOutcome>> {
    let remaining = state.max_expansions.saturating_sub(state.expansions);
    // W16: adj 只覆盖 slice，必须只遍历 slice——遍历整个 chunk 会把超出预算的节点
    // 当成"无邻接"跳过（adj.get() 为 None），其子图被静默丢弃且不报截断。
    let slice = &chunk[..chunk.len().min(remaining)];
    // M11c: slice 为空时 query_incident_edges_batch 自身早退返回空表，
    // 无需 remaining==0 的特判分支。
    let adj = query_incident_edges_batch(conn, slice)?;
    for node in slice {
        // M11c: slice 长度 ≤ remaining，循环内 expansions 恒 < max——
        // 旧 `expansions >= max` 中途截断分支不可达，删除；截断统一由
        // 循环后的 slice.len() < chunk.len() 判定（含 remaining==0 情形）。
        state.expansions += 1;
        let Some(edges) = adj.get(node) else {
            continue;
        };
        let next = collect_next.then_some(&mut *next_frontier);
        if visit_path_edges(node, edges, state, next) {
            return Ok(Some(LayerExpandOutcome::Found { length: dist + 1 }));
        }
    }
    if slice.len() < chunk.len() {
        tracing::warn!(
            "graph too dense, path results truncated at {} rows",
            state.max_expansions
        );
        return Ok(Some(LayerExpandOutcome::Truncated));
    }
    Ok(None)
}

/// 展开单个节点的出入边：首访邻居记 parent，命中 dst 返回 `true`；
/// `next_frontier` 为 `Some` 时收集下一层候选（末层传 `None`）。
fn visit_path_edges(
    node: &str,
    edges: &[(String, String)],
    state: &mut PathBfsState,
    mut next_frontier: Option<&mut Vec<String>>,
) -> bool {
    for (nbr, rel_type) in edges {
        if !state.visited.insert(nbr.clone()) {
            continue;
        }
        state
            .parent
            .insert(nbr.clone(), (node.to_string(), rel_type.clone()));
        if nbr == &state.dst {
            return true;
        }
        if let Some(next) = next_frontier.as_deref_mut() {
            next.push(nbr.clone());
        }
    }
    false
}

/// 沿 parent 链回溯出 [Entity, Edge, ..., Entity] 交替序列。
///
/// `Ok(None)` = parent 链断裂（cur 既不是 src 也没有父记录）。正常 BFS 下不可达
/// （每个入队节点都先写 parent），保留为防御分支：调用方必须据此降级为
/// found=false，而不是报 found=true 配一条空 path。
///
/// M8c: 节点名按 `IN (...)` 分批一次查回（不再逐节点点查），且 SQLite 错误
/// 上抛而非 `.unwrap_or_else` 吞掉——只有**行缺失**（实体并发改名/删除）才
/// 回退 canonical 字面；查询本身失败必须让调用方感知（500），不得伪装成
/// 正常路径。
pub(super) fn reconstruct_path(
    conn: &rusqlite::Connection,
    src: &str,
    dst: &str,
    parent: &HashMap<String, (String, String)>,
) -> anyhow::Result<Option<Vec<PathStep>>> {
    let mut hops: Vec<(String, String)> = Vec::new();
    let mut cur = dst.to_string();
    while cur != src {
        let Some((prev, rel)) = parent.get(&cur) else {
            return Ok(None);
        };
        hops.push((cur, rel.clone()));
        cur = prev.clone();
    }
    hops.reverse();

    let mut nodes: Vec<String> = Vec::with_capacity(hops.len() + 1);
    nodes.push(src.to_string());
    nodes.extend(hops.iter().map(|(node, _)| node.clone()));
    let names = lookup_entity_names(conn, &nodes)?;
    let name_of = |canonical: &str| -> String {
        names
            .get(canonical)
            .cloned()
            .unwrap_or_else(|| canonical.to_string())
    };

    let mut out = Vec::with_capacity(hops.len() * 2 + 1);
    out.push(PathStep::Entity {
        canonical: src.to_string(),
        name: name_of(src),
    });
    for (node, rel_type) in hops {
        out.push(PathStep::Edge { rel_type });
        let name = name_of(&node);
        out.push(PathStep::Entity {
            canonical: node,
            name,
        });
    }
    Ok(Some(out))
}

/// M8c: 分批（`<= SQL_IN_CHUNK_SIZE`）查回 canonical → name。缺失行不入表
/// （调用方回退 canonical）；任何 SQLite 错误上抛。
fn lookup_entity_names(
    conn: &rusqlite::Connection,
    nodes: &[String],
) -> anyhow::Result<HashMap<String, String>> {
    let mut names = HashMap::with_capacity(nodes.len());
    crate::index::sql_util::for_each_in_chunk(
        nodes,
        |chunk, placeholders| -> anyhow::Result<()> {
            let sql =
                format!("SELECT canonical, name FROM entities WHERE canonical IN ({placeholders})");
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(rusqlite::params_from_iter(chunk.iter()), |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?;
            for row in rows {
                let (canonical, name) = row?;
                names.insert(canonical, name);
            }
            Ok(())
        },
    )?;
    Ok(names)
}

/// `path` 的可扩展上界变体：使用内存 `visited` + `parent` Map 执行有界 BFS，
/// 消除指数级递归 CTE 膨胀。触达 `max_results` 次节点展开时输出警告并返回。
pub fn path_with_max_results(
    db: &Db,
    src: &str,
    dst: &str,
    max_hops: u32,
    max_results: usize,
) -> anyhow::Result<PathResult> {
    if !(1..=MAX_PATH_HOPS).contains(&max_hops) {
        anyhow::bail!(
            "max_hops must be in 1..={}, got {}",
            MAX_PATH_HOPS,
            max_hops
        );
    }
    let max_expansions = max_results.max(1);
    let src_c = resolve_entity_canonical(db.conn(), src)?;
    let dst_c = resolve_entity_canonical(db.conn(), dst)?;

    if src_c.is_empty() || dst_c.is_empty() {
        return Ok(PathResult {
            found: false,
            length: 0,
            path: Vec::new(),
            truncated: false,
        });
    }

    if src_c == dst_c {
        return Ok(PathResult {
            found: true,
            length: 0,
            path: Vec::new(),
            truncated: false,
        });
    }

    let conn = db.conn();
    let mut state = PathBfsState::new(&src_c, &dst_c, max_hops, max_expansions);
    let mut frontier: Vec<String> = vec![src_c.clone()];
    let mut truncated = false;

    for dist in 0..max_hops {
        if frontier.is_empty() {
            break;
        }
        match expand_path_layer(conn, &frontier, dist, &mut state)? {
            LayerExpandOutcome::Found { length } => {
                // 防御（正常 BFS 不可达）：parent 链断裂时不得报 found=true 配空
                // path —— 降级为 not-found 并 warn，让上层可感知。
                let Some(path) = reconstruct_path(conn, &src_c, &dst_c, &state.parent)? else {
                    tracing::warn!(
                        "graph path parent chain broken ({} -> {}); reporting not-found",
                        src_c,
                        dst_c
                    );
                    return Ok(PathResult {
                        found: false,
                        length: 0,
                        path: Vec::new(),
                        truncated: false,
                    });
                };
                return Ok(PathResult {
                    found: true,
                    length,
                    path,
                    truncated: false,
                });
            }
            LayerExpandOutcome::Truncated => {
                truncated = true;
                break;
            }
            LayerExpandOutcome::Continue(next) => {
                frontier = next;
            }
        }
    }

    Ok(PathResult {
        found: false,
        length: 0,
        path: Vec::new(),
        truncated,
    })
}

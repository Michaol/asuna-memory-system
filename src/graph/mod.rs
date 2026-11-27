//! 图谱记忆层 (Graph Memory Layer)
//!
//! 三层正交架构中的第三层。事实层（SQLite sessions/turns）和成长层（Markdown）一字不动。
//! 图谱层复用同一个 SQLite 数据库，新增 `entities` + `relations` 两张表。
//!
//! 作者分层（v1.3.0 的"agent 是图谱唯一作者"已不准确）：
//! - **断言层**（`relation_kind='asserted'`，store.rs）：agent 通过 graph_assert /
//!   link_entity 显式写入。server 不调 LLM、不做规则抽取、不做 fuzzy 语义合并。
//! - **派生层**（`relation_kind='derived'`，memory/graph_integration.rs）：server 在
//!   L1 原子入库时自动建边（mentions / supersedes / from_session / related_to），
//!   实体名取自 L1 抽取结果——这是唯一由 server 侧产生的图谱内容。
//!
//! 读路径可见性策略（neighbors / path / multi_hop_query 三处统一）：只遍历
//! `relation_kind IN ('asserted', 'derived')` 的边。当前 schema 只有这两种 kind
//! （列默认 'asserted'），故白名单等价于"全部边"；新增 kind 时必须同步三处 SQL
//! （query/neighbors.rs 的 query_adjacent_batch、query/path.rs 的
//! query_incident_edges_batch 与 graph_integration.rs 的 query_neighbor_canonicals），
//! 否则读路径静默漂移。
//!
//! 跳数上限（三套语义不同，各自的结果帽兜住成本）：
//! - query::MAX_HOPS = 5（/graph/neighbors）：邻居是"上下文扩散"面，结果本身被
//!   limit（≤200）截断，5 跳已远超实用半径，再深只增开销不增信息。
//! - query::MAX_PATH_HOPS = 10（/graph/path）：最短路径要在稀疏图里连通两端，需要
//!   更大搜索半径；只返回一条路径，成本由 MAX_PATH_RESULTS=5000 展开帽兜住。
//! - graph_integration 的 MAX_HOPS_LIMIT = 10（/search 多跳召回）：深度换原子召回，
//!   成本由 MAX_VISITED_NODES=10_000 兜住，触达即 warn 明示结果可能不完整（W5.4）；
//!   截断标志经 multi_hop_query_with_truncation 透出供响应体消费（A2）。
//!
//! `entities.last_seen` 目前是 dormant 列（全仓只写不读）：断言层 upsert 刷新它
//! （store.rs），派生层 INSERT OR IGNORE 不刷新——不为无读者的列增加热路径 UPDATE。

pub mod canonical;
pub mod query;
pub mod store;

pub use query::{path, pending_turn_ids, NeighborQuery};
pub use store::{assert_triples, link_entity, prune_dangling_refs, TripleInput};

#[cfg(test)]
mod tests;

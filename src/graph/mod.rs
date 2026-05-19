//! 图谱记忆层 (Graph Memory Layer)
//!
//! 三层正交架构中的第三层。事实层（SQLite sessions/turns）和成长层（Markdown）一字不动。
//! 图谱层复用同一个 SQLite 数据库，新增 `entities` + `relations` 两张表。
//!
//! agent 是图谱内容的唯一作者；server 不调 LLM 也不做规则抽取。

pub mod canonical;
pub mod query;
pub mod store;

pub use canonical::canonicalize;
pub use query::{neighbors, path, pending_turn_ids, NeighborQuery};
pub use store::{assert_triples, link_entity, TripleInput};

#[cfg(test)]
mod tests;

//! 图谱记忆层 (Graph Memory Layer)
//!
//! 三层正交架构中的第三层。事实层（SQLite sessions/turns）和成长层（Markdown）一字不动。
//! 图谱层复用同一个 SQLite 数据库，新增 `entities` + `relations` 两张表。
//!
//! agent 是图谱内容的唯一作者；server 不调 LLM 也不做规则抽取。

pub mod canonical;
#[allow(dead_code)] // TODO(P4): MCP tools consume these
pub mod query;
#[allow(dead_code)] // TODO(P4): MCP tools consume these
pub mod store;

#[allow(unused_imports)] // TODO(P4): MCP tools import these
pub use canonical::canonicalize;
#[allow(unused_imports)] // TODO(P4): MCP tools import these
pub use query::{neighbors, path, Direction, Neighbor, NeighborQuery, PathResult, PathStep};
#[allow(unused_imports)] // TODO(P4): MCP tools import these
pub use store::{assert_triples, AssertStats, TripleInput};

#[cfg(test)]
mod tests;

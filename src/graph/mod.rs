//! 图谱记忆层 (Graph Memory Layer)
//!
//! 三层正交架构中的第三层。事实层（SQLite sessions/turns）和成长层（Markdown）一字不动。
//! 图谱层复用同一个 SQLite 数据库，新增 `entities` + `relations` 两张表。
//!
//! agent 是图谱内容的唯一作者；server 不调 LLM 也不做规则抽取。

pub mod canonical;

// TODO(task-2.2): 移除 #[allow(dead_code)]，当 MCP graph_assert 工具调用 store::assert_triples 时
#[allow(dead_code)]
pub mod store;

// TODO(task-2.2): 移除 #[allow(unused_imports)]，当 MCP 层通过 crate::graph::{...} 引用时
#[allow(unused_imports)]
pub use canonical::canonicalize;
#[allow(unused_imports)]
pub use store::{assert_triples, AssertStats, TripleInput};

#[cfg(test)]
mod tests;

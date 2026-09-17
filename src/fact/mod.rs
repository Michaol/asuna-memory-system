// conversation 已迁至 index/（J37：JSONL 是存储格式，属存储层；消除
// index→fact 依赖边）。保留兼容再导出，crate::fact::conversation::… 路径不变。
pub use crate::index::conversation;
pub mod search;
pub mod session_store;

#[cfg(test)]
mod bench_test;
#[cfg(test)]
mod e2e_test;
#[cfg(test)]
mod overwrite_test;

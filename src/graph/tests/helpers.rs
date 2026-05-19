//! Shared test helpers for graph integration tests.

use crate::graph::TripleInput;
use crate::index::db::Db;

pub fn fresh_db() -> Db {
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();
    db
}

pub fn t(src: &str, rel: &str, dst: &str) -> TripleInput {
    TripleInput {
        src: src.to_string(),
        rel: rel.to_string(),
        dst: dst.to_string(),
        src_type: None,
        dst_type: None,
        confidence: None,
        source_turn: None,
    }
}

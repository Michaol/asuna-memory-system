//! Graph layer integration tests.
//!
//! Split by sub-module:
//! - `helpers` — shared `fresh_db()` / `t()` builders
//! - `store_tests` — assert_triples + link_entity + FK / CASCADE schema invariants
//! - `query_tests` — neighbors + path + pending_turn_ids

mod helpers;
mod query_tests;
mod store_tests;

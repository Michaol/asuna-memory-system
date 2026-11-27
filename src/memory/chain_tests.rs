use super::*;
use crate::index::db::Db;

fn setup() -> Db {
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();
    db
}

#[test]
fn test_get_chain_single_entry() {
    let db = setup();
    let now = crate::util::time::now_unix_ms();

    db.conn()
        .execute(
            "INSERT INTO bounded_memory (target, content, created_at, updated_at, confidence)
             VALUES ('memory', 'entry A', ?1, ?1, 'high')",
            rusqlite::params![now],
        )
        .unwrap();

    let id = db.conn().last_insert_rowid();
    let chain = get_chain(&db, id).unwrap();

    assert_eq!(chain.len(), 1);
    assert_eq!(chain[0].content, "entry A");
    assert!(chain[0].supersedes_id.is_none());
}

#[test]
fn test_get_chain_with_supersedes() {
    let db = setup();
    let now = crate::util::time::now_unix_ms();

    // Create v1
    db.conn()
        .execute(
            "INSERT INTO bounded_memory (target, content, created_at, updated_at, confidence)
             VALUES ('memory', 'I like Rust', ?1, ?1, 'high')",
            rusqlite::params![now],
        )
        .unwrap();
    let v1_id = db.conn().last_insert_rowid();

    // Create v2 that supersedes v1
    db.conn()
        .execute(
            "INSERT INTO bounded_memory
             (target, content, created_at, updated_at, confidence, memory_type, supersedes_id)
             VALUES ('memory', 'I prefer Python', ?1, ?1, 'high', 'atom', ?2)",
            rusqlite::params![now + 1000, v1_id],
        )
        .unwrap();
    let v2_id = db.conn().last_insert_rowid();

    // Chain from v2 should include both
    let chain = get_chain(&db, v2_id).unwrap();
    assert_eq!(chain.len(), 2);
    assert_eq!(chain[0].content, "I prefer Python");
    assert_eq!(chain[1].content, "I like Rust");
}

#[test]
fn test_get_latest_version() {
    let db = setup();
    let now = crate::util::time::now_unix_ms();

    // Create v1
    db.conn()
        .execute(
            "INSERT INTO bounded_memory (target, content, created_at, updated_at, confidence)
             VALUES ('memory', 'v1', ?1, ?1, 'high')",
            rusqlite::params![now],
        )
        .unwrap();
    let v1_id = db.conn().last_insert_rowid();

    // Create v2 supersedes v1
    db.conn()
        .execute(
            "INSERT INTO bounded_memory
             (target, content, created_at, updated_at, confidence, supersedes_id)
             VALUES ('memory', 'v2', ?1, ?1, 'high', ?2)",
            rusqlite::params![now + 1000, v1_id],
        )
        .unwrap();
    let v2_id = db.conn().last_insert_rowid();

    // Create v3 supersedes v2
    db.conn()
        .execute(
            "INSERT INTO bounded_memory
             (target, content, created_at, updated_at, confidence, supersedes_id)
             VALUES ('memory', 'v3', ?1, ?1, 'high', ?2)",
            rusqlite::params![now + 2000, v2_id],
        )
        .unwrap();
    let v3_id = db.conn().last_insert_rowid();

    // From v1, latest should be v3
    assert_eq!(get_latest_version(&db, v1_id).unwrap(), v3_id);
    // From v3, latest is itself
    assert_eq!(get_latest_version(&db, v3_id).unwrap(), v3_id);
}

#[test]
fn test_create_superseding() {
    let db = setup();
    let now = crate::util::time::now_unix_ms();

    db.conn()
        .execute(
            "INSERT INTO bounded_memory (target, content, created_at, updated_at, confidence)
             VALUES ('memory', 'old fact', ?1, ?1, 'high')",
            rusqlite::params![now],
        )
        .unwrap();
    let old_id = db.conn().last_insert_rowid();

    let new_id = create_superseding(
        &db,
        "memory",
        "new fact",
        "atom",
        0.9,
        Some("[1,2,3]"),
        old_id,
    )
    .unwrap();

    let chain = get_chain(&db, new_id).unwrap();
    assert_eq!(chain.len(), 2);
    assert_eq!(chain[0].content, "new fact");
    assert_eq!(chain[0].memory_type, "atom");
    assert_eq!(chain[1].content, "old fact");

    // C14-a: create_superseding persists the REAL score into
    // confidence_score (was schema-default 1.0 before).
    let score: f64 = db
        .conn()
        .query_row(
            "SELECT confidence_score FROM bounded_memory WHERE id = ?1",
            rusqlite::params![new_id],
            |r| r.get(0),
        )
        .unwrap();
    assert!((score - 0.9).abs() < 1e-9);
}

/// C14-a: get_chain reads the persisted REAL confidence_score column,
/// not the TEXT bucket re-derived via CASE. 0.42 buckets 'medium' (CASE
/// would yield 0.5) — the chain entry must carry 0.42.
#[test]
fn test_get_chain_reads_real_confidence_score_column() {
    let db = setup();
    let now = crate::util::time::now_unix_ms();
    db.conn()
        .execute(
            "INSERT INTO bounded_memory (target, content, created_at, updated_at, confidence)
             VALUES ('memory', 'old fact', ?1, ?1, 'high')",
            rusqlite::params![now],
        )
        .unwrap();
    let old_id = db.conn().last_insert_rowid();
    let new_id = create_superseding(&db, "memory", "new fact", "atom", 0.42, None, old_id).unwrap();

    let chain = get_chain(&db, new_id).unwrap();
    assert_eq!(chain.len(), 2);
    assert!(
        (chain[0].confidence_score - 0.42).abs() < 1e-9,
        "real column (0.42), not the 'medium' bucket (0.5): {}",
        chain[0].confidence_score
    );
}

/// A dangling supersedes pointer (parent hard-deleted) truncates the
/// chain with a warn instead of failing the whole read via `?`. FK checks
/// are toggled off only to manufacture the dangling shape (FK ON would
/// reject the delete); production reads never toggle them.
#[test]
fn test_get_chain_truncates_at_dangling_supersedes() {
    let db = setup();
    let now = crate::util::time::now_unix_ms();
    db.conn()
        .execute(
            "INSERT INTO bounded_memory (target, content, created_at, updated_at, confidence)
             VALUES ('memory', 'v1', ?1, ?1, 'high')",
            rusqlite::params![now],
        )
        .unwrap();
    let v1_id = db.conn().last_insert_rowid();
    db.conn()
        .execute(
            "INSERT INTO bounded_memory
             (target, content, created_at, updated_at, confidence, supersedes_id)
             VALUES ('memory', 'v2', ?1, ?1, 'high', ?2)",
            rusqlite::params![now + 1000, v1_id],
        )
        .unwrap();
    let v2_id = db.conn().last_insert_rowid();

    db.conn()
        .execute_batch("PRAGMA foreign_keys = OFF;")
        .unwrap();
    db.conn()
        .execute(
            "DELETE FROM bounded_memory WHERE id = ?1",
            rusqlite::params![v1_id],
        )
        .unwrap();
    db.conn()
        .execute_batch("PRAGMA foreign_keys = ON;")
        .unwrap();

    let chain = get_chain(&db, v2_id).unwrap();
    assert_eq!(chain.len(), 1, "chain truncates at the missing parent");
    assert_eq!(chain[0].content, "v2");

    // A wholly missing entry id yields an empty chain, not an error.
    assert!(get_chain(&db, 999_999).unwrap().is_empty());
}

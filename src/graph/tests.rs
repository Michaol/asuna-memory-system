//! Integration tests for the graph layer.

use crate::graph::{assert_triples, TripleInput};
use crate::index::db::Db;

fn fresh_db() -> Db {
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();
    db
}

fn t(src: &str, rel: &str, dst: &str) -> TripleInput {
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

#[test]
fn test_assert_basic_triples() {
    let db = fresh_db();
    let triples = vec![
        TripleInput {
            src: "Alice".to_string(),
            rel: "works_at".to_string(),
            dst: "OpenAI".to_string(),
            src_type: Some("person".to_string()),
            dst_type: Some("org".to_string()),
            confidence: Some(0.9),
            source_turn: Some(42),
        },
        t("Alice", "friend_of", "Bob"),
    ];
    let stats = assert_triples(&db, &triples).unwrap();
    assert_eq!(stats.entities_created, 3); // Alice, OpenAI, Bob
    assert_eq!(stats.entities_updated, 1); // Alice (second triple)
    assert_eq!(stats.relations_created, 2);
}

#[test]
fn test_assert_dedup_same_triple_canonical_insensitive() {
    let db = fresh_db();
    assert_triples(&db, &[t("Alice", "works_at", "OpenAI")]).unwrap();
    let triples = vec![TripleInput {
        src: "alice".to_string(),
        rel: "works_at".to_string(),
        dst: "openai".to_string(),
        src_type: None,
        dst_type: None,
        confidence: Some(0.95),
        source_turn: None,
    }];
    let stats = assert_triples(&db, &triples).unwrap();
    assert_eq!(stats.entities_created, 0);
    assert_eq!(stats.relations_created, 0);
    assert_eq!(stats.relations_updated, 1);

    let conf: f64 = db
        .conn()
        .query_row(
            "SELECT confidence FROM relations WHERE src_canonical='alice' AND rel_type='works_at' AND dst_canonical='openai'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!((conf - 0.95).abs() < 1e-6, "confidence should be MAX, got {}", conf);
}

#[test]
fn test_assert_empty_rejected() {
    let db = fresh_db();
    assert!(assert_triples(&db, &[]).is_err());
}

#[test]
fn test_assert_invalid_confidence_rejected() {
    let db = fresh_db();
    let mut bad = t("a", "r", "b");
    bad.confidence = Some(1.5);
    assert!(assert_triples(&db, &[bad]).is_err());

    let mut bad2 = t("a", "r", "b");
    bad2.confidence = Some(-0.1);
    assert!(assert_triples(&db, &[bad2]).is_err());
}

#[test]
fn test_assert_invalid_empty_field_rejected() {
    let db = fresh_db();
    assert!(assert_triples(&db, &[t("", "r", "b")]).is_err());
    assert!(assert_triples(&db, &[t("a", "", "b")]).is_err());
    assert!(assert_triples(&db, &[t("a", "r", "")]).is_err());
}

#[test]
fn test_assert_lower_confidence_does_not_decrease() {
    let db = fresh_db();
    let mut high = t("a", "r", "b");
    high.confidence = Some(0.9);
    assert_triples(&db, &[high]).unwrap();

    let mut low = t("a", "r", "b");
    low.confidence = Some(0.2);
    let stats = assert_triples(&db, &[low]).unwrap();
    assert_eq!(stats.relations_updated, 1);

    let conf: f64 = db.conn().query_row(
        "SELECT confidence FROM relations WHERE src_canonical='a'",
        [],
        |r| r.get(0),
    ).unwrap();
    assert!((conf - 0.9).abs() < 1e-6, "confidence should remain MAX=0.9, got {}", conf);
}

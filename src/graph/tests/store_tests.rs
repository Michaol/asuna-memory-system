//! Tests for the graph store layer: assert_triples, link_entity, FK / CASCADE pinning.

use super::helpers::{fresh_db, t};
use crate::graph::store::link_entity;
use crate::graph::{assert_triples, TripleInput};

// ─────────────────────────────────────────────
// assert_triples
// ─────────────────────────────────────────────

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

    let conf: f64 = db
        .conn()
        .query_row(
            "SELECT confidence FROM relations WHERE src_canonical='a'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!((conf - 0.9).abs() < 1e-6, "confidence should remain MAX=0.9, got {}", conf);
}

#[test]
fn test_assert_self_loop_counts_entity_once() {
    let db = fresh_db();
    let stats = assert_triples(&db, &[t("Alice", "knows", "Alice")]).unwrap();
    assert_eq!(stats.entities_created, 1);
    assert_eq!(stats.entities_updated, 0);
    assert_eq!(stats.relations_created, 1);
}

#[test]
fn test_assert_performance_10_triples() {
    let db = fresh_db();
    let triples: Vec<TripleInput> = (0..10)
        .map(|i| TripleInput {
            src: format!("entity_{}", i),
            rel: "rel_test".to_string(),
            dst: format!("entity_{}", i + 100),
            src_type: None,
            dst_type: None,
            confidence: None,
            source_turn: Some(i as i64),
        })
        .collect();

    let start = std::time::Instant::now();
    assert_triples(&db, &triples).unwrap();
    let elapsed = start.elapsed();

    println!("10 triples write: {:?}", elapsed);
    // 预算：10ms。硬失败阈值 20ms（实测 ~0.7ms，留 ~25× 余量足以吸收 CI 噪声 + 真实回归）。
    assert!(
        elapsed.as_millis() < 20,
        "10 triples took {:?}, over 10ms budget",
        elapsed
    );
}

// ─────────────────────────────────────────────
// FK enforcement / CASCADE
// ─────────────────────────────────────────────

#[test]
fn test_fk_enforcement_on_memory_db() {
    // PRAGMA foreign_keys = ON 必须在 :memory: 数据库上也生效。
    // 否则 P4 graph_link_entity 依赖的 ON DELETE CASCADE 会静默失败。
    let db = fresh_db();
    let err = db
        .conn()
        .execute(
            "INSERT INTO relations (src_canonical, rel_type, dst_canonical, confidence, created_at)
             VALUES ('ghost1', 'r', 'ghost2', 0.5, 0)",
            [],
        )
        .unwrap_err();
    let msg = format!("{}", err).to_lowercase();
    assert!(
        msg.contains("foreign key"),
        "expected foreign key error, got: {}",
        err
    );
}

#[test]
fn test_cascade_deletes_relations() {
    // 验证 ON DELETE CASCADE 真实生效——这是 P4 link_entity 的基础设施假设。
    let db = fresh_db();
    assert_triples(
        &db,
        &[
            t("Alice", "works_at", "OpenAI"),
            t("Alice", "friend_of", "Bob"),
        ],
    )
    .unwrap();

    // 关系数应为 2
    let n: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM relations", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 2);

    // 删除 alice 实体，CASCADE 应清理所有相关关系
    db.conn()
        .execute("DELETE FROM entities WHERE canonical = 'alice'", [])
        .unwrap();

    let n_after: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM relations", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n_after, 0, "CASCADE 应当清理所有指向/源自 alice 的关系");
}

// ─────────────────────────────────────────────
// link_entity
// ─────────────────────────────────────────────

#[test]
fn test_link_entity_rewires_outgoing() {
    let db = fresh_db();
    assert_triples(
        &db,
        &[
            t("Alice", "works_at", "OpenAI"),
            t("Alice", "knows", "Bob"),
        ],
    )
    .unwrap();

    let rewired = link_entity(&db, "Alice", "Alice Smith").unwrap();
    assert!(
        rewired >= 2,
        "should rewire at least 2 edges, got {}",
        rewired
    );

    // alice gone
    let n: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM entities WHERE canonical='alice'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 0);

    // alice smith has 2 outgoing edges
    let n: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM relations WHERE src_canonical='alice smith'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 2);
}

#[test]
fn test_link_entity_rewires_incoming() {
    let db = fresh_db();
    assert_triples(&db, &[t("Bob", "knows", "Alice")]).unwrap();
    link_entity(&db, "Alice", "Alice Smith").unwrap();

    let n: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM relations WHERE dst_canonical='alice smith'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 1);
}

#[test]
fn test_link_entity_merges_duplicates() {
    let db = fresh_db();
    // Both alice and "alice smith" point to OpenAI
    assert_triples(
        &db,
        &[
            t("Alice", "works_at", "OpenAI"),
            t("Alice Smith", "works_at", "OpenAI"),
        ],
    )
    .unwrap();
    link_entity(&db, "Alice", "Alice Smith").unwrap();

    // Only ONE edge should remain
    let n: i64 = db
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM relations WHERE src_canonical='alice smith' AND dst_canonical='openai'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 1);
}

#[test]
fn test_link_entity_same_canonical_rejected() {
    let db = fresh_db();
    assert!(link_entity(&db, "Alice", "alice").is_err());
}

#[test]
fn test_link_entity_creates_target_if_missing() {
    // Target doesn't exist yet — link should create it before rewiring
    let db = fresh_db();
    assert_triples(&db, &[t("Alice", "knows", "Bob")]).unwrap();

    link_entity(&db, "Alice", "Alice Smith").unwrap();

    // alice smith should now exist
    let n: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM entities WHERE canonical='alice smith'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 1);
}

#[test]
fn test_link_entity_missing_from_is_noop() {
    // from 不存在时静默 no-op，返回 0
    let db = fresh_db();
    let n = link_entity(&db, "Ghost", "Real").unwrap();
    assert_eq!(n, 0);

    // Real should still have been created (target auto-creation always happens)
    let count: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM entities WHERE canonical='real'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 1);
}

//! Tests for the graph query layer: neighbors, path, pending_turn_ids.

use super::helpers::{fresh_db, t};
use crate::graph::query::{neighbors, path, pending_turn_ids, Direction, NeighborQuery, PathStep};
use crate::graph::{assert_triples, TripleInput};

// ─────────────────────────────────────────────
// neighbors
// ─────────────────────────────────────────────

#[test]
fn test_neighbors_1hop_out() {
    let db = fresh_db();
    assert_triples(
        &db,
        &[
            t("Alice", "works_at", "OpenAI"),
            t("Alice", "friend_of", "Bob"),
        ],
    )
    .unwrap();

    let q = NeighborQuery {
        entity: "Alice".to_string(),
        rel_type: None,
        direction: Direction::Out,
        hops: 1,
        limit: 50,
    };
    let result = neighbors(&db, &q).unwrap();
    let canonicals: Vec<_> = result.iter().map(|n| n.canonical.as_str()).collect();
    assert!(canonicals.contains(&"openai"));
    assert!(canonicals.contains(&"bob"));
    assert_eq!(result.len(), 2);
    for n in &result {
        assert_eq!(n.distance, 1);
    }
}

#[test]
fn test_neighbors_filtered_by_rel_type() {
    let db = fresh_db();
    assert_triples(
        &db,
        &[
            t("Alice", "works_at", "OpenAI"),
            t("Alice", "friend_of", "Bob"),
        ],
    )
    .unwrap();

    let q = NeighborQuery {
        entity: "Alice".to_string(),
        rel_type: Some("works_at".to_string()),
        direction: Direction::Out,
        hops: 1,
        limit: 50,
    };
    let result = neighbors(&db, &q).unwrap();
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].canonical, "openai");
}

#[test]
fn test_neighbors_direction_in() {
    let db = fresh_db();
    assert_triples(&db, &[t("Alice", "works_at", "OpenAI")]).unwrap();
    let q = NeighborQuery {
        entity: "OpenAI".to_string(),
        rel_type: None,
        direction: Direction::In,
        hops: 1,
        limit: 50,
    };
    let result = neighbors(&db, &q).unwrap();
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].canonical, "alice");
}

#[test]
fn test_neighbors_direction_both() {
    let db = fresh_db();
    assert_triples(
        &db,
        &[t("Alice", "knows", "Bob"), t("Carol", "knows", "Alice")],
    )
    .unwrap();
    let q = NeighborQuery {
        entity: "Alice".to_string(),
        rel_type: None,
        direction: Direction::Both,
        hops: 1,
        limit: 50,
    };
    let result = neighbors(&db, &q).unwrap();
    let canonicals: Vec<_> = result.iter().map(|n| n.canonical.as_str()).collect();
    assert!(canonicals.contains(&"bob"));
    assert!(canonicals.contains(&"carol"));
    assert_eq!(
        result.len(),
        2,
        "Both direction should return exactly Bob + Carol"
    );
}

#[test]
fn test_neighbors_2hop() {
    let db = fresh_db();
    assert_triples(
        &db,
        &[t("Alice", "knows", "Bob"), t("Bob", "knows", "Carol")],
    )
    .unwrap();
    let q = NeighborQuery {
        entity: "Alice".to_string(),
        rel_type: None,
        direction: Direction::Out,
        hops: 2,
        limit: 50,
    };
    let result = neighbors(&db, &q).unwrap();
    let canonicals: Vec<_> = result.iter().map(|n| n.canonical.as_str()).collect();
    assert!(canonicals.contains(&"bob"));
    assert!(canonicals.contains(&"carol"));

    // Verify distances: bob is 1-hop, carol is 2-hop
    let bob = result.iter().find(|n| n.canonical == "bob").unwrap();
    let carol = result.iter().find(|n| n.canonical == "carol").unwrap();
    assert_eq!(bob.distance, 1);
    assert_eq!(carol.distance, 2);
}

#[test]
fn test_neighbors_invalid_hops() {
    let db = fresh_db();

    // hops = 0 should reject
    let q_zero = NeighborQuery {
        entity: "Alice".to_string(),
        rel_type: None,
        direction: Direction::Out,
        hops: 0,
        limit: 50,
    };
    assert!(neighbors(&db, &q_zero).is_err());

    // hops = 6 (above MAX_HOPS=5) should reject
    let q_high = NeighborQuery {
        entity: "Alice".to_string(),
        rel_type: None,
        direction: Direction::Out,
        hops: 6,
        limit: 50,
    };
    assert!(neighbors(&db, &q_high).is_err());
}

#[test]
fn test_neighbors_empty_entity() {
    // canonical 化为空时应返回空 Vec，不报错
    let db = fresh_db();
    let q = NeighborQuery {
        entity: "   ".to_string(),
        rel_type: None,
        direction: Direction::Out,
        hops: 1,
        limit: 50,
    };
    let result = neighbors(&db, &q).unwrap();
    assert!(result.is_empty());
}

#[test]
fn test_neighbors_cycle_terminates() {
    // 环形图：A→B→A，2 hops 必须终止不死循环（递归 CTE 用 UNION 而非 UNION ALL）
    let db = fresh_db();
    assert_triples(&db, &[t("A", "r", "B"), t("B", "r", "A")]).unwrap();
    let q = NeighborQuery {
        entity: "A".to_string(),
        rel_type: None,
        direction: Direction::Out,
        hops: 3,
        limit: 50,
    };
    let result = neighbors(&db, &q).unwrap(); // must terminate
    // A→B (hop 1)，B→A (hop 2)；A 是 seed 被排除，所以 result 只有 B
    let canonicals: Vec<_> = result.iter().map(|n| n.canonical.as_str()).collect();
    assert!(canonicals.contains(&"b"));
    // Seed itself must not appear in results even though the cycle revisits it
    assert!(
        !canonicals.contains(&"a"),
        "seed must be excluded from neighbors"
    );
}

// ─────────────────────────────────────────────
// path
// ─────────────────────────────────────────────

#[test]
fn test_path_direct() {
    let db = fresh_db();
    assert_triples(&db, &[t("Alice", "knows", "Bob")]).unwrap();
    let p = path(&db, "Alice", "Bob", 5).unwrap();
    assert!(p.found);
    assert_eq!(p.length, 1);
    // 路径内容：[Alice, knows, Bob]
    assert_eq!(p.path.len(), 3, "1-hop path should have 3 elements");
    match &p.path[0] {
        PathStep::Entity { canonical, name } => {
            assert_eq!(canonical, "alice");
            assert_eq!(name, "Alice");
        }
        _ => panic!("path[0] should be Entity"),
    }
    match &p.path[1] {
        PathStep::Edge { rel_type } => assert_eq!(rel_type, "knows"),
        _ => panic!("path[1] should be Edge"),
    }
    match &p.path[2] {
        PathStep::Entity { canonical, name } => {
            assert_eq!(canonical, "bob");
            assert_eq!(name, "Bob");
        }
        _ => panic!("path[2] should be Entity"),
    }
}

#[test]
fn test_path_2hop() {
    let db = fresh_db();
    assert_triples(
        &db,
        &[t("Alice", "knows", "Bob"), t("Bob", "works_at", "OpenAI")],
    )
    .unwrap();
    let p = path(&db, "Alice", "OpenAI", 5).unwrap();
    assert!(p.found);
    assert_eq!(p.length, 2);
    // 路径内容：[Alice, knows, Bob, works_at, OpenAI] = 5 elements
    assert_eq!(p.path.len(), 5, "2-hop path should have 5 elements");
    let canonicals: Vec<&str> = p
        .path
        .iter()
        .filter_map(|s| match s {
            PathStep::Entity { canonical, .. } => Some(canonical.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(canonicals, vec!["alice", "bob", "openai"]);
    let rels: Vec<&str> = p
        .path
        .iter()
        .filter_map(|s| match s {
            PathStep::Edge { rel_type } => Some(rel_type.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(rels, vec!["knows", "works_at"]);
}

#[test]
fn test_path_not_found() {
    let db = fresh_db();
    assert_triples(
        &db,
        &[t("Alice", "knows", "Bob"), t("Carol", "knows", "Dave")],
    )
    .unwrap();
    let p = path(&db, "Alice", "Dave", 5).unwrap();
    assert!(!p.found);
}

#[test]
fn test_path_respects_max_hops() {
    let db = fresh_db();
    assert_triples(
        &db,
        &[t("a", "r", "b"), t("b", "r", "c"), t("c", "r", "d")],
    )
    .unwrap();
    // Path a->d is length 3
    let p = path(&db, "a", "d", 2).unwrap();
    assert!(!p.found, "should not find path within max_hops=2");
    let p = path(&db, "a", "d", 5).unwrap();
    assert!(p.found);
    assert_eq!(p.length, 3);
}

#[test]
fn test_path_invalid_max_hops() {
    let db = fresh_db();
    assert!(path(&db, "a", "b", 0).is_err());
    assert!(path(&db, "a", "b", 11).is_err());
}

#[test]
fn test_path_same_node() {
    // src == dst case: found with length 0
    let db = fresh_db();
    assert_triples(&db, &[t("Alice", "knows", "Bob")]).unwrap();
    let p = path(&db, "Alice", "alice", 5).unwrap();
    assert!(p.found);
    assert_eq!(p.length, 0);
}

#[test]
fn test_path_empty_canonical() {
    // src or dst that canonicalizes empty: returns found=false
    let db = fresh_db();
    let p = path(&db, "   ", "Bob", 5).unwrap();
    assert!(!p.found);
    let p = path(&db, "Alice", "  ", 5).unwrap();
    assert!(!p.found);
}

// ─────────────────────────────────────────────
// pending_turn_ids
// ─────────────────────────────────────────────

#[test]
fn test_pending_turn_ids_filters_referenced() {
    let db = fresh_db();
    // 写一个引用 turn_id=10 的三元组
    let triples = vec![TripleInput {
        src: "a".to_string(),
        rel: "x".to_string(),
        dst: "b".to_string(),
        src_type: None,
        dst_type: None,
        confidence: None,
        source_turn: Some(10),
    }];
    assert_triples(&db, &triples).unwrap();

    // [10, 20, 30] 中 10 已被引用，20/30 应作为 pending 返回
    let pending = pending_turn_ids(&db, &[10, 20, 30]).unwrap();
    assert_eq!(pending, vec![20, 30]);
}

#[test]
fn test_pending_turn_ids_empty_input() {
    let db = fresh_db();
    assert!(pending_turn_ids(&db, &[]).unwrap().is_empty());
}

#[test]
fn test_pending_turn_ids_no_relations() {
    let db = fresh_db();
    let pending = pending_turn_ids(&db, &[1, 2, 3]).unwrap();
    assert_eq!(pending, vec![1, 2, 3]);
}

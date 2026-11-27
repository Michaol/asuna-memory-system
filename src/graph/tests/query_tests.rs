//! Tests for the graph query layer: neighbors, path, pending_turn_ids.

use super::helpers::{fresh_db, t};
use crate::graph::query::{
    neighbors, neighbors_with_truncation, path, path_with_max_results, pending_turn_ids, Direction,
    NeighborQuery, PathStep,
};
use crate::graph::{assert_triples, TripleInput};

/// W14 回归：session 实体键必须无损——"Sprint-1" 与 "sprint-1" 是两个不同会话
/// （sessions.session_id 为 BINARY 主键，validate_session_id 允许大写与空格），
/// 折叠成同一图节点会让多跳/邻居检索跨会话混合记忆。
#[test]
fn test_w14_session_entity_key_is_lossless() {
    let db = fresh_db();
    for key in ["session_Sprint-1", "session_sprint-1"] {
        db.conn()
            .execute(
                "INSERT INTO entities (canonical, name, entity_type, first_seen, last_seen)
                 VALUES (?1, ?1, 'session', 0, 0)",
                rusqlite::params![key],
            )
            .unwrap();
    }
    let conn = db.conn();
    let upper = crate::graph::query::resolve_entity_canonical(conn, "session_Sprint-1").unwrap();
    let lower = crate::graph::query::resolve_entity_canonical(conn, "session_sprint-1").unwrap();
    assert_eq!(upper, "session_Sprint-1");
    assert_eq!(lower, "session_sprint-1");
    assert_ne!(upper, lower, "W14: 大小写不同的会话不得折叠成同一节点");
}

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

/// Diamond graph: C is reachable at distance 1 (A->C) AND distance 2 (A->B->C).
/// neighbors() must return C exactly once, at its minimum distance (1).
#[test]
fn test_neighbors_diamond_no_duplicate() {
    let db = fresh_db();
    assert_triples(
        &db,
        &[t("A", "rel", "B"), t("A", "rel", "C"), t("B", "rel", "C")],
    )
    .unwrap();
    let q = NeighborQuery {
        entity: "A".to_string(),
        rel_type: None,
        direction: Direction::Out,
        hops: 2,
        limit: 50,
    };
    let result = neighbors(&db, &q).unwrap();
    let c_rows: Vec<_> = result.iter().filter(|n| n.canonical == "c").collect();
    assert_eq!(
        c_rows.len(),
        1,
        "C must appear exactly once, got {:?}",
        result
    );
    assert_eq!(
        c_rows[0].distance, 1,
        "C must surface at its minimum distance"
    );
    // B at 1, C at 1 → two distinct neighbors total.
    assert_eq!(result.len(), 2);
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
    assert_triples(&db, &[t("a", "r", "b"), t("b", "r", "c"), t("c", "r", "d")]).unwrap();
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

/// J17: the result-row cap is honored without changing the returned shortest
/// path. Multiple equal-length candidate rows exist (two parallel 1-hop edges
/// + one 2-hop path); cap=1 still yields the distance-1 answer.
#[test]
fn test_path_max_results_cap_keeps_shortest() {
    let db = fresh_db();
    assert_triples(
        &db,
        &[
            t("A", "knows", "B"),
            t("A", "likes", "B"),
            t("A", "r", "C"),
            t("C", "r", "B"),
        ],
    )
    .unwrap();

    let full = path_with_max_results(&db, "A", "B", 5, 5000).unwrap();
    let capped = path_with_max_results(&db, "A", "B", 5, 1).unwrap();

    assert!(full.found);
    assert_eq!(full.length, 1);
    assert!(
        capped.found,
        "cap=1 must not turn an existing path into not-found"
    );
    assert_eq!(
        capped.length, full.length,
        "cap must not change shortest length"
    );
    assert!(!capped.path.is_empty());
    // 端点一致：path[0] 与最后元素都是 Entity
    match (&capped.path[0], capped.path.last().unwrap()) {
        (PathStep::Entity { canonical: s, .. }, PathStep::Entity { canonical: d, .. }) => {
            assert_eq!(s, "a");
            assert_eq!(d, "b");
        }
        _ => panic!("path endpoints must be Entity"),
    }

    // max_results=0 内部按 1 处理（不得 LIMIT 0 误报未找到）
    let zero = path_with_max_results(&db, "A", "B", 5, 0).unwrap();
    assert!(zero.found);

    // 不存在的路径在 cap 下依然 not found
    let none = path_with_max_results(&db, "A", "Z", 5, 1).unwrap();
    assert!(!none.found);
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

    // Chunked IN list (>500 IDs) must succeed cleanly
    let many: Vec<i64> = (1..=650).collect();
    assert_eq!(pending_turn_ids(&db, &many).unwrap().len(), 650);
}

#[test]
fn test_path_dense_graph_terminates_quickly() {
    let db = fresh_db();
    let mut triples = Vec::new();
    for i in 0..8 {
        for j in (i + 1)..8 {
            triples.push(t(&format!("N{i}"), "edge", &format!("N{j}")));
        }
    }
    assert_triples(&db, &triples).unwrap();
    let res = path(&db, "N0", "N7", 10).unwrap();
    assert!(res.found);
    assert_eq!(res.length, 1);

    // Unreachable target in dense graph under tight expansion cap
    let miss = path_with_max_results(&db, "N0", "Unreachable", 10, 4).unwrap();
    assert!(!miss.found);
}

// ─────────────────────────────────────────────
// neighbors limit（结果帽 clamp + 满 limit 早停）
// ─────────────────────────────────────────────

/// limit 是结果帽不是有效性约束：0 → clamp 1（返回 1 条，不是空集也不报错），
/// 超大值 → clamp 200。与 hops 的 400 校验尺标不同（见 query.rs MAX_LIMIT 文档）。
#[test]
fn test_neighbors_limit_clamped_not_rejected() {
    let db = fresh_db();
    assert_triples(&db, &[t("A", "r", "B"), t("A", "r", "C"), t("A", "r", "D")]).unwrap();

    let mut q = NeighborQuery {
        entity: "A".to_string(),
        rel_type: None,
        direction: Direction::Out,
        hops: 1,
        limit: 0,
    };
    let zero = neighbors(&db, &q).unwrap();
    assert_eq!(zero.len(), 1, "limit=0 clamp 到 1");
    assert_eq!(zero[0].canonical, "b", "同层按 canonical 取最小");

    q.limit = 2;
    let two = neighbors(&db, &q).unwrap();
    let got: Vec<&str> = two.iter().map(|n| n.canonical.as_str()).collect();
    assert_eq!(got, vec!["b", "c"]);

    q.limit = u32::MAX;
    let all = neighbors(&db, &q).unwrap();
    assert_eq!(all.len(), 3, "超大 limit 不报错，返回全部可用邻居");
}

/// MAX_LIMIT=200 上限：201 个邻居 + limit=u32::MAX → 恰好 200 条，
/// 且截断发生在按 (distance, canonical) 排序之后。
#[test]
fn test_neighbors_limit_upper_cap_200() {
    let db = fresh_db();
    let names: Vec<String> = (0..201).map(|i| format!("N{i:03}")).collect();
    let triples: Vec<TripleInput> = names.iter().map(|n| t("Hub", "r", n)).collect();
    assert_triples(&db, &triples).unwrap();

    let q = NeighborQuery {
        entity: "Hub".to_string(),
        rel_type: None,
        direction: Direction::Out,
        hops: 1,
        limit: u32::MAX,
    };
    let res = neighbors(&db, &q).unwrap();
    assert_eq!(res.len(), 200);
    assert_eq!(res[0].canonical, "n000");
    assert_eq!(res[199].canonical, "n199");
}

/// 满 limit 早停必须在**整层结束后**判：hop1 有 x/y/z、hop2 还有更远节点，
/// limit=2 必须返回同层 canonical 最小的 x/y（层内提前 break 会漏掉它们）。
#[test]
fn test_neighbors_early_stop_keeps_full_distance_layer() {
    let db = fresh_db();
    assert_triples(
        &db,
        &[
            t("A", "r", "Z"),
            t("A", "r", "Y"),
            t("A", "r", "X"),
            t("Z", "r", "Z2"),
            t("Y", "r", "Y2"),
            t("X", "r", "X2"),
        ],
    )
    .unwrap();

    let q = NeighborQuery {
        entity: "A".to_string(),
        rel_type: None,
        direction: Direction::Out,
        hops: 3,
        limit: 2,
    };
    let res = neighbors(&db, &q).unwrap();
    let got: Vec<(&str, u32)> = res
        .iter()
        .map(|n| (n.canonical.as_str(), n.distance))
        .collect();
    assert_eq!(
        got,
        vec![("x", 1), ("y", 1)],
        "满 limit 的那一层必须完整参与排序"
    );
}

/// W17 回归：多跳最短路径在批处理分层 BFS 下保持确定性并跨多跳连通。
#[test]
fn test_w17_batched_path_multihop_and_deterministic_order() {
    let db = fresh_db();
    assert_triples(
        &db,
        &[
            t("Start", "b_edge", "B1"),
            t("Start", "a_edge", "A1"),
            t("A1", "next", "Goal"),
            t("B1", "next", "Goal"),
            t("Goal", "tail", "Extra"),
        ],
    )
    .unwrap();

    let res = path_with_max_results(&db, "Start", "Goal", 4, 100).unwrap();
    assert!(res.found);
    assert_eq!(res.length, 2);
    // Outgoing 按 rel_type, dst_canonical 排序，故 a_edge / A1 先被发现
    assert_eq!(
        res.path,
        vec![
            PathStep::Entity {
                canonical: "start".to_string(),
                name: "Start".to_string()
            },
            PathStep::Edge {
                rel_type: "a_edge".to_string()
            },
            PathStep::Entity {
                canonical: "a1".to_string(),
                name: "A1".to_string()
            },
            PathStep::Edge {
                rel_type: "next".to_string()
            },
            PathStep::Entity {
                canonical: "goal".to_string(),
                name: "Goal".to_string()
            },
        ]
    );
}

/// W17: 种子 hub + n 个 spoke（单跳 Out 邻居）。直接批量 SQL 建图，
/// 避免 10k 次 assert_triples 往返。
fn seed_hub_spokes(db: &crate::index::db::Db, n: usize) {
    let mut sql = String::from(
        "INSERT INTO entities (canonical, name, entity_type, first_seen, last_seen) \
         VALUES ('hub', 'hub', 'unknown', 0, 0);\n",
    );
    sql.push_str(
        "INSERT INTO entities (canonical, name, entity_type, first_seen, last_seen) VALUES ",
    );
    for i in 0..n {
        if i > 0 {
            sql.push(',');
        }
        sql.push_str(&format!("('n{i:05}', 'n{i:05}', 'unknown', 0, 0)"));
    }
    sql.push_str(";\nINSERT INTO relations (src_canonical, rel_type, dst_canonical, confidence, created_at) VALUES ");
    for i in 0..n {
        if i > 0 {
            sql.push(',');
        }
        sql.push_str(&format!("('hub', 'r', 'n{i:05}', 0.5, 0)"));
    }
    sql.push(';');
    db.conn().execute_batch(&sql).unwrap();
}

/// W17 回归：邻居数**恰好**触达节点帽（visited = MAX_NEIGHBOR_VISITED，含起点）
/// 且帽外无结果时，不得误报 truncated。旧实现 `>= cap` 即置位 → 假阳性。
#[test]
fn test_w17_neighbors_exact_cap_not_truncated() {
    let db = fresh_db();
    // visited = 1(hub) + 9_999 = 10_000 = MAX_NEIGHBOR_VISITED，恰好触帽。
    seed_hub_spokes(&db, 9_999);
    let q = NeighborQuery {
        entity: "hub".to_string(),
        rel_type: None,
        direction: Direction::Out,
        hops: 1,
        limit: 200,
    };
    let (res, truncated) = neighbors_with_truncation(&db, &q).unwrap();
    assert!(!truncated, "恰好触帽且无帽外结果不得报截断");
    assert_eq!(res.len(), 200, "limit 子集照常返回");
}

/// W17 回归：帽外**确有**结果（第 MAX+1 个新节点）→ truncated=true，
/// 且结果集精确截到帽内（多余探测节点不入 distances）。
#[test]
fn test_w17_neighbors_over_cap_truncated() {
    let db = fresh_db();
    // visited 将达 10_001 > 帽 → 探测命中。
    seed_hub_spokes(&db, 10_000);
    let q = NeighborQuery {
        entity: "hub".to_string(),
        rel_type: None,
        direction: Direction::Out,
        hops: 1,
        limit: 200,
    };
    let (res, truncated) = neighbors_with_truncation(&db, &q).unwrap();
    assert!(truncated, "帽外有结果必须报截断");
    assert_eq!(res.len(), 200);
}

/// M8c 回归：path 的 SQLite 查询失败必须上抛（500），不得吞错降级成
/// found=false 的"干净"答案。progress handler 中断每个 VDBE 步。
#[test]
fn test_m8c_path_query_error_propagates() {
    let mut db = fresh_db();
    assert_triples(&db, &[t("A", "r", "B")]).unwrap();
    db.conn_mut().progress_handler(1, Some(|| true)).unwrap();
    let res = path(&db, "A", "B", 3);
    db.conn_mut()
        .progress_handler(0, None::<fn() -> bool>)
        .unwrap();
    assert!(res.is_err(), "SQLite failure must propagate out of path()");
}

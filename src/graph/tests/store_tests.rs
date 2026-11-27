//! Tests for the graph store layer: assert_triples, link_entity, FK / CASCADE pinning.

use super::helpers::{fresh_db, t};
use crate::graph::store::{link_entity, prune_dangling_refs};
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
    assert!(
        (conf - 0.95).abs() < 1e-6,
        "confidence should be MAX, got {}",
        conf
    );
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
    assert!(
        (conf - 0.9).abs() < 1e-6,
        "confidence should remain MAX=0.9, got {}",
        conf
    );
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
        &[t("Alice", "works_at", "OpenAI"), t("Alice", "knows", "Bob")],
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
        .query_row(
            "SELECT COUNT(*) FROM entities WHERE canonical='alice'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 0);

    // alice smith has 2 outgoing edges
    let n: i64 = db
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM relations WHERE src_canonical='alice smith'",
            [],
            |r| r.get(0),
        )
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
        .query_row(
            "SELECT COUNT(*) FROM relations WHERE dst_canonical='alice smith'",
            [],
            |r| r.get(0),
        )
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
        .query_row(
            "SELECT COUNT(*) FROM entities WHERE canonical='alice smith'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 1);
}

#[test]
fn test_link_entity_missing_from_is_noop() {
    // from 不存在时为真 no-op：返回 0 且不创建孤儿目标实体
    let db = fresh_db();
    let n = link_entity(&db, "Ghost", "Real").unwrap();
    assert_eq!(n, 0);

    let count: i64 = db
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM entities WHERE canonical='real'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        count, 0,
        "missing from must not create an orphan target entity"
    );
}

#[test]
fn test_link_entity_preserves_relation_kind() {
    let db = fresh_db();
    assert_triples(&db, &[t("Alice", "knows", "OpenAI")]).unwrap();
    db.conn()
        .execute(
            "UPDATE relations SET relation_kind = 'derived' WHERE src_canonical = 'alice'",
            [],
        )
        .unwrap();

    link_entity(&db, "Alice", "Alice Smith").unwrap();

    let kind: String = db
        .conn()
        .query_row(
            "SELECT relation_kind FROM relations WHERE src_canonical = 'alice smith' AND dst_canonical = 'openai'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(kind, "derived");
}

/// 自环取舍（v1.3.0 设计）：from 上的自环边随 CASCADE 删除，**不**重写为 to 的
/// 自环；from→to 的边重定向后也成自环，被 SQL 过滤。返回值仍计入这些被处理的边。
#[test]
fn test_link_entity_drops_self_loops() {
    let db = fresh_db();
    assert_triples(
        &db,
        &[
            t("Alice", "knows", "Alice"),       // from 自环
            t("Alice", "knows", "Bob"),         // 正常出边
            t("Alice", "knows", "Alice Smith"), // 重定向后成 to 自环 → 过滤
        ],
    )
    .unwrap();

    let processed = link_entity(&db, "Alice", "Alice Smith").unwrap();
    assert_eq!(processed, 3, "三条边都被处理（含被丢弃的自环）");

    let conn = db.conn();
    let self_loops: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM relations WHERE src_canonical = dst_canonical",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(self_loops, 0, "不得留下任何自环");
    let total: i64 = conn
        .query_row("SELECT COUNT(*) FROM relations", [], |r| r.get(0))
        .unwrap();
    assert_eq!(total, 1, "只剩 alice smith -> bob");
    let dst: String = conn
        .query_row("SELECT dst_canonical FROM relations", [], |r| r.get(0))
        .unwrap();
    assert_eq!(dst, "bob");
    let alice: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM entities WHERE canonical = 'alice'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(alice, 0);
}

// ─────────────────────────────────────────────
// prune_dangling_refs
// ─────────────────────────────────────────────

/// 只把失效的 source_turn 置 NULL：关系/实体本身保留，活 turn 的引用不动，
/// entities.source_turn 同步清理但不计入返回值；重复调用幂等。
#[test]
fn test_prune_dangling_refs_nulls_dead_refs_only() {
    let db = fresh_db();
    // 活 turn id=7（turns.session_id 有 FK，需要一行会话）
    db.conn()
        .execute_batch(
            "INSERT INTO sessions (session_id, start_ts, file_path, created_at, updated_at)
             VALUES ('s-live', 1, 'x', 1, 1);
             INSERT INTO turns (id, session_id, seq, timestamp_ms, role, preview)
             VALUES (7, 's-live', 0, 1, 'user', 'p');",
        )
        .unwrap();

    let mut dead = t("Alice", "knows", "Bob");
    dead.source_turn = Some(999); // 不存在的 turn
    let mut live = t("Alice", "likes", "Carol");
    live.source_turn = Some(7); // 活 turn
    assert_triples(&db, &[dead, live]).unwrap();

    assert_eq!(prune_dangling_refs(&db).unwrap(), 1, "只清理悬空的那一条");

    let conn = db.conn();
    let dead_ref: Option<i64> = conn
        .query_row(
            "SELECT source_turn FROM relations WHERE src_canonical='alice' AND dst_canonical='bob'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(dead_ref, None);
    let live_ref: Option<i64> = conn
        .query_row(
            "SELECT source_turn FROM relations WHERE src_canonical='alice' AND dst_canonical='carol'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(live_ref, Some(7), "活 turn 引用不得被清掉");
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM relations", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 2, "三元组语义独立于来源 turn，关系本身不删");

    // alice/bob 首次写入带的是悬空 999（第二次 assert 只刷新 last_seen）
    for canon in ["alice", "bob"] {
        let ref_val: Option<i64> = conn
            .query_row(
                "SELECT source_turn FROM entities WHERE canonical = ?1",
                rusqlite::params![canon],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(ref_val, None, "entities.source_turn 同步清理: {canon}");
    }
    let carol_ref: Option<i64> = conn
        .query_row(
            "SELECT source_turn FROM entities WHERE canonical = 'carol'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(carol_ref, Some(7));

    assert_eq!(prune_dangling_refs(&db).unwrap(), 0, "幂等");
}

/// 空图谱 / 无悬空引用时返回 0，不报错。
#[test]
fn test_prune_dangling_refs_noop_on_clean_graph() {
    let db = fresh_db();
    assert_eq!(prune_dangling_refs(&db).unwrap(), 0);
    assert_triples(&db, &[t("Alice", "knows", "Bob")]).unwrap();
    assert_eq!(prune_dangling_refs(&db).unwrap(), 0);
}

/// 冲突保留首次写入版本：name / entity_type / source_turn 不被后续 assert 覆盖
/// （只有 last_seen 刷新、confidence 走 MAX）。
#[test]
fn test_assert_conflict_preserves_first_name_and_source_turn() {
    let db = fresh_db();
    let mut first = t("Alice", "knows", "Bob");
    first.src_type = Some("person".to_string());
    first.source_turn = Some(11);
    assert_triples(&db, &[first]).unwrap();

    let mut second = t("ALICE", "knows", "BOB");
    second.src_type = Some("robot".to_string());
    second.source_turn = Some(22);
    second.confidence = Some(0.9);
    let stats = assert_triples(&db, &[second]).unwrap();
    assert_eq!(stats.entities_created, 0);
    assert_eq!(stats.relations_created, 0);
    assert_eq!(stats.relations_updated, 1);

    let conn = db.conn();
    let (name, ty, st): (String, String, Option<i64>) = conn
        .query_row(
            "SELECT name, entity_type, source_turn FROM entities WHERE canonical='alice'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(name, "Alice", "name 保留首次写入版本");
    assert_eq!(ty, "person");
    assert_eq!(st, Some(11));

    let rel_st: Option<i64> = conn
        .query_row(
            "SELECT source_turn FROM relations WHERE src_canonical='alice' AND dst_canonical='bob'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(rel_st, Some(11), "relation.source_turn 同样不被覆盖");
}

/// M5 回归：merge_triple 修剪实体名与实体类型（空白类型回退 unknown），
/// link_entity 修剪目标名。W17 契约更新：新建/合并目标**不继承** from 的
/// memory_atom_id（概念节点不得暴露于原子级联删除）。
#[test]
fn test_m5_trim_and_link_entity_does_not_propagate_memory_atom_id() {
    let db = fresh_db();
    let now = crate::util::time::now_unix_ms();
    db.conn()
        .execute(
            "INSERT INTO bounded_memory (id, target, content, created_at, updated_at, confidence, memory_type)
             VALUES (42, 'memory', 'atom-owned', ?1, ?1, 'high', 'atom')",
            rusqlite::params![now],
        )
        .unwrap();

    let tr = TripleInput {
        src: "  Alice  ".to_string(),
        rel: "knows".to_string(),
        dst: "  Bob  ".to_string(),
        src_type: Some("   ".to_string()),
        dst_type: Some(" person ".to_string()),
        confidence: Some(0.8),
        source_turn: None,
    };
    assert_triples(&db, &[tr]).unwrap();

    let (alice_name, alice_ty): (String, String) = db
        .conn()
        .query_row(
            "SELECT name, entity_type FROM entities WHERE canonical='alice'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(alice_name, "Alice");
    assert_eq!(alice_ty, "unknown");

    let (bob_name, bob_ty): (String, String) = db
        .conn()
        .query_row(
            "SELECT name, entity_type FROM entities WHERE canonical='bob'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(bob_name, "Bob");
    assert_eq!(bob_ty, "person");

    // 绑定 alice -> bounded_memory(42)，然后 link_entity 到尚不存在的 "  Alice New  "
    db.conn()
        .execute(
            "UPDATE entities SET memory_atom_id = 42 WHERE canonical = 'alice'",
            [],
        )
        .unwrap();
    link_entity(&db, "alice", "  Alice New  ").unwrap();

    let (new_name, atom_id): (String, Option<i64>) = db
        .conn()
        .query_row(
            "SELECT name, memory_atom_id FROM entities WHERE canonical='alice new'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(new_name, "Alice New");
    assert_eq!(
        atom_id, None,
        "W17: 新建合并目标是共享概念节点，不得继承 from.memory_atom_id"
    );
}

/// W17 回归：atom 绑定实体 link 到**已存在**的概念节点（memory_atom_id NULL）
/// 时不得补填 atom id；随后模拟原子驱逐的级联删除
/// （DELETE FROM entities WHERE memory_atom_id = ?，bounded_memory 驱逐同款），
/// 概念节点与其边必须存活。
#[test]
fn test_w17_link_entity_shields_concept_from_atom_cascade() {
    let db = fresh_db();
    let now = crate::util::time::now_unix_ms();
    db.conn()
        .execute(
            "INSERT INTO bounded_memory (id, target, content, created_at, updated_at, confidence, memory_type)
             VALUES (7, 'memory', 'atom-owned', ?1, ?1, 'high', 'atom')",
            rusqlite::params![now],
        )
        .unwrap();
    // 概念节点 bob 已存在（断言层写入，memory_atom_id NULL），alice 绑定 atom 7。
    // alice→bob 在 link 后成自环被过滤；alice→rust 重定向为 bob→rust 存活边。
    assert_triples(
        &db,
        &[t("alice", "knows", "bob"), t("alice", "uses", "rust")],
    )
    .unwrap();
    db.conn()
        .execute(
            "UPDATE entities SET memory_atom_id = 7 WHERE canonical = 'alice'",
            [],
        )
        .unwrap();

    link_entity(&db, "alice", "bob").unwrap();

    let bob_atom: Option<i64> = db
        .conn()
        .query_row(
            "SELECT memory_atom_id FROM entities WHERE canonical = 'bob'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(bob_atom, None, "已存在概念节点不得被补填 from 的 atom id");

    // 原子驱逐级联（与 bounded_memory remove/evict 的 DELETE 同款）。
    db.conn()
        .execute("DELETE FROM entities WHERE memory_atom_id = 7", [])
        .unwrap();
    let bob_alive: i64 = db
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM entities WHERE canonical = 'bob'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(bob_alive, 1, "概念节点不得随原子级联删除");
    let edge_count: i64 = db
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM relations WHERE src_canonical = 'bob' OR dst_canonical = 'bob'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(edge_count, 1, "重定向到概念节点的边必须存活");
}

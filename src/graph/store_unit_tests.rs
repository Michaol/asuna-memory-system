use super::*;

fn fresh_db() -> Db {
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();
    db
}

fn triple(rel: &str, confidence: f64) -> TripleInput {
    TripleInput {
        src: "Alice".to_string(),
        rel: rel.to_string(),
        dst: "Bob".to_string(),
        src_type: None,
        dst_type: None,
        confidence: Some(confidence),
        source_turn: None,
    }
}

fn alice_bob_edges(db: &Db) -> Vec<(String, f64)> {
    let mut stmt = db
        .conn()
        .prepare(
            "SELECT rel_type, confidence FROM relations
             WHERE src_canonical = 'alice' AND dst_canonical = 'bob'",
        )
        .unwrap();
    stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, f64>(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
}

/// M7 回归：rel_type 落库前 trim——"knows " 与 "knows" 必须是同一条边，
/// confidence 走 MAX 合并，而不是因 PK 不同拆成两条（拆开后 rel_type
/// 精确过滤还会漏掉空白变体）。
#[test]
fn test_rel_type_whitespace_variants_merge_into_one_edge() {
    let db = fresh_db();
    let stats = assert_triples(&db, &[triple("knows ", 0.4)]).unwrap();
    assert_eq!(stats.relations_created, 1);

    for rel in ["knows", " knows", "\tknows\n"] {
        let stats = assert_triples(&db, &[triple(rel, 0.9)]).unwrap();
        assert_eq!(stats.relations_created, 0, "rel={:?} 必须命中已有边", rel);
        assert_eq!(stats.relations_updated, 1);
    }

    let edges = alice_bob_edges(&db);
    assert_eq!(edges.len(), 1, "空白变体不得拆成多条边: {:?}", edges);
    assert_eq!(edges[0].0, "knows");
    assert!(
        (edges[0].1 - 0.9).abs() < 1e-6,
        "confidence 应为 MAX=0.9，got {}",
        edges[0].1
    );
}

/// M7 的边界：只 trim 两端空白，不折叠大小写、不折叠内部空白——
/// rel_type 是 agent 自定义标签，"Knows" / "works  at" 仍是不同关系。
#[test]
fn test_rel_type_case_and_inner_whitespace_not_folded() {
    let db = fresh_db();
    assert_triples(&db, &[triple("knows", 0.5), triple("Knows", 0.5)]).unwrap();
    let edges = alice_bob_edges(&db);
    assert_eq!(edges.len(), 2, "大小写不同仍是不同 rel_type: {:?}", edges);

    assert_triples(&db, &[triple("works  at", 0.5)]).unwrap();
    let rel: String = db
        .conn()
        .query_row(
            "SELECT rel_type FROM relations WHERE rel_type LIKE 'works%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(rel, "works  at", "内部空白不折叠");
}

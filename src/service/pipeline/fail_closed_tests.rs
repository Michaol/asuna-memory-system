use super::{filter_l2_atoms, integrate_session_atoms, retain_live_stored, Db, StoredAtom};

fn atom(content: &str, entities: &[&str]) -> crate::memory::l1::Atom {
    crate::memory::l1::Atom {
        content: content.to_string(),
        atom_type: "fact".to_string(),
        confidence: 0.9,
        entities: entities.iter().map(ToString::to_string).collect(),
    }
}

fn insert_atom_row(db: &Db, content: &str) -> i64 {
    db.conn()
        .execute(
            "INSERT INTO bounded_memory (target, content, created_at, updated_at, confidence, memory_type)
             VALUES ('memory', ?1, 0, 0, 'medium', 'atom')",
            rusqlite::params![content],
        )
        .unwrap();
    db.conn().last_insert_rowid()
}

fn stored(source_index: usize, id: i64) -> StoredAtom {
    StoredAtom {
        source_index,
        id,
        supersedes_id: None,
        embedding: vec![1.0, 0.0, 0.0],
    }
}

/// F11: prepare failure (here the bounded_memory table is gone) must fail
/// closed — empty result, so graph integration and L2 skip this round
/// instead of re-inserting orphans from an unverifiable batch.
#[test]
fn retain_live_stored_prepare_failure_fails_closed() {
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();
    let id = insert_atom_row(&db, "原子甲");
    db.conn().execute("DROP TABLE bounded_memory", []).unwrap();

    let atoms = vec![atom("原子甲", &["Rust"])];
    let kept = retain_live_stored(&db, vec![stored(0, id)]);
    assert!(kept.is_empty(), "prepare failure must drop the whole batch");
    assert_eq!(integrate_session_atoms(&db, &atoms, &kept, "s-f11"), 0);
    assert!(filter_l2_atoms(&atoms, kept).is_empty());
}

/// F11: a per-row liveness query failure must also fail closed. The
/// progress handler interrupts every VDBE step, so prepare succeeds but
/// the EXISTS query errors mid-flight.
#[test]
fn retain_live_stored_query_failure_fails_closed() {
    let mut db = Db::open_memory().unwrap();
    db.init_schema().unwrap();
    let id_a = insert_atom_row(&db, "原子甲");
    let id_b = insert_atom_row(&db, "原子乙");
    let atoms = vec![atom("原子甲", &["Rust"]), atom("原子乙", &["Tokio"])];
    let batch = vec![stored(0, id_a), stored(1, id_b)];

    db.conn_mut().progress_handler(1, Some(|| true)).unwrap();
    let kept = retain_live_stored(&db, batch);
    db.conn_mut()
        .progress_handler(0, None::<fn() -> bool>)
        .unwrap();

    assert!(kept.is_empty(), "query failure must drop the whole batch");
    assert_eq!(integrate_session_atoms(&db, &atoms, &kept, "s-f11"), 0);
    let entities: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM entities", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        entities, 0,
        "no entity may be inserted for an unverifiable batch"
    );
    assert!(filter_l2_atoms(&atoms, kept).is_empty());
}

/// W21c: L2 镜像存活复查失败必须 fail-closed——旧实现乐观把未验证 id 当存活，
/// 会为（可能已驱逐的）行写孤儿 .md。修复后复查失败 → 跳过镜像；复查恢复 →
/// 存活行照常写镜像（证明跳过确由失败触发，而非夹具损坏）。
#[test]
fn l2_mirror_liveness_query_failure_fails_closed() {
    let tmp = tempfile::TempDir::new().unwrap();
    let dir = tmp.path().join("scenarios");
    std::fs::create_dir_all(&dir).unwrap();
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();
    let id = insert_atom_row(&db, "场景摘要");
    db.conn()
        .execute(
            "UPDATE bounded_memory SET memory_type = 'scenario' WHERE id = ?1",
            rusqlite::params![id],
        )
        .unwrap();
    let db = std::sync::Arc::new(std::sync::Mutex::new(db));
    let llm = std::sync::Arc::new(crate::memory::llm::LlmClient::new("test", "test", "test"));
    let aggregator = crate::memory::scenario::ScenarioAggregator::new(&llm, &dir);
    let scenario = crate::memory::scenario::Scenario {
        title: "t".to_string(),
        atom_ids: Vec::new(),
        summary: "场景摘要".to_string(),
        created_at: 5000,
        updated_at: 5000,
    };

    // progress handler 中断每个 VDBE 步：存活 SELECT prepare 成功但中途报错。
    db.lock()
        .unwrap()
        .conn_mut()
        .progress_handler(1, Some(|| true))
        .unwrap();
    super::l2::sync_scenario_mirrors(
        &db,
        &aggregator,
        std::slice::from_ref(&scenario),
        &dir,
        &[(0, id)],
        &[],
    );
    db.lock()
        .unwrap()
        .conn_mut()
        .progress_handler(0, None::<fn() -> bool>)
        .unwrap();
    assert!(
        std::fs::read_dir(&dir).unwrap().next().is_none(),
        "unverified rows must not get .md mirrors (fail-closed)"
    );

    // 复查恢复后同一行照常写镜像（正对照）。
    super::l2::sync_scenario_mirrors(
        &db,
        &aggregator,
        std::slice::from_ref(&scenario),
        &dir,
        &[(0, id)],
        &[],
    );
    assert!(
        std::fs::read_dir(&dir).unwrap().next().is_some(),
        "verified live row must still get its mirror"
    );
}

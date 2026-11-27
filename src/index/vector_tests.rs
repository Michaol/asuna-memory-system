use super::*;

/// 生成一个测试向量（维度从 Db 配置读取，只有一个非零分量）
fn make_test_vec(db: &Db, val: f32) -> Vec<f32> {
    let mut v = vec![0.0f32; db.dimensions()];
    v[0] = val;
    v
}

#[test]
fn test_vector_insert_and_search() {
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();

    // 先插入 turn 记录
    db.conn()
        .execute(
            "INSERT INTO sessions (session_id, start_ts, file_path, created_at, updated_at)
         VALUES ('test', 0, 'test.jsonl', 0, 0)",
            [],
        )
        .unwrap();

    db.conn()
        .execute(
            "INSERT INTO turns (session_id, seq, timestamp_ms, role, preview)
         VALUES ('test', 1, 0, 'user', 'hello')",
            [],
        )
        .unwrap();

    let turn_id = db.conn().last_insert_rowid();

    let store = VectorStore::new(&db);

    // 插入向量
    let vec1 = make_test_vec(&db, 1.0);
    store.insert(turn_id, &vec1).unwrap();
    assert_eq!(store.count().unwrap(), 1);

    // 搜索（相同向量应得 distance 0）
    let results = store.search(&vec1, 5).unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].0, turn_id);
    assert!(
        results[0].1 < 0.01,
        "distance should be near 0, got {}",
        results[0].1
    );

    // 删除
    store.delete(turn_id).unwrap();
    assert_eq!(store.count().unwrap(), 0);
}

/// Proves the vec0 table actually uses the cosine metric at runtime (not just
/// in the DDL text). Two orthogonal unit vectors have cosine distance ≈ 1.0;
/// under the old L2 default the int8 distance would be ≈ 127·√2 ≈ 180.
#[test]
fn test_vector_metric_is_cosine() {
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();

    db.conn()
        .execute(
            "INSERT INTO sessions (session_id, start_ts, file_path, created_at, updated_at)
         VALUES ('test', 0, 'test.jsonl', 0, 0)",
            [],
        )
        .unwrap();
    db.conn()
        .execute(
            "INSERT INTO turns (session_id, seq, timestamp_ms, role, preview)
         VALUES ('test', 1, 0, 'user', 'hello')",
            [],
        )
        .unwrap();
    let turn_id = db.conn().last_insert_rowid();

    let store = VectorStore::new(&db);

    // Stored vector points along axis 0; query points along axis 1 (orthogonal).
    let mut stored = vec![0.0f32; db.dimensions()];
    stored[0] = 1.0;
    store.insert(turn_id, &stored).unwrap();

    let mut query = vec![0.0f32; db.dimensions()];
    query[1] = 1.0;

    let results = store.search(&query, 5).unwrap();
    assert_eq!(results.len(), 1);
    let distance = results[0].1;
    assert!(
        distance <= 2.0,
        "cosine distance must be in [0, 2]; got {} (looks like L2, metric not applied)",
        distance
    );
    assert!(
        (distance - 1.0).abs() < 0.05,
        "orthogonal vectors should have cosine distance ≈ 1.0, got {}",
        distance
    );
}

#[test]
fn test_vector_clear() {
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();

    db.conn()
        .execute(
            "INSERT INTO sessions (session_id, start_ts, file_path, created_at, updated_at)
         VALUES ('test', 0, 'test.jsonl', 0, 0)",
            [],
        )
        .unwrap();

    for i in 0..3 {
        db.conn()
            .execute(
                "INSERT INTO turns (session_id, seq, timestamp_ms, role, preview)
             VALUES ('test', ?1, 0, 'user', 'test')",
                rusqlite::params![i + 1],
            )
            .unwrap();
        let id = db.conn().last_insert_rowid();
        let store = VectorStore::new(&db);
        // W13: 用非零值（i+1），零向量会被 insert 跳过
        store
            .insert(id, &make_test_vec(&db, (i + 1) as f32))
            .unwrap();
    }

    let store = VectorStore::new(&db);
    assert_eq!(store.count().unwrap(), 3);
    store.clear().unwrap();
    assert_eq!(store.count().unwrap(), 0);
}

/// W13 回归：零/非有限向量 insert 跳过（Ok 但不落库，绝不硬拒致回滚）；
/// 无效查询向量 search 短路返回空（即使库中存在有效向量）。
#[test]
fn test_vector_search_zero_top_k_and_zero_vector() {
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();
    db.conn()
        .execute(
            "INSERT INTO sessions (session_id, start_ts, file_path, created_at, updated_at)
             VALUES ('tz', 0, 'tz.jsonl', 0, 0)",
            [],
        )
        .unwrap();
    db.conn()
        .execute(
            "INSERT INTO turns (session_id, seq, timestamp_ms, role, preview)
             VALUES ('tz', 1, 0, 'user', 'zero vec')",
            [],
        )
        .unwrap();
    let id = db.conn().last_insert_rowid();
    let store = VectorStore::new(&db);
    let zero = vec![0.0f32; db.dimensions()];

    // 零向量 insert 跳过：Ok 但不落库（不得硬拒返回 Err 致整次保存回滚）
    store.insert(id, &zero).unwrap();
    assert_eq!(store.count().unwrap(), 0, "零向量必须被跳过而非插入");

    // 非有限向量同样跳过
    let mut nan_vec = vec![0.0f32; db.dimensions()];
    nan_vec[0] = f32::NAN;
    store.insert(id, &nan_vec).unwrap();
    assert_eq!(store.count().unwrap(), 0, "非有限向量必须被跳过");

    // 无效查询向量短路返回空（top_k=0 与 top_k>0 皆然）
    assert!(store.search(&zero, 0).unwrap().is_empty());
    assert!(store.search(&zero, 5).unwrap().is_empty());
    assert!(store.search(&nan_vec, 5).unwrap().is_empty());

    // 插入一条有效向量后，无效查询仍短路返回空（不得返回该有效行）
    store.insert(id, &make_test_vec(&db, 1.0)).unwrap();
    assert_eq!(store.count().unwrap(), 1);
    assert!(store.search(&zero, 5).unwrap().is_empty());
    // 有效查询正常命中
    assert_eq!(store.search(&make_test_vec(&db, 1.0), 5).unwrap().len(), 1);
}

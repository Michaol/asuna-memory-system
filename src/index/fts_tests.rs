use super::*;

fn setup_test_data(db: &Db) {
    db.conn()
        .execute(
            "INSERT INTO sessions (session_id, start_ts, file_path, created_at, updated_at)
         VALUES ('s1', 0, 'test.jsonl', 0, 0)",
            [],
        )
        .unwrap();

    db.conn()
        .execute_batch(
            "INSERT INTO turns (id, session_id, seq, timestamp_ms, role, preview) VALUES
         (1, 's1', 1, 1000, 'user', 'Rust is a systems programming language'),
         (2, 's1', 2, 2000, 'assistant', 'Python is great for data science'),
         (3, 's1', 3, 3000, 'user', 'I love Rust for its memory safety');",
        )
        .unwrap();
}

#[test]
fn test_fts_search() {
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();
    setup_test_data(&db);

    let store = FtsStore::new(&db);
    let results = store.search("Rust", 10).unwrap();
    assert!(
        results.len() >= 2,
        "should find at least 2 results for 'Rust'"
    );
}

#[test]
fn test_fts_search_with_time_filter() {
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();
    setup_test_data(&db);

    let store = FtsStore::new(&db);
    // 只搜索 1500ms 之后的内容
    let results = store
        .search_with_time_filter("Rust", Some(1500), None, 10)
        .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].turn_id, 3);
}

/// W2 回归：无空格中文多词查询此前是整串 FTS5 PHRASE（要求分词后相邻同序），
/// "所有权借用检查器" 命中不了 "所有权和借用检查器"，且整串只算 1 个词条使
/// ">=2 词条" 的 OR 回退对中文结构性不可达。现按 jieba 切词后 AND。
#[test]
fn test_w2_cjk_multiword_matches_non_adjacent() {
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();
    db.conn()
        .execute(
            "INSERT INTO sessions (session_id, start_ts, file_path, created_at, updated_at)
             VALUES ('s1', 0, 'test.jsonl', 0, 0)",
            [],
        )
        .unwrap();
    db.conn()
        .execute(
            "INSERT INTO turns (id, session_id, seq, timestamp_ms, role, preview)
             VALUES (1, 's1', 1, 1000, 'user', 'Rust 通过所有权和借用检查器保证内存安全')",
            [],
        )
        .unwrap();

    let store = FtsStore::new(&db);
    let results = store.search("所有权借用检查器", 10).unwrap();
    assert_eq!(
        results.len(),
        1,
        "W2: 非邻接中文多词查询必须命中（旧 PHRASE 行为返回 0）"
    );
}

/// jieba 词级分词：验证中文词级搜索能力
#[test]
fn test_jieba_chinese_word_search() {
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();

    db.conn()
        .execute(
            "INSERT INTO sessions (session_id, start_ts, file_path, created_at, updated_at)
             VALUES ('s1', 0, 'test.jsonl', 0, 0)",
            [],
        )
        .unwrap();

    db.conn()
        .execute_batch(
            "INSERT INTO turns (id, session_id, seq, timestamp_ms, role, preview) VALUES
             (1, 's1', 1, 1000, 'user', '我喜欢编程'),
             (2, 's1', 2, 2000, 'user', '数据库设计很重要'),
             (3, 's1', 3, 3000, 'user', 'Rust is great');",
        )
        .unwrap();

    let store = FtsStore::new(&db);

    // "编程" 作为独立词应命中 turn 1
    let results = store.search("编程", 10).unwrap();
    assert_eq!(
        results.len(),
        1,
        "jieba: '编程' 应命中 1 条，实际 {} 条",
        results.len()
    );

    // "数据库" 作为独立词应命中 turn 2
    let results2 = store.search("数据库", 10).unwrap();
    assert_eq!(
        results2.len(),
        1,
        "jieba: '数据库' 应命中 1 条，实际 {} 条",
        results2.len()
    );

    // "设计" 作为独立词应命中 turn 2
    let results3 = store.search("设计", 10).unwrap();
    assert_eq!(
        results3.len(),
        1,
        "jieba: '设计' 应命中 1 条，实际 {} 条",
        results3.len()
    );
}

/// jieba 英文搜索不受影响
#[test]
fn test_jieba_english_search() {
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();
    setup_test_data(&db);

    let store = FtsStore::new(&db);
    let results = store.search("programming", 10).unwrap();
    assert_eq!(results.len(), 1, "English word search should work");
}

#[test]
fn test_multi_word_safe_fts_and_or_fallback_and_injection() {
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();
    setup_test_data(&db);

    let store = FtsStore::new(&db);

    // Non-adjacent multi-word match ("Rust" + "language" in turn 1)
    let r1 = store.search("Rust language", 10).unwrap();
    assert_eq!(r1.len(), 1);
    assert_eq!(r1[0].turn_id, 1);

    // Non-adjacent multi-word match ("Rust" + "memory" + "safety" in turn 3)
    let r2 = store.search("Rust memory safety", 10).unwrap();
    assert_eq!(r2.len(), 1);
    assert_eq!(r2[0].turn_id, 3);

    // AND yields 0 hits -> multi-term OR fallback still surfaces "Rust"
    let r_or = store.search("Rust nonexistent_token", 10).unwrap();
    assert!(
        r_or.len() >= 2,
        "multi-term OR fallback should match Rust turns, got {}",
        r_or.len()
    );

    // FTS5 operator injection & punctuation-only queries do not error
    let r_inj = store.search("\"Rust\" OR (NOT *foo:bar)", 10).unwrap();
    assert!(!r_inj.is_empty());
    assert!(store.search("   !!! ---   ", 10).unwrap().is_empty());
    assert!(store.search("Rust", 0).unwrap().is_empty());
}

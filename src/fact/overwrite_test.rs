use crate::fact::conversation::{SessionHeader, Turn};
use crate::fact::session_store::SessionStore;
use crate::index::db::Db;
use crate::index::rebuild;

#[test]
fn test_session_overwrite_and_rebuild_consistency() {
    let tmp = std::env::temp_dir().join(format!(
        "asuna_overwrite_test_{}",
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
    ));
    std::fs::create_dir_all(&tmp).unwrap();

    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();
    let store = SessionStore::new(&tmp, &db);

    let session_id = "deep-b71f8462".to_string();

    // 1. 首次导入 (2 轮)
    let header1 = SessionHeader {
        v: 1,
        header_type: "session_header".to_string(),
        session_id: session_id.clone(),
        start_time: "2026-04-10T23:40:00+08:00".to_string(),
        profile_id: "deep-test".to_string(),
        source: Some("deep-test".to_string()),
        agent_model: None,
        title: Some("深测会话".to_string()),
        tags: vec!["deep".to_string(), "test".to_string()],
    };
    let turns1 = vec![
        Turn {
            ts: "2026-04-10T23:40:00+08:00".to_string(),
            seq: 1,
            role: "user".to_string(),
            content: "AMS 深度测试开始".to_string(),
            metadata: None,
        },
        Turn {
            ts: "2026-04-10T23:40:05+08:00".to_string(),
            seq: 2,
            role: "assistant".to_string(),
            content: "记录测试结果".to_string(),
            metadata: None,
        },
    ];
    store.save(&header1, &turns1).expect("Initial save failed");

    // 2. 验证首次状态
    let count: i64 = db
        .conn()
        .query_row(
            "SELECT count(*) FROM turns WHERE session_id = ?1",
            [&session_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 2, "Should have 2 turns initially");

    // 3. 再次导入相同 session_id (1 轮, 期望覆盖)
    let header2 = SessionHeader {
        v: 1,
        header_type: "session_header".to_string(),
        session_id: session_id.clone(),
        start_time: "2026-04-10T23:41:00+08:00".to_string(),
        profile_id: "deep-test".to_string(),
        source: Some("deep-test-2".to_string()),
        agent_model: None,
        title: Some("覆盖版会话".to_string()),
        tags: vec!["overwrite".to_string()],
    };
    let turns2 = vec![Turn {
        ts: "2026-04-10T23:41:00+08:00".to_string(),
        seq: 1,
        role: "user".to_string(),
        content: "这是覆盖版本".to_string(),
        metadata: None,
    }];
    store
        .save(&header2, &turns2)
        .expect("Overwrite save failed");

    // 4. 核心验证：检查 turns 表是否只有新版本
    let count_after: i64 = db
        .conn()
        .query_row(
            "SELECT count(*) FROM turns WHERE session_id = ?1",
            [&session_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        count_after, 1,
        "Overwrite failed: old turns leaked into database!"
    );

    let roles: Vec<String> = db
        .conn()
        .prepare("SELECT role FROM turns WHERE session_id = ?1")
        .unwrap()
        .query_map([&session_id], |r| r.get(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert_eq!(roles, vec!["user".to_string()]);

    // 5. 再次验证：rebuild 是否会崩溃或报错
    // 在真实文件系统模拟 rebuild
    let stats = rebuild::rebuild_from_jsonl(&tmp, &db).expect("Rebuild after overwrite failed!");
    assert_eq!(stats.sessions_processed, 1);
    assert!(
        stats.errors.is_empty(),
        "Rebuild errors: {:?}",
        stats.errors
    );

    // 最终检查数据库中的 turn 数量仍然是 1
    let final_count: i64 = db
        .conn()
        .query_row(
            "SELECT count(*) FROM turns WHERE session_id = ?1",
            [&session_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(final_count, 1);

    std::fs::remove_dir_all(&tmp).unwrap();
}

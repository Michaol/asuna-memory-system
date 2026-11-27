use super::{
    check_consistency, conversation, insert_quantized_vector_rows, new_shared_progress,
    rebuild_from_jsonl, rebuild_from_jsonl_with_callback_and_preview,
    rebuild_from_jsonl_with_progress, should_do_incremental_rebuild, time, Db, RebuildStatus,
    VectorBatchCounters,
};
use crate::index::conversation::{SessionHeader, Turn};
use std::sync::{Arc, Mutex};

#[test]
fn test_rebuild_from_jsonl() {
    let tmp = tempfile::TempDir::new().unwrap();

    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();

    // 写入两个测试会话
    let header1 = SessionHeader {
        v: 1,
        header_type: "session_header".to_string(),
        session_id: "rebuild-1".to_string(),
        start_time: "2026-04-01T10:00:00.000+08:00".to_string(),
        profile_id: "default".to_string(),
        source: Some("test".to_string()),
        agent_model: None,
        title: None,
        tags: vec![],
    };
    let turns1 = vec![Turn {
        ts: "2026-04-01T10:00:05.000+08:00".to_string(),
        seq: 1,
        role: "user".to_string(),
        content: "你好".to_string(),
        metadata: None,
    }];

    let header2 = SessionHeader {
        v: 1,
        header_type: "session_header".to_string(),
        session_id: "rebuild-2".to_string(),
        start_time: "2026-04-02T14:00:00.000+08:00".to_string(),
        profile_id: "default".to_string(),
        source: Some("test".to_string()),
        agent_model: None,
        title: None,
        tags: vec![],
    };
    let turns2 = vec![
        Turn {
            ts: "2026-04-02T14:00:05.000+08:00".to_string(),
            seq: 1,
            role: "user".to_string(),
            content: "关于 Rust".to_string(),
            metadata: None,
        },
        Turn {
            ts: "2026-04-02T14:00:10.000+08:00".to_string(),
            seq: 2,
            role: "assistant".to_string(),
            content: "Rust 很好".to_string(),
            metadata: None,
        },
    ];

    conversation::write_session(tmp.path(), &header1, &turns1).unwrap();
    conversation::write_session(tmp.path(), &header2, &turns2).unwrap();

    // 重建并校验持久化行内容 + FTS5 索引
    let stats = rebuild_from_jsonl(tmp.path(), &db, None, true).unwrap();
    assert_eq!(stats.sessions_processed, 2);
    assert_eq!(stats.turns_indexed, 3);
    assert_eq!(stats.vectors_indexed, 0); // no embedder provided
    assert!(stats.errors.is_empty());

    let rows: Vec<(String, i64, String, String)> = {
        let mut stmt = db
            .conn()
            .prepare("SELECT session_id, seq, role, preview FROM turns ORDER BY session_id, seq")
            .unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    };
    assert_eq!(
        rows,
        vec![
            ("rebuild-1".into(), 1, "user".into(), "你好".into()),
            ("rebuild-2".into(), 1, "user".into(), "关于 Rust".into()),
            (
                "rebuild-2".into(),
                2,
                "assistant".into(),
                "Rust 很好".into()
            ),
        ],
        "rebuilt DB turns must match JSONL session/seq/role/content exactly"
    );

    let fts_hits: i64 = db
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM turns_fts WHERE turns_fts MATCH 'Rust'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(fts_hits, 2, "turns_fts must index exactly the 2 Rust turns");
    let fts_miss: i64 = db
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM turns_fts WHERE turns_fts MATCH 'nonexistenttoken'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(fts_miss, 0);

    // 一致性检查 + 二次全量重建幂等
    let consistency = check_consistency(tmp.path(), &db).unwrap();
    assert_eq!(consistency.jsonl_count, 2);
    assert_eq!(consistency.db_session_count, 2);
    assert!(consistency.in_sync);

    // 失败路径（fail-closed）：损坏且连 header 都无法解析的 JSONL 必须拒绝重建，
    // 且不得清空先前已建好的完好会话数据。
    std::fs::write(tmp.path().join("corrupt.jsonl"), "{broken json\n").unwrap();
    let err = rebuild_from_jsonl(tmp.path(), &db, None, true)
        .expect_err("unparseable-header JSONL must fail closed and refuse rebuild");
    assert!(
        err.to_string().contains("拒绝重建"),
        "unexpected error: {err}"
    );
    let survived_turns: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM turns", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        survived_turns, 3,
        "refused rebuild must leave existing DB turns untouched"
    );
}

/// 验证 rebuild 后 FTS 数量与 turns 数量严格一致
#[test]
fn test_rebuild_fts_consistency() {
    let tmp = std::env::temp_dir().join(format!(
        "asuna_fts_consist_{}",
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
    ));
    std::fs::create_dir_all(&tmp).unwrap();

    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();

    // 写入 1 个会话，包含 4 轮对话
    let header = SessionHeader {
        v: 1,
        header_type: "session_header".to_string(),
        session_id: "fts-consist-1".to_string(),
        start_time: "2026-04-12T10:00:00.000+08:00".to_string(),
        profile_id: "default".to_string(),
        source: Some("test".to_string()),
        agent_model: None,
        title: None,
        tags: vec![],
    };
    let turns = vec![
        Turn {
            ts: "2026-04-12T10:00:01.000+08:00".to_string(),
            seq: 1,
            role: "user".to_string(),
            content: "Rust ownership model".to_string(),
            metadata: None,
        },
        Turn {
            ts: "2026-04-12T10:00:02.000+08:00".to_string(),
            seq: 2,
            role: "assistant".to_string(),
            content: "借用检查器保证内存安全".to_string(),
            metadata: None,
        },
        Turn {
            ts: "2026-04-12T10:00:03.000+08:00".to_string(),
            seq: 3,
            role: "user".to_string(),
            content: "lifetime annotations".to_string(),
            metadata: None,
        },
        Turn {
            ts: "2026-04-12T10:00:04.000+08:00".to_string(),
            seq: 4,
            role: "assistant".to_string(),
            content: "生命周期标注确保引用有效".to_string(),
            metadata: None,
        },
    ];

    conversation::write_session(&tmp, &header, &turns).unwrap();

    // 重建（无 embedder）
    let stats = rebuild_from_jsonl(&tmp, &db, None, true).unwrap();
    assert_eq!(stats.sessions_processed, 1);
    assert_eq!(stats.turns_indexed, 4);
    assert!(stats.errors.is_empty());

    // FTS 数量必须与 turns 数量严格一致
    let turn_count: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM turns", [], |r| r.get(0))
        .unwrap();
    let fts_count: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM turns_fts", [], |r| r.get(0))
        .unwrap_or(0);
    assert_eq!(
        turn_count, fts_count,
        "FTS count ({fts_count}) must equal turns count ({turn_count}) after rebuild"
    );

    // keyword 搜索可命中
    let store = crate::index::fts::FtsStore::new(&db);
    let results = store.search("Rust", 10).unwrap();
    assert!(
        !results.is_empty(),
        "keyword search 'Rust' must return results after rebuild"
    );

    std::fs::remove_dir_all(&tmp).unwrap();
}

#[test]
fn test_rebuild_with_progress_tracking() {
    let tmp = std::env::temp_dir().join(format!(
        "asuna_progress_{}",
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
    ));
    std::fs::create_dir_all(&tmp).unwrap();
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();
    let progress = new_shared_progress();
    let _ = rebuild_from_jsonl_with_progress(&tmp, &db, None, &progress, true).unwrap();
    let p = progress.lock().unwrap();
    assert_eq!(p.status, RebuildStatus::Completed);
    assert!(p.finished_at.is_some());
    std::fs::remove_dir_all(&tmp).unwrap();
}

#[test]
fn test_progress_default_is_idle() {
    let progress = new_shared_progress();
    let p = progress.lock().unwrap();
    assert_eq!(p.status, RebuildStatus::Idle);
    assert_eq!(p.started_at, 0);
}

fn header_for(session_id: &str, start_time: &str) -> SessionHeader {
    SessionHeader {
        v: 1,
        header_type: "session_header".to_string(),
        session_id: session_id.to_string(),
        start_time: start_time.to_string(),
        profile_id: "default".to_string(),
        source: Some("test".to_string()),
        agent_model: None,
        title: None,
        tags: vec![],
    }
}

fn turn(seq: u32, content: &str) -> Turn {
    Turn {
        ts: "2026-04-01T10:00:05.000+08:00".to_string(),
        seq,
        role: "user".to_string(),
        content: content.to_string(),
        metadata: None,
    }
}

/// C6 回归：vec_turns 非空时以 embedder=None 做完整重建，向量必然被 Phase 1
/// 清空且无法重建——必须通过 stats.errors 显式报告（旧实现静默报成功）。
/// 增量路径（跳过 Phase 1）不得误报。
#[test]
fn test_rebuild_no_embedder_reports_vector_wipe() {
    let tmp = std::env::temp_dir().join(format!(
        "asuna_vecwipe_{}",
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
    ));
    std::fs::create_dir_all(&tmp).unwrap();

    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();

    // 预置一条真实 int8 向量（turn 存在但稍后会被重建替换）
    db.conn()
        .execute(
            "INSERT INTO sessions (session_id, start_ts, file_path, created_at, updated_at)
             VALUES ('vecwipe', 0, 'vecwipe.jsonl', 0, 0)",
            [],
        )
        .unwrap();
    db.conn()
        .execute(
            "INSERT INTO turns (session_id, seq, timestamp_ms, role, preview)
             VALUES ('vecwipe', 1, 0, 'user', 'hello')",
            [],
        )
        .unwrap();
    let turn_id = db.conn().last_insert_rowid();
    let store = crate::index::vector::VectorStore::new(&db);
    let mut v = vec![0.0f32; db.dimensions()];
    v[0] = 1.0;
    store.insert(turn_id, &v).unwrap();
    assert_eq!(store.count().unwrap(), 1);

    // JSONL 与会话一致（供后半段增量路径检测）
    let header = header_for("vecwipe", "2026-04-01T10:00:00.000+08:00");
    conversation::write_session(&tmp, &header, &[turn(1, "hello")]).unwrap();

    // 完整重建、无 embedder → 清空向量并报告错误
    let stats = rebuild_from_jsonl(&tmp, &db, None, true).unwrap();
    assert!(
        stats.errors.iter().any(|e| e.contains("无嵌入后端")),
        "full rebuild without embedder must report the vector wipe, got: {:?}",
        stats.errors
    );
    assert_eq!(store.count().unwrap(), 0, "vec_turns should be wiped");

    // 增量路径：vec_turns 已空且 Phase 1 被跳过 → 不得出现该错误
    let stats2 = rebuild_from_jsonl(&tmp, &db, None, false).unwrap();
    assert!(
        stats2.errors.is_empty(),
        "incremental path must not report a wipe it did not perform: {:?}",
        stats2.errors
    );

    std::fs::remove_dir_all(&tmp).unwrap();
}

/// C9 回归：完整重建重插 turns 后 id 全变，entities/relations.source_turn 与
/// bounded_memory.source_turn_ids 必须按 (session_id, seq) 身份 remap；
/// 映射不到的置 NULL / 从 JSON 数组中丢弃。
#[test]
fn test_rebuild_remaps_provenance_refs() {
    let tmp = std::env::temp_dir().join(format!(
        "asuna_remap_{}",
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
    ));
    std::fs::create_dir_all(&tmp).unwrap();

    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();

    // 旧索引：session "remap-1" 两条 turn；session "vanished" 一条（JSONL 中已无）
    db.conn()
        .execute(
            "INSERT INTO sessions (session_id, start_ts, file_path, created_at, updated_at)
             VALUES ('remap-1', 0, 'a.jsonl', 0, 0), ('vanished', 0, 'b.jsonl', 0, 0)",
            [],
        )
        .unwrap();
    db.conn()
        .execute(
            "INSERT INTO turns (session_id, seq, timestamp_ms, role, preview)
             VALUES ('remap-1', 1, 0, 'user', 'one')",
            [],
        )
        .unwrap();
    let old_id1 = db.conn().last_insert_rowid();
    db.conn()
        .execute(
            "INSERT INTO turns (session_id, seq, timestamp_ms, role, preview)
             VALUES ('remap-1', 2, 0, 'user', 'two')",
            [],
        )
        .unwrap();
    let old_id2 = db.conn().last_insert_rowid();
    db.conn()
        .execute(
            "INSERT INTO turns (session_id, seq, timestamp_ms, role, preview)
             VALUES ('vanished', 1, 0, 'user', 'gone')",
            [],
        )
        .unwrap();
    let vanished_id = db.conn().last_insert_rowid();

    // 图谱软引用：A→可 remap，B→已消失会话（应置 NULL），relation→另一可 remap id
    db.conn()
        .execute(
            "INSERT INTO entities (canonical, name, entity_type, first_seen, last_seen, source_turn)
             VALUES ('A', 'A', 'person', 0, 0, ?1), ('B', 'B', 'person', 0, 0, ?2)",
            rusqlite::params![old_id1, vanished_id],
        )
        .unwrap();
    db.conn()
        .execute(
            "INSERT INTO relations (src_canonical, rel_type, dst_canonical, source_turn, created_at)
             VALUES ('A', 'knows', 'B', ?1, 0)",
            rusqlite::params![old_id2],
        )
        .unwrap();

    // 有界记忆溯源：可 remap 元素 + 已消失元素 + 从未存在过的 id
    db.conn()
        .execute(
            "INSERT INTO bounded_memory (target, content, created_at, updated_at, source_turn_ids)
             VALUES ('memory', 'atom-x', 0, 0, ?1)",
            rusqlite::params![format!("[{}, {}, 999999]", old_id1, vanished_id)],
        )
        .unwrap();

    // JSONL 只含 session "remap-1"（seq 1,2）
    let header = header_for("remap-1", "2026-04-01T10:00:00.000+08:00");
    conversation::write_session(&tmp, &header, &[turn(1, "one"), turn(2, "two")]).unwrap();

    let stats = rebuild_from_jsonl(&tmp, &db, None, true).unwrap();
    assert!(stats.errors.is_empty(), "errors: {:?}", stats.errors);

    // 新 id（AUTOINCREMENT 接续，必然不同于旧 id——证明 remap 真实发生）
    let new_id_of = |sid: &str, seq: i64| -> i64 {
        db.conn()
            .query_row(
                "SELECT id FROM turns WHERE session_id = ?1 AND seq = ?2",
                rusqlite::params![sid, seq],
                |r| r.get(0),
            )
            .unwrap()
    };
    let new_id1 = new_id_of("remap-1", 1);
    let new_id2 = new_id_of("remap-1", 2);
    assert_ne!(new_id1, old_id1);
    assert_ne!(new_id2, old_id2);

    let source_of = |canon: &str| -> Option<i64> {
        db.conn()
            .query_row(
                "SELECT source_turn FROM entities WHERE canonical = ?1",
                rusqlite::params![canon],
                |r| r.get(0),
            )
            .unwrap()
    };
    assert_eq!(
        source_of("A"),
        Some(new_id1),
        "entity A must point at remapped turn"
    );
    assert_eq!(
        source_of("B"),
        None,
        "entity B (vanished session) must fall back to NULL"
    );

    let rel_src: Option<i64> = db
        .conn()
        .query_row(
            "SELECT source_turn FROM relations WHERE src_canonical = 'A'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(rel_src, Some(new_id2), "relation must be remapped");

    let ids_json: String = db
        .conn()
        .query_row(
            "SELECT source_turn_ids FROM bounded_memory WHERE content = 'atom-x'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Vec<i64>>(&ids_json).unwrap(),
        vec![new_id1],
        "unmappable source_turn_ids elements must be dropped"
    );

    std::fs::remove_dir_all(&tmp).unwrap();
}

/// J3 回归：增量检测须做 session_id 集合 + 每会话轮数两级对比——等量换代、
/// 同 id 换轮数都不能被放行；doctor（check_consistency）与 rebuild 判定一致。
#[test]
fn test_incremental_detection_two_tier() {
    let tmp = std::env::temp_dir().join(format!(
        "asuna_incsync_{}",
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
    ));
    std::fs::create_dir_all(&tmp).unwrap();

    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();

    // DB 侧种子：session "j3-a" 两条 turn（经完整重建写入）
    let header_a = header_for("j3-a", "2026-04-01T10:00:00.000+08:00");
    conversation::write_session(&tmp, &header_a, &[turn(1, "one"), turn(2, "two")]).unwrap();
    rebuild_from_jsonl(&tmp, &db, None, true).unwrap();
    assert!(
        should_do_incremental_rebuild(&db, &tmp),
        "完全一致时应可增量"
    );
    assert!(check_consistency(&tmp, &db).unwrap().in_sync);

    // 等量换代：换成 session "j3-b"（同 1 个文件、同 2 条 turn）
    std::fs::remove_dir_all(&tmp).unwrap();
    std::fs::create_dir_all(&tmp).unwrap();
    let header_b = header_for("j3-b", "2026-04-02T10:00:00.000+08:00");
    conversation::write_session(&tmp, &header_b, &[turn(1, "one"), turn(2, "two")]).unwrap();
    assert!(
        !should_do_incremental_rebuild(&db, &tmp),
        "等量换代必须触发完整重建"
    );
    assert!(
        !check_consistency(&tmp, &db).unwrap().in_sync,
        "doctor 与 rebuild 判定必须一致"
    );

    // 同 id 但轮数不同
    std::fs::remove_dir_all(&tmp).unwrap();
    std::fs::create_dir_all(&tmp).unwrap();
    conversation::write_session(&tmp, &header_a, &[turn(1, "one")]).unwrap();
    assert!(
        !should_do_incremental_rebuild(&db, &tmp),
        "每会话轮数不同必须触发完整重建"
    );

    std::fs::remove_dir_all(&tmp).unwrap();
}

/// C7 结构回归（Phase 2 事务重排后）：增量模式下预插一条 vec 行，
/// 用不可达端点的 API embedder 跑 Phase 2——嵌入（失败前）不得破坏
/// 已索引向量（断点续传 skip 生效），失败批计入 stats.errors（经
/// failed_count），向量不新增。嵌入移出事务后，本测试同时钉住
/// "失败只影响未索引部分" 的等价语义。（重试退避 ≈ 3-4s。）
#[test]
fn test_rebuild_phase2_resume_survives_embed_failure() {
    let tmp = tempfile::TempDir::new().unwrap();

    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();

    // 2-turn 会话，先以无 embedder 完整重建建立 turns（不触发 C6 误报：
    // 此时 vec_turns 为空）
    let header = header_for("resume", "2026-04-01T10:00:00.000+08:00");
    conversation::write_session(tmp.path(), &header, &[turn(1, "one"), turn(2, "two")]).unwrap();
    let stats0 = rebuild_from_jsonl(tmp.path(), &db, None, true).unwrap();
    assert!(stats0.errors.is_empty(), "{:?}", stats0.errors);

    let turn_ids: Vec<i64> = {
        let mut stmt = db
            .conn()
            .prepare("SELECT id FROM turns ORDER BY id")
            .unwrap();
        stmt.query_map([], |r| r.get::<_, i64>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    };
    assert_eq!(turn_ids.len(), 2);

    // 模拟断点：第一条 turn 已有向量
    let store = crate::index::vector::VectorStore::new(&db);
    let mut v = vec![0.0f32; db.dimensions()];
    v[0] = 1.0;
    store.insert(turn_ids[0], &v).unwrap();
    assert_eq!(store.count().unwrap(), 1);

    // 不可达 API embedder + 增量模式（JSONL↔DB 一致 → 跳过 Phase 1，
    // 保留 vec 行）：Phase 2 只嵌入第二条 turn，必然失败
    let mut emb_cfg = crate::config::Config::default().embedding;
    emb_cfg.api_url = "http://127.0.0.1:9/v1".to_string();
    emb_cfg.api_model = "unreachable-test".to_string();
    let embedder = crate::embedder::LazyEmbedder::from_config(&emb_cfg)
        .expect("配置了 api_url + api_model，API embedder 应构造成功");

    let stats = rebuild_from_jsonl(tmp.path(), &db, Some(&embedder), false).unwrap();
    assert!(
        stats.errors.iter().any(|e| e.contains("未能索引")),
        "embed 失败必须汇入 stats.errors，got: {:?}",
        stats.errors
    );
    assert_eq!(
        stats.vectors_indexed, 1,
        "已索引的那条按断点续传计入 skipped，不得重复嵌入"
    );
    assert_eq!(
        store.count().unwrap(),
        1,
        "嵌入失败不得破坏既有向量（写事务零网络调用）"
    );
}

/// C7 回归：两个 JSONL 文件共享同一 session_id（不同 start_time → 不同
/// 路径）时，foreign_keys=ON + turns 无 CASCADE 下旧实现的 INSERT OR
/// REPLACE 删父行会 FK 失败、丢掉较新文件。修复后按路径排序后见者
/// （较新）确定性胜出：无错误、DB 仅一个 session 行、turns 为胜出文件内容。
#[test]
fn test_rebuild_duplicate_session_id_keeps_newer() {
    let tmp = std::env::temp_dir().join(format!(
        "asuna_dupsid_{}",
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
    ));
    std::fs::create_dir_all(&tmp).unwrap();
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();

    let older = header_for("dup-c7", "2026-04-01T10:00:00.000+08:00");
    let newer = header_for("dup-c7", "2026-04-02T10:00:00.000+08:00");
    conversation::write_session(&tmp, &older, &[turn(1, "old-only")]).unwrap();
    conversation::write_session(&tmp, &newer, &[turn(1, "new-1"), turn(2, "new-2")]).unwrap();

    let stats = rebuild_from_jsonl(&tmp, &db, None, true).unwrap();
    assert!(
        stats.errors.is_empty(),
        "重复 session_id 不得 FK 失败: {:?}",
        stats.errors
    );

    let session_count: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))
        .unwrap();
    assert_eq!(session_count, 1, "去重后仅一个 session 行");

    let previews: Vec<String> = {
        let mut stmt = db
            .conn()
            .prepare("SELECT preview FROM turns WHERE session_id = 'dup-c7' ORDER BY seq")
            .unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    };
    assert_eq!(
        previews,
        vec!["new-1".to_string(), "new-2".to_string()],
        "排序后见者（较新文件）胜出"
    );

    std::fs::remove_dir_all(&tmp).unwrap();
}

/// N1 回归：重复 session_id 且较新文件 start_time 不可解析时，去重不得
/// 先删旧 turns 再早退致数据丢失——应跳过较新文件、保留旧 turns。
#[test]
fn test_rebuild_duplicate_session_id_bad_newer_keeps_old() {
    let tmp = std::env::temp_dir().join(format!(
        "asuna_dupbadts_{}",
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
    ));
    std::fs::create_dir_all(&tmp).unwrap();
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();

    // 较旧文件（有效时间戳，04-01）
    let older = header_for("dup-n1", "2026-04-01T10:00:00.000+08:00");
    conversation::write_session(&tmp, &older, &[turn(1, "keep-old")]).unwrap();

    // 较新文件（坏 start_time），手动置于排序更后的路径（04-02）。
    // write_session_at 仅序列化不解析 start_time，故可写坏时间戳 header。
    let newer = header_for("dup-n1", "BAD-TIMESTAMP");
    let newer_path = tmp
        .join("2026")
        .join("04")
        .join("02")
        .join("20260402T100000_dup-n1.jsonl");
    std::fs::create_dir_all(newer_path.parent().unwrap()).unwrap();
    conversation::write_session_at(&newer_path, &newer, &[turn(1, "new-bad")]).unwrap();

    let stats = rebuild_from_jsonl(&tmp, &db, None, true).unwrap();

    // 旧 turns 必须保留（无数据丢失）
    let previews: Vec<String> = {
        let mut stmt = db
            .conn()
            .prepare("SELECT preview FROM turns WHERE session_id = 'dup-n1' ORDER BY seq")
            .unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    };
    assert_eq!(
        previews,
        vec!["keep-old".to_string()],
        "较新文件坏时间戳时旧 turns 必须保留，不得先删后早退丢数据"
    );
    let session_count: i64 = db
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM sessions WHERE session_id = 'dup-n1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(session_count, 1);
    // 较新文件解析失败应记入 errors（可观测，非静默）
    assert!(
        stats.errors.iter().any(|e| e.contains("时间解析失败")),
        "跳过较新文件须记录错误: {:?}",
        stats.errors
    );

    std::fs::remove_dir_all(&tmp).unwrap();
}

/// W9 回归：turns 非空但 turns_fts 被清空（崩溃残留不一致）时，旧实现
/// DELETE FROM turns 触发 turns_ad 逐行 'delete' 会 FTS5_CORRUPT 失败、
/// 回滚 Phase 1。修复后先 DROP turns_ad → 'delete-all' → 重建 turns_ad，
/// rebuild 必须成功、FTS 与 turns 数量一致，且 turns_ai 保留（重插依赖）。
#[test]
fn test_rebuild_survives_inconsistent_fts() {
    let tmp = std::env::temp_dir().join(format!(
        "asuna_ftscorrupt_{}",
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
    ));
    std::fs::create_dir_all(&tmp).unwrap();
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();

    let header = header_for("fts-inconsist", "2026-04-01T10:00:00.000+08:00");
    conversation::write_session(&tmp, &header, &[turn(1, "alpha"), turn(2, "beta")]).unwrap();
    rebuild_from_jsonl(&tmp, &db, None, true).unwrap();

    // 制造不一致：清空 FTS 但保留 turns（模拟崩溃残留）
    db.conn()
        .execute("INSERT INTO turns_fts(turns_fts) VALUES('delete-all')", [])
        .unwrap();
    let turns_before: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM turns", [], |r| r.get(0))
        .unwrap();
    assert_eq!(turns_before, 2);

    // 再次重建必须成功（旧实现会在此 FTS5_CORRUPT 失败）
    let stats = rebuild_from_jsonl(&tmp, &db, None, true).unwrap();
    assert!(stats.errors.is_empty(), "{:?}", stats.errors);

    let turn_count: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM turns", [], |r| r.get(0))
        .unwrap();
    let fts_count: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM turns_fts", [], |r| r.get(0))
        .unwrap();
    assert_eq!(turn_count, fts_count, "重建后 FTS 必须与 turns 一致");

    let trigger_count = |name: &str| -> i64 {
        db.conn()
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='trigger' AND name=?1",
                rusqlite::params![name],
                |r| r.get(0),
            )
            .unwrap()
    };
    assert_eq!(trigger_count("turns_ai"), 1, "turns_ai 必须保留");
    assert_eq!(trigger_count("turns_ad"), 1, "turns_ad 必须重建");

    std::fs::remove_dir_all(&tmp).unwrap();
}

/// W12 回归：Phase 2 插向量前复核 turns 存活——已删除/不存在的 turn_id
/// 不得插入孤儿向量（占用 KNN LIMIT），且跳过不计入 failed。
#[test]
fn test_phase2_skips_vector_for_deleted_turn() {
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();
    db.conn()
        .execute(
            "INSERT INTO sessions (session_id, start_ts, file_path, created_at, updated_at)
             VALUES ('w12', 0, 'w12.jsonl', 0, 0)",
            [],
        )
        .unwrap();
    db.conn()
        .execute(
            "INSERT INTO turns (session_id, seq, timestamp_ms, role, preview)
             VALUES ('w12', 1, 0, 'user', 'live')",
            [],
        )
        .unwrap();
    let live_id = db.conn().last_insert_rowid();
    let dead_id = live_id + 999; // 不存在于 turns

    let mut v = vec![0.0f32; db.dimensions()];
    v[0] = 1.0;
    let bytes = crate::embedder::quantize_to_int8(&v);
    let rows = vec![(live_id, bytes.clone()), (dead_id, bytes)];
    let mut counters = VectorBatchCounters {
        vectors_indexed: 0,
        failed_count: 0,
        errors: Vec::new(),
    };
    let tx = db.immediate_tx().unwrap();
    insert_quantized_vector_rows(&tx, &rows, &mut counters).unwrap();
    tx.commit().unwrap();

    assert_eq!(counters.vectors_indexed, 1, "仅存活 turn 计入索引");
    assert_eq!(counters.failed_count, 0, "死 turn 跳过不算失败");
    let vec_ids: Vec<i64> = {
        let mut stmt = db.conn().prepare("SELECT rowid FROM vec_turns").unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    };
    assert_eq!(vec_ids, vec![live_id], "不得为已删除 turn 插孤儿向量");
}

// ---- W1.1/W2.4 测试 seam：本地 mock 嵌入服务（OpenAI 格式） ----

/// 启动本地 mock 嵌入服务：对每个 input 返回常量向量。
/// zero=false → 全 0.5（l2_normalize 后为单位向量，分量 ~0.031，absmax 量化
/// 应得全 127——与 v2.7.1 旧 clamp(-1,1)*127 标度的 ±4 可区分）；zero=true → 全零。
/// 返回 (base_url, 停止标志, 线程句柄)；测试结束置 stop 并 join。
fn spawn_mock_embed_server(
    dim: usize,
    zero: bool,
) -> (
    String,
    Arc<std::sync::atomic::AtomicBool>,
    std::thread::JoinHandle<()>,
) {
    use std::io::{Read, Write};
    use std::sync::atomic::{AtomicBool, Ordering};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();
    let handle = std::thread::spawn(move || {
        while !stop_thread.load(Ordering::SeqCst) {
            let (mut stream, _) = match listener.accept() {
                Ok(s) => s,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                    continue;
                }
                Err(_) => break,
            };
            stream.set_nonblocking(false).unwrap();
            // 读请求头 + Content-Length 体
            let mut data = Vec::new();
            let mut buf = [0u8; 8192];
            let header_end = loop {
                let n = stream.read(&mut buf).unwrap_or(0);
                if n == 0 {
                    break None;
                }
                data.extend_from_slice(&buf[..n]);
                if let Some(pos) = data.windows(4).position(|w| w == b"\r\n\r\n") {
                    break Some(pos + 4);
                }
            };
            let Some(header_end) = header_end else {
                continue;
            };
            let headers = String::from_utf8_lossy(&data[..header_end]).to_lowercase();
            let content_len: usize = headers
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(0);
            while data.len() < header_end + content_len {
                let n = stream.read(&mut buf).unwrap_or(0);
                if n == 0 {
                    break;
                }
                data.extend_from_slice(&buf[..n]);
            }
            let body = &data[header_end..header_end + content_len];
            let req: serde_json::Value =
                serde_json::from_slice(body).unwrap_or(serde_json::Value::Null);
            let n_inputs = req
                .get("input")
                .and_then(serde_json::Value::as_array)
                .map(Vec::len)
                .unwrap_or(0);
            let value = if zero { 0.0f32 } else { 0.5f32 };
            let emb = vec![value; dim];
            let data_arr: Vec<serde_json::Value> = (0..n_inputs)
                .map(|i| serde_json::json!({"embedding": emb, "index": i}))
                .collect();
            let resp = serde_json::json!({ "data": data_arr }).to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                resp.len(),
                resp
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });
    (format!("http://{addr}/v1"), stop, handle)
}

fn mock_embedder(url: &str) -> crate::embedder::LazyEmbedder {
    let mut emb_cfg = crate::config::Config::default().embedding;
    emb_cfg.api_url = url.to_string();
    emb_cfg.api_model = "mock-test".to_string();
    crate::embedder::LazyEmbedder::from_config(&emb_cfg)
        .expect("配置了 api_url + api_model，API embedder 应构造成功")
}

/// 预置一个 atom 及其 v2.7.1 旧标度向量（单位向量分量 ~0.03 在 clamp(-1,1)*127
/// 下仅落 ±4 级，这里直接用全 4 字节模拟）。返回 atom id。
fn seed_atom_with_old_scale_vec(db: &Db, content: &str) -> i64 {
    db.conn()
        .execute(
            "INSERT INTO bounded_memory (target, content, created_at, updated_at, memory_type)
             VALUES ('memory', ?1, 1, 1, 'atom')",
            [content],
        )
        .unwrap();
    let id = db.conn().last_insert_rowid();
    let old_bytes = vec![4u8; db.dimensions()];
    db.conn()
        .execute(
            "INSERT INTO vec_bounded_memory (id, embedding) VALUES (?1, vec_int8(?2))",
            rusqlite::params![id, old_bytes],
        )
        .unwrap();
    id
}

fn atom_vec_bytes(db: &Db, id: i64) -> Vec<u8> {
    db.conn()
        .query_row(
            "SELECT embedding FROM vec_bounded_memory WHERE id = ?1",
            [id],
            |r| r.get(0),
        )
        .unwrap()
}

/// W1.1 回归（全链路）：rebuild --full + 可用 embedder 必须重编码 atom 向量——
/// Phase 1 清空旧标度行，Phase 2 后回填为 absmax 标度（单位向量 → 全 127，
/// 而非旧 clamp 标度的 ±4）。HISTORY 升级指引据此兑现。
#[test]
fn test_full_rebuild_reencodes_atom_vectors() {
    let tmp = std::env::temp_dir().join(format!(
        "asuna_w11_{}",
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
    ));
    std::fs::create_dir_all(&tmp).unwrap();
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();

    let header = header_for("w11", "2026-04-01T10:00:00.000+08:00");
    conversation::write_session(&tmp, &header, &[turn(1, "hello atom")]).unwrap();
    let atom_id = seed_atom_with_old_scale_vec(&db, "atom content");
    assert_eq!(atom_vec_bytes(&db, atom_id), vec![4u8; db.dimensions()]);

    let (url, stop, handle) = spawn_mock_embed_server(db.dimensions(), false);
    let embedder = mock_embedder(&url);
    let stats = rebuild_from_jsonl(&tmp, &db, Some(&embedder), true).unwrap();
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    handle.join().unwrap();

    assert!(stats.errors.is_empty(), "{:?}", stats.errors);
    assert_eq!(stats.vectors_indexed, 1, "turn 向量正常嵌入");
    let bytes = atom_vec_bytes(&db, atom_id);
    assert_eq!(bytes.len(), db.dimensions());
    assert!(
        bytes.iter().all(|&b| b == 127),
        "atom 向量必须以 absmax 标度重编码（单位向量 → 全 127），got 前8字节 {:?}",
        &bytes[..8]
    );

    std::fs::remove_dir_all(&tmp).unwrap();
}

/// N1 回归：完整重建 + 不可达 embedder——Phase 1 清空 atom 向量后回填 embed
/// 失败 warn+continue 仍返 Ok，缺口必须汇入 stats.errors（旧实现静默报成功，
/// atom 向量丢失却无任何 atom 专属错误）。重试退避 ≈ 3-4s。
#[test]
fn test_full_rebuild_atom_reembed_gap_reported() {
    let tmp = std::env::temp_dir().join(format!(
        "asuna_n1gap_{}",
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
    ));
    std::fs::create_dir_all(&tmp).unwrap();
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();

    let header = header_for("n1gap", "2026-04-01T10:00:00.000+08:00");
    conversation::write_session(&tmp, &header, &[turn(1, "hello")]).unwrap();
    let atom_id = seed_atom_with_old_scale_vec(&db, "atom content");

    // 不可达 API embedder（127.0.0.1:9 discard 端口，连接必拒）：完整重建触发
    // reencode_atom_vectors，Phase 1 清空 vec_bounded_memory，回填 embed 失败。
    let mut emb_cfg = crate::config::Config::default().embedding;
    emb_cfg.api_url = "http://127.0.0.1:9/v1".to_string();
    emb_cfg.api_model = "unreachable-test".to_string();
    let embedder = crate::embedder::LazyEmbedder::from_config(&emb_cfg)
        .expect("配置了 api_url + api_model，API embedder 应构造成功");

    let stats = rebuild_from_jsonl(&tmp, &db, Some(&embedder), true).unwrap();
    assert!(
        stats
            .errors
            .iter()
            .any(|e| e.contains("atom 向量重嵌入不完整")),
        "atom 向量缺口必须汇入 stats.errors，got: {:?}",
        stats.errors
    );
    assert_eq!(
        db.count_atoms_missing_vec().unwrap(),
        1,
        "活 atom 缺向量数应为 1"
    );
    let vec_count: i64 = db
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM vec_bounded_memory WHERE id = ?1",
            [atom_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(vec_count, 0, "回填失败后 atom 向量不得残留旧标度行");

    std::fs::remove_dir_all(&tmp).unwrap();
}

/// W1.1 回归：无 embedder 的完整重建不得清空 atom 向量（避免无重嵌入的净损失）。
#[test]
fn test_full_rebuild_no_embedder_keeps_atom_vectors() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();

    let header = header_for("w11-noemb", "2026-04-01T10:00:00.000+08:00");
    conversation::write_session(tmp.path(), &header, &[turn(1, "hello")]).unwrap();
    let atom_id = seed_atom_with_old_scale_vec(&db, "atom content");

    let stats = rebuild_from_jsonl(tmp.path(), &db, None, true).unwrap();
    assert!(stats.errors.is_empty(), "{:?}", stats.errors);
    assert_eq!(
        atom_vec_bytes(&db, atom_id),
        vec![4u8; db.dimensions()],
        "无 embedder 时 atom 向量保留旧标度，不得清空"
    );
}

/// W1.1 回归：增量重建（跳过 Phase 1）不得清空/重编码 atom 向量。
#[test]
fn test_incremental_rebuild_keeps_atom_vectors() {
    let tmp = std::env::temp_dir().join(format!(
        "asuna_w11inc_{}",
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
    ));
    std::fs::create_dir_all(&tmp).unwrap();
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();

    let header = header_for("w11-inc", "2026-04-01T10:00:00.000+08:00");
    conversation::write_session(&tmp, &header, &[turn(1, "hello")]).unwrap();
    // 先建立 DB↔JSONL 一致（增量检测前提）
    rebuild_from_jsonl(&tmp, &db, None, true).unwrap();
    let atom_id = seed_atom_with_old_scale_vec(&db, "atom content");

    let (url, stop, handle) = spawn_mock_embed_server(db.dimensions(), false);
    let embedder = mock_embedder(&url);
    let stats = rebuild_from_jsonl(&tmp, &db, Some(&embedder), false).unwrap();
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    handle.join().unwrap();

    assert!(stats.errors.is_empty(), "{:?}", stats.errors);
    assert_eq!(stats.vectors_indexed, 1, "增量 Phase 2 正常嵌入 turn 向量");
    assert_eq!(
        atom_vec_bytes(&db, atom_id),
        vec![4u8; db.dimensions()],
        "增量模式 atom 向量保持原样"
    );

    std::fs::remove_dir_all(&tmp).unwrap();
}

/// W2.4 回归：rebuild 批量路径全零向量守卫——mock 返回全零向量时跳过+计
/// failed（汇入 stats.errors），不得落库全零 int8 行吃 KNN LIMIT。
#[test]
fn test_phase2_zero_vector_batch_guard() {
    let tmp = std::env::temp_dir().join(format!(
        "asuna_w24_{}",
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
    ));
    std::fs::create_dir_all(&tmp).unwrap();
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();

    let header = header_for("w24", "2026-04-01T10:00:00.000+08:00");
    conversation::write_session(&tmp, &header, &[turn(1, "zero vec turn")]).unwrap();

    let (url, stop, handle) = spawn_mock_embed_server(db.dimensions(), true);
    let embedder = mock_embedder(&url);
    let stats = rebuild_from_jsonl(&tmp, &db, Some(&embedder), true).unwrap();
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    handle.join().unwrap();

    assert_eq!(stats.vectors_indexed, 0);
    assert!(
        stats.errors.iter().any(|e| e.contains("未能索引")),
        "全零向量跳过必须计入 failed 并汇入 errors: {:?}",
        stats.errors
    );
    let vec_count: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM vec_turns_rowids", [], |r| r.get(0))
        .unwrap();
    assert_eq!(vec_count, 0, "全零向量不得落库");

    std::fs::remove_dir_all(&tmp).unwrap();
}

/// W2.6 回归：坏 turn ts 回退 start_ts 必须 warn + 记 errors（与 header
/// start_time 失败同口径），数据仍落库（M1 保数据）。
#[test]
fn test_rebuild_bad_turn_ts_fallback_recorded() {
    let tmp = std::env::temp_dir().join(format!(
        "asuna_w26_{}",
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
    ));
    std::fs::create_dir_all(&tmp).unwrap();
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();

    let header = header_for("w26", "2026-04-01T10:00:00.000+08:00");
    let mut bad = turn(2, "bad ts turn");
    bad.ts = "NOT-A-TIMESTAMP".to_string();
    conversation::write_session(&tmp, &header, &[turn(1, "good"), bad]).unwrap();

    let stats = rebuild_from_jsonl(&tmp, &db, None, true).unwrap();
    assert_eq!(stats.turns_indexed, 2, "坏 ts 不丢数据");
    assert!(
        stats.errors.iter().any(|e| e.contains("已回退 start_ts")),
        "坏 ts 回退必须记 errors: {:?}",
        stats.errors
    );
    let start_ts = time::ts_to_unix_ms(&header.start_time).unwrap();
    let bad_ts_ms: i64 = db
        .conn()
        .query_row(
            "SELECT timestamp_ms FROM turns WHERE session_id = 'w26' AND seq = 2",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(bad_ts_ms, start_ts, "坏 ts 回退到 start_ts");

    std::fs::remove_dir_all(&tmp).unwrap();
}

/// D2-11/D2-10 回归：同文件重复 seq 去重（保留先见者），turn_count 记实际
/// 成功插入数而非 JSONL 侧 turns.len()。
#[test]
fn test_rebuild_dedups_duplicate_seq_and_counts_actual() {
    let tmp = std::env::temp_dir().join(format!(
        "asuna_d211_{}",
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
    ));
    std::fs::create_dir_all(&tmp).unwrap();
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();

    let header = header_for("d211", "2026-04-01T10:00:00.000+08:00");
    conversation::write_session(
        &tmp,
        &header,
        &[turn(1, "first"), turn(1, "dup"), turn(2, "second")],
    )
    .unwrap();

    let stats = rebuild_from_jsonl(&tmp, &db, None, true).unwrap();
    assert_eq!(stats.turns_indexed, 2, "重复 seq 去重后仅 2 条");
    assert!(
        stats.errors.is_empty(),
        "去重 warn-only 不记 errors: {:?}",
        stats.errors
    );

    let (turn_count, previews): (i64, Vec<String>) = {
        let tc: i64 = db
            .conn()
            .query_row(
                "SELECT turn_count FROM sessions WHERE session_id = 'd211'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let mut stmt = db
            .conn()
            .prepare("SELECT preview FROM turns WHERE session_id = 'd211' ORDER BY seq")
            .unwrap();
        let pv = stmt
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        (tc, pv)
    };
    assert_eq!(turn_count, 2, "turn_count 记实际成功数（非 turns.len()=3）");
    assert_eq!(
        previews,
        vec!["first".to_string(), "second".to_string()],
        "重复 seq 保留先见者"
    );

    std::fs::remove_dir_all(&tmp).unwrap();
}

/// D2-9 回归：end_ts 取全部可解析 turn ts 的 MAX（不假定文件有序）。
#[test]
fn test_rebuild_end_ts_uses_max() {
    let tmp = std::env::temp_dir().join(format!(
        "asuna_d29_{}",
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
    ));
    std::fs::create_dir_all(&tmp).unwrap();
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();

    let header = header_for("d29", "2026-04-01T10:00:00.000+08:00");
    let mut t1 = turn(1, "later");
    t1.ts = "2026-04-01T12:00:00.000+08:00".to_string();
    let mut t2 = turn(2, "earlier");
    t2.ts = "2026-04-01T11:00:00.000+08:00".to_string();
    conversation::write_session(&tmp, &header, &[t1, t2]).unwrap();

    rebuild_from_jsonl(&tmp, &db, None, true).unwrap();
    let end_ts: i64 = db
        .conn()
        .query_row(
            "SELECT end_ts FROM sessions WHERE session_id = 'd29'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        end_ts,
        time::ts_to_unix_ms("2026-04-01T12:00:00.000+08:00").unwrap(),
        "end_ts 取 MAX 而非 last()"
    );

    std::fs::remove_dir_all(&tmp).unwrap();
}

/// D1-4 回归：Phase 1 完成即经回调上报 sessions/turns 计数（运行中不再恒 0）。
#[test]
fn test_progress_callback_reports_phase1_counts() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();

    let header = header_for("d14", "2026-04-01T10:00:00.000+08:00");
    conversation::write_session(tmp.path(), &header, &[turn(1, "one"), turn(2, "two")]).unwrap();

    type ProgressCalls = Arc<Mutex<Vec<(usize, usize, usize, usize)>>>;
    let calls: ProgressCalls = Arc::new(Mutex::new(Vec::new()));
    let cb_calls = calls.clone();
    let cb = move |s: usize, t: usize, v: usize, total: usize| {
        cb_calls.lock().unwrap().push((s, t, v, total));
    };
    rebuild_from_jsonl_with_callback_and_preview(tmp.path(), &db, None, Some(&cb), true, 200)
        .unwrap();

    let c = calls.lock().unwrap();
    assert!(!c.is_empty(), "Phase 1 后必须至少上报一次");
    assert_eq!(c[0], (1, 2, 0, 0), "首次上报携带 Phase 1 会话/轮次计数");
}

/// W5.2 语义回归：JSONL 解析移到事务外后，坏文件仍逐文件记 errors、不阻断
/// 其余会话索引（错误累积语义不变）。
#[test]
fn test_rebuild_parse_error_accumulates_outside_tx() {
    let tmp = std::env::temp_dir().join(format!(
        "asuna_w52_{}",
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
    ));
    std::fs::create_dir_all(&tmp).unwrap();
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();

    let header = header_for("w52-good", "2026-04-01T10:00:00.000+08:00");
    conversation::write_session(&tmp, &header, &[turn(1, "good")]).unwrap();
    std::fs::write(tmp.join("corrupt.jsonl"), "this is not json\n").unwrap();

    let stats = rebuild_from_jsonl(&tmp, &db, None, true).unwrap();
    assert_eq!(stats.sessions_processed, 1, "好文件正常索引");
    assert_eq!(stats.turns_indexed, 1);
    assert!(
        stats.errors.iter().any(|e| e.contains("解析失败")),
        "坏文件记 errors: {:?}",
        stats.errors
    );

    std::fs::remove_dir_all(&tmp).unwrap();
}

/// C1 回归：DB 已有会话而 conversations 目录缺失/为空时，**普通 rebuild**
/// （不带 --full）不得清空索引。旧实现把"目录不存在"当合法空语料，增量检测
/// 判不一致后走全量 Phase 1，DELETE 掉 turns/sessions/vec_turns 并把图谱
/// source_turn 置 NULL，最后打印"完成: 0 个会话"且 exit 0。
#[test]
fn test_rebuild_refuses_wipe_when_corpus_missing() {
    let tmp = std::env::temp_dir().join(format!(
        "asuna_c1_{}",
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
    ));
    std::fs::create_dir_all(&tmp).unwrap();
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();

    let header = header_for("c1-live", "2026-04-01T10:00:00.000+08:00");
    conversation::write_session(&tmp, &header, &[turn(1, "保留我")]).unwrap();
    rebuild_from_jsonl(&tmp, &db, None, true).unwrap();

    // 语料整体消失：卷未挂载 / 目录被删 / --profile 打错
    std::fs::remove_dir_all(&tmp).unwrap();
    let err =
        rebuild_from_jsonl(&tmp, &db, None, false).expect_err("语料缺失而 DB 非空时必须拒绝重建");
    assert!(err.to_string().contains("拒绝重建"), "{err}");

    let turns: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM turns", [], |r| r.get(0))
        .unwrap();
    assert_eq!(turns, 1, "已入库的 turn 不得被清空");
}

/// W1 回归：某会话已索引后其 JSONL 中间行损坏。header 仍可解析 → 会话身份可识别
/// → scoped 清表保住它仍正确的 DB 行/向量/溯源，其余会话照常索引，且保留会话的
/// FTS posting 必须补回（'delete-all' 清掉了全部）。旧实现要么连带销毁、要么
/// 拒绝整个重建而阻塞所有会话。
#[test]
fn test_rebuild_preserves_session_when_its_file_is_unreadable() {
    let tmp = std::env::temp_dir().join(format!(
        "asuna_w1_{}",
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
    ));
    std::fs::create_dir_all(&tmp).unwrap();
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();

    let header = header_for("w1-live", "2026-04-01T10:00:00.000+08:00");
    let path = conversation::write_session(&tmp, &header, &[turn(1, "仍然正确")]).unwrap();
    rebuild_from_jsonl(&tmp, &db, None, true).unwrap();

    // 中间行损坏（末行残缺已被 read_session 容忍，不再触发守卫）：整文件 Err，
    // 该会话被排除出 parsed，而清表是全局的 → 必须拒绝重建。
    let original = std::fs::read_to_string(&path).unwrap();
    let mut lines: Vec<&str> = original.lines().collect();
    let last = lines.pop().unwrap();
    lines.push("{\"torn_middle\":");
    lines.push(last);
    std::fs::write(&path, lines.join("\n") + "\n").unwrap();

    // 另加一个健康会话：坏文件不得再阻塞其余会话的索引恢复
    let other = header_for("w1-other", "2026-04-02T10:00:00.000+08:00");
    conversation::write_session(&tmp, &other, &[turn(1, "无关会话")]).unwrap();

    let stats = rebuild_from_jsonl(&tmp, &db, None, true)
        .expect("header 可识别时应 scoped 保留而非拒绝整个重建");
    assert!(
        stats.errors.iter().any(|e| e.contains("解析失败")),
        "损坏仍须记 errors: {:?}",
        stats.errors
    );

    let count = |sid: &str| -> i64 {
        db.conn()
            .query_row(
                "SELECT COUNT(*) FROM turns WHERE session_id = ?1",
                rusqlite::params![sid],
                |r| r.get(0),
            )
            .unwrap()
    };
    assert_eq!(count("w1-live"), 1, "坏文件会话仍正确的 DB 行必须保留");
    assert_eq!(count("w1-other"), 1, "无关会话必须照常索引");

    // 'delete-all' 清掉了全部 posting，保留会话的必须按 rowid 补回
    let fts = crate::index::fts::FtsStore::new(&db);
    assert_eq!(
        fts.search("仍然正确", 10).unwrap().len(),
        1,
        "保留会话重建后必须仍可检索（FTS posting 已补回）"
    );
}

/// W22 回归：重复 session_id 的较新文件**有 turns 却一行未落**（此处用触发器
/// 模拟全部 turns INSERT 失败）时，不得释放保存点——否则上面的 DELETE FROM
/// turns 被提交，先前胜出者仍正确的行被销毁。
#[test]
fn test_rebuild_duplicate_replacement_rolls_back_when_zero_turns_inserted() {
    let tmp = std::env::temp_dir().join(format!(
        "asuna_w22_{}",
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
    ));
    std::fs::create_dir_all(&tmp).unwrap();
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();

    let older = header_for("dup-w22", "2026-04-01T10:00:00.000+08:00");
    let newer = header_for("dup-w22", "2026-04-02T10:00:00.000+08:00");
    conversation::write_session(&tmp, &older, &[turn(1, "old-1")]).unwrap();
    conversation::write_session(&tmp, &newer, &[turn(1, "new-1")]).unwrap();
    rebuild_from_jsonl(&tmp, &db, None, true).unwrap();

    db.conn()
        .execute_batch(
            "CREATE TRIGGER block_newer_turns BEFORE INSERT ON turns
             WHEN NEW.preview LIKE 'new-%'
             BEGIN
                 SELECT RAISE(ABORT, 'simulated turn insert failure');
             END;",
        )
        .unwrap();

    let stats = rebuild_from_jsonl(&tmp, &db, None, true).unwrap();
    assert!(
        stats
            .errors
            .iter()
            .any(|e| e.contains("simulated turn insert failure")),
        "turns 插入失败须记 errors: {:?}",
        stats.errors
    );

    let previews: Vec<String> = {
        let mut stmt = db
            .conn()
            .prepare("SELECT preview FROM turns WHERE session_id = 'dup-w22' ORDER BY seq")
            .unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    };
    assert_eq!(
        previews,
        vec!["old-1".to_string()],
        "W22: 替代文件写入 0 行时必须回滚，保留先前胜出者的 turns"
    );

    std::fs::remove_dir_all(&tmp).unwrap();
}

/// W18 回归：rebuild 不得把未变化会话的 updated_at 戳成 now（否则升级指引要求
/// 的 `rebuild --full` 之后，consolidation 会把整个归档语料判为刚变化并立即
/// 全量重生成 L3/L4/L5）。
#[test]
fn test_rebuild_preserves_session_timestamps() {
    let tmp = std::env::temp_dir().join(format!(
        "asuna_w18_{}",
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
    ));
    std::fs::create_dir_all(&tmp).unwrap();
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();

    let header = header_for("w18", "2026-04-01T10:00:00.000+08:00");
    conversation::write_session(&tmp, &header, &[turn(1, "内容")]).unwrap();
    let before = crate::util::time::now_unix_ms();
    rebuild_from_jsonl(&tmp, &db, None, true).unwrap();

    let read_updated = || -> i64 {
        db.conn()
            .query_row(
                "SELECT updated_at FROM sessions WHERE session_id = 'w18'",
                [],
                |r| r.get(0),
            )
            .unwrap()
    };
    let first = read_updated();
    assert!(
        first >= before && first <= crate::util::time::now_unix_ms(),
        "首次入库的新会话 updated_at 应落于重建时间窗内: before={before}, first={first}"
    );

    // 确保毫秒时钟推进，防止二次重建误写 now 却因同毫秒落入盲区
    std::thread::sleep(std::time::Duration::from_millis(5));
    assert!(crate::util::time::now_unix_ms() > first);

    rebuild_from_jsonl(&tmp, &db, None, true).unwrap();
    assert_eq!(read_updated(), first, "重复重建不得改写 updated_at");

    std::fs::remove_dir_all(&tmp).unwrap();
}

/// W4 + M6 回归：
/// 1. 重复 session_id 按排序后者胜出时，sessions_processed 与 turns_indexed 须去重准确（1 / 2）；
/// 2. 后者写入在删旧 turns 之后失败（如 sessions INSERT 触发器报错）时，SavepointGuard 回滚
///    DELETE FROM turns，先前胜出者的 session 与 turns 完整保留。
#[test]
fn test_rebuild_duplicate_session_savepoint_and_dedup_counts() {
    let tmp = std::env::temp_dir().join(format!(
        "asuna_w4_savepoint_{}",
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
    ));
    std::fs::create_dir_all(&tmp).unwrap();
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();

    let older = header_for("dup-w4", "2026-04-01T10:00:00.000+08:00");
    let newer = header_for("dup-w4", "2026-04-02T10:00:00.000+08:00");
    conversation::write_session(&tmp, &older, &[turn(1, "old-1")]).unwrap();
    conversation::write_session(&tmp, &newer, &[turn(1, "new-1"), turn(2, "new-2")]).unwrap();

    let stats = rebuild_from_jsonl(&tmp, &db, None, true).unwrap();
    assert!(stats.errors.is_empty(), "{:?}", stats.errors);
    assert_eq!(stats.sessions_processed, 1, "M6: 重复 session 只计 1 次");
    assert_eq!(stats.turns_indexed, 2, "M6: 仅保留胜出版本的 2 条 turns");

    // 安装临时触发器，拦截新 file_path（2026/04/02）对 sessions 的插入，模拟删旧 turns 后插入失败。
    db.conn()
        .execute_batch(
            "CREATE TRIGGER fail_newer_session BEFORE INSERT ON sessions
             WHEN NEW.file_path LIKE '%20260402%'
             BEGIN
                 SELECT RAISE(ABORT, 'simulated session replace failure');
             END;",
        )
        .unwrap();

    let stats_fail = rebuild_from_jsonl(&tmp, &db, None, true).unwrap();
    assert!(
        stats_fail
            .errors
            .iter()
            .any(|e| e.contains("simulated session replace failure")),
        "插入失败须记录到 errors: {:?}",
        stats_fail.errors
    );
    assert_eq!(stats_fail.sessions_processed, 1);
    assert_eq!(
        stats_fail.turns_indexed, 1,
        "失败版本回滚后仅计旧版本 1 条 turn"
    );

    let previews: Vec<String> = {
        let mut stmt = db
            .conn()
            .prepare("SELECT preview FROM turns WHERE session_id = 'dup-w4' ORDER BY seq")
            .unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    };
    assert_eq!(
        previews,
        vec!["old-1".to_string()],
        "W4: SAVEPOINT 回滚必须保留先前胜出者的 turns，不得因先 DELETE 而丢失数据"
    );

    std::fs::remove_dir_all(&tmp).unwrap();
}

/// W6 回归：同文件存在重复 seq 时，rebuild 后的增量一致性检查须判定 in_sync=true。
#[test]
fn test_rebuild_then_check_consistency_in_sync_with_duplicate_seq() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();

    let header = header_for("w6-sync", "2026-04-01T10:00:00.000+08:00");
    conversation::write_session(
        tmp.path(),
        &header,
        &[turn(1, "first"), turn(2, "second"), turn(1, "dup-first")],
    )
    .unwrap();

    let stats = rebuild_from_jsonl(tmp.path(), &db, None, true).unwrap();
    assert_eq!(stats.turns_indexed, 2);
    // D2-11 保留先见者：seq=1 必须保留 "first" 而非后见的 "dup-first"
    let stored: Vec<(i64, String)> = {
        let mut stmt = db
            .conn()
            .prepare("SELECT seq, preview FROM turns WHERE session_id = 'w6-sync' ORDER BY seq")
            .unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    };
    assert_eq!(
        stored,
        vec![(1, "first".to_string()), (2, "second".to_string())],
        "重复 seq 去重必须保留先见者并丢弃后见副本"
    );

    let consistency = check_consistency(tmp.path(), &db).unwrap();
    assert!(
        consistency.in_sync,
        "重复 seq JSONL 重建后增量一致性检查必须一致: jsonl={} db={}",
        consistency.jsonl_count, consistency.db_session_count
    );
}

/// W14a 回归：含残缺尾行（被中断的追加）的语料重建后，增量检测必须判一致——
/// 摘要不再把残行计入 turn_count，避免启动陷入无限完整重建/重嵌入循环。
#[test]
fn test_rebuild_torn_final_line_stays_incremental() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();

    let header = header_for("w14a-torn", "2026-04-01T10:00:00.000+08:00");
    let path =
        conversation::write_session(tmp.path(), &header, &[turn(1, "first"), turn(2, "second")])
            .unwrap();
    // 模拟 SIGKILL 中断的追加：半截 JSON、无换行
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    f.write_all(b"{\"ts\":\"2026-04-01T10:0").unwrap();
    drop(f);

    let stats = rebuild_from_jsonl(tmp.path(), &db, None, true).unwrap();
    assert_eq!(stats.turns_indexed, 2, "残缺尾行被丢弃，完整 turn 保留");

    let consistency = check_consistency(tmp.path(), &db).unwrap();
    assert!(
        consistency.in_sync,
        "残缺尾行不得造成永久不一致（无限完整重建循环）: jsonl={} db={}",
        consistency.jsonl_count, consistency.db_session_count
    );
    assert!(
        should_do_incremental_rebuild(&db, tmp.path()),
        "残缺尾行语料二次启动必须走增量重建"
    );
}

/// M8b 回归：rebuild unwind 时 RAII 守卫必须把 progress 置为 Failed
/// （含 finished_at 与错误信息），不得永久卡在 Running；started_at 保留。
#[test]
fn test_progress_panic_guard_marks_failed_on_unwind() {
    let progress = new_shared_progress();
    {
        let mut p = progress.lock().unwrap();
        p.status = RebuildStatus::Running;
        p.started_at = 1;
    }
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = super::ProgressPanicGuard(progress.clone());
        panic!("simulated rebuild panic");
    }));
    assert!(result.is_err());
    let p = crate::util::recover_poison(&progress);
    assert_eq!(p.status, RebuildStatus::Failed);
    assert!(p.finished_at.is_some(), "finished_at must be set on panic");
    assert!(
        p.errors.iter().any(|e| e.contains("panic")),
        "panic error message must be recorded: {:?}",
        p.errors
    );
    assert_eq!(p.started_at, 1, "started_at must be preserved");
}

/// M8b 回归：正常返回路径守卫 Drop 为空操作——不得覆盖已写入的终态。
#[test]
fn test_progress_panic_guard_noop_on_normal_drop() {
    let progress = new_shared_progress();
    {
        let mut p = progress.lock().unwrap();
        p.status = RebuildStatus::Completed;
        p.finished_at = Some(42);
    }
    {
        let _guard = super::ProgressPanicGuard(progress.clone());
    }
    let p = progress.lock().unwrap();
    assert_eq!(p.status, RebuildStatus::Completed);
    assert_eq!(p.finished_at, Some(42));
    assert!(p.errors.is_empty());
}

use super::{
    append_jsonl_turns, remove_verified_legacy_jsonl, repair_torn_jsonl_tail,
    rollback_partial_append, write_all_or_rollback, SaveMode, SessionStore,
};
use crate::fact::conversation::{SessionHeader, Turn};
use crate::index::db::Db;
use crate::util::time;
use std::path::{Path, PathBuf};

fn make_header() -> SessionHeader {
    SessionHeader {
        v: 1,
        header_type: "session_header".to_string(),
        session_id: "dual-write-test".to_string(),
        start_time: "2026-04-10T14:30:00.000+08:00".to_string(),
        profile_id: "default".to_string(),
        source: Some("test".to_string()),
        agent_model: None,
        title: None,
        tags: vec!["test".to_string()],
    }
}

fn make_turns() -> Vec<Turn> {
    vec![
        Turn {
            ts: "2026-04-10T14:30:05.000+08:00".to_string(),
            seq: 1,
            role: "user".to_string(),
            content: "测试双写".to_string(),
            metadata: None,
        },
        Turn {
            ts: "2026-04-10T14:30:10.000+08:00".to_string(),
            seq: 2,
            role: "assistant".to_string(),
            content: "收到".to_string(),
            metadata: Some(serde_json::json!({
                "usage": {"input_tokens": 5, "output_tokens": 2}
            })),
        },
    ]
}

#[test]
fn test_session_store_dual_write() {
    let tmp = tempfile::TempDir::new().unwrap();

    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();

    let store = SessionStore::new(tmp.path(), &db);
    let header = make_header();
    let turns = make_turns();
    let stats = store.save(&header, &turns, None).unwrap();

    assert_eq!(stats.session_id, "dual-write-test");
    assert_eq!(stats.turns_saved, 2);
    assert!(!stats.vectors_skipped, "无 embedder 时不应报告跳过向量");

    let count: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 1);

    let db_turns: Vec<(i64, String, String)> = {
        let mut stmt = db
            .conn()
            .prepare("SELECT seq, role, preview FROM turns WHERE session_id = ?1 ORDER BY seq")
            .unwrap();
        stmt.query_map(rusqlite::params![header.session_id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
    };
    assert_eq!(
        db_turns,
        vec![
            (1, "user".into(), "测试双写".into()),
            (2, "assistant".into(), "收到".into()),
        ],
        "SQLite turns must match saved seq/role/preview"
    );

    // FTS5 同步触发器生效
    let fts_hits: i64 = db
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM turns_fts WHERE turns_fts MATCH '双写'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(fts_hits, 1, "turns_fts must index saved turn preview");

    // 读回 JSONL 验证双写内容一致（真相源与 DB 逐条对齐）
    assert!(stats.file_path.exists());
    let (disk_header, disk_turns) =
        crate::fact::conversation::read_session(&stats.file_path).unwrap();
    assert_eq!(disk_header.session_id, header.session_id);
    assert_eq!(disk_header.profile_id, header.profile_id);
    assert_eq!(disk_turns.len(), 2);
    assert_eq!(disk_turns[0].content, "测试双写");
    assert_eq!(disk_turns[1].content, "收到");
    assert_eq!(disk_turns[1].role, "assistant");

    // 失败路径：非法 start_time 必须提前拒绝，不得污染 DB 或新增 JSONL
    let mut bad_header = make_header();
    bad_header.session_id = "bad-ts-session".to_string();
    bad_header.start_time = "not-a-timestamp".to_string();
    assert!(store.save(&bad_header, &turns, None).is_err());
    let count_after: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count_after, 1, "failed save must not insert session row");
    assert_eq!(
        crate::index::conversation::list_sessions(tmp.path()).len(),
        1,
        "failed save must not create extra JSONL file"
    );
}

/// 验证：JSONL 写入失败（目录不可写）时 DB 必须保持一致 —— 但因为我们的实现是
/// "先 commit 再 JSONL"，DB 会更新而 JSONL 缺失。这是可接受的（缺 JSONL 不破坏 DB）。
/// 反向（旧实现）会留下残骸，已废弃。
#[test]
fn test_preview_length_config() {
    let tmp = std::env::temp_dir().join(format!(
        "asuna_preview_{}",
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
    ));
    std::fs::create_dir_all(&tmp).unwrap();

    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();
    let store = SessionStore::new(&tmp, &db).with_preview_length(5);

    let header = make_header();
    let turns = vec![Turn {
        ts: "2026-04-10T14:30:05.000+08:00".to_string(),
        seq: 1,
        role: "user".to_string(),
        content: "一二三四五六七八九十".to_string(),
        metadata: None,
    }];
    store.save(&header, &turns, None).unwrap();

    let preview: String = db
        .conn()
        .query_row("SELECT preview FROM turns LIMIT 1", [], |r| r.get(0))
        .unwrap();
    assert_eq!(preview, "一二三四五");

    std::fs::remove_dir_all(&tmp).unwrap();
}

/// C8/U4/U16 降级契约：API embedder 指向不可达端点（127.0.0.1:9 discard 端口，
/// 连接必然被拒，离线确定性）时，save() 必须 Ok 且会话数据完整落库，
/// 仅向量缺失并在 SaveStats 中置位警告。
/// 注意：cfg(test) 下 embed_batch 退避基数为 10ms（1s/2s 仅生产）、连接超时
/// 250ms，且 127.0.0.1:9 连接拒绝即时返回——本测试毫秒级完成，非数秒。
#[test]
fn test_save_degrades_when_embedder_fails() {
    let tmp = std::env::temp_dir().join(format!(
        "asuna_embed_fail_{}",
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
    ));
    std::fs::create_dir_all(&tmp).unwrap();

    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();

    let mut emb_cfg = crate::config::Config::default().embedding;
    emb_cfg.api_url = "http://127.0.0.1:9/v1".to_string();
    emb_cfg.api_model = "unreachable-test".to_string();
    let embedder = crate::embedder::LazyEmbedder::from_config(&emb_cfg)
        .expect("配置了 api_url + api_model，API embedder 应构造成功");

    let store = SessionStore::new(&tmp, &db);
    let stats = store
        .save(&make_header(), &make_turns(), Some(&embedder))
        .expect("嵌入失败不得阻断 save()");

    assert!(stats.vectors_skipped, "嵌入失败必须置位 vectors_skipped");
    assert_eq!(stats.turns_saved, 2);

    let session_count: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))
        .unwrap();
    assert_eq!(session_count, 1, "sessions 行必须落库");

    let turn_count: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM turns", [], |r| r.get(0))
        .unwrap();
    assert_eq!(turn_count, 2, "turns 行必须落库");

    assert!(stats.file_path.exists(), "JSONL 文件必须写盘");

    let vec_count: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM vec_turns", [], |r| r.get(0))
        .unwrap();
    assert_eq!(vec_count, 0, "向量应为空（本次跳过）");

    std::fs::remove_dir_all(&tmp).unwrap();
}

/// J13: save 的向量清理必须限定在本 session——覆盖式重存时旧 turn 的向量行
/// 被精确删除，其他 session 的向量行（哨兵）不得被波及。
#[test]
fn test_save_vec_cleanup_scoped_to_session() {
    let tmp = std::env::temp_dir().join(format!(
        "asuna_vecscope_{}",
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
    ));
    std::fs::create_dir_all(&tmp).unwrap();

    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();
    let store = SessionStore::new(&tmp, &db);

    let unit_vec = |val: f32| {
        let mut v = vec![0.0f32; db.dimensions()];
        v[0] = val;
        v
    };

    // 哨兵：另一个 session 的 turn + 向量，必须全程存活
    db.conn()
        .execute(
            "INSERT INTO sessions (session_id, start_ts, file_path, created_at, updated_at)
             VALUES ('sentinel-sess', 0, 'sentinel.jsonl', 0, 0)",
            [],
        )
        .unwrap();
    db.conn()
        .execute(
            "INSERT INTO turns (session_id, seq, timestamp_ms, role, preview)
             VALUES ('sentinel-sess', 1, 0, 'user', 'sentinel')",
            [],
        )
        .unwrap();
    let sentinel_turn_id = db.conn().last_insert_rowid();
    let bytes = crate::embedder::quantize_to_int8(&unit_vec(9.0));
    db.conn()
        .execute(
            "INSERT INTO vec_turns (rowid, embedding) VALUES (?1, vec_int8(?2))",
            rusqlite::params![sentinel_turn_id, bytes],
        )
        .unwrap();

    let header = make_header();
    let turns = make_turns();

    // 第一次 save（带向量）
    store
        .save_with_embeddings(&header, &turns, Some(&[unit_vec(1.0), unit_vec(2.0)]))
        .unwrap();
    let first_ids: Vec<i64> = db
        .conn()
        .prepare("SELECT id FROM turns WHERE session_id = 'dual-write-test'")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(first_ids.len(), 2);

    let mut vec_rowids: Vec<i64> = db
        .conn()
        .prepare("SELECT rowid FROM vec_turns")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    vec_rowids.sort_unstable();
    let mut expected = vec![sentinel_turn_id];
    expected.extend(first_ids.iter());
    expected.sort_unstable();
    assert_eq!(vec_rowids, expected, "首次 save：哨兵 + 本 session 向量");

    // 第二次 save（覆盖语义）：旧 turn 向量必须清掉，哨兵必须保留
    store
        .save_with_embeddings(&header, &turns, Some(&[unit_vec(3.0), unit_vec(4.0)]))
        .unwrap();
    let new_ids: Vec<i64> = db
        .conn()
        .prepare("SELECT id FROM turns WHERE session_id = 'dual-write-test'")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();

    let mut vec_rowids: Vec<i64> = db
        .conn()
        .prepare("SELECT rowid FROM vec_turns")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    vec_rowids.sort_unstable();
    let mut expected = vec![sentinel_turn_id];
    expected.extend(new_ids.iter());
    expected.sort_unstable();
    assert_eq!(
        vec_rowids, expected,
        "重存后仅剩哨兵 + 新 turn 向量；旧 turn 向量必须被精确清理"
    );
    for old in &first_ids {
        assert!(
            !vec_rowids.contains(old),
            "stale vector rowid {} must be gone",
            old
        );
    }
    assert!(
        vec_rowids.contains(&sentinel_turn_id),
        "sentinel (other session) vector must survive"
    );

    std::fs::remove_dir_all(&tmp).unwrap();
}

fn turn_at(seq: u32, content: &str) -> Turn {
    Turn {
        ts: "2026-04-10T14:35:00.000+08:00".to_string(),
        seq,
        role: "user".to_string(),
        content: content.to_string(),
        metadata: None,
    }
}

/// C3 回归：JSONL 真相源写入失败时 Append 必须整体回滚（turns + capture_dedup
/// 占位），调用方收到错误并可安全重试。旧实现 commit 之后才写 JSONL 且失败只
/// warn → /capture 返回 200、幂等键被永久烧掉，rebuild 时这些 turn 消失。
#[test]
fn test_append_rolls_back_when_jsonl_write_fails() {
    let tmp = std::env::temp_dir().join(format!(
        "asuna_c3_{}",
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
    ));
    std::fs::create_dir_all(&tmp).unwrap();
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();
    let store = SessionStore::new(&tmp, &db);
    let header = make_header();
    let turns = [turn_at(1, "第一条")];

    // 让目标 JSONL 路径成为一个目录 → append open 必失败
    let target = crate::fact::conversation::compute_session_path(&tmp, &header).unwrap();
    std::fs::create_dir_all(&target).unwrap();

    let err = store
        .save_append_idempotent(&header, &turns, None, Some("batch-c3"))
        .expect_err("JSONL 写失败必须上抛，不得报成功");
    assert!(err.to_string().contains("JSONL append failed"), "{err}");

    let n: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM turns", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 0, "失败批次不得留下 turns");
    let dedup: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM capture_dedup", [], |r| r.get(0))
        .unwrap();
    assert_eq!(dedup, 0, "失败批次不得烧掉 batch_id 幂等键");

    // 障碍移除后同 batch_id 重试必须真正写入（而非命中幂等返回缓存值）
    std::fs::remove_dir(&target).unwrap();
    let (stats, hit) = store
        .save_append_idempotent(&header, &turns, None, Some("batch-c3"))
        .unwrap();
    assert!(!hit, "回滚后重试不应命中幂等");
    assert_eq!(stats.turns_saved, 1);

    std::fs::remove_dir_all(&tmp).unwrap();
}

/// C5 回归：`delete-turn` 只删 DB 行而保留 JSONL 行后，新 Append 不得复用文件里
/// 已存在的 seq——否则 rebuild 的"保留先见者"去重会永久丢弃新捕获的 turn。
#[test]
fn test_append_seq_respects_jsonl_truth_source() {
    let tmp = std::env::temp_dir().join(format!(
        "asuna_c5_{}",
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
    ));
    std::fs::create_dir_all(&tmp).unwrap();
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();
    let store = SessionStore::new(&tmp, &db);
    let header = make_header();

    store
        .save_append_idempotent(
            &header,
            &[turn_at(1, "一"), turn_at(2, "二"), turn_at(3, "三")],
            None,
            None,
        )
        .unwrap();
    let path = crate::fact::conversation::compute_session_path(&tmp, &header).unwrap();

    // 模拟 cmd_delete_turn：只删 DB 行，JSONL 保留
    db.conn()
        .execute(
            "DELETE FROM turns WHERE session_id = ?1 AND seq = 3",
            rusqlite::params![header.session_id],
        )
        .unwrap();

    store
        .save_append_idempotent(&header, &[turn_at(1, "新事实")], None, None)
        .unwrap();

    let seqs: Vec<i64> = {
        let mut stmt = db
            .conn()
            .prepare("SELECT seq FROM turns WHERE session_id = ?1 ORDER BY seq")
            .unwrap();
        stmt.query_map(rusqlite::params![header.session_id], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    };
    assert_eq!(seqs, vec![1, 2, 4], "新 turn 必须避开 JSONL 已占用的 seq 3");

    let (_, jsonl_turns) = crate::fact::conversation::read_session(&path).unwrap();
    let mut uniq: Vec<u32> = jsonl_turns.iter().map(|t| t.seq).collect();
    uniq.sort_unstable();
    uniq.dedup();
    assert_eq!(uniq.len(), jsonl_turns.len(), "JSONL 不得出现重复 seq");

    crate::index::rebuild::rebuild_from_jsonl_with_preview(&tmp, &db, None, true, 200).unwrap();
    let kept: i64 = db
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM turns WHERE preview LIKE '%新事实%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(kept, 1, "rebuild 后新捕获的 turn 必须仍在");

    std::fs::remove_dir_all(&tmp).unwrap();
}

/// C7 回归：上次追加被中断留下半行后，下一次 Append 必须自愈（截回最后一个
/// 完整行），而不是把新行拼到残行上使整会话文件永久不可解析。
#[test]
fn test_append_repairs_torn_jsonl_tail() {
    let tmp = std::env::temp_dir().join(format!(
        "asuna_c7_{}",
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
    ));
    std::fs::create_dir_all(&tmp).unwrap();
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();
    let store = SessionStore::new(&tmp, &db);
    let header = make_header();
    store
        .save_append_idempotent(&header, &[turn_at(1, "第一条")], None, None)
        .unwrap();
    let path = crate::fact::conversation::compute_session_path(&tmp, &header).unwrap();

    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    f.write_all(b"{\"ts\":\"2026-").unwrap();
    drop(f);
    let raw = std::fs::read_to_string(&path).unwrap();
    assert!(
        raw.ends_with("{\"ts\":\"2026-"),
        "前提：尾部存在被中断追加留下的残缺半行"
    );

    store
        .save_append_idempotent(&header, &[turn_at(2, "第二条")], None, None)
        .unwrap();
    let (_, turns) =
        crate::fact::conversation::read_session(&path).expect("追加后文件必须自愈可解析");
    assert_eq!(turns.len(), 2, "残缺半行被截掉，两条完整 turn 保留");
    assert!(turns.iter().any(|t| t.content == "第二条"));

    std::fs::remove_dir_all(&tmp).unwrap();
}

/// 整批重放幂等回归：JSONL 写成功但 DB COMMIT 失败后，文件里留下了这一批而
/// DB 没有。重试同 batch_id 时旧实现会以更高 seq 再追加一份相同内容，rebuild
/// 只按 seq 去重 → 两份都被索引（真相源静默重复）。现在应复用已有 seq 区间并
/// 跳过追加。
#[test]
fn test_append_replay_after_failed_commit_does_not_duplicate() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();
    let store = SessionStore::new(tmp.path(), &db);
    let header = make_header();
    let batch = [turn_at(1, "甲"), turn_at(2, "乙")];

    store
        .save_append_idempotent(&header, &batch, None, Some("b-replay"))
        .unwrap();
    let path = crate::fact::conversation::compute_session_path(tmp.path(), &header).unwrap();

    // 模拟 COMMIT 失败：JSONL 已落盘，DB 侧 turns 与 dedup 占位均未存活
    db.conn()
        .execute(
            "DELETE FROM turns WHERE session_id = ?1",
            rusqlite::params![header.session_id],
        )
        .unwrap();
    db.conn().execute("DELETE FROM capture_dedup", []).unwrap();

    let (stats, hit) = store
        .save_append_idempotent(&header, &batch, None, Some("b-replay"))
        .unwrap();
    assert!(!hit);
    assert_eq!(stats.turns_saved, 2);

    let (_, jsonl_turns) = crate::fact::conversation::read_session(&path).unwrap();
    assert_eq!(jsonl_turns.len(), 2, "重放不得在真相源里追加第二份相同内容");
    let db_seqs: Vec<i64> = {
        let mut stmt = db
            .conn()
            .prepare("SELECT seq FROM turns WHERE session_id = ?1 ORDER BY seq")
            .unwrap();
        stmt.query_map(rusqlite::params![header.session_id], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    };
    assert_eq!(db_seqs, vec![1, 2], "DB 必须复用文件里已有的 seq 区间");
}

/// 重放判据必须要求"文件领先于 DB"：用户合法地连发两次相同批次（无 batch_id）
/// 时尾部同样全等，但那不是重放——若误判就会以相同 seq 再插一遍 DB，造成
/// DB 4 行 / JSONL 2 行的新分歧。两次都必须正常追加。
#[test]
fn test_append_identical_batch_twice_is_not_treated_as_replay() {
    let tmp = std::env::temp_dir().join(format!(
        "asuna_notreplay_{}",
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
    ));
    std::fs::create_dir_all(&tmp).unwrap();
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();
    let store = SessionStore::new(&tmp, &db);
    let header = make_header();
    let batch = [turn_at(1, "甲"), turn_at(2, "乙")];

    store
        .save_append_idempotent(&header, &batch, None, None)
        .unwrap();
    store
        .save_append_idempotent(&header, &batch, None, None)
        .unwrap();

    let path = crate::fact::conversation::compute_session_path(&tmp, &header).unwrap();
    let (_, jsonl_turns) = crate::fact::conversation::read_session(&path).unwrap();
    assert_eq!(jsonl_turns.len(), 4, "合法重复批次必须两份都追加");
    let n: i64 = db
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM turns WHERE session_id = ?1",
            rusqlite::params![header.session_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 4, "DB 必须与 JSONL 同为 4 行，不得以相同 seq 重插");

    std::fs::remove_dir_all(&tmp).unwrap();
}

/// J41 store-level 契约（Append 侧）：同 session 两次 Append → seq 从
/// MAX(seq) 续排且连续、turn_count 累加、vec_turns 行数 = 总 turns 且
/// rowid 与 turns.id 一一对应（J33f：统一走 VectorStore::insert）、
/// file_path 为真实 JSONL 相对路径（J33b：无 gateway:// 伪 URI）且文件
/// 存在、JSONL 为 header + 全部 5 行（含续排后的 seq）。
#[test]
fn test_append_mode_twice_accumulates_with_vectors() {
    let tmp = std::env::temp_dir().join(format!(
        "asuna_append2_{}",
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
    ));
    std::fs::create_dir_all(&tmp).unwrap();

    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();
    let store = SessionStore::new(&tmp, &db);

    let unit_vec = |val: f32| {
        let mut v = vec![0.0f32; db.dimensions()];
        v[0] = val;
        v
    };
    let header = make_header();

    let stats1 = store
        .save_with_embeddings_mode(
            &header,
            &make_turns(),
            Some(&[unit_vec(1.0), unit_vec(2.0)]),
            SaveMode::Append,
        )
        .unwrap();
    assert_eq!(stats1.turns_saved, 2);

    let batch2 = vec![Turn {
        ts: "2026-04-10T14:31:00.000+08:00".to_string(),
        seq: 99, // Append 模式必须忽略输入 seq（/capture 请求体本无 seq）
        role: "user".to_string(),
        content: "追加轮次".to_string(),
        metadata: None,
    }];
    let stats2 = store
        .save_with_embeddings_mode(&header, &batch2, Some(&[unit_vec(3.0)]), SaveMode::Append)
        .unwrap();
    assert_eq!(stats2.turns_saved, 1);

    // seq 连续：1,2,3（第二批不得是 99）
    let seqs: Vec<i64> = {
        let mut stmt = db
            .conn()
            .prepare("SELECT seq FROM turns ORDER BY seq")
            .unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    };
    assert_eq!(seqs, vec![1, 2, 3], "Append 必须从 MAX(seq) 续排");

    // sessions：turn_count 累加，file_path 真实且文件存在
    let (turn_count, file_path): (i64, String) = db
        .conn()
        .query_row(
            "SELECT turn_count, file_path FROM sessions WHERE session_id = ?1",
            rusqlite::params![header.session_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(turn_count, 3, "turn_count 必须累加");
    assert!(
        !file_path.contains("gateway://"),
        "J33b: file_path 不得再写伪 URI, got {file_path}"
    );
    let jsonl = tmp.join(&file_path);
    assert!(jsonl.exists(), "file_path 必须指向真实落盘的 JSONL");

    // JSONL：header + 3 行，行内 seq 与 DB 续排一致
    let (hdr, jsonl_turns) = super::super::conversation::read_session(&jsonl).unwrap();
    assert_eq!(hdr.session_id, header.session_id);
    assert_eq!(jsonl_turns.len(), 3);
    assert_eq!(
        jsonl_turns.iter().map(|t| t.seq).collect::<Vec<_>>(),
        vec![1, 2, 3]
    );

    // 向量：行数 = 总 turns，且 rowid 集合 == 本 session 的 turns.id 集合
    let turn_ids: Vec<i64> = {
        let mut stmt = db
            .conn()
            .prepare("SELECT id FROM turns WHERE session_id = ?1 ORDER BY id")
            .unwrap();
        stmt.query_map(rusqlite::params![header.session_id], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    };
    let vec_ids: Vec<i64> = {
        let mut stmt = db
            .conn()
            .prepare("SELECT rowid FROM vec_turns ORDER BY rowid")
            .unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    };
    assert_eq!(vec_ids, turn_ids, "Append 向量 rowid 必须逐 turn 对应");

    std::fs::remove_dir_all(&tmp).unwrap();
}

/// J41 store-level 契约（Overwrite 侧）：同 session 两次 Overwrite →
/// turns 行数不翻倍、S8 定点清理后 vec_turns 仅剩最新一轮、turn_count
/// 为全量覆盖值（非累加）、JSONL 全量重写（行数不叠加）、file_path 真实。
#[test]
fn test_overwrite_mode_twice_no_row_doubling() {
    let tmp = std::env::temp_dir().join(format!(
        "asuna_ovr2_{}",
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
    ));
    std::fs::create_dir_all(&tmp).unwrap();

    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();
    let store = SessionStore::new(&tmp, &db);

    let unit_vec = |val: f32| {
        let mut v = vec![0.0f32; db.dimensions()];
        v[0] = val;
        v
    };
    let header = make_header();
    let turns = make_turns();

    store
        .save_with_embeddings_mode(
            &header,
            &turns,
            Some(&[unit_vec(1.0), unit_vec(2.0)]),
            SaveMode::Overwrite,
        )
        .unwrap();
    let first_ids: Vec<i64> = {
        let mut stmt = db.conn().prepare("SELECT id FROM turns").unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    };
    store
        .save_with_embeddings_mode(
            &header,
            &turns,
            Some(&[unit_vec(3.0), unit_vec(4.0)]),
            SaveMode::Overwrite,
        )
        .unwrap();

    let turn_count: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM turns", [], |r| r.get(0))
        .unwrap();
    assert_eq!(turn_count, 2, "Overwrite 两次后 turns 行数不得翻倍");

    let vec_count: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM vec_turns", [], |r| r.get(0))
        .unwrap();
    assert_eq!(vec_count, 2, "S8 定点清理：仅剩最新一轮的向量");
    let live_ids: Vec<i64> = {
        let mut stmt = db.conn().prepare("SELECT id FROM turns").unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    };
    for old in &first_ids {
        assert!(
            !live_ids.contains(old) && !has_vec_row(&db, *old),
            "旧 turn 行与其向量都必须被覆盖清掉 (rowid {old})"
        );
    }

    let (sessions_count, session_turn_count, file_path): (i64, i64, String) = db
        .conn()
        .query_row(
            "SELECT COUNT(*), (SELECT turn_count FROM sessions WHERE session_id = 'dual-write-test'),
                    (SELECT file_path FROM sessions WHERE session_id = 'dual-write-test')",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(sessions_count, 1);
    assert_eq!(session_turn_count, 2, "Overwrite 语义为全量覆盖而非累加");
    assert!(!file_path.contains("gateway://"));

    // JSONL 全量重写：header + 2 行（不是两次保存叠加的 5 行）
    let jsonl = tmp.join(&file_path);
    let (_, jsonl_turns) = super::super::conversation::read_session(&jsonl).unwrap();
    assert_eq!(jsonl_turns.len(), 2);

    std::fs::remove_dir_all(&tmp).unwrap();
}

/// 辅助：vec_turns 中是否存在指定 rowid（S8 定点清理断言用）
fn has_vec_row(db: &Db, rowid: i64) -> bool {
    db.conn()
        .query_row(
            "SELECT COUNT(*) FROM vec_turns WHERE rowid = ?1",
            rusqlite::params![rowid],
            |r| r.get::<_, i64>(0),
        )
        .unwrap_or(0)
        > 0
}

/// 复现 v2.6.2 REST /capture 的升级前状态（U5 改名 × J33b file_path 改写
/// 缝隙）：磁盘上是旧命名 JSONL 文件（日期目录/紧凑时间串取 header
/// start_time 的墙钟值，后缀 = session_id 前 8 字符——手写死值，不复用
/// `compute_legacy_session_path` 以免自证），DB 行 file_path 为
/// `gateway://` 伪 URI、turns 两行。
fn seed_pre_u5_rest_session(tmp: &Path, db: &Db) -> (SessionHeader, PathBuf, Vec<Turn>) {
    let header = make_header();
    let old_turns = make_turns();
    let legacy = tmp
        .join("2026")
        .join("04")
        .join("10")
        .join("20260410T143000_dual-wri.jsonl");
    std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
    crate::index::conversation::write_session_at(&legacy, &header, &old_turns).unwrap();

    let start_ts = crate::util::time::ts_to_unix_ms(&header.start_time).unwrap();
    db.conn()
        .execute(
            "INSERT INTO sessions (session_id, start_ts, file_path, turn_count, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, 0, 0)",
            rusqlite::params![
                header.session_id,
                start_ts,
                format!("gateway://{}", header.session_id),
                old_turns.len() as i64,
            ],
        )
        .unwrap();
    for t in &old_turns {
        db.conn()
            .execute(
                "INSERT INTO turns (session_id, seq, timestamp_ms, role, preview, char_count)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![
                    header.session_id,
                    t.seq as i64,
                    start_ts,
                    t.role,
                    t.content.chars().take(200).collect::<String>(),
                    t.content.chars().count() as i64,
                ],
            )
            .unwrap();
    }
    (header, legacy, old_turns)
}

/// U5 升级缝隙回归（Append 侧）：v2.6.2 /capture 会话升级后首次追加，旧
/// 命名文件的 turns 必须迁移进新命名目标文件后删除旧文件（JSONL 真相源
/// 不丢数据），终态 = 单文件、seq 连续、DB file_path 为真实路径——否则
/// 同一 session_id 双文件并存，rebuild 混并两份 turns、增量检测恒不一致。
#[test]
fn test_append_migrates_pre_u5_legacy_jsonl() {
    let tmp = std::env::temp_dir().join(format!(
        "asuna_legacy_app_{}",
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
    ));
    std::fs::create_dir_all(&tmp).unwrap();
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();
    let (header, legacy, old_turns) = seed_pre_u5_rest_session(&tmp, &db);

    let appended = Turn {
        ts: "2026-04-10T14:35:00.000+08:00".to_string(),
        seq: 99, // Append 忽略输入 seq，事务内续排为 3
        role: "user".to_string(),
        content: "升级后追加".to_string(),
        metadata: None,
    };
    let store = SessionStore::new(&tmp, &db);
    let stats = store
        .save_with_embeddings_mode(&header, &[appended], None, SaveMode::Append)
        .unwrap();

    // 旧文件已迁移并删除，磁盘上本会话只剩一个文件
    assert!(!legacy.exists(), "改名前旧文件必须在迁移后删除");
    let files = crate::index::conversation::list_sessions(&tmp);
    assert_eq!(files.len(), 1, "升级缝隙封死后不得双文件并存");
    assert_eq!(files[0], stats.file_path);

    // DB file_path 已从伪 URI 刷新为真实路径且指向该文件
    let file_rel: String = db
        .conn()
        .query_row(
            "SELECT file_path FROM sessions WHERE session_id = ?1",
            rusqlite::params![header.session_id],
            |r| r.get(0),
        )
        .unwrap();
    assert!(!file_rel.contains("gateway://"), "J33b: {file_rel}");
    assert_eq!(tmp.join(&file_rel), files[0]);

    // 目标文件 = header + 迁移的 2 轮 + 新 1 轮，seq 连续、内容不丢
    let (_, turns) = crate::index::conversation::read_session(&files[0]).unwrap();
    assert_eq!(
        turns.iter().map(|t| t.seq).collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    assert_eq!(turns[0].content, old_turns[0].content, "迁移不得丢 turns");
    assert_eq!(turns[2].content, "升级后追加");

    // DB 与文件行数一致（增量检测的前提）
    let (db_turns, turn_count): (i64, i64) = db
        .conn()
        .query_row(
            "SELECT (SELECT COUNT(*) FROM turns), (SELECT turn_count FROM sessions WHERE session_id = ?1)",
            rusqlite::params![header.session_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(db_turns, 3);
    assert_eq!(turn_count, 3);

    std::fs::remove_dir_all(&tmp).unwrap();
}

/// U5 升级缝隙回归（Overwrite 侧）：升级后旧会话首次 MCP save_session /
/// CLI import 覆盖修正——DB 伪 URI 定位不到旧文件，但新命名规则复原必须
/// 命中并删除旧文件（旧内容按覆盖语义丢弃、不迁移）。若放任并存，rebuild
/// 会把被修正前的旧内容以重复 seq 复活。
#[test]
fn test_overwrite_removes_pre_u5_legacy_jsonl() {
    let tmp = std::env::temp_dir().join(format!(
        "asuna_legacy_ovr_{}",
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
    ));
    std::fs::create_dir_all(&tmp).unwrap();
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();
    let (header, legacy, _) = seed_pre_u5_rest_session(&tmp, &db);

    let corrected = Turn {
        ts: "2026-04-10T14:36:00.000+08:00".to_string(),
        seq: 1,
        role: "user".to_string(),
        content: "修正后内容".to_string(),
        metadata: None,
    };
    let store = SessionStore::new(&tmp, &db);
    store
        .save_with_embeddings_mode(&header, &[corrected], None, SaveMode::Overwrite)
        .unwrap();

    assert!(
        !legacy.exists(),
        "覆盖语义：改名前旧文件必须被删除（防 rebuild 复活）"
    );
    let files = crate::index::conversation::list_sessions(&tmp);
    assert_eq!(files.len(), 1);
    let (_, turns) = crate::index::conversation::read_session(&files[0]).unwrap();
    assert_eq!(turns.len(), 1, "请求内容即新真相");
    assert_eq!(turns[0].content, "修正后内容");
    let file_rel: String = db
        .conn()
        .query_row(
            "SELECT file_path FROM sessions WHERE session_id = ?1",
            rusqlite::params![header.session_id],
            |r| r.get(0),
        )
        .unwrap();
    assert!(!file_rel.contains("gateway://"));

    std::fs::remove_dir_all(&tmp).unwrap();
}

/// W15 回归：prior_db 必须在事务内读。外部连接（CLI / 另一进程）持写锁
/// 改写 sessions.file_path 时，save_overwrite 阻塞在 BEGIN IMMEDIATE，
/// 外部改动随后才提交——事务外预读拿到的是改写前的快照，新记录的旧文件
/// 漏清理；事务内读则无论 interleaving 如何都看得到已提交的新值。
/// sleep 只为把外部提交排在 save 启动之后（修复后的断言不依赖该时序）。
#[test]
fn test_overwrite_prior_db_read_sees_external_commit() {
    let tmp = tempfile::TempDir::new().unwrap();
    let conv = tmp.path().join("conversations");
    std::fs::create_dir_all(&conv).unwrap();
    let db_path = tmp.path().join("index.db");
    let db = Db::open(&db_path).unwrap();
    db.init_schema().unwrap();

    let header = make_header();
    let start_ts = crate::util::time::ts_to_unix_ms(&header.start_time).unwrap();
    db.conn()
        .execute(
            "INSERT INTO sessions (session_id, start_ts, file_path, turn_count, created_at, updated_at)
             VALUES (?1, ?2, ?3, 0, 0, 0)",
            rusqlite::params![
                header.session_id,
                start_ts,
                format!("gateway://{}", header.session_id),
            ],
        )
        .unwrap();

    // 外部连接把记录改到一个真实旧文件（非 U5 命名，只能由 DB 记录候选命中）
    let legacy = conv.join("recorded_by_external.jsonl");
    crate::index::conversation::write_session_at(&legacy, &header, &make_turns()).unwrap();
    let external = rusqlite::Connection::open(&db_path).unwrap();
    external.execute_batch("BEGIN IMMEDIATE").unwrap();
    external
        .execute(
            "UPDATE sessions SET file_path = ?1 WHERE session_id = ?2",
            rusqlite::params!["recorded_by_external.jsonl", header.session_id],
        )
        .unwrap();

    let turns = make_turns();
    let handle = std::thread::spawn(move || {
        let store = SessionStore::new(&conv, &db);
        store.save_with_embeddings_mode(&header, &turns, None, SaveMode::Overwrite)
    });
    std::thread::sleep(std::time::Duration::from_millis(300));
    external.execute_batch("COMMIT").unwrap();
    let stats = handle.join().unwrap().unwrap();
    drop(external);

    assert!(
        !legacy.exists(),
        "清理必须按事务内读到的 DB file_path 定位旧文件（W15）"
    );
    assert!(stats.file_path.exists(), "覆盖保存本身必须成功");
}

/// 删除前置校验：U5 之前的命名规则下，前 8 字符相同（"dual-write-test"/
/// "dual-writex"）的两个会话共享旧文件名——候选路径上真实存在的可能是
/// **别的会话**的文件（header session_id 不匹配）。清理必须按 header 校验
/// 身份，外来文件原样保留。
#[test]
fn test_legacy_cleanup_never_touches_foreign_session_file() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();
    let (header, legacy, _) = seed_pre_u5_rest_session(tmp.path(), &db);
    // 把该文件伪装成"同前缀的另一会话"（v2.6.2 前缀碰撞的既成事实）
    let mut foreign_header = header.clone();
    foreign_header.session_id = "dual-writex".to_string(); // 前 8 字符同为 "dual-wri"
    crate::index::conversation::write_session_at(
        &legacy,
        &foreign_header,
        &[Turn {
            ts: "2026-04-10T14:30:05.000+08:00".to_string(),
            seq: 1,
            role: "user".to_string(),
            content: "别家会话的数据".to_string(),
            metadata: None,
        }],
    )
    .unwrap();

    let store = SessionStore::new(tmp.path(), &db);
    store
        .save_with_embeddings_mode(&header, &make_turns(), None, SaveMode::Append)
        .unwrap();

    assert!(
        legacy.exists(),
        "session_id 不匹配的旧命名文件（前缀碰撞）不得被删除或迁移"
    );
    let files = crate::index::conversation::list_sessions(tmp.path());
    assert_eq!(files.len(), 2, "外来文件保留 + 本会话新文件");
    let own = files.iter().find(|f| *f != &legacy).unwrap().clone();
    let (h, turns) = crate::index::conversation::read_session(&own).unwrap();
    assert_eq!(h.session_id, header.session_id);
    assert_eq!(
        turns.iter().map(|t| t.seq).collect::<Vec<_>>(),
        vec![3, 4],
        "不迁移外来内容，只追加本批（DB 已有 1,2，续排 3,4）"
    );
    let (_, foreign_turns) = crate::index::conversation::read_session(&legacy).unwrap();
    assert_eq!(foreign_turns[0].content, "别家会话的数据");
}

#[test]
fn test_tampered_file_path_outside_conversations_dir_never_deleted() {
    let tmp = tempfile::TempDir::new().unwrap();
    let conv_dir = tmp.path().join("conversations");
    std::fs::create_dir_all(&conv_dir).unwrap();
    let victim = tmp.path().join("outside_victim.jsonl");

    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();
    let header = make_header();
    crate::index::conversation::write_session_at(&victim, &header, &make_turns()).unwrap();

    // Point sessions.file_path outside conv_dir via both relative traversal and absolute path
    for bad_rel in [
        "../outside_victim.jsonl".to_string(),
        victim.to_string_lossy().to_string(),
    ] {
        db.conn()
            .execute(
                "INSERT OR REPLACE INTO sessions
                 (session_id, start_ts, file_path, created_at, updated_at)
                 VALUES (?1, 1000, ?2, 1000, 1000)",
                rusqlite::params![header.session_id, bad_rel],
            )
            .unwrap();

        let store = SessionStore::new(&conv_dir, &db);
        store
            .save_with_embeddings_mode(&header, &make_turns(), None, SaveMode::Append)
            .unwrap();
        assert!(
            victim.exists(),
            "Append must never read/delete victim outside conv_dir for {bad_rel}"
        );

        db.conn()
            .execute(
                "UPDATE sessions SET file_path = ?1 WHERE session_id = ?2",
                rusqlite::params![bad_rel, header.session_id],
            )
            .unwrap();
        store.save(&header, &make_turns(), None).unwrap();
        assert!(
            victim.exists(),
            "Overwrite must never delete victim outside conv_dir for {bad_rel}"
        );
    }
}

#[test]
fn test_overwrite_remaps_and_clears_provenance_refs() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();
    let store = SessionStore::new(tmp.path(), &db);

    let header = make_header();
    let turns = make_turns(); // seq 1 and seq 2
    store.save(&header, &turns, None).unwrap();

    let id_seq1: i64 = db
        .conn()
        .query_row(
            "SELECT id FROM turns WHERE session_id = ?1 AND seq = 1",
            rusqlite::params![header.session_id],
            |r| r.get(0),
        )
        .unwrap();
    let id_seq2: i64 = db
        .conn()
        .query_row(
            "SELECT id FROM turns WHERE session_id = ?1 AND seq = 2",
            rusqlite::params![header.session_id],
            |r| r.get(0),
        )
        .unwrap();

    db.conn()
        .execute(
            "INSERT INTO entities (canonical, name, entity_type, first_seen, last_seen, source_turn)
             VALUES ('e1', 'E1', 'concept', 1, 1, ?1), ('e2', 'E2', 'concept', 1, 1, ?2)",
            rusqlite::params![id_seq1, id_seq2],
        )
        .unwrap();
    db.conn()
        .execute(
            "INSERT INTO relations (src_canonical, rel_type, dst_canonical, confidence, source_turn, created_at)
             VALUES ('e1', 'rel', 'e2', 0.9, ?1, 1)",
            rusqlite::params![id_seq1],
        )
        .unwrap();
    db.conn()
        .execute(
            "INSERT INTO bounded_memory (target, content, created_at, updated_at, memory_type, source_turn_ids)
             VALUES ('memory', 'atom fact', 1, 1, 'atom', ?1)",
            rusqlite::params![serde_json::json!([id_seq1, id_seq2, 999999]).to_string()],
        )
        .unwrap();

    // Overwrite keeping ONLY seq 1 (seq 2 removed)
    store.save(&header, &[turns[0].clone()], None).unwrap();
    let new_id_seq1: i64 = db
        .conn()
        .query_row(
            "SELECT id FROM turns WHERE session_id = ?1 AND seq = 1",
            rusqlite::params![header.session_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_ne!(new_id_seq1, id_seq1);

    let e1_turn: Option<i64> = db
        .conn()
        .query_row(
            "SELECT source_turn FROM entities WHERE canonical = 'e1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let e2_turn: Option<i64> = db
        .conn()
        .query_row(
            "SELECT source_turn FROM entities WHERE canonical = 'e2'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let rel_turn: Option<i64> = db
        .conn()
        .query_row(
            "SELECT source_turn FROM relations WHERE src_canonical = 'e1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let bm_ids_json: String = db
        .conn()
        .query_row(
            "SELECT source_turn_ids FROM bounded_memory WHERE content = 'atom fact'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let bm_ids: Vec<i64> = serde_json::from_str(&bm_ids_json).unwrap();

    assert_eq!(e1_turn, Some(new_id_seq1));
    assert_eq!(rel_turn, Some(new_id_seq1));
    assert_eq!(e2_turn, None);
    assert_eq!(bm_ids, vec![new_id_seq1, 999999]);
}

/// C12: json_each 下推只是预筛——坏 JSON / 非 i64 数组（对象/标量/浮点数组）
/// 即便被 EXISTS 误取，Rust 侧 serde parse 仍失败跳过，与原全表扫语义等价；
/// 命中行照常 remap，未命中行原样不动，且整个 Overwrite 不因坏 JSON 回滚。
#[test]
fn test_remap_pushdown_skips_malformed_and_non_array_json() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();
    let store = SessionStore::new(tmp.path(), &db);
    let header = make_header();
    let turns = make_turns(); // seq 1,2
    store.save(&header, &turns, None).unwrap();
    let id_of = |seq: i64| -> i64 {
        db.conn()
            .query_row(
                "SELECT id FROM turns WHERE session_id = ?1 AND seq = ?2",
                rusqlite::params![header.session_id, seq],
                |r| r.get(0),
            )
            .unwrap()
    };
    let id_seq1 = id_of(1);
    let id_seq2 = id_of(2);

    // 命中数组 / 未命中数组 / 坏 JSON / 对象含 stale id / 标量 / 浮点数组含 stale id
    let cases = [
        (
            "hit",
            serde_json::json!([id_seq1, id_seq2, 999999]).to_string(),
        ),
        ("miss", serde_json::json!([999998, 999999]).to_string()),
        ("malformed", "not json".to_string()),
        ("object", format!("{{\"a\":{}}}", id_seq1)),
        ("scalar", format!("{}", id_seq1)),
        ("float_array", format!("[{}.0]", id_seq1)),
    ];
    for (content, json) in &cases {
        db.conn()
            .execute(
                "INSERT INTO bounded_memory (target, content, created_at, updated_at, memory_type, source_turn_ids)
                 VALUES ('memory', ?1, 1, 1, 'atom', ?2)",
                rusqlite::params![content, json],
            )
            .unwrap();
    }

    // Overwrite 只保留 seq1（seq2 变 stale，无映射→丢弃）
    store.save(&header, &[turns[0].clone()], None).unwrap();
    let new_id_seq1 = id_of(1);

    let read = |content: &str| -> String {
        db.conn()
            .query_row(
                "SELECT source_turn_ids FROM bounded_memory WHERE content = ?1",
                [content],
                |r| r.get(0),
            )
            .unwrap()
    };
    // 命中数组：seq1 remap、seq2 丢弃、999999 保留
    let hit: Vec<i64> = serde_json::from_str(&read("hit")).unwrap();
    assert_eq!(hit, vec![new_id_seq1, 999999]);
    // 其余形态原样不动（serde parse 失败或无 stale 命中），且 save 未回滚
    assert_eq!(
        read("miss"),
        serde_json::json!([999998, 999999]).to_string()
    );
    assert_eq!(read("malformed"), "not json");
    assert_eq!(read("object"), format!("{{\"a\":{}}}", id_seq1));
    assert_eq!(read("scalar"), format!("{}", id_seq1));
    assert_eq!(read("float_array"), format!("[{}.0]", id_seq1));
}

/// C12: stale-id 集 >500 时分块 IN——跨块命中的行须被收集且只改写一次
/// （两阶段去重），等价于原单遍全表扫。构造 501 轮旧会话，bm 行分别引用
/// 首块(seq1)+末块(seq501)、仅末块、仅首块的 turn id，Overwrite 仅留 seq1。
#[test]
fn test_remap_pushdown_cross_chunk_boundary() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();
    let store = SessionStore::new(tmp.path(), &db);
    let header = make_header();
    let turns: Vec<Turn> = (1..=501)
        .map(|seq| Turn {
            ts: "2026-04-10T14:30:05.000+08:00".to_string(),
            seq,
            role: "user".to_string(),
            content: format!("turn {seq}"),
            metadata: None,
        })
        .collect();
    store.save(&header, &turns, None).unwrap();
    let id_of = |seq: i64| -> i64 {
        db.conn()
            .query_row(
                "SELECT id FROM turns WHERE session_id = ?1 AND seq = ?2",
                rusqlite::params![header.session_id, seq],
                |r| r.get(0),
            )
            .unwrap()
    };
    let id_seq1 = id_of(1);
    let id_seq501 = id_of(501);
    db.conn()
        .execute(
            "INSERT INTO bounded_memory (target, content, created_at, updated_at, memory_type, source_turn_ids)
             VALUES ('memory','cross',1,1,'atom',?1),('memory','last',1,1,'atom',?2),('memory','first',1,1,'atom',?3)",
            rusqlite::params![
                serde_json::json!([id_seq1, id_seq501, 999999]).to_string(),
                serde_json::json!([id_seq501]).to_string(),
                serde_json::json!([id_seq1]).to_string(),
            ],
        )
        .unwrap();

    // Overwrite 仅留 seq1 → seq501 stale 无映射丢弃，seq1 remap
    store.save(&header, &[turns[0].clone()], None).unwrap();
    let new_id_seq1 = id_of(1);
    let read = |content: &str| -> Vec<i64> {
        let json: String = db
            .conn()
            .query_row(
                "SELECT source_turn_ids FROM bounded_memory WHERE content = ?1",
                [content],
                |r| r.get(0),
            )
            .unwrap();
        serde_json::from_str(&json).unwrap()
    };
    assert_eq!(read("cross"), vec![new_id_seq1, 999999]);
    assert_eq!(read("last"), Vec::<i64>::new());
    assert_eq!(read("first"), vec![new_id_seq1]);
}

#[test]
fn test_save_append_preserves_turn_metadata_in_jsonl() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();
    let store = SessionStore::new(tmp.path(), &db);

    let header = make_header();
    let turns = make_turns();
    let stats = store
        .save_with_embeddings_mode(&header, &turns, None, SaveMode::Append)
        .unwrap();

    let (_, read_back) = crate::index::conversation::read_session(&stats.file_path).unwrap();
    assert_eq!(read_back.len(), 2);
    assert_eq!(read_back[1].metadata, turns[1].metadata);
}

/// Append JSONL seq u32 饱和：DB seq 用 i64 续排（可超 u32），JSONL Turn.seq
/// 是 u32，max_seq+1 溢出时 unwrap_or(u32::MAX) 截断。既有测试无 seq 近 u32::MAX。
/// 纯 SQL 播种（无 JSONL 落盘）→ Append 候选旧文件皆缺，不触发 U5 迁移。
#[test]
fn test_append_jsonl_seq_saturates_at_u32_max() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();
    let store = SessionStore::new(tmp.path(), &db);
    let header = make_header();
    let start_ts = time::ts_to_unix_ms(&header.start_time).unwrap();
    db.conn()
        .execute(
            "INSERT INTO sessions (session_id, start_ts, file_path, turn_count, created_at, updated_at)
             VALUES (?1, ?2, 'seed.jsonl', 1, ?3, ?3)",
            rusqlite::params![header.session_id, start_ts, start_ts],
        )
        .unwrap();
    db.conn()
        .execute(
            "INSERT INTO turns (session_id, seq, timestamp_ms, role, preview)
             VALUES (?1, ?2, ?3, 'user', 'seed')",
            rusqlite::params![header.session_id, u32::MAX as i64, start_ts],
        )
        .unwrap();

    let new_turn = Turn {
        ts: "2026-04-10T14:30:05.000+08:00".to_string(),
        seq: 1, // Append 忽略输入 seq，事务内从 MAX(seq)+1 续排
        role: "assistant".to_string(),
        content: "追加".to_string(),
        metadata: None,
    };
    let stats = store
        .save_with_embeddings_mode(&header, &[new_turn], None, SaveMode::Append)
        .unwrap();

    // DB seq = u32::MAX + 1（i64 不截断）
    let db_seq: i64 = db
        .conn()
        .query_row(
            "SELECT MAX(seq) FROM turns WHERE session_id = ?1",
            rusqlite::params![header.session_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(db_seq, u32::MAX as i64 + 1);
    // JSONL seq 饱和到 u32::MAX
    let (_, jsonl_turns) = crate::index::conversation::read_session(&stats.file_path).unwrap();
    assert_eq!(jsonl_turns.len(), 1);
    assert_eq!(jsonl_turns[0].seq, u32::MAX);
}

/// W10 回归：大小写不敏感 FS 上，与目标文件仅大小写不同的 legacy 候选
/// 指向同一文件——清理不得自删刚写好的目标。规范对规范比较后跳过。
/// （Linux 大小写敏感：别名候选不存在 → exists() 过滤 → 同样保留。）
#[test]
fn test_legacy_cleanup_case_alias_does_not_self_delete() {
    let tmp = tempfile::TempDir::new().unwrap();
    let conv = tmp.path().join("conversations");
    std::fs::create_dir_all(&conv).unwrap();
    let header = make_header();
    let day_dir = conv.join("2026").join("04").join("10");
    std::fs::create_dir_all(&day_dir).unwrap();
    let keep = day_dir.join("20260410T143000_DUAL-WRI.jsonl");
    crate::index::conversation::write_session_at(&keep, &header, &make_turns()).unwrap();

    // 同 session 的真实旧命名文件（不同文件名）应被清理，形成跨平台正反对照
    let distinct_legacy = day_dir.join("20260410T143000_old_legacy.jsonl");
    crate::index::conversation::write_session_at(&distinct_legacy, &header, &make_turns()).unwrap();

    // 仅后缀大小写不同的别名候选（Windows 上与 keep 指同一文件）
    let alias = day_dir.join("20260410T143000_dual-wri.jsonl");
    remove_verified_legacy_jsonl(
        &conv,
        &[alias, distinct_legacy.clone()],
        &keep,
        &header.session_id,
    );

    assert!(keep.exists(), "大小写别名候选不得自删刚写好的目标文件");
    assert!(
        !distinct_legacy.exists(),
        "同 session_id 的真实不同路径旧文件必须被清理"
    );
}

/// W2.7: 追加写中途失败留下半行 JSON 时，rollback_partial_append 截回写前
/// 长度（保 W3 失败批次不留痕），下次 Append 不拼接残行、文件可完整读取。
#[test]
fn test_append_rollback_truncates_partial_write_and_self_heals() {
    let tmp = tempfile::TempDir::new().unwrap();
    let path = tmp.path().join("session.jsonl");
    let header = make_header();
    let turns = make_turns();

    // 先成功写入 header + 第一轮（模拟上次成功的会话文件）
    append_jsonl_turns(&path, &header, &turns[..1]).unwrap();
    let pre_len = std::fs::metadata(&path).unwrap().len();

    // 模拟撕裂写：append 半行 JSON（write_all 中途失败留下的残行）
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        f.write_all(b"{\"ts\":\"2026-").unwrap();
    }
    let torn_len = std::fs::metadata(&path).unwrap().len();
    assert!(torn_len > pre_len, "残行应使文件变长");

    // 生产回滚操作：截回 pre_len
    {
        let f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        rollback_partial_append(&f, pre_len, &path);
    }
    let rolled_len = std::fs::metadata(&path).unwrap().len();
    assert_eq!(rolled_len, pre_len, "回滚必须截回写前长度");

    // 下次 Append 正常自愈：不重写 header、续写第二轮，文件可完整读取
    append_jsonl_turns(&path, &header, &turns[1..]).unwrap();
    let (h, read_turns) = crate::index::conversation::read_session(&path).unwrap();
    assert_eq!(h.session_id, header.session_id);
    assert_eq!(read_turns.len(), 2, "残行已回滚，两轮完整可读");
    assert_eq!(read_turns[0].content, turns[0].content);
    assert_eq!(read_turns[1].content, turns[1].content);
}

/// W2.7: write_all 失败（只读句柄无写权限）时 write_all_or_rollback 返回原始
/// Err，并 best-effort 尝试回滚（只读句柄 set_len 亦失败 → warn，不 panic）。
#[test]
fn test_append_write_failure_returns_err() {
    let tmp = tempfile::TempDir::new().unwrap();
    let path = tmp.path().join("session.jsonl");
    std::fs::write(&path, b"header\n").unwrap();
    let pre_len = std::fs::metadata(&path).unwrap().len();

    let mut ro = std::fs::OpenOptions::new().read(true).open(&path).unwrap();
    write_all_or_rollback(&mut ro, pre_len, b"partial", &path).unwrap_err();
    // write_all 未写入任何字节，文件长度不变
    assert_eq!(std::fs::metadata(&path).unwrap().len(), pre_len);
}

/// M1: Append 向量 insert 失败仅 warn 不回滚——会话数据照常落库、vectors_skipped
/// 置位（对比 Overwrite 的失败即回滚）。删 vec_turns 使 insert 确定性失败。
#[test]
fn test_append_vector_insert_failure_does_not_rollback() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();
    db.conn().execute_batch("DROP TABLE vec_turns;").unwrap();
    let store = SessionStore::new(tmp.path(), &db);

    let mut v = vec![0.0f32; db.dimensions()];
    v[0] = 1.0;
    let header = make_header();
    let stats = store
        .save_with_embeddings_mode(
            &header,
            &make_turns(),
            Some(&[v.clone(), v]),
            SaveMode::Append,
        )
        .expect("向量 insert 失败不得阻断 Append 保存");
    assert!(stats.vectors_skipped, "insert 失败必须置位 vectors_skipped");
    assert_eq!(stats.turns_saved, 2);
    let turn_count: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM turns", [], |r| r.get(0))
        .unwrap();
    assert_eq!(turn_count, 2, "会话数据必须照常落库（不回滚）");
}

/// M10: embeddings 切片短于 turns（生产不可达）时 Overwrite/Append 均透出
/// vectors_skipped 且不失败。
#[test]
fn test_short_embeddings_slice_flags_skipped() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();
    let store = SessionStore::new(tmp.path(), &db);
    let mut v = vec![0.0f32; db.dimensions()];
    v[0] = 1.0;
    let header = make_header();
    let turns = make_turns(); // 2 turns，只给 1 个向量

    let ovr = store
        .save_with_embeddings_mode(&header, &turns, Some(&[v.clone()]), SaveMode::Overwrite)
        .unwrap();
    assert!(ovr.vectors_skipped, "Overwrite 短切片必须置位");
    let app = store
        .save_with_embeddings_mode(&header, &turns, Some(&[v]), SaveMode::Append)
        .unwrap();
    assert!(app.vectors_skipped, "Append 短切片必须置位");
}

/// 空 turns 切片保存：Overwrite/Append 均 Ok、turns_saved=0、JSONL 仅 header。
#[test]
fn test_empty_turns_slice_saves_header_only() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();
    let store = SessionStore::new(tmp.path(), &db);
    let header = make_header();

    let ovr = store
        .save_with_embeddings_mode(&header, &[], None, SaveMode::Overwrite)
        .unwrap();
    assert_eq!(ovr.turns_saved, 0);
    let (h, turns) = crate::index::conversation::read_session(&ovr.file_path).unwrap();
    assert_eq!(h.session_id, header.session_id);
    assert!(turns.is_empty(), "空 turns → JSONL 仅 header");

    let app = store
        .save_with_embeddings_mode(&header, &[], None, SaveMode::Append)
        .unwrap();
    assert_eq!(app.turns_saved, 0);
    let (_, turns2) = crate::index::conversation::read_session(&app.file_path).unwrap();
    assert!(turns2.is_empty());
}

/// M18/W1.3: 坏 turn.ts 解析失败回落 start_ts（warn 由 turn_ts_ms_or_warn 发出），
/// timestamp_ms 落库为 start_ts——Append 与 Overwrite 两路一致。
#[test]
fn test_bad_turn_ts_falls_back_to_start_ts() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();
    let store = SessionStore::new(tmp.path(), &db);
    let header = make_header();
    let start_ts = crate::util::time::ts_to_unix_ms(&header.start_time).unwrap();
    let bad = vec![Turn {
        ts: "not-a-timestamp".to_string(),
        seq: 1,
        role: "user".to_string(),
        content: "坏时间戳".to_string(),
        metadata: None,
    }];

    store
        .save_with_embeddings_mode(&header, &bad, None, SaveMode::Append)
        .unwrap();
    let app_ts: i64 = db
        .conn()
        .query_row("SELECT timestamp_ms FROM turns", [], |r| r.get(0))
        .unwrap();
    assert_eq!(app_ts, start_ts, "Append 坏 ts 必须回落 start_ts");

    store
        .save_with_embeddings_mode(&header, &bad, None, SaveMode::Overwrite)
        .unwrap();
    let ovr_ts: i64 = db
        .conn()
        .query_row("SELECT timestamp_ms FROM turns", [], |r| r.get(0))
        .unwrap();
    assert_eq!(ovr_ts, start_ts, "Overwrite 坏 ts 必须回落 start_ts");
}

/// M17: 空 content 的 turn 在嵌入前被过滤——全部 turn 空 content 时
/// embed_documents 收到空切片（api.embed_batch 对空输入即时 Ok，不触网），
/// 故即使 embedder 指向不可达端点，save 仍 Ok 且 vectors_skipped=false
/// （对比 test_save_degrades_when_embedder_fails 的非空 content → true）。
#[test]
fn test_empty_content_filtered_before_embedding() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();

    let mut emb_cfg = crate::config::Config::default().embedding;
    emb_cfg.api_url = "http://127.0.0.1:9/v1".to_string();
    emb_cfg.api_model = "unreachable-test".to_string();
    let embedder = crate::embedder::LazyEmbedder::from_config(&emb_cfg).unwrap();

    let store = SessionStore::new(tmp.path(), &db);
    let header = make_header();
    let empty_turns = vec![
        Turn {
            ts: "2026-04-10T14:30:05.000+08:00".to_string(),
            seq: 1,
            role: "user".to_string(),
            content: String::new(),
            metadata: None,
        },
        Turn {
            ts: "2026-04-10T14:30:10.000+08:00".to_string(),
            seq: 2,
            role: "assistant".to_string(),
            content: String::new(),
            metadata: None,
        },
    ];
    let stats = store
        .save(&header, &empty_turns, Some(&embedder))
        .expect("空 content 过滤后不触网，save 必须 Ok");
    assert!(
        !stats.vectors_skipped,
        "空 content 被过滤 → embed_documents 收空切片即时 Ok，不置位"
    );
    assert_eq!(stats.turns_saved, 2);
    let vec_count: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM vec_turns", [], |r| r.get(0))
        .unwrap();
    assert_eq!(vec_count, 0, "空 content turn 无向量（W13 守卫跳过空 Vec）");
    let turn_count: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM turns", [], |r| r.get(0))
        .unwrap();
    assert_eq!(turn_count, 2, "会话数据照常落库");
}

/// C5 回归：delete-turn 造成部分缺口后，重发与 JSONL 尾部全等的批次不得被误判为
/// 重放——尾批起始 seq 必须**严格领先于** DB MAX(seq) 才复用区间。否则
/// first <= db_max_seq 会复用撞号 seq 区间，重复 seq 或复活已删 turn。
#[test]
fn test_append_after_delete_turn_does_not_resurrect_or_duplicate_seq() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();
    let store = SessionStore::new(tmp.path(), &db);
    let header = make_header();

    // DB + JSONL: seq 1,2,3
    store
        .save_append_idempotent(
            &header,
            &[turn_at(1, "甲"), turn_at(2, "乙"), turn_at(3, "丙")],
            None,
            None,
        )
        .unwrap();
    // delete-turn seq 2（中间）→ DB 1,3
    db.conn()
        .execute(
            "DELETE FROM turns WHERE session_id = ?1 AND seq = 2",
            rusqlite::params![header.session_id],
        )
        .unwrap();
    // 追加新 turn → seq 4（DB 1,3,4；JSONL 1,2,3,4）
    store
        .save_append_idempotent(&header, &[turn_at(1, "丁")], None, None)
        .unwrap();
    // delete-turn seq 4 → DB 1,3（db_max 3）；JSONL 1,2,3,4（jsonl_max 4 > 3）
    db.conn()
        .execute(
            "DELETE FROM turns WHERE session_id = ?1 AND seq = 4",
            rusqlite::params![header.session_id],
        )
        .unwrap();
    // 重发与 JSONL 尾部 [3丙,4丁] 全等的批次：first=3 <= db_max 3 → 非重放
    store
        .save_append_idempotent(&header, &[turn_at(1, "丙"), turn_at(1, "丁")], None, None)
        .unwrap();

    let seqs: Vec<i64> = {
        let mut stmt = db
            .conn()
            .prepare("SELECT seq FROM turns WHERE session_id = ?1 ORDER BY seq")
            .unwrap();
        stmt.query_map(rusqlite::params![header.session_id], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    };
    assert_eq!(seqs, vec![1, 3, 5, 6], "不得重复 seq 或复活已删 turn");
    let n4: i64 = db
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM turns WHERE session_id = ?1 AND seq = 4",
            rusqlite::params![header.session_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n4, 0, "已删除的 seq 4 不得被复活");
}

/// C5 回归：非连续的 JSONL 尾批（部分缺口）即便起始 seq 领先于 DB 也不得认定为
/// 重放——复用 first..first+N 会与文件里已有的非连续 seq 撞号。
#[test]
fn test_append_non_contiguous_tail_not_replay() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();
    let store = SessionStore::new(tmp.path(), &db);
    let header = make_header();

    store
        .save_append_idempotent(&header, &[turn_at(1, "甲")], None, None)
        .unwrap();
    let path = crate::fact::conversation::compute_session_path(tmp.path(), &header).unwrap();
    // 外部向 JSONL 追加非连续 seq 3,5（缺口），DB 仍只有 seq 1
    append_jsonl_turns(&path, &header, &[turn_at(3, "乙"), turn_at(5, "丙")]).unwrap();
    // 重发与尾部 [3乙,5丙] 全等的批次：first=3 > db_max 1 但非连续 → 非重放
    store
        .save_append_idempotent(&header, &[turn_at(1, "乙"), turn_at(1, "丙")], None, None)
        .unwrap();

    let seqs: Vec<i64> = {
        let mut stmt = db
            .conn()
            .prepare("SELECT seq FROM turns WHERE session_id = ?1 ORDER BY seq")
            .unwrap();
        stmt.query_map(rusqlite::params![header.session_id], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    };
    assert_eq!(seqs, vec![1, 6, 7], "非连续尾批不得复用撞号 seq 区间");
    let (_, jsonl_turns) = crate::fact::conversation::read_session(&path).unwrap();
    let mut uniq: Vec<u32> = jsonl_turns.iter().map(|t| t.seq).collect();
    uniq.sort_unstable();
    uniq.dedup();
    assert_eq!(uniq.len(), jsonl_turns.len(), "JSONL 不得出现重复 seq");
}

/// C6 回归：DB 事务在 COMMIT 前失败时，legacy pre-U5 旧文件必须原样保留——
/// 旧实现在事务内删除 stale_files，COMMIT 失败会永久丢失旧文件（可能是该会话
/// 唯一的真相源副本）。用延迟外键触发器让 COMMIT 确定性失败。
#[test]
fn test_legacy_files_survive_failed_commit() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();
    let (header, legacy, _old_turns) = seed_pre_u5_rest_session(tmp.path(), &db);

    // 延迟外键陷阱：turns 每次 INSERT 触发插入一条引用不存在 turn 的子行，
    // 外键延迟到 COMMIT 才校验 → COMMIT 必失败（JSONL 已在事务内写盘）。
    db.conn()
        .execute_batch(
            "CREATE TABLE c6_trap (id INTEGER PRIMARY KEY,
                 turn_id INTEGER REFERENCES turns(id) DEFERRABLE INITIALLY DEFERRED);
             CREATE TRIGGER c6_trap_ai AFTER INSERT ON turns
             BEGIN INSERT INTO c6_trap(turn_id) VALUES (-1); END;",
        )
        .unwrap();

    let store = SessionStore::new(tmp.path(), &db);
    let appended = turn_at(99, "升级后追加");
    let res = store.save_append_idempotent(&header, &[appended], None, Some("batch-c6"));
    assert!(res.is_err(), "延迟外键必须让 COMMIT 失败");

    // 旧文件原样保留（删除严格在 commit 之后，commit 失败不得触发）
    assert!(legacy.exists(), "COMMIT 失败时 legacy 旧文件必须原样保留");
    // DB 回滚：新批次不得留下 turns / 幂等占位
    let n: i64 = db
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM turns WHERE preview LIKE '%升级后追加%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 0, "失败批次不得留下新 turns");
    let dedup: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM capture_dedup", [], |r| r.get(0))
        .unwrap();
    assert_eq!(dedup, 0, "失败批次不得烧掉 batch_id 幂等键");
}

/// W14b 回归：仅含完整 header 但缺结尾换行的文件，repair_torn_jsonl_tail 必须
/// 保留 header 并标记补 '\n'（返回 (len, true)），不得截到 0 销毁 header。
#[test]
fn test_repair_preserves_header_only_without_newline() {
    let tmp = tempfile::TempDir::new().unwrap();
    let path = tmp.path().join("header_only.jsonl");
    let header = make_header();
    let bytes = serde_json::to_string(&header).unwrap();
    std::fs::write(&path, &bytes).unwrap(); // 无结尾 '\n'
    let len = bytes.len() as u64;

    let (new_len, needs_nl) = repair_torn_jsonl_tail(&path, len);
    assert_eq!(new_len, len, "完整 header 不得被截断");
    assert!(needs_nl, "应标记补一个换行");
    assert_eq!(std::fs::metadata(&path).unwrap().len(), len);

    // 追加 turn 后 header 仍在、会话可完整读取
    append_jsonl_turns(&path, &header, &[turn_at(1, "第一条")]).unwrap();
    let (h, turns) = crate::fact::conversation::read_session(&path).unwrap();
    assert_eq!(h.session_id, header.session_id, "header 身份必须保留");
    assert_eq!(turns.len(), 1);
}

/// W14b 回归：尾部超过 16 MiB 仍无换行时，回扫达上限即安全返回 (len, false)，
/// 不截断、不把整段尾巴塞进内存（防 OOM）。
#[test]
fn test_repair_caps_backward_scan_at_16mib() {
    let tmp = tempfile::TempDir::new().unwrap();
    let path = tmp.path().join("huge_torn.jsonl");
    let header = make_header();
    let mut data = serde_json::to_string(&header).unwrap();
    data.push('\n');
    data.push_str(&"x".repeat(17 * 1024 * 1024)); // 17 MiB 无换行残缺尾部
    std::fs::write(&path, &data).unwrap();
    let len = data.len() as u64;

    let (new_len, needs_nl) = repair_torn_jsonl_tail(&path, len);
    assert_eq!(new_len, len, "超上限不得截断");
    assert!(!needs_nl);
    assert_eq!(
        std::fs::metadata(&path).unwrap().len(),
        len,
        "文件必须原样保留（不截断）"
    );
}

/// M7 回归：Overwrite 覆盖已有会话必须保留首次 created_at；输入 turns 含重复
/// seq 时按"保留先见者"去重入库（与 rebuild 同口径）。
#[test]
fn test_overwrite_preserves_created_at_and_dedups_seq() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();
    let store = SessionStore::new(tmp.path(), &db);
    let header = make_header();

    store
        .save_with_embeddings(&header, &[turn_at(1, "首轮")], None)
        .unwrap();
    let created_at: i64 = db
        .conn()
        .query_row(
            "SELECT created_at FROM sessions WHERE session_id = ?1",
            rusqlite::params![header.session_id],
            |r| r.get(0),
        )
        .unwrap();

    // 覆盖：重复 seq 1（保留先见者 "甲"），seq 2 正常
    store
        .save_with_embeddings(
            &header,
            &[turn_at(1, "甲"), turn_at(1, "乙"), turn_at(2, "丙")],
            None,
        )
        .unwrap();
    let created_at2: i64 = db
        .conn()
        .query_row(
            "SELECT created_at FROM sessions WHERE session_id = ?1",
            rusqlite::params![header.session_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(created_at2, created_at, "覆盖必须保留首次 created_at");

    let rows: Vec<(i64, String)> = {
        let mut stmt = db
            .conn()
            .prepare("SELECT seq, preview FROM turns WHERE session_id = ?1 ORDER BY seq")
            .unwrap();
        stmt.query_map(rusqlite::params![header.session_id], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
    };
    assert_eq!(rows.len(), 2, "重复 seq 必须去重（保留先见者）");
    assert_eq!(rows[0], (1, "甲".to_string()));
    assert_eq!(rows[1], (2, "丙".to_string()));
}

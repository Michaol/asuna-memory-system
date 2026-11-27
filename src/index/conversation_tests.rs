use super::{
    compute_legacy_session_path, compute_session_path, list_sessions, read_session,
    read_session_summary, try_list_sessions, write_session, write_session_at, SessionHeader, Turn,
};
use chrono::Utc;

fn make_test_header() -> SessionHeader {
    SessionHeader {
        v: 1,
        header_type: "session_header".to_string(),
        session_id: "test-session-abc123".to_string(),
        start_time: "2026-04-10T10:02:00.000+08:00".to_string(),
        profile_id: "default".to_string(),
        source: Some("test".to_string()),
        agent_model: Some("test-model".to_string()),
        title: None,
        tags: vec![],
    }
}

fn make_test_turns() -> Vec<Turn> {
    vec![
        Turn {
            ts: "2026-04-10T10:02:05.123+08:00".to_string(),
            seq: 1,
            role: "user".to_string(),
            content: "你好".to_string(),
            metadata: None,
        },
        Turn {
            ts: "2026-04-10T10:02:07.456+08:00".to_string(),
            seq: 2,
            role: "assistant".to_string(),
            content: "你好！有什么可以帮助你的？".to_string(),
            metadata: Some(serde_json::json!({
                "model": "test-model",
                "usage": {"input_tokens": 10, "output_tokens": 20}
            })),
        },
    ]
}

#[test]
fn test_write_and_read_session() {
    let tmp = std::env::temp_dir().join(format!(
        "asuna_test_{}",
        Utc::now().timestamp_nanos_opt().unwrap_or(0)
    ));
    std::fs::create_dir_all(&tmp).unwrap();

    let header = make_test_header();
    let turns = make_test_turns();

    let path = write_session(&tmp, &header, &turns).unwrap();
    assert!(path.exists());

    let (read_header, read_turns) = read_session(&path).unwrap();
    assert_eq!(read_header.session_id, header.session_id);
    assert_eq!(read_header.start_time, header.start_time);
    assert_eq!(read_turns.len(), 2);
    assert_eq!(read_turns[0].content, "你好");
    assert_eq!(read_turns[1].role, "assistant");
    assert!(read_turns[1].metadata.is_some());

    // 清理
    std::fs::remove_dir_all(&tmp).unwrap();
}

#[test]
fn test_directory_structure() {
    let tmp = std::env::temp_dir().join(format!(
        "asuna_test_dir_{}",
        Utc::now().timestamp_nanos_opt().unwrap_or(0)
    ));
    std::fs::create_dir_all(&tmp).unwrap();

    let header = make_test_header();
    let turns = make_test_turns();

    let path = write_session(&tmp, &header, &turns).unwrap();

    // 验证年/月/日目录
    assert!(path.to_string_lossy().contains("2026"));
    assert!(path.to_string_lossy().contains("04"));
    assert!(path.to_string_lossy().contains("10"));

    // 验证文件名格式
    let filename = path.file_name().unwrap().to_string_lossy();
    assert!(filename.starts_with("20260410T100200_"));
    assert!(filename.ends_with(".jsonl"));

    std::fs::remove_dir_all(&tmp).unwrap();
}

#[test]
fn test_list_sessions() {
    let tmp = std::env::temp_dir().join(format!(
        "asuna_test_list_{}",
        Utc::now().timestamp_nanos_opt().unwrap_or(0)
    ));
    std::fs::create_dir_all(&tmp).unwrap();

    let header = make_test_header();
    let turns = make_test_turns();

    write_session(&tmp, &header, &turns).unwrap();

    let sessions = list_sessions(&tmp);
    assert_eq!(sessions.len(), 1);

    std::fs::remove_dir_all(&tmp).unwrap();
}

/// U5 回归：文件名不再用 session_id 前 8 字符（"session-1"/"session-2"
/// 共享前 8 字符 + 同秒 → 同路径互相覆盖），改用 sha256 前 8 位 hex。
#[test]
fn test_compute_session_path_disambiguates_shared_prefix() {
    let tmp = std::env::temp_dir().join(format!(
        "asuna_test_prefix_collision_{}",
        Utc::now().timestamp_nanos_opt().unwrap_or(0)
    ));

    let mut h1 = make_test_header();
    h1.session_id = "prefix-aaa-1".to_string();
    let mut h2 = make_test_header();
    h2.session_id = "prefix-aaa-2".to_string();
    // 两者共享前 8 字符 "prefix-a"，旧实现同名互相覆盖

    let p1 = compute_session_path(&tmp, &h1).unwrap();
    let p2 = compute_session_path(&tmp, &h2).unwrap();
    assert_ne!(p1, p2, "不同 session_id 同秒不得撞同一文件名");
    assert_eq!(p1.parent(), p2.parent(), "日期目录仍相同");
    assert_ne!(
        p1.file_name(),
        p2.file_name(),
        "旧前 8 字符命名会给出相同文件名"
    );

    std::fs::remove_dir_all(&tmp).ok();
}

/// 覆盖语义保留：同一 session_id 同一秒 → 同一路径（重存覆盖本会话文件）。
#[test]
fn test_compute_session_path_stable_for_same_id_and_time() {
    let tmp = std::env::temp_dir().join(format!(
        "asuna_test_stable_{}",
        Utc::now().timestamp_nanos_opt().unwrap_or(0)
    ));
    let h1 = make_test_header();
    let mut h2 = make_test_header();
    h2.title = Some("另一个标题".to_string()); // 无关字段变化不得影响路径
    assert_eq!(
        compute_session_path(&tmp, &h1).unwrap(),
        compute_session_path(&tmp, &h2).unwrap()
    );
    std::fs::remove_dir_all(&tmp).ok();
}

/// U5 升级缝隙回归：旧命名路径必须逐位复原 v2.6.2 `compute_session_path`
/// 的产物（日期目录与紧凑时间串相同，后缀为 session_id 前 8 字符），
/// SessionStore 的迁移/清理据此定位旧文件。
#[test]
fn test_compute_legacy_session_path_reproduces_pre_u5_naming() {
    let tmp = std::env::temp_dir().join(format!(
        "asuna_test_legacy_{}",
        Utc::now().timestamp_nanos_opt().unwrap_or(0)
    ));
    let h = make_test_header(); // session_id "test-session-abc123" → 前8字符 "test-ses"

    let legacy = compute_legacy_session_path(&tmp, &h.session_id, &h.start_time).unwrap();
    let legacy_lossy = legacy.to_string_lossy().replace('\\', "/");
    assert!(
        legacy_lossy.ends_with("/2026/04/10/20260410T100200_test-ses.jsonl"),
        "旧命名复原失败: {legacy_lossy}"
    );
    // 与新命名同目录、不同后缀（这正是升级后旧文件逃过清理的机制）
    let new = compute_session_path(&tmp, &h).unwrap();
    assert_eq!(legacy.parent(), new.parent());
    assert_ne!(legacy.file_name(), new.file_name());

    std::fs::remove_dir_all(&tmp).ok();
}

/// W5.3 回归：轻量摘要与 read_session 的 header/轮数口径一致（空行跳过）。
#[test]
fn test_read_session_summary_matches_full_read() {
    let tmp = std::env::temp_dir().join(format!(
        "asuna_summary_{}",
        Utc::now().timestamp_nanos_opt().unwrap_or(0)
    ));
    std::fs::create_dir_all(&tmp).unwrap();
    let header = make_test_header();
    let turns = make_test_turns();
    let path = write_session(&tmp, &header, &turns).unwrap();
    // 追加一个空行（read_session 跳过空行，摘要也须跳过）
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    f.write_all(b"\n").unwrap();
    drop(f);

    let (h, n) = read_session_summary(&path).unwrap();
    assert_eq!(h.session_id, header.session_id);
    assert_eq!(n, 2);
    let (_, full_turns) = read_session(&path).unwrap();
    assert_eq!(n, full_turns.len(), "摘要计数与全量读取一致");

    std::fs::remove_dir_all(&tmp).unwrap();
}

/// W5.3 口径：摘要只校验 header，turn 行仅计数不解析——坏行文件由 rebuild
/// 的 read_session 捕获；header 坏/空文件仍上抛（触发完整重建）。
#[test]
fn test_read_session_summary_bad_turn_line_and_empty_file() {
    let tmp = std::env::temp_dir().join(format!(
        "asuna_summary_bad_{}",
        Utc::now().timestamp_nanos_opt().unwrap_or(0)
    ));
    std::fs::create_dir_all(&tmp).unwrap();
    let header = make_test_header();
    let path = write_session(&tmp, &header, &make_test_turns()).unwrap();
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    f.write_all(b"garbage line\n").unwrap();
    drop(f);

    // W14a: 坏 turn 行若为**末行**（剩余行全空）按"被中断的追加残行"容忍
    // （warn+丢弃、不计数），与 read_session 口径一致——否则摘要恒比 DB 多 1，
    // 增量检测恒判不一致，启动陷入无限完整重建循环。
    let (_, n) = read_session_summary(&path).unwrap();
    assert_eq!(n, 2, "残缺末行不计数，与 read_session 一致");
    let (_, turns) = read_session(&path).expect("末行残缺应被容忍");
    assert_eq!(turns.len(), 2, "残缺末行被丢弃，完整 turn 保留");

    // 中间行损坏仍整文件 Err（无法判定是残缺还是真损坏，保守）
    let mid = tmp.join("mid_bad.jsonl");
    let good = std::fs::read_to_string(&path).unwrap();
    let mut lines: Vec<&str> = good.lines().collect();
    let last = lines.pop().unwrap();
    lines.push("garbage middle line");
    lines.push(last);
    std::fs::write(&mid, lines.join("\n") + "\n").unwrap();
    assert!(read_session(&mid).is_err(), "中间行损坏必须整文件报错");
    // W14a: 中间坏行摘要仍计数（2 完整 + 1 中间坏行；末尾残行不计数）
    let (_, mid_n) = read_session_summary(&mid).unwrap();
    assert_eq!(mid_n, 3, "中间坏行仍计数以触发完整重建");

    // 空文件：摘要上抛
    let empty = tmp.join("empty.jsonl");
    std::fs::write(&empty, "").unwrap();
    assert!(read_session_summary(&empty).is_err());

    std::fs::remove_dir_all(&tmp).unwrap();
}

/// W14a 回归：SIGKILL 残缺尾行（半截 JSON、无换行）的摘要计数必须与
/// read_session 完全一致——尾部空行不影响"剩余全空"判定。
#[test]
fn test_read_session_summary_torn_final_line_matches_read_session() {
    let tmp = std::env::temp_dir().join(format!(
        "asuna_w14a_torn_{}",
        Utc::now().timestamp_nanos_opt().unwrap_or(0)
    ));
    std::fs::create_dir_all(&tmp).unwrap();
    let header = make_test_header();
    let path = write_session(&tmp, &header, &make_test_turns()).unwrap();

    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    f.write_all(b"{\"ts\":\"2026-04-10T10:02:0").unwrap();
    drop(f);

    let (_, n) = read_session_summary(&path).unwrap();
    let (_, turns) = read_session(&path).unwrap();
    assert_eq!(n, turns.len(), "残缺尾行摘要计数必须与全量读取一致");
    assert_eq!(n, 2);

    // 残行后再追加空行：仍判定为残缺末行（剩余行全空）
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    f.write_all(b"\n\n").unwrap();
    drop(f);
    let (_, n2) = read_session_summary(&path).unwrap();
    assert_eq!(n2, 2, "尾部空行不改变残缺末行判定");

    std::fs::remove_dir_all(&tmp).unwrap();
}

/// D2-5 回归：紧凑 start_time（ts_to_unix_ms 可解析）也必须能算出路径，
/// 与时间解析器对称。
#[test]
fn test_compute_session_path_accepts_compact_start_time() {
    let tmp = std::env::temp_dir().join(format!(
        "asuna_compact_{}",
        Utc::now().timestamp_nanos_opt().unwrap_or(0)
    ));
    let mut h = make_test_header();
    h.start_time = "20260410T100200".to_string();
    assert!(crate::util::time::ts_to_unix_ms(&h.start_time).is_ok());
    let p = compute_session_path(&tmp, &h).unwrap();
    let lossy = p.to_string_lossy().replace('\\', "/");
    assert!(
        lossy.contains("/2026/04/10/20260410T100200_"),
        "紧凑 start_time 路径计算失败: {lossy}"
    );
    std::fs::remove_dir_all(&tmp).ok();
}

/// D7-5 回归：.JSONL 大写扩展名同样被列出（Windows 大小写不敏感）。
#[test]
fn test_list_sessions_uppercase_extension() {
    let tmp = std::env::temp_dir().join(format!(
        "asuna_upper_{}",
        Utc::now().timestamp_nanos_opt().unwrap_or(0)
    ));
    std::fs::create_dir_all(&tmp).unwrap();
    std::fs::write(tmp.join("upper.JSONL"), "{}\n").unwrap();
    std::fs::write(tmp.join("lower.jsonl"), "{}\n").unwrap();
    std::fs::write(tmp.join("notjson.txt"), "{}\n").unwrap();
    let files = try_list_sessions(&tmp).unwrap();
    assert_eq!(files.len(), 2, "大小写扩展名都须列出: {files:?}");
    std::fs::remove_dir_all(&tmp).unwrap();
}

/// D7-5 回归：极深目录树不再无限递归（超过深度上限的子树跳过，不栈溢出）。
#[test]
fn test_list_sessions_depth_cap() {
    let tmp = std::env::temp_dir().join(format!(
        "asuna_depth_{}",
        Utc::now().timestamp_nanos_opt().unwrap_or(0)
    ));
    std::fs::create_dir_all(&tmp).unwrap();
    let mut deep = tmp.join("deep");
    for i in 0..20 {
        deep = deep.join(format!("d{i}"));
    }
    std::fs::create_dir_all(&deep).unwrap();
    std::fs::write(deep.join("buried.jsonl"), "{}\n").unwrap();
    // 浅层正常文件仍被列出
    std::fs::write(tmp.join("shallow.jsonl"), "{}\n").unwrap();

    let files = try_list_sessions(&tmp).unwrap();
    assert_eq!(files.len(), 1, "超深子树跳过，浅层文件保留: {files:?}");
    assert!(files[0].ends_with("shallow.jsonl"));

    std::fs::remove_dir_all(&tmp).unwrap();
}

/// W6 回归：同文件存在重复 seq 时，read_session_summary 按 seq 去重后的轮次计数
/// 须与 rebuild（insert_session_turns 去重）口径一致，避免增量检测恒判不一致。
#[test]
fn test_read_session_summary_dedups_duplicate_seq() {
    let tmp = std::env::temp_dir().join(format!(
        "asuna_w6_seqdup_{}",
        Utc::now().timestamp_nanos_opt().unwrap_or(0)
    ));
    std::fs::create_dir_all(&tmp).unwrap();
    let file = tmp.join("dup.jsonl");
    let mut h = make_test_header();
    h.session_id = "w6-dup".to_string();
    let mut t1 = make_test_turns();
    // 插入重复 seq=1 的第 3 条 turn（实际唯一 seq 只有 1 与 2）
    let mut dup = t1[0].clone();
    dup.content = "duplicate seq=1".to_string();
    t1.push(dup);

    write_session_at(&file, &h, &t1).unwrap();
    let (parsed_header, turn_count) = read_session_summary(&file).unwrap();
    assert_eq!(parsed_header.session_id, "w6-dup");
    assert_eq!(turn_count, 2, "重复 seq 须去重计数，只算 2 轮");

    std::fs::remove_dir_all(&tmp).unwrap();
}

/// Phase 4 (`T4.2`) 已知向量回归：锁定 sha2 升级前后 `<hash8>` 磁盘命名契约逐字节不变。
/// 独立基准（`sha256("test-session-abc123")`）：
/// `32745a5316e4230400a2aa062ead0b7db3ff264c1b58e339f032494b7f6e44f9` → 前 8 hex `32745a53`。
#[test]
fn test_compute_session_path_known_sha256_hash8_vector() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mut header = make_test_header();
    header.session_id = "test-session-abc123".to_string();
    header.start_time = "2026-05-01T12:34:56Z".to_string();

    let path = compute_session_path(tmp.path(), &header).unwrap();
    let lossy = path.to_string_lossy().replace('\\', "/");
    assert!(
        lossy.ends_with("/2026/05/01/20260501T123456_32745a53.jsonl"),
        "expected canonical hash8 path suffix, got {lossy}"
    );
    assert_eq!(
        path.file_name().and_then(std::ffi::OsStr::to_str),
        Some("20260501T123456_32745a53.jsonl")
    );
}

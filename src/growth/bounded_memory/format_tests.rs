use super::*;
use crate::index::db::Db;

/// F10: 驱逐候选查询的行解码失败必须上抛，不能静默丢行——被丢的 atom
/// 逃出淘汰核算，容量账目与 MEMORY.md 就此失真。
#[test]
fn test_prepare_sync_atoms_propagates_row_decode_error() {
    let dir = std::env::temp_dir().join(format!(
        "asuna_growth_format_{}_{}",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();
    // atom budget = 500 * 0.3 = 150 chars
    let bm = BoundedMemory::new(&dir, &db, 500, 200).with_atom_capacity_ratio(0.3);

    let now = crate::util::time::now_unix_ms();
    // 活 atom 200 chars > budget：健康路径下必被驱逐。
    db.conn()
        .execute(
            "INSERT INTO bounded_memory (target, content, created_at, updated_at, confidence, memory_type)
             VALUES ('memory', ?1, ?2, ?2, 'medium', 'atom')",
            rusqlite::params!["x".repeat(200), now],
        )
        .unwrap();
    // BLOB 存进 TEXT 列（SQLite 动态类型允许，FTS 触发器按文本照常索引），
    // 但 row.get::<String> 解码失败——正是 filter_map(Result::ok) 会吞掉的行错。
    db.conn()
        .execute(
            "INSERT INTO bounded_memory (target, content, created_at, updated_at, confidence, memory_type)
             VALUES ('memory', CAST(?1 AS BLOB), ?2, ?2, 'medium', 'atom')",
            rusqlite::params!["undecodable_atom", now + 1],
        )
        .unwrap();

    assert!(
        bm.prepare_sync_atoms_to_md().is_err(),
        "行解码失败必须上抛，而不是静默丢掉该 atom"
    );
    let remaining: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM bounded_memory", [], |r| r.get(0))
        .unwrap();
    assert_eq!(remaining, 2, "上抛路径不得发生任何驱逐");

    std::fs::remove_dir_all(&dir).unwrap();
}

/// W2.3 回归：守卫内部归一 CRLF——L1（plan.rs）用原始 atom.content 调
/// has_entry_separator，CRLF 分隔符变体必须与 LF 变体一样被拦，否则绕过守卫
/// 入库后 render_md_from_db 伪分隔。普通 CRLF 多行（无分隔符）与合法中缀 § 不得误拒。
#[test]
fn test_has_entry_separator_rejects_crlf_variants() {
    // CRLF 分隔符变体：完整 / 尾截断 / 头截断
    assert!(has_entry_separator("a\r\n§\r\nb"));
    assert!(has_entry_separator("a\r\n§"));
    assert!(has_entry_separator("§\r\nb"));
    // LF canonical 变体仍被拦（回归）
    assert!(has_entry_separator("a\n§\nb"));
    assert!(has_entry_separator("a\n§"));
    assert!(has_entry_separator("§\nb"));
    // 普通 CRLF 多行（无分隔符）不是分隔符变体——reconcile 两侧归一已覆盖，守卫不得误拒
    assert!(!has_entry_separator("line1\r\nline2"));
    // 合法中缀 § 与干净内容不受影响
    assert!(!has_entry_separator("see §5 fine"));
    assert!(!has_entry_separator("干净原子"));
}

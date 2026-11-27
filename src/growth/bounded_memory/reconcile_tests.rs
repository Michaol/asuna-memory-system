use super::*;
use crate::index::db::Db;

/// 维度2: reinsert 的 active dup 复查必须在 IMMEDIATE tx 内——即便某条目在
/// tx 外的 reconcile_check 之后被并发写入成活行，reinsert 也不得再插重复。
/// 这里直接喂一个已存在活行的条目给 reinsert_md_only_entries，钉住 tx 内复查
/// 跳过它（对比 write 的 NEW-B-growth tx 内复查）。
#[test]
fn test_reinsert_skips_active_duplicate_in_tx() {
    let dir = std::env::temp_dir().join(format!(
        "asuna_growth_reinsert_dup_{}_{}",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();
    let bm = BoundedMemory::new(&dir, &db, 2200, 1375);

    // 先写一条活行
    bm.write("memory", "已存在活条目", "high", None).unwrap();

    // 直接把同内容条目喂给 reinsert（模拟 check→reinsert 间隙里并发写入）
    let entries = ["已存在活条目".to_string()];
    let (inserted, _stale) = bm.reinsert_md_only_entries("memory", &entries).unwrap();
    assert_eq!(inserted, 0, "tx 内 active dup 复查必须跳过已存在活行");

    let count: i64 = db
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM bounded_memory WHERE target='memory'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 1, "不得插入重复活行");

    std::fs::remove_dir_all(&dir).unwrap();
}

/// W2.3 回归：检测面归一——历史 CRLF 分隔符坏行（完整 / 头截断 / 尾截断变体）
/// 必须被 query_bad_multi_entry_rows 检出，否则 split 漏检、reconcile_fix 永不收敛。
/// 普通 CRLF 多行（无分隔符）不是坏行（reconcile 两侧归一已覆盖），不得误检。
#[test]
fn test_query_bad_detects_crlf_separator_rows() {
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();
    let now = crate::util::time::now_unix_ms();
    for content in [
        "a\r\n§\r\nb",
        "x\r\n§",
        "§\r\ny",
        "line1\r\nline2",
        "干净原子",
    ] {
        db.conn()
            .execute(
                "INSERT INTO bounded_memory (target, content, created_at, updated_at, confidence, memory_type)
                 VALUES ('memory', ?1, ?2, ?2, 'medium', 'atom')",
                rusqlite::params![content, now],
            )
            .unwrap();
    }

    let bad = query_bad_multi_entry_rows(db.conn(), "memory").unwrap();
    assert_eq!(bad.len(), 3, "仅三个 CRLF 分隔符变体是坏行");
    assert!(bad.iter().any(|r| r.content == "a\r\n§\r\nb"));
    assert!(bad.iter().any(|r| r.content == "x\r\n§"));
    assert!(bad.iter().any(|r| r.content == "§\r\ny"));
}

/// W2.3 回归：历史 CRLF 分隔符坏行经 reconcile_fix 必须拆分收敛——首次 fix 把
/// "a\r\n§\r\nb" 拆成 "a"/"b" 两条活行并删除原坏行；再次 fix 检不出坏行、不新增
/// 任何行（不制造永不收敛重复），报告两侧差集为空。
#[test]
fn test_reconcile_fix_converges_crlf_separator_row() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();
    let bm = BoundedMemory::new(tmp.path(), &db, 2200, 1375);

    // 模拟历史 L1 绕过入库：原始 CRLF 分隔符原子直插 DB（未归一），并按当时
    // 渲染出 .md——原样 join 把 CRLF 带进文件（伪分隔源）。
    let now = crate::util::time::now_unix_ms();
    db.conn()
        .execute(
            "INSERT INTO bounded_memory (target, content, created_at, updated_at, confidence, memory_type)
             VALUES ('memory', ?1, ?2, ?2, 'medium', 'atom')",
            rusqlite::params!["a\r\n§\r\nb", now],
        )
        .unwrap();
    bm.rebuild_md_from_db("memory").unwrap();

    bm.reconcile_fix("memory").unwrap();

    let contents: Vec<String> = db
        .conn()
        .prepare("SELECT content FROM bounded_memory WHERE target='memory' ORDER BY content")
        .unwrap()
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(
        contents,
        vec!["a".to_string(), "b".to_string()],
        "坏行必须拆成两条单条目活行并删除原行"
    );

    // 再次 fix：无坏行、无新增行——收敛，不制造重复。
    let split = bm.split_multi_entry_rows("memory").unwrap();
    assert_eq!(split.bad_rows, 0, "收敛后不得再检出坏行");
    bm.reconcile_fix("memory").unwrap();
    let count: i64 = db
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM bounded_memory WHERE target='memory'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 2, "重复 reconcile_fix 不得插入重复行");
    let report = bm.reconcile_check("memory").unwrap();
    assert!(
        report.only_in_md.is_empty() && report.only_in_db.is_empty(),
        "报告必须收敛: {report:?}"
    );
}

/// C4 回归：.md 独有手工条目与 DB § 多条目坏行并存时，reconcile_fix 必须拆分
/// DB 坏行**并**保留 .md 独有条目——旧实现先调 split_multi_entry_rows（其立即
/// rebuild .md）会用 DB 内容覆盖盘面，在 reinsert 保留之前就把手工条目永久冲掉。
#[test]
fn test_reconcile_fix_retains_md_only_entry_while_splitting_db_bad_row() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();
    let bm = BoundedMemory::new(tmp.path(), &db, 2200, 1375);
    let now = crate::util::time::now_unix_ms();

    // DB § 多条目坏行
    db.conn()
        .execute(
            "INSERT INTO bounded_memory (target, content, created_at, updated_at, confidence, memory_type)
             VALUES ('memory', ?1, ?2, ?2, 'medium', 'atom')",
            rusqlite::params!["a\n§\nb", now],
        )
        .unwrap();
    // .md 独有手工条目（无对应 DB 行）
    std::fs::write(
        tmp.path().join("MEMORY.md"),
        "<!-- ASUNA MEMORY | capacity: 2200 chars | updated: 2026-01-01T00:00:00Z -->\n\n手工独有\n",
    )
    .unwrap();

    bm.reconcile_fix("memory").unwrap();

    let mut contents: Vec<String> = db
        .conn()
        .prepare("SELECT content FROM bounded_memory WHERE target='memory'")
        .unwrap()
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    contents.sort();
    assert_eq!(
        contents,
        vec!["a".to_string(), "b".to_string(), "手工独有".to_string()],
        "DB 坏行必须拆成 a/b 且 .md 独有手工条目必须保留入库"
    );
    let md = std::fs::read_to_string(tmp.path().join("MEMORY.md")).unwrap();
    assert!(md.contains("手工独有"), ".md 必须保留手工条目: {md}");
}

/// W10 回归：被 tab 包裹的已被取代旧事实，经 2-arg TRIM 修剪后必须命中 .md 里的
/// 陈旧内容并被 reinsert 跳过——单 arg TRIM 只去空格，"\t旧事实\t" 会绕过匹配
/// 被回插成活行（复活）。
#[test]
fn test_tab_wrapped_superseded_entry_not_resurrected() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();
    let bm = BoundedMemory::new(tmp.path(), &db, 2200, 1375);
    let now = crate::util::time::now_unix_ms();

    // 旧事实（tab 包裹）被新事实取代
    db.conn()
        .execute(
            "INSERT INTO bounded_memory (target, content, created_at, updated_at, confidence, memory_type)
             VALUES ('memory', ?1, ?2, ?2, 'medium', 'atom')",
            rusqlite::params!["\t旧事实\t", now],
        )
        .unwrap();
    let old_id = db.conn().last_insert_rowid();
    db.conn()
        .execute(
            "INSERT INTO bounded_memory (target, content, created_at, updated_at, confidence, memory_type, supersedes_id)
             VALUES ('memory', ?1, ?2, ?2, 'medium', 'atom', ?3)",
            rusqlite::params!["新事实", now, old_id],
        )
        .unwrap();
    // .md 含陈旧 "旧事实"（修剪后形式）
    std::fs::write(
        tmp.path().join("MEMORY.md"),
        "<!-- ASUNA MEMORY | capacity: 2200 chars | updated: 2026-01-01T00:00:00Z -->\n\n旧事实\n",
    )
    .unwrap();

    bm.reconcile_fix("memory").unwrap();

    let active_old: i64 = db
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM bounded_memory bm
             WHERE bm.target='memory'
               AND TRIM(REPLACE(bm.content, char(13)||char(10), char(10)), char(9)||char(10)||char(13)||' ') = '旧事实'
               AND NOT EXISTS (SELECT 1 FROM bounded_memory s WHERE s.supersedes_id = bm.id)",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(active_old, 0, "tab 包裹的已被取代旧事实不得复活为活行");
    let md = std::fs::read_to_string(tmp.path().join("MEMORY.md")).unwrap();
    assert!(!md.contains("旧事实"), ".md 不得写回已被取代的旧事实: {md}");
    assert!(md.contains("新事实"));
}

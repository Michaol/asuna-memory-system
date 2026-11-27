use super::{extract_dim_tag, migration_statements, schema, Db};
use rusqlite::OptionalExtension;
use std::time::{SystemTime, UNIX_EPOCH};

fn temp_db_path() -> std::path::PathBuf {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("asuna_test_{}.db", ts))
}

#[test]
fn test_open_and_init() {
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();

    // 验证核心表与虚拟表存在
    let tables: Vec<String> = db
        .conn()
        .prepare("SELECT name FROM sqlite_master WHERE type='table' OR type='view' ORDER BY name")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();

    for expected in [
        "sessions",
        "turns",
        "bounded_memory",
        "audit_log",
        "entities",
        "relations",
        // A1/W4.3: capture_dedup 幂等表随 user_version 5 建表。
        "capture_dedup",
        "turns_fts",
        "bounded_memory_fts",
        "vec_turns",
        "vec_bounded_memory",
    ] {
        assert!(
            tables.contains(&expected.to_string()),
            "missing table/virtual table `{expected}`: {tables:?}"
        );
    }
    // S14d: memory_history was inert since its v2.6 introduction and has
    // been dropped from SCHEMA_SQL — a fresh DB must not carry it.
    assert!(
        !tables.contains(&"memory_history".to_string()),
        "memory_history must no longer be created (S14d)"
    );

    // 验证全部 6 个 FTS 同步触发器齐全
    let triggers: Vec<String> = db
        .conn()
        .prepare("SELECT name FROM sqlite_master WHERE type='trigger' ORDER BY name")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    for trig in [
        "turns_ai",
        "turns_ad",
        "turns_au",
        "bounded_memory_ai",
        "bounded_memory_ad",
        "bounded_memory_au",
    ] {
        assert!(
            triggers.contains(&trig.to_string()),
            "missing FTS trigger `{trig}`: {triggers:?}"
        );
    }

    // 验证 PRAGMA user_version 落到当前版本，且二次 init_schema 幂等
    let version: i32 = db
        .conn()
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, 6, "fresh init_schema must stamp user_version=6");

    db.init_schema()
        .expect("second init_schema must be idempotent");
    let version_after: i32 = db
        .conn()
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version_after, version);
}

/// v2.6 migration regression: a v2.5.3-era database (bounded_memory without
/// edited_at) must gain the column on first init_schema. Guards against the
/// comment-prefixed-ALTER silent no-op bug (the historical reason the P3
/// migration never ran; fixed 2026-09 — see migration_statements).
#[test]
fn test_v26_edited_at_migration_on_old_db() {
    let path = temp_db_path();
    let db = Db::open(&path).unwrap();

    // Simulate a v2.5.3-era bounded_memory table (no edited_at column)
    db.conn()
        .execute_batch(
            "CREATE TABLE bounded_memory (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                target TEXT NOT NULL,
                content TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                source_session TEXT,
                confidence TEXT DEFAULT 'medium',
                memory_type TEXT DEFAULT 'manual',
                supersedes_id INTEGER REFERENCES bounded_memory(id),
                source_turn_ids TEXT,
                confidence_score REAL DEFAULT 1.0
            );
            INSERT INTO bounded_memory (target, content, created_at, updated_at)
            VALUES ('memory', 'existing row', 1, 1);",
        )
        .unwrap();

    db.init_schema().unwrap();

    let has_col: bool = db
        .conn()
        .prepare("PRAGMA table_info(bounded_memory)")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(1))
        .unwrap()
        .filter_map(Result::ok)
        .any(|name| name == "edited_at");
    assert!(
        has_col,
        "edited_at must be migrated onto a v2.5.3-era database"
    );

    // Existing rows survive the upgrade with NULL edited_at
    let (count, edited): (i64, i64) = db
        .conn()
        .query_row(
            "SELECT COUNT(*), COUNT(edited_at) FROM bounded_memory",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(count, 1);
    assert_eq!(edited, 0, "pre-existing rows must have NULL edited_at");

    drop(db);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{}-wal", path.display()));
    let _ = std::fs::remove_file(format!("{}-shm", path.display()));
}

/// Migration runner regression (C5): comment-prefixed statements must still
/// execute. The old runner skipped fragments starting with "--" after
/// split(';'), so every comment-prefixed ALTER in MIGRATION_P3_SQL was
/// silently dropped and the whole P3 migration no-op'd.
#[test]
fn test_migration_statements_strips_comment_lines() {
    let stmts = migration_statements(
        "\n-- comment\nALTER TABLE t ADD COLUMN a INTEGER;\n-- another\nCREATE INDEX i ON t(a);\n",
    );
    assert_eq!(stmts.len(), 2);
    assert!(stmts[0].starts_with("ALTER"), "got: {}", stmts[0]);
    assert!(stmts[1].starts_with("CREATE"), "got: {}", stmts[1]);
    // The real P3 constant: 4 ALTER + 2 CREATE INDEX = 6 executable statements
    assert_eq!(migration_statements(schema::MIGRATION_P3_SQL).len(), 6);
}

/// C5 regression: a pre-P3 (v1.0.0-era) database — bounded_memory ending at
/// `confidence`, without the four P3 columns — must upgrade on startup. Two
/// bugs are pinned: (1) the P3 migration actually executes its comment-adjacent
/// ALTERs; (2) SCHEMA_SQL no longer creates the memory_type/supersedes indexes
/// before the migration has added those columns (which made init_schema fail
/// outright on such old DBs).
#[test]
fn test_pre_p3_db_upgrade() {
    let path = temp_db_path();
    let mut db = Db::open(&path).unwrap();

    // v1.0.0-era bounded_memory DDL (git show 2ebc8fe:src/index/schema.rs)
    db.conn()
        .execute_batch(
            "CREATE TABLE bounded_memory (
                id            INTEGER PRIMARY KEY AUTOINCREMENT,
                target        TEXT    NOT NULL,
                content       TEXT    NOT NULL,
                created_at    INTEGER NOT NULL,
                updated_at    INTEGER NOT NULL,
                source_session TEXT,
                confidence    TEXT    DEFAULT 'medium'
            );
            INSERT INTO bounded_memory (target, content, created_at, updated_at)
            VALUES ('memory', 'pre-P3 旧数据', 1, 1);",
        )
        .unwrap();

    // Must succeed: previously init_schema died on CREATE INDEX ... (memory_type)
    // inside SCHEMA_SQL before the (no-op) P3 migration.
    db.set_dimensions(64);
    db.init_schema().unwrap();

    let cols: Vec<String> = db
        .conn()
        .prepare("PRAGMA table_info(bounded_memory)")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(1))
        .unwrap()
        .filter_map(Result::ok)
        .collect();
    for col in [
        "memory_type",
        "supersedes_id",
        "source_turn_ids",
        "confidence_score",
        "edited_at",
    ] {
        assert!(cols.iter().any(|c| c == col), "missing column {col}");
    }

    let idx: Vec<String> = db
        .conn()
        .prepare("SELECT name FROM sqlite_master WHERE type='index' AND tbl_name='bounded_memory'")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .filter_map(Result::ok)
        .collect();
    assert!(
        idx.iter().any(|i| i == "idx_bounded_memory_type"),
        "idx_bounded_memory_type must exist, got {idx:?}"
    );
    assert!(
        idx.iter().any(|i| i == "idx_bounded_memory_supersedes"),
        "idx_bounded_memory_supersedes must exist, got {idx:?}"
    );

    // Old data survives the upgrade
    let (count, content): (i64, String) = db
        .conn()
        .query_row(
            "SELECT COUNT(*), MAX(content) FROM bounded_memory",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(count, 1);
    assert_eq!(content, "pre-P3 旧数据");

    // Idempotent: re-running init_schema tolerates duplicate-column errors
    db.init_schema().unwrap();
    let count2: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM bounded_memory", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count2, 1);

    drop(db);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{}-wal", path.display()));
    let _ = std::fs::remove_file(format!("{}-shm", path.display()));
}

#[test]
fn test_wal_mode() {
    let path = temp_db_path();
    let db = Db::open(&path).unwrap();
    let mode = db.journal_mode().unwrap();
    assert_eq!(mode, "wal");

    // 清理
    drop(db);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{}-wal", path.display()));
    let _ = std::fs::remove_file(format!("{}-shm", path.display()));
}

#[test]
fn test_integrity_check() {
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();
    assert!(db.integrity_check().unwrap());
}

#[test]
fn test_vec_turns_table() {
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();

    // 验证 vec_turns 虚拟表存在
    let has_vec: bool = db
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE name='vec_turns'",
            [],
            |r| r.get::<_, i64>(0).map(|c| c > 0),
        )
        .unwrap();
    assert!(has_vec, "vec_turns virtual table should exist");
}

#[test]
fn test_custom_dimensions() {
    let mut db = Db::open_memory().unwrap();
    db.set_dimensions(1024);
    db.init_schema().unwrap();

    // Verify vec_turns uses 1024 dimensions
    let sql: String = db
        .conn()
        .query_row(
            "SELECT sql FROM sqlite_master WHERE name='vec_turns'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        sql.contains("int8[1024]"),
        "vec_turns should use int8[1024], got: {}",
        sql
    );
    assert!(
        sql.contains("distance_metric=cosine"),
        "vec_turns should use cosine metric, got: {}",
        sql
    );

    // Verify vec_bounded_memory also uses 1024
    let sql2: String = db
        .conn()
        .query_row(
            "SELECT sql FROM sqlite_master WHERE name='vec_bounded_memory'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        sql2.contains("int8[1024]"),
        "vec_bounded_memory should use int8[1024], got: {}",
        sql2
    );
    assert!(
        sql2.contains("distance_metric=cosine"),
        "vec_bounded_memory should use cosine metric, got: {}",
        sql2
    );

    // 运行时行为验证：1024 维写入与 KNN 查询精确命中，错维（768）必须被拒。
    let vec_store = crate::index::vector::VectorStore::new(&db);
    let mut v1024 = vec![0.0f32; 1024];
    v1024[0] = 1.0;
    vec_store.insert(42, &v1024).unwrap();
    let hits = vec_store.search(&v1024, 5).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].0, 42);
    assert!(
        hits[0].1 < 1e-4,
        "identical vector distance ~0, got {}",
        hits[0].1
    );

    let mut wrong_dim = vec![0.0f32; 768];
    wrong_dim[0] = 1.0;
    assert!(
        vec_store.insert(43, &wrong_dim).is_err(),
        "mismatched 768-dim vector must be rejected by int8[1024] table"
    );
}

#[test]
fn test_dimension_migration() {
    let path = temp_db_path();

    // Phase 1: Create DB with 768 dimensions
    {
        let mut db = Db::open(&path).unwrap();
        db.set_dimensions(768);
        db.init_schema().unwrap();
    }

    // Phase 2: Reopen with 1024 dimensions — should auto-migrate
    {
        let mut db = Db::open(&path).unwrap();
        db.set_dimensions(1024);
        db.init_schema().unwrap();

        let sql: String = db
            .conn()
            .query_row(
                "SELECT sql FROM sqlite_master WHERE name='vec_turns'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            sql.contains("int8[1024]"),
            "should migrate to 1024, got: {}",
            sql
        );
    }

    // 清理
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{}-wal", path.display()));
    let _ = std::fs::remove_file(format!("{}-shm", path.display()));
}

#[test]
fn test_distance_metric_migration() {
    let path = temp_db_path();

    // Phase 1: simulate a pre-fix DB whose vec tables use the default L2 metric
    // (no distance_metric option) by recreating them with the old DDL.
    {
        let mut db = Db::open(&path).unwrap();
        db.set_dimensions(1024);
        db.init_schema().unwrap();

        db.conn().execute("DROP TABLE vec_turns", []).unwrap();
        db.conn()
            .execute("DROP TABLE vec_bounded_memory", [])
            .unwrap();
        db.conn()
            .execute_batch(
                "CREATE VIRTUAL TABLE vec_turns USING vec0(embedding int8[1024]);
                 CREATE VIRTUAL TABLE vec_bounded_memory USING vec0(
                     id INTEGER PRIMARY KEY,
                     embedding int8[1024]
                 );",
            )
            .unwrap();

        let pre: String = db
            .conn()
            .query_row(
                "SELECT sql FROM sqlite_master WHERE name='vec_turns'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            !pre.contains("distance_metric"),
            "phase 1 should be default L2, got: {}",
            pre
        );
    }

    // Phase 2: reopen with init_schema — should detect the missing cosine metric
    // and auto-rebuild both vec tables.
    {
        let mut db = Db::open(&path).unwrap();
        db.set_dimensions(1024);
        db.init_schema().unwrap();

        let sql: String = db
            .conn()
            .query_row(
                "SELECT sql FROM sqlite_master WHERE name='vec_turns'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            sql.contains("distance_metric=cosine"),
            "vec_turns should migrate to cosine, got: {}",
            sql
        );

        let sql2: String = db
            .conn()
            .query_row(
                "SELECT sql FROM sqlite_master WHERE name='vec_bounded_memory'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            sql2.contains("distance_metric=cosine"),
            "vec_bounded_memory should migrate to cosine, got: {}",
            sql2
        );
    }

    // 清理
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{}-wal", path.display()));
    let _ = std::fs::remove_file(format!("{}-shm", path.display()));
}

#[test]
fn test_extract_dim_tag() {
    assert_eq!(
        extract_dim_tag("CREATE VIRTUAL TABLE vec_turns USING vec0(embedding int8[768])"),
        Some("int8[768]".to_string())
    );
    assert_eq!(
        extract_dim_tag("CREATE VIRTUAL TABLE v USING vec0(embedding float32[1024])"),
        Some("float32[1024]".to_string())
    );
    assert_eq!(extract_dim_tag("no dimension here"), None);
}

/// 验证旧版 unicode61 FTS 触发器迁移到 jieba
#[test]
fn test_fts_jieba_migration() {
    let path = temp_db_path();

    // Phase 1: Create DB with current schema, then downgrade FTS to unicode61
    {
        let db = Db::open(&path).unwrap();
        db.init_schema().unwrap();

        // Simulate old unicode61 schema: drop jieba FTS and recreate with unicode61
        db.conn().execute_batch(
            "DROP TRIGGER IF EXISTS turns_ai;
             DROP TRIGGER IF EXISTS turns_ad;
             DROP TRIGGER IF EXISTS turns_au;
             DROP TABLE IF EXISTS turns_fts;
             CREATE VIRTUAL TABLE turns_fts USING fts5(preview, content='', content_rowid=id, tokenize='unicode61 remove_diacritics 2');
             CREATE TRIGGER turns_ai AFTER INSERT ON turns BEGIN INSERT INTO turns_fts(rowid, preview) VALUES (new.id, tokenize_zh(new.preview)); END;
             CREATE TRIGGER turns_ad AFTER DELETE ON turns BEGIN INSERT INTO turns_fts(turns_fts, rowid, preview) VALUES ('delete', old.id, tokenize_zh(old.preview)); END;
             CREATE TRIGGER turns_au AFTER UPDATE ON turns BEGIN INSERT INTO turns_fts(turns_fts, rowid, preview) VALUES ('delete', old.id, tokenize_zh(old.preview)); INSERT INTO turns_fts(rowid, preview) VALUES (new.id, tokenize_zh(new.preview)); END;
             DROP TRIGGER IF EXISTS bounded_memory_ai;
             DROP TRIGGER IF EXISTS bounded_memory_ad;
             DROP TRIGGER IF EXISTS bounded_memory_au;
             DROP TABLE IF EXISTS bounded_memory_fts;
             CREATE VIRTUAL TABLE bounded_memory_fts USING fts5(content, content='bounded_memory', content_rowid='id', tokenize='unicode61 remove_diacritics 2');
             CREATE TRIGGER bounded_memory_ai AFTER INSERT ON bounded_memory BEGIN INSERT INTO bounded_memory_fts(rowid, content) VALUES (new.id, tokenize_zh(new.content)); END;",
        ).unwrap();
    }

    // Phase 2: Reopen — should detect unicode61 and migrate to jieba
    {
        let db = Db::open(&path).unwrap();
        db.init_schema().unwrap();

        // Verify trigger no longer references tokenize_zh
        let trigger_sql: String = db
            .conn()
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type='trigger' AND name='turns_ai'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            !trigger_sql.contains("tokenize_zh"),
            "trigger should not reference tokenize_zh after migration, got: {}",
            trigger_sql
        );

        // Verify FTS table uses jieba
        let fts_sql: String = db
            .conn()
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type='table' AND name='turns_fts'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            fts_sql.contains("jieba"),
            "turns_fts should use jieba tokenizer, got: {}",
            fts_sql
        );

        // Verify bounded_memory_fts also uses jieba
        let bm_fts_sql: String = db
            .conn()
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type='table' AND name='bounded_memory_fts'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            bm_fts_sql.contains("jieba"),
            "bounded_memory_fts should use jieba tokenizer, got: {}",
            bm_fts_sql
        );
    }

    // 清理
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{}-wal", path.display()));
    let _ = std::fs::remove_file(format!("{}-shm", path.display()));
}

/// J4: 迁移回填改单事务后的正确性冒烟——2000 行 turns 在 unicode61→jieba
/// 迁移后必须全量回填进 turns_fts 且可按词命中（行为与逐条提交等价，只是快）。
#[test]
fn test_fts_jieba_migration_bulk_backfill() {
    let path = temp_db_path();
    const N: i64 = 2000;

    // Phase 1: 建库、灌 2000 行、再把 FTS 降级成旧 unicode61 架构
    {
        let db = Db::open(&path).unwrap();
        db.init_schema().unwrap();
        db.conn()
            .execute(
                "INSERT INTO sessions (session_id, start_ts, file_path, created_at, updated_at)
                 VALUES ('bulk', 0, 'bulk.jsonl', 0, 0)",
                [],
            )
            .unwrap();
        db.conn().execute("BEGIN IMMEDIATE", []).unwrap();
        for i in 0..N {
            db.conn()
                .execute(
                    "INSERT INTO turns (session_id, seq, timestamp_ms, role, preview)
                     VALUES ('bulk', ?1, ?2, 'user', ?3)",
                    rusqlite::params![i, i, format!("bulk smoke preview alpha{}", i)],
                )
                .unwrap();
        }
        db.conn().execute("COMMIT", []).unwrap();

        db.conn().execute_batch(
            "DROP TRIGGER IF EXISTS turns_ai;
             DROP TRIGGER IF EXISTS turns_ad;
             DROP TRIGGER IF EXISTS turns_au;
             DROP TABLE IF EXISTS turns_fts;
             CREATE VIRTUAL TABLE turns_fts USING fts5(preview, content='', content_rowid=id, tokenize='unicode61 remove_diacritics 2');",
        )
        .unwrap();
    }

    // Phase 2: 重开触发迁移 → 单事务回填全部 2000 行
    {
        let db = Db::open(&path).unwrap();
        db.init_schema().unwrap();

        let fts_count: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM turns_fts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(fts_count, N, "all turns must be backfilled into turns_fts");

        // 可检索性：唯一 token 恰好命中自己的行，且能 JOIN 回 turns
        let hits: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM turns_fts f
                 JOIN turns t ON f.rowid = t.id
                 WHERE turns_fts MATCH '\"alpha1999\"'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(hits, 1, "unique token must match exactly its own row");
    }

    // 清理
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{}-wal", path.display()));
    let _ = std::fs::remove_file(format!("{}-shm", path.display()));
}

/// Verify bounded_memory_fts backfill works after FTS table is emptied
/// (e.g., after jieba migration drops and recreates the FTS table).
#[test]
fn test_bounded_memory_fts_backfill() {
    let path = temp_db_path();

    // Phase 1: Create DB, insert entries into bounded_memory
    {
        let db = Db::open(&path).unwrap();
        db.init_schema().unwrap();

        db.conn().execute_batch(
            "INSERT INTO bounded_memory (target, content, created_at, updated_at, confidence, memory_type)
             VALUES ('memory', '我喜欢编程', 1000, 1000, 'high', 'atom');
             INSERT INTO bounded_memory (target, content, created_at, updated_at, confidence, memory_type)
             VALUES ('memory', '数据库设计很重要', 2000, 2000, 'medium', 'atom');
             INSERT INTO bounded_memory (target, content, created_at, updated_at, confidence, memory_type)
             VALUES ('memory', 'Rust is great', 3000, 3000, 'high', 'atom');",
        ).unwrap();

        // Verify entries exist in bounded_memory
        let bm_count: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM bounded_memory", [], |r| r.get(0))
            .unwrap();
        assert_eq!(bm_count, 3);
    }

    // Phase 2: Simulate FTS table being emptied (like jieba migration)
    {
        let db = Db::open(&path).unwrap();
        // Drop and recreate FTS table empty (simulating what jieba migration does)
        db.conn()
            .execute_batch(
                "DROP TRIGGER IF EXISTS bounded_memory_ai;
             DROP TRIGGER IF EXISTS bounded_memory_ad;
             DROP TRIGGER IF EXISTS bounded_memory_au;
             DROP TABLE IF EXISTS bounded_memory_fts;
             CREATE VIRTUAL TABLE bounded_memory_fts USING fts5(
                 content, content='bounded_memory', content_rowid='id', tokenize='jieba'
             );",
            )
            .unwrap();

        // Now FTS index is empty but bounded_memory has 3 entries
    }

    // Phase 3: Reopen — init_schema should detect and backfill
    {
        let db = Db::open(&path).unwrap();
        db.init_schema().unwrap();

        // Verify FTS now has data: search for "编程" should find the entry
        let count: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM bounded_memory_fts WHERE bounded_memory_fts MATCH '编程'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            count > 0,
            "bounded_memory_fts should be backfilled, '编程' search returned 0"
        );

        // Search for "数据库" should also work
        let count2: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM bounded_memory_fts WHERE bounded_memory_fts MATCH '数据库'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            count2 > 0,
            "bounded_memory_fts should be backfilled, '数据库' search returned 0"
        );

        // Search for "Rust" should also work
        let count3: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM bounded_memory_fts WHERE bounded_memory_fts MATCH 'Rust'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            count3 > 0,
            "bounded_memory_fts should be backfilled, 'Rust' search returned 0"
        );
    }

    // 清理
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{}-wal", path.display()));
    let _ = std::fs::remove_file(format!("{}-shm", path.display()));
}

/// C14-c: the vec-backfill candidate set must exclude atoms superseded by
/// a newer row — supersede de-indexes them on purpose, and re-embedding
/// them at startup would resurrect the contradicted vectors (the whole
/// invalidation chain becomes moot). The chain HEAD (the superseding atom,
/// still lacking a vec) must stay a candidate.
#[test]
fn test_backfill_candidates_exclude_superseded() {
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();
    db.conn()
        .execute_batch(
            "INSERT INTO bounded_memory (target, content, created_at, updated_at, confidence, memory_type)
             VALUES ('memory', 'live atom head', 1000, 1000, 'high', 'atom');
             INSERT INTO bounded_memory (target, content, created_at, updated_at, confidence, memory_type, supersedes_id)
             VALUES ('memory', 'replaces nothing', 2000, 2000, 'high', 'atom', 1);
             INSERT INTO bounded_memory (target, content, created_at, updated_at, confidence, memory_type)
             VALUES ('memory', 'manual entry', 3000, 3000, 'high', 'manual');
             INSERT INTO bounded_memory (target, content, created_at, updated_at, confidence, memory_type)
             VALUES ('memory', 'scenario row', 4000, 4000, 'high', 'scenario');",
        )
        .unwrap();

    // Row 1 is superseded by row 2; rows 3/4 are not atoms.
    let candidates: Vec<(i64, String)> = db.bounded_memory_backfill_candidates().unwrap();
    assert_eq!(
        candidates.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
        vec![2],
        "only the non-superseded atom is a backfill candidate, got: {candidates:?}"
    );
}

/// NB2: a startup backfill from BEFORE the C14-c candidate filter
/// resurrected vectors for superseded atoms; those rows never shrink on
/// their own (the embed stage only inserts). maybe_backfill_bounded_memory_vec
/// must purge them (idempotent, repeated calls are no-ops) while leaving
/// the live rows' vectors untouched.
#[test]
fn test_vec_backfill_purges_superseded_vectors() {
    let mut db = Db::open_memory().unwrap();
    db.set_dimensions(3);
    db.init_schema().unwrap();
    db.conn()
        .execute_batch(
            "INSERT INTO bounded_memory (target, content, created_at, updated_at, confidence, memory_type)
             VALUES ('memory', 'superseded old atom', 1, 1, 'high', 'atom');
             INSERT INTO bounded_memory (target, content, created_at, updated_at, confidence, memory_type, supersedes_id)
             VALUES ('memory', 'chain head', 2, 2, 'high', 'atom', 1);
             INSERT INTO bounded_memory (target, content, created_at, updated_at, confidence, memory_type)
             VALUES ('memory', 'live atom', 3, 3, 'high', 'atom');",
        )
        .unwrap();
    // Simulate the pre-fix resurrection: vectors for ALL three rows.
    for id in [1i64, 2, 3] {
        db.conn()
            .execute(
                "INSERT INTO vec_bounded_memory (id, embedding) VALUES (?1, vec_int8(?2))",
                rusqlite::params![id, crate::embedder::quantize_to_int8(&[1.0, 0.0, 0.0])],
            )
            .unwrap();
    }

    // API embedder pointed at an unreachable host: the embed stage must be
    // irrelevant here (both live rows already have vectors → pending empty).
    let mut emb_cfg = crate::config::Config::default().embedding;
    emb_cfg.api_url = "http://127.0.0.1:9/v1".to_string();
    emb_cfg.api_model = "unreachable-test".to_string();
    let embedder = crate::embedder::LazyEmbedder::from_config(&emb_cfg)
        .expect("api embedder with url+model must construct");

    db.maybe_backfill_bounded_memory_vec(&embedder).unwrap();

    let survivors: Vec<i64> = {
        let mut stmt = db
            .conn()
            .prepare("SELECT id FROM vec_bounded_memory ORDER BY id")
            .unwrap();
        stmt.query_map([], |r| r.get::<_, i64>(0))
            .unwrap()
            .filter_map(Result::ok)
            .collect()
    };
    assert_eq!(
        survivors,
        vec![2, 3],
        "only the superseded row's vector is purged; live rows keep theirs"
    );

    // Idempotent: a second run removes nothing further.
    db.maybe_backfill_bounded_memory_vec(&embedder).unwrap();
    let survivors2: Vec<i64> = {
        let mut stmt = db
            .conn()
            .prepare("SELECT id FROM vec_bounded_memory ORDER BY id")
            .unwrap();
        stmt.query_map([], |r| r.get::<_, i64>(0))
            .unwrap()
            .filter_map(Result::ok)
            .collect()
    };
    assert_eq!(survivors2, vec![2, 3], "purge is idempotent");
}

#[test]
fn test_open_readonly_enforces_authorizer_and_no_create() {
    let missing = temp_db_path();
    assert!(
        Db::open_readonly(&missing).is_err(),
        "open_readonly must not create a missing database"
    );
    assert!(!missing.exists());

    let path = temp_db_path();
    {
        let db = Db::open(&path).unwrap();
        db.init_schema().unwrap();
        db.conn()
            .execute(
                "INSERT INTO sessions (session_id, start_ts, file_path, created_at, updated_at)
                 VALUES ('ro-s1', 1000, 'f.jsonl', 1000, 1000)",
                [],
            )
            .unwrap();
    }

    let ro = Db::open_readonly(&path).unwrap();
    let cnt: i64 = ro
        .conn()
        .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))
        .unwrap();
    assert_eq!(cnt, 1);

    // Introspection pragmas, query_only read, and custom tokenize_zh function work
    assert!(ro
        .conn()
        .prepare("PRAGMA table_info(sessions)")
        .unwrap()
        .query_map([], |_| Ok(()))
        .is_ok());
    let qo: i64 = ro
        .conn()
        .query_row("PRAGMA query_only", [], |r| r.get(0))
        .unwrap();
    assert_eq!(qo, 1);
    let tok: String = ro
        .conn()
        .query_row("SELECT tokenize_zh('测试分词')", [], |r| r.get(0))
        .unwrap();
    assert!(!tok.is_empty());

    // W6: integrity_check/quick_check 必须在只读 authorizer 下成功——FTS5
    // xIntegrity 内部读 PRAGMA data_version（纯内省头值），已放行其只读形态。
    assert!(
        ro.integrity_check().unwrap(),
        "read-only integrity_check must succeed"
    );
    let quick: String = ro
        .conn()
        .query_row("PRAGMA quick_check", [], |r| r.get(0))
        .unwrap();
    assert_eq!(quick, "ok");
    let dv: i64 = ro
        .conn()
        .query_row("PRAGMA data_version", [], |r| r.get(0))
        .unwrap();
    assert!(dv > 0);
    // 赋值形态 PRAGMA 仍拒绝（只读隔离不放宽）
    assert!(ro.conn().execute_batch("PRAGMA data_version = 7;").is_err());
    assert!(ro
        .conn()
        .execute_batch("PRAGMA user_version = 99;")
        .is_err());

    // Write, schema mutation, ATTACH, and PRAGMA writes are all denied
    assert!(ro
        .conn()
        .execute("DELETE FROM sessions WHERE session_id = 'ro-s1'", [])
        .is_err());
    assert!(ro
        .conn()
        .execute(
            "INSERT INTO sessions (session_id, start_ts, file_path, created_at, updated_at) VALUES ('x', 1, 'x', 1, 1)",
            []
        )
        .is_err());
    assert!(ro
        .conn()
        .execute_batch("ATTACH DATABASE ':memory:' AS aux;")
        .is_err());
    assert!(ro.conn().execute_batch("PRAGMA query_only = OFF;").is_err());
    assert!(ro
        .conn()
        .execute_batch("CREATE TABLE evil(x INT);")
        .is_err());

    drop(ro);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{}-wal", path.display()));
    let _ = std::fs::remove_file(format!("{}-shm", path.display()));
}

#[test]
fn test_schema_user_version_narrow_triggers_and_indexes() {
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();

    let ver: i32 = db
        .conn()
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(ver, 6);

    let turns_au: String = db
        .conn()
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type='trigger' AND name='turns_au'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(turns_au.contains("UPDATE OF preview"), "{turns_au}");

    let bm_au: String = db
        .conn()
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type='trigger' AND name='bounded_memory_au'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(bm_au.contains("UPDATE OF content"), "{bm_au}");

    let idx_count: i64 = db
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name IN ('idx_sessions_updated', 'idx_entities_src_turn')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(idx_count, 2);

    // Idempotent re-init keeps user_version=6 and existing data intact
    db.init_schema().unwrap();
    let ver2: i32 = db
        .conn()
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(ver2, 6);
}

/// A1/W4.3 迁移回归：pre-A1 的 v4 库（无 capture_dedup）必须在 init_schema
/// 时经版本门重放 SCHEMA_SQL 建表并升至当前版本（v6）；幂等再跑不重复建表/不丢数据。
#[test]
fn test_capture_dedup_migration_v4_to_v5() {
    let path = temp_db_path();
    {
        let db = Db::open(&path).unwrap();
        db.init_schema().unwrap();
        // 模拟 pre-A1 v4 库：删表 + 回退版本号。
        db.conn()
            .execute_batch("DROP TABLE capture_dedup; PRAGMA user_version = 4;")
            .unwrap();
    }
    {
        let db = Db::open(&path).unwrap();
        db.init_schema().unwrap();
        let has_table: bool = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='capture_dedup'",
                [],
                |r| r.get::<_, i64>(0).map(|c| c > 0),
            )
            .unwrap();
        assert!(has_table, "v4 库必须经版本门重建 capture_dedup");
        let ver: i32 = db
            .conn()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(ver, 6);
        // 幂等：再跑 init_schema 不报错、表仍在、版本不变。
        db.init_schema().unwrap();
        let ver2: i32 = db
            .conn()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(ver2, 6);
    }
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{}-wal", path.display()));
    let _ = std::fs::remove_file(format!("{}-shm", path.display()));
}

#[test]
fn test_init_schema_backfills_empty_turns_fts_when_turns_exist() {
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();

    db.conn()
        .execute(
            "INSERT INTO sessions (session_id, start_ts, file_path, created_at, updated_at)
             VALUES ('s-crash', 1, 's.jsonl', 1, 1)",
            [],
        )
        .unwrap();
    db.conn()
        .execute(
            "INSERT INTO turns (session_id, seq, timestamp_ms, role, preview)
             VALUES ('s-crash', 1, 1000, 'user', 'crashrecovery keyword')",
            [],
        )
        .unwrap();

    // Simulate crash window after recreating empty jieba turns_fts before backfill ran.
    db.conn()
        .execute("INSERT INTO turns_fts(turns_fts) VALUES('delete-all')", [])
        .unwrap();
    let before: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM turns_fts", [], |r| r.get(0))
        .unwrap();
    assert_eq!(before, 0);

    db.init_schema().unwrap();
    let hits: i64 = db
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM turns_fts WHERE turns_fts MATCH 'crashrecovery'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(hits, 1);
}

/// W2.5 回归：needs-backfill 判定在事务外，并发首启两进程可能先后各回填一次。
/// 事务内 delete-all 使回填幂等——重复调用不得叠加 term posting（bm25 虚高 /
/// 重复命中）。旧实现无条件 INSERT，docsize REPLACE 令 COUNT(*) 抓不到。
#[test]
fn test_backfill_turns_fts_repeat_is_idempotent() {
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();
    db.conn()
        .execute(
            "INSERT INTO sessions (session_id, start_ts, file_path, created_at, updated_at)
             VALUES ('w25', 0, 'w25.jsonl', 0, 0)",
            [],
        )
        .unwrap();
    db.conn()
        .execute_batch(
            "INSERT INTO turns (session_id, seq, timestamp_ms, role, preview)
             VALUES ('w25', 1, 0, 'user', 'uniquetoken alpha beta');
             INSERT INTO turns (session_id, seq, timestamp_ms, role, preview)
             VALUES ('w25', 2, 0, 'user', 'other text gamma');",
        )
        .unwrap();
    // 模拟崩溃残留：FTS 清空、turns 仍在 → 两进程都会判定需要回填
    db.conn()
        .execute("INSERT INTO turns_fts(turns_fts) VALUES('delete-all')", [])
        .unwrap();

    let rank_of_unique = || -> f64 {
        db.conn()
            .query_row(
                "SELECT bm25(turns_fts) FROM turns_fts WHERE turns_fts MATCH 'uniquetoken'",
                [],
                |r| r.get(0),
            )
            .unwrap()
    };
    let hits_of_unique = || -> i64 {
        db.conn()
            .query_row(
                "SELECT COUNT(*) FROM turns_fts WHERE turns_fts MATCH 'uniquetoken'",
                [],
                |r| r.get(0),
            )
            .unwrap()
    };

    db.backfill_turns_fts().unwrap();
    let rank1 = rank_of_unique();
    assert_eq!(hits_of_unique(), 1);
    // 第二次回填（并发首启的落后进程）：delete-all 幂等，排名与命中数不变
    db.backfill_turns_fts().unwrap();
    assert_eq!(hits_of_unique(), 1, "重复回填不得产生重复命中");
    assert_eq!(
        rank1,
        rank_of_unique(),
        "重复回填不得抬高 bm25（posting 叠加）"
    );

    let fts_count: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM turns_fts", [], |r| r.get(0))
        .unwrap();
    assert_eq!(fts_count, 2);
}

/// D2-6 回归：部分缺失（非全空）也要触发回填——旧 EXISTS 检测只认"全空"，
/// 崩溃留下半截 FTS 永不修复。
#[test]
fn test_turns_fts_needs_backfill_detects_partial_missing() {
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();
    db.conn()
        .execute(
            "INSERT INTO sessions (session_id, start_ts, file_path, created_at, updated_at)
             VALUES ('d26', 0, 'd26.jsonl', 0, 0)",
            [],
        )
        .unwrap();
    db.conn()
        .execute_batch(
            "INSERT INTO turns (session_id, seq, timestamp_ms, role, preview)
             VALUES ('d26', 1, 0, 'user', 'recoveryalpha preview');
             INSERT INTO turns (session_id, seq, timestamp_ms, role, preview)
             VALUES ('d26', 2, 0, 'user', 'recoverybeta preview');",
        )
        .unwrap();
    assert!(!db.turns_fts_needs_backfill().unwrap());

    // 删掉其中一条的 posting（FTS 非空但缺行）
    let (id, preview): (i64, String) = db
        .conn()
        .query_row("SELECT id, preview FROM turns WHERE seq = 1", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    db.conn()
        .execute(
            "INSERT INTO turns_fts(turns_fts, rowid, preview) VALUES ('delete', ?1, ?2)",
            rusqlite::params![id, preview],
        )
        .unwrap();
    assert!(
        db.turns_fts_needs_backfill().unwrap(),
        "部分缺失必须触发回填（旧 EXISTS 检测抓不到）"
    );

    db.backfill_turns_fts().unwrap();
    assert!(!db.turns_fts_needs_backfill().unwrap());
    let hits: i64 = db
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM turns_fts WHERE turns_fts MATCH 'recoveryalpha'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(hits, 1, "缺失行必须被修复");
}

/// D2-7 回归：NULL preview 不入 FTS（触发器与回填对称）；NULL↔文本 的
/// UPDATE 四分支正确；删除 NULL preview turn 不得 FTS5_CORRUPT。
#[test]
fn test_null_preview_fts_symmetry() {
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();
    db.conn()
        .execute(
            "INSERT INTO sessions (session_id, start_ts, file_path, created_at, updated_at)
             VALUES ('d27', 0, 'd27.jsonl', 0, 0)",
            [],
        )
        .unwrap();
    db.conn()
        .execute_batch(
            "INSERT INTO turns (session_id, seq, timestamp_ms, role, preview)
             VALUES ('d27', 1, 0, 'user', NULL);
             INSERT INTO turns (session_id, seq, timestamp_ms, role, preview)
             VALUES ('d27', 2, 0, 'user', 'textkeyword');",
        )
        .unwrap();
    let fts_count = || -> i64 {
        db.conn()
            .query_row("SELECT COUNT(*) FROM turns_fts", [], |r| r.get(0))
            .unwrap()
    };
    assert_eq!(fts_count(), 1, "NULL preview 不得入 FTS");
    assert!(
        !db.turns_fts_needs_backfill().unwrap(),
        "触发器与回填口径对称，计数一致无需回填"
    );

    // 删除 NULL preview turn：turns_ad 守卫跳过 'delete'（旧实现会 FTS5_CORRUPT）
    db.conn()
        .execute("DELETE FROM turns WHERE seq = 1", [])
        .unwrap();
    assert_eq!(fts_count(), 1);

    // 文本 → NULL：turns_au 只删不插
    db.conn()
        .execute("UPDATE turns SET preview = NULL WHERE seq = 2", [])
        .unwrap();
    assert_eq!(fts_count(), 0);

    // NULL → 文本：turns_au 只插不删（不得对不存在的 posting 发 'delete'）
    db.conn()
        .execute("UPDATE turns SET preview = 'newkeyword' WHERE seq = 2", [])
        .unwrap();
    assert_eq!(fts_count(), 1);
    let hits: i64 = db
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM turns_fts WHERE turns_fts MATCH 'newkeyword'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(hits, 1);
}

/// D2-8 回归：turns_fts 已是 jieba 而 bounded_memory_fts 独自停留 unicode61
/// 时也必须迁移（旧检测只看 turns_fts，L1 中文检索静默失效）。
#[test]
fn test_bm_fts_unicode61_alone_migrates() {
    let path = temp_db_path();
    {
        let db = Db::open(&path).unwrap();
        db.init_schema().unwrap();
        db.conn()
            .execute(
                "INSERT INTO bounded_memory (target, content, created_at, updated_at, memory_type)
                 VALUES ('memory', '我喜欢编程', 1, 1, 'atom')",
                [],
            )
            .unwrap();
        // 只把 bounded_memory_fts 降级成 unicode61（turns_fts 保持 jieba）
        db.conn()
            .execute_batch(
                "DROP TRIGGER IF EXISTS bounded_memory_ai;
                 DROP TRIGGER IF EXISTS bounded_memory_ad;
                 DROP TRIGGER IF EXISTS bounded_memory_au;
                 DROP TABLE IF EXISTS bounded_memory_fts;
                 CREATE VIRTUAL TABLE bounded_memory_fts USING fts5(
                     content, content='bounded_memory', content_rowid='id',
                     tokenize='unicode61 remove_diacritics 2'
                 );",
            )
            .unwrap();
    }
    {
        let db = Db::open(&path).unwrap();
        db.init_schema().unwrap();
        let bm_sql: String = db
            .conn()
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type='table' AND name='bounded_memory_fts'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            bm_sql.contains("jieba"),
            "bm_fts 必须迁移到 jieba: {bm_sql}"
        );
        let hits: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM bounded_memory_fts WHERE bounded_memory_fts MATCH '编程'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(hits, 1, "迁移后中文词级检索必须可用");
    }
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{}-wal", path.display()));
    let _ = std::fs::remove_file(format!("{}-shm", path.display()));
}

/// W2.4 回归：atom 回填批量路径复用 W13 守卫——全零/非有限向量跳过不落库
/// （防全零 int8 行 cosine NaN/NULL 吃 LIMIT k），有效向量正常插入。
#[test]
fn test_backfill_chunk_skips_invalid_vectors() {
    let mut db = Db::open_memory().unwrap();
    db.set_dimensions(4);
    db.init_schema().unwrap();
    db.conn()
        .execute_batch(
            "INSERT INTO bounded_memory (target, content, created_at, updated_at, memory_type)
             VALUES ('memory', 'zero atom', 1, 1, 'atom');
             INSERT INTO bounded_memory (target, content, created_at, updated_at, memory_type)
             VALUES ('memory', 'valid atom', 2, 2, 'atom');",
        )
        .unwrap();
    let chunk = vec![
        (1i64, "zero atom".to_string()),
        (2i64, "valid atom".to_string()),
    ];
    let embeddings = vec![vec![0.0f32; 4], vec![1.0f32, 0.0, 0.0, 0.0]];
    db.insert_backfill_vec_chunk(&chunk, &embeddings).unwrap();

    let ids: Vec<i64> = {
        let mut stmt = db
            .conn()
            .prepare("SELECT id FROM vec_bounded_memory ORDER BY id")
            .unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    };
    assert_eq!(ids, vec![2], "全零向量必须被跳过，仅有效向量落库");

    // 非有限同样跳过
    let nan_emb = vec![vec![f32::NAN, 0.0, 0.0, 0.0]];
    db.insert_backfill_vec_chunk(&[(1, "zero atom".to_string())], &nan_emb)
        .unwrap();
    let count: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM vec_bounded_memory", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 1, "非有限向量必须被跳过");
}

/// W8 回归：chunk/embeddings 长度不一致必须在开事务前拒绝——zip 会静默
/// 截断到较短一侧，让部分 atom 无声丢失向量索引且上层视同成功。
#[test]
fn test_backfill_chunk_rejects_length_mismatch() {
    let mut db = Db::open_memory().unwrap();
    db.set_dimensions(4);
    db.init_schema().unwrap();
    db.conn()
        .execute_batch(
            "INSERT INTO bounded_memory (target, content, created_at, updated_at, memory_type)
             VALUES ('memory', 'atom one', 1, 1, 'atom');
             INSERT INTO bounded_memory (target, content, created_at, updated_at, memory_type)
             VALUES ('memory', 'atom two', 2, 2, 'atom');",
        )
        .unwrap();
    let chunk = vec![
        (1i64, "atom one".to_string()),
        (2i64, "atom two".to_string()),
    ];
    let short = vec![vec![1.0f32, 0.0, 0.0, 0.0]];
    let err = db.insert_backfill_vec_chunk(&chunk, &short).unwrap_err();
    assert!(err.to_string().contains("长度不一致"), "got: {err}");
    assert!(
        db.conn().is_autocommit(),
        "mismatch must be rejected before opening a transaction"
    );
    let count: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM vec_bounded_memory", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 0, "no partial insert on mismatch");

    // embeddings 更长同样拒绝
    let long = vec![vec![1.0f32, 0.0, 0.0, 0.0]; 3];
    assert!(db.insert_backfill_vec_chunk(&chunk, &long).is_err());
}

/// D7-3 回归：MIGRATION_* 常量按 ';' 切分执行——不得含 CREATE TRIGGER
/// （BEGIN…END 体内的 ';' 会被拆碎静默空跑）。未来新增触发器迁移时本测试
/// 先行报警，提示改用独立执行路径。
#[test]
fn test_migration_constants_contain_no_trigger_bodies() {
    for sql in [
        schema::MIGRATION_P3_SQL,
        schema::MIGRATION_P8_ALTER_SQL,
        schema::MIGRATION_P8_INDEX_SQL,
        schema::MIGRATION_V26_EDITED_AT_SQL,
    ] {
        for stmt in migration_statements(sql) {
            let upper = stmt.to_ascii_uppercase();
            assert!(
                upper.starts_with("ALTER") || upper.starts_with("CREATE"),
                "迁移语句形状异常: {stmt}"
            );
            assert!(
                !upper.contains("BEGIN"),
                "MIGRATION_* 常量不得含触发器体（按 ';' 切分会拆碎）: {stmt}"
            );
        }
    }
}

#[test]
fn test_turns_fts_detects_orphans_on_empty_turns_and_backfill_clears_them() {
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();
    db.conn()
        .execute(
            "INSERT INTO turns_fts(rowid, preview) VALUES (999, 'orphan posting')",
            [],
        )
        .unwrap();

    assert!(
        db.turns_fts_needs_backfill().unwrap(),
        "empty turns with orphan FTS posting must trigger backfill"
    );
    db.backfill_turns_fts().unwrap();
    assert!(!db.turns_fts_needs_backfill().unwrap());
    let docsize: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM turns_fts_docsize", [], |r| r.get(0))
        .unwrap();
    assert_eq!(docsize, 0);
}

#[test]
fn test_maybe_backfill_bounded_memory_fts_heals_partial_divergence() {
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();
    let now = 1000i64;
    db.conn()
        .execute(
            "INSERT INTO bounded_memory (id, target, content, created_at, updated_at, confidence)
             VALUES (1, 'memory', 'first alpha', ?1, ?1, 'high'),
                    (2, 'memory', 'second beta', ?1, ?1, 'high')",
            [now],
        )
        .unwrap();
    db.conn()
        .execute(
            "INSERT INTO bounded_memory_fts(bounded_memory_fts, rowid, content)
             VALUES ('delete', 1, 'first alpha')",
            [],
        )
        .unwrap();
    let before_hits: i64 = db
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM bounded_memory_fts WHERE bounded_memory_fts MATCH 'alpha'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        before_hits, 0,
        "precondition: posting must be absent before rebuild"
    );

    db.maybe_backfill_bounded_memory_fts(false).unwrap();
    let hits: i64 = db
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM bounded_memory_fts WHERE bounded_memory_fts MATCH 'alpha'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        hits, 1,
        "partial FTS divergence must rebuild missing posting"
    );
}

#[test]
fn test_extract_dim_tag_case_insensitive() {
    assert_eq!(
        extract_dim_tag("CREATE VIRTUAL TABLE vec_turns USING vec0(embedding INT8[768])")
            .as_deref(),
        Some("int8[768]")
    );
    assert_eq!(
        extract_dim_tag("CREATE VIRTUAL TABLE vec_turns USING vec0(embedding FLOAT32[1024])")
            .as_deref(),
        Some("float32[1024]")
    );
    // 负向边界：无向量类型前缀、空串、未闭合方括号、非支持类型必须返回 None
    assert_eq!(
        extract_dim_tag("CREATE TABLE turns (id INTEGER PRIMARY KEY)"),
        None
    );
    assert_eq!(extract_dim_tag(""), None);
    assert_eq!(extract_dim_tag("embedding int8[1024"), None);
    assert_eq!(extract_dim_tag("embedding float16[1024]"), None);
    assert_eq!(
        extract_dim_tag("embedding InT8[512]").as_deref(),
        Some("int8[512]")
    );
}

#[test]
fn test_fts_v5_and_legacy_v0_to_v6_rebuilds_both_tables_from_09_postings() {
    for start_ver in [5i32, 0i32] {
        let dir = tempfile::TempDir::new().unwrap();
        let db_path = dir.path().join("memory.db");
        {
            let db = Db::open(&db_path).unwrap();
            db.init_schema().unwrap();
            db.conn()
                .execute(
                    "INSERT INTO sessions (session_id, start_ts, file_path, created_at, updated_at)
                     VALUES ('s-mig', 1, 's.jsonl', 1, 1)",
                    [],
                )
                .unwrap();
            db.conn()
                .execute(
                    "INSERT INTO turns (id, session_id, seq, timestamp_ms, role, preview)
                     VALUES (1, 's-mig', 1, 1000, 'user', 'asuna-memory bounded_memory_fts v3.0.0')",
                    [],
                )
                .unwrap();
            db.conn()
                .execute(
                    "INSERT INTO bounded_memory (id, target, content, created_at, updated_at)
                     VALUES (1, 'memory', 'asuna-memory config.json v3.0.0', 1000, 1000)",
                    [],
                )
                .unwrap();

            // 防假绿关键步：清空 0.11 重建出的词项，手工注入模拟 0.9 时代拆词后的旧 posting，
            // 并保持 docsize 行数与源表相等（让常规 backfill 计数校验盲区暴露出来）。
            db.conn()
                .execute_batch(&format!(
                    "INSERT INTO turns_fts(turns_fts) VALUES('delete-all');
                     INSERT INTO bounded_memory_fts(bounded_memory_fts) VALUES('delete-all');
                     INSERT INTO turns_fts(rowid, preview) VALUES (1, 'asuna memory bounded memory fts v3.0');
                     INSERT INTO bounded_memory_fts(rowid, content) VALUES (1, 'asuna memory config json v3.0');
                     PRAGMA user_version = {start_ver};"
                ))
                .unwrap();

            assert!(
                !db.turns_fts_needs_backfill().unwrap(),
                "start_ver={start_ver}: docsize count must equal turns count before v6 migration"
            );
            let turns_new_pre: i64 = db
                .conn()
                .query_row(
                    "SELECT COUNT(*) FROM turns_fts WHERE turns_fts MATCH '\"asuna-memory\"'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            let turns_old_pre: i64 = db
                .conn()
                .query_row(
                    "SELECT COUNT(*) FROM turns_fts WHERE turns_fts MATCH '\"asuna\"'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(turns_new_pre, 0);
            assert_eq!(turns_old_pre, 1);

            let bm_new_pre: i64 = db
                .conn()
                .query_row(
                    "SELECT COUNT(*) FROM bounded_memory_fts WHERE bounded_memory_fts MATCH '\"asuna-memory\"'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            let bm_old_pre: i64 = db
                .conn()
                .query_row(
                    "SELECT COUNT(*) FROM bounded_memory_fts WHERE bounded_memory_fts MATCH '\"config\"'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(bm_new_pre, 0);
            assert_eq!(bm_old_pre, 1);
        }

        // 重开并触发 v5/v0 -> v6 原子重建
        let db = Db::open(&db_path).unwrap();
        db.init_schema().unwrap();

        let ver: i32 = db
            .conn()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            ver, 6,
            "start_ver={start_ver} must migrate to user_version=6"
        );

        for term in ["\"asuna-memory\"", "\"bounded_memory_fts\"", "\"v3.0.0\""] {
            let hits: i64 = db
                .conn()
                .query_row(
                    "SELECT COUNT(*) FROM turns_fts WHERE turns_fts MATCH ?1",
                    [term],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(hits, 1, "start_ver={start_ver}: turns_fts missing {term}");
        }
        let turns_old_post: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM turns_fts WHERE turns_fts MATCH '\"asuna\"'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(turns_old_post, 0, "stale 0.9 token 'asuna' must be gone");

        for term in ["\"asuna-memory\"", "\"config.json\"", "\"v3.0.0\""] {
            let hits: i64 = db
                .conn()
                .query_row(
                    "SELECT COUNT(*) FROM bounded_memory_fts WHERE bounded_memory_fts MATCH ?1",
                    [term],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(
                hits, 1,
                "start_ver={start_ver}: bounded_memory_fts missing {term}"
            );
        }
        let bm_old_post: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM bounded_memory_fts WHERE bounded_memory_fts MATCH '\"config\"'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(bm_old_post, 0, "stale 0.9 token 'config' must be gone");

        db.conn()
            .execute(
                "INSERT INTO turns_fts(turns_fts) VALUES('integrity-check')",
                [],
            )
            .unwrap();
        db.conn()
            .execute(
                "INSERT INTO bounded_memory_fts(bounded_memory_fts, rank) VALUES('integrity-check', 1)",
                [],
            )
            .unwrap();

        // 触发器 DELETE 验证：不得因 0.9 vs 0.11 posting 不一致而报 FTS5_CORRUPT
        db.conn()
            .execute("DELETE FROM turns WHERE id = 1", [])
            .unwrap();
        db.conn()
            .execute("DELETE FROM bounded_memory WHERE id = 1", [])
            .unwrap();
        let turns_docsize: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM turns_fts_docsize", [], |r| r.get(0))
            .unwrap();
        let bm_docsize: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM bounded_memory_fts_docsize", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(turns_docsize, 0);
        assert_eq!(bm_docsize, 0);
    }
}

#[test]
fn test_fts_v5_to_v6_rollback_keeps_user_version_at_5_on_failure() {
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();
    db.conn()
        .execute(
            "INSERT INTO sessions (session_id, start_ts, file_path, created_at, updated_at)
             VALUES ('s-rb', 1, 's.jsonl', 1, 1)",
            [],
        )
        .unwrap();
    db.conn()
        .execute(
            "INSERT INTO turns (id, session_id, seq, timestamp_ms, role, preview)
             VALUES (1, 's-rb', 1, 1000, 'user', 'asuna-memory v3.0.0')",
            [],
        )
        .unwrap();
    db.conn()
        .execute(
            "INSERT INTO bounded_memory (id, target, content, created_at, updated_at)
             VALUES (1, 'memory', 'asuna-memory config.json', 1000, 1000)",
            [],
        )
        .unwrap();

    // 注入 0.9 posting 并置 user_version = 5，随后删除 bounded_memory_fts_data 制造事务中途失败
    db.conn()
        .execute_batch(
            "INSERT INTO turns_fts(turns_fts) VALUES('delete-all');
             INSERT INTO turns_fts(rowid, preview) VALUES (1, 'asuna memory v3.0');
             PRAGMA user_version = 5;
             DROP TABLE bounded_memory_fts_data;",
        )
        .unwrap();

    let err = db
        .init_schema()
        .expect_err("missing bounded_memory_fts_data must abort v5->v6 transaction");
    let _ = err;

    let ver: i32 = db
        .conn()
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        ver, 5,
        "failed v5->v6 rebuild must roll back user_version to 5"
    );

    let turns_old: i64 = db
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM turns_fts WHERE turns_fts MATCH '\"asuna\"'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let turns_new: i64 = db
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM turns_fts WHERE turns_fts MATCH '\"asuna-memory\"'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(turns_old, 1, "turns_fts rebuild must have rolled back");
    assert_eq!(turns_new, 0);
}

/// W19 回归：legacy FTS 的 DROP 与 SCHEMA_SQL/触发器/迁移/FTS 重建/user_version
/// 同事务——中途失败必须整体回滚，不得留下"旧 FTS 已删、新 FTS 未建"的半迁移态。
/// 构造：bounded_memory_fts 降级 unicode61（触发 legacy DROP），turns_fts 保持
/// jieba 但抽走 _data 影子表（SCHEMA_SQL 的 IF NOT EXISTS 不会重建它）→
/// rebuild_turns_fts_on 的 'delete-all' 在事务中途失败。
#[test]
fn test_w19_legacy_fts_drop_rolls_back_on_mid_migration_failure() {
    let path = temp_db_path();
    {
        let db = Db::open(&path).unwrap();
        db.init_schema().unwrap();
        db.conn()
            .execute_batch(
                "INSERT INTO sessions (session_id, start_ts, file_path, created_at, updated_at)
                     VALUES ('s-w19', 1, 's.jsonl', 1, 1);
                 DROP TRIGGER IF EXISTS bounded_memory_ai;
                 DROP TRIGGER IF EXISTS bounded_memory_ad;
                 DROP TRIGGER IF EXISTS bounded_memory_au;
                 DROP TABLE IF EXISTS bounded_memory_fts;
                 CREATE VIRTUAL TABLE bounded_memory_fts USING fts5(content, content='bounded_memory', content_rowid='id', tokenize='unicode61 remove_diacritics 2');
                 CREATE TRIGGER bounded_memory_ai AFTER INSERT ON bounded_memory BEGIN INSERT INTO bounded_memory_fts(rowid, content) VALUES (new.id, new.content); END;
                 INSERT INTO turns (session_id, seq, timestamp_ms, role, preview)
                     VALUES ('s-w19', 1, 1000, 'user', 'asuna memory');
                 DROP TABLE turns_fts_data;
                 PRAGMA user_version = 0;",
            )
            .unwrap();
    }

    let db = Db::open(&path).unwrap();
    assert!(
        db.init_schema().is_err(),
        "missing turns_fts_data must abort the migration transaction"
    );

    // legacy DROP 必须随事务回滚恢复——bounded_memory_fts 仍是 unicode61 旧表
    let bm_sql: String = db
        .conn()
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type='table' AND name='bounded_memory_fts'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        bm_sql.contains("unicode61"),
        "legacy bounded_memory_fts must be restored by rollback, got: {bm_sql}"
    );
    let bm_trigger: Option<String> = db
        .conn()
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type='trigger' AND name='bounded_memory_ai'",
            [],
            |r| r.get(0),
        )
        .optional()
        .unwrap();
    assert!(bm_trigger.is_some(), "legacy trigger must be restored too");
    // user_version 不得推进
    let ver: i32 = db
        .conn()
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(ver, 0, "failed migration must roll back user_version");

    drop(db);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{}-wal", path.display()));
    let _ = std::fs::remove_file(format!("{}-shm", path.display()));
}

#[test]
fn test_rusqlite_043_empty_sql_and_sqlite_vec_abi() {
    let mut db = Db::open_memory().unwrap();
    db.set_dimensions(3);
    db.init_schema().unwrap();

    assert!(matches!(db.conn().prepare(""), Err(rusqlite::Error::Empty)));
    assert!(matches!(
        db.conn().prepare("-- comment only"),
        Err(rusqlite::Error::Empty)
    ));

    let sqlite_ver: String = db
        .conn()
        .query_row("SELECT sqlite_version()", [], |r| r.get(0))
        .unwrap();
    assert!(
        sqlite_ver.starts_with("3.54."),
        "expected SQLite 3.54.x bundled in rusqlite 0.43, got: {sqlite_ver}"
    );
    let vec_ver: String = db
        .conn()
        .query_row("SELECT vec_version()", [], |r| r.get(0))
        .unwrap();
    assert!(
        vec_ver.starts_with("v0.1."),
        "unexpected sqlite-vec version: {vec_ver}"
    );

    db.conn()
        .execute(
            "INSERT INTO sessions (session_id, start_ts, file_path, created_at, updated_at)
             VALUES ('s-vec', 1, 's.jsonl', 1, 1)",
            [],
        )
        .unwrap();
    db.conn()
        .execute(
            "INSERT INTO turns (id, session_id, seq, timestamp_ms, role, preview)
             VALUES (10, 's-vec', 1, 1000, 'user', 'alpha'), (11, 's-vec', 2, 2000, 'user', 'beta')",
            [],
        )
        .unwrap();
    let v_x = crate::embedder::quantize_to_int8(&[1.0, 0.0, 0.0]);
    let v_y = crate::embedder::quantize_to_int8(&[0.0, 1.0, 0.0]);
    db.conn()
        .execute(
            "INSERT INTO vec_turns (rowid, embedding) VALUES (?1, vec_int8(?2)), (?3, vec_int8(?4))",
            rusqlite::params![10i64, v_x, 11i64, v_y],
        )
        .unwrap();
    db.conn()
        .execute(
            "INSERT INTO bounded_memory (id, target, content, created_at, updated_at)
             VALUES (20, 'memory', 'alpha', 1000, 1000), (21, 'memory', 'beta', 2000, 2000)",
            [],
        )
        .unwrap();
    db.conn()
        .execute(
            "INSERT INTO vec_bounded_memory (id, embedding) VALUES (?1, vec_int8(?2)), (?3, vec_int8(?4))",
            rusqlite::params![20i64, v_x, 21i64, v_y],
        )
        .unwrap();

    let best_turn: i64 = db
        .conn()
        .query_row(
            "SELECT rowid FROM vec_turns WHERE embedding MATCH vec_int8(?1) AND k = 1",
            rusqlite::params![v_x],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(best_turn, 10);

    let best_atom: i64 = db
        .conn()
        .query_row(
            "SELECT id FROM vec_bounded_memory WHERE embedding MATCH vec_int8(?1) AND k = 1",
            rusqlite::params![v_y],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(best_atom, 21);
}

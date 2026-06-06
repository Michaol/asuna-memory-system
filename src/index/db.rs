use rusqlite::Connection;
use std::ffi::c_char;
use std::path::Path;
use std::sync::Once;

use super::schema;

/// 确保 sqlite-vec 扩展只注册一次
static VEC_INIT: Once = Once::new();

fn ensure_vec_extension() {
    VEC_INIT.call_once(|| {
        // SAFETY: sqlite3_vec_init 与 sqlite3_auto_extension 期望的签名兼容
        // (sqlite3*, char**, const sqlite3_api_routines*) -> int
        //
        // 使用 std::ffi::c_char 而非硬编码 i8/u8 —— 这一点很关键：
        //   - x86_64 Linux / Windows / macOS Intel：c_char = i8
        //   - aarch64 Linux / ARM 平台：c_char = u8
        // 硬编码任一类型都会在另一类平台上编译失败（实测 aarch64-linux-gnu 报 E0308）。
        //
        // 前提：sqlite-vec 0.1.x 和 rusqlite 0.39.x 使用同一 bundled sqlite3 ABI。
        // 升级这两个 crate 时必须验证 ABI 兼容性。
        unsafe {
            let func: unsafe extern "C" fn(
                *mut rusqlite::ffi::sqlite3,
                *mut *mut c_char,
                *const rusqlite::ffi::sqlite3_api_routines,
            ) -> i32 = std::mem::transmute(sqlite_vec::sqlite3_vec_init as *const ());
            rusqlite::ffi::sqlite3_auto_extension(Some(func));
        }
    });
}

pub struct Db {
    conn: Connection,
    /// Target vector dimensions for vec0 tables. Must be set via `set_dimensions()`
    /// before `init_schema()`. Defaults to 1024 (matching `Config` default).
    dimensions: usize,
}

impl Db {
    /// 打开或创建数据库连接
    ///
    /// 启动时执行 `wal_checkpoint(TRUNCATE)`：将上次运行残留在 WAL 中的数据
    /// 刷入主 DB 文件。TRUNCATE 模式会等待读写者释放并截断 WAL 文件，
    /// 确保外部工具（如 `asuna-memory sql`）能看到完整数据。
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        ensure_vec_extension();
        let conn = Connection::open(path)?;
        // Register jieba FTS5 tokenizer (must be called per-connection, before any FTS5 ops)
        sqlite_jieba_tokenizer::load(&conn).map_err(|e| anyhow::anyhow!("jieba tokenizer: {}", e))?;
        Self::register_functions(&conn)?;
        conn.pragma_update(None, "journal_mode", "wal")?;
        conn.pragma_update(None, "synchronous", "normal")?;
        conn.pragma_update(None, "busy_timeout", "5000")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        // 启动时强制 checkpoint — 把上次运行残留的 WAL 数据刷入主 DB
        conn.pragma_update(None, "wal_checkpoint", "TRUNCATE")?;
        Ok(Self { conn, dimensions: 1024 })
    }

    /// 内存数据库（仅测试使用）
    #[cfg(test)]
    pub fn open_memory() -> anyhow::Result<Self> {
        ensure_vec_extension();
        let conn = Connection::open_in_memory()?;
        // Register jieba FTS5 tokenizer
        sqlite_jieba_tokenizer::load(&conn).map_err(|e| anyhow::anyhow!("jieba tokenizer: {}", e))?;
        Self::register_functions(&conn)?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        Ok(Self { conn, dimensions: 1024 })
    }

    /// Set target vector dimensions. Call before init_schema() to configure vec0 tables.
    pub fn set_dimensions(&mut self, d: usize) {
        self.dimensions = d;
    }

    /// Get the configured target dimensions.
    pub fn dimensions(&self) -> usize {
        self.dimensions
    }

    fn register_functions(conn: &Connection) -> anyhow::Result<()> {
        // [Deprecated] tokenize_zh UDF: 保留向后兼容，但 FTS 触发器已改用 jieba tokenizer，
        // 不再依赖此 UDF。外部工具（如 asuna-memory sql）仍可使用。
        conn.create_scalar_function(
            "tokenize_zh",
            1,
            rusqlite::functions::FunctionFlags::SQLITE_DETERMINISTIC,
            move |ctx| {
                let text = ctx.get::<String>(0)?;
                Ok(crate::util::text::tokenize_chinese(&text))
            },
        )?;
        Ok(())
    }

    /// 执行建表
    pub fn init_schema(&self) -> anyhow::Result<()> {
        // 检测旧版 FTS 架构并标记需要迁移
        let old_fts: Result<String, _> = self.conn.query_row(
            "SELECT sql FROM sqlite_master WHERE type='table' AND name='turns_fts'",
            [],
            |r| r.get(0),
        );

        let mut needs_fts_rebuild = false;
        if let Ok(sql) = old_fts {
            // 旧版 external-content 模式（content=turns）→ 需要迁移
            if sql.contains("content=turns") || sql.contains("content='turns'") {
                tracing::warn!("检测到旧版 external-content FTS 架构，正在迁移...");
                self.conn.execute("DROP TABLE IF EXISTS turns_fts", [])?;
                needs_fts_rebuild = true;
            }
            // 旧版 unicode61 tokenizer → 需要迁移到 jieba
            else if sql.contains("unicode61") {
                tracing::warn!("检测到旧版 unicode61 FTS tokenizer，正在迁移到 jieba...");
                self.conn.execute_batch(
                    "DROP TRIGGER IF EXISTS turns_ai;
                     DROP TRIGGER IF EXISTS turns_ad;
                     DROP TRIGGER IF EXISTS turns_au;
                     DROP TABLE IF EXISTS turns_fts;
                     DROP TRIGGER IF EXISTS bounded_memory_ai;
                     DROP TRIGGER IF EXISTS bounded_memory_ad;
                     DROP TRIGGER IF EXISTS bounded_memory_au;
                     DROP TABLE IF EXISTS bounded_memory_fts;",
                )?;
                needs_fts_rebuild = true;
            }
        }

        self.conn.execute_batch(schema::SCHEMA_SQL)?;
        self.conn.execute_batch(schema::FTS_TRIGGERS_SQL)?;

        // Run migrations BEFORE backfill so all columns exist
        self.run_migration_p3()?;
        self.run_migration_p8()?;

        // Backfill bounded_memory_fts if the FTS table is empty but bounded_memory has entries
        self.maybe_backfill_bounded_memory_fts()?;

        if needs_fts_rebuild {
            tracing::info!("重建 FTS 索引（jieba tokenizer）...");
            let mut stmt = self
                .conn
                .prepare("SELECT id, preview FROM turns WHERE preview IS NOT NULL")?;
            let rows = stmt.query_map([], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?;
            for row in rows {
                let (id, preview) = row?;
                // jieba tokenizer 在 FTS5 引擎内自动分词，无需预处理
                self.conn.execute(
                    "INSERT INTO turns_fts(rowid, preview) VALUES (?1, ?2)",
                    rusqlite::params![id, preview],
                )?;
            }
            tracing::info!("FTS 索引重建完成");
        }

        // 创建向量虚拟表（维度由 self.dimensions 决定）
        let dim = self.dimensions;
        let vec_turns_ddl = format!(
            "CREATE VIRTUAL TABLE IF NOT EXISTS vec_turns USING vec0(embedding int8[{dim}]);"
        );
        let vec_bm_ddl = format!(
            "CREATE VIRTUAL TABLE IF NOT EXISTS vec_bounded_memory USING vec0(
                id INTEGER PRIMARY KEY,
                embedding int8[{dim}]
            );"
        );
        self.conn.execute_batch(&vec_turns_ddl)?;
        self.conn.execute_batch(&vec_bm_ddl)?;

        // 迁移：检测 vec_turns 维度不匹配，自动删除重建
        let target_tag = format!("int8[{dim}]");
        let vec_schema: Result<String, _> = self.conn.query_row(
            "SELECT sql FROM sqlite_master WHERE name='vec_turns'",
            [],
            |r| r.get(0),
        );
        if let Ok(sql) = vec_schema {
            if !sql.contains(&target_tag) {
                tracing::warn!(
                    "vec_turns 维度不匹配（现有: {}, 目标: {dim}），正在重建...",
                    extract_dim_tag(&sql).unwrap_or_else(|| "unknown".into())
                );
                self.conn.execute("DROP TABLE vec_turns", [])?;
                self.conn.execute_batch(&vec_turns_ddl)?;
            }
        }

        // 迁移：检测 vec_bounded_memory 维度或类型不匹配，自动删除重建
        let vec_bm_schema: Result<String, _> = self.conn.query_row(
            "SELECT sql FROM sqlite_master WHERE name='vec_bounded_memory'",
            [],
            |r| r.get(0),
        );
        if let Ok(sql) = vec_bm_schema {
            if sql.contains("float32") || !sql.contains(&target_tag) {
                tracing::warn!(
                    "vec_bounded_memory 维度不匹配（现有: {}, 目标: {dim}），正在重建...",
                    extract_dim_tag(&sql).unwrap_or_else(|| "unknown".into())
                );
                self.conn.execute("DROP TABLE vec_bounded_memory", [])?;
                self.conn.execute_batch(&vec_bm_ddl)?;
            }
        }

        Ok(())
    }

    /// Run P3 migration: add memory_type, supersedes_id, source_turn_ids, confidence_score
    ///
    /// Always attempts each ALTER TABLE. "duplicate column" errors are silently
    /// skipped, making this safe to run on any database state — including partial
    /// migrations where some columns exist but others don't.
    fn run_migration_p3(&self) -> anyhow::Result<()> {
        for stmt in schema::MIGRATION_P3_SQL.split(';') {
            let stmt = stmt.trim();
            if stmt.is_empty() || stmt.starts_with("--") {
                continue;
            }
            match self.conn.execute_batch(stmt) {
                Ok(_) => {
                    if stmt.starts_with("ALTER") {
                        tracing::info!("P3 migration: added column");
                    }
                }
                Err(e) if e.to_string().contains("duplicate column") => {
                    // Column already exists, skip
                }
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }

    /// Run P8 migration: add memory_atom_id to entities and relation_kind to relations
    ///
    /// Same idempotent approach as run_migration_p3.
    fn run_migration_p8(&self) -> anyhow::Result<()> {
        for sql_stmt in schema::MIGRATION_P8_ALTER_SQL.split(';') {
            let sql_stmt = sql_stmt.trim();
            if sql_stmt.is_empty() || sql_stmt.starts_with("--") {
                continue;
            }
            match self.conn.execute_batch(sql_stmt) {
                Ok(_) => {}
                Err(e) if e.to_string().contains("duplicate column") => {
                    // Column already exists, skip
                }
                Err(e) => return Err(e.into()),
            }
        }

        for sql_stmt in schema::MIGRATION_P8_INDEX_SQL.split(';') {
            let sql_stmt = sql_stmt.trim();
            if sql_stmt.is_empty() || sql_stmt.starts_with("--") {
                continue;
            }
            self.conn.execute_batch(sql_stmt)?;
        }

        Ok(())
    }

    /// Backfill vec_bounded_memory for atoms that have no vector index.
    ///
    /// This handles the case where vec_bounded_memory was wiped (e.g., after a
    /// float32→int8 schema migration) but bounded_memory entries still exist.
    /// Without this, semantic search on bounded memory silently degrades to FTS.
    pub fn maybe_backfill_bounded_memory_vec(
        &self,
        embedder: &crate::embedder::LazyEmbedder,
    ) -> anyhow::Result<()> {
        // 1. Collect all atom entries that need vectors
        let atoms: Vec<(i64, String)> = {
            let mut stmt = self.conn.prepare(
                "SELECT id, content FROM bounded_memory
                 WHERE COALESCE(memory_type, 'manual') = 'atom'
                   AND content IS NOT NULL",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?;
            rows.filter_map(|r| r.ok()).collect()
        };

        if atoms.is_empty() {
            return Ok(());
        }

        // 2. Find which ones already have vectors
        let existing_ids: std::collections::HashSet<i64> = {
            let mut stmt = self
                .conn
                .prepare("SELECT id FROM vec_bounded_memory")?;
            let rows = stmt.query_map([], |row| row.get::<_, i64>(0))?;
            rows.filter_map(|r| r.ok()).collect()
        };

        let pending: Vec<(i64, String)> = atoms
            .into_iter()
            .filter(|(id, _)| !existing_ids.contains(id))
            .collect();

        if pending.is_empty() {
            return Ok(());
        }

        tracing::info!(
            "Backfilling vec_bounded_memory: {} atoms need embeddings...",
            pending.len()
        );

        // 3. Batch embed (size from embedder config) + transactional insert
        let batch_size = embedder.batch_size().max(1);

        for chunk in pending.chunks(batch_size) {
            let texts: Vec<&str> = chunk.iter().map(|(_, c)| c.as_str()).collect();
            let embeddings = match embedder.embed_documents(&texts) {
                Ok(embs) => embs,
                Err(e) => {
                    tracing::warn!("vec_bounded_memory backfill: batch embed failed: {}", e);
                    continue;
                }
            };

            self.conn.execute_batch("BEGIN IMMEDIATE")?;
            for ((id, _), embedding) in chunk.iter().zip(embeddings.iter()) {
                let bytes = crate::embedder::onnx::quantize_to_int8(embedding);
                if let Err(e) = self.conn.execute(
                    "INSERT INTO vec_bounded_memory (id, embedding) VALUES (?1, vec_int8(?2))",
                    rusqlite::params![id, bytes],
                ) {
                    tracing::warn!("vec_bounded_memory backfill: insert id={} failed: {}", id, e);
                }
            }
            self.conn.execute_batch("COMMIT")?;
        }

        let final_count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM vec_bounded_memory", [], |r| {
                r.get(0)
            })
            .unwrap_or(0);

        tracing::info!(
            "vec_bounded_memory backfill complete: {} vectors total",
            final_count
        );
        Ok(())
    }

    /// Backfill bounded_memory_fts from the bounded_memory source table.
    ///
    /// For external-content FTS5 tables, `SELECT COUNT(*)` may delegate to the content
    /// table and return a non-zero count even when the FTS index is empty. To avoid this,
    /// we use the FTS5 `'rebuild'` command which reliably re-indexes from the content table.
    /// This is called on every startup — idempotent and fast for small tables (v2.4.0 fix).
    fn maybe_backfill_bounded_memory_fts(&self) -> anyhow::Result<()> {
        let bm_count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM bounded_memory", [], |r| r.get(0))
            .unwrap_or(0);

        if bm_count == 0 {
            return Ok(()); // Nothing to backfill
        }

        // Use FTS5 'rebuild' command: deletes all FTS index entries and re-indexes
        // from the content table. This is the reliable way to sync external-content
        // FTS5 tables — COUNT(*) based checks are unreliable for this table type.
        self.conn.execute(
            "INSERT INTO bounded_memory_fts(bounded_memory_fts) VALUES('rebuild')",
            [],
        )?;

        tracing::info!("bounded_memory_fts rebuild complete ({} source entries)", bm_count);
        Ok(())
    }

    /// 获取底层连接引用
    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    /// 完整性校验
    pub fn integrity_check(&self) -> anyhow::Result<bool> {
        let result: String = self
            .conn
            .query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
        Ok(result == "ok")
    }

    /// 获取 journal_mode（仅测试使用）
    #[cfg(test)]
    pub fn journal_mode(&self) -> anyhow::Result<String> {
        let mode: String = self
            .conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))?;
        Ok(mode)
    }
}

/// Extract the dimension tag from a vec0 CREATE TABLE SQL statement.
/// E.g. "CREATE VIRTUAL TABLE ... USING vec0(embedding int8[768])" → "int8[768]"
fn extract_dim_tag(sql: &str) -> Option<String> {
    // Look for patterns like "int8[768]" or "float32[1024]"
    let start = sql.find("int8[").or_else(|| sql.find("float32["))?;
    let end = sql[start..].find(']')? + start + 1;
    Some(sql[start..end].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
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

        // 验证表存在
        let tables: Vec<String> = db
            .conn()
            .prepare(
                "SELECT name FROM sqlite_master WHERE type='table' OR type='view' ORDER BY name",
            )
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();

        assert!(tables.contains(&"sessions".to_string()));
        assert!(tables.contains(&"turns".to_string()));
        assert!(tables.contains(&"bounded_memory".to_string()));
        assert!(tables.contains(&"audit_log".to_string()));
        assert!(tables.contains(&"entities".to_string()));
        assert!(tables.contains(&"relations".to_string()));
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
        assert!(sql.contains("int8[1024]"), "vec_turns should use int8[1024], got: {}", sql);

        // Verify vec_bounded_memory also uses 1024
        let sql2: String = db
            .conn()
            .query_row(
                "SELECT sql FROM sqlite_master WHERE name='vec_bounded_memory'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(sql2.contains("int8[1024]"), "vec_bounded_memory should use int8[1024], got: {}", sql2);
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
            assert!(sql.contains("int8[1024]"), "should migrate to 1024, got: {}", sql);
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
            let trigger_sql: String = db.conn().query_row(
                "SELECT sql FROM sqlite_master WHERE type='trigger' AND name='turns_ai'",
                [], |r| r.get(0),
            ).unwrap();
            assert!(!trigger_sql.contains("tokenize_zh"),
                "trigger should not reference tokenize_zh after migration, got: {}", trigger_sql);

            // Verify FTS table uses jieba
            let fts_sql: String = db.conn().query_row(
                "SELECT sql FROM sqlite_master WHERE type='table' AND name='turns_fts'",
                [], |r| r.get(0),
            ).unwrap();
            assert!(fts_sql.contains("jieba"),
                "turns_fts should use jieba tokenizer, got: {}", fts_sql);

            // Verify bounded_memory_fts also uses jieba
            let bm_fts_sql: String = db.conn().query_row(
                "SELECT sql FROM sqlite_master WHERE type='table' AND name='bounded_memory_fts'",
                [], |r| r.get(0),
            ).unwrap();
            assert!(bm_fts_sql.contains("jieba"),
                "bounded_memory_fts should use jieba tokenizer, got: {}", bm_fts_sql);
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
            let bm_count: i64 = db.conn().query_row(
                "SELECT COUNT(*) FROM bounded_memory", [], |r| r.get(0),
            ).unwrap();
            assert_eq!(bm_count, 3);
        }

        // Phase 2: Simulate FTS table being emptied (like jieba migration)
        {
            let db = Db::open(&path).unwrap();
            // Drop and recreate FTS table empty (simulating what jieba migration does)
            db.conn().execute_batch(
                "DROP TRIGGER IF EXISTS bounded_memory_ai;
                 DROP TRIGGER IF EXISTS bounded_memory_ad;
                 DROP TRIGGER IF EXISTS bounded_memory_au;
                 DROP TABLE IF EXISTS bounded_memory_fts;
                 CREATE VIRTUAL TABLE bounded_memory_fts USING fts5(
                     content, content='bounded_memory', content_rowid='id', tokenize='jieba'
                 );",
            ).unwrap();

            // Now FTS index is empty but bounded_memory has 3 entries
        }

        // Phase 3: Reopen — init_schema should detect and backfill
        {
            let db = Db::open(&path).unwrap();
            db.init_schema().unwrap();

            // Verify FTS now has data: search for "编程" should find the entry
            let count: i64 = db.conn().query_row(
                "SELECT COUNT(*) FROM bounded_memory_fts WHERE bounded_memory_fts MATCH '编程'",
                [], |r| r.get(0),
            ).unwrap();
            assert!(count > 0, "bounded_memory_fts should be backfilled, '编程' search returned 0");

            // Search for "数据库" should also work
            let count2: i64 = db.conn().query_row(
                "SELECT COUNT(*) FROM bounded_memory_fts WHERE bounded_memory_fts MATCH '数据库'",
                [], |r| r.get(0),
            ).unwrap();
            assert!(count2 > 0, "bounded_memory_fts should be backfilled, '数据库' search returned 0");

            // Search for "Rust" should also work
            let count3: i64 = db.conn().query_row(
                "SELECT COUNT(*) FROM bounded_memory_fts WHERE bounded_memory_fts MATCH 'Rust'",
                [], |r| r.get(0),
            ).unwrap();
            assert!(count3 > 0, "bounded_memory_fts should be backfilled, 'Rust' search returned 0");
        }

        // 清理
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{}-wal", path.display()));
        let _ = std::fs::remove_file(format!("{}-shm", path.display()));
    }
}

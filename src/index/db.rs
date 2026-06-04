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
        // 前提：sqlite-vec 0.1.x 和 rusqlite 0.32.x 使用同一 bundled sqlite3 ABI。
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
        Self::register_functions(&conn)?;
        conn.pragma_update(None, "journal_mode", "wal")?;
        conn.pragma_update(None, "synchronous", "normal")?;
        conn.pragma_update(None, "busy_timeout", "5000")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        // 启动时强制 checkpoint — 把上次运行残留的 WAL 数据刷入主 DB
        conn.pragma_update(None, "wal_checkpoint", "TRUNCATE")?;
        Ok(Self { conn })
    }

    /// 内存数据库（仅测试使用）
    #[cfg(test)]
    pub fn open_memory() -> anyhow::Result<Self> {
        ensure_vec_extension();
        let conn = Connection::open_in_memory()?;
        Self::register_functions(&conn)?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        Ok(Self { conn })
    }

    fn register_functions(conn: &Connection) -> anyhow::Result<()> {
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
        let old_schema: Result<String, _> = self.conn.query_row(
            "SELECT sql FROM sqlite_master WHERE type='table' AND name='turns_fts'",
            [],
            |r| r.get(0),
        );

        let mut needs_rebuild = false;
        if let Ok(sql) = old_schema {
            if sql.contains("content=turns") || sql.contains("content='turns'") {
                tracing::warn!(
                    "检测到旧版 external-content FTS 架构，正在自动迁移为 contentless..."
                );
                self.conn.execute("DROP TABLE turns_fts", [])?;
                needs_rebuild = true;
            }
        }

        self.conn.execute_batch(schema::SCHEMA_SQL)?;
        self.conn.execute_batch(schema::FTS_TRIGGERS_SQL)?;

        // Run migrations BEFORE backfill so all columns exist
        self.run_migration_p3()?;
        self.run_migration_p8()?;

        // Backfill bounded_memory_fts if the FTS table is empty but bounded_memory has entries
        self.maybe_backfill_bounded_memory_fts()?;

        if needs_rebuild {
            tracing::info!("向新架构自动恢复 FTS 索引...");
            let mut stmt = self
                .conn
                .prepare("SELECT id, preview FROM turns WHERE preview IS NOT NULL")?;
            let rows = stmt.query_map([], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?;
            for row in rows {
                let (id, preview) = row?;
                let tokenized = crate::util::text::tokenize_chinese(&preview);
                self.conn.execute(
                    "INSERT INTO turns_fts(rowid, preview) VALUES (?1, ?2)",
                    rusqlite::params![id, tokenized],
                )?;
            }
        }

        // 创建向量虚拟表
        self.conn.execute_batch(
            "CREATE VIRTUAL TABLE IF NOT EXISTS vec_turns USING vec0(embedding int8[768]);",
        )?;

        // 创建 bounded_memory 向量索引表（int8 量化，与 vec_turns 一致）
        self.conn.execute_batch(
            "CREATE VIRTUAL TABLE IF NOT EXISTS vec_bounded_memory USING vec0(
                id INTEGER PRIMARY KEY,
                embedding int8[768]
            );",
        )?;

        // 迁移：如果现有数据库是旧版 384 维向量表，删除重建
        let vec_schema: Result<String, _> = self.conn.query_row(
            "SELECT sql FROM sqlite_master WHERE name='vec_turns'",
            [],
            |r| r.get(0),
        );
        if let Ok(sql) = vec_schema {
            if sql.contains("int8[384]") {
                tracing::warn!("检测到旧版 384 维向量表，正在重建为 768 维...");
                self.conn.execute("DROP TABLE vec_turns", [])?;
                self.conn.execute_batch(
                    "CREATE VIRTUAL TABLE vec_turns USING vec0(embedding int8[768]);",
                )?;
            }
        }

        // 迁移：vec_bounded_memory float32[768] → int8[768]
        // 已有数据会被丢弃（下次管线运行时重新嵌入）
        let vec_bm_schema: Result<String, _> = self.conn.query_row(
            "SELECT sql FROM sqlite_master WHERE name='vec_bounded_memory'",
            [],
            |r| r.get(0),
        );
        if let Ok(sql) = vec_bm_schema {
            if sql.contains("float32") {
                tracing::warn!("检测到旧版 float32 vec_bounded_memory 表，正在重建为 int8...");
                self.conn.execute("DROP TABLE vec_bounded_memory", [])?;
                self.conn.execute_batch(
                    "CREATE VIRTUAL TABLE vec_bounded_memory USING vec0(
                        id INTEGER PRIMARY KEY,
                        embedding int8[768]
                    );",
                )?;
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

        // 3. Batch embed (32 per batch, matching rebuild.rs) + transactional insert
        const BATCH_SIZE: usize = 32;

        for chunk in pending.chunks(BATCH_SIZE) {
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

    /// Backfill bounded_memory_fts if the FTS table is empty but bounded_memory has entries.
    /// This handles migration from databases created before bounded_memory_fts existed.
    fn maybe_backfill_bounded_memory_fts(&self) -> anyhow::Result<()> {
        let fts_count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM bounded_memory_fts", [], |r| r.get(0))
            .unwrap_or(0);

        if fts_count > 0 {
            return Ok(()); // Already populated
        }

        let bm_count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM bounded_memory", [], |r| r.get(0))
            .unwrap_or(0);

        if bm_count == 0 {
            return Ok(()); // Nothing to backfill
        }

        tracing::info!(
            "Backfilling bounded_memory_fts: {} entries to index...",
            bm_count
        );

        let mut stmt = self
            .conn
            .prepare("SELECT id, content FROM bounded_memory")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?;

        for row in rows {
            let (id, content) = row?;
            let tokenized = crate::util::text::tokenize_chinese(&content);
            self.conn.execute(
                "INSERT INTO bounded_memory_fts(rowid, content) VALUES (?1, ?2)",
                rusqlite::params![id, tokenized],
            )?;
        }

        tracing::info!("bounded_memory_fts backfill complete");
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
}

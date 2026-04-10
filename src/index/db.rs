use std::path::Path;
use std::sync::Once;
use rusqlite::Connection;

use super::schema;

/// 确保 sqlite-vec 扩展只注册一次
static VEC_INIT: Once = Once::new();

fn ensure_vec_extension() {
    VEC_INIT.call_once(|| {
        unsafe {
            rusqlite::ffi::sqlite3_auto_extension(
                Some(std::mem::transmute(
                    sqlite_vec::sqlite3_vec_init as *const (),
                )),
            );
        }
    });
}

pub struct Db {
    conn: Connection,
}

impl Db {
    /// 打开或创建数据库连接
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        ensure_vec_extension();
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "wal")?;
        conn.pragma_update(None, "synchronous", "normal")?;
        conn.pragma_update(None, "busy_timeout", "5000")?;
        Ok(Self { conn })
    }

    /// 内存数据库（用于测试）
    pub fn open_memory() -> anyhow::Result<Self> {
        ensure_vec_extension();
        let conn = Connection::open_in_memory()?;
        Ok(Self { conn })
    }

    /// 执行建表
    pub fn init_schema(&self) -> anyhow::Result<()> {
        self.conn.execute_batch(schema::SCHEMA_SQL)?;
        self.conn.execute_batch(schema::FTS_TRIGGERS_SQL)?;

        // 创建向量虚拟表
        self.conn.execute_batch(
            "CREATE VIRTUAL TABLE IF NOT EXISTS vec_turns USING vec0(embedding int8[384]);"
        )?;

        Ok(())
    }

    /// 获取底层连接引用
    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    /// 完整性校验
    pub fn integrity_check(&self) -> anyhow::Result<bool> {
        let result: String = self.conn.query_row(
            "PRAGMA integrity_check",
            [],
            |row| row.get(0),
        )?;
        Ok(result == "ok")
    }

    /// 获取 journal_mode
    pub fn journal_mode(&self) -> anyhow::Result<String> {
        let mode: String = self.conn.query_row(
            "PRAGMA journal_mode",
            [],
            |row| row.get(0),
        )?;
        Ok(mode)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_db_path() -> std::path::PathBuf {
        let ts = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        std::env::temp_dir().join(format!("asuna_test_{}.db", ts))
    }

    #[test]
    fn test_open_and_init() {
        let db = Db::open_memory().unwrap();
        db.init_schema().unwrap();

        // 验证表存在
        let tables: Vec<String> = db.conn()
            .prepare("SELECT name FROM sqlite_master WHERE type='table' OR type='view' ORDER BY name")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();

        assert!(tables.contains(&"sessions".to_string()));
        assert!(tables.contains(&"turns".to_string()));
        assert!(tables.contains(&"bounded_memory".to_string()));
        assert!(tables.contains(&"audit_log".to_string()));
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
        let has_vec: bool = db.conn()
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name='vec_turns'",
                [],
                |r| r.get::<_, i64>(0).map(|c| c > 0),
            )
            .unwrap();
        assert!(has_vec, "vec_turns virtual table should exist");
    }
}

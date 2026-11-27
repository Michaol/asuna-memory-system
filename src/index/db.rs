use rusqlite::Connection;
use std::path::Path;

use super::db_vec::ensure_vec_extension;
#[cfg(test)]
use super::{db_migrations::migration_statements, db_vec::extract_dim_tag, schema};

pub use super::db_backfill::{
    is_permanent_backfill_error, run_backfill_chunks_with_control, BackfillChunkOutcome,
    MAX_CONSECUTIVE_BACKFILL_FAILURES,
};

fn is_readonly_pragma(name: &str, has_value: bool) -> bool {
    let lower = name.to_ascii_lowercase();
    let introspection = matches!(
        lower.as_str(),
        "table_info"
            | "table_xinfo"
            | "table_list"
            | "index_list"
            | "index_info"
            | "index_xinfo"
            | "foreign_key_list"
            | "database_list"
            | "compile_options"
            | "integrity_check"
            | "quick_check"
            | "pragma_list"
            | "collation_list"
            | "function_list"
            | "module_list"
    );
    if introspection {
        return true;
    }
    let readable_state = matches!(
        lower.as_str(),
        "user_version"
            | "schema_version"
            | "page_count"
            | "page_size"
            | "freelist_count"
            | "journal_mode"
            | "foreign_keys"
            | "query_only"
            // W6: FTS5 xIntegrity 内部读 PRAGMA data_version（PragFlg_ReadOnly
            // 头值计数器，纯内省）——缺它 integrity_check/quick_check 报 SQLITE_AUTH。
            | "data_version"
    );
    readable_state && !has_value
}

fn authorize_readonly_action(
    ctx: rusqlite::hooks::AuthContext<'_>,
) -> rusqlite::hooks::Authorization {
    use rusqlite::hooks::{AuthAction, Authorization};
    match ctx.action {
        AuthAction::Select
        | AuthAction::Read { .. }
        | AuthAction::Function { .. }
        | AuthAction::Recursive => Authorization::Allow,
        AuthAction::Pragma {
            pragma_name,
            pragma_value,
        } if is_readonly_pragma(pragma_name, pragma_value.is_some()) => Authorization::Allow,
        _ => Authorization::Deny,
    }
}

pub struct Db {
    pub(super) conn: Connection,
    /// Target vector dimensions for vec0 tables. Must be set via `set_dimensions()`
    /// before `init_schema()`. Defaults to 1024 (matching `Config` default).
    pub(super) dimensions: usize,
}

impl Db {
    /// 打开或创建数据库连接（WAL 模式、`busy_timeout = 5000ms`、`foreign_keys = ON`）
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        ensure_vec_extension()?;
        // D4-1: 与 open_readonly 统一 flags，显式不含 SQLITE_OPEN_URI（rusqlite
        // 默认含 URI）——db_path 恒为普通文件系统路径（profile_db_path），排除
        // URI 解释避免 file: 前缀路径指向不同库 + URI 参数注入面。
        let conn = Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
                | rusqlite::OpenFlags::SQLITE_OPEN_CREATE
                | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        // Register jieba FTS5 tokenizer (must be called per-connection, before any FTS5 ops)
        super::jieba_tokenizer::register(&conn)?;
        Self::register_functions(&conn)?;
        conn.pragma_update(None, "busy_timeout", "5000")?;
        conn.pragma_update(None, "journal_mode", "wal")?;
        conn.pragma_update(None, "synchronous", "normal")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        Ok(Self {
            conn,
            dimensions: crate::embedder::EMBEDDING_DIMENSIONS,
        })
    }

    /// Open an existing database strictly read-only for `asuna-memory sql`, with
    /// `SQLITE_OPEN_READ_ONLY`, `query_only = ON`, and an SQLite authorizer blocking
    /// all writes, schema changes, ATTACH/DETACH, writable PRAGMAs, and transactions.
    pub fn open_readonly(path: &Path) -> anyhow::Result<Self> {
        ensure_vec_extension()?;
        let mut conn = Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        super::jieba_tokenizer::register(&conn)?;
        Self::register_functions(&conn)?;
        conn.pragma_update(None, "busy_timeout", "5000")?;
        conn.pragma_update(None, "query_only", "ON")?;
        conn.authorizer(Some(authorize_readonly_action))?;
        Ok(Self {
            conn,
            dimensions: crate::embedder::EMBEDDING_DIMENSIONS,
        })
    }

    /// 内存数据库（仅测试使用）
    #[cfg(test)]
    pub fn open_memory() -> anyhow::Result<Self> {
        ensure_vec_extension()?;
        let conn = Connection::open_in_memory()?;
        // Register jieba FTS5 tokenizer
        super::jieba_tokenizer::register(&conn)?;
        Self::register_functions(&conn)?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        Ok(Self {
            conn,
            dimensions: crate::embedder::EMBEDDING_DIMENSIONS,
        })
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

    /// Open an RAII `IMMEDIATE` write transaction (automatic rollback on drop).
    pub fn immediate_tx(&self) -> rusqlite::Result<rusqlite::Transaction<'_>> {
        rusqlite::Transaction::new_unchecked(&self.conn, rusqlite::TransactionBehavior::Immediate)
    }

    /// Roll back any open dangling transaction left behind after a recovered panic.
    pub fn rollback_if_in_tx(&self) {
        if !self.conn.is_autocommit() {
            if let Err(e) = self.conn.execute_batch("ROLLBACK") {
                tracing::error!("Failed to rollback dangling transaction: {}", e);
            }
        }
    }

    /// 执行建表
    pub fn init_schema(&self) -> anyhow::Result<()> {
        // v4: D2-7 NULL preview 触发器守卫——bump 版本让既有库也重放 FTS_TRIGGERS_SQL。
        // v5: A1/W4.3 capture_dedup 幂等表——bump 让既有库重放 SCHEMA_SQL 建表
        // （CREATE TABLE IF NOT EXISTS，幂等）。
        // v6: jieba-rs 0.9 -> 0.11.0（. / _ / - 连接符不再拆词），既有 FTS5 索引
        // （turns_fts + bounded_memory_fts）必须在单事务内随 user_version 原子重建，
        // 避免查询失配及 DELETE/UPDATE 触发器 'delete' 命令触发 FTS5_CORRUPT。
        const CURRENT_SCHEMA_VERSION: i32 = 6;
        const JIEBA_011_SCHEMA_VERSION: i32 = 6;

        // D2-13: 版本/触发器计数读失败必须上抛——unwrap_or(0) 会把读故障伪装成
        // 旧库（空跑迁移）或触发器齐全（跳过修复）。
        // W19: 检测（含 legacy FTS DROP）+ SCHEMA_SQL + 触发器 + P3/P8/v26 迁移 +
        // FTS 重建 + user_version 全部在同一 IMMEDIATE 事务内——任一步失败整体
        // 回滚，不再留下"旧 FTS 已删、新 FTS 未建"的半迁移态。
        let tx = self.immediate_tx()?;
        let user_version: i32 = tx.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        // 在 SCHEMA_SQL 执行前检测 FTS 表是否已存在，以精确区分：
        // - 全新空库（user_version == 0 && !had_existing_fts）：无需重建
        // - v2.4-v2.7.1 历史库（user_version == 0 && had_existing_fts）：必须重建！
        let had_existing_fts = Self::has_existing_fts_tables(&tx)?;
        let legacy_fts_migrated = Self::detect_old_fts_architecture(&tx)?;
        let tokenizer_upgraded =
            user_version < JIEBA_011_SCHEMA_VERSION && (user_version > 0 || had_existing_fts);
        let needs_fts_rebuild = legacy_fts_migrated || tokenizer_upgraded;
        let trigger_count: i64 = tx.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='trigger' AND name IN \
             ('turns_ai','turns_ad','turns_au','bounded_memory_ai','bounded_memory_ad','bounded_memory_au')",
            [],
            |r| r.get(0),
        )?;

        if user_version < CURRENT_SCHEMA_VERSION || needs_fts_rebuild || trigger_count < 6 {
            Self::apply_schema_and_migrations(&tx, needs_fts_rebuild, CURRENT_SCHEMA_VERSION)?;
        }
        tx.commit()?;

        // 当 needs_fts_rebuild 为 true 时，两张 FTS 表已在 apply_schema_and_migrations
        // 的同一事务内随 user_version 原子重建完毕；仅在 false 时做常规行数偏差自愈。
        if !needs_fts_rebuild {
            if self.turns_fts_needs_backfill()? {
                self.backfill_turns_fts()?;
            }
            self.maybe_backfill_bounded_memory_fts(false)?;
        }

        // Always verify vec0 tables match self.dimensions & distance_metric=cosine
        self.ensure_vec_tables()
    }

    /// 获取底层连接引用
    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    #[cfg(test)]
    pub fn conn_mut(&mut self) -> &mut Connection {
        &mut self.conn
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
#[path = "db_tests.rs"]
mod tests;

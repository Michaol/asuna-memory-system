use rusqlite::{Connection, OptionalExtension};

use super::db::Db;
use super::schema;

impl Db {
    pub(super) fn has_existing_fts_tables(conn: &Connection) -> anyhow::Result<bool> {
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master
             WHERE type = 'table' AND name IN ('turns_fts', 'bounded_memory_fts')",
            [],
            |r| r.get(0),
        )?;
        Ok(count > 0)
    }

    /// W19: 调用方（init_schema）已持有 IMMEDIATE 事务——SCHEMA_SQL、触发器、
    /// P3/P8/v26 迁移、FTS 重建与 PRAGMA user_version 在同一事务内原子提交，
    /// 任一步失败整体回滚（含 detect_old_fts_architecture 的 legacy DROP）。
    pub(super) fn apply_schema_and_migrations(
        conn: &Connection,
        needs_fts_rebuild: bool,
        target_version: i32,
    ) -> anyhow::Result<()> {
        conn.execute_batch(schema::SCHEMA_SQL)?;
        conn.execute_batch(schema::FTS_TRIGGERS_SQL)?;

        // Run migrations BEFORE backfill so all columns exist
        Self::run_migration_p3(conn)?;
        Self::run_migration_p8(conn)?;
        Self::run_migration_v26_edited_at(conn)?;

        if needs_fts_rebuild {
            Self::rebuild_turns_fts_on(conn)?;
            Self::rebuild_bounded_memory_fts_on(conn)?;
        }
        conn.pragma_update(None, "user_version", target_version)?;
        Ok(())
    }

    /// Detect crash window where `turns` has non-null previews while `turns_fts`
    /// is missing entries (e.g. crash after dropping/recreating `turns_fts` before
    /// backfill, or a partially-applied backfill).
    pub(super) fn turns_fts_needs_backfill(&self) -> anyhow::Result<bool> {
        // D2-6: 计数比对取代"全空" EXISTS——部分缺失/孤儿 posting（FTS 行数多于
        // 非 NULL preview 行数）同样触发修复。与 backfill 事务内 delete-all（W2.5）
        // 配套，重回填幂等，不会叠加重复 posting。
        let turn_count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM turns WHERE preview IS NOT NULL",
            [],
            |r| r.get(0),
        )?;
        let fts_count: i64 =
            self.conn
                .query_row("SELECT COUNT(*) FROM turns_fts_docsize", [], |r| r.get(0))?;
        Ok(turn_count != fts_count)
    }

    fn rebuild_turns_fts_on(conn: &Connection) -> anyhow::Result<()> {
        tracing::info!(
            target: "asuna_memory::index::db",
            "重建 FTS 索引（jieba tokenizer）..."
        );
        // W2.5: 事务内先 delete-all 再插——needs-backfill 判定在事务外，并发
        // 首启两进程可能同时判定需要回填；contentless FTS5 的 docsize 用 REPLACE，
        // 无条件 INSERT 会叠加重复 term posting（bm25 虚高）。先清空使回填幂等。
        conn.execute("INSERT INTO turns_fts(turns_fts) VALUES('delete-all')", [])?;
        let mut stmt = conn.prepare("SELECT id, preview FROM turns WHERE preview IS NOT NULL")?;
        // W28f: 循环外 prepare，避免逐行重复编译 INSERT
        let mut ins = conn.prepare("INSERT INTO turns_fts(rowid, preview) VALUES (?1, ?2)")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (id, preview) = row?;
            ins.execute(rusqlite::params![id, preview])?;
        }
        tracing::info!(target: "asuna_memory::index::db", "FTS 索引重建完成");
        Ok(())
    }

    fn rebuild_bounded_memory_fts_on(conn: &Connection) -> anyhow::Result<()> {
        let bm_count: i64 =
            conn.query_row("SELECT COUNT(*) FROM bounded_memory", [], |r| r.get(0))?;
        let fts_count: i64 =
            conn.query_row("SELECT COUNT(*) FROM bounded_memory_fts_docsize", [], |r| {
                r.get(0)
            })?;
        if bm_count == 0 && fts_count == 0 {
            return Ok(());
        }
        conn.execute(
            "INSERT INTO bounded_memory_fts(bounded_memory_fts) VALUES('rebuild')",
            [],
        )?;
        tracing::info!(
            target: "asuna_memory::index::db",
            "bounded_memory_fts rebuild complete ({} source entries)",
            bm_count
        );
        Ok(())
    }

    pub(super) fn backfill_turns_fts(&self) -> anyhow::Result<()> {
        // 单事务回填（J4）：WAL 下避免逐条提交 fsync。失败自动随 Drop 回滚。
        let tx = self.immediate_tx()?;
        Self::rebuild_turns_fts_on(&tx)?;
        tx.commit()?;
        Ok(())
    }

    /// 检测旧版 FTS 架构并执行删除式迁移。
    fn drop_bounded_memory_fts(conn: &Connection) -> anyhow::Result<()> {
        conn.execute_batch(
            "DROP TRIGGER IF EXISTS bounded_memory_ai;
             DROP TRIGGER IF EXISTS bounded_memory_ad;
             DROP TRIGGER IF EXISTS bounded_memory_au;
             DROP TABLE IF EXISTS bounded_memory_fts;",
        )?;
        Ok(())
    }

    fn migrate_legacy_turns_fts(conn: &Connection) -> anyhow::Result<bool> {
        // 只有“表不存在”(无行) 才算无需迁移；其余 SQLite 故障按 D2-13 上抛。
        let Some(sql) = conn
            .query_row::<String, _, _>(
                "SELECT sql FROM sqlite_master WHERE type='table' AND name='turns_fts'",
                [],
                |r| r.get(0),
            )
            .optional()?
        else {
            return Ok(false);
        };
        if sql.contains("content=turns") || sql.contains("content='turns'") {
            tracing::warn!(
                target: "asuna_memory::index::db",
                "检测到旧版 external-content FTS 架构，正在迁移..."
            );
            conn.execute("DROP TABLE IF EXISTS turns_fts", [])?;
            return Ok(true);
        }
        if sql.contains("unicode61") {
            tracing::warn!(
                target: "asuna_memory::index::db",
                "检测到旧版 unicode61 FTS tokenizer，正在迁移到 jieba..."
            );
            conn.execute_batch(
                "DROP TRIGGER IF EXISTS turns_ai;
                 DROP TRIGGER IF EXISTS turns_ad;
                 DROP TRIGGER IF EXISTS turns_au;
                 DROP TABLE IF EXISTS turns_fts;",
            )?;
            Self::drop_bounded_memory_fts(conn)?;
            return Ok(true);
        }
        Ok(false)
    }

    fn migrate_legacy_bounded_memory_fts(conn: &Connection) -> anyhow::Result<bool> {
        // D2-8: bounded_memory_fts 单独检测——turns_fts 已是 jieba 而它独自停留
        // unicode61 时，旧实现只看 turns_fts 永不迁移，L1 中文检索静默失效。
        let Some(sql) = conn
            .query_row::<String, _, _>(
                "SELECT sql FROM sqlite_master WHERE type='table' AND name='bounded_memory_fts'",
                [],
                |r| r.get(0),
            )
            .optional()?
        else {
            return Ok(false);
        };
        if !sql.contains("unicode61") {
            return Ok(false);
        }
        tracing::warn!(
            target: "asuna_memory::index::db",
            "检测到旧版 unicode61 bounded_memory_fts tokenizer，正在迁移到 jieba..."
        );
        Self::drop_bounded_memory_fts(conn)?;
        Ok(true)
    }

    /// 返回是否需要重建 turns_fts 索引（旧架构被删除后由 init_schema 回填）。
    pub(super) fn detect_old_fts_architecture(conn: &Connection) -> anyhow::Result<bool> {
        let turns_migrated = Self::migrate_legacy_turns_fts(conn)?;
        let bm_migrated = Self::migrate_legacy_bounded_memory_fts(conn)?;
        Ok(turns_migrated || bm_migrated)
    }

    /// Run P3 migration: add memory_type, supersedes_id, source_turn_ids, confidence_score
    ///
    /// Always attempts each ALTER TABLE. "duplicate column" errors are silently
    /// skipped, making this safe to run on any database state — including partial
    /// migrations where some columns exist but others don't.
    fn run_migration_p3(conn: &Connection) -> anyhow::Result<()> {
        for stmt in migration_statements(schema::MIGRATION_P3_SQL) {
            match conn.execute_batch(&stmt) {
                Ok(_) => {
                    if stmt.starts_with("ALTER") {
                        tracing::info!(
                            target: "asuna_memory::index::db",
                            "P3 migration: added column"
                        );
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
    fn run_migration_p8(conn: &Connection) -> anyhow::Result<()> {
        for sql_stmt in migration_statements(schema::MIGRATION_P8_ALTER_SQL) {
            match conn.execute_batch(&sql_stmt) {
                Ok(_) => {}
                Err(e) if e.to_string().contains("duplicate column") => {
                    // Column already exists, skip
                }
                Err(e) => return Err(e.into()),
            }
        }

        for sql_stmt in migration_statements(schema::MIGRATION_P8_INDEX_SQL) {
            conn.execute_batch(&sql_stmt)?;
        }

        Ok(())
    }

    /// Run v2.6 migration: add edited_at to bounded_memory.
    ///
    /// Same idempotent duplicate-column-tolerant approach as run_migration_p8.
    fn run_migration_v26_edited_at(conn: &Connection) -> anyhow::Result<()> {
        for sql_stmt in migration_statements(schema::MIGRATION_V26_EDITED_AT_SQL) {
            match conn.execute_batch(&sql_stmt) {
                Ok(_) => {}
                Err(e) if e.to_string().contains("duplicate column") => {
                    // Column already exists, skip
                }
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }

    /// Backfill bounded_memory_fts from the bounded_memory source table.
    ///
    /// When `force` is false, compares row counts between `bounded_memory` and
    /// `bounded_memory_fts_docsize` so both empty FTS and partial divergence
    /// (including orphan postings when `bounded_memory` is empty) are healed.
    pub(super) fn maybe_backfill_bounded_memory_fts(&self, force: bool) -> anyhow::Result<()> {
        // D3-4: 读失败上抛（原 unwrap_or(false) 把故障伪装成"空表/已回填"，
        // 静默跳过回填留下空索引）。
        let bm_count: i64 =
            self.conn
                .query_row("SELECT COUNT(*) FROM bounded_memory", [], |r| r.get(0))?;
        let fts_count: i64 =
            self.conn
                .query_row("SELECT COUNT(*) FROM bounded_memory_fts_docsize", [], |r| {
                    r.get(0)
                })?;

        if !force && bm_count == fts_count {
            return Ok(());
        }
        if force && bm_count == 0 && fts_count == 0 {
            return Ok(());
        }

        self.conn.execute(
            "INSERT INTO bounded_memory_fts(bounded_memory_fts) VALUES('rebuild')",
            [],
        )?;

        tracing::info!(
            target: "asuna_memory::index::db",
            "bounded_memory_fts rebuild complete ({} source entries)",
            bm_count
        );
        Ok(())
    }
}

/// Split a MIGRATION_* constant into executable statements.
///
/// 必须先按行剥离整行 "--" 注释、再按 ';' 切分：旧实现对 `split(';')` 的片段
/// 跳过 trim 后以 "--" 开头者，而 MIGRATION_P3_SQL 的每条语句前都紧邻一行注释，
/// 导致 4 条 ALTER 与首条 CREATE INDEX 永远不执行（P3 迁移整体空转，2026-09 修复）。
/// 现行规约：MIGRATION_* 常量可自由包含整行 "--" 注释，但不得包含内嵌 ';'
/// 的语句（如 CREATE TRIGGER … BEGIN … END;）——按 ';' 切分会将其拆碎。
pub(super) fn migration_statements(sql: &str) -> Vec<String> {
    sql.lines()
        .filter(|line| !line.trim_start().starts_with("--"))
        .collect::<Vec<_>>()
        .join("\n")
        .split(';')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

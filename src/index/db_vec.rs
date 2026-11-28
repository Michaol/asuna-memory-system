use rusqlite::OptionalExtension;
use std::ffi::c_char;
use std::sync::OnceLock;

use super::db::Db;

/// W19: 确保 sqlite-vec 扩展只注册一次，并缓存注册结果——失败时后续所有
/// Db::open* 上抛同一错误（原 warn-only 让后续建 vec0 表报 "no such module"，
/// 原始错误码丢失无从排查）。sqlite3_auto_extension 重复注册同一入口是
/// 无害 no-op（bundled SQLite 文档），OnceLock 保证只求值一次。
static VEC_INIT: OnceLock<Result<(), String>> = OnceLock::new();

pub(super) fn ensure_vec_extension() -> anyhow::Result<()> {
    VEC_INIT
        .get_or_init(|| {
            // SAFETY: sqlite3_vec_init 与 sqlite3_auto_extension 期望的签名兼容
            // (sqlite3*, char**, const sqlite3_api_routines*) -> int
            //
            // 使用 std::ffi::c_char 而非硬编码 i8/u8 —— 这一点很关键：
            //   - x86_64 Linux / Windows / macOS Intel：c_char = i8
            //   - aarch64 Linux / ARM 平台：c_char = u8
            // 硬编码任一类型都会在另一类平台上编译失败（实测 aarch64-linux-gnu 报 E0308）。
            //
            // 前提：sqlite-vec 0.1.x 和 rusqlite 0.43.x 使用同一 bundled sqlite3 ABI。
            // 升级这两个 crate 时必须验证 ABI 兼容性。
            unsafe {
                let func: unsafe extern "C" fn(
                    *mut rusqlite::ffi::sqlite3,
                    *mut *mut c_char,
                    *const rusqlite::ffi::sqlite3_api_routines,
                ) -> i32 = std::mem::transmute(sqlite_vec::sqlite3_vec_init as *const ());
                // D3-5: 注册失败必须留痕——否则后续建 vec0 表报 "no such module"，
                // 原始错误码丢失无从排查。
                let rc = rusqlite::ffi::sqlite3_auto_extension(Some(func));
                if rc != rusqlite::ffi::SQLITE_OK {
                    tracing::error!(
                        target: "asuna_memory::index::db",
                        "sqlite-vec auto-extension 注册失败 (rc={})，vec0 向量表将不可用",
                        rc
                    );
                    return Err(format!(
                        "sqlite-vec auto-extension 注册失败 (rc={rc})，vec0 向量表不可用"
                    ));
                }
                Ok(())
            }
        })
        .clone()
        .map_err(|e| anyhow::anyhow!(e))
}

impl Db {
    pub(super) fn ensure_vec_tables(&self) -> anyhow::Result<()> {
        let dim = self.dimensions;
        let vec_turns_ddl = format!(
            "CREATE VIRTUAL TABLE IF NOT EXISTS vec_turns USING vec0(embedding int8[{dim}] distance_metric=cosine);"
        );
        let vec_bm_ddl = format!(
            "CREATE VIRTUAL TABLE IF NOT EXISTS vec_bounded_memory USING vec0(
                id INTEGER PRIMARY KEY,
                embedding int8[{dim}] distance_metric=cosine
            );"
        );
        // D2-12: DROP+CREATE 包进单事务——中途失败不再留"表已删未重建"的半迁移态。
        let tx = self.immediate_tx()?;
        self.conn.execute_batch(&vec_turns_ddl)?;
        self.conn.execute_batch(&vec_bm_ddl)?;

        let target_tag = format!("int8[{dim}]");
        // 表刚 CREATE IF NOT EXISTS，无行只可能是外部并发删除；真实读故障按 D2-13 上抛，
        // 不得静默跳过维度/度量校验。
        let vec_schema: Option<String> = self
            .conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE name='vec_turns'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(sql) = vec_schema {
            // D7-2: 规范化空白+大小写后比对——外部工具建的 "distance_metric = cosine"
            // /大写写法不再被误判为变更而每次启动 DROP 重建。
            let norm = normalize_ddl(&sql);
            if !norm.contains(&target_tag) || !norm.contains("distance_metric=cosine") {
                tracing::warn!(
                    target: "asuna_memory::index::db",
                    "vec_turns 距离度量/维度变更（现有: {}, 目标: int8[{dim}] cosine），已清空向量索引；\
                     语义/混合搜索将降级为关键词，请运行 `rebuild` 重新嵌入 turns 向量。\
                     多写进程契约：同一 profile 的所有并发写进程必须配置一致的 embedding.dimensions，\
                     否则各进程启动时会反复清空对方刚建好的向量索引",
                    extract_dim_tag(&sql).unwrap_or_else(|| "unknown".into())
                );
                self.conn.execute("DROP TABLE vec_turns", [])?;
                self.conn.execute_batch(&vec_turns_ddl)?;
            }
        }

        let vec_bm_schema: Option<String> = self
            .conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE name='vec_bounded_memory'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(sql) = vec_bm_schema {
            let norm = normalize_ddl(&sql);
            if norm.contains("float32")
                || !norm.contains(&target_tag)
                || !norm.contains("distance_metric=cosine")
            {
                tracing::warn!(
                    target: "asuna_memory::index::db",
                    "vec_bounded_memory 距离度量/维度变更（现有: {}, 目标: int8[{dim}] cosine），已清空向量索引；\
                     atom 向量将在启动时自动回填，无需手动操作。\
                     多写进程契约：同一 profile 的所有并发写进程必须配置一致的 embedding.dimensions，\
                     否则各进程启动时会反复清空对方刚建好的向量索引",
                    extract_dim_tag(&sql).unwrap_or_else(|| "unknown".into())
                );
                self.conn.execute("DROP TABLE vec_bounded_memory", [])?;
                self.conn.execute_batch(&vec_bm_ddl)?;
            }
        }
        tx.commit()?;
        Ok(())
    }
}

/// Extract the dimension tag from a vec0 CREATE TABLE SQL statement.
/// E.g. "CREATE VIRTUAL TABLE ... USING vec0(embedding int8[768])" → "int8[768]"
pub(super) fn extract_dim_tag(sql: &str) -> Option<String> {
    // Look for patterns like "int8[768]" or "float32[1024]" case-insensitively.
    let lower = sql.to_ascii_lowercase();
    let start = lower.find("int8[").or_else(|| lower.find("float32["))?;
    let tail = lower.get(start..)?;
    let end = tail.find(']')? + 1;
    tail.get(..end).map(ToString::to_string)
}

/// D7-2: 规范化 DDL 文本供子串比对——去掉全部空白并转小写，使
/// "distance_metric = cosine" / "INT8[1024]" 等外部工具写法与自写 DDL 等价。
fn normalize_ddl(sql: &str) -> String {
    sql.chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>()
        .to_ascii_lowercase()
}

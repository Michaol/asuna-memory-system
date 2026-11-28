mod append;
mod jsonl_io;
mod legacy_files;
mod overwrite;

use super::conversation::{SessionHeader, Turn};
use crate::index::db::Db;
use crate::util::time;
#[cfg(test)]
use jsonl_io::{
    append_jsonl_turns, repair_torn_jsonl_tail, rollback_partial_append, write_all_or_rollback,
};
#[cfg(test)]
use legacy_files::remove_verified_legacy_jsonl;
use std::path::{Path, PathBuf};

/// SessionStore: JSONL 文件 + SQLite 索引双写
pub struct SessionStore<'a> {
    conversations_dir: &'a Path,
    db: &'a Db,
    /// 单条 turn preview 截取的最大 Unicode 字符数
    preview_length: usize,
}

#[derive(Debug)]
pub struct SaveStats {
    pub session_id: String,
    pub file_path: PathBuf,
    pub turns_saved: usize,
    /// true 表示本次保存有向量缺失（派生数据，可通过 rebuild 后补），会话数据
    /// 照常落库。置位来源：① embedder 已配置但 `embed_documents` 整批失败；
    /// ② Append 模式下逐 turn 向量 insert 失败（M1 数据优先，仅 warn 不回滚）；
    /// ③ embeddings 切片短于 turns（M10，生产不可达）。
    /// 不置位的情形：Overwrite 模式 insert 失败随事务回滚（直接返回 Err，不产
    /// SaveStats）；零/非有限向量被 VectorStore W13 守卫静默跳过（insert 返回
    /// Ok），不计入本标志——故本标志是“整批/逐行失败”信号，非“零向量跳过”计数。
    pub vectors_skipped: bool,
}

/// 保存模式（J33 收敛：REST /capture 与 MCP save_session 共用同一持久化实现，
/// 两个入口的语义差异由模式显式区分，不再是两份各自漂移的 SQL）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaveMode {
    /// 覆盖（MCP save_session / CLI import）：DELETE 本 session 全部 turns +
    /// INSERT OR REPLACE sessions 全列（title/source/tags/total_tokens/end_ts…）；
    /// seq 取输入 turn.seq；JSONL 全量重写走 staged-rename（事务外预写临时文件、
    /// 事务内 rename 入位，写盘失败随事务回滚）并清理旧路径文件；
    /// 向量写失败随事务回滚（数据与索引要么都新要么都旧）。
    Overwrite,
    /// 追加（REST /capture）：seq 从 COALESCE(MAX(seq),0)+1 事务内续排（输入
    /// turn.seq 被忽略）；sessions 走最小列集（新会话 INSERT / 已有会话
    /// turn_count 累加 + file_path 刷新）；向量写失败仅 warn 不回滚（M1）；
    /// JSONL 增量追加且 best-effort（失败只记日志，W3）。U5 改名 × J33b 升级
    /// 缝隙：升级前旧命名的同会话文件在首写时先迁移进目标文件再删除
    /// （见 `SessionStore::prior_jsonl_for_append`）。
    /// 完整差异见 `SessionStore::save_append`。
    Append,
}

impl<'a> SessionStore<'a> {
    pub fn new(conversations_dir: &'a Path, db: &'a Db) -> Self {
        Self {
            conversations_dir,
            db,
            preview_length: 200,
        }
    }

    /// 注入 preview_length 配置（与 Config.conversation.preview_length 联动）
    pub fn with_preview_length(mut self, n: usize) -> Self {
        // 至少留 1 字符，避免退化为空 preview
        self.preview_length = n.max(1);
        self
    }

    pub(super) fn preview_of(&self, content: &str) -> String {
        content.chars().take(self.preview_length).collect()
    }

    /// 计算目标 JSONL 绝对路径 + 相对 conversations_dir 的相对路径（写入 DB
    /// sessions.file_path 列）。save_overwrite 与 save_append 共用，消除两处
    /// ~9 行重复（审查 v4 维度7）。strip_prefix 失败时回退绝对路径（历史行为）。
    pub(super) fn session_paths(
        &self,
        header: &SessionHeader,
    ) -> anyhow::Result<(PathBuf, String)> {
        let file_path = super::conversation::compute_session_path(self.conversations_dir, header)?;
        // M7: 相对路径写入 DB sessions.file_path 列，必须是合法 UTF-8——
        // to_string_lossy 会把非 UTF-8 字节静默替换成 U+FFFD，rebuild/cleanup
        // 按该列定位文件时永远找不到原文件。非 UTF-8 路径直接拒绝。
        let file_rel_path = file_path
            .strip_prefix(self.conversations_dir)
            .unwrap_or(&file_path)
            .to_str()
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "会话路径含非 UTF-8 字符，无法写入索引: {}",
                    file_path.display()
                )
            })?
            .to_string();
        Ok((file_path, file_rel_path))
    }

    /// 保存会话（JSONL + SQLite 双写，可自动生成向量）
    ///
    /// 降级契约：向量是可后补的派生数据。embedder 已配置但 `embed_documents`
    /// 失败（网络 / 限流 / quota 等）时，本次保存**不失败**——会话数据照常
    /// 落库（DB + JSONL），向量跳过，并在 `SaveStats::vectors_skipped` 中置位，
    /// 调用方应提示用户稍后运行 rebuild 补齐。仅 DB / JSONL 写入失败才返回 Err。
    ///
    /// 锁纪律（U15，同 [`crate::fact::search::search_sessions`]）：本函数内部
    /// 调用 `embed_documents`（同步网络 I/O + 重试退避）。持有与其他流量共享的
    /// std::sync::Mutex（如网关全局 DB 锁）的调用方 MUST 在拿锁之前自行预计算
    /// 向量，改用 [`Self::save_with_embeddings_mode`]，否则锁会横跨网络往返。
    /// MCP save_session / CLI import 的连接不与并发网关锁共享（MCP 单线程、
    /// CLI 独立进程），可直接用本签名。
    pub fn save(
        &self,
        header: &SessionHeader,
        turns: &[Turn],
        embedder: Option<&crate::embedder::LazyEmbedder>,
    ) -> anyhow::Result<SaveStats> {
        if let Some(emb) = embedder {
            let previews: Vec<String> = turns.iter().map(|t| self.preview_of(&t.content)).collect();
            // M17: 空 content 的 preview 为空串，部分嵌入 API 拒空输入——一条空
            // turn 会毒化整批向量（整批 Err → 全部 turn 丢向量）。只把非空 preview
            // 送 API，空位回填空 Vec 保持 embeddings 与 turns 索引对齐；VectorStore
            // 的 W13 守卫把空/零向量判为无效并跳过（insert 返回 Ok），数据照常落库。
            let embed_idx: Vec<usize> = previews
                .iter()
                .enumerate()
                .filter_map(|(i, p)| (!p.is_empty()).then_some(i))
                .collect();
            let embed_refs: Vec<&str> = embed_idx.iter().map(|&i| previews[i].as_str()).collect();
            // 文档侧使用 Document 前缀，避免与 query 侧前缀错配导致召回率下降
            match emb.embed_documents(&embed_refs) {
                Ok(vecs) => {
                    let mut embeddings = vec![Vec::new(); turns.len()];
                    for (&i, v) in embed_idx.iter().zip(vecs) {
                        embeddings[i] = v;
                    }
                    self.save_with_embeddings(header, turns, Some(&embeddings))
                }
                Err(e) => {
                    tracing::warn!(
                        "会话 {} 嵌入失败，本次跳过向量（可稍后 rebuild 补齐）: {}",
                        header.session_id,
                        e
                    );
                    let mut stats = self.save_with_embeddings(header, turns, None)?;
                    stats.vectors_skipped = true;
                    Ok(stats)
                }
            }
        } else {
            self.save_with_embeddings(header, turns, None)
        }
    }

    /// 保存会话（JSONL + SQLite 双写 + 向量索引），显式指定保存模式。
    ///
    /// 顺序契约（J33 收敛时自 http.rs 的内联事务与 session_store 的覆盖路径合并
    /// 而来；C3/C6 后两种模式的 JSONL 与清理时机不同，分别注明）：
    /// 1. 计算 path
    /// 2. 读旧 path + start_ts 用于清理/升级迁移：Overwrite 在事务内读（W15：
    ///    事务外预读会被跨进程 CLI/外部 sqlite 改写 sessions 行钻空子）；Append
    ///    在事务**外**预读快照（C3 锁纪律：候选枚举/legacy 读取/seq 扫描是
    ///    O(文件) IO，不得跨 BEGIN IMMEDIATE 写锁）。
    /// 3. 事务内写 DB；Append 的 JSONL 真相源追加也在**同一事务内**（C3：写失败
    ///    整体回滚，客户端可安全重试）；Overwrite 的 JSONL 事务外预写临时文件、
    ///    事务内 rename 收尾（staged-rename，写失败整体回滚）。
    /// 4. commit
    /// 5. 删除旧 JSONL 严格在 commit **之后**（C6：COMMIT 失败回滚时 legacy 旧
    ///    文件必须原样保留）——Overwrite：DB 记录路径 + U5 改名前旧文件；
    ///    Append：迁移完的旧文件。不同路径才删。
    ///
    /// 失败语义按模式不同：
    /// - Overwrite：staged-rename 原子契约——JSONL 事务外预写、事务内 rename 入位；
    ///   预写或 rename 失败 → DB 随事务回滚（DB 与真相源要么都新要么都旧）。残余
    ///   窗口：rename 成功后 COMMIT 失败 → 磁盘已新而 DB 回滚，下次保存/rebuild
    ///   （JSONL 为真相源）抹平。
    /// - Append：JSONL 与 DB 同事务，任一失败整体回滚（DB 与真相源一致）；
    ///   commit 后删旧文件失败只 warn（旧文件并存，下次写再清理）。
    ///
    /// 两种模式的语义差异（turns 处置 / sessions 列集 / seq / 向量与 JSONL
    /// 的失败粒度）见 [`SaveMode`] 与各分支文档注释；J41 跨入口契约测试
    /// （`transport::http` 测试模块与下方 store 级测试）钉住这些差异是有意的。
    pub fn save_with_embeddings_mode(
        &self,
        header: &SessionHeader,
        turns: &[Turn],
        embeddings: Option<&[Vec<f32>]>,
        mode: SaveMode,
    ) -> anyhow::Result<SaveStats> {
        match mode {
            SaveMode::Overwrite => self.save_overwrite(header, turns, embeddings),
            SaveMode::Append => self
                .save_append_idempotent(header, turns, embeddings, None)
                .map(|(stats, _)| stats),
        }
    }

    /// W10: 支持可选 `batch_id` 的原子幂等追加：若传入 `Some(bid)`，则将
    /// `capture_dedup` 插入与 `sessions`/`turns`/`vec_turns` 写入合进同一个
    /// `immediate_tx`；返回值 `(SaveStats, dedup_hit)` 中 `dedup_hit == true`
    /// 表示同 `(session_id, batch_id)` 已提交，直接返回首存 `turns_saved`、不追加 turns/JSONL。
    pub fn save_append_idempotent(
        &self,
        header: &SessionHeader,
        turns: &[Turn],
        embeddings: Option<&[Vec<f32>]>,
        batch_id: Option<&str>,
    ) -> anyhow::Result<(SaveStats, bool)> {
        self.save_append(header, turns, embeddings, batch_id)
    }

    /// 覆盖保存（MCP save_session / CLI import 的历史语义，逐位保持）。
    /// [`Self::save_with_embeddings_mode`] 的薄包装，签名保留以稳定调用方。
    pub fn save_with_embeddings(
        &self,
        header: &SessionHeader,
        turns: &[Turn],
        embeddings: Option<&[Vec<f32>]>,
    ) -> anyhow::Result<SaveStats> {
        self.save_with_embeddings_mode(header, turns, embeddings, SaveMode::Overwrite)
    }
}

/// M18/W1.3: turn.ts 解析失败时回落 start_ts 并 warn（不再静默）——坏 ts 塌缩
/// 到 start_ts 会污染时间窗过滤与 recency，必须可观测（与 header start_time
/// 失败上抛、rebuild 侧回退记 errors 的口径对齐）。
pub(super) fn turn_ts_ms_or_warn(ts: &str, start_ts: i64, session_id: &str, seq: i64) -> i64 {
    match time::ts_to_unix_ms(ts) {
        Ok(ms) => ms,
        Err(e) => {
            tracing::warn!(
                "会话 {} turn seq {} 时间戳解析失败，回落 start_ts: {:?} ({})",
                session_id,
                seq,
                ts,
                e
            );
            start_ts
        }
    }
}

/// 事务封装：使用 RAII `IMMEDIATE` 事务，成功 COMMIT，Err 或 panic 自动回滚。
pub(super) fn run_in_transaction<F, T>(db: &Db, f: F) -> anyhow::Result<T>
where
    F: FnOnce() -> anyhow::Result<T>,
{
    let tx = db.immediate_tx()?;
    let val = f()?;
    tx.commit()?;
    Ok(val)
}

/// M13b: Append 与 Overwrite 共用的 turn 行 INSERT 语句（列集逐位一致）。
pub(super) const INSERT_TURN_SQL: &str =
    "INSERT INTO turns (session_id, seq, timestamp_ms, role, preview, char_count)
     VALUES (?1, ?2, ?3, ?4, ?5, ?6)";

/// M13b: 共用的 turn 行入库执行器——seq / ts_ms / preview 由各模式按自身契约
/// 算好后传入（Append 事务内续排、Overwrite 取输入 seq），向量与事务处置仍各归
/// 各模式。消除两处逐字段重复的 params! 样板。
pub(super) fn execute_turn_insert(
    stmt: &mut rusqlite::CachedStatement<'_>,
    session_id: &str,
    seq: i64,
    ts_ms: i64,
    role: &str,
    preview: &str,
    char_count: i64,
) -> anyhow::Result<()> {
    stmt.execute(rusqlite::params![
        session_id, seq, ts_ms, role, preview, char_count,
    ])?;
    Ok(())
}

#[cfg(test)]
mod tests;

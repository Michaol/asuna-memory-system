use super::conversation::{SessionHeader, Turn};
use crate::index::db::Db;
use crate::index::vector::VectorStore;
use crate::util::time;
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
    /// true 表示 embedder 已配置但 embed_documents 失败：会话数据照常落库，
    /// 向量本次跳过（派生数据，可通过 rebuild 后补）。
    pub vectors_skipped: bool,
}

/// 保存模式（J33 收敛：REST /capture 与 MCP save_session 共用同一持久化实现，
/// 两个入口的语义差异由模式显式区分，不再是两份各自漂移的 SQL）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaveMode {
    /// 覆盖（MCP save_session / CLI import）：DELETE 本 session 全部 turns +
    /// INSERT OR REPLACE sessions 全列（title/source/tags/total_tokens/end_ts…）；
    /// seq 取输入 turn.seq；JSONL 全量重写并清理旧路径文件；
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

    fn preview_of(&self, content: &str) -> String {
        content.chars().take(self.preview_length).collect()
    }

    /// 保存会话（JSONL + SQLite 双写，可自动生成向量）
    ///
    /// 降级契约：向量是可后补的派生数据。embedder 已配置但 `embed_documents`
    /// 失败（网络 / 限流 / quota 等）时，本次保存**不失败**——会话数据照常
    /// 落库（DB + JSONL），向量跳过，并在 `SaveStats::vectors_skipped` 中置位，
    /// 调用方应提示用户稍后运行 rebuild 补齐。仅 DB / JSONL 写入失败才返回 Err。
    pub fn save(
        &self,
        header: &SessionHeader,
        turns: &[Turn],
        embedder: Option<&crate::embedder::LazyEmbedder>,
    ) -> anyhow::Result<SaveStats> {
        if let Some(emb) = embedder {
            let previews: Vec<String> = turns.iter().map(|t| self.preview_of(&t.content)).collect();
            let preview_refs: Vec<&str> = previews.iter().map(String::as_str).collect();
            // 文档侧使用 Document 前缀，避免与 query 侧前缀错配导致召回率下降
            match emb.embed_documents(&preview_refs) {
                Ok(embeddings) => self.save_with_embeddings(header, turns, Some(&embeddings)),
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
    /// 顺序契约（两种模式共用，J33 收敛时自 http.rs 的内联事务与
    /// session_store 的覆盖路径合并而来）：
    /// 1. 计算 path
    /// 2. 读旧 path + start_ts 用于清理/升级迁移（Overwrite 在事务外只读，
    ///    Append 在事务内一并读取）
    /// 3. 事务内写 DB
    /// 4. commit
    /// 5. commit 成功后才写 JSONL（Append 先把改名前旧文件的 turns 迁入目标）
    /// 6. 删除旧 JSONL（Overwrite：DB 记录路径 + U5 改名前旧文件；Append：
    ///    迁移完的旧文件）——不同路径才删
    ///
    /// 这样保证：
    /// - DB tx 失败：JSONL 完全未触动；
    /// - JSONL 写盘失败：DB 已更新但磁盘缺失（rebuild 时该 session 直接缺席）。
    ///   补救语义按模式不同，"下次 save 会覆盖"只对 Overwrite 成立：Overwrite
    ///   下次保存全量重写 JSONL，缺口一次抹平；Append 是增量 best-effort（W3）
    ///   ——失败的批次只留下 warn，之后的追加只续写新 turn，已丢的历史不会
    ///   回补，磁盘与 DB 的差集持续存在到人工 Overwrite 为止。
    ///
    /// 旧实现先写 JSONL，DB 失败会留残骸——更糟。
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
            SaveMode::Append => self.save_append(header, turns, embeddings),
        }
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

    fn save_overwrite(
        &self,
        header: &SessionHeader,
        turns: &[Turn],
        embeddings: Option<&[Vec<f32>]>,
    ) -> anyhow::Result<SaveStats> {
        // 1. 计算目标 JSONL 路径（仅计算，不写盘）
        let file_path = super::conversation::compute_session_path(self.conversations_dir, header)?;
        let file_rel_path = file_path
            .strip_prefix(self.conversations_dir)
            .unwrap_or(&file_path)
            .to_string_lossy()
            .to_string();

        // 2. 计算元信息
        let start_ts = time::ts_to_unix_ms(&header.start_time)?;
        let end_ts = turns
            .last()
            .map(|t| time::ts_to_unix_ms(&t.ts).unwrap_or(start_ts));
        let total_tokens: i64 = turns.iter().map(|t| turn_tokens(t.metadata.as_ref())).sum();
        let now = time::now_unix_ms();
        let tags_json = if header.tags.is_empty() {
            None
        } else {
            Some(serde_json::to_string(&header.tags)?)
        };

        let conn = self.db.conn();

        // 3. 读旧 JSONL 路径 + start_ts（用于 commit 后清理与 U5 升级缝隙的
        //    旧命名文件定位；事务外只读，安全）
        let prior_db: Option<(String, i64)> = conn
            .query_row(
                "SELECT file_path, start_ts FROM sessions WHERE session_id = ?1",
                rusqlite::params![header.session_id],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
            )
            .ok();
        let old_jsonl_path: Option<PathBuf> = prior_db
            .as_ref()
            .map(|(p, _)| self.conversations_dir.join(p));

        // 4. 进入事务：所有 DB 改动包裹其中
        let vec_store = VectorStore::new(self.db);
        run_in_transaction(conn, || {
            // 先收集本 session 现有 turn 的 rowid（必须在 DELETE 之前，同一事务
            // 内快照一致）。用于精确删除它们的向量——旧实现每存一次就对全库做
            // `rowid NOT IN (SELECT id FROM turns)` 全表扫描，代价与库规模成正比；
            // 而本次操作只可能孤立本 session 的向量（turns AUTOINCREMENT 不复用
            // id）。全库级孤儿清理由 rebuild（整体清空 vec_turns）与 doctor 的
            // 只读检测兜底。
            // S1488：去掉即返临时变量。注意 stmt 必须保持独立绑定——把
            // collect 链放进块尾表达式位会让 MappedRows 临时值活过 stmt
            // 的落域（E0597），"直接返回表达式"在 rusqlite 里不成立。
            let mut stale_stmt = conn.prepare("SELECT id FROM turns WHERE session_id = ?1")?;
            let stale_turn_ids: Vec<i64> = stale_stmt
                .query_map(rusqlite::params![header.session_id], |r| r.get::<_, i64>(0))?
                .collect::<rusqlite::Result<Vec<i64>>>()?;

            // 清理旧索引（INSERT OR REPLACE 只覆盖 sessions 表）
            conn.execute(
                "DELETE FROM turns WHERE session_id = ?1",
                rusqlite::params![header.session_id],
            )?;
            // 清理本 session 旧 turn 的向量（vec0 支持 rowid 等值删除）
            for id in &stale_turn_ids {
                conn.execute(
                    "DELETE FROM vec_turns WHERE rowid = ?1",
                    rusqlite::params![id],
                )?;
            }

            // 写入 session
            conn.execute(
                "INSERT OR REPLACE INTO sessions
                 (session_id, start_ts, end_ts, file_path, title, profile_id, source, agent_model,
                  turn_count, total_tokens, tags, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                rusqlite::params![
                    header.session_id,
                    start_ts,
                    end_ts,
                    file_rel_path,
                    header.title,
                    header.profile_id,
                    header.source,
                    header.agent_model,
                    turns.len() as i64,
                    total_tokens,
                    tags_json,
                    now,
                    now,
                ],
            )?;

            // 写入 turns + 可选向量
            self.write_turn_rows(conn, header, turns, start_ts, embeddings, &vec_store)
        })?;

        // 5. DB commit 成功后写 JSONL
        super::conversation::write_session_at(&file_path, header, turns)?;

        // 6. 清理旧 JSONL（不同路径才删，且不要因清理失败而拒绝整体成功）
        cleanup_old_jsonl(old_jsonl_path, &file_path);

        // 6b. U5 升级缝隙（改名 × `gateway://` 伪 URI）：v2.6.2 及之前经 REST
        //     /capture 写入的会话，DB file_path 定位不到磁盘上的旧命名文件，
        //     放任不管则同一 session_id 永久并存两个文件，rebuild 混并两份
        //     turns、增量检测恒判不一致。Overwrite 语义本就全量替换，按
        //     session_id 校验后直接删除旧文件（不迁移，请求内容即新真相）。
        remove_verified_legacy_jsonl(
            &legacy_jsonl_candidates(
                self.conversations_dir,
                header,
                prior_db.as_ref().map(|(p, ts)| (p.as_str(), *ts)),
            ),
            &file_path,
            &header.session_id,
        );

        Ok(SaveStats {
            session_id: header.session_id.clone(),
            file_path,
            turns_saved: turns.len(),
            vectors_skipped: false,
        })
    }

    /// 追加保存（REST /capture 的历史语义，J33 自 http.rs 内联事务逐位迁移）。
    ///
    /// 与 [`Self::save_overwrite`] 的差异（均为显式契约，J41 契约测试钉住）：
    /// - 不删 turns；seq 事务内从 COALESCE(MAX(seq),0)+1 续排，输入 turn.seq
    ///   被忽略（/capture 请求体本就没有 seq 字段）；
    /// - sessions 最小列集：新会话 INSERT（start_ts/file_path/turn_count/
    ///   created_at/updated_at），已有会话 UPDATE 累加 turn_count、刷新
    ///   updated_at 与 file_path。title/source/total_tokens/end_ts 保持原值
    ///   （新行为 schema 默认）——与旧 /capture 逐位一致；
    /// - J33b：file_path 写**真实 JSONL 相对路径**（旧 /capture 写
    ///   `gateway://{id}` 伪 URI，破坏 rebuild/cleanup/溯源对该列的使用）；
    /// - 向量统一走 VectorStore::insert（J33f：旧 /capture 的手写 vec_int8
    ///   SQL 与之逐字段等价，已删）；写失败仅 warn 不回滚（M1：数据优先），
    ///   与 Overwrite 的“失败即回滚”不同；
    /// - JSONL 增量追加且 best-effort（W3），无同路径旧文件清理（路径派生自
    ///   session 的 start_ts，同一会话多次追加稳定指向同一文件）；但跨版本
    ///   升级会话例外——U5 改名让旧命名文件逃过全部清理，首写时先迁移后删除
    ///   （见 [`Self::prior_jsonl_for_append`]）。
    fn save_append(
        &self,
        header: &SessionHeader,
        turns: &[Turn],
        embeddings: Option<&[Vec<f32>]>,
    ) -> anyhow::Result<SaveStats> {
        // 1. 计算目标 JSONL 路径（与旧 archive_session_jsonl 一致：仅由 header 推导）
        let file_path = super::conversation::compute_session_path(self.conversations_dir, header)?;
        let file_rel_path = file_path
            .strip_prefix(self.conversations_dir)
            .unwrap_or(&file_path)
            .to_string_lossy()
            .to_string();

        let start_ts = time::ts_to_unix_ms(&header.start_time)?;
        let now = time::now_unix_ms();

        let conn = self.db.conn();
        let vec_store = VectorStore::new(self.db);

        // 2. 事务：session 行（INSERT/UPDATE）+ turns 续排 + 向量。
        //    返回本次续排的起点（max_seq，用于 commit 后构造 JSONL 行）与
        //    既有行的 (file_path, start_ts)（供 U5 升级旧文件定位，见
        //    legacy_jsonl_candidates）。
        //    会话存在性判断在事务内完成（调用方持 DB mutex，与旧 handler
        //    在事务前读 sessions.start_ts 的结果一致）。
        let (max_seq, prior_db): (i64, Option<(String, i64)>) = run_in_transaction(conn, || {
            let existing: Option<(i64, String)> = conn
                .query_row(
                    "SELECT start_ts, file_path FROM sessions WHERE session_id = ?1",
                    rusqlite::params![header.session_id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .ok();

            if existing.is_none() {
                conn.execute(
                    "INSERT INTO sessions (session_id, start_ts, file_path, turn_count, created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
                    rusqlite::params![
                        header.session_id,
                        start_ts,
                        file_rel_path,
                        turns.len() as i64,
                        now,
                    ],
                )?;
            } else {
                conn.execute(
                    "UPDATE sessions SET turn_count = turn_count + ?1, updated_at = ?2, file_path = ?3
                     WHERE session_id = ?4",
                    rusqlite::params![turns.len() as i64, now, file_rel_path, header.session_id],
                )?;
            }

            let max_seq: i64 = conn.query_row(
                "SELECT COALESCE(MAX(seq), 0) FROM turns WHERE session_id = ?1",
                rusqlite::params![header.session_id],
                |r| r.get(0),
            )?;

            for (i, turn) in turns.iter().enumerate() {
                let ts_ms = time::ts_to_unix_ms(&turn.ts).unwrap_or(start_ts);
                let preview = self.preview_of(&turn.content);
                let char_count = turn.content.chars().count() as i64;
                let seq = max_seq + (i as i64) + 1;

                conn.execute(
                    "INSERT INTO turns (session_id, seq, timestamp_ms, role, preview, char_count)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    rusqlite::params![
                        header.session_id,
                        seq,
                        ts_ms,
                        turn.role,
                        preview,
                        char_count,
                    ],
                )?;

                // J33f：向量统一走 VectorStore::insert（SQL 与旧 /capture 手写版
                // 逐字段一致）。失败仅 warn（M1 数据优先契约，见函数文档）。
                if let Some(embs) = embeddings {
                    if i < embs.len() {
                        let turn_id = conn.last_insert_rowid();
                        if let Err(e) = vec_store.insert(turn_id, &embs[i]) {
                            tracing::warn!("vec_turns insert failed for turn {}: {}", turn_id, e);
                        }
                    }
                }
            }
            Ok((max_seq, existing.map(|(ts, fp)| (fp, ts))))
        })?;

        // 3. DB commit 成功后写 JSONL（顺序契约），增量追加 + best-effort。
        //    JSONL 行的 seq 与事务内续排一致（旧实现用同一 max_seq 在
        //    archive_session_jsonl 独立重算），u32 截断语义保持。
        let new_turns: Vec<Turn> = turns
            .iter()
            .enumerate()
            .map(|(i, t)| Turn {
                ts: t.ts.clone(),
                seq: u32::try_from(max_seq + (i as i64) + 1).unwrap_or(u32::MAX),
                role: t.role.clone(),
                content: t.content.clone(),
                metadata: None,
            })
            .collect();

        // 3b. U5 升级缝隙：目标文件不存在（升级后首写）时，把改名前旧文件的
        //     turns 迁到本次追加之前（JSONL 是真相源，改名不得丢数据），并
        //     登记写盘成功后要删除的旧文件。
        let candidates = legacy_jsonl_candidates(
            self.conversations_dir,
            header,
            prior_db.as_ref().map(|(p, ts)| (p.as_str(), *ts)),
        );
        let (prior_turns, stale_files) =
            self.prior_jsonl_for_append(header, &file_path, &candidates);
        let mut jsonl_turns = prior_turns;
        jsonl_turns.extend(new_turns);

        if let Err(e) = append_jsonl_turns(&file_path, header, &jsonl_turns) {
            // W3: log JSONL failures instead of silent discard —— but never fail the save
            tracing::warn!(
                "JSONL append failed for session {}: {}",
                header.session_id,
                e
            );
        } else {
            // 迁移内容已落盘（或目标文件本就覆盖旧文件），旧文件方可删除；
            // 写盘失败则保留旧文件，下次追加重试迁移（数据优先，同 W3 契约）。
            for stale in stale_files {
                if let Err(e) = std::fs::remove_file(&stale) {
                    tracing::warn!("升级旧 JSONL 清理失败 {}: {}", stale.display(), e);
                }
            }
        }

        Ok(SaveStats {
            session_id: header.session_id.clone(),
            file_path,
            turns_saved: turns.len(),
            vectors_skipped: false,
        })
    }

    /// U5 升级缝隙（改名 × `gateway://` 伪 URI，见
    /// [`super::conversation::compute_legacy_session_path`] 文档）：Append 写盘
    /// 前对候选旧文件的处置。候选按 header session_id 校验——前缀碰撞的外来
    /// 文件与解析失败的文件一律不碰。
    ///
    /// - 目标文件不存在（升级后首写）：返回旧文件的 turns（跨候选按 seq 去重，
    ///   首见优先——后见者是被 DB 取代的旧版本，warn 后随文件一起丢弃），旧文件
    ///   登记为“写盘成功后删除”；
    /// - 目标文件已存在（上次写成功但删旧失败的崩溃残留）：仅当旧文件的 seq 集
    ///   ⊆ 目标文件（已被覆盖）才登记删除，否则保留并 warn（宁并存不误删）。
    fn prior_jsonl_for_append(
        &self,
        header: &SessionHeader,
        target: &Path,
        candidates: &[PathBuf],
    ) -> (Vec<Turn>, Vec<PathBuf>) {
        let present: Vec<PathBuf> = candidates
            .iter()
            .filter(|p| *p != target && p.exists())
            .cloned()
            .collect();
        if present.is_empty() {
            return (Vec::new(), Vec::new());
        }

        let mut stale: Vec<PathBuf> = Vec::new();
        if !target.exists() {
            // 首写迁移：turns 迁入目标文件，文件在写盘成功后删除。
            let mut turns: Vec<Turn> = Vec::new();
            let mut seen: std::collections::HashSet<u32> = std::collections::HashSet::new();
            for cand in present {
                match super::conversation::read_session(&cand) {
                    Ok((h, cand_turns)) if h.session_id == header.session_id => {
                        stale.push(cand.clone());
                        for t in cand_turns {
                            if seen.insert(t.seq) {
                                turns.push(t);
                            } else {
                                tracing::warn!(
                                    "升级迁移：旧文件 {} 中 seq {} 与已迁移内容重复，按 DB 现状丢弃",
                                    cand.display(),
                                    t.seq
                                );
                            }
                        }
                    }
                    Ok(_) => tracing::debug!(
                        "升级清理：旧命名文件 session_id 不匹配（前缀碰撞），不触碰 {}",
                        cand.display()
                    ),
                    Err(e) => tracing::warn!(
                        "升级迁移：旧文件解析失败，不迁移不删除 {}: {}",
                        cand.display(),
                        e
                    ),
                }
            }
            return (turns, stale);
        }

        // 目标文件已存在：不迁移（追加会重复），只做“已被覆盖”判定后的删除。
        let target_seqs: Option<std::collections::HashSet<u32>> =
            match super::conversation::read_session(target) {
                Ok((_, ts)) => Some(ts.iter().map(|t| t.seq).collect()),
                Err(e) => {
                    tracing::warn!(
                        "升级清理：目标文件读取失败，本轮旧文件处理跳过 {}: {}",
                        target.display(),
                        e
                    );
                    None
                }
            };
        if let Some(target_seqs) = target_seqs {
            for cand in present {
                match super::conversation::read_session(&cand) {
                    Ok((h, cand_turns))
                        if h.session_id == header.session_id
                            && cand_turns.iter().all(|t| target_seqs.contains(&t.seq)) =>
                    {
                        stale.push(cand);
                    }
                    Ok((h, _)) if h.session_id == header.session_id => tracing::warn!(
                        "升级清理：旧文件 {} 含目标文件未覆盖的 turns，保留（该会话并存两个 JSONL，需人工合并）",
                        cand.display()
                    ),
                    Ok(_) => {}
                    Err(e) => tracing::warn!(
                        "升级清理：旧文件解析失败，保留 {}: {}",
                        cand.display(),
                        e
                    ),
                }
            }
        }
        (Vec::new(), stale)
    }

    /// 写入 turns 行 + 可选向量（事务内调用）
    fn write_turn_rows(
        &self,
        conn: &rusqlite::Connection,
        header: &SessionHeader,
        turns: &[Turn],
        start_ts: i64,
        embeddings: Option<&[Vec<f32>]>,
        vec_store: &VectorStore<'_>,
    ) -> anyhow::Result<()> {
        for (i, turn) in turns.iter().enumerate() {
            let ts_ms = time::ts_to_unix_ms(&turn.ts).unwrap_or(start_ts);
            let preview = self.preview_of(&turn.content);
            let char_count = turn.content.chars().count() as i64;

            conn.execute(
                "INSERT INTO turns (session_id, seq, timestamp_ms, role, preview, char_count)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![
                    header.session_id,
                    turn.seq as i64,
                    ts_ms,
                    turn.role,
                    preview,
                    char_count,
                ],
            )?;

            if let Some(embs) = embeddings {
                if i < embs.len() {
                    let turn_id = conn.last_insert_rowid();
                    vec_store.insert(turn_id, &embs[i])?;
                }
            }
        }
        Ok(())
    }
}

/// 单条 turn 的 token 总数（usage.input_tokens + usage.output_tokens，缺失时为 0）
fn turn_tokens(metadata: Option<&serde_json::Value>) -> i64 {
    metadata
        .and_then(|m| {
            let u = m.get("usage")?;
            let inp = u.get("input_tokens")?.as_i64()?;
            let out = u.get("output_tokens")?.as_i64()?;
            Some(inp + out)
        })
        .unwrap_or(0)
}

/// 清理旧 JSONL（不同路径才删，且不要因清理失败而拒绝整体成功）
fn cleanup_old_jsonl(old_jsonl_path: Option<PathBuf>, file_path: &Path) {
    let Some(old) = old_jsonl_path else {
        return;
    };
    if !old.exists() || old == file_path {
        return;
    }
    if let Err(e) = std::fs::remove_file(&old) {
        tracing::warn!("旧 JSONL 清理失败 {}: {}", old.display(), e);
    }
}

/// U5 升级缝隙的候选旧文件路径集（见
/// [`super::conversation::compute_legacy_session_path`]）：
/// ① DB `file_path` 记录的旧路径（`gateway://` 伪 URI 磁盘上不存在，由调用方
///    的 exists() 自然过滤）；
/// ② 按改名前命名规则从 header.start_time 串复原——REST /capture 两版命名都
///    用 unix_ms_to_iso(DB start_ts)/first_ts 同源串，此复原对升级主场景精确；
/// ③ 同规则再从 DB start_ts 复原一次，覆盖客户端 header 串与当年命名字符串
///    表示不一致的偏移情形。
/// 保序去重。
fn legacy_jsonl_candidates(
    conversations_dir: &Path,
    header: &SessionHeader,
    prior_db: Option<(&str, i64)>,
) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    if let Some((rel, db_start_ts)) = prior_db {
        let p = conversations_dir.join(rel);
        if !out.contains(&p) {
            out.push(p);
        }
        if let Ok(p) = super::conversation::compute_legacy_session_path(
            conversations_dir,
            &header.session_id,
            &time::unix_ms_to_iso(db_start_ts),
        ) {
            if !out.contains(&p) {
                out.push(p);
            }
        }
    }
    if let Ok(p) = super::conversation::compute_legacy_session_path(
        conversations_dir,
        &header.session_id,
        &header.start_time,
    ) {
        if !out.contains(&p) {
            out.push(p);
        }
    }
    out
}

/// Overwrite 侧的旧命名文件清理：header session_id 匹配才删（Overwrite 请求
/// 内容即新真相，不迁移）；不匹配（前缀碰撞）或解析失败的文件不碰。
fn remove_verified_legacy_jsonl(candidates: &[PathBuf], keep: &Path, session_id: &str) {
    for cand in candidates {
        if cand == keep || !cand.exists() {
            continue;
        }
        match super::conversation::read_session(cand) {
            Ok((h, _)) if h.session_id == session_id => {
                if let Err(e) = std::fs::remove_file(cand) {
                    tracing::warn!("升级改名前旧 JSONL 清理失败 {}: {}", cand.display(), e);
                }
            }
            Ok(_) => tracing::debug!(
                "升级清理：旧命名文件 session_id 不匹配（前缀碰撞），不触碰 {}",
                cand.display()
            ),
            Err(e) => tracing::warn!(
                "升级清理：旧命名文件解析失败，不删除 {}: {}",
                cand.display(),
                e
            ),
        }
    }
}

/// Append turn lines to a JSONL file. Creates the file with header if it doesn't exist,
/// otherwise appends lines only. Ensures parent directory exists.
///
/// J33 收敛时自 transport/http.rs（旧 /capture 的归档路径）逐位迁入，
/// 仅供 [`SaveMode::Append`] 使用；Overwrite 模式走
/// `conversation::write_session_at` 全量重写。
fn append_jsonl_turns(
    jsonl_path: &Path,
    header: &SessionHeader,
    turns: &[Turn],
) -> anyhow::Result<()> {
    use std::io::Write;

    // Ensure parent directory exists (W2 fix)
    if let Some(parent) = jsonl_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(jsonl_path)?;

    // Check file size AFTER opening (avoids TOCTOU race between exists() and open())
    if file.metadata().map(|m| m.len() == 0).unwrap_or(true) {
        // Empty or new file: write header line first
        writeln!(file, "{}", serde_json::to_string(header)?)?;
    }

    for turn in turns {
        writeln!(file, "{}", serde_json::to_string(turn)?)?;
    }

    Ok(())
}

/// 事务封装：成功 COMMIT，任何 Err 触发 ROLLBACK。
/// 使用 SQL 级别 BEGIN/COMMIT/ROLLBACK 以兼容 `&Connection`（Rc 共享场景）。
fn run_in_transaction<F, T>(conn: &rusqlite::Connection, f: F) -> anyhow::Result<T>
where
    F: FnOnce() -> anyhow::Result<T>,
{
    conn.execute_batch("BEGIN IMMEDIATE")?;
    match f() {
        Ok(val) => {
            conn.execute_batch("COMMIT")?;
            Ok(val)
        }
        Err(e) => {
            // 尽力回滚；即使 rollback 失败也返回原始错误
            if let Err(rb_err) = conn.execute_batch("ROLLBACK") {
                tracing::error!("回滚失败: {} (原始错误: {})", rb_err, e);
            }
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fact::conversation::SessionHeader;

    fn make_header() -> SessionHeader {
        SessionHeader {
            v: 1,
            header_type: "session_header".to_string(),
            session_id: "dual-write-test".to_string(),
            start_time: "2026-04-10T14:30:00.000+08:00".to_string(),
            profile_id: "default".to_string(),
            source: Some("test".to_string()),
            agent_model: None,
            title: None,
            tags: vec!["test".to_string()],
        }
    }

    fn make_turns() -> Vec<Turn> {
        vec![
            Turn {
                ts: "2026-04-10T14:30:05.000+08:00".to_string(),
                seq: 1,
                role: "user".to_string(),
                content: "测试双写".to_string(),
                metadata: None,
            },
            Turn {
                ts: "2026-04-10T14:30:10.000+08:00".to_string(),
                seq: 2,
                role: "assistant".to_string(),
                content: "收到".to_string(),
                metadata: Some(serde_json::json!({
                    "usage": {"input_tokens": 5, "output_tokens": 2}
                })),
            },
        ]
    }

    #[test]
    fn test_session_store_dual_write() {
        let tmp = std::env::temp_dir().join(format!(
            "asuna_dual_{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        ));
        std::fs::create_dir_all(&tmp).unwrap();

        let db = Db::open_memory().unwrap();
        db.init_schema().unwrap();

        let store = SessionStore::new(&tmp, &db);
        let stats = store.save(&make_header(), &make_turns(), None).unwrap();

        assert_eq!(stats.session_id, "dual-write-test");
        assert_eq!(stats.turns_saved, 2);
        assert!(!stats.vectors_skipped, "无 embedder 时不应报告跳过向量");

        let count: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);

        let turn_count: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM turns", [], |r| r.get(0))
            .unwrap();
        assert_eq!(turn_count, 2);

        assert!(stats.file_path.exists());

        std::fs::remove_dir_all(&tmp).unwrap();
    }

    /// 验证：JSONL 写入失败（目录不可写）时 DB 必须保持一致 —— 但因为我们的实现是
    /// "先 commit 再 JSONL"，DB 会更新而 JSONL 缺失。这是可接受的（缺 JSONL 不破坏 DB）。
    /// 反向（旧实现）会留下残骸，已废弃。
    #[test]
    fn test_preview_length_config() {
        let tmp = std::env::temp_dir().join(format!(
            "asuna_preview_{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        ));
        std::fs::create_dir_all(&tmp).unwrap();

        let db = Db::open_memory().unwrap();
        db.init_schema().unwrap();
        let store = SessionStore::new(&tmp, &db).with_preview_length(5);

        let header = make_header();
        let turns = vec![Turn {
            ts: "2026-04-10T14:30:05.000+08:00".to_string(),
            seq: 1,
            role: "user".to_string(),
            content: "一二三四五六七八九十".to_string(),
            metadata: None,
        }];
        store.save(&header, &turns, None).unwrap();

        let preview: String = db
            .conn()
            .query_row("SELECT preview FROM turns LIMIT 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(preview, "一二三四五");

        std::fs::remove_dir_all(&tmp).unwrap();
    }

    /// C8/U4/U16 降级契约：API embedder 指向不可达端点（127.0.0.1:9 discard 端口，
    /// 连接必然被拒，离线确定性）时，save() 必须 Ok 且会话数据完整落库，
    /// 仅向量缺失并在 SaveStats 中置位警告。
    /// 注意：embed_batch 对 Transport 错误会重试 3 次（1s + 2s 退避），测试约 3-4s。
    #[test]
    fn test_save_degrades_when_embedder_fails() {
        let tmp = std::env::temp_dir().join(format!(
            "asuna_embed_fail_{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        ));
        std::fs::create_dir_all(&tmp).unwrap();

        let db = Db::open_memory().unwrap();
        db.init_schema().unwrap();

        let mut emb_cfg = crate::config::Config::default().embedding;
        emb_cfg.api_url = "http://127.0.0.1:9/v1".to_string();
        emb_cfg.api_model = "unreachable-test".to_string();
        let embedder = crate::embedder::LazyEmbedder::from_config(&emb_cfg, None)
            .expect("配置了 api_url + api_model，API embedder 应构造成功");

        let store = SessionStore::new(&tmp, &db);
        let stats = store
            .save(&make_header(), &make_turns(), Some(&embedder))
            .expect("嵌入失败不得阻断 save()");

        assert!(stats.vectors_skipped, "嵌入失败必须置位 vectors_skipped");
        assert_eq!(stats.turns_saved, 2);

        let session_count: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(session_count, 1, "sessions 行必须落库");

        let turn_count: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM turns", [], |r| r.get(0))
            .unwrap();
        assert_eq!(turn_count, 2, "turns 行必须落库");

        assert!(stats.file_path.exists(), "JSONL 文件必须写盘");

        let vec_count: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM vec_turns", [], |r| r.get(0))
            .unwrap();
        assert_eq!(vec_count, 0, "向量应为空（本次跳过）");

        std::fs::remove_dir_all(&tmp).unwrap();
    }

    /// J13: save 的向量清理必须限定在本 session——覆盖式重存时旧 turn 的向量行
    /// 被精确删除，其他 session 的向量行（哨兵）不得被波及。
    #[test]
    fn test_save_vec_cleanup_scoped_to_session() {
        let tmp = std::env::temp_dir().join(format!(
            "asuna_vecscope_{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        ));
        std::fs::create_dir_all(&tmp).unwrap();

        let db = Db::open_memory().unwrap();
        db.init_schema().unwrap();
        let store = SessionStore::new(&tmp, &db);

        let unit_vec = |val: f32| {
            let mut v = vec![0.0f32; db.dimensions()];
            v[0] = val;
            v
        };

        // 哨兵：另一个 session 的 turn + 向量，必须全程存活
        db.conn()
            .execute(
                "INSERT INTO sessions (session_id, start_ts, file_path, created_at, updated_at)
                 VALUES ('sentinel-sess', 0, 'sentinel.jsonl', 0, 0)",
                [],
            )
            .unwrap();
        db.conn()
            .execute(
                "INSERT INTO turns (session_id, seq, timestamp_ms, role, preview)
                 VALUES ('sentinel-sess', 1, 0, 'user', 'sentinel')",
                [],
            )
            .unwrap();
        let sentinel_turn_id = db.conn().last_insert_rowid();
        let bytes = crate::embedder::onnx::quantize_to_int8(&unit_vec(9.0));
        db.conn()
            .execute(
                "INSERT INTO vec_turns (rowid, embedding) VALUES (?1, vec_int8(?2))",
                rusqlite::params![sentinel_turn_id, bytes],
            )
            .unwrap();

        let header = make_header();
        let turns = make_turns();

        // 第一次 save（带向量）
        store
            .save_with_embeddings(&header, &turns, Some(&[unit_vec(1.0), unit_vec(2.0)]))
            .unwrap();
        let first_ids: Vec<i64> = db
            .conn()
            .prepare("SELECT id FROM turns WHERE session_id = 'dual-write-test'")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(first_ids.len(), 2);

        let mut vec_rowids: Vec<i64> = db
            .conn()
            .prepare("SELECT rowid FROM vec_turns")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        vec_rowids.sort_unstable();
        let mut expected = vec![sentinel_turn_id];
        expected.extend(first_ids.iter());
        expected.sort_unstable();
        assert_eq!(vec_rowids, expected, "首次 save：哨兵 + 本 session 向量");

        // 第二次 save（覆盖语义）：旧 turn 向量必须清掉，哨兵必须保留
        store
            .save_with_embeddings(&header, &turns, Some(&[unit_vec(3.0), unit_vec(4.0)]))
            .unwrap();
        let new_ids: Vec<i64> = db
            .conn()
            .prepare("SELECT id FROM turns WHERE session_id = 'dual-write-test'")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();

        let mut vec_rowids: Vec<i64> = db
            .conn()
            .prepare("SELECT rowid FROM vec_turns")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        vec_rowids.sort_unstable();
        let mut expected = vec![sentinel_turn_id];
        expected.extend(new_ids.iter());
        expected.sort_unstable();
        assert_eq!(
            vec_rowids, expected,
            "重存后仅剩哨兵 + 新 turn 向量；旧 turn 向量必须被精确清理"
        );
        for old in &first_ids {
            assert!(
                !vec_rowids.contains(old),
                "stale vector rowid {} must be gone",
                old
            );
        }
        assert!(
            vec_rowids.contains(&sentinel_turn_id),
            "sentinel (other session) vector must survive"
        );

        std::fs::remove_dir_all(&tmp).unwrap();
    }

    /// J41 store-level 契约（Append 侧）：同 session 两次 Append → seq 从
    /// MAX(seq) 续排且连续、turn_count 累加、vec_turns 行数 = 总 turns 且
    /// rowid 与 turns.id 一一对应（J33f：统一走 VectorStore::insert）、
    /// file_path 为真实 JSONL 相对路径（J33b：无 gateway:// 伪 URI）且文件
    /// 存在、JSONL 为 header + 全部 5 行（含续排后的 seq）。
    #[test]
    fn test_append_mode_twice_accumulates_with_vectors() {
        let tmp = std::env::temp_dir().join(format!(
            "asuna_append2_{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        ));
        std::fs::create_dir_all(&tmp).unwrap();

        let db = Db::open_memory().unwrap();
        db.init_schema().unwrap();
        let store = SessionStore::new(&tmp, &db);

        let unit_vec = |val: f32| {
            let mut v = vec![0.0f32; db.dimensions()];
            v[0] = val;
            v
        };
        let header = make_header();

        let stats1 = store
            .save_with_embeddings_mode(
                &header,
                &make_turns(),
                Some(&[unit_vec(1.0), unit_vec(2.0)]),
                SaveMode::Append,
            )
            .unwrap();
        assert_eq!(stats1.turns_saved, 2);

        let batch2 = vec![Turn {
            ts: "2026-04-10T14:31:00.000+08:00".to_string(),
            seq: 99, // Append 模式必须忽略输入 seq（/capture 请求体本无 seq）
            role: "user".to_string(),
            content: "追加轮次".to_string(),
            metadata: None,
        }];
        let stats2 = store
            .save_with_embeddings_mode(&header, &batch2, Some(&[unit_vec(3.0)]), SaveMode::Append)
            .unwrap();
        assert_eq!(stats2.turns_saved, 1);

        // seq 连续：1,2,3（第二批不得是 99）
        let seqs: Vec<i64> = {
            let mut stmt = db
                .conn()
                .prepare("SELECT seq FROM turns ORDER BY seq")
                .unwrap();
            stmt.query_map([], |r| r.get(0))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap()
        };
        assert_eq!(seqs, vec![1, 2, 3], "Append 必须从 MAX(seq) 续排");

        // sessions：turn_count 累加，file_path 真实且文件存在
        let (turn_count, file_path): (i64, String) = db
            .conn()
            .query_row(
                "SELECT turn_count, file_path FROM sessions WHERE session_id = ?1",
                rusqlite::params![header.session_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(turn_count, 3, "turn_count 必须累加");
        assert!(
            !file_path.contains("gateway://"),
            "J33b: file_path 不得再写伪 URI, got {file_path}"
        );
        let jsonl = tmp.join(&file_path);
        assert!(jsonl.exists(), "file_path 必须指向真实落盘的 JSONL");

        // JSONL：header + 3 行，行内 seq 与 DB 续排一致
        let (hdr, jsonl_turns) = super::super::conversation::read_session(&jsonl).unwrap();
        assert_eq!(hdr.session_id, header.session_id);
        assert_eq!(jsonl_turns.len(), 3);
        assert_eq!(
            jsonl_turns.iter().map(|t| t.seq).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );

        // 向量：行数 = 总 turns，且 rowid 集合 == 本 session 的 turns.id 集合
        let turn_ids: Vec<i64> = {
            let mut stmt = db
                .conn()
                .prepare("SELECT id FROM turns WHERE session_id = ?1 ORDER BY id")
                .unwrap();
            stmt.query_map(rusqlite::params![header.session_id], |r| r.get(0))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap()
        };
        let vec_ids: Vec<i64> = {
            let mut stmt = db
                .conn()
                .prepare("SELECT rowid FROM vec_turns ORDER BY rowid")
                .unwrap();
            stmt.query_map([], |r| r.get(0))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap()
        };
        assert_eq!(vec_ids, turn_ids, "Append 向量 rowid 必须逐 turn 对应");

        std::fs::remove_dir_all(&tmp).unwrap();
    }

    /// J41 store-level 契约（Overwrite 侧）：同 session 两次 Overwrite →
    /// turns 行数不翻倍、S8 定点清理后 vec_turns 仅剩最新一轮、turn_count
    /// 为全量覆盖值（非累加）、JSONL 全量重写（行数不叠加）、file_path 真实。
    #[test]
    fn test_overwrite_mode_twice_no_row_doubling() {
        let tmp = std::env::temp_dir().join(format!(
            "asuna_ovr2_{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        ));
        std::fs::create_dir_all(&tmp).unwrap();

        let db = Db::open_memory().unwrap();
        db.init_schema().unwrap();
        let store = SessionStore::new(&tmp, &db);

        let unit_vec = |val: f32| {
            let mut v = vec![0.0f32; db.dimensions()];
            v[0] = val;
            v
        };
        let header = make_header();
        let turns = make_turns();

        store
            .save_with_embeddings_mode(
                &header,
                &turns,
                Some(&[unit_vec(1.0), unit_vec(2.0)]),
                SaveMode::Overwrite,
            )
            .unwrap();
        let first_ids: Vec<i64> = {
            let mut stmt = db.conn().prepare("SELECT id FROM turns").unwrap();
            stmt.query_map([], |r| r.get(0))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap()
        };
        store
            .save_with_embeddings_mode(
                &header,
                &turns,
                Some(&[unit_vec(3.0), unit_vec(4.0)]),
                SaveMode::Overwrite,
            )
            .unwrap();

        let turn_count: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM turns", [], |r| r.get(0))
            .unwrap();
        assert_eq!(turn_count, 2, "Overwrite 两次后 turns 行数不得翻倍");

        let vec_count: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM vec_turns", [], |r| r.get(0))
            .unwrap();
        assert_eq!(vec_count, 2, "S8 定点清理：仅剩最新一轮的向量");
        let live_ids: Vec<i64> = {
            let mut stmt = db.conn().prepare("SELECT id FROM turns").unwrap();
            stmt.query_map([], |r| r.get(0))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap()
        };
        for old in &first_ids {
            assert!(
                !live_ids.contains(old) && !has_vec_row(&db, *old),
                "旧 turn 行与其向量都必须被覆盖清掉 (rowid {old})"
            );
        }

        let (sessions_count, session_turn_count, file_path): (i64, i64, String) = db
            .conn()
            .query_row(
                "SELECT COUNT(*), (SELECT turn_count FROM sessions WHERE session_id = 'dual-write-test'),
                        (SELECT file_path FROM sessions WHERE session_id = 'dual-write-test')",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(sessions_count, 1);
        assert_eq!(session_turn_count, 2, "Overwrite 语义为全量覆盖而非累加");
        assert!(!file_path.contains("gateway://"));

        // JSONL 全量重写：header + 2 行（不是两次保存叠加的 5 行）
        let jsonl = tmp.join(&file_path);
        let (_, jsonl_turns) = super::super::conversation::read_session(&jsonl).unwrap();
        assert_eq!(jsonl_turns.len(), 2);

        std::fs::remove_dir_all(&tmp).unwrap();
    }

    /// 辅助：vec_turns 中是否存在指定 rowid（S8 定点清理断言用）
    fn has_vec_row(db: &Db, rowid: i64) -> bool {
        db.conn()
            .query_row(
                "SELECT COUNT(*) FROM vec_turns WHERE rowid = ?1",
                rusqlite::params![rowid],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_or(0)
            > 0
    }

    /// 复现 v2.6.2 REST /capture 的升级前状态（U5 改名 × J33b file_path 改写
    /// 缝隙）：磁盘上是旧命名 JSONL 文件（日期目录/紧凑时间串取 header
    /// start_time 的墙钟值，后缀 = session_id 前 8 字符——手写死值，不复用
    /// `compute_legacy_session_path` 以免自证），DB 行 file_path 为
    /// `gateway://` 伪 URI、turns 两行。
    fn seed_pre_u5_rest_session(tmp: &Path, db: &Db) -> (SessionHeader, PathBuf, Vec<Turn>) {
        let header = make_header();
        let old_turns = make_turns();
        let legacy = tmp
            .join("2026")
            .join("04")
            .join("10")
            .join("20260410T143000_dual-wri.jsonl");
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        crate::index::conversation::write_session_at(&legacy, &header, &old_turns).unwrap();

        let start_ts = crate::util::time::ts_to_unix_ms(&header.start_time).unwrap();
        db.conn()
            .execute(
                "INSERT INTO sessions (session_id, start_ts, file_path, turn_count, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, 0, 0)",
                rusqlite::params![
                    header.session_id,
                    start_ts,
                    format!("gateway://{}", header.session_id),
                    old_turns.len() as i64,
                ],
            )
            .unwrap();
        for t in &old_turns {
            db.conn()
                .execute(
                    "INSERT INTO turns (session_id, seq, timestamp_ms, role, preview, char_count)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    rusqlite::params![
                        header.session_id,
                        t.seq as i64,
                        start_ts,
                        t.role,
                        t.content.chars().take(200).collect::<String>(),
                        t.content.chars().count() as i64,
                    ],
                )
                .unwrap();
        }
        (header, legacy, old_turns)
    }

    /// U5 升级缝隙回归（Append 侧）：v2.6.2 /capture 会话升级后首次追加，旧
    /// 命名文件的 turns 必须迁移进新命名目标文件后删除旧文件（JSONL 真相源
    /// 不丢数据），终态 = 单文件、seq 连续、DB file_path 为真实路径——否则
    /// 同一 session_id 双文件并存，rebuild 混并两份 turns、增量检测恒不一致。
    #[test]
    fn test_append_migrates_pre_u5_legacy_jsonl() {
        let tmp = std::env::temp_dir().join(format!(
            "asuna_legacy_app_{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        let db = Db::open_memory().unwrap();
        db.init_schema().unwrap();
        let (header, legacy, old_turns) = seed_pre_u5_rest_session(&tmp, &db);

        let appended = Turn {
            ts: "2026-04-10T14:35:00.000+08:00".to_string(),
            seq: 99, // Append 忽略输入 seq，事务内续排为 3
            role: "user".to_string(),
            content: "升级后追加".to_string(),
            metadata: None,
        };
        let store = SessionStore::new(&tmp, &db);
        let stats = store
            .save_with_embeddings_mode(&header, &[appended], None, SaveMode::Append)
            .unwrap();

        // 旧文件已迁移并删除，磁盘上本会话只剩一个文件
        assert!(!legacy.exists(), "改名前旧文件必须在迁移后删除");
        let files = crate::index::conversation::list_sessions(&tmp);
        assert_eq!(files.len(), 1, "升级缝隙封死后不得双文件并存");
        assert_eq!(files[0], stats.file_path);

        // DB file_path 已从伪 URI 刷新为真实路径且指向该文件
        let file_rel: String = db
            .conn()
            .query_row(
                "SELECT file_path FROM sessions WHERE session_id = ?1",
                rusqlite::params![header.session_id],
                |r| r.get(0),
            )
            .unwrap();
        assert!(!file_rel.contains("gateway://"), "J33b: {file_rel}");
        assert_eq!(tmp.join(&file_rel), files[0]);

        // 目标文件 = header + 迁移的 2 轮 + 新 1 轮，seq 连续、内容不丢
        let (_, turns) = crate::index::conversation::read_session(&files[0]).unwrap();
        assert_eq!(
            turns.iter().map(|t| t.seq).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert_eq!(turns[0].content, old_turns[0].content, "迁移不得丢 turns");
        assert_eq!(turns[2].content, "升级后追加");

        // DB 与文件行数一致（增量检测的前提）
        let (db_turns, turn_count): (i64, i64) = db
            .conn()
            .query_row(
                "SELECT (SELECT COUNT(*) FROM turns), (SELECT turn_count FROM sessions WHERE session_id = ?1)",
                rusqlite::params![header.session_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(db_turns, 3);
        assert_eq!(turn_count, 3);

        std::fs::remove_dir_all(&tmp).unwrap();
    }

    /// U5 升级缝隙回归（Overwrite 侧）：升级后旧会话首次 MCP save_session /
    /// CLI import 覆盖修正——DB 伪 URI 定位不到旧文件，但新命名规则复原必须
    /// 命中并删除旧文件（旧内容按覆盖语义丢弃、不迁移）。若放任并存，rebuild
    /// 会把被修正前的旧内容以重复 seq 复活。
    #[test]
    fn test_overwrite_removes_pre_u5_legacy_jsonl() {
        let tmp = std::env::temp_dir().join(format!(
            "asuna_legacy_ovr_{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        let db = Db::open_memory().unwrap();
        db.init_schema().unwrap();
        let (header, legacy, _) = seed_pre_u5_rest_session(&tmp, &db);

        let corrected = Turn {
            ts: "2026-04-10T14:36:00.000+08:00".to_string(),
            seq: 1,
            role: "user".to_string(),
            content: "修正后内容".to_string(),
            metadata: None,
        };
        let store = SessionStore::new(&tmp, &db);
        store
            .save_with_embeddings_mode(&header, &[corrected], None, SaveMode::Overwrite)
            .unwrap();

        assert!(
            !legacy.exists(),
            "覆盖语义：改名前旧文件必须被删除（防 rebuild 复活）"
        );
        let files = crate::index::conversation::list_sessions(&tmp);
        assert_eq!(files.len(), 1);
        let (_, turns) = crate::index::conversation::read_session(&files[0]).unwrap();
        assert_eq!(turns.len(), 1, "请求内容即新真相");
        assert_eq!(turns[0].content, "修正后内容");
        let file_rel: String = db
            .conn()
            .query_row(
                "SELECT file_path FROM sessions WHERE session_id = ?1",
                rusqlite::params![header.session_id],
                |r| r.get(0),
            )
            .unwrap();
        assert!(!file_rel.contains("gateway://"));

        std::fs::remove_dir_all(&tmp).unwrap();
    }

    /// 删除前置校验：U5 之前的命名规则下，前 8 字符相同（"dual-write-test"/
    /// "dual-writex"）的两个会话共享旧文件名——候选路径上真实存在的可能是
    /// **别的会话**的文件（header session_id 不匹配）。清理必须按 header 校验
    /// 身份，外来文件原样保留。
    #[test]
    fn test_legacy_cleanup_never_touches_foreign_session_file() {
        let tmp = std::env::temp_dir().join(format!(
            "asuna_legacy_foreign_{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        let db = Db::open_memory().unwrap();
        db.init_schema().unwrap();
        let (header, legacy, _) = seed_pre_u5_rest_session(&tmp, &db);
        // 把该文件伪装成"同前缀的另一会话"（v2.6.2 前缀碰撞的既成事实）
        let mut foreign_header = header.clone();
        foreign_header.session_id = "dual-writex".to_string(); // 前 8 字符同为 "dual-wri"
        crate::index::conversation::write_session_at(
            &legacy,
            &foreign_header,
            &[Turn {
                ts: "2026-04-10T14:30:05.000+08:00".to_string(),
                seq: 1,
                role: "user".to_string(),
                content: "别家会话的数据".to_string(),
                metadata: None,
            }],
        )
        .unwrap();

        let store = SessionStore::new(&tmp, &db);
        store
            .save_with_embeddings_mode(&header, &make_turns(), None, SaveMode::Append)
            .unwrap();

        assert!(
            legacy.exists(),
            "session_id 不匹配的旧命名文件（前缀碰撞）不得被删除或迁移"
        );
        let files = crate::index::conversation::list_sessions(&tmp);
        assert_eq!(files.len(), 2, "外来文件保留 + 本会话新文件");
        let own = files.iter().find(|f| *f != &legacy).unwrap().clone();
        let (h, turns) = crate::index::conversation::read_session(&own).unwrap();
        assert_eq!(h.session_id, header.session_id);
        assert_eq!(
            turns.iter().map(|t| t.seq).collect::<Vec<_>>(),
            vec![3, 4],
            "不迁移外来内容，只追加本批（DB 已有 1,2，续排 3,4）"
        );
        let (_, foreign_turns) = crate::index::conversation::read_session(&legacy).unwrap();
        assert_eq!(foreign_turns[0].content, "别家会话的数据");

        std::fs::remove_dir_all(&tmp).unwrap();
    }
}

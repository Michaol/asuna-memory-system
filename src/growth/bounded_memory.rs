use std::path::{Path, PathBuf};
use crate::index::db::Db;
use crate::util::time;

const ENTRY_SEPARATOR: &str = "\n§\n";

/// 安全截取字符串前 N 个 Unicode 字符（不会切断 UTF-8 多字节序列）
fn truncate_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// 转义 SQLite LIKE 通配符（%, _, \），使用 \ 作为 ESCAPE 字符
fn escape_like(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' | '%' | '_' => {
                out.push('\\');
                out.push(c);
            }
            other => out.push(other),
        }
    }
    out
}

/// 有界记忆管理器
pub struct BoundedMemory<'a> {
    memory_dir: PathBuf,
    db: &'a Db,
    memory_limit: usize,
    user_limit: usize,
    security_scan: bool,
    /// Fraction of MEMORY.md capacity reserved for auto-extracted atoms (default 0.3).
    atom_capacity_ratio: f64,
}

/// 溯源验证结果
#[derive(Debug, serde::Serialize)]
pub struct ProvenanceInfo {
    pub target: String,
    pub content: String,
    pub source_session: Option<String>,
    pub confidence: String,
    pub created_at: String,
    pub session_exists: bool,
    pub session_file_path: Option<String>,
}

impl<'a> BoundedMemory<'a> {
    pub fn new(memory_dir: &Path, db: &'a Db, memory_limit: usize, user_limit: usize) -> Self {
        Self {
            memory_dir: memory_dir.to_path_buf(),
            db,
            memory_limit,
            user_limit,
            security_scan: true,
            atom_capacity_ratio: 0.3,
        }
    }

    /// 可选关闭安全扫描（用于受信任的内部调用 / 配置覆盖）
    pub fn with_security_scan(mut self, enabled: bool) -> Self {
        self.security_scan = enabled;
        self
    }

    /// 设置 atom 容量占比（MEMORY.md 中分配给自动提取 atoms 的比例）
    pub fn with_atom_capacity_ratio(mut self, ratio: f64) -> Self {
        self.atom_capacity_ratio = ratio;
        self
    }

    /// 严格白名单验证 target，禁止路径穿越
    fn target_file(&self, target: &str) -> anyhow::Result<PathBuf> {
        match target {
            "memory" => Ok(self.memory_dir.join("MEMORY.md")),
            "user" => Ok(self.memory_dir.join("USER.md")),
            other => anyhow::bail!("非法 target: {} (仅支持 'memory' / 'user')", other),
        }
    }

    fn capacity(&self, target: &str) -> usize {
        match target {
            "memory" => self.memory_limit,
            "user" => self.user_limit,
            _ => self.memory_limit,
        }
    }

    fn metadata_header(&self, target: &str, capacity: usize) -> String {
        let label = if target == "user" { "ASUNA USER PROFILE" } else { "ASUNA MEMORY" };
        let updated = time::unix_ms_to_iso(time::now_unix_ms());
        format!("<!-- {} | capacity: {} chars | updated: {} -->", label, capacity, updated)
    }

    fn run_scan(&self, content: &str) -> anyhow::Result<()> {
        if !self.security_scan {
            return Ok(());
        }
        let scan = crate::growth::security::scan_content(content);
        if !scan.is_safe() {
            anyhow::bail!("安全扫描未通过: {}", scan.reason());
        }
        Ok(())
    }

    /// 读取全文
    pub fn read(&self, target: &str) -> anyhow::Result<String> {
        let path = self.target_file(target)?;
        if path.exists() {
            Ok(std::fs::read_to_string(path)?)
        } else {
            Ok(String::new())
        }
    }

    /// 写入新条目（追加）
    pub fn write(&self, target: &str, content: &str, confidence: &str, session_id: Option<&str>) -> anyhow::Result<()> {
        self.run_scan(content)?;

        // 拒绝含条目分隔符的 content，否则一行会包含多个逻辑条目，
        // 导致 .md 与 DB 行数不一致，reconcile_check 误报差异。
        if content.contains(ENTRY_SEPARATOR) {
            anyhow::bail!("content 不能包含条目分隔符 '\\n§\\n'（一次只能写一个条目；多条请用多次 write）");
        }

        let path = self.target_file(target)?;
        let capacity = self.capacity(target);

        let current = self.read(target)?;
        let body = extract_body(&current);

        // 检查是否重复
        if body.split(ENTRY_SEPARATOR).any(|e| e.trim() == content.trim()) {
            anyhow::bail!("条目已存在，拒绝重复写入");
        }

        // 计算新内容
        let new_body = if body.is_empty() {
            content.to_string()
        } else {
            format!("{}{}{}", body, ENTRY_SEPARATOR, content)
        };

        // 容量检查
        if new_body.chars().count() > capacity {
            anyhow::bail!(
                "超出容量上限: {}/{} 字符。请先整合或删除旧条目",
                new_body.chars().count(),
                capacity
            );
        }

        // SQLite FIRST — 失败则 .md 不被触碰，保证一致性
        let now = time::now_unix_ms();
        self.db.conn().execute(
            "INSERT INTO bounded_memory (target, content, created_at, updated_at, source_session, confidence)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![target, content, now, now, session_id, confidence],
        )?;

        // 审计日志
        crate::growth::audit::log_action(
            self.db,
            "write",
            target,
            &serde_json::json!({"content_preview": truncate_chars(content, 100)}).to_string(),
            session_id,
        )?;

        // .md file LAST — 如果失败，DB 有记录可通过 reconcile_fix 恢复
        let header = self.metadata_header(target, capacity);
        let full = format!("{}\n\n{}", header, new_body);
        std::fs::create_dir_all(&self.memory_dir)?;
        std::fs::write(&path, full)?;

        Ok(())
    }

    /// 按条目（§ 分隔）匹配并整体替换。
    /// old_text 必须匹配某个**完整条目**或其中某个条目的子串；
    /// 若匹配多个条目则全部更新，避免文件层与 DB 层语义漂移。
    pub fn update(&self, target: &str, old_text: &str, new_text: &str, session_id: Option<&str>) -> anyhow::Result<()> {
        self.run_scan(new_text)?;

        // 拒绝含条目分隔符的 new_text：update 按条目粒度替换，
        // 若 new_text 含 § 会把一条目分裂成多条，破坏 DB/.md 一致性。
        if new_text.contains(ENTRY_SEPARATOR) {
            anyhow::bail!("new_text 不能包含条目分隔符 '\\n§\\n'（update 仅能修改单个条目内容）");
        }

        let capacity = self.capacity(target);
        let current = self.read(target)?;
        let body = extract_body(&current);

        if !body.contains(old_text) {
            anyhow::bail!("未找到要替换的文本");
        }

        // 按条目分割，逐条匹配
        let entries: Vec<&str> = body.split(ENTRY_SEPARATOR).collect();
        let mut updated_entries: Vec<String> = Vec::with_capacity(entries.len());
        let mut hits = 0usize;
        for entry in entries {
            if entry.contains(old_text) {
                updated_entries.push(entry.replace(old_text, new_text));
                hits += 1;
            } else {
                updated_entries.push(entry.to_string());
            }
        }
        let updated_body = updated_entries.join(ENTRY_SEPARATOR);

        let updated_char_count = updated_body.chars().count();
        if updated_char_count > capacity {
            anyhow::bail!("替换后超出容量上限: {}/{} 字符", updated_char_count, capacity);
        }

        let header = self.metadata_header(target, capacity);
        let full = format!("{}\n\n{}", header, updated_body);
        let path = self.target_file(target)?;

        // SQLite FIRST — 失败则 .md 不被触碰
        let escaped = escape_like(old_text);
        self.db.conn().execute(
            "UPDATE bounded_memory SET content = REPLACE(content, ?1, ?2), updated_at = ?3
             WHERE target = ?4 AND content LIKE ?5 ESCAPE '\\'",
            rusqlite::params![old_text, new_text, time::now_unix_ms(), target, format!("%{}%", escaped)],
        )?;

        crate::growth::audit::log_action(
            self.db,
            "update",
            target,
            &serde_json::json!({
                "old": truncate_chars(old_text, 50),
                "new": truncate_chars(new_text, 50),
                "entries_affected": hits
            }).to_string(),
            session_id,
        )?;

        // .md file LAST
        std::fs::write(&path, full)?;

        Ok(())
    }

    /// 按条目级匹配删除：包含 old_text 的整条条目被移除（保证 § 分隔符规范）
    pub fn remove(&self, target: &str, old_text: &str, session_id: Option<&str>) -> anyhow::Result<()> {
        let path = self.target_file(target)?;
        let current = self.read(target)?;
        let body = extract_body(&current);

        if !body.contains(old_text) {
            anyhow::bail!("未找到要删除的文本");
        }

        // 条目级过滤：丢弃含 old_text 的整条条目，余下重组
        let kept: Vec<&str> = body
            .split(ENTRY_SEPARATOR)
            .filter(|entry| !entry.contains(old_text))
            .collect();
        let new_body = kept.join(ENTRY_SEPARATOR);

        let capacity = self.capacity(target);
        let header = self.metadata_header(target, capacity);
        let full = if new_body.trim().is_empty() {
            format!("{}\n\n", header)
        } else {
            format!("{}\n\n{}", header, new_body)
        };

        // SQLite FIRST — 失败则 .md 不被触碰
        let escaped = escape_like(old_text);
        self.db.conn().execute(
            "DELETE FROM bounded_memory WHERE target = ?1 AND content LIKE ?2 ESCAPE '\\'",
            rusqlite::params![target, format!("%{}%", escaped)],
        )?;

        crate::growth::audit::log_action(
            self.db,
            "remove",
            target,
            &serde_json::json!({"removed": truncate_chars(old_text, 50)}).to_string(),
            session_id,
        )?;

        // .md file LAST
        std::fs::write(&path, full)?;

        Ok(())
    }

    /// 查询指定 target 的所有记忆条目（含溯源信息）
    pub fn list_entries(&self, target: &str) -> anyhow::Result<Vec<ProvenanceInfo>> {
        let mut stmt = self.db.conn().prepare(
            "SELECT target, content, source_session, confidence, created_at
             FROM bounded_memory
             WHERE target = ?1
             ORDER BY created_at DESC"
        )?;

        let rows = stmt.query_map(rusqlite::params![target], |row| {
            Ok(ProvenanceInfo {
                target: row.get(0)?,
                content: row.get(1)?,
                source_session: row.get(2)?,
                confidence: row.get(3)?,
                created_at: {
                    let ms: i64 = row.get(4)?;
                    time::unix_ms_to_iso(ms)
                },
                session_exists: false,
                session_file_path: None,
            })
        })?;

        let mut results: Vec<ProvenanceInfo> = rows.filter_map(|r| r.ok()).collect();

        // Batch fetch session file paths in a single query (avoid N+1)
        let session_ids: Vec<String> = results
            .iter()
            .filter_map(|info| info.source_session.clone())
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();

        if !session_ids.is_empty() {
            let placeholders: Vec<String> = (1..=session_ids.len())
                .map(|i| format!("?{}", i))
                .collect();
            let sql = format!(
                "SELECT session_id, file_path FROM sessions WHERE session_id IN ({})",
                placeholders.join(", ")
            );
            let mut session_stmt = self.db.conn().prepare(&sql)?;
            let params: Vec<Box<dyn rusqlite::types::ToSql>> = session_ids
                .iter()
                .map(|s| Box::new(s.clone()) as Box<dyn rusqlite::types::ToSql>)
                .collect();
            let param_refs: Vec<&dyn rusqlite::types::ToSql> =
                params.iter().map(|p| p.as_ref()).collect();

            let mut path_map = std::collections::HashMap::new();
            let session_rows = session_stmt.query_map(param_refs.as_slice(), |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            for row in session_rows {
                if let Ok((sid, path)) = row {
                    path_map.insert(sid, path);
                }
            }

            for info in &mut results {
                if let Some(ref sid) = info.source_session {
                    if let Some(path) = path_map.get(sid) {
                        info.session_exists = true;
                        info.session_file_path = Some(path.clone());
                    }
                }
            }
        }

        Ok(results)
    }

    /// 验证成长层记忆与事实层的一致性
    pub fn verify_provenance(&self, target: &str) -> anyhow::Result<ProvenanceReport> {
        let entries = self.list_entries(target)?;
        let total = entries.len();
        let verified = entries.iter().filter(|e| e.source_session.is_some() && e.session_exists).count();
        let missing = entries.iter().filter(|e| e.source_session.is_some() && !e.session_exists).count();
        let no_source = entries.iter().filter(|e| e.source_session.is_none()).count();

        Ok(ProvenanceReport {
            target: target.to_string(),
            total_entries: total,
            verified,
            missing_source: missing,
            no_source,
            entries,
        })
    }

    /// 对比 .md 条目与 SQLite 行，返回差异报告。
    ///
    /// 注意：使用 HashSet 比较，会按内容去重。若 .md 或 DB 中存在完全相同的
    /// 重复条目（正常写入路径已去重，仅手动操作 DB 才可能出现），
    /// `md_entry_count`/`db_entry_count` 包含重复计数，但 `only_in_md`/`only_in_db`
    /// 不包含重复项（6.1 fix）。
    pub fn reconcile_check(&self, target: &str) -> anyhow::Result<ReconcileReport> {
        let md_content = self.read(target)?;
        let md_body = extract_body(&md_content);
        let md_entries: Vec<&str> = if md_body.is_empty() {
            vec![]
        } else {
            md_body.split(ENTRY_SEPARATOR).collect()
        };

        let mut stmt = self.db.conn().prepare(
            "SELECT content FROM bounded_memory WHERE target = ?1 ORDER BY created_at"
        )?;
        let db_entries: Vec<String> = stmt.query_map(
            rusqlite::params![target], |row| row.get::<_, String>(0)
        )?.filter_map(|r| r.ok()).collect();

        let md_set: std::collections::HashSet<&str> = md_entries.iter().map(|e| e.trim()).collect();
        let db_set: std::collections::HashSet<&str> = db_entries.iter().map(|e| e.trim()).collect();

        let only_in_md: Vec<String> = md_set.difference(&db_set).map(|s| s.to_string()).collect();
        let only_in_db: Vec<String> = db_set.difference(&md_set).map(|s| s.to_string()).collect();

        Ok(ReconcileReport {
            target: target.to_string(),
            md_entry_count: md_entries.len(),
            db_entry_count: db_entries.len(),
            only_in_md,
            only_in_db,
        })
    }

    /// 无损合并 .md 和 SQLite（修复 DB/文件不一致）。
    ///
    /// - .md 独有的条目 → 插入 SQLite（精确内容匹配去重，语义重复不处理）
    /// - SQLite 独有的条目 → 追加到 .md
    /// - 两边都有的 → 不变
    ///
    /// 若合并后超过容量上限，写入会附带警告但仍执行。
    pub fn reconcile_fix(&self, target: &str) -> anyhow::Result<usize> {
        // 0. 先拆分含多个 § 分隔符的坏行（幂等；无坏行则为 no-op）。
        //    必须在 reconcile_check 之前跑，否则多条目行会让 check 误报差异，
        //    后续"无损合并"反而把已经展开的子条目重复写回 DB。
        let split = self.split_multi_entry_rows(target)?;
        if split.bad_rows > 0 {
            tracing::info!(
                "reconcile_fix[{}]: 拆分 {} 条多条目坏行 → 新增 {} 子条目, 跳过 {} 重复",
                target, split.bad_rows, split.sub_entries_created, split.duplicates_skipped
            );
        }

        // 1. 获取差异报告
        let report = self.reconcile_check(target)?;

        // 2. .md 独有 → 插入 DB
        // 'memory' 目标存放 LLM 抽取的 atom；以 'atom' 重插，避免被误标为永不淘汰的
        // 'manual'（否则会绕过容量淘汰、并污染来源标记）。其它目标（如 user）仍为 manual。
        let now = time::now_unix_ms();
        let reinsert_type = if target == "memory" { "atom" } else { "manual" };
        let mut inserted = 0usize;
        for entry in &report.only_in_md {
            self.db.conn().execute(
                "INSERT INTO bounded_memory (target, content, created_at, updated_at, confidence, memory_type)
                 VALUES (?1, ?2, ?3, ?4, 'medium', ?5)",
                rusqlite::params![target, entry, now, now, reinsert_type],
            )?;
            inserted += 1;
        }

        // 3. 重读完整 DB → 重建 .md
        let total = self.rebuild_md_from_db(target)?;

        tracing::info!(
            "reconcile_fix[{}]: .md独有 {} 条已入库, SQLite独有 {} 条已合并, 总计 {} 条",
            target, inserted, report.only_in_db.len(), total
        );
        Ok(total)
    }

    /// 从 DB 全量重建目标 target 的 .md 文件（覆盖写入）。
    ///
    /// 用于在 DB 内容发生结构性变化（如拆分多条目行、合并差异）后保持 .md 与 DB 一致。
    /// 不会主动触发淘汰；若超出容量会打 warn 但仍写入（与 reconcile_fix 行为一致）。
    fn rebuild_md_from_db(&self, target: &str) -> anyhow::Result<usize> {
        let capacity = self.capacity(target);
        let mut stmt = self.db.conn().prepare(
            "SELECT content FROM bounded_memory WHERE target = ?1 ORDER BY created_at",
        )?;
        let all_entries: Vec<String> = stmt
            .query_map(rusqlite::params![target], |row| row.get::<_, String>(0))?
            .filter_map(|r| r.ok())
            .collect();

        let new_body = all_entries.join(ENTRY_SEPARATOR);
        let body_chars = new_body.chars().count();
        if body_chars > capacity {
            tracing::warn!(
                "rebuild_md_from_db[{}]: 重建后总长 {} 超出容量 {}，写入会携带警告",
                target, body_chars, capacity
            );
        }
        let header = self.metadata_header(target, capacity);
        let full = if new_body.trim().is_empty() {
            format!("{}\n\n", header)
        } else {
            format!("{}\n\n{}", header, new_body)
        };
        let path = self.target_file(target)?;
        std::fs::create_dir_all(&self.memory_dir)?;
        std::fs::write(&path, full)?;
        Ok(all_entries.len())
    }

    /// 拆分 target 中含多个 § 分隔符的坏行（一行多条目 → 多行各一单条目）。
    ///
    /// 设计约束：bounded_memory 每行应只存一个条目。若某行 content 含 `\n§\n`，
    /// 则 .md（用 `\n§\n` 拼接后按 `\n§\n` 拆分）会看到多个逻辑条目，
    /// 而 DB 行数统计只看到一行，导致 `reconcile_check` 误报差异。
    ///
    /// 行为：
    /// - 拆分后每条子条目作为新行插入，保留原行的元数据（created_at/updated_at/
    ///   confidence/memory_type/source_session/supersedes_id/source_turn_ids/confidence_score）
    /// - 若某子条目已存在于 DB（精确字符串匹配），跳过（不产生重复）
    /// - 删除原坏行
    /// - 重建该 target 的 .md 文件
    ///
    /// 幂等：无坏行时返回全零报告，不改动 DB 或 .md。
    pub fn split_multi_entry_rows(&self, target: &str) -> anyhow::Result<SplitReport> {
        let conn = self.db.conn();

        // 1. 找出含 ENTRY_SEPARATOR 的坏行
        // 单行内所有字段打包到一个 struct，避免 10 元素 tuple 触发 clippy::type_complexity
        struct BadRow {
            id: i64,
            content: String,
            created_at: i64,
            updated_at: i64,
            source_session: Option<String>,
            confidence: String,
            memory_type: String,
            supersedes_id: Option<i64>,
            source_turn_ids: Option<String>,
            confidence_score: Option<f64>,
        }
        let sep_pattern = format!("%{}%", ENTRY_SEPARATOR);
        let mut stmt = conn.prepare(
            "SELECT id, content, created_at, updated_at, source_session,
                    confidence, memory_type, supersedes_id, source_turn_ids, confidence_score
             FROM bounded_memory
             WHERE target = ?1 AND content LIKE ?2 ESCAPE '\\'",
        )?;
        let bad_rows: Vec<BadRow> = stmt
            .query_map(rusqlite::params![target, sep_pattern], |row| {
                Ok(BadRow {
                    id: row.get::<_, i64>(0)?,
                    content: row.get::<_, String>(1)?,
                    created_at: row.get::<_, i64>(2)?,
                    updated_at: row.get::<_, i64>(3)?,
                    source_session: row.get::<_, Option<String>>(4)?,
                    confidence: row.get::<_, String>(5)?,
                    memory_type: row.get::<_, String>(6)?,
                    supersedes_id: row.get::<_, Option<i64>>(7)?,
                    source_turn_ids: row.get::<_, Option<String>>(8)?,
                    confidence_score: row.get::<_, Option<f64>>(9)?,
                })
            })?
            .filter_map(|r| r.ok())
            .collect();

        if bad_rows.is_empty() {
            return Ok(SplitReport {
                target: target.to_string(),
                bad_rows: 0,
                sub_entries_created: 0,
                duplicates_skipped: 0,
            });
        }

        // 2. 预编译查重 + 插入 + 删除语句
        let mut exists_stmt = conn.prepare(
            "SELECT 1 FROM bounded_memory WHERE target = ?1 AND content = ?2 LIMIT 1",
        )?;
        let mut insert_stmt = conn.prepare(
            "INSERT INTO bounded_memory
                (target, content, created_at, updated_at, source_session,
                 confidence, memory_type, supersedes_id, source_turn_ids, confidence_score)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        )?;
        let mut delete_stmt = conn.prepare("DELETE FROM bounded_memory WHERE id = ?1")?;

        let mut created = 0usize;
        let mut skipped = 0usize;

        for row in &bad_rows {
            let sub_entries: Vec<&str> = row.content
                .split(ENTRY_SEPARATOR)
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .collect();

            for sub in sub_entries {
                // 查重
                let exists: bool = exists_stmt
                    .query_row(rusqlite::params![target, sub], |_| Ok(()))
                    .is_ok();
                if exists {
                    skipped += 1;
                    continue;
                }
                insert_stmt.execute(rusqlite::params![
                    target,
                    sub,
                    row.created_at,
                    row.updated_at,
                    row.source_session,
                    row.confidence,
                    row.memory_type,
                    row.supersedes_id,
                    row.source_turn_ids,
                    row.confidence_score,
                ])?;
                created += 1;
            }
            delete_stmt.execute(rusqlite::params![row.id])?;
        }

        // 3. 重建 .md，使文件与 DB 一致
        self.rebuild_md_from_db(target)?;

        Ok(SplitReport {
            target: target.to_string(),
            bad_rows: bad_rows.len(),
            sub_entries_created: created,
            duplicates_skipped: skipped,
        })
    }

    /// Sync auto-extracted atoms to MEMORY.md with capacity-aware eviction.
    ///
    /// Called after `L1Extractor::store_atoms()` commits new atoms to DB.
    /// Evicts oldest `memory_type='atom'` entries if they exceed the atom budget,
    /// then rebuilds MEMORY.md from DB to maintain consistency.
    ///
    /// Returns the number of entries evicted.
    pub fn sync_atoms_to_md(&self) -> anyhow::Result<usize> {
        let atom_budget = (self.memory_limit as f64 * self.atom_capacity_ratio) as usize;
        let capacity = self.capacity("memory");

        // Footprint of protected (non-atom) entries — these are never evicted, but
        // they count toward the total capacity bound.
        let manual_chars: usize = {
            let mut stmt = self.db.conn().prepare(
                "SELECT content FROM bounded_memory
                 WHERE target = 'memory' AND COALESCE(memory_type, 'manual') != 'atom'",
            )?;
            let v: usize = stmt
                .query_map([], |row| row.get::<_, String>(0))?
                .filter_map(|r| r.ok())
                .map(|c| c.chars().count() + 3)
                .sum();
            v
        };

        // Get current atom entries ordered oldest-first (eviction candidates)
        let mut stmt = self.db.conn().prepare(
            "SELECT id, content FROM bounded_memory
             WHERE target = 'memory' AND COALESCE(memory_type, 'manual') = 'atom'
             ORDER BY created_at ASC",
        )?;
        let atoms: Vec<(i64, String)> = stmt
            .query_map([], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?
            .filter_map(|r| r.ok())
            .collect();

        // Each entry contributes content chars + 3 for the "§\n" separator.
        let mut remaining_atom: usize = atoms.iter().map(|(_, c)| c.chars().count() + 3).sum();

        // Evict oldest atoms until BOTH the atom budget AND the total capacity
        // (manual + atom) are satisfied. Manual entries are protected, so if they
        // alone exceed capacity we cannot fully enforce the bound (warn below).
        let mut evicted = 0usize;
        let mut freed = 0usize;
        for (id, content) in &atoms {
            let atom_ok = remaining_atom <= atom_budget;
            let total_ok = manual_chars + remaining_atom <= capacity;
            if atom_ok && total_ok {
                break;
            }
            self.db.conn().execute(
                "DELETE FROM bounded_memory WHERE id = ?1",
                rusqlite::params![id],
            )?;
            let _ = self.db.conn().execute(
                "DELETE FROM vec_bounded_memory WHERE id = ?1",
                rusqlite::params![id],
            );
            let c = content.chars().count() + 3;
            remaining_atom = remaining_atom.saturating_sub(c);
            freed += c;
            evicted += 1;
        }
        if evicted > 0 {
            tracing::info!(
                "Atom capacity eviction: removed {} atoms (freed {} chars, atom_budget {}, capacity {})",
                evicted, freed, atom_budget, capacity
            );
            // Eviction permanently destroys auto-extracted memory; record it durably
            // for provenance (other mutations already audit; eviction previously did not).
            let _ = crate::growth::audit::log_action(
                self.db,
                "evict",
                "memory",
                &format!("evicted={} freed_chars={} atom_budget={} capacity={}", evicted, freed, atom_budget, capacity),
                None,
            );
        }
        if manual_chars > capacity {
            tracing::warn!(
                "bounded memory 'memory': 受保护(manual)条目共 {} 字符已超出容量 {}，无法通过淘汰 atom 收敛",
                manual_chars, capacity
            );
        }

        // Rebuild MEMORY.md directly from DB (includes both manual + atom entries).
        // We intentionally do NOT call reconcile_fix() here — after eviction the DB
        // state is authoritative, and reconcile_fix would re-insert evicted atoms
        // that are still in .md (regression).
        let mut stmt = self.db.conn().prepare(
            "SELECT content FROM bounded_memory WHERE target = 'memory' ORDER BY created_at"
        )?;
        let all_entries: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .filter_map(|r| r.ok())
            .collect();

        let capacity = self.capacity("memory");
        let body = all_entries.join(ENTRY_SEPARATOR);
        let header = self.metadata_header("memory", capacity);
        let full = if body.trim().is_empty() {
            format!("{}\n\n", header)
        } else {
            format!("{}\n\n{}", header, body)
        };
        let path = self.target_file("memory")?;
        std::fs::write(&path, full)?;

        Ok(evicted)
    }
}

/// 溯源验证报告
#[derive(Debug, serde::Serialize)]
pub struct ProvenanceReport {
    pub target: String,
    pub total_entries: usize,
    pub verified: usize,
    pub missing_source: usize,
    pub no_source: usize,
    pub entries: Vec<ProvenanceInfo>,
}

/// DB/.md 一致性检查报告
#[derive(Debug, serde::Serialize)]
pub struct ReconcileReport {
    pub target: String,
    pub md_entry_count: usize,
    pub db_entry_count: usize,
    pub only_in_md: Vec<String>,
    pub only_in_db: Vec<String>,
}

/// 多条目行拆分报告
#[derive(Debug, serde::Serialize)]
pub struct SplitReport {
    pub target: String,
    /// 拆分了多少条含多个 § 分隔符的坏行
    pub bad_rows: usize,
    /// 拆分后实际新增的子条目行数（不含跳过重复）
    pub sub_entries_created: usize,
    /// 因 DB 中已有精确匹配而跳过的子条目数
    pub duplicates_skipped: usize,
}

/// 从完整文件内容中提取条目正文（去掉元数据头）
fn extract_body(content: &str) -> String {
    if content.is_empty() {
        return String::new();
    }
    let lines: Vec<&str> = content.lines().collect();
    let start = if lines.first().is_some_and(|l| l.contains("<!-- ASUNA")) {
        2 // 跳过头和空行
    } else {
        0
    };
    // Guard against a truncated/corrupted file (e.g. only the header line): a bare
    // `lines[start..]` would panic when start > lines.len().
    lines.get(start..).map(|s| s.join("\n")).unwrap_or_default().trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (std::path::PathBuf, Db) {
        let dir = std::env::temp_dir().join(format!(
            "asuna_growth_{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let db = Db::open_memory().unwrap();
        db.init_schema().unwrap();
        (dir, db)
    }

    #[test]
    fn test_memory_write_and_read() {
        let (dir, db) = setup();
        let bm = BoundedMemory::new(&dir, &db, 2200, 1375);

        bm.write("memory", "用户喜欢简洁回复", "high", None).unwrap();
        let content = bm.read("memory").unwrap();
        assert!(content.contains("用户喜欢简洁回复"));
        assert!(content.contains("ASUNA MEMORY"));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_capacity_limit() {
        let (dir, db) = setup();
        let bm = BoundedMemory::new(&dir, &db, 100, 100);

        let long_content = "a".repeat(90);
        bm.write("memory", &long_content, "medium", None).unwrap();

        let extra = "b".repeat(50);
        let result = bm.write("memory", &extra, "low", None);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("容量上限"));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_duplicate_rejection() {
        let (dir, db) = setup();
        let bm = BoundedMemory::new(&dir, &db, 2200, 1375);

        bm.write("memory", "重复条目", "high", None).unwrap();
        let result = bm.write("memory", "重复条目", "high", None);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("重复"));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_update_substring() {
        let (dir, db) = setup();
        let bm = BoundedMemory::new(&dir, &db, 2200, 1375);

        bm.write("memory", "旧信息", "medium", None).unwrap();
        bm.update("memory", "旧信息", "新信息", None).unwrap();

        let content = bm.read("memory").unwrap();
        assert!(content.contains("新信息"));
        assert!(!content.contains("旧信息"));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_remove_entry() {
        let (dir, db) = setup();
        let bm = BoundedMemory::new(&dir, &db, 2200, 1375);

        bm.write("memory", "条目A", "high", None).unwrap();
        bm.write("memory", "条目B", "high", None).unwrap();
        bm.remove("memory", "条目A", None).unwrap();

        let content = bm.read("memory").unwrap();
        assert!(!content.contains("条目A"));
        assert!(content.contains("条目B"));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// 路径穿越攻击防御：非白名单 target 必须被拒绝
    #[test]
    fn test_reject_path_traversal_target() {
        let (dir, db) = setup();
        let bm = BoundedMemory::new(&dir, &db, 2200, 1375);

        for evil in ["../etc/passwd", "..\\..\\config", "memory/../user", "."] {
            let result = bm.write(evil, "x", "high", None);
            assert!(result.is_err(), "应拒绝非法 target: {}", evil);
            assert!(
                result.unwrap_err().to_string().contains("非法 target"),
                "应返回非法 target 错误"
            );
        }

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// 中文内容超过 100 字符时不应 panic（之前是字节切片）
    #[test]
    fn test_long_chinese_content_no_panic() {
        let (dir, db) = setup();
        let bm = BoundedMemory::new(&dir, &db, 2200, 1375);
        let long_zh = "中".repeat(80);
        bm.write("memory", &long_zh, "low", None).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// LIKE 通配符 % / _ 不应被当通配符处理（已转义）
    #[test]
    fn test_like_wildcard_escaping() {
        let (dir, db) = setup();
        let bm = BoundedMemory::new(&dir, &db, 2200, 1375);
        bm.write("memory", "纯文本条目", "low", None).unwrap();
        // 含 % 的 old_text 不应误匹配纯文本
        let r = bm.update("memory", "%", "X", None);
        assert!(r.is_err(), "未匹配应报错而不是误匹配");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// 删除多个相邻条目后分隔符不应残留为 §§§§
    #[test]
    fn test_remove_separator_cleanup() {
        let (dir, db) = setup();
        let bm = BoundedMemory::new(&dir, &db, 2200, 1375);
        bm.write("memory", "A", "low", None).unwrap();
        bm.write("memory", "B", "low", None).unwrap();
        bm.write("memory", "C", "low", None).unwrap();
        bm.remove("memory", "A", None).unwrap();
        bm.remove("memory", "B", None).unwrap();
        let content = bm.read("memory").unwrap();
        assert!(!content.contains("§§"), "不应残留连续分隔符: {}", content);
        assert!(content.contains("C"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_reconcile_detects_divergence() {
        let (dir, db) = setup();
        let bm = BoundedMemory::new(&dir, &db, 2200, 1375);
        bm.write("memory", "entry_A", "high", None).unwrap();
        // 模拟 DB 丢失条目（手动从 DB 删除）
        db.conn().execute("DELETE FROM bounded_memory WHERE content='entry_A'", []).unwrap();
        let report = bm.reconcile_check("memory").unwrap();
        assert!(!report.only_in_md.is_empty(), "should detect entry only in .md");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_reconcile_fix_restores_consistency() {
        let (dir, db) = setup();
        let bm = BoundedMemory::new(&dir, &db, 2200, 1375);
        bm.write("memory", "entry_X", "high", None).unwrap();
        bm.write("memory", "entry_Y", "high", None).unwrap();
        // 模拟 .md 文件损坏（"corrupted" 是 .md 独有内容）
        std::fs::write(dir.join("MEMORY.md"), "<!-- ASUNA MEMORY -->\n\ncorrupted").unwrap();
        let count = bm.reconcile_fix("memory").unwrap();
        // 无损合并：DB 2 条 + .md 独有 "corrupted" → 总计 3 条
        assert_eq!(count, 3);
        let content = bm.read("memory").unwrap();
        assert!(content.contains("entry_X"));
        assert!(content.contains("entry_Y"));
        // "corrupted" 作为 .md 独有条目被保留（插入 DB + 写入 .md）
        assert!(content.contains("corrupted"));
        // 修复后 reconcile_check 应一致
        let report = bm.reconcile_check("memory").unwrap();
        assert!(report.only_in_md.is_empty());
        assert!(report.only_in_db.is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// .md 独有条目在 reconcile_fix 后应被保留（插入 DB + 保留在 .md）
    #[test]
    fn test_reconcile_fix_preserves_md_only() {
        let (dir, db) = setup();
        let bm = BoundedMemory::new(&dir, &db, 2200, 1375);

        // DB 写入 1 条
        bm.write("memory", "entry_C", "high", None).unwrap();

        // .md 写入 2 条独有内容（模拟手动编辑）
        let md_content = format!(
            "<!-- ASUNA MEMORY | capacity: 2200 -->\n\nentry_A\n§\nentry_B\n§\nentry_C"
        );
        std::fs::write(dir.join("MEMORY.md"), md_content).unwrap();

        // reconcile_fix 应无损合并
        let count = bm.reconcile_fix("memory").unwrap();
        assert_eq!(count, 3);

        // 验证：3 条都在 DB 中
        let db_count: i64 = db.conn().query_row(
            "SELECT COUNT(*) FROM bounded_memory WHERE target='memory'",
            [], |r| r.get(0)
        ).unwrap();
        assert_eq!(db_count, 3);

        // 验证：.md 包含所有 3 条
        let md = bm.read("memory").unwrap();
        assert!(md.contains("entry_A"));
        assert!(md.contains("entry_B"));
        assert!(md.contains("entry_C"));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// sync_atoms_to_md 驱逐后 .md 不应包含被驱逐的 atom
    #[test]
    fn test_sync_atoms_no_regression() {
        let (dir, db) = setup();
        // 容量很小（500），atom 占比 0.3 → atom budget = 150 chars
        let bm = BoundedMemory::new(&dir, &db, 500, 200)
            .with_atom_capacity_ratio(0.3);

        // 手动写入一条 manual（不会被驱逐）
        bm.write("memory", "manual_entry_kept", "high", None).unwrap();

        // 直接往 DB 插入 3 条长 atom（绕过 write 的容量检查）
        let now = crate::util::time::now_unix_ms();
        for i in 0..3 {
            let content = format!("atom_content_{}_", i) + &"x".repeat(60); // ~75 chars each
            db.conn().execute(
                "INSERT INTO bounded_memory (target, content, created_at, updated_at, confidence, memory_type)
                 VALUES ('memory', ?1, ?2, ?2, 'medium', 'atom')",
                rusqlite::params![content, now + i],
            ).unwrap();
        }

        // sync_atoms_to_md 应驱逐部分 atom（3×75=225 > budget 150）
        let evicted = bm.sync_atoms_to_md().unwrap();
        assert!(evicted > 0, "should evict at least 1 atom");

        // .md 中的条目数应与 DB 一致（无 .md 独有残留）
        let report = bm.reconcile_check("memory").unwrap();
        assert!(report.only_in_md.is_empty(), "no .md-only entries after sync, got: {:?}", report.only_in_md);
        assert!(report.only_in_db.is_empty(), "no DB-only entries after sync, got: {:?}", report.only_in_db);

        // manual 条目必须保留
        let md = bm.read("memory").unwrap();
        assert!(md.contains("manual_entry_kept"), "manual entry must survive eviction");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// write() 必须拒绝含 \n§\n 的 content，否则会产生 DB/.md 行数不一致的坏行。
    #[test]
    fn test_write_rejects_multi_entry_content() {
        let (dir, db) = setup();
        let bm = BoundedMemory::new(&dir, &db, 2200, 1375);

        let bad = format!("条目 A{}条目 B", ENTRY_SEPARATOR);
        let result = bm.write("memory", &bad, "medium", None);
        assert!(result.is_err(), "write should reject content containing \\n§\\n");
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("条目分隔符"), "error should mention separator, got: {}", msg);

        // 拒绝后 DB 与 .md 都必须是空的
        let count: i64 = db.conn().query_row(
            "SELECT COUNT(*) FROM bounded_memory WHERE target='memory'",
            [], |r| r.get(0),
        ).unwrap();
        assert_eq!(count, 0, "no row should be inserted on rejection");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// update() 必须拒绝含 \n§\n 的 new_text，否则会把一条目分裂成多条。
    #[test]
    fn test_update_rejects_multi_entry_new_text() {
        let (dir, db) = setup();
        let bm = BoundedMemory::new(&dir, &db, 2200, 1375);

        bm.write("memory", "原始条目内容", "medium", None).unwrap();

        let bad_new = format!("改后 A{}改后 B", ENTRY_SEPARATOR);
        let result = bm.update("memory", "原始", &bad_new, None);
        assert!(result.is_err(), "update should reject new_text containing \\n§\n");
        assert!(result.unwrap_err().to_string().contains("条目分隔符"));

        // DB 中原始条目必须保持不变
        let count: i64 = db.conn().query_row(
            "SELECT COUNT(*) FROM bounded_memory WHERE target='memory'",
            [], |r| r.get(0),
        ).unwrap();
        assert_eq!(count, 1);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// 端到端测试：split_multi_entry_rows 拆分坏行、保留元数据、跳过重复、重建 .md
    #[test]
    fn test_split_multi_entry_rows() {
        let (dir, db) = setup();
        let bm = BoundedMemory::new(&dir, &db, 2200, 1375).with_security_scan(false);

        // 2 个正常单条目行
        bm.write("memory", "normal_1", "high", Some("s1")).unwrap();
        bm.write("memory", "normal_2", "medium", Some("s2")).unwrap();

        // 1 个坏行：含 3 个子条目（绕过 write 校验，模拟历史数据）
        let bad_content = format!("sub_A{}sub_B{}sub_C", ENTRY_SEPARATOR, ENTRY_SEPARATOR);
        db.conn().execute(
            "INSERT INTO bounded_memory (target, content, created_at, updated_at, source_session, confidence, memory_type)
             VALUES ('memory', ?1, 1000, 1000, 's3', 'high', 'atom')",
            rusqlite::params![bad_content],
        ).unwrap();

        // 1 个子条目已存在于 DB（应被跳过）
        bm.write("memory", "sub_B", "low", Some("sX")).unwrap();

        // 拆分前 reconcile_check 应看到差异
        let before = bm.reconcile_check("memory").unwrap();
        assert!(
            !before.only_in_md.is_empty() || !before.only_in_db.is_empty(),
            "pre-split reconcile should show divergence: md={}, db={}",
            before.md_entry_count, before.db_entry_count
        );

        let report = bm.split_multi_entry_rows("memory").unwrap();
        assert_eq!(report.bad_rows, 1);
        // sub_A, sub_C 应被新增；sub_B 已存在应被跳过
        assert_eq!(report.sub_entries_created, 2, "expected 2 created (sub_A, sub_C)");
        assert_eq!(report.duplicates_skipped, 1, "sub_B should be skipped as duplicate");

        // 拆分后：2 个原始正常行 + 1 个已存在的 sub_B + 2 个新增 = 5 行
        let count: i64 = db.conn().query_row(
            "SELECT COUNT(*) FROM bounded_memory WHERE target='memory'",
            [], |r| r.get(0),
        ).unwrap();
        assert_eq!(count, 5, "expected 5 rows after split");

        // 不应再有任何坏行
        let bad_after: i64 = db.conn().query_row(
            "SELECT COUNT(*) FROM bounded_memory WHERE target='memory' AND content LIKE ?1 ESCAPE '\\'",
            rusqlite::params![format!("%{}%", ENTRY_SEPARATOR)],
            |r| r.get(0),
        ).unwrap();
        assert_eq!(bad_after, 0, "no multi-entry rows should remain");

        // 拆分后的子条目必须保留原坏行的元数据
        let (conf, mem_type): (String, String) = db.conn().query_row(
            "SELECT confidence, memory_type FROM bounded_memory WHERE content='sub_A'",
            [], |r| Ok((r.get(0)?, r.get(1)?)),
        ).unwrap();
        assert_eq!(conf, "high", "sub_A must inherit 'high' confidence from bad row");
        assert_eq!(mem_type, "atom", "sub_A must inherit 'atom' memory_type from bad row");

        // reconcile_check 必须一致
        let after = bm.reconcile_check("memory").unwrap();
        assert!(after.only_in_md.is_empty(), "no .md-only entries after split: {:?}", after.only_in_md);
        assert!(after.only_in_db.is_empty(), "no DB-only entries after split: {:?}", after.only_in_db);
        assert_eq!(after.md_entry_count, after.db_entry_count);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// 幂等性：无坏行时 split 必须返回全零报告，DB 与 .md 不被改动。
    #[test]
    fn test_split_multi_entry_rows_noop_when_clean() {
        let (dir, db) = setup();
        let bm = BoundedMemory::new(&dir, &db, 2200, 1375);

        bm.write("memory", "clean_1", "high", None).unwrap();
        bm.write("memory", "clean_2", "medium", None).unwrap();

        let report = bm.split_multi_entry_rows("memory").unwrap();
        assert_eq!(report.bad_rows, 0);
        assert_eq!(report.sub_entries_created, 0);
        assert_eq!(report.duplicates_skipped, 0);

        let count: i64 = db.conn().query_row(
            "SELECT COUNT(*) FROM bounded_memory WHERE target='memory'",
            [], |r| r.get(0),
        ).unwrap();
        assert_eq!(count, 2, "clean DB must be untouched");

        std::fs::remove_dir_all(&dir).unwrap();
    }
}

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
        }
    }

    /// 可选关闭安全扫描（用于受信任的内部调用 / 配置覆盖）
    pub fn with_security_scan(mut self, enabled: bool) -> Self {
        self.security_scan = enabled;
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

        let mut results = Vec::new();
        for row in rows {
            let mut info = row?;
            if let Some(ref sid) = info.source_session {
                if let Ok(path) = self.db.conn().query_row(
                    "SELECT file_path FROM sessions WHERE session_id = ?1",
                    rusqlite::params![sid],
                    |r| r.get::<_, String>(0),
                ) {
                    info.session_exists = true;
                    info.session_file_path = Some(path);
                }
            }
            results.push(info);
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

    /// 以 SQLite 为准重写 .md 文件（修复 DB/文件不一致）。
    /// 若重建内容超过容量上限，写入会附带警告但仍执行（6.2 fix）。
    pub fn reconcile_fix(&self, target: &str) -> anyhow::Result<usize> {
        let mut stmt = self.db.conn().prepare(
            "SELECT content FROM bounded_memory WHERE target = ?1 ORDER BY created_at"
        )?;
        let db_entries: Vec<String> = stmt.query_map(
            rusqlite::params![target], |row| row.get::<_, String>(0)
        )?.filter_map(|r| r.ok()).collect();

        let capacity = self.capacity(target);
        let new_body = db_entries.join(ENTRY_SEPARATOR);
        let body_chars = new_body.chars().count();
        if body_chars > capacity {
            tracing::warn!(
                "reconcile_fix: DB 条目总长 {} 超出容量上限 {}，后续 write 可能被拒绝",
                body_chars,
                capacity
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
        Ok(db_entries.len())
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
    lines[start..].join("\n").trim().to_string()
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
        // 模拟 .md 文件损坏
        std::fs::write(dir.join("MEMORY.md"), "<!-- ASUNA MEMORY -->\n\ncorrupted").unwrap();
        let count = bm.reconcile_fix("memory").unwrap();
        assert_eq!(count, 2);
        let content = bm.read("memory").unwrap();
        assert!(content.contains("entry_X"));
        assert!(content.contains("entry_Y"));
        assert!(!content.contains("corrupted"));
        // 修复后 reconcile_check 应一致
        let report = bm.reconcile_check("memory").unwrap();
        assert!(report.only_in_md.is_empty());
        assert!(report.only_in_db.is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

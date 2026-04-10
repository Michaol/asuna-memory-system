use std::path::{Path, PathBuf};
use crate::index::db::Db;
use crate::util::time;

const ENTRY_SEPARATOR: &str = "\n§\n";

/// 有界记忆管理器
pub struct BoundedMemory<'a> {
    memory_dir: PathBuf,
    db: &'a Db,
    memory_limit: usize,
    user_limit: usize,
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
        Self { memory_dir: memory_dir.to_path_buf(), db, memory_limit, user_limit }
    }

    fn target_file(&self, target: &str) -> PathBuf {
        match target {
            "memory" => self.memory_dir.join("MEMORY.md"),
            "user" => self.memory_dir.join("USER.md"),
            _ => self.memory_dir.join(format!("{}.md", target)),
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

    /// 读取全文
    pub fn read(&self, target: &str) -> anyhow::Result<String> {
        let path = self.target_file(target);
        if path.exists() {
            Ok(std::fs::read_to_string(path)?)
        } else {
            Ok(String::new())
        }
    }

    /// 写入新条目（追加）
    pub fn write(&self, target: &str, content: &str, confidence: &str, session_id: Option<&str>) -> anyhow::Result<()> {
        // 安全扫描
        let scan_result = crate::growth::security::scan_content(content);
        if !scan_result.is_safe() {
            anyhow::bail!("安全扫描未通过: {}", scan_result.reason());
        }

        let path = self.target_file(target);
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

        let header = self.metadata_header(target, capacity);
        let full = format!("{}\n\n{}", header, new_body);
        std::fs::create_dir_all(&self.memory_dir)?;
        std::fs::write(&path, full)?;

        // 审计日志
        crate::growth::audit::log_action(
            self.db,
            "write",
            target,
            &serde_json::json!({"content_preview": &content[..content.len().min(100)]}).to_string(),
            session_id,
        )?;

        // SQLite bounded_memory 表
        let now = time::now_unix_ms();
        self.db.conn().execute(
            "INSERT INTO bounded_memory (target, content, created_at, updated_at, source_session, confidence)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![target, content, now, now, session_id, confidence],
        )?;

        Ok(())
    }

    /// 子串替换更新
    pub fn update(&self, target: &str, old_text: &str, new_text: &str, session_id: Option<&str>) -> anyhow::Result<()> {
        let scan_result = crate::growth::security::scan_content(new_text);
        if !scan_result.is_safe() {
            anyhow::bail!("安全扫描未通过: {}", scan_result.reason());
        }

        let path = self.target_file(target);
        let capacity = self.capacity(target);
        let current = self.read(target)?;

        if !current.contains(old_text) {
            anyhow::bail!("未找到要替换的文本");
        }

        let updated = current.replace(old_text, new_text);
        let updated_body = extract_body(&updated);
        let updated_char_count = updated_body.chars().count();
        if updated_char_count > capacity {
            anyhow::bail!("替换后超出容量上限: {}/{} 字符", updated_char_count, capacity);
        }

        std::fs::write(&path, &updated)?;

        crate::growth::audit::log_action(
            self.db,
            "update",
            target,
            &serde_json::json!({
                "old": &old_text[..old_text.len().min(50)],
                "new": &new_text[..new_text.len().min(50)]
            }).to_string(),
            session_id,
        )?;

        Ok(())
    }

    /// 删除匹配条目
    pub fn remove(&self, target: &str, old_text: &str, session_id: Option<&str>) -> anyhow::Result<()> {
        let path = self.target_file(target);
        let current = self.read(target)?;

        if !current.contains(old_text) {
            anyhow::bail!("未找到要删除的文本");
        }

        // 移除条目 + 清理分隔符
        let updated = current.replace(old_text, "");
        // 清理连续分隔符
        let updated = updated.replace("\n§\n§\n", "\n§\n");
        let updated = updated.trim_end_matches("\n§\n").to_string();

        std::fs::write(&path, &updated)?;

        crate::growth::audit::log_action(
            self.db,
            "remove",
            target,
            &serde_json::json!({"removed": &old_text[..old_text.len().min(50)]}).to_string(),
            session_id,
        )?;

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
            // 验证源会话是否存在
            if let Some(ref sid) = info.source_session {
                info.session_exists = self.db.conn().query_row(
                    "SELECT file_path FROM sessions WHERE session_id = ?1",
                    rusqlite::params![sid],
                    |r| r.get::<_, String>(0),
                ).ok().is_some();
                if info.session_exists {
                    info.session_file_path = self.db.conn().query_row(
                        "SELECT file_path FROM sessions WHERE session_id = ?1",
                        rusqlite::params![sid],
                        |r| r.get(0),
                    ).ok();
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

/// 从完整文件内容中提取条目正文（去掉元数据头）
fn extract_body(content: &str) -> String {
    if content.is_empty() {
        return String::new();
    }
    // 跳过元数据头行
    let lines: Vec<&str> = content.lines().collect();
    let start = if lines.first().map_or(false, |l| l.contains("<!-- ASUNA")) {
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

        // 先写入一个接近上限的条目
        let long_content = "a".repeat(90);
        bm.write("memory", &long_content, "medium", None).unwrap();

        // 再写入应该失败
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
}

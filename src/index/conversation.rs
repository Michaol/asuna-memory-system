//! JSONL 会话文件的读写（SessionHeader / Turn 的序列化与目录枚举）。
//!
//! 归属理由（J37）：JSONL 会话文件是记忆系统的**存储格式**，属存储层
//! （`index` = 存储/索引层，SQLite 侧的同层兄弟是 `db`/`schema`），而不是
//! 事实层的业务逻辑；`rebuild.rs` 按文件内容重建 DB 索引是其主要读者。
//! 从 `fact/` 移到此处，消除了原先 `rebuild → 本模块` 造成的 fact↔index
//! 双向依赖。`fact/mod.rs` 保留了兼容再导出，crate 内
//! `fact::conversation::…` 路径继续可用。

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// 会话头（JSONL 第一行）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionHeader {
    pub v: u32,
    #[serde(rename = "type")]
    pub header_type: String,
    pub session_id: String,
    pub start_time: String,
    pub profile_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
}

/// 对话轮次
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Turn {
    pub ts: String,
    pub seq: u32,
    pub role: String,
    pub content: String,
    /// 任意扩展字段 (model, usage, tool_name, arguments 等)
    #[serde(flatten)]
    pub metadata: Option<serde_json::Value>,
}

/// 计算会话 JSONL 文件的目标路径（不创建目录、不写盘）
///
/// 命名契约（U5）：`<dir>/<YYYY>/<MM>/<DD>/<YYYYMMDD>T<HHMMSS>_<hash8>.jsonl`，
/// `<hash8>` = sha256(session_id) 的前 8 位 hex。**没有任何代码从文件名反解析
/// session_id 或时间戳**——rebuild 按文件内容第一行的 header 识别会话，
/// `cleanup_old_jsonl` 按 DB `sessions.file_path` 定位旧文件。因此文件名只要求
/// 唯一性：同一 session_id 同一秒 → 同一路径（重存覆盖，语义本就如此）；
/// 不同 session_id 即使共享前缀（如 "session-1"/"session-2"）也几乎必然不同名。
///
/// 跨版本升级缝隙（U5 改名 × J33b file_path 改写）：v2.6.2 及之前经 REST
/// /capture 写出的会话，磁盘上是**旧命名**文件而 DB `file_path` 是
/// `gateway://` 伪 URI——改名后任何写路径的清理都定位不到旧文件，同一
/// session_id 会在磁盘上永久并存两个文件（rebuild 会把两份 turns 混插，
/// 增量检测恒判不一致）。`SessionStore` 的 Append/Overwrite 在写盘后按
/// `compute_legacy_session_path`（crate 内部）定位并迁移/清理该旧文件，封死此缝隙。
pub fn compute_session_path(
    conversations_dir: &Path,
    header: &SessionHeader,
) -> anyhow::Result<PathBuf> {
    let (dir, compact_time) = session_path_parts(conversations_dir, &header.start_time)?;
    use sha2::{Digest, Sha256};
    // U5: was `session_id.chars().take(8)` — two ids sharing an 8-char prefix
    // written in the same second overwrote each other's JSONL.
    let mut hasher = Sha256::new();
    hasher.update(header.session_id.as_bytes());
    let digest = hasher.finalize();
    let short_id: String = digest
        .iter()
        .take(4)
        .map(|b| format!("{:02x}", b))
        .collect();
    let filename = format!("{}_{}.jsonl", compact_time, short_id);
    Ok(dir.join(&filename))
}

/// U5 改名**之前**的命名规则（文件名后缀 = `session_id` 前 8 字符）计算路径。
///
/// 仅供 `SessionStore` 升级清理使用（pub(crate)：模块 doc 已声明除此之外任何
/// 代码都不得依赖旧命名规则）：旧文件与新路径共享同一 start_time 串
/// （v2.6.2 与 HEAD 的 /capture 都以 `unix_ms_to_iso(DB start_ts)` 命名），
/// 故同一 header 串下只差后缀，可从 header 精确复原。
pub(crate) fn compute_legacy_session_path(
    conversations_dir: &Path,
    session_id: &str,
    start_time: &str,
) -> anyhow::Result<PathBuf> {
    let (dir, compact_time) = session_path_parts(conversations_dir, start_time)?;
    let short_id: String = session_id.chars().take(8).collect();
    Ok(dir.join(format!("{}_{}.jsonl", compact_time, short_id)))
}

/// 两个命名规则共用的部分：解析 start_time → (日期目录, 紧凑时间串)。
fn session_path_parts(
    conversations_dir: &Path,
    start_time: &str,
) -> anyhow::Result<(PathBuf, String)> {
    let start_dt = chrono::DateTime::parse_from_rfc3339(start_time).or_else(|_| {
        let naive = chrono::NaiveDateTime::parse_from_str(start_time, "%Y-%m-%dT%H:%M:%S%.f")?;
        Ok::<_, anyhow::Error>(naive.and_utc().fixed_offset())
    })?;

    let dir = conversations_dir
        .join(start_dt.format("%Y").to_string())
        .join(start_dt.format("%m").to_string())
        .join(start_dt.format("%d").to_string());
    let compact_time = start_dt.format("%Y%m%dT%H%M%S").to_string();
    Ok((dir, compact_time))
}

/// 把 (header, turns) 序列化为 JSONL 写到指定路径（创建父目录）
pub fn write_session_at(
    file_path: &Path,
    header: &SessionHeader,
    turns: &[Turn],
) -> anyhow::Result<()> {
    if let Some(parent) = file_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut content = serde_json::to_string(header)?;
    content.push('\n');
    for turn in turns {
        content.push_str(&serde_json::to_string(turn)?);
        content.push('\n');
    }
    std::fs::write(file_path, content)?;
    Ok(())
}

/// 写入一个会话为 JSONL 文件，返回文件路径（仅测试使用）
#[cfg(test)]
pub fn write_session(
    conversations_dir: &Path,
    header: &SessionHeader,
    turns: &[Turn],
) -> anyhow::Result<PathBuf> {
    let file_path = compute_session_path(conversations_dir, header)?;
    write_session_at(&file_path, header, turns)?;
    Ok(file_path)
}

/// 读取 JSONL 文件，返回 (SessionHeader, Vec<Turn>)
pub fn read_session(path: &Path) -> anyhow::Result<(SessionHeader, Vec<Turn>)> {
    let content = std::fs::read_to_string(path)?;
    let mut lines = content.lines();

    let header_line = lines
        .next()
        .ok_or_else(|| anyhow::anyhow!("JSONL 文件为空"))?;
    let header: SessionHeader = serde_json::from_str(header_line)?;

    let mut turns = Vec::new();
    for line in lines {
        if line.trim().is_empty() {
            continue;
        }
        let turn: Turn = serde_json::from_str(line)?;
        turns.push(turn);
    }

    Ok((header, turns))
}

/// 列出所有 JSONL 会话文件（传入 conversations 目录）
pub fn list_sessions(conversations_dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    collect_jsonl_files(conversations_dir, &mut files);
    files.sort();
    files
}

fn collect_jsonl_files(dir: &Path, out: &mut Vec<PathBuf>) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect_jsonl_files(&path, out);
            } else if path.extension().is_some_and(|e| e == "jsonl") {
                out.push(path);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn make_test_header() -> SessionHeader {
        SessionHeader {
            v: 1,
            header_type: "session_header".to_string(),
            session_id: "test-session-abc123".to_string(),
            start_time: "2026-04-10T10:02:00.000+08:00".to_string(),
            profile_id: "default".to_string(),
            source: Some("test".to_string()),
            agent_model: Some("test-model".to_string()),
            title: None,
            tags: vec![],
        }
    }

    fn make_test_turns() -> Vec<Turn> {
        vec![
            Turn {
                ts: "2026-04-10T10:02:05.123+08:00".to_string(),
                seq: 1,
                role: "user".to_string(),
                content: "你好".to_string(),
                metadata: None,
            },
            Turn {
                ts: "2026-04-10T10:02:07.456+08:00".to_string(),
                seq: 2,
                role: "assistant".to_string(),
                content: "你好！有什么可以帮助你的？".to_string(),
                metadata: Some(serde_json::json!({
                    "model": "test-model",
                    "usage": {"input_tokens": 10, "output_tokens": 20}
                })),
            },
        ]
    }

    #[test]
    fn test_write_and_read_session() {
        let tmp = std::env::temp_dir().join(format!(
            "asuna_test_{}",
            Utc::now().timestamp_nanos_opt().unwrap_or(0)
        ));
        std::fs::create_dir_all(&tmp).unwrap();

        let header = make_test_header();
        let turns = make_test_turns();

        let path = write_session(&tmp, &header, &turns).unwrap();
        assert!(path.exists());

        let (read_header, read_turns) = read_session(&path).unwrap();
        assert_eq!(read_header.session_id, header.session_id);
        assert_eq!(read_header.start_time, header.start_time);
        assert_eq!(read_turns.len(), 2);
        assert_eq!(read_turns[0].content, "你好");
        assert_eq!(read_turns[1].role, "assistant");
        assert!(read_turns[1].metadata.is_some());

        // 清理
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn test_directory_structure() {
        let tmp = std::env::temp_dir().join(format!(
            "asuna_test_dir_{}",
            Utc::now().timestamp_nanos_opt().unwrap_or(0)
        ));
        std::fs::create_dir_all(&tmp).unwrap();

        let header = make_test_header();
        let turns = make_test_turns();

        let path = write_session(&tmp, &header, &turns).unwrap();

        // 验证年/月/日目录
        assert!(path.to_string_lossy().contains("2026"));
        assert!(path.to_string_lossy().contains("04"));
        assert!(path.to_string_lossy().contains("10"));

        // 验证文件名格式
        let filename = path.file_name().unwrap().to_string_lossy();
        assert!(filename.starts_with("20260410T100200_"));
        assert!(filename.ends_with(".jsonl"));

        std::fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn test_list_sessions() {
        let tmp = std::env::temp_dir().join(format!(
            "asuna_test_list_{}",
            Utc::now().timestamp_nanos_opt().unwrap_or(0)
        ));
        std::fs::create_dir_all(&tmp).unwrap();

        let header = make_test_header();
        let turns = make_test_turns();

        write_session(&tmp, &header, &turns).unwrap();

        let sessions = list_sessions(&tmp);
        assert_eq!(sessions.len(), 1);

        std::fs::remove_dir_all(&tmp).unwrap();
    }

    /// U5 回归：文件名不再用 session_id 前 8 字符（"session-1"/"session-2"
    /// 共享前 8 字符 + 同秒 → 同路径互相覆盖），改用 sha256 前 8 位 hex。
    #[test]
    fn test_compute_session_path_disambiguates_shared_prefix() {
        let tmp = std::env::temp_dir().join(format!(
            "asuna_test_prefix_collision_{}",
            Utc::now().timestamp_nanos_opt().unwrap_or(0)
        ));

        let mut h1 = make_test_header();
        h1.session_id = "prefix-aaa-1".to_string();
        let mut h2 = make_test_header();
        h2.session_id = "prefix-aaa-2".to_string();
        // 两者共享前 8 字符 "prefix-a"，旧实现同名互相覆盖

        let p1 = compute_session_path(&tmp, &h1).unwrap();
        let p2 = compute_session_path(&tmp, &h2).unwrap();
        assert_ne!(p1, p2, "不同 session_id 同秒不得撞同一文件名");
        assert_eq!(p1.parent(), p2.parent(), "日期目录仍相同");
        assert_ne!(
            p1.file_name(),
            p2.file_name(),
            "旧前 8 字符命名会给出相同文件名"
        );

        std::fs::remove_dir_all(&tmp).ok();
    }

    /// 覆盖语义保留：同一 session_id 同一秒 → 同一路径（重存覆盖本会话文件）。
    #[test]
    fn test_compute_session_path_stable_for_same_id_and_time() {
        let tmp = std::env::temp_dir().join(format!(
            "asuna_test_stable_{}",
            Utc::now().timestamp_nanos_opt().unwrap_or(0)
        ));
        let h1 = make_test_header();
        let mut h2 = make_test_header();
        h2.title = Some("另一个标题".to_string()); // 无关字段变化不得影响路径
        assert_eq!(
            compute_session_path(&tmp, &h1).unwrap(),
            compute_session_path(&tmp, &h2).unwrap()
        );
        std::fs::remove_dir_all(&tmp).ok();
    }

    /// U5 升级缝隙回归：旧命名路径必须逐位复原 v2.6.2 `compute_session_path`
    /// 的产物（日期目录与紧凑时间串相同，后缀为 session_id 前 8 字符），
    /// SessionStore 的迁移/清理据此定位旧文件。
    #[test]
    fn test_compute_legacy_session_path_reproduces_pre_u5_naming() {
        let tmp = std::env::temp_dir().join(format!(
            "asuna_test_legacy_{}",
            Utc::now().timestamp_nanos_opt().unwrap_or(0)
        ));
        let h = make_test_header(); // session_id "test-session-abc123" → 前8字符 "test-ses"

        let legacy = compute_legacy_session_path(&tmp, &h.session_id, &h.start_time).unwrap();
        let legacy_lossy = legacy.to_string_lossy().replace('\\', "/");
        assert!(
            legacy_lossy.ends_with("/2026/04/10/20260410T100200_test-ses.jsonl"),
            "旧命名复原失败: {legacy_lossy}"
        );
        // 与新命名同目录、不同后缀（这正是升级后旧文件逃过清理的机制）
        let new = compute_session_path(&tmp, &h).unwrap();
        assert_eq!(legacy.parent(), new.parent());
        assert_ne!(legacy.file_name(), new.file_name());

        std::fs::remove_dir_all(&tmp).ok();
    }
}

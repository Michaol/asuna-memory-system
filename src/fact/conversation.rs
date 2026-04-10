use std::path::{Path, PathBuf};
use serde::{Deserialize, Serialize};
use chrono::Utc;

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

/// 写入一个会话为 JSONL 文件，返回文件路径
/// conversations_dir: 对话归档根目录（如 profiles/default/conversations）
pub fn write_session(
    conversations_dir: &Path,
    header: &SessionHeader,
    turns: &[Turn],
) -> anyhow::Result<PathBuf> {
    // 解析 start_time 生成目录结构: 年/月/日
    let start_dt = chrono::DateTime::parse_from_rfc3339(&header.start_time)
        .or_else(|_| {
            // 尝试无时区
            let naive = chrono::NaiveDateTime::parse_from_str(&header.start_time, "%Y-%m-%dT%H:%M:%S%.f")?;
            Ok::<_, anyhow::Error>(naive.and_utc().fixed_offset())
        })?;

    let dir = conversations_dir
        .join(start_dt.format("%Y").to_string())
        .join(start_dt.format("%m").to_string())
        .join(start_dt.format("%d").to_string());

    std::fs::create_dir_all(&dir)?;

    // 文件名: {ISO日期时间}_{session_id}.jsonl
    // 使用紧凑格式
    let compact_time = start_dt.format("%Y%m%dT%H%M%S");
    let short_id: String = header.session_id.chars().take(8).collect();
    let filename = format!("{}_{}.jsonl", compact_time, short_id);
    let file_path = dir.join(&filename);

    // 写入 JSONL
    let mut content = serde_json::to_string(header)?;
    content.push('\n');
    for turn in turns {
        content.push_str(&serde_json::to_string(turn)?);
        content.push('\n');
    }

    std::fs::write(&file_path, content)?;

    // 返回相对于 data_dir 的路径
    Ok(file_path)
}

/// 读取 JSONL 文件，返回 (SessionHeader, Vec<Turn>)
pub fn read_session(path: &Path) -> anyhow::Result<(SessionHeader, Vec<Turn>)> {
    let content = std::fs::read_to_string(path)?;
    let mut lines = content.lines();

    let header_line = lines.next().ok_or_else(|| anyhow::anyhow!("JSONL 文件为空"))?;
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
            } else if path.extension().map_or(false, |e| e == "jsonl") {
                out.push(path);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let tmp = std::env::temp_dir().join(format!("asuna_test_{}", Utc::now().timestamp_nanos_opt().unwrap_or(0)));
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
        let tmp = std::env::temp_dir().join(format!("asuna_test_dir_{}", Utc::now().timestamp_nanos_opt().unwrap_or(0)));
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
        let tmp = std::env::temp_dir().join(format!("asuna_test_list_{}", Utc::now().timestamp_nanos_opt().unwrap_or(0)));
        std::fs::create_dir_all(&tmp).unwrap();

        let header = make_test_header();
        let turns = make_test_turns();

        write_session(&tmp, &header, &turns).unwrap();

        let sessions = list_sessions(&tmp);
        assert_eq!(sessions.len(), 1);

        std::fs::remove_dir_all(&tmp).unwrap();
    }
}

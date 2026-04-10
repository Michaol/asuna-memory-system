use serde_json::{json, Value};
use std::sync::Arc;

use crate::config::Config;
use crate::index::db::Db;
use crate::fact::conversation::{SessionHeader, Turn};
use crate::fact::session_store::SessionStore;
use crate::growth::bounded_memory::BoundedMemory;

/// MCP 工具定义
pub fn tool_definitions() -> Vec<Value> {
    vec![
        json!({
            "name": "save_session",
            "description": "保存完整对话到记忆系统事实层。每轮对话必须包含 ISO 8601 时间戳。",
            "inputSchema": {
                "type": "object",
                "required": ["session_id", "turns"],
                "properties": {
                    "session_id": {
                        "type": "string",
                        "description": "会话唯一标识"
                    },
                    "turns": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "required": ["timestamp", "role", "content"],
                            "properties": {
                                "timestamp": { "type": "string", "description": "ISO 8601 时间戳" },
                                "role": { "type": "string", "enum": ["user", "assistant", "tool_call", "system"] },
                                "content": { "type": "string" },
                                "metadata": { "type": "object" }
                            }
                        }
                    },
                    "source": { "type": "string" },
                    "title": { "type": "string" },
                    "tags": { "type": "array", "items": { "type": "string" } }
                }
            }
        }),
        json!({
            "name": "search_sessions",
            "description": "多维度检索历史对话。支持语义搜索、关键词搜索和时间范围过滤。",
            "inputSchema": {
                "type": "object",
                "required": ["query"],
                "properties": {
                    "query": { "type": "string" },
                    "time_range": {
                        "type": "object",
                        "properties": {
                            "after": { "type": "string" },
                            "before": { "type": "string" },
                            "last_days": { "type": "integer" }
                        }
                    },
                    "role": { "type": "string" },
                    "top_k": { "type": "integer", "default": 5 },
                    "search_mode": { "type": "string", "enum": ["semantic", "keyword", "hybrid"], "default": "hybrid" }
                }
            }
        }),
        json!({
            "name": "memory_write",
            "description": "向有界记忆写入新条目。",
            "inputSchema": {
                "type": "object",
                "required": ["target", "content"],
                "properties": {
                    "target": { "type": "string", "enum": ["memory", "user"] },
                    "content": { "type": "string" },
                    "confidence": { "type": "string", "enum": ["high", "medium", "low"], "default": "medium" },
                    "session_id": { "type": "string", "description": "源会话 ID（用于溯源）" }
                }
            }
        }),
        json!({
            "name": "memory_update",
            "description": "通过子串匹配更新已有记忆条目",
            "inputSchema": {
                "type": "object",
                "required": ["target", "old_text", "new_text"],
                "properties": {
                    "target": { "type": "string", "enum": ["memory", "user"] },
                    "old_text": { "type": "string" },
                    "new_text": { "type": "string" }
                }
            }
        }),
        json!({
            "name": "memory_remove",
            "description": "删除记忆条目",
            "inputSchema": {
                "type": "object",
                "required": ["target", "old_text"],
                "properties": {
                    "target": { "type": "string", "enum": ["memory", "user"] },
                    "old_text": { "type": "string" }
                }
            }
        }),
        json!({
            "name": "memory_read",
            "description": "读取当前有界记忆全文",
            "inputSchema": {
                "type": "object",
                "required": ["target"],
                "properties": {
                    "target": { "type": "string", "enum": ["memory", "user"] }
                }
            }
        }),
        json!({
            "name": "user_profile",
            "description": "读写用户画像",
            "inputSchema": {
                "type": "object",
                "required": ["action"],
                "properties": {
                    "action": { "type": "string", "enum": ["read", "write", "update", "remove"] },
                    "content": { "type": "string" },
                    "old_text": { "type": "string" },
                    "new_text": { "type": "string" },
                    "confidence": { "type": "string", "enum": ["high", "medium", "low"], "default": "medium" }
                }
            }
        }),
        json!({
            "name": "rebuild_index",
            "description": "从 JSONL 文件重建 SQLite 索引",
            "inputSchema": {
                "type": "object",
                "properties": {}
            }
        }),
        json!({
            "name": "memory_provenance",
            "description": "验证成长层记忆的溯源信息，检查记忆条目是否可追溯到原始对话。",
            "inputSchema": {
                "type": "object",
                "required": ["target"],
                "properties": {
                    "target": { "type": "string", "enum": ["memory", "user"] }
                }
            }
        }),
    ]
}

/// 工具调用处理器
pub struct ToolHandler {
    config: Config,
    db: Arc<Db>,
}

impl ToolHandler {
    pub fn new(config: Config, db: Arc<Db>) -> Self {
        Self { config, db }
    }

    /// 路由工具调用
    pub fn call(&self, name: &str, args: &Value) -> Result<Value, String> {
        match name {
            "save_session" => self.save_session(args),
            "search_sessions" => self.search_sessions(args),
            "memory_write" => self.memory_write(args),
            "memory_update" => self.memory_update(args),
            "memory_remove" => self.memory_remove(args),
            "memory_read" => self.memory_read(args),
            "user_profile" => self.user_profile(args),
            "memory_provenance" => self.memory_provenance(args),
            "rebuild_index" => self.rebuild_index(),
            _ => Err(format!("未知工具: {}", name)),
        }
    }

    fn save_session(&self, args: &Value) -> Result<Value, String> {
        let session_id = args["session_id"].as_str().ok_or("缺少 session_id")?;
        let turns_arr = args["turns"].as_array().ok_or("缺少 turns")?;
        let source = args["source"].as_str().map(|s| s.to_string());
        let title = args["title"].as_str().map(|s| s.to_string());
        let tags: Vec<String> = args["tags"]
            .as_array()
            .map(|arr| arr.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect())
            .unwrap_or_default();

        // 支持可选的 profile 覆盖
        let profile_id = args["profile"].as_str()
            .unwrap_or(&self.config.profile_id)
            .to_string();

        // 解析 header
        let first_turn_ts = turns_arr.first()
            .and_then(|t| t["timestamp"].as_str())
            .unwrap_or("");

        let header = SessionHeader {
            v: 1,
            header_type: "session_header".to_string(),
            session_id: session_id.to_string(),
            start_time: first_turn_ts.to_string(),
            profile_id,
            source,
            agent_model: None,
            title,
            tags,
        };

        // 解析 turns
        let mut turns = Vec::new();
        for (i, t) in turns_arr.iter().enumerate() {
            let ts = t["timestamp"].as_str().unwrap_or("").to_string();
            let role = t["role"].as_str().unwrap_or("user").to_string();
            let content = t["content"].as_str().unwrap_or("").to_string();
            let metadata = if t.get("metadata").is_some() {
                Some(t["metadata"].clone())
            } else {
                None
            };
            turns.push(Turn {
                ts,
                seq: (i + 1) as u32,
                role,
                content,
                metadata,
            });
        }

        let conv_dir = self.config.conversations_dir();
        let store = SessionStore::new(&conv_dir, &self.db);
        let stats = store.save(&header, &turns)
            .map_err(|e| format!("保存失败: {}", e))?;

        Ok(json!({
            "status": "ok",
            "session_id": stats.session_id,
            "file_path": stats.file_path.to_string_lossy(),
            "turns_saved": stats.turns_saved
        }))
    }

    fn search_sessions(&self, args: &Value) -> Result<Value, String> {
        let query = args["query"].as_str().ok_or("缺少 query")?;
        let top_k = args["top_k"].as_u64().unwrap_or(5) as usize;
        let search_mode = args["search_mode"].as_str().unwrap_or("hybrid");

        let mode = match search_mode {
            "semantic" => crate::fact::search::SearchMode::Semantic,
            "keyword" => crate::fact::search::SearchMode::Keyword,
            _ => crate::fact::search::SearchMode::Hybrid,
        };

        let after_ms = args["time_range"]["after"].as_str()
            .map(|s| crate::util::time::ts_to_unix_ms(s).unwrap_or(0));
        let before_ms = args["time_range"]["before"].as_str()
            .map(|s| crate::util::time::ts_to_unix_ms(s).unwrap_or(i64::MAX));
        let last_days = args["time_range"]["last_days"].as_i64();
        let effective_after = if let Some(days) = last_days {
            Some(crate::util::time::now_unix_ms() - days * 86400000)
        } else {
            after_ms
        };

        let role = args["role"].as_str().map(|s| s.to_string());

        let params = crate::fact::search::SearchParams {
            query: query.to_string(),
            search_mode: mode,
            top_k,
            after_ms: effective_after,
            before_ms,
            role,
        };

        let results = crate::fact::search::search_sessions(
            &self.db,
            None, // Phase 2: embedder 在 Phase 2b 接入
            &params,
        ).map_err(|e| e.to_string())?;

        Ok(json!({
            "status": "ok",
            "count": results.len(),
            "results": results
        }))
    }

    fn memory_write(&self, args: &Value) -> Result<Value, String> {
        let target = args["target"].as_str().ok_or("缺少 target")?;
        let content = args["content"].as_str().ok_or("缺少 content")?;
        let confidence = args["confidence"].as_str().unwrap_or("medium");
        let session_id = args["session_id"].as_str();

        let bm = BoundedMemory::new(
            &self.config.memory_dir(),
            &self.db,
            self.config.memory.memory_char_limit,
            self.config.memory.user_char_limit,
        );
        bm.write(target, content, confidence, session_id)
            .map_err(|e| e.to_string())?;

        Ok(json!({"status": "ok", "target": target}))
    }

    fn memory_update(&self, args: &Value) -> Result<Value, String> {
        let target = args["target"].as_str().ok_or("缺少 target")?;
        let old_text = args["old_text"].as_str().ok_or("缺少 old_text")?;
        let new_text = args["new_text"].as_str().ok_or("缺少 new_text")?;

        let bm = BoundedMemory::new(
            &self.config.memory_dir(),
            &self.db,
            self.config.memory.memory_char_limit,
            self.config.memory.user_char_limit,
        );
        bm.update(target, old_text, new_text, None)
            .map_err(|e| e.to_string())?;

        Ok(json!({"status": "ok"}))
    }

    fn memory_remove(&self, args: &Value) -> Result<Value, String> {
        let target = args["target"].as_str().ok_or("缺少 target")?;
        let old_text = args["old_text"].as_str().ok_or("缺少 old_text")?;

        let bm = BoundedMemory::new(
            &self.config.memory_dir(),
            &self.db,
            self.config.memory.memory_char_limit,
            self.config.memory.user_char_limit,
        );
        bm.remove(target, old_text, None)
            .map_err(|e| e.to_string())?;

        Ok(json!({"status": "ok"}))
    }

    fn memory_read(&self, args: &Value) -> Result<Value, String> {
        let target = args["target"].as_str().ok_or("缺少 target")?;

        let bm = BoundedMemory::new(
            &self.config.memory_dir(),
            &self.db,
            self.config.memory.memory_char_limit,
            self.config.memory.user_char_limit,
        );
        let content = bm.read(target).map_err(|e| e.to_string())?;

        Ok(json!({"target": target, "content": content}))
    }

    fn user_profile(&self, args: &Value) -> Result<Value, String> {
        let action = args["action"].as_str().ok_or("缺少 action")?;

        let bm = BoundedMemory::new(
            &self.config.memory_dir(),
            &self.db,
            self.config.memory.memory_char_limit,
            self.config.memory.user_char_limit,
        );

        match action {
            "read" => {
                let content = bm.read("user").map_err(|e| e.to_string())?;
                Ok(json!({"action": "read", "content": content}))
            }
            "write" => {
                let content = args["content"].as_str().ok_or("缺少 content")?;
                let confidence = args["confidence"].as_str().unwrap_or("medium");
                bm.write("user", content, confidence, None).map_err(|e| e.to_string())?;
                Ok(json!({"status": "ok"}))
            }
            "update" => {
                let old_text = args["old_text"].as_str().ok_or("缺少 old_text")?;
                let new_text = args["new_text"].as_str().ok_or("缺少 new_text")?;
                bm.update("user", old_text, new_text, None).map_err(|e| e.to_string())?;
                Ok(json!({"status": "ok"}))
            }
            "remove" => {
                let old_text = args["old_text"].as_str().ok_or("缺少 old_text")?;
                bm.remove("user", old_text, None).map_err(|e| e.to_string())?;
                Ok(json!({"status": "ok"}))
            }
            _ => Err(format!("未知 action: {}", action)),
        }
    }

    fn memory_provenance(&self, args: &Value) -> Result<Value, String> {
        let target = args["target"].as_str().ok_or("缺少 target")?;

        let bm = BoundedMemory::new(
            &self.config.memory_dir(),
            &self.db,
            self.config.memory.memory_char_limit,
            self.config.memory.user_char_limit,
        );
        let report = bm.verify_provenance(target).map_err(|e| e.to_string())?;

        Ok(json!({
            "status": "ok",
            "report": report
        }))
    }

    fn rebuild_index(&self) -> Result<Value, String> {
        let stats = crate::index::rebuild::rebuild_from_jsonl(
            &self.config.conversations_dir(),
            &self.db,
        ).map_err(|e| e.to_string())?;

        Ok(json!({
            "status": "ok",
            "sessions_processed": stats.sessions_processed,
            "turns_indexed": stats.turns_indexed,
            "errors": stats.errors
        }))
    }
}

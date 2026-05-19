use serde_json::{json, Value};
use std::rc::Rc;

use crate::config::Config;
use crate::fact::conversation::{SessionHeader, Turn};
use crate::fact::session_store::SessionStore;
use crate::growth::bounded_memory::BoundedMemory;
use crate::index::db::Db;

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
        json!({
            "name": "graph_assert",
            "description": "Write entity-relation triples to the graph memory layer. canonical-normalizes src/dst (lowercase + trim + whitespace fold). On duplicate triples, confidence is updated to MAX(existing, new); on duplicate entities, name and entity_type from first write are preserved.",
            "inputSchema": {
                "type": "object",
                "required": ["triples"],
                "properties": {
                    "triples": {
                        "type": "array",
                        "minItems": 1,
                        "items": {
                            "type": "object",
                            "required": ["src", "rel", "dst"],
                            "properties": {
                                "src": {"type": "string"},
                                "rel": {"type": "string"},
                                "dst": {"type": "string"},
                                "src_type": {"type": "string"},
                                "dst_type": {"type": "string"},
                                "confidence": {"type": "number", "minimum": 0, "maximum": 1},
                                "source_turn": {"type": "integer"}
                            }
                        }
                    },
                    "session_id": {"type": "string"}
                }
            }
        }),
        json!({
            "name": "graph_neighbors",
            "description": "Query N-hop neighbors of an entity. Supports rel_type filter and direction (out/in/both). hops in 1..=5.",
            "inputSchema": {
                "type": "object",
                "required": ["entity"],
                "properties": {
                    "entity": {"type": "string"},
                    "rel_type": {"type": "string"},
                    "direction": {"type": "string", "enum": ["out", "in", "both"], "default": "both"},
                    "hops": {"type": "integer", "minimum": 1, "maximum": 5, "default": 1},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 200, "default": 50}
                }
            }
        }),
        json!({
            "name": "graph_path",
            "description": "Find shortest path length between two entities (max_hops 1..=10). Returns found/length; full path serialization is a v1.3.1 polish.",
            "inputSchema": {
                "type": "object",
                "required": ["src", "dst"],
                "properties": {
                    "src": {"type": "string"},
                    "dst": {"type": "string"},
                    "max_hops": {"type": "integer", "minimum": 1, "maximum": 10, "default": 5}
                }
            }
        }),
        json!({
            "name": "graph_link_entity",
            "description": "Merge alias: rewire all edges from `from` entity to `to` entity, then delete `from`. Irreversible. Duplicate edges after rewiring are merged automatically (target side wins).",
            "inputSchema": {
                "type": "object",
                "required": ["from", "to"],
                "properties": {
                    "from": {"type": "string"},
                    "to": {"type": "string"},
                    "session_id": {"type": "string"}
                }
            }
        }),
    ]
}

/// 工具调用处理器
pub struct ToolHandler {
    config: Config,
    db: Rc<Db>,
    embedder: Option<crate::embedder::LazyEmbedder>,
}

impl ToolHandler {
    pub fn new(config: Config, db: Rc<Db>) -> Self {
        let embedder = config
            .discover_model_dir()
            .map(|path| crate::embedder::LazyEmbedder::new(&path));

        Self {
            config,
            db,
            embedder,
        }
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
            "graph_assert" => self.graph_assert(args),
            "graph_neighbors" => self.graph_neighbors(args),
            "graph_path" => self.graph_path(args),
            "graph_link_entity" => self.graph_link_entity(args),
            _ => Err(format!("未知工具: {}", name)),
        }
    }

    fn save_session(&self, args: &Value) -> Result<Value, String> {
        let session_id = args["session_id"].as_str().ok_or("缺少 session_id")?;
        let turns_arr = args["turns"].as_array().ok_or("缺少 turns")?;

        // [M2-FIX] 验证 turns 非空，避免空数组导致 start_time 解析失败
        if turns_arr.is_empty() {
            return Err("turns 数组不能为空".to_string());
        }
        let source = args["source"].as_str().map(|s| s.to_string());
        let title = args["title"].as_str().map(|s| s.to_string());
        let tags: Vec<String> = args["tags"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();

        // 支持可选的 profile 覆盖
        let profile_id = args["profile"]
            .as_str()
            .unwrap_or(&self.config.profile_id)
            .to_string();

        // 解析 header
        let first_turn_ts = turns_arr
            .first()
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

        // 解析 turns（严格校验 role，避免吞错）
        const VALID_ROLES: &[&str] = &["user", "assistant", "tool_call", "system"];
        let mut turns = Vec::new();
        for (i, t) in turns_arr.iter().enumerate() {
            let ts = t["timestamp"]
                .as_str()
                .ok_or_else(|| format!("turn[{}] 缺少 timestamp", i))?
                .to_string();
            let role = t["role"]
                .as_str()
                .ok_or_else(|| format!("turn[{}] 缺少 role", i))?;
            if !VALID_ROLES.contains(&role) {
                return Err(format!(
                    "turn[{}] role 非法: '{}' (允许: {:?})",
                    i, role, VALID_ROLES
                ));
            }
            let content = t["content"]
                .as_str()
                .ok_or_else(|| format!("turn[{}] 缺少 content", i))?
                .to_string();
            let metadata = if t.get("metadata").is_some() {
                Some(t["metadata"].clone())
            } else {
                None
            };
            turns.push(Turn {
                ts,
                seq: (i + 1) as u32,
                role: role.to_string(),
                content,
                metadata,
            });
        }

        let conv_dir = self.config.conversations_dir();
        let store = SessionStore::new(&conv_dir, &self.db)
            .with_preview_length(self.config.conversation.preview_length);
        let stats = store
            .save(&header, &turns, self.embedder.as_ref())
            .map_err(|e| format!("保存失败: {}", e))?;

        let mut response = json!({
            "status": "ok",
            "session_id": stats.session_id,
            "file_path": stats.file_path.to_string_lossy(),
            "turns_saved": stats.turns_saved
        });

        // 软提示：列出本次 session 中尚未被任何 relation 引用的 turn_ids
        if self.config.graph.enabled && self.config.graph.remind_on_save {
            if let Ok(turn_ids) = self.session_turn_ids(&stats.session_id) {
                if !turn_ids.is_empty() {
                    if let Ok(pending) =
                        crate::graph::pending_turn_ids(&self.db, &turn_ids)
                    {
                        if !pending.is_empty() {
                            response["graph_pending"] = json!({
                                "turn_ids": pending,
                                "hint": "These turns have no graph assertions yet. Call graph_assert with extracted triples (subject, relation, object) and source_turn=<id> to enable relationship queries."
                            });
                        }
                    }
                }
            }
        }

        Ok(response)
    }

    fn search_sessions(&self, args: &Value) -> Result<Value, String> {
        let query = args["query"].as_str().ok_or("缺少 query")?;
        let top_k = args["top_k"]
            .as_u64()
            .map(|v| v as usize)
            .unwrap_or(self.config.search.default_top_k);
        let search_mode = args["search_mode"]
            .as_str()
            .unwrap_or(&self.config.search.search_mode);

        let mode = match search_mode {
            "semantic" => crate::fact::search::SearchMode::Semantic,
            "keyword" => crate::fact::search::SearchMode::Keyword,
            _ => crate::fact::search::SearchMode::Hybrid,
        };

        let after_ms = args["time_range"]["after"]
            .as_str()
            .map(|s| crate::util::time::ts_to_unix_ms(s).unwrap_or(0));
        let before_ms = args["time_range"]["before"]
            .as_str()
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

        let results =
            crate::fact::search::search_sessions(&self.db, self.embedder.as_ref(), &params)
                .map_err(|e| e.to_string())?;

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

        let bm = self.make_bounded_memory();
        bm.write(target, content, confidence, session_id)
            .map_err(|e| e.to_string())?;

        Ok(json!({"status": "ok", "target": target}))
    }

    fn memory_update(&self, args: &Value) -> Result<Value, String> {
        let target = args["target"].as_str().ok_or("缺少 target")?;
        let old_text = args["old_text"].as_str().ok_or("缺少 old_text")?;
        let new_text = args["new_text"].as_str().ok_or("缺少 new_text")?;

        let bm = self.make_bounded_memory();
        bm.update(target, old_text, new_text, None)
            .map_err(|e| e.to_string())?;

        Ok(json!({"status": "ok"}))
    }

    fn memory_remove(&self, args: &Value) -> Result<Value, String> {
        let target = args["target"].as_str().ok_or("缺少 target")?;
        let old_text = args["old_text"].as_str().ok_or("缺少 old_text")?;

        let bm = self.make_bounded_memory();
        bm.remove(target, old_text, None)
            .map_err(|e| e.to_string())?;

        Ok(json!({"status": "ok"}))
    }

    fn memory_read(&self, args: &Value) -> Result<Value, String> {
        let target = args["target"].as_str().ok_or("缺少 target")?;

        let bm = self.make_bounded_memory();
        let content = bm.read(target).map_err(|e| e.to_string())?;

        Ok(json!({"target": target, "content": content}))
    }

    fn user_profile(&self, args: &Value) -> Result<Value, String> {
        let action = args["action"].as_str().ok_or("缺少 action")?;

        let bm = self.make_bounded_memory();

        match action {
            "read" => {
                let content = bm.read("user").map_err(|e| e.to_string())?;
                Ok(json!({"action": "read", "content": content}))
            }
            "write" => {
                let content = args["content"].as_str().ok_or("缺少 content")?;
                let confidence = args["confidence"].as_str().unwrap_or("medium");
                bm.write("user", content, confidence, None)
                    .map_err(|e| e.to_string())?;
                Ok(json!({"status": "ok"}))
            }
            "update" => {
                let old_text = args["old_text"].as_str().ok_or("缺少 old_text")?;
                let new_text = args["new_text"].as_str().ok_or("缺少 new_text")?;
                bm.update("user", old_text, new_text, None)
                    .map_err(|e| e.to_string())?;
                Ok(json!({"status": "ok"}))
            }
            "remove" => {
                let old_text = args["old_text"].as_str().ok_or("缺少 old_text")?;
                bm.remove("user", old_text, None)
                    .map_err(|e| e.to_string())?;
                Ok(json!({"status": "ok"}))
            }
            _ => Err(format!("未知 action: {}", action)),
        }
    }

    fn memory_provenance(&self, args: &Value) -> Result<Value, String> {
        let target = args["target"].as_str().ok_or("缺少 target")?;

        let bm = self.make_bounded_memory();
        let report = bm.verify_provenance(target).map_err(|e| e.to_string())?;

        Ok(json!({
            "status": "ok",
            "report": report
        }))
    }

    /// 集中构造 BoundedMemory，统一注入 security_scan 配置
    fn make_bounded_memory(&self) -> BoundedMemory<'_> {
        BoundedMemory::new(
            &self.config.memory_dir(),
            &self.db,
            self.config.memory.memory_char_limit,
            self.config.memory.user_char_limit,
        )
        .with_security_scan(self.config.memory.security_scan)
    }

    fn rebuild_index(&self) -> Result<Value, String> {
        let stats =
            crate::index::rebuild::rebuild_from_jsonl(&self.config.conversations_dir(), &self.db, self.embedder.as_ref())
                .map_err(|e| e.to_string())?;

        Ok(json!({
            "status": "ok",
            "sessions_processed": stats.sessions_processed,
            "turns_indexed": stats.turns_indexed,
            "vectors_indexed": stats.vectors_indexed,
            "errors": stats.errors
        }))
    }

    fn check_graph_enabled(&self) -> Result<(), String> {
        if !self.config.graph.enabled {
            return Err("graph disabled in config".to_string());
        }
        Ok(())
    }

    fn graph_assert(&self, args: &Value) -> Result<Value, String> {
        self.check_graph_enabled()?;
        let triples_value = args
            .get("triples")
            .ok_or_else(|| "missing triples".to_string())?;
        let triples: Vec<crate::graph::TripleInput> =
            serde_json::from_value(triples_value.clone())
                .map_err(|e| format!("invalid triples: {}", e))?;
        let stats = crate::graph::assert_triples(&self.db, &triples).map_err(|e| e.to_string())?;
        Ok(json!({
            "status": "ok",
            "entities_created": stats.entities_created,
            "entities_updated": stats.entities_updated,
            "relations_created": stats.relations_created,
            "relations_updated": stats.relations_updated
        }))
    }

    fn graph_neighbors(&self, args: &Value) -> Result<Value, String> {
        self.check_graph_enabled()?;
        let q: crate::graph::NeighborQuery = serde_json::from_value(args.clone())
            .map_err(|e| format!("invalid query: {}", e))?;
        let neighbors = crate::graph::neighbors(&self.db, &q).map_err(|e| e.to_string())?;
        Ok(json!({
            "status": "ok",
            "neighbors": neighbors
        }))
    }

    fn graph_path(&self, args: &Value) -> Result<Value, String> {
        self.check_graph_enabled()?;
        let src = args["src"].as_str().ok_or("missing src")?;
        let dst = args["dst"].as_str().ok_or("missing dst")?;
        let max_hops = args["max_hops"].as_u64().unwrap_or(5) as u32;
        let result = crate::graph::path(&self.db, src, dst, max_hops).map_err(|e| e.to_string())?;
        Ok(json!({
            "status": "ok",
            "found": result.found,
            "length": result.length,
            "path": result.path
        }))
    }

    fn graph_link_entity(&self, args: &Value) -> Result<Value, String> {
        self.check_graph_enabled()?;
        let from = args["from"].as_str().ok_or("missing from")?;
        let to = args["to"].as_str().ok_or("missing to")?;
        let rewired = crate::graph::link_entity(&self.db, from, to).map_err(|e| e.to_string())?;
        Ok(json!({
            "status": "ok",
            "edges_rewired": rewired,
            "old_entity_removed": crate::graph::canonicalize(from)
        }))
    }

    fn session_turn_ids(&self, session_id: &str) -> Result<Vec<i64>, String> {
        let mut stmt = self
            .db
            .conn()
            .prepare("SELECT id FROM turns WHERE session_id = ?1 ORDER BY seq")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([session_id], |row| row.get::<_, i64>(0))
            .map_err(|e| e.to_string())?;
        rows.collect::<Result<Vec<_>, _>>().map_err(|e| e.to_string())
    }
}

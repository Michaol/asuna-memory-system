use serde_json::{json, Value};

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
                    "tags": { "type": "array", "items": { "type": "string" } },
                    "profile": { "type": "string", "description": "Must equal the server's active profile (storage is bound to it; per-call override is not supported)" }
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
                    "role": { "type": "string", "enum": ["user", "assistant", "tool_call", "system"] },
                    "top_k": { "type": "integer", "default": 5, "maximum": 50, "description": "缺省取 config search.default_top_k（默认 5）；超过 50 按 50 截断（与 HTTP /search 同口径）" },
                    "search_mode": { "type": "string", "enum": ["semantic", "keyword", "hybrid"], "default": "hybrid", "description": "缺省取 config search.search_mode（默认 hybrid），非固定 hybrid" }
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
                    "new_text": { "type": "string" },
                    "session_id": { "type": "string", "description": "Source session ID for audit trail" }
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
                    "old_text": { "type": "string" },
                    "session_id": { "type": "string", "description": "Source session ID for audit trail" }
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
            "description": "从 JSONL 文件重建 SQLite 索引（后台异步执行）。使用 rebuild_status 查询进度。",
            "inputSchema": {
                "type": "object",
                "properties": {}
            }
        }),
        json!({
            "name": "rebuild_status",
            "description": "查询后台 rebuild_index 的执行进度",
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
                    }
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
            "description": "Find shortest path between two entities (max_hops 1..=10). Returns found/length/path where path is an alternating sequence of Entity and Edge objects.",
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
            "description": "Merge alias: rewire all edges from `from` entity to `to` entity, then delete `from`. Irreversible. Duplicate edges after rewiring are merged automatically (target side wins). Returns both old_canonical (DB key removed) and old_original_input (what you passed).",
            "inputSchema": {
                "type": "object",
                "required": ["from", "to"],
                "properties": {
                    "from": {"type": "string"},
                    "to": {"type": "string"}
                }
            }
        }),
        json!({
            "name": "graph_prune_dangling",
            "description": "Clean up dangling source_turn references: set relations.source_turn / entities.source_turn to NULL where the referenced turn no longer exists. Does NOT delete relations themselves — only clears stale provenance links. Run after large turn deletions to keep `doctor --verbose` dangling count at 0.",
            "inputSchema": {
                "type": "object",
                "properties": {}
            }
        }),
    ]
}

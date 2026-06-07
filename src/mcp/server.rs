use std::io::{self, BufRead, Write};
use std::rc::Rc;
use serde_json::{json, Value};

use crate::config::Config;
use crate::index::db::Db;
use super::protocol::*;
use super::tools::{self, ToolHandler};

/// 序列化 JSON-RPC 响应，失败时回退到内部错误响应（避免 panic）
fn to_response_value(resp: impl serde::Serialize) -> Value {
    serde_json::to_value(resp).unwrap_or_else(|e| {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": null,
            "error": { "code": -32603, "message": format!("序列化失败: {}", e) }
        })
    })
}

/// MCP stdio 服务器
pub struct Server {
    config: Config,
    db: Rc<Db>,
}

impl Server {
    pub fn new(config: Config, db: Rc<Db>) -> Self {
        Self { config, db }
    }

    /// 主循环：从 stdin 读取 JSON-RPC 请求，处理后写入 stdout
    pub fn run(&self) -> anyhow::Result<()> {
        let stdin = io::stdin();
        let stdout = io::stdout();
        let mut stdout_lock = stdout.lock();

        let handler = ToolHandler::new(self.config.clone(), self.db.clone());

        for line in stdin.lock().lines() {
            let line = line?;
            let line = line.trim();
            if line.is_empty() {
                continue;
            }

            let response = self.handle_line(line, &handler);

            if let Some(response) = response {
                let response_str = serde_json::to_string(&response)?;
                writeln!(stdout_lock, "{}", response_str)?;
                stdout_lock.flush()?;
            }
        }

        Ok(())
    }

    fn handle_line(&self, line: &str, handler: &ToolHandler) -> Option<Value> {
        // 解析请求
        let request: JsonRpcRequest = match serde_json::from_str(line) {
            Ok(req) => req,
            Err(e) => {
                return Some(to_response_value(
                    JsonRpcErrorResponse::new(Value::Null, PARSE_ERROR, &format!("JSON 解析错误: {}", e))
                ));
            }
        };

        let id = request.id.clone().unwrap_or(Value::Null);

        match request.method.as_str() {
            "initialize" => {
                Some(to_response_value(JsonRpcResponse::new(id, json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {
                        "tools": {}
                    },
                    "serverInfo": {
                        "name": "asuna-memory",
                        "version": env!("CARGO_PKG_VERSION")
                    }
                }))))
            }
            "notifications/initialized" => {
                // 通知，不需要响应
                None
            }
            "tools/list" => {
                Some(to_response_value(JsonRpcResponse::new(id, json!({
                    "tools": tools::tool_definitions()
                }))))
            }
            "tools/call" => {
                let params = request.params.unwrap_or(json!({}));
                let name = params["name"].as_str().unwrap_or("");
                let args = params.get("arguments").cloned().unwrap_or(json!({}));

                // [I8] MCP 协议规定 tools/call 工具层错误采用 content + isError 格式，
                // 区别于 JSON-RPC 传输层错误（使用 error 字段）。
                // 参考: https://modelcontextprotocol.io/docs/concepts/tools#error-handling
                // catch_unwind: 单个工具 panic 不应击垮整个 stdio 服务器进程。
                let call_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    handler.call(name, &args)
                }));
                match call_result {
                    Ok(Ok(result)) => {
                        Some(to_response_value(JsonRpcResponse::new(id, json!({
                            "content": [{"type": "text", "text": serde_json::to_string_pretty(&result).unwrap_or_default()}]
                        }))))
                    }
                    Ok(Err(e)) => {
                        Some(to_response_value(JsonRpcResponse::new(id, json!({
                            "content": [{"type": "text", "text": format!("错误: {}", e)}],
                            "isError": true
                        }))))
                    }
                    Err(_) => {
                        tracing::error!("工具 {} 执行 panic，已隔离", name);
                        Some(to_response_value(JsonRpcResponse::new(id, json!({
                            "content": [{"type": "text", "text": format!("内部错误: 工具 {} 执行时发生 panic", name)}],
                            "isError": true
                        }))))
                    }
                }
            }
            _ => {
                Some(to_response_value(
                    JsonRpcErrorResponse::new(id, METHOD_NOT_FOUND, &format!("未知方法: {}", request.method))
                ))
            }
        }
    }
}

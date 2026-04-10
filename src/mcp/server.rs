use std::io::{self, BufRead, Write};
use std::sync::Arc;
use serde_json::{json, Value};

use crate::config::Config;
use crate::index::db::Db;
use super::protocol::*;
use super::tools::{self, ToolHandler};

/// MCP stdio 服务器
pub struct Server {
    config: Config,
    db: Arc<Db>,
}

impl Server {
    pub fn new(config: Config, db: Arc<Db>) -> Self {
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
                return Some(serde_json::to_value(
                    JsonRpcErrorResponse::new(Value::Null, PARSE_ERROR, &format!("JSON 解析错误: {}", e))
                ).unwrap());
            }
        };

        let id = request.id.clone().unwrap_or(Value::Null);

        match request.method.as_str() {
            "initialize" => {
                Some(serde_json::to_value(JsonRpcResponse::new(id, json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {
                        "tools": {}
                    },
                    "serverInfo": {
                        "name": "asuna-memory",
                        "version": env!("CARGO_PKG_VERSION")
                    }
                }))).unwrap())
            }
            "notifications/initialized" => {
                // 通知，不需要响应
                return None;
            }
            "tools/list" => {
                Some(serde_json::to_value(JsonRpcResponse::new(id, json!({
                    "tools": tools::tool_definitions()
                }))).unwrap())
            }
            "tools/call" => {
                let params = request.params.unwrap_or(json!({}));
                let name = params["name"].as_str().unwrap_or("");
                let args = params.get("arguments").cloned().unwrap_or(json!({}));

                match handler.call(name, &args) {
                    Ok(result) => {
                        Some(serde_json::to_value(JsonRpcResponse::new(id, json!({
                            "content": [{"type": "text", "text": serde_json::to_string_pretty(&result).unwrap_or_default()}]
                        }))).unwrap())
                    }
                    Err(e) => {
                        Some(serde_json::to_value(JsonRpcResponse::new(id, json!({
                            "content": [{"type": "text", "text": format!("错误: {}", e)}],
                            "isError": true
                        }))).unwrap())
                    }
                }
            }
            _ => {
                Some(serde_json::to_value(
                    JsonRpcErrorResponse::new(id, METHOD_NOT_FOUND, &format!("未知方法: {}", request.method))
                ).unwrap())
            }
        }
    }
}

use serde_json::{json, Value};
use std::io::{self, BufRead, Write};
use std::rc::Rc;

use super::protocol::{
    JsonRpcErrorResponse, JsonRpcRequest, JsonRpcResponse, INVALID_PARAMS, INVALID_REQUEST,
    METHOD_NOT_FOUND, PARSE_ERROR,
};
use super::tools::{self, ToolHandler};
use crate::config::Config;
use crate::index::db::Db;

/// Maximum JSON-RPC request line length accepted over stdio (16 MiB).
const MAX_MCP_LINE_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, PartialEq, Eq)]
enum LineRead {
    Eof,
    Line(String),
    TooLarge,
    InvalidUtf8,
}

/// W21a: 严格 UTF-8——非法字节序列不得经 from_utf8_lossy 静默替换 U+FFFD
/// （会篡改 JSON 内容），返回 framing 错误交协议层报 -32700。
fn strict_line(buf: &[u8]) -> LineRead {
    match std::str::from_utf8(buf) {
        Ok(s) => LineRead::Line(s.to_string()),
        Err(_) => LineRead::InvalidUtf8,
    }
}

/// W25a: `fill_buf` 遇 EINTR 重试，与 std `read_line`/`read_until` 等价
/// （信号打断不应让读循环提前退出/报错）。重试内联在调用方循环内：缓冲须在
/// 同一轮消费完、借用每轮结束——返回借用的独立 retry helper 会触发 NLL
/// E0499（match scrutinee 的可变借用跨臂存活，与重试臂重借用冲突）。
fn drain_until_newline<R: BufRead>(reader: &mut R) -> io::Result<()> {
    loop {
        let rest = match reader.fill_buf() {
            Ok(buf) => buf,
            Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if rest.is_empty() {
            return Ok(());
        }
        if let Some(pos) = rest.iter().position(|&b| b == b'\n') {
            reader.consume(pos + 1);
            return Ok(());
        }
        let n = rest.len();
        reader.consume(n);
    }
}

enum ChunkAction {
    Done { consume: usize, result: LineRead },
    NeedMore { consume: usize },
    TooLargeDrain { consume: usize },
}

fn decide_chunk_action(available: &[u8], buf: &mut Vec<u8>, max_bytes: usize) -> ChunkAction {
    if available.is_empty() {
        return ChunkAction::Done {
            consume: 0,
            result: if buf.is_empty() {
                LineRead::Eof
            } else {
                strict_line(buf)
            },
        };
    }
    if let Some(pos) = available.iter().position(|&b| b == b'\n') {
        let take = pos + 1;
        let oversized = buf.len() + take > max_bytes;
        if !oversized {
            buf.extend_from_slice(&available[..take]);
        }
        return ChunkAction::Done {
            consume: take,
            result: if oversized {
                LineRead::TooLarge
            } else {
                strict_line(buf)
            },
        };
    }
    let avail_len = available.len();
    if buf.len() + avail_len > max_bytes {
        return ChunkAction::TooLargeDrain { consume: avail_len };
    }
    buf.extend_from_slice(available);
    ChunkAction::NeedMore { consume: avail_len }
}

fn read_chunk_step<R: BufRead>(
    reader: &mut R,
    buf: &mut Vec<u8>,
    max_bytes: usize,
) -> io::Result<Option<LineRead>> {
    loop {
        // W25a: 每次只调用一次 fill_buf()，在 Ok 臂内立即消费借用并转为 owned
        // ChunkAction，规避 NLL 跨 match 臂可变借用冲突且绝不在 Interrupted 之外重调。
        let action = match reader.fill_buf() {
            Ok(available) => decide_chunk_action(available, buf, max_bytes),
            Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        return match action {
            ChunkAction::Done { consume, result } => {
                if consume > 0 {
                    reader.consume(consume);
                }
                Ok(Some(result))
            }
            ChunkAction::TooLargeDrain { consume } => {
                reader.consume(consume);
                // Drain the remainder of the oversized line until '\n' or EOF so the next request stays aligned.
                drain_until_newline(reader)?;
                Ok(Some(LineRead::TooLarge))
            }
            ChunkAction::NeedMore { consume } => {
                reader.consume(consume);
                Ok(None)
            }
        };
    }
}

fn read_line_capped<R: BufRead>(reader: &mut R, max_bytes: usize) -> io::Result<LineRead> {
    let mut buf = Vec::new();
    loop {
        if let Some(outcome) = read_chunk_step(reader, &mut buf, max_bytes)? {
            return Ok(outcome);
        }
    }
}

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

fn parse_id_field(
    obj: &serde_json::Map<String, Value>,
) -> Result<Option<Value>, JsonRpcErrorResponse> {
    let Some(id_val) = obj.get("id") else {
        return Ok(None);
    };
    // M4: JSON-RPC id 不得含小数——浮点数（如 1.5）拒绝；整数（i64/u64）、
    // string、null 放行。带非 null id 的消息一律回错误响应，不得静默丢弃。
    let acceptable = id_val.is_string()
        || id_val.is_null()
        || id_val.as_number().is_some_and(|n| n.is_i64() || n.is_u64());
    if acceptable {
        return Ok(Some(id_val.clone()));
    }
    Err(JsonRpcErrorResponse::new(
        Value::Null,
        INVALID_REQUEST,
        "无效请求: id 必须为 string、整数 number 或 null",
    ))
}

fn validate_rpc_fields(
    obj: &serde_json::Map<String, Value>,
    err_id: Value,
) -> Result<(String, Option<Value>), JsonRpcErrorResponse> {
    if obj.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err(JsonRpcErrorResponse::new(
            err_id,
            INVALID_REQUEST,
            "无效请求: jsonrpc 必须为 \"2.0\"",
        ));
    }
    let Some(method) = obj
        .get("method")
        .and_then(Value::as_str)
        .filter(|m| !m.is_empty())
    else {
        return Err(JsonRpcErrorResponse::new(
            err_id,
            INVALID_REQUEST,
            "无效请求: method 必须为非空字符串",
        ));
    };
    let params = match obj.get("params") {
        None | Some(Value::Null) => None,
        Some(v) if v.is_object() || v.is_array() => Some(v.clone()),
        Some(_) => {
            return Err(JsonRpcErrorResponse::new(
                err_id,
                INVALID_REQUEST,
                "无效请求: params 必须为 object 或 array",
            ));
        }
    };
    Ok((method.to_string(), params))
}

fn parse_jsonrpc_request(line: &str) -> Result<JsonRpcRequest, JsonRpcErrorResponse> {
    let raw: Value = serde_json::from_str(line).map_err(|e| {
        JsonRpcErrorResponse::new(Value::Null, PARSE_ERROR, &format!("JSON 解析错误: {}", e))
    })?;
    let Some(obj) = raw.as_object() else {
        return Err(JsonRpcErrorResponse::new(
            Value::Null,
            INVALID_REQUEST,
            "无效请求: 请求必须为 JSON 对象",
        ));
    };
    let id = parse_id_field(obj)?;
    let err_id = id.clone().unwrap_or(Value::Null);
    let (method, params) = validate_rpc_fields(obj, err_id)?;
    Ok(JsonRpcRequest { id, method, params })
}

fn handle_tools_call(db: &Db, id: Value, params: Option<Value>, handler: &ToolHandler) -> Value {
    let Some(params_obj) = params.as_ref().and_then(Value::as_object) else {
        return to_response_value(JsonRpcErrorResponse::new(
            id,
            INVALID_PARAMS,
            "无效参数: tools/call params 必须为 JSON 对象",
        ));
    };
    let Some(name) = params_obj
        .get("name")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    else {
        return to_response_value(JsonRpcErrorResponse::new(
            id,
            INVALID_PARAMS,
            "无效参数: tools/call 缺少有效的工具名 name",
        ));
    };
    let args = match params_obj.get("arguments") {
        None | Some(Value::Null) => json!({}),
        Some(v) if v.is_object() => v.clone(),
        Some(_) => {
            return to_response_value(JsonRpcErrorResponse::new(
                id,
                INVALID_PARAMS,
                "无效参数: tools/call arguments 必须为 JSON 对象",
            ));
        }
    };

    // MCP 规范（tools / Error Handling）：未知工具名是协议层错误（-32602），
    // 不是工具执行错误（content+isError）——后者留给工具内部的参数/业务失败，
    // 供模型自纠重试；协议错误客户端多半无法恢复。
    if !ToolHandler::is_known_tool(name) {
        return to_response_value(JsonRpcErrorResponse::new(
            id,
            INVALID_PARAMS,
            &format!("未知工具: {}", name),
        ));
    }

    // [I8] MCP 协议规定 tools/call 工具层错误采用 content + isError 格式，
    // 区别于 JSON-RPC 传输层错误（使用 error 字段）。
    // catch_unwind: 单个工具 panic 不应击垮整个 stdio 服务器进程。
    let call_result =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handler.call(name, &args)));
    match call_result {
        Ok(Ok(result)) => {
            // N6: 序列化失败不得返回静默成功形态（空 text 且无 isError）。
            // result 已是 Value，生产不可达，仍显式置 isError 兜底。
            match serde_json::to_string_pretty(&result) {
                Ok(text) => to_response_value(JsonRpcResponse::new(
                    id,
                    json!({
                        "content": [{"type": "text", "text": text}]
                    }),
                )),
                Err(e) => to_response_value(JsonRpcResponse::new(
                    id,
                    json!({
                        "content": [{"type": "text", "text": format!("错误: 结果序列化失败: {}", e)}],
                        "isError": true
                    }),
                )),
            }
        }
        Ok(Err(e)) => to_response_value(JsonRpcResponse::new(
            id,
            json!({
                "content": [{"type": "text", "text": format!("错误: {}", e)}],
                "isError": true
            }),
        )),
        Err(_) => {
            db.rollback_if_in_tx();
            tracing::error!("工具 {} 执行 panic，已隔离", name);
            to_response_value(JsonRpcResponse::new(
                id,
                json!({
                    "content": [{"type": "text", "text": format!("内部错误: 工具 {} 执行时发生 panic", name)}],
                    "isError": true
                }),
            ))
        }
    }
}

impl Server {
    pub fn new(config: Config, db: Rc<Db>) -> Self {
        Self { config, db }
    }

    /// 主循环：从 stdin 读取 JSON-RPC 请求，处理后写入 stdout
    pub fn run(&self) -> anyhow::Result<()> {
        let stdin = io::stdin();
        let mut stdin_lock = stdin.lock();
        let stdout = io::stdout();
        let mut stdout_lock = stdout.lock();

        let handler = ToolHandler::new(self.config.clone(), self.db.clone());

        loop {
            match read_line_capped(&mut stdin_lock, MAX_MCP_LINE_BYTES)? {
                LineRead::Eof => break,
                LineRead::TooLarge => {
                    tracing::warn!("MCP stdin line exceeded {} bytes limit", MAX_MCP_LINE_BYTES);
                    let err_resp = to_response_value(JsonRpcErrorResponse::new(
                        Value::Null,
                        INVALID_REQUEST,
                        "请求行超过最大字节限制",
                    ));
                    let response_str = serde_json::to_string(&err_resp)?;
                    writeln!(stdout_lock, "{}", response_str)?;
                    stdout_lock.flush()?;
                }
                LineRead::InvalidUtf8 => {
                    // W21a: 非法 UTF-8 回 -32700 解析错误（id 无法从坏字节中可靠取得）。
                    tracing::warn!("MCP stdin line is not valid UTF-8");
                    let err_resp = to_response_value(JsonRpcErrorResponse::new(
                        Value::Null,
                        PARSE_ERROR,
                        "JSON 解析错误: 输入不是有效 UTF-8",
                    ));
                    let response_str = serde_json::to_string(&err_resp)?;
                    writeln!(stdout_lock, "{}", response_str)?;
                    stdout_lock.flush()?;
                }
                LineRead::Line(raw) => {
                    let line = raw.trim();
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
            }
        }

        Ok(())
    }

    fn handle_line(&self, line: &str, handler: &ToolHandler) -> Option<Value> {
        let request = match parse_jsonrpc_request(line) {
            Ok(req) => req,
            Err(err_resp) => return Some(to_response_value(err_resp)),
        };

        // JSON-RPC 2.0 通知（不含 id 字段）必须保持静默，不发送任何响应
        let id = request.id?;

        match request.method.as_str() {
            "initialize" => Some(to_response_value(JsonRpcResponse::new(
                id,
                json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {
                        "tools": {}
                    },
                    "serverInfo": {
                        "name": "asuna-memory",
                        "version": env!("CARGO_PKG_VERSION")
                    }
                }),
            ))),
            // M4: 仅 "id": null 的 notifications/initialized 静默（兼容历史客户端，
            // characterization_37 / differential_harness 基线钉住）；带非 null id 的
            // 消息一律回错误响应（落入下方 -32601），不得静默丢弃。
            "notifications/initialized" if id.is_null() => None,
            "ping" => Some(to_response_value(JsonRpcResponse::new(id, json!({})))),
            "tools/list" => Some(to_response_value(JsonRpcResponse::new(
                id,
                json!({
                    "tools": tools::tool_definitions()
                }),
            ))),
            "tools/call" => Some(handle_tools_call(&self.db, id, request.params, handler)),
            _ => Some(to_response_value(JsonRpcErrorResponse::new(
                id,
                METHOD_NOT_FOUND,
                &format!("未知方法: {}", request.method),
            ))),
        }
    }
}

#[cfg(test)]
#[path = "server_tests.rs"]
mod tests;

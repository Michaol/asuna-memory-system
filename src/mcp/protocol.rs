use serde::{Deserialize, Serialize};
use serde_json::Value;

/// JSON-RPC 2.0 请求
#[derive(Debug, Deserialize)]
pub struct JsonRpcRequest {
    #[allow(dead_code)]
    pub jsonrpc: String,
    pub id: Option<Value>,
    pub method: String,
    pub params: Option<Value>,
}

/// JSON-RPC 2.0 成功响应
#[derive(Debug, Serialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    pub id: Value,
    pub result: Value,
}

/// JSON-RPC 2.0 错误响应
#[derive(Debug, Serialize)]
pub struct JsonRpcErrorResponse {
    pub jsonrpc: String,
    pub id: Value,
    pub error: JsonRpcError,
}

#[derive(Debug, Serialize)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl JsonRpcResponse {
    pub fn new(id: Value, result: Value) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            result,
        }
    }
}

impl JsonRpcErrorResponse {
    pub fn new(id: Value, code: i32, message: &str) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            error: JsonRpcError {
                code,
                message: message.to_string(),
                data: None,
            },
        }
    }
}

/// 标准错误码
pub const PARSE_ERROR: i32 = -32700;
#[allow(dead_code)]
pub const INVALID_REQUEST: i32 = -32600;
pub const METHOD_NOT_FOUND: i32 = -32601;
#[allow(dead_code)]
pub const INVALID_PARAMS: i32 = -32602;
#[allow(dead_code)]
pub const INTERNAL_ERROR: i32 = -32603;

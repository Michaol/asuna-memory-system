//! Transport layer implementations for AMS
//!
//! - `mcp`: MCP stdio server (JSON-RPC 2.0)
//! - `http`: HTTP REST gateway (axum)

pub mod http;
pub mod state;

//! Transport layer implementations for AMS
//!
//! - `mcp`: MCP stdio server (JSON-RPC 2.0)
//! - `http`: HTTP REST gateway (axum)
//! - `pipeline`: Post-session L1 extraction + graph integration

pub mod http;
pub mod pipeline;
pub mod state;

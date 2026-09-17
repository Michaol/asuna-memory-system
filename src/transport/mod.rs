//! Transport layer implementations for AMS
//!
//! - `http`: HTTP REST gateway (axum)
//! - `state`: shared gateway state
//!
//! Note: the MCP stdio server (JSON-RPC 2.0) is the top-level sibling module
//! `crate::mcp`, not a submodule of `transport` (J36-3). The post-session
//! pipeline moved to `crate::service::pipeline` (J37) — it is gateway
//! orchestration, not transport infrastructure.

pub mod http;
pub mod state;

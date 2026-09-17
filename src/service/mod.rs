//! Service / orchestration layer for AMS.
//!
//! Sits below `transport` (the protocol entry points) and above the domain
//! modules (`memory`, `graph`, `index`, `growth`, …): it composes those
//! modules into end-to-end workflows but speaks no wire protocol itself.
//!
//! - `pipeline`: Post-session L1 extraction + graph integration, spawned by
//!   the gateway's `/session/end`. It was originally declared under
//!   `transport`, but it is a gateway-only orchestrator, not a transport
//!   facility (J37).

pub mod pipeline;

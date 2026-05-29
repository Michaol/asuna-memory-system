//! Short-term memory: Context offload and recall
//!
//! Stores long text (tool outputs, logs) to external files to reduce
//! context window usage. Agent references content via node_id.

pub mod offload;

pub use offload::{offload_text, recall_text, NodeId};

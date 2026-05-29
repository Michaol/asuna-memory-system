//! Memory module: multi-layer memory system for AMS
//!
//! Layers:
//! - L0: Conversation (JSONL + SQLite turns table)
//! - L1: Atom (atomic facts in bounded_memory table)
//! - L2: Scenario (scene blocks, future)
//! - L3: Persona (user profile, future)
//!
//! Core mechanisms:
//! - Evolution Chain: supersedes pointer chain for versioning
//! - Dedup: vector similarity-based duplicate/conflict detection
//! - LLM extraction: automatic fact extraction from conversations
//! - A-MAC admission: 5-dimensional scoring for memory admission

#[allow(dead_code)]
pub mod admission;
#[allow(dead_code)]
pub mod chain;
#[allow(dead_code)]
pub mod dedup;
#[allow(dead_code)]
pub mod l1;
#[allow(dead_code)]
pub mod llm;

#[allow(unused_imports)]
pub use admission::{AdmissionScore, AdmissionScorer, ScoreDimensions};
#[allow(unused_imports)]
pub use chain::{create_superseding, get_chain, get_latest_version, ChainEntry};
#[allow(unused_imports)]
pub use dedup::{check_dedup, cosine_similarity, DedupResult};
#[allow(unused_imports)]
pub use l1::{Atom, ExtractionResult, L1Extractor, TurnContent};
#[allow(unused_imports)]
pub use llm::LlmClient;

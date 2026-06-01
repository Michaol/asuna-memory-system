//! Memory module: multi-layer memory system for AMS
//!
//! Layers:
//! - L0: Conversation (JSONL + SQLite turns table)
//! - L1: Atom (atomic facts in bounded_memory table)
//! - L2: Scenario (scene blocks, aggregated from L1)
//! - L3: Persona (user profile, generated from L2)
//! - L4: Mental Model (cognitive frameworks, abstracted from L2/L3)
//! - L5: Intent Prediction (future needs, predicted from L4)
//!
//! Core mechanisms:
//! - Evolution Chain: supersedes pointer chain for versioning
//! - Dedup: vector similarity-based duplicate/conflict detection
//! - LLM extraction: automatic fact extraction from conversations
//! - A-MAC admission: 5-dimensional scoring for memory admission
//! - Progressive disclosure: layered retrieval (L3→L2→L1→L0)
//! - Graph integration: automatic relation derivation for L1 atoms

#[allow(dead_code)]
pub mod admission;
#[allow(dead_code)]
pub mod chain;
#[allow(dead_code)]
pub mod dedup;
#[allow(dead_code)]
pub mod graph_integration;
#[allow(dead_code)]
pub mod intent_prediction;
#[allow(dead_code)]
pub mod l1;
#[allow(dead_code)]
pub mod llm;
#[allow(dead_code)]
pub mod mental_model;
#[allow(dead_code)]
pub mod persona;
#[allow(dead_code)]
pub mod retrieval;
#[allow(dead_code)]
pub mod scenario;
#[allow(dead_code)]
pub mod skill;

#[allow(unused_imports)]
pub use admission::{AdmissionScore, AdmissionScorer, ScoreDimensions};
#[allow(unused_imports)]
pub use chain::{create_superseding, get_chain, get_latest_version, ChainEntry};
#[allow(unused_imports)]
pub use dedup::{check_dedup, cosine_similarity, DedupResult};
#[allow(unused_imports)]
pub use graph_integration::{integrate_atom_with_graph, multi_hop_query, GraphIntegrationResult};
#[allow(unused_imports)]
pub use intent_prediction::{AnticipatedNeeds, IntentPredictor, LikelyNextTopics};
#[allow(unused_imports)]
pub use l1::{Atom, ExtractionResult, L1Extractor, TurnContent};
#[allow(unused_imports)]
pub use llm::LlmClient;
#[allow(unused_imports)]
pub use mental_model::{CommunicationStyle, DecisionFramework, MentalModelGenerator, WorkflowPatterns};
#[allow(unused_imports)]
pub use persona::{Persona, PersonaGenerator};
#[allow(unused_imports)]
pub use retrieval::{RecallResult, RetrievalEngine};
#[allow(unused_imports)]
pub use scenario::{Scenario, ScenarioAggregator};
#[allow(unused_imports)]
pub use skill::{ExecutionTrace, Skill, SkillMemory};

/// Map a numeric confidence score (0.0–1.0) to the TEXT confidence enum.
///
/// The `bounded_memory` table stores confidence as TEXT ('high'/'medium'/'low'),
/// not as a numeric column. This helper provides a consistent mapping
/// across all modules that write confidence values.
pub fn confidence_text(score: f64) -> &'static str {
    if score >= 0.7 {
        "high"
    } else if score >= 0.4 {
        "medium"
    } else {
        "low"
    }
}

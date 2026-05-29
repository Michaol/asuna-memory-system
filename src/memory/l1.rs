//! L1 Atom extraction: extract atomic facts from conversation turns
//!
//! Pipeline:
//! 1. Collect turns since last extraction
//! 2. Send to LLM for fact extraction
//! 3. A-MAC admission scoring (5-dimensional)
//! 4. Vector dedup against existing L1 atoms
//! 5. Conflict detection → supersedes chain
//! 6. Store to bounded_memory table

use crate::config::AdmissionConfig;
use crate::index::db::Db;
use crate::memory::admission::AdmissionScorer;
use crate::memory::dedup::{check_dedup, DedupResult};
use crate::memory::llm::LlmClient;
use serde::{Deserialize, Serialize};

/// An extracted atomic fact
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Atom {
    pub content: String,
    pub atom_type: String,
    pub confidence: f64,
}

/// Result of LLM extraction
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractionResult {
    pub atoms: Vec<Atom>,
}

/// L1 extraction pipeline
pub struct L1Extractor<'a> {
    db: &'a Db,
    llm: &'a LlmClient,
    admission: Option<AdmissionScorer<'a>>,
}

impl<'a> L1Extractor<'a> {
    pub fn new(db: &'a Db, llm: &'a LlmClient) -> Self {
        Self {
            db,
            llm,
            admission: None,
        }
    }

    /// Create L1Extractor with admission scoring enabled
    pub fn with_admission(
        db: &'a Db,
        llm: &'a LlmClient,
        admission_config: &'a AdmissionConfig,
    ) -> Self {
        let admission = if admission_config.enabled {
            Some(AdmissionScorer::new(admission_config, Some(llm)))
        } else {
            None
        };
        Self {
            db,
            llm,
            admission,
        }
    }

    /// Extract atoms from conversation turns
    pub fn extract_from_turns(&self, turns: &[TurnContent]) -> anyhow::Result<Vec<Atom>> {
        if turns.is_empty() {
            return Ok(vec![]);
        }

        // Format turns for LLM
        let conversation = turns
            .iter()
            .map(|t| format!("{}: {}", t.role, t.content))
            .collect::<Vec<_>>()
            .join("\n");

        let system = r#"You are a memory extraction system. Extract atomic facts from the conversation.
Each fact should be:
- A single, self-contained piece of information
- Written in present tense
- Specific and precise

Return JSON format:
{
  "atoms": [
    {"content": "fact text", "atom_type": "fact|preference|decision|relationship", "confidence": 0.9}
  ]
}

atom_type values:
- fact: objective information
- preference: user preferences or likes/dislikes
- decision: choices or commitments made
- relationship: connections between people or concepts"#;

        let result: ExtractionResult = self.llm.chat_json(system, &conversation)?;
        Ok(result.atoms)
    }

    /// Store atoms with admission scoring, dedup and conflict detection
    pub fn store_atoms(
        &self,
        atoms: &[Atom],
        source_turn_ids: &[i64],
    ) -> anyhow::Result<Vec<i64>> {
        let mut stored_ids = Vec::new();
        let turn_ids_json = serde_json::to_string(source_turn_ids)?;

        // Load existing L1 embeddings for dedup and admission scoring
        let existing = self.load_existing_embeddings()?;
        let existing_embeddings: Vec<Vec<f32>> = existing.iter().map(|(_, e)| e.clone()).collect();

        // Format conversation context for admission scoring
        let conversation_context = format!("Processing {} atoms from {} turns", atoms.len(), source_turn_ids.len());

        for atom in atoms {
            // Generate embedding for the atom
            let embedding = self.embed_text(&atom.content)?;

            // A-MAC admission scoring (if enabled)
            if let Some(ref scorer) = self.admission {
                let admission_result = scorer.score(
                    &atom.content,
                    &atom.atom_type,
                    &embedding,
                    &existing_embeddings,
                    &conversation_context,
                )?;

                if !admission_result.admitted {
                    tracing::info!(
                        "Atom rejected by admission (score={:.2}, threshold={:.2}): {}",
                        admission_result.score,
                        scorer.threshold(),
                        atom.content
                    );
                    continue; // Skip this atom
                }

                tracing::debug!(
                    "Atom admitted (score={:.2}, U={:.2} N={:.2} R={:.2} I={:.2} C={:.2}): {}",
                    admission_result.score,
                    admission_result.dimensions.utility,
                    admission_result.dimensions.novelty,
                    admission_result.dimensions.recency,
                    admission_result.dimensions.importance,
                    admission_result.dimensions.confidence,
                    atom.content
                );
            }

            // Check for duplicates/conflicts
            match check_dedup(&embedding, &existing) {
                DedupResult::Duplicate { existing_id } => {
                    tracing::debug!(
                        "Skipping duplicate atom (existing_id={}): {}",
                        existing_id,
                        atom.content
                    );
                }
                DedupResult::Conflict { existing_id } => {
                    tracing::info!(
                        "Creating supersedes chain for conflicting atom (existing_id={}): {}",
                        existing_id,
                        atom.content
                    );
                    let new_id = crate::memory::chain::create_superseding(
                        self.db,
                        "memory",
                        &atom.content,
                        "atom",
                        atom.confidence,
                        Some(&turn_ids_json),
                        existing_id,
                    )?;
                    stored_ids.push(new_id);
                }
                DedupResult::Unique => {
                    let now = crate::util::time::now_unix_ms();
                    self.db.conn().execute(
                        "INSERT INTO bounded_memory
                         (target, content, created_at, updated_at, confidence,
                          memory_type, source_turn_ids, confidence_score)
                         VALUES ('memory', ?1, ?2, ?2, 'medium', 'atom', ?3, ?4)",
                        rusqlite::params![
                            atom.content,
                            now,
                            turn_ids_json,
                            atom.confidence,
                        ],
                    )?;
                    let id = self.db.conn().last_insert_rowid();
                    stored_ids.push(id);
                    tracing::info!("Stored unique atom (id={}): {}", id, atom.content);
                }
            }
        }

        Ok(stored_ids)
    }

    /// Load existing L1 atom embeddings from the database
    fn load_existing_embeddings(&self) -> anyhow::Result<Vec<(i64, Vec<f32>)>> {
        // For now, return empty — will be implemented when embedding integration is added
        // TODO: Load from vec_bounded_memory or compute on-the-fly
        Ok(vec![])
    }

    /// Generate embedding for text
    fn embed_text(&self, _text: &str) -> anyhow::Result<Vec<f32>> {
        // TODO: Integrate with embedding model
        // For now, return a dummy embedding
        Ok(vec![0.0; 768])
    }
}

/// A single turn's content for extraction
#[derive(Debug, Clone)]
pub struct TurnContent {
    pub role: String,
    pub content: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_atom_deserialization() {
        let json = r#"{"atoms": [
            {"content": "User prefers Rust", "atom_type": "preference", "confidence": 0.9},
            {"content": "User works on web projects", "atom_type": "fact", "confidence": 0.8}
        ]}"#;

        let result: ExtractionResult = serde_json::from_str(json).unwrap();
        assert_eq!(result.atoms.len(), 2);
        assert_eq!(result.atoms[0].atom_type, "preference");
        assert_eq!(result.atoms[1].confidence, 0.8);
    }

    #[test]
    fn test_turn_content_creation() {
        let turn = TurnContent {
            role: "user".to_string(),
            content: "Hello".to_string(),
        };
        assert_eq!(turn.role, "user");
        assert_eq!(turn.content, "Hello");
    }
}

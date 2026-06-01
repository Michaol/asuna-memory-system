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
use crate::embedder::LazyEmbedder;
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
    embedder: Option<&'a LazyEmbedder>,
    admission: Option<AdmissionScorer<'a>>,
}

impl<'a> L1Extractor<'a> {
    pub fn new(db: &'a Db, llm: &'a LlmClient, embedder: Option<&'a LazyEmbedder>) -> Self {
        Self {
            db,
            llm,
            embedder,
            admission: None,
        }
    }

    /// Create L1Extractor with admission scoring enabled
    pub fn with_admission(
        db: &'a Db,
        llm: &'a LlmClient,
        embedder: Option<&'a LazyEmbedder>,
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
            embedder,
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

        // Begin transaction for atomic operations
        let tx = self.db.conn().unchecked_transaction()?;

        for atom in atoms {
            // Generate embedding for the atom
            let embedding = self.embed_text(&atom.content)?;

            // Query the timestamp of the source turns for recency scoring
            let turn_timestamp_ms = if let Some(&turn_id) = source_turn_ids.first() {
                self.db.conn().query_row(
                    "SELECT timestamp_ms FROM turns WHERE id = ?1",
                    [turn_id],
                    |row| row.get::<_, i64>(0),
                ).unwrap_or_else(|_| chrono::Utc::now().timestamp_millis())
            } else {
                chrono::Utc::now().timestamp_millis()
            };

            // A-MAC admission scoring (if enabled)
            if let Some(ref scorer) = self.admission {
                let admission_result = scorer.score(
                    &atom.content,
                    &atom.atom_type,
                    &embedding,
                    &existing_embeddings,
                    &conversation_context,
                    turn_timestamp_ms,
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

                    // Store the embedding for the new atom
                    let embedding_bytes: Vec<u8> = embedding
                        .iter()
                        .flat_map(|f| f.to_le_bytes().to_vec())
                        .collect();

                    self.db.conn().execute(
                        "INSERT INTO vec_bounded_memory (id, embedding) VALUES (?1, ?2)",
                        rusqlite::params![new_id, embedding_bytes],
                    )?;

                    stored_ids.push(new_id);
                }
                DedupResult::Unique => {
                    let now = crate::util::time::now_unix_ms();
                    self.db.conn().execute(
                        "INSERT INTO bounded_memory
                         (target, content, created_at, updated_at, confidence,
                          memory_type, source_turn_ids)
                         VALUES ('memory', ?1, ?2, ?2, ?3, 'atom', ?4)",
                        rusqlite::params![
                            atom.content,
                            now,
                            crate::memory::confidence_text(atom.confidence),
                            turn_ids_json,
                        ],
                    )?;
                    let id = self.db.conn().last_insert_rowid();

                    // Store the embedding in vec_bounded_memory
                    let embedding_bytes: Vec<u8> = embedding
                        .iter()
                        .flat_map(|f| f.to_le_bytes().to_vec())
                        .collect();

                    self.db.conn().execute(
                        "INSERT INTO vec_bounded_memory (id, embedding) VALUES (?1, ?2)",
                        rusqlite::params![id, embedding_bytes],
                    )?;

                    stored_ids.push(id);
                    tracing::info!("Stored unique atom (id={}) with embedding: {}", id, atom.content);
                }
            }
        }

        // Commit transaction
        tx.commit()?;

        Ok(stored_ids)
    }

    /// Load existing L1 atom embeddings from the database
    fn load_existing_embeddings(&self) -> anyhow::Result<Vec<(i64, Vec<f32>)>> {
        let conn = self.db.conn();

        // Query all atoms with memory_type='atom' and their embeddings
        let mut stmt = conn.prepare(
            "SELECT bm.id, vec.embedding
             FROM bounded_memory bm
             INNER JOIN vec_bounded_memory vec ON bm.id = vec.id
             WHERE COALESCE(bm.memory_type, 'manual') = 'atom'"
        )?;

        let embeddings = stmt.query_map([], |row| {
            let id: i64 = row.get(0)?;
            let embedding_bytes: Vec<u8> = row.get(1)?;

            // Convert bytes to f32 vector (768 dimensions, 4 bytes each)
            let embedding: Vec<f32> = embedding_bytes
                .chunks_exact(4)
                .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
                .collect();

            Ok((id, embedding))
        })?;

        let mut result = Vec::new();
        for embedding in embeddings {
            result.push(embedding?);
        }

        tracing::debug!("Loaded {} existing L1 atom embeddings", result.len());
        Ok(result)
    }

    /// Generate embedding for text using the embedder
    fn embed_text(&self, text: &str) -> anyhow::Result<Vec<f32>> {
        match self.embedder {
            Some(embedder) => {
                let embedding = embedder.embed_document(text)?;
                tracing::debug!("Generated embedding for text: {}... ({} dimensions)",
                    text.chars().take(50).collect::<String>(),
                    embedding.len()
                );
                Ok(embedding)
            }
            None => {
                // Fallback to zero vector if no embedder available
                tracing::warn!("No embedder available, returning zero vector");
                Ok(vec![0.0; 768])
            }
        }
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

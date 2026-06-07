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
use crate::embedder::onnx::quantize_to_int8;
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
    /// Entity names extracted from the atom content (proper nouns, technical terms, etc.)
    /// Used for automatic graph `mentions` relations. `#[serde(default)]` for backward
    /// compatibility with LLM responses that omit this field.
    #[serde(default)]
    pub entities: Vec<String>,
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
    /// Optional growth layer for dual-write to MEMORY.md
    bounded_memory: Option<crate::growth::bounded_memory::BoundedMemory<'a>>,
}

impl<'a> L1Extractor<'a> {
    pub fn new(db: &'a Db, llm: &'a LlmClient, embedder: Option<&'a LazyEmbedder>) -> Self {
        Self {
            db,
            llm,
            embedder,
            admission: None,
            bounded_memory: None,
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
            bounded_memory: None,
        }
    }

    /// Set growth layer for dual-write to MEMORY.md.
    /// When set, `store_atoms` will also append atoms to the .md file
    /// with capacity-aware eviction.
    pub fn with_growth(
        mut self,
        bounded_memory: crate::growth::bounded_memory::BoundedMemory<'a>,
    ) -> Self {
        self.bounded_memory = Some(bounded_memory);
        self
    }

    /// Extract atoms from conversation turns
    pub fn extract_from_turns(&self, turns: &[TurnContent]) -> anyhow::Result<Vec<Atom>> {
        extract_atoms(self.llm, turns)
    }

    /// Store atoms with admission scoring, dedup and conflict detection
    pub fn store_atoms(
        &self,
        atoms: &[Atom],
        source_turn_ids: &[i64],
    ) -> anyhow::Result<Vec<i64>> {
        let mut stored_ids = Vec::new();
        let turn_ids_json = serde_json::to_string(source_turn_ids)?;

        // Load existing L1 embeddings for dedup and admission scoring.
        // `existing` is updated in-loop during the insert pass so that duplicates
        // within a single batch are detected against earlier atoms in the batch.
        let mut existing = self.load_existing_embeddings()?;
        let existing_embeddings: Vec<Vec<f32>> = existing.iter().map(|(_, e)| e.clone()).collect();

        // Format conversation context for admission scoring
        let conversation_context = format!("Processing {} atoms from {} turns", atoms.len(), source_turn_ids.len());

        // ── Pass 1: compute embeddings + admission decisions (LLM / network) ──
        // No DB transaction is open here, so blocking embedding/LLM calls do not
        // hold the SQLite write lock across the network (avoids WAL growth and
        // cross-process SQLITE_BUSY).
        let mut planned: Vec<(&Atom, Vec<f32>, bool)> = Vec::new();
        for atom in atoms {
            // Generate embedding for the atom
            let embedding = self.embed_text(&atom.content)?;

            // An all-zero embedding means no embedder was available (embed_text
            // fallback). Cosine distance is undefined for zero vectors (0/0 = NaN),
            // so we must not index them — a NaN `distance` would corrupt KNN ordering.
            // Skip vector indexing; maybe_backfill_bounded_memory_vec() indexes it
            // once an embedder is configured.
            let has_embedding = embedding.iter().any(|&v| v != 0.0);

            // A-MAC admission scoring (if enabled), against the pre-batch set.
            if let Some(ref scorer) = self.admission {
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

            planned.push((atom, embedding, has_embedding));
        }

        // ── Pass 2: dedup + insert (transaction, no network) ──
        let tx = self.db.conn().unchecked_transaction()?;

        for (atom, embedding, has_embedding) in &planned {
            let has_embedding = *has_embedding;

            // Check for duplicates/conflicts against pre-existing atoms AND atoms
            // already admitted earlier in THIS batch (existing is updated in-loop),
            // so intra-batch duplicates are not all stored.
            match check_dedup(embedding, &existing) {
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

                    // De-index the superseded (contradicted) atom so its stale vector
                    // does not co-surface with the replacement in semantic search.
                    self.db.conn().execute(
                        "DELETE FROM vec_bounded_memory WHERE id = ?1",
                        rusqlite::params![existing_id],
                    )?;
                    existing.retain(|(id, _)| *id != existing_id);

                    // Store the embedding for the new atom (INT8 quantized)
                    if has_embedding {
                        let embedding_bytes = quantize_to_int8(embedding);

                        self.db.conn().execute(
                            "INSERT INTO vec_bounded_memory (id, embedding) VALUES (?1, vec_int8(?2))",
                            rusqlite::params![new_id, embedding_bytes],
                        )?;
                        existing.push((new_id, embedding.clone()));
                    }

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

                    // Store the embedding in vec_bounded_memory (INT8 quantized)
                    if has_embedding {
                        let embedding_bytes = quantize_to_int8(embedding);

                        self.db.conn().execute(
                            "INSERT INTO vec_bounded_memory (id, embedding) VALUES (?1, vec_int8(?2))",
                            rusqlite::params![id, embedding_bytes],
                        )?;
                        existing.push((id, embedding.clone()));
                    }

                    stored_ids.push(id);
                    tracing::info!("Stored unique atom (id={}): {}", id, atom.content);
                }
            }
        }

        // Commit transaction
        tx.commit()?;

        // Dual-write: sync atoms to MEMORY.md with capacity-aware eviction
        if let Some(ref bm) = self.bounded_memory {
            match bm.sync_atoms_to_md() {
                Ok(evicted) => {
                    if evicted > 0 {
                        tracing::info!("store_atoms: evicted {} atoms from MEMORY.md", evicted);
                    }
                }
                Err(e) => {
                    tracing::warn!("store_atoms: failed to sync atoms to MEMORY.md: {}", e);
                }
            }
        }

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

            // Convert INT8 bytes back to f32 vector (1 byte per dimension, dimension-agnostic)
            let embedding: Vec<f32> = embedding_bytes
                .iter()
                .map(|&b| (b as i8) as f32 / 127.0)
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
                let dim = self.db.dimensions();
                tracing::warn!("No embedder available, returning zero vector (dim={})", dim);
                Ok(vec![0.0; dim])
            }
        }
    }
}

/// Extract atoms from conversation turns using only the LLM (no DB access).
///
/// Exposed as a free function so callers (e.g. the gateway pipeline) can run the
/// slow, network-bound extraction WITHOUT holding the global DB lock.
pub fn extract_atoms(llm: &LlmClient, turns: &[TurnContent]) -> anyhow::Result<Vec<Atom>> {
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
    {"content": "fact text", "atom_type": "fact|preference|decision|relationship", "confidence": 0.9, "entities": ["entity1", "entity2"]}
  ]
}

atom_type values:
- fact: objective information
- preference: user preferences or likes/dislikes
- decision: choices or commitments made
- relationship: connections between people or concepts

entities: Proper nouns, technical terms, product names, people, organizations
mentioned in the content. Max 5 per atom. Use the original language of the content.
Omit generic words. If no entities, use an empty array."#;

    let result: ExtractionResult = llm.chat_json(system, &conversation)?;
    Ok(result.atoms)
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

    /// Without an embedder, embed_text() returns a zero vector. Under the cosine
    /// metric a stored zero vector produces a NaN distance that corrupts KNN
    /// ordering, so store_atoms must persist the atom to bounded_memory but skip
    /// vector indexing (the backfill re-indexes it once an embedder exists).
    #[test]
    fn test_store_atoms_no_embedder_skips_vec_index() {
        let db = Db::open_memory().unwrap();
        db.init_schema().unwrap();

        let llm = LlmClient::new("http://localhost", "test-key", "test-model");
        let extractor = L1Extractor::new(&db, &llm, None);

        let atoms = vec![
            Atom {
                content: "User prefers Rust".to_string(),
                atom_type: "preference".to_string(),
                confidence: 0.9,
                entities: vec![],
            },
            Atom {
                content: "User works on web projects".to_string(),
                atom_type: "fact".to_string(),
                confidence: 0.8,
                entities: vec![],
            },
        ];

        let stored = extractor.store_atoms(&atoms, &[]).unwrap();
        assert_eq!(stored.len(), 2, "both atoms should be stored to bounded_memory");

        // Atoms are persisted to bounded_memory ...
        let bm_count: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM bounded_memory WHERE memory_type='atom'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(bm_count, 2);

        // ... but NO zero vectors are indexed (they would yield NaN cosine distance).
        let vec_count: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM vec_bounded_memory", [], |r| r.get(0))
            .unwrap();
        assert_eq!(vec_count, 0, "no-embedder atoms must not be vector-indexed");
    }
}

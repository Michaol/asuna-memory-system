//! Progressive disclosure retrieval engine
//!
//! Retrieves memories in a layered fashion:
//! L3 Persona → L2 Scenarios → L1 Atoms → L0 Conversation
//!
//! Token budget control:
//! - L3: max 200 tokens
//! - L2: max 300 tokens per scenario, 600 total
//! - L1: max 100 tokens per atom, 500 total
//! - L0: max 500 tokens per turn, remaining budget
//!
//! When budget exceeded, truncate from L0 first.

use crate::config::RecallConfig;
use crate::embedder::LazyEmbedder;
use crate::index::db::Db;
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Recall result with layered memories
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecallResult {
    pub persona: Option<String>,
    pub scenarios: Vec<String>,
    pub atoms: Vec<String>,
    pub conversation: Vec<String>,
    pub total_tokens: usize,
    pub token_budget: usize,
}

/// Progressive disclosure retrieval engine
pub struct RetrievalEngine<'a> {
    db: &'a Db,
    config: &'a RecallConfig,
    memory_dir: std::path::PathBuf,
    embedder: Option<&'a LazyEmbedder>,
}

impl<'a> RetrievalEngine<'a> {
    pub fn new(
        db: &'a Db,
        config: &'a RecallConfig,
        memory_dir: &Path,
        embedder: Option<&'a LazyEmbedder>,
    ) -> Self {
        Self {
            db,
            config,
            memory_dir: memory_dir.to_path_buf(),
            embedder,
        }
    }

    /// Retrieve memories using progressive disclosure
    pub fn recall(&self, query: &str) -> anyhow::Result<RecallResult> {
        let mut result = RecallResult {
            persona: None,
            scenarios: vec![],
            atoms: vec![],
            conversation: vec![],
            total_tokens: 0,
            token_budget: self.config.token_budget,
        };

        // L3: Persona (max 200 tokens)
        if let Some(persona) = self.load_persona_summary()? {
            let tokens = estimate_tokens(&persona);
            if tokens <= 200 {
                result.persona = Some(persona);
                result.total_tokens += tokens;
            }
        }

        // L2: Scenarios (max 300 tokens each, 600 total)
        let scenarios = self.search_scenarios(query, 3)?;
        for scenario in scenarios {
            let tokens = estimate_tokens(&scenario);
            if tokens <= 300 && result.total_tokens + tokens <= result.token_budget {
                result.scenarios.push(scenario);
                result.total_tokens += tokens;
            }

            if result.total_tokens >= 600 {
                break;
            }
        }

        // L1: Atoms (max 100 tokens each, 500 total)
        let atoms = self.search_atoms(query, 5)?;
        for atom in atoms {
            let tokens = estimate_tokens(&atom);
            if tokens <= 100 && result.total_tokens + tokens <= result.token_budget {
                result.atoms.push(atom);
                result.total_tokens += tokens;
            }

            if result.total_tokens >= 500 {
                break;
            }
        }

        // L0: Conversation (max 500 tokens each, remaining budget)
        let remaining_budget = result.token_budget.saturating_sub(result.total_tokens);
        let conversation = self.search_conversation(query, 3)?;
        for turn in conversation {
            let tokens = estimate_tokens(&turn);
            if tokens <= 500 && result.total_tokens + tokens <= result.token_budget {
                result.conversation.push(turn);
                result.total_tokens += tokens;
            }

            if result.total_tokens >= remaining_budget {
                break;
            }
        }

        Ok(result)
    }

    /// Load persona summary (first 200 tokens)
    fn load_persona_summary(&self) -> anyhow::Result<Option<String>> {
        let persona_path = self.memory_dir.join("persona.md");

        if !persona_path.exists() {
            return Ok(None);
        }

        let content = std::fs::read_to_string(&persona_path)?;

        // Extract first 200 tokens (roughly 800 characters)
        let summary = content
            .lines()
            .filter(|line| !line.starts_with('#') && !line.starts_with("---"))
            .take(10)
            .collect::<Vec<_>>()
            .join("\n");

        Ok(Some(summary))
    }

    /// Search scenarios by relevance (placeholder - will use embedding similarity)
    fn search_scenarios(&self, _query: &str, limit: usize) -> anyhow::Result<Vec<String>> {
        let scenarios_dir = self.memory_dir.join("scenarios");
        if !scenarios_dir.exists() {
            return Ok(vec![]);
        }

        let mut scenarios = Vec::new();

        for entry in std::fs::read_dir(&scenarios_dir)? {
            let entry = entry?;
            let path = entry.path();

            if path.extension().and_then(|s| s.to_str()) == Some("md") {
                if let Ok(content) = std::fs::read_to_string(&path) {
                    // Extract summary from frontmatter
                    let parts: Vec<&str> = content.splitn(3, "---").collect();
                    if parts.len() >= 3 {
                        scenarios.push(parts[2].trim().to_string());
                    }

                    if scenarios.len() >= limit {
                        break;
                    }
                }
            }
        }

        Ok(scenarios)
    }

    /// Search L1 atoms by relevance using vector similarity when embedder is available
    fn search_atoms(&self, query: &str, limit: usize) -> anyhow::Result<Vec<String>> {
        // Try vector similarity search if embedder is available
        if let Some(embedder) = self.embedder {
            if let Ok(query_embedding) = embedder.embed_query(query) {
                // Convert query embedding to bytes (little-endian f32)
                let query_bytes: Vec<u8> = query_embedding
                    .iter()
                    .flat_map(|f| f.to_le_bytes().to_vec())
                    .collect();

                // Use vector similarity search with vec_bounded_memory
                let mut stmt = self.db.conn().prepare(
                    "SELECT bm.content
                     FROM bounded_memory bm
                     JOIN vec_bounded_memory vec ON bm.id = vec.id
                     WHERE bm.memory_type = 'atom'
                     ORDER BY vec.distance(vec.embedding, ?1) ASC
                     LIMIT ?2",
                )?;

                let atoms: Vec<String> = stmt
                    .query_map(rusqlite::params![query_bytes, limit as i64], |row| row.get(0))?
                    .filter_map(|r| r.ok())
                    .collect();

                return Ok(atoms);
            }
        }

        // Fallback to confidence + recency ordering when embedder is unavailable
        let mut stmt = self.db.conn().prepare(
            "SELECT content FROM bounded_memory
             WHERE memory_type = 'atom'
             ORDER BY confidence_score DESC, created_at DESC
             LIMIT ?1",
        )?;

        let atoms: Vec<String> = stmt
            .query_map(rusqlite::params![limit as i64], |row| row.get(0))?
            .filter_map(|r| r.ok())
            .collect();

        Ok(atoms)
    }

    /// Search L0 conversation turns by relevance using FTS5 full-text search
    fn search_conversation(&self, query: &str, limit: usize) -> anyhow::Result<Vec<String>> {
        if query.trim().is_empty() {
            return Ok(vec![]);
        }

        // Use FTS5 full-text search with ranking
        let mut stmt = self.db.conn().prepare(
            "SELECT t.preview
             FROM turns_fts f
             JOIN turns t ON f.rowid = t.id
             WHERE turns_fts MATCH ?1
             ORDER BY rank
             LIMIT ?2",
        )?;

        let turns: Vec<String> = stmt
            .query_map(rusqlite::params![query, limit as i64], |row| row.get(0))?
            .filter_map(|r| r.ok())
            .collect();

        Ok(turns)
    }
}

/// Estimate token count (rough approximation: 1 token ≈ 4 characters)
fn estimate_tokens(text: &str) -> usize {
    text.len().div_ceil(4)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_estimate_tokens() {
        assert_eq!(estimate_tokens("hello"), 2); // 5 chars → 2 tokens
        assert_eq!(estimate_tokens("hello world"), 3); // 11 chars → 3 tokens
        assert_eq!(estimate_tokens(""), 0);
    }

    #[test]
    fn test_recall_result_serialization() {
        let result = RecallResult {
            persona: Some("User likes Rust".to_string()),
            scenarios: vec!["Scenario 1".to_string()],
            atoms: vec!["Atom 1".to_string()],
            conversation: vec!["Turn 1".to_string()],
            total_tokens: 100,
            token_budget: 2000,
        };

        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("User likes Rust"));
    }
}

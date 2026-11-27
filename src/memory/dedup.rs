//! Vector deduplication and conflict detection for L1 atoms
//!
//! - Cosine similarity > 0.95 → duplicate (skip)
//! - Cosine similarity 0.80–0.95 → potential conflict (supersedes chain)
//! - Cosine similarity < 0.80 → unique (store)

use serde::{Deserialize, Serialize};

/// Result of checking a new atom against existing atoms
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DedupResult {
    /// New atom is unique, should be stored
    Unique,
    /// New atom is a duplicate of an existing atom
    Duplicate { existing_id: i64 },
    /// New atom conflicts with an existing atom (semantic contradiction).
    /// `similarity` is the best-match cosine (informational: drives the
    /// supersede-chain log — the confidence gate, not the value, decides).
    Conflict { existing_id: i64, similarity: f32 },
}

/// Compute cosine similarity between two vectors
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }

    let mut dot = 0.0f32;
    let mut norm_a = 0.0f32;
    let mut norm_b = 0.0f32;

    for i in 0..a.len() {
        dot += a[i] * b[i];
        norm_a += a[i] * a[i];
        norm_b += b[i] * b[i];
    }

    let denom = norm_a.sqrt() * norm_b.sqrt();
    if !denom.is_finite() || denom < 1e-10 {
        0.0
    } else {
        let sim = dot / denom;
        if sim.is_finite() {
            sim
        } else {
            0.0
        }
    }
}

/// Check if a new atom should be stored, given existing atom embeddings.
///
/// Thresholds:
/// - similarity > 0.95 → Duplicate
/// - similarity 0.80–0.95 → Conflict
/// - similarity < 0.80 → Unique
pub fn check_dedup(new_embedding: &[f32], existing: &[(i64, Vec<f32>)]) -> DedupResult {
    const DUPLICATE_THRESHOLD: f32 = 0.95;
    const CONFLICT_THRESHOLD: f32 = 0.80;

    let mut best_similarity = 0.0f32;
    let mut best_id = 0i64;

    for (id, embedding) in existing {
        let sim = cosine_similarity(new_embedding, embedding);
        if sim > best_similarity {
            best_similarity = sim;
            best_id = *id;
        }
    }

    if best_similarity > DUPLICATE_THRESHOLD {
        DedupResult::Duplicate {
            existing_id: best_id,
        }
    } else if best_similarity > CONFLICT_THRESHOLD {
        DedupResult::Conflict {
            existing_id: best_id,
            similarity: best_similarity,
        }
    } else {
        DedupResult::Unique
    }
}

#[cfg(test)]
#[path = "dedup_tests.rs"]
mod tests;

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
    /// New atom conflicts with an existing atom (semantic contradiction)
    Conflict { existing_id: i64 },
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
    if denom < 1e-10 {
        0.0
    } else {
        dot / denom
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
        }
    } else {
        DedupResult::Unique
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cosine_similarity_identical() {
        let a = vec![1.0, 2.0, 3.0];
        let sim = cosine_similarity(&a, &a);
        assert!((sim - 1.0).abs() < 1e-6);
    }

    #[test]
    fn test_cosine_similarity_orthogonal() {
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![0.0, 1.0, 0.0];
        let sim = cosine_similarity(&a, &b);
        assert!(sim.abs() < 1e-6);
    }

    #[test]
    fn test_cosine_similarity_opposite() {
        let a = vec![1.0, 2.0, 3.0];
        let b = vec![-1.0, -2.0, -3.0];
        let sim = cosine_similarity(&a, &b);
        assert!((sim + 1.0).abs() < 1e-6);
    }

    #[test]
    fn test_cosine_similarity_empty() {
        assert_eq!(cosine_similarity(&[], &[]), 0.0);
        assert_eq!(cosine_similarity(&[1.0], &[]), 0.0);
    }

    #[test]
    fn test_check_dedup_unique() {
        let new = vec![1.0, 0.0, 0.0];
        let existing = vec![(1, vec![0.0, 1.0, 0.0])];
        assert!(matches!(check_dedup(&new, &existing), DedupResult::Unique));
    }

    #[test]
    fn test_check_dedup_duplicate() {
        let new = vec![1.0, 0.0, 0.0];
        let existing = vec![(42, vec![1.0, 0.01, 0.0])];
        match check_dedup(&new, &existing) {
            DedupResult::Duplicate { existing_id } => assert_eq!(existing_id, 42),
            other => panic!("Expected Duplicate, got {:?}", other),
        }
    }

    #[test]
    fn test_check_dedup_conflict() {
        let new = vec![1.0, 0.0, 0.0];
        // Similarity ~0.87 (between 0.80 and 0.95)
        let existing = vec![(7, vec![0.87, 0.5, 0.0])];
        match check_dedup(&new, &existing) {
            DedupResult::Conflict { existing_id } => assert_eq!(existing_id, 7),
            other => panic!("Expected Conflict, got {:?}", other),
        }
    }

    #[test]
    fn test_check_dedup_empty_existing() {
        let new = vec![1.0, 2.0, 3.0];
        assert!(matches!(check_dedup(&new, &[]), DedupResult::Unique));
    }

    #[test]
    fn test_check_dedup_selects_best_match() {
        let new = vec![1.0, 0.0, 0.0];
        let existing = vec![
            (1, vec![0.0, 1.0, 0.0]),   // orthogonal
            (2, vec![1.0, 0.01, 0.0]),   // near-duplicate
            (3, vec![0.5, 0.5, 0.0]),    // moderate
        ];
        match check_dedup(&new, &existing) {
            DedupResult::Duplicate { existing_id } => assert_eq!(existing_id, 2),
            other => panic!("Expected Duplicate(2), got {:?}", other),
        }
    }
}

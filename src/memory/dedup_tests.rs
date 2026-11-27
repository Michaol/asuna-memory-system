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
        DedupResult::Conflict {
            existing_id,
            similarity,
        } => {
            assert_eq!(existing_id, 7);
            assert!((similarity - 0.87).abs() < 0.01, "got {}", similarity);
        }
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
        (1, vec![0.0, 1.0, 0.0]),  // orthogonal
        (2, vec![1.0, 0.01, 0.0]), // near-duplicate
        (3, vec![0.5, 0.5, 0.0]),  // moderate
    ];
    match check_dedup(&new, &existing) {
        DedupResult::Duplicate { existing_id } => assert_eq!(existing_id, 2),
        other => panic!("Expected Duplicate(2), got {:?}", other),
    }
}

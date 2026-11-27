use super::{rrf_rank_score, RrfAccumulator, DEFAULT_RRF_K};

#[test]
fn test_rrf_rank_score_values() {
    assert!((DEFAULT_RRF_K - 60.0).abs() < 1e-12);
    assert!((rrf_rank_score(DEFAULT_RRF_K, 0) - 1.0 / 61.0).abs() < 1e-12);
    assert!((rrf_rank_score(DEFAULT_RRF_K, 1) - 1.0 / 62.0).abs() < 1e-12);
}

#[test]
fn test_rrf_accumulator_fuses_legs_and_preserves_first_value() {
    let mut acc = RrfAccumulator::<i64, &'static str, 2>::with_default_k();
    acc.add_leg(0, [(10, "first-10"), (20, "first-20")]);
    acc.add_leg(1, [(20, "second-20"), (30, "second-30")]);

    let ranked = acc.into_ranked();
    assert_eq!(ranked[0].key, 20);
    assert_eq!(ranked[0].value, "first-20");
    let expected_20 = rrf_rank_score(60.0, 1) + rrf_rank_score(60.0, 0);
    assert!((ranked[0].score - expected_20).abs() < 1e-12);
    assert!(ranked[0].leg_scores[0].is_some());
    assert!(ranked[0].leg_scores[1].is_some());

    // Key 10 appeared at rank=0 (1/61, leg 0) while key 30 appeared at rank=1 (1/62, leg 1).
    assert_eq!(ranked[1].key, 10);
    assert!(ranked[1].leg_scores[0].is_some());
    assert!(ranked[1].leg_scores[1].is_none());
    assert_eq!(ranked[2].key, 30);
    assert!(ranked[2].leg_scores[0].is_none());
    assert!(ranked[2].leg_scores[1].is_some());
}

#[test]
fn test_rrf_accumulator_tie_breakers() {
    let mut default_acc = RrfAccumulator::<i64, &'static str, 2>::with_default_k();
    default_acc.add_leg(0, [(10, "leg0-10")]);
    default_acc.add_leg(1, [(30, "leg1-30")]);
    let default_ranked = default_acc.into_ranked();
    // Both have rank=0 contribution (1/61); default tie-break orders descending key.
    assert_eq!(default_ranked[0].key, 30);
    assert_eq!(default_ranked[1].key, 10);

    let mut custom_acc = RrfAccumulator::<i64, i64, 2>::new(DEFAULT_RRF_K);
    custom_acc.add_leg(0, [(1, 100)]);
    custom_acc.add_leg(1, [(2, 999)]);
    let custom_ranked = custom_acc.into_ranked_by(|a, b| b.value.cmp(&a.value));
    assert_eq!(custom_ranked[0].key, 2);
    assert_eq!(custom_ranked[1].key, 1);
}

/// W18c: k must be finite and non-negative — a negative k panics at construction.
#[test]
#[should_panic(expected = "must be finite and non-negative")]
fn test_rrf_new_rejects_negative_k() {
    let _acc = RrfAccumulator::<i64, (), 2>::new(-1.0);
}

/// W18c: NaN k panics at construction (is_finite() is false for NaN).
#[test]
#[should_panic(expected = "must be finite and non-negative")]
fn test_rrf_new_rejects_nan_k() {
    let _acc = RrfAccumulator::<i64, (), 2>::new(f64::NAN);
}

/// W18c: an out-of-bounds leg index panics instead of indexing past leg_scores.
#[test]
#[should_panic(expected = "out of bounds")]
fn test_rrf_add_leg_out_of_bounds_panics() {
    let mut acc = RrfAccumulator::<i64, (), 2>::with_default_k();
    acc.add_leg(2, [(1, ())]);
}

/// W18c: a duplicate key within the SAME leg keeps only the first (best-rank)
/// hit — the later rank must not add score or overwrite the value.
#[test]
fn test_rrf_duplicate_key_in_same_leg_counts_first_hit_only() {
    let mut acc = RrfAccumulator::<i64, &'static str, 2>::with_default_k();
    acc.add_leg(0, [(10, "first"), (10, "dup"), (20, "twenty")]);
    let ranked = acc.into_ranked();
    assert_eq!(ranked.len(), 2);
    let hit10 = ranked.iter().find(|h| h.key == 10).unwrap();
    assert_eq!(hit10.value, "first");
    let first_only = rrf_rank_score(DEFAULT_RRF_K, 0);
    assert!(
        (hit10.score - first_only).abs() < 1e-12,
        "duplicate rank=1 hit must not add score: {}",
        hit10.score
    );
    assert_eq!(hit10.leg_scores[0], Some(first_only));
}

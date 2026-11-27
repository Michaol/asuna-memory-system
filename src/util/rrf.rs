use std::cmp::Ordering;
use std::collections::HashMap;
use std::hash::Hash;

/// Standard smoothing constant k=60 from Cormack, Clarke & Buettcher (2009),
/// "Reciprocal Rank Fusion outperforms Condorcet and individual Rank Learning Methods".
pub const DEFAULT_RRF_K: f64 = 60.0;

/// Compute the 0-indexed RRF contribution `1.0 / (k + rank + 1.0)`.
pub fn rrf_rank_score(k: f64, rank: usize) -> f64 {
    1.0 / (k + rank as f64 + 1.0)
}

/// Fused item returned by [`RrfAccumulator`], carrying total RRF score and per-leg contributions.
#[derive(Debug, Clone)]
pub struct RrfHit<K, V, const LEGS: usize> {
    pub key: K,
    pub value: V,
    pub score: f64,
    pub leg_scores: [Option<f64>; LEGS],
}

/// Multi-leg Reciprocal Rank Fusion accumulator parameterized by key/value types and leg count.
///
/// The first leg to insert a key supplies its `value`; subsequent occurrences accumulate both the
/// combined RRF `score` and the corresponding slot in `leg_scores`.
pub struct RrfAccumulator<K, V, const LEGS: usize> {
    k: f64,
    entries: HashMap<K, RrfHit<K, V, LEGS>>,
}

impl<K, V, const LEGS: usize> RrfAccumulator<K, V, LEGS>
where
    K: Eq + Hash + Clone,
{
    pub fn new(k: f64) -> Self {
        assert!(
            k.is_finite() && k >= 0.0,
            "RRF smoothing constant k must be finite and non-negative, got {k}"
        );
        Self {
            k,
            entries: HashMap::new(),
        }
    }

    pub fn with_default_k() -> Self {
        Self::new(DEFAULT_RRF_K)
    }

    pub fn add_leg(&mut self, leg: usize, items: impl IntoIterator<Item = (K, V)>) {
        assert!(leg < LEGS, "RRF leg index {leg} out of bounds ({LEGS})");
        for (rank, (key, value)) in items.into_iter().enumerate() {
            let rrf_score = rrf_rank_score(self.k, rank);
            let hit = self.entries.entry(key.clone()).or_insert_with(|| RrfHit {
                key,
                value,
                score: 0.0,
                leg_scores: [None; LEGS],
            });
            // Deduplicate duplicate keys within the same leg (keep first/best rank).
            if hit.leg_scores[leg].is_some() {
                continue;
            }
            hit.score += rrf_score;
            hit.leg_scores[leg] = Some(rrf_score);
        }
    }

    #[cfg(test)]
    pub fn into_ranked(self) -> Vec<RrfHit<K, V, LEGS>>
    where
        K: Ord,
    {
        self.into_ranked_by(|a, b| b.key.cmp(&a.key))
    }

    pub fn into_ranked_by<F>(self, mut tie_break: F) -> Vec<RrfHit<K, V, LEGS>>
    where
        F: FnMut(&RrfHit<K, V, LEGS>, &RrfHit<K, V, LEGS>) -> Ordering,
    {
        let mut hits: Vec<RrfHit<K, V, LEGS>> = self.entries.into_values().collect();
        hits.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| tie_break(a, b)));
        hits
    }
}

#[cfg(test)]
#[path = "rrf_tests.rs"]
mod tests;

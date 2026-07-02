use uuid::Uuid;
use std::collections::HashMap;

/// Reciprocal Rank Fusion (ADR-0006).
///
/// Takes two ranked lists of chunk IDs and returns a merged list sorted by
/// descending RRF score.
///
/// Formula: RRF(d) = Σ 1 / (k + rank(d))
///   where `k` is the smoothing constant (typically 60) and rank is
///   1-based.  Because the formula is rank-based it is scale-invariant,
///   so cosine distance and FTS rank scores need no normalization.
///
/// # Arguments
/// * `dense`  — chunk IDs ranked best-first by the dense ANN leg.
/// * `lexical` — chunk IDs ranked best-first by the lexical FTS leg.
/// * `k`      — RRF smoothing constant (use 60.0 per the original paper).
pub fn fuse(dense: &[Uuid], lexical: &[Uuid], k: f64) -> Vec<(Uuid, f64)> {
    let mut scores: HashMap<Uuid, f64> = HashMap::new();

    for (rank, &id) in dense.iter().enumerate() {
        *scores.entry(id).or_default() += 1.0 / (k + (rank + 1) as f64);
    }

    for (rank, &id) in lexical.iter().enumerate() {
        *scores.entry(id).or_default() += 1.0 / (k + (rank + 1) as f64);
    }

    let mut ranked: Vec<(Uuid, f64)> = scores.into_iter().collect();
    ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    ranked
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn fuse_empty_lists() {
        let result = fuse(&[], &[], 60.0);
        assert!(result.is_empty());
    }

    #[test]
    fn fuse_single_list() {
        let ids: Vec<Uuid> = (0..3).map(|_| Uuid::new_v4()).collect();
        let result = fuse(&ids, &[], 60.0);
        // Top result from a single list keeps its relative order.
        assert_eq!(result.len(), 3);
        assert!(result[0].1 > result[1].1);
    }

    #[test]
    fn fuse_agreement_boosts_score() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        // Both lists agree that `a` is rank-1.
        let result = fuse(&[a, b], &[a, b], 60.0);
        assert_eq!(result[0].0, a);
        // `a` should have a higher score than `b` because it tops both lists.
        assert!(result[0].1 > result[1].1);
    }

    #[test]
    fn rrf_score_formula_correct() {
        let id = Uuid::new_v4();
        let result = fuse(&[id], &[id], 60.0);
        // RRF(id) = 1/(60+1) + 1/(60+1) ≈ 0.032786...
        let expected = 2.0 / 61.0;
        assert!((result[0].1 - expected).abs() < 1e-9);
    }
}

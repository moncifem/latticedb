//! Metrics and seeded bootstrap confidence intervals.

use std::collections::BTreeSet;

/// Precision, recall and F1.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Prf {
    /// Fraction of returned labels that are correct.
    pub precision: f64,
    /// Fraction of expected labels that were returned.
    pub recall: f64,
    /// Harmonic mean.
    pub f1: f64,
}

fn ratio(n: usize, d: usize) -> f64 {
    if d == 0 { 1.0 } else { n as f64 / d as f64 }
}

/// Micro-averaged precision, recall and F1 over examples given as `(expected, got)` pairs.
pub fn prf(pairs: &[(Vec<String>, Vec<String>)]) -> Prf {
    let (mut tp, mut got_n, mut exp_n) = (0usize, 0usize, 0usize);
    for (expected, got) in pairs {
        let e: BTreeSet<&String> = expected.iter().collect();
        let g: BTreeSet<&String> = got.iter().collect();
        tp += e.intersection(&g).count();
        got_n += g.len();
        exp_n += e.len();
    }
    let precision = ratio(tp, got_n);
    let recall = ratio(tp, exp_n);
    let f1 = if precision == 0.0 && recall == 0.0 {
        0.0
    } else {
        2.0 * precision * recall / (precision + recall)
    };
    Prf {
        precision,
        recall,
        f1,
    }
}

/// Expected calibration error with `bins` equal-width confidence bins. `samples` are
/// `(confidence, was_correct)`.
pub fn ece(samples: &[(f64, bool)], bins: usize) -> f64 {
    if samples.is_empty() || bins == 0 {
        return 0.0;
    }
    let mut total = vec![0usize; bins];
    let mut correct = vec![0usize; bins];
    let mut conf_sum = vec![0.0f64; bins];
    for (c, ok) in samples {
        let b = ((c.clamp(0.0, 1.0) * bins as f64) as usize).min(bins - 1);
        if let Some(t) = total.get_mut(b) {
            *t += 1;
        }
        if let Some(s) = conf_sum.get_mut(b) {
            *s += c;
        }
        if *ok && let Some(k) = correct.get_mut(b) {
            *k += 1;
        }
    }
    let n = samples.len() as f64;
    (0..bins)
        .map(|b| {
            let t = total.get(b).copied().unwrap_or(0);
            if t == 0 {
                return 0.0;
            }
            let acc = correct.get(b).copied().unwrap_or(0) as f64 / t as f64;
            let conf = conf_sum.get(b).copied().unwrap_or(0.0) / t as f64;
            (t as f64 / n) * (acc - conf).abs()
        })
        .sum()
}

/// nDCG@k for one query. `ranked` are the relevance grades of the returned items in rank order;
/// `all_judged` are the grades of **every** judged item for the query, which defines the ideal
/// ranking (an omitted highly relevant item must lower the score). Grades must be finite and
/// non-negative, otherwise the result is `None`.
pub fn ndcg_at_k(ranked: &[f64], all_judged: &[f64], k: usize) -> Option<f64> {
    let ok = |v: &&f64| v.is_finite() && **v >= 0.0;
    if ranked.iter().chain(all_judged).any(|v| !ok(&v)) {
        return None;
    }
    let dcg = |rels: &[f64]| -> f64 {
        rels.iter()
            .take(k)
            .enumerate()
            .map(|(i, r)| (2f64.powf(*r) - 1.0) / ((i as f64) + 2.0).log2())
            .sum()
    };
    let mut ideal = all_judged.to_vec();
    ideal.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
    let best = dcg(&ideal);
    Some(if best == 0.0 { 0.0 } else { dcg(ranked) / best })
}

/// Tiny seeded generator so confidence intervals reproduce exactly.
struct Xorshift(u64);

impl Xorshift {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
}

/// Percentile bootstrap confidence interval of the mean of `values`.
pub fn bootstrap_ci(values: &[f64], iterations: usize, seed: u64, confidence: f64) -> (f64, f64) {
    if values.is_empty() {
        return (0.0, 0.0);
    }
    let mut rng = Xorshift(seed | 1);
    let mut means: Vec<f64> = (0..iterations.max(1))
        .map(|_| {
            let sum: f64 = (0..values.len())
                .map(|_| {
                    values
                        .get((rng.next() % values.len() as u64) as usize)
                        .copied()
                        .unwrap_or(0.0)
                })
                .sum();
            sum / values.len() as f64
        })
        .collect();
    means.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let tail = (1.0 - confidence.clamp(0.0, 1.0)) / 2.0;
    let lo = ((means.len() as f64 - 1.0) * tail).round() as usize;
    let hi = ((means.len() as f64 - 1.0) * (1.0 - tail)).round() as usize;
    (
        means.get(lo).copied().unwrap_or(0.0),
        means.get(hi).copied().unwrap_or(0.0),
    )
}

/// A bootstrap interval for each of precision, recall and F1 that resamples **examples** and
/// recomputes the micro-averaged metric on every replicate, so the interval estimates the same
/// statistic the point estimate reports.
pub fn bootstrap_micro_prf(
    pairs: &[(Vec<String>, Vec<String>)],
    iterations: usize,
    seed: u64,
    confidence: f64,
) -> [(f64, f64); 3] {
    if pairs.is_empty() {
        return [(0.0, 0.0); 3];
    }
    let mut rng = Xorshift(seed | 1);
    let mut series: [Vec<f64>; 3] = [Vec::new(), Vec::new(), Vec::new()];
    for _ in 0..iterations.max(1) {
        let sample: Vec<(Vec<String>, Vec<String>)> = (0..pairs.len())
            .filter_map(|_| {
                pairs
                    .get((rng.next() % pairs.len() as u64) as usize)
                    .cloned()
            })
            .collect();
        let m = prf(&sample);
        series[0].push(m.precision);
        series[1].push(m.recall);
        series[2].push(m.f1);
    }
    let tail = (1.0 - confidence.clamp(0.0, 1.0)) / 2.0;
    let mut out = [(0.0, 0.0); 3];
    for (slot, vals) in out.iter_mut().zip(series.iter_mut()) {
        vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let lo = ((vals.len() as f64 - 1.0) * tail).round() as usize;
        let hi = ((vals.len() as f64 - 1.0) * (1.0 - tail)).round() as usize;
        *slot = (
            vals.get(lo).copied().unwrap_or(0.0),
            vals.get(hi).copied().unwrap_or(0.0),
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn pair(e: &[&str], g: &[&str]) -> (Vec<String>, Vec<String>) {
        (
            e.iter().map(|s| (*s).to_string()).collect(),
            g.iter().map(|s| (*s).to_string()).collect(),
        )
    }

    #[test]
    fn prf_known_values() {
        let m = prf(&[pair(&["a", "b"], &["a", "c"])]);
        assert!(
            (m.precision - 0.5).abs() < 1e-9
                && (m.recall - 0.5).abs() < 1e-9
                && (m.f1 - 0.5).abs() < 1e-9
        );
        let perfect = prf(&[pair(&["a"], &["a"]), pair(&[], &[])]);
        assert!((perfect.f1 - 1.0).abs() < 1e-9);
        assert_eq!(prf(&[pair(&["a"], &[])]).f1, 0.0);
    }

    #[test]
    fn ece_is_zero_when_calibrated_and_large_when_overconfident() {
        let calibrated: Vec<(f64, bool)> = (0..100).map(|i| (0.8, i < 80)).collect();
        assert!(ece(&calibrated, 10) < 1e-9);
        let overconfident: Vec<(f64, bool)> = (0..100).map(|i| (0.95, i < 50)).collect();
        assert!(ece(&overconfident, 10) > 0.4);
    }

    #[test]
    fn ndcg_prefers_the_ideal_order() {
        let all = [3.0, 2.0, 1.0, 0.0];
        assert!((ndcg_at_k(&[3.0, 2.0, 1.0, 0.0], &all, 4).unwrap() - 1.0).abs() < 1e-9);
        assert!(ndcg_at_k(&[0.0, 1.0, 2.0, 3.0], &all, 4).unwrap() < 0.8);
        // Returning only a grade-1 item when a grade-3 item exists must not score 1.0.
        assert!(ndcg_at_k(&[1.0], &[1.0, 3.0], 5).unwrap() < 0.5);
        assert_eq!(ndcg_at_k(&[f64::NAN], &[1.0], 5), None);
        assert_eq!(ndcg_at_k(&[-1.0], &[1.0], 5), None);
    }

    #[test]
    fn bootstrap_is_reproducible_and_brackets_the_mean() {
        let v: Vec<f64> = (0..50).map(|i| f64::from(i % 2)).collect();
        let a = bootstrap_ci(&v, 500, 42, 0.95);
        assert_eq!(a, bootstrap_ci(&v, 500, 42, 0.95));
        // A different seed still brackets the mean (the exact digits may coincide on a coarse grid).
        let b = bootstrap_ci(&v, 500, 43, 0.95);
        assert!(b.0 < 0.5 && 0.5 < b.1);
        assert!(a.0 < 0.5 && 0.5 < a.1);
    }

    #[test]
    fn the_interval_matches_the_micro_averaged_statistic() {
        // 99 correct single-label examples and one completely missed 100-label example: the micro
        // recall is 99/199, and the interval must sit near it, not near the per-example mean 0.99.
        let mut pairs: Vec<(Vec<String>, Vec<String>)> =
            (0..99).map(|_| pair(&["a"], &["a"])).collect();
        let many: Vec<String> = (0..100).map(|i| format!("m{i}")).collect();
        pairs.push((many, vec![]));
        let point = prf(&pairs).recall;
        assert!((point - 99.0 / 199.0).abs() < 1e-9);
        let [_, recall, _] = bootstrap_micro_prf(&pairs, 400, 1, 0.95);
        assert!(
            recall.0 < 0.9 && recall.1 > 0.2,
            "interval {recall:?} should surround the micro recall {point}"
        );
    }

    proptest! {
        #[test]
        fn metrics_stay_in_unit_interval(e in proptest::collection::vec("[a-d]", 0..5), g in proptest::collection::vec("[a-d]", 0..5)) {
            let m = prf(&[(e, g)]);
            prop_assert!((0.0..=1.0).contains(&m.precision) && (0.0..=1.0).contains(&m.recall) && (0.0..=1.0).contains(&m.f1));
        }
    }
}

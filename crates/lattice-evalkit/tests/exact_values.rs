//! Exact-value tests for the metric, gate and report code, found by mutation testing: a formula or a
//! boundary that no test pins can change without anything noticing. The bootstrap and dataset values
//! are goldens: they must reproduce bit for bit.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use lattice_evalkit::drift::{ExactComparator, Noisy, Silent, generate};
use lattice_evalkit::gate::{MetricResult, Threshold, evaluate, parse_thresholds};
use lattice_evalkit::metrics::{bootstrap_ci, bootstrap_micro_prf, ece, ndcg_at_k, prf};
use lattice_evalkit::report::to_json;
use lattice_evalkit::suite::Provider;
use std::collections::BTreeMap;

fn pair(e: &[&str], g: &[&str]) -> (Vec<String>, Vec<String>) {
    (
        e.iter().map(|s| (*s).to_string()).collect(),
        g.iter().map(|s| (*s).to_string()).collect(),
    )
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-12
}

#[test]
fn prf_when_nothing_matches_is_zero() {
    let m = prf(&[pair(&["a"], &["b"])]);
    assert!(close(m.precision, 0.0) && close(m.recall, 0.0) && close(m.f1, 0.0));
    let m = prf(&[pair(&["a", "b"], &["a"])]);
    assert!(close(m.precision, 1.0) && close(m.recall, 0.5) && close(m.f1, 2.0 / 3.0));
}

#[test]
fn ece_is_the_weighted_gap_between_confidence_and_accuracy() {
    let samples = [(0.9, true), (0.9, false), (0.2, false)];
    // bin 9: 2 of 3 samples, accuracy 0.5 against confidence 0.9; bin 2: 1 of 3, accuracy 0 against 0.2.
    let expected = (2.0 / 3.0) * 0.4 + (1.0 / 3.0) * 0.2;
    assert!(close(ece(&samples, 10), expected));
    // Overconfidence and underconfidence count the same.
    assert!(close(ece(&[(0.1, true)], 10), 0.9));
    assert!(close(ece(&[(0.9, false)], 10), 0.9));
    // Two populated bins of equal weight: the error is the average of their gaps (0.4 and 0.3), which a
    // single merged bin would report as 0.05.
    let spread = [(0.9, true), (0.9, false), (0.2, false), (0.2, true)];
    assert!(close(ece(&spread, 10), 0.5 * 0.4 + 0.5 * 0.3));
    // Full confidence belongs in the last bin, not past it.
    assert!(close(ece(&[(1.0, false)], 10), 1.0));
    // A perfectly calibrated sample has no error.
    assert!(close(ece(&[(1.0, true), (0.0, false)], 10), 0.0));
}

#[test]
fn ece_with_nothing_to_bin_is_zero_and_does_not_panic() {
    assert!(close(ece(&[(0.5, true)], 0), 0.0));
    assert!(close(ece(&[], 10), 0.0));
}

#[test]
fn ndcg_matches_the_textbook_formula() {
    let ranked = [3.0, 2.0, 0.0];
    let judged = [3.0, 2.0, 1.0, 0.0];
    let gain = |r: f64| 2f64.powf(r) - 1.0;
    let dcg = gain(3.0) / 2f64.log2() + gain(2.0) / 3f64.log2();
    let ideal = dcg + gain(1.0) / 4f64.log2();
    assert!(close(ndcg_at_k(&ranked, &judged, 3).unwrap(), dcg / ideal));
    // The cut-off ignores everything after k.
    assert!(close(ndcg_at_k(&ranked, &judged, 1).unwrap(), 1.0));
    assert!(ndcg_at_k(&[f64::NAN], &[1.0], 3).is_none());
    assert!(ndcg_at_k(&[-1.0], &[1.0], 3).is_none());
}

#[test]
fn bootstrap_intervals_reproduce_bit_for_bit() {
    let v = [0.1, 0.4, 0.5, 0.9, 1.0, 0.3, 0.7, 0.2];
    assert_eq!(bootstrap_ci(&v, 200, 42, 0.9), (0.325, 0.6625));
    assert_eq!(bootstrap_ci(&v, 1, 7, 0.95), (0.5625, 0.5625));
    let pairs = vec![
        pair(&["a", "b"], &["a"]),
        pair(&["c"], &["c", "d"]),
        pair(&[], &["x"]),
        pair(&["y"], &["y"]),
        pair(&["z"], &[]),
    ];
    // Few iterations at a wide tail make the percentile index sensitive to its arithmetic.
    assert_eq!(bootstrap_ci(&v, 10, 3, 0.5), (0.41250000000000003, 0.5375));
    assert_eq!(
        bootstrap_micro_prf(&pairs, 10, 3, 0.5),
        [
            (0.5, 0.7142857142857143),
            (0.6, 0.8333333333333334),
            (0.5714285714285715, 0.7692307692307692)
        ]
    );
    assert_eq!(
        bootstrap_micro_prf(&pairs, 200, 5, 0.9),
        [
            (0.2857142857142857, 1.0),
            (0.3333333333333333, 1.0),
            (0.3333333333333333, 0.7692307692307692)
        ]
    );
}

#[test]
fn the_drift_dataset_is_a_golden_and_has_the_documented_shape() {
    let d = generate(11, 40, 30);
    assert_eq!(
        d.hash,
        "6689807c8ec5739892c90f9c95bb8e53b1d85f1aebb03577744228d8e221db5d"
    );
    assert_eq!(
        d.examples.first().map(|e| e.input.as_str()),
        Some("claim: setting_0 = 283; observed: setting_0 = 283")
    );
    for e in &d.examples {
        let drifted = e.expected.len() == 1;
        let (claim, observed) = e.input.split_once("; observed: ").unwrap();
        let c: u64 = claim.rsplit(" = ").next().unwrap().parse().unwrap();
        let o: u64 = observed.rsplit(" = ").next().unwrap().parse().unwrap();
        assert!((10..1000).contains(&c));
        if drifted {
            assert!((c + 1..=c + 50).contains(&o), "{c} {o}");
        } else {
            assert_eq!(c, o);
        }
    }
}

#[test]
fn a_zero_mutation_rate_mutates_nothing() {
    let d = generate(5, 5000, 0);
    assert!(d.examples.iter().all(|e| e.expected.is_empty()));
    let all = generate(5, 200, 100);
    assert!(all.examples.iter().all(|e| e.expected.len() == 1));
}

#[test]
fn the_stand_in_providers_say_what_they_claim() {
    let drifted = "claim: k = 1; observed: k = 2";
    let same = "claim: k = 1; observed: k = 1";
    assert_eq!(ExactComparator.name(), "exact-comparator");
    assert_eq!(
        ExactComparator.predict(drifted),
        vec!["drift:k".to_string()]
    );
    assert!(ExactComparator.predict(same).is_empty());
    assert_eq!(Silent.name(), "silent");
    assert!(Silent.predict(drifted).is_empty());
    assert_eq!(Noisy.name(), "noisy");
    assert_eq!(Noisy.predict(same), vec!["drift:k".to_string()]);
    assert_eq!(Noisy.predict(drifted), vec!["drift:k".to_string()]);
}

fn result(value: f64, lo: f64, hi: f64, n: usize) -> BTreeMap<String, MetricResult> {
    BTreeMap::from([(
        "f1".to_string(),
        MetricResult {
            value,
            ci_lo: lo,
            ci_hi: hi,
            n,
        },
    )])
}

fn gate(lower: f64, tolerance: f64) -> Vec<Threshold> {
    vec![Threshold {
        name: "f1".into(),
        lower_bound: lower,
        confidence: 0.95,
        tolerance,
    }]
}

#[test]
fn thresholds_reject_confidence_outside_the_open_interval() {
    let t = |c: &str| {
        parse_thresholds(&format!(
            "[[metric]]\nname=\"f1\"\nlower_bound=0.5\nconfidence={c}\ntolerance=0.0\n"
        ))
    };
    assert!(t("0.95").is_ok());
    assert!(t("1.0").is_err());
    assert!(t("0.0").is_err());
}

#[test]
fn any_non_finite_part_of_a_result_fails_the_gate() {
    for r in [
        result(f64::NAN, 0.8, 0.9, 10),
        result(0.85, f64::NAN, 0.9, 10),
        result(0.85, 0.8, f64::NAN, 10),
    ] {
        let g = evaluate(&gate(0.5, 0.0), &r);
        assert!(!g.passed);
        assert_eq!(
            g.rows.first().map(|r| r.reason.as_str()),
            Some("result is not a finite number")
        );
    }
}

#[test]
fn an_interval_that_misses_its_value_fails_the_gate() {
    for r in [
        result(0.85, 0.9, 0.95, 10),  // lower end above the value
        result(0.95, 0.8, 0.9, 10),   // value above the upper end
        result(0.85, 0.85, 0.80, 10), // inverted interval
    ] {
        let g = evaluate(&gate(0.5, 0.0), &r);
        assert_eq!(
            g.rows.first().map(|r| r.reason.as_str()),
            Some("interval does not contain the value")
        );
    }
    // A degenerate interval on the value is fine.
    assert!(evaluate(&gate(0.5, 0.0), &result(0.85, 0.85, 0.85, 10)).passed);
}

#[test]
fn the_tolerance_lowers_the_bar_for_the_interval_only() {
    // Value clears 0.9; the interval's lower end 0.89 is within 0.02 of it.
    assert!(evaluate(&gate(0.9, 0.02), &result(0.95, 0.89, 0.97, 10)).passed);
    // 0.87 is more than 0.02 below.
    assert!(!evaluate(&gate(0.9, 0.02), &result(0.95, 0.87, 0.97, 10)).passed);
    // A value below the bound fails whatever the interval says.
    assert!(!evaluate(&gate(0.9, 0.5), &result(0.89, 0.88, 0.9, 10)).passed);
    assert!(!evaluate(&gate(0.9, 0.0), &result(0.95, 0.9, 0.97, 0)).passed);
}

#[test]
fn json_escapes_control_characters_but_not_the_space() {
    let g = evaluate(&gate(0.5, 0.0), &BTreeMap::new());
    let json = to_json("s\u{1f} t", "h", "p", &g);
    assert!(json.contains("s\\u001f t"), "{json}");
    let json = to_json("tab\tnl\ncr\r\"q\"\\", "h", "p", &g);
    assert!(json.contains("tab\\tnl\\ncr\\r\\\"q\\\"\\\\"), "{json}");
}

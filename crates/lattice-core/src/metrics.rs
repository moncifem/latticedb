//! A small metrics registry. Names follow `lattice_<layer>_<thing>_<unit>`, which the tests check
//! for every name the crate defines.

use std::collections::BTreeMap;

/// Names of every metric the kernel emits.
pub mod names {
    /// Change sets acknowledged as newly committed.
    pub const COMMIT_TOTAL: &str = "lattice_core_commit_total";
    /// Commits answered with "already committed".
    pub const COMMIT_DUPLICATE_TOTAL: &str = "lattice_core_commit_duplicate_total";
    /// Commits that failed.
    pub const COMMIT_ERROR_TOTAL: &str = "lattice_core_commit_error_total";
    /// Time to commit one batch, from first check to acknowledgement.
    pub const COMMIT_LATENCY_SECONDS: &str = "lattice_core_commit_latency_seconds";

    /// Every name above, for validation.
    pub const ALL: [&str; 4] = [
        COMMIT_TOTAL,
        COMMIT_DUPLICATE_TOTAL,
        COMMIT_ERROR_TOTAL,
        COMMIT_LATENCY_SECONDS,
    ];
}

const LAYERS: [&str; 11] = [
    "abi", "seams", "sim", "lsl", "core", "engines", "identity", "service", "api", "intel", "packs",
];
const UNITS: [&str; 5] = ["total", "seconds", "bytes", "ratio", "count"];

/// Why a metric name is not acceptable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NameError(pub String);

/// Checks `lattice_<layer>_<thing>_<unit>` with lowercase ASCII words.
pub fn validate_name(name: &str) -> Result<(), NameError> {
    let parts: Vec<&str> = name.split('_').collect();
    let bad = |why: &str| Err(NameError(format!("`{name}`: {why}")));
    if parts.len() < 4 || parts.first() != Some(&"lattice") {
        return bad("expected lattice_<layer>_<thing>_<unit>");
    }
    if !parts.iter().all(|p| {
        !p.is_empty()
            && p.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
    }) {
        return bad("use lowercase ascii words separated by single underscores");
    }
    if !parts.get(1).is_some_and(|l| LAYERS.contains(l)) {
        return bad("second word must be a layer name");
    }
    if !parts.last().is_some_and(|u| UNITS.contains(u)) {
        return bad("last word must be a unit (total, seconds, bytes, ratio, count)");
    }
    Ok(())
}

/// Count, sum and maximum of observed values.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Histogram {
    /// Number of observations.
    pub count: u64,
    /// Sum of observations.
    pub sum: f64,
    /// Largest observation.
    pub max: f64,
}

/// Counters and histograms keyed by validated static names.
#[derive(Debug, Clone, Default)]
pub struct Metrics {
    counters: BTreeMap<&'static str, u64>,
    histograms: BTreeMap<&'static str, Histogram>,
}

impl Metrics {
    /// Adds one to a counter.
    pub fn incr(&mut self, name: &'static str) {
        *self.counters.entry(name).or_insert(0) += 1;
    }

    /// Records a value in a histogram.
    pub fn observe(&mut self, name: &'static str, value: f64) {
        let h = self.histograms.entry(name).or_default();
        h.count += 1;
        h.sum += value;
        h.max = h.max.max(value);
    }

    /// The value of a counter (zero if never incremented).
    pub fn counter(&self, name: &str) -> u64 {
        self.counters.get(name).copied().unwrap_or(0)
    }

    /// A histogram, if anything was observed.
    pub fn histogram(&self, name: &str) -> Option<Histogram> {
        self.histograms.get(name).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_defined_metric_name_follows_the_convention() {
        for n in names::ALL {
            assert_eq!(validate_name(n), Ok(()), "{n}");
        }
    }

    #[test]
    fn bad_names_are_rejected() {
        for n in [
            "commit_total",
            "lattice_core_total",
            "Lattice_core_commit_total",
            "lattice_nope_commit_total",
            "lattice_core_commit_apples",
            "lattice_core__commit_total",
        ] {
            assert!(validate_name(n).is_err(), "{n}");
        }
    }

    #[test]
    fn counters_and_histograms_accumulate() {
        let mut m = Metrics::default();
        m.incr(names::COMMIT_TOTAL);
        m.incr(names::COMMIT_TOTAL);
        m.observe(names::COMMIT_LATENCY_SECONDS, 0.5);
        m.observe(names::COMMIT_LATENCY_SECONDS, 1.5);
        assert_eq!(m.counter(names::COMMIT_TOTAL), 2);
        assert_eq!(m.counter(names::COMMIT_ERROR_TOTAL), 0);
        let h = m.histogram(names::COMMIT_LATENCY_SECONDS).unwrap();
        assert_eq!((h.count, h.sum, h.max), (2, 2.0, 1.5));
    }
}

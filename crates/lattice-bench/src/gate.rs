//! Result files and the regression comparison.

use std::collections::BTreeMap;

/// A benchmark that got slower than allowed.
#[derive(Debug, Clone, PartialEq)]
pub struct Regression {
    /// Benchmark name.
    pub name: String,
    /// Baseline nanoseconds per operation.
    pub baseline_ns: f64,
    /// Current nanoseconds per operation.
    pub current_ns: f64,
}

impl Regression {
    /// How much slower, as a fraction (0.2 means 20 percent slower).
    pub fn slowdown(&self) -> f64 {
        self.current_ns / self.baseline_ns - 1.0
    }
}

/// Benchmarks present in the baseline that are slower than `baseline * (1 + tolerance)`, and
/// benchmarks missing from the current run (a vanished benchmark is not a pass).
pub fn compare(
    baseline: &BTreeMap<String, f64>,
    current: &BTreeMap<String, f64>,
    tolerance: f64,
) -> Result<Vec<Regression>, String> {
    let mut out = Vec::new();
    for (name, base) in baseline {
        if !(base.is_finite() && *base > 0.0) {
            return Err(format!("baseline for `{name}` is not a positive number"));
        }
        match current.get(name) {
            None => {
                return Err(format!(
                    "benchmark `{name}` is in the baseline but was not run"
                ));
            }
            Some(cur) if !cur.is_finite() => {
                return Err(format!("current `{name}` is not a number"));
            }
            Some(cur) if *cur > base * (1.0 + tolerance) => out.push(Regression {
                name: name.clone(),
                baseline_ns: *base,
                current_ns: *cur,
            }),
            Some(_) => {}
        }
    }
    Ok(out)
}

/// Parses `{"name": ns, ...}`.
pub fn parse_results(text: &str) -> Result<BTreeMap<String, f64>, String> {
    let v: serde_json::Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
    let obj = v.as_object().ok_or("results must be a JSON object")?;
    obj.iter()
        .map(|(k, v)| {
            v.as_f64()
                .map(|n| (k.clone(), n))
                .ok_or_else(|| format!("`{k}` is not a number"))
        })
        .collect()
}

/// Deterministic JSON text for a results map.
pub fn render_results(results: &BTreeMap<String, f64>) -> String {
    let body: Vec<String> = results
        .iter()
        .map(|(k, v)| format!("  \"{k}\": {v:.1}"))
        .collect();
    format!("{{\n{}\n}}\n", body.join(",\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, f64)]) -> BTreeMap<String, f64> {
        pairs.iter().map(|(k, v)| ((*k).to_string(), *v)).collect()
    }

    #[test]
    fn an_injected_twenty_percent_slowdown_is_caught_and_noise_is_not() {
        let base = map(&[("commit", 1000.0), ("parse", 500.0)]);
        let slowed = map(&[("commit", 1200.0), ("parse", 500.0)]);
        let r = compare(&base, &slowed, 0.15).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].name, "commit");
        assert!((r[0].slowdown() - 0.2).abs() < 1e-9);
        let noisy = map(&[("commit", 1080.0), ("parse", 470.0)]);
        assert!(compare(&base, &noisy, 0.15).unwrap().is_empty());
        assert!(
            compare(&base, &slowed, 0.25).unwrap().is_empty(),
            "a looser tolerance lets 20 percent through"
        );
    }

    #[test]
    fn missing_or_nonsense_numbers_fail_instead_of_passing() {
        let base = map(&[("a", 10.0)]);
        assert!(compare(&base, &BTreeMap::new(), 0.1).is_err());
        assert!(compare(&base, &map(&[("a", f64::NAN)]), 0.1).is_err());
        assert!(compare(&map(&[("a", 0.0)]), &map(&[("a", 1.0)]), 0.1).is_err());
    }

    #[test]
    fn results_round_trip() {
        let m = map(&[("a", 12.5), ("b", 3.0)]);
        assert_eq!(parse_results(&render_results(&m)).unwrap(), m);
        assert!(parse_results("[1]").is_err());
        assert!(parse_results("{\"a\": \"x\"}").is_err());
    }
}

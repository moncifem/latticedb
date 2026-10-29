//! Reports. The output is a pure function of the inputs, so identical runs give identical bytes.

use crate::gate::GateReport;

/// A JSON number, or `null` for a value JSON cannot represent.
fn num(v: f64) -> String {
    if v.is_finite() {
        format!("{v:.6}")
    } else {
        "null".to_string()
    }
}

/// Escapes a string for use inside a JSON string literal, including control characters.
fn esc_json(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// Escapes text for HTML element content and attribute values.
fn esc_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// Deterministic JSON for a gate verdict.
pub fn to_json(suite: &str, dataset_hash: &str, provider: &str, report: &GateReport) -> String {
    let rows: Vec<String> = report
        .rows
        .iter()
        .map(|r| {
            let (v, lo, hi, n) = r.result.map_or(
                ("null".to_string(), "null".to_string(), "null".to_string(), "0".to_string()),
                |m| (num(m.value), num(m.ci_lo), num(m.ci_hi), m.n.to_string()),
            );
            format!(
                "{{\"metric\":\"{}\",\"value\":{v},\"ci_lo\":{lo},\"ci_hi\":{hi},\"n\":{n},\"bound\":{},\"passed\":{},\"reason\":\"{}\"}}",
                esc_json(&r.name),
                num(r.bound),
                r.passed,
                esc_json(&r.reason)
            )
        })
        .collect();
    format!(
        "{{\"suite\":\"{}\",\"dataset_hash\":\"{}\",\"provider\":\"{}\",\"passed\":{},\"metrics\":[{}]}}",
        esc_json(suite),
        esc_json(dataset_hash),
        esc_json(provider),
        report.passed,
        rows.join(",")
    )
}

/// Complete scorecard evidence, preserving exact numbers and threshold/dataset provenance.
/// The legacy `to_json` format remains unchanged. No example or provider is run by this function.
/// Refuses a mismatched gate report or dataset hash rather than authenticating inconsistent inputs.
pub fn to_evidence_json(
    suite: &str,
    dataset: &crate::dataset::Dataset,
    provider: &str,
    thresholds: &[crate::gate::Threshold],
    report: &GateReport,
) -> Result<String, String> {
    use std::collections::{BTreeMap, BTreeSet};
    if suite.is_empty()
        || provider.is_empty()
        || dataset.name.is_empty()
        || dataset.provenance.is_empty()
        || dataset.version == 0
        || crate::dataset::content_hash(&dataset.examples) != dataset.hash
    {
        return Err("invalid suite/provider or dataset provenance/hash".into());
    }
    let mut names = BTreeSet::new();
    for t in thresholds {
        if t.name.is_empty()
            || !names.insert(&t.name)
            || !t.lower_bound.is_finite()
            || !t.tolerance.is_finite()
            || t.tolerance < 0.0
            || !(t.confidence > 0.0 && t.confidence < 1.0)
        {
            return Err("invalid or duplicate threshold".into());
        }
    }
    if thresholds.is_empty() || report.rows.len() != thresholds.len() {
        return Err("missing gate thresholds or rows".into());
    }
    if report
        .rows
        .iter()
        .any(|row| row.result.is_some_and(|m| m.n > dataset.examples.len()))
    {
        return Err("metric sample count exceeds dataset examples".into());
    }
    let results: BTreeMap<_, _> = report
        .rows
        .iter()
        .filter_map(|row| row.result.map(|result| (row.name.clone(), result)))
        .collect();
    if crate::gate::evaluate(thresholds, &results) != *report {
        return Err("report disagrees with the supplied thresholds/results".into());
    }
    // f64's shortest round-trip representation avoids moving a value across a gate boundary.
    let exact = |v: f64| {
        if v.is_finite() {
            v.to_string()
        } else {
            "null".into()
        }
    };
    let mut rows = Vec::new();
    for (row, threshold) in report.rows.iter().zip(thresholds) {
        let (value, lo, hi, n) = row
            .result
            .map_or(("null".into(), "null".into(), "null".into(), 0), |m| {
                (exact(m.value), exact(m.ci_lo), exact(m.ci_hi), m.n)
            });
        rows.push(format!(
            "{{\"metric\":\"{}\",\"value\":{value},\"ci_lo\":{lo},\"ci_hi\":{hi},\"n\":{n},\"bound\":{},\"confidence\":{},\"tolerance\":{},\"passed\":{},\"reason\":\"{}\"}}",
            esc_json(&row.name), exact(threshold.lower_bound), exact(threshold.confidence),
            exact(threshold.tolerance), row.passed, esc_json(&row.reason)
        ));
    }
    let split = match dataset.split {
        crate::dataset::Split::Dev => "dev",
        crate::dataset::Split::Test => "test",
    };
    Ok(format!(
        "{{\"schema\":2,\"suite\":\"{}\",\"dataset_hash\":\"{}\",\"dataset\":{{\"name\":\"{}\",\"version\":{},\"split\":\"{split}\",\"provenance\":\"{}\",\"hash\":\"{}\",\"examples\":{}}},\"provider\":\"{}\",\"passed\":{},\"metrics\":[{}]}}",
        esc_json(suite),
        esc_json(&dataset.hash),
        esc_json(&dataset.name),
        dataset.version,
        esc_json(&dataset.provenance),
        esc_json(&dataset.hash),
        dataset.examples.len(),
        esc_json(provider),
        report.passed,
        rows.join(",")
    ))
}

/// A small HTML table for people. Every inserted string is escaped.
pub fn to_html(suite: &str, provider: &str, report: &GateReport) -> String {
    let mut out = format!(
        "<h1>{}: {}</h1><p>provider: {}</p><table><tr><th>metric</th><th>value</th><th>interval</th><th>bound</th><th>result</th></tr>",
        esc_html(suite),
        if report.passed { "PASS" } else { "FAIL" },
        esc_html(provider)
    );
    for r in &report.rows {
        let (v, iv) = r.result.map_or(("-".into(), "-".into()), |m| {
            (
                format!("{:.4}", m.value),
                format!("[{:.4}, {:.4}]", m.ci_lo, m.ci_hi),
            )
        });
        out.push_str(&format!(
            "<tr><td>{}</td><td>{v}</td><td>{iv}</td><td>{:.4}</td><td>{}</td></tr>",
            esc_html(&r.name),
            r.bound,
            if r.passed {
                "pass".to_string()
            } else {
                esc_html(&r.reason)
            }
        ));
    }
    out.push_str("</table>");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gate::{MetricResult, Row};

    fn report(name: &str, reason: &str, value: f64) -> GateReport {
        GateReport {
            rows: vec![Row {
                name: name.into(),
                result: Some(MetricResult {
                    value,
                    ci_lo: 0.0,
                    ci_hi: 1.0,
                    n: 3,
                }),
                bound: 0.5,
                passed: false,
                reason: reason.into(),
            }],
            passed: false,
        }
    }

    #[test]
    fn enriched_report_keeps_exact_gate_fields_and_dataset_provenance() {
        use crate::dataset::{Dataset, Example, Split};
        use crate::gate::{Threshold, evaluate};
        use std::collections::BTreeMap;
        let dataset = Dataset::new(
            "unit-data",
            1,
            Split::Test,
            "unit fixture",
            vec![Example {
                input: "input".into(),
                expected: vec!["label".into()],
            }],
        );
        let thresholds = vec![Threshold {
            name: "f1".into(),
            lower_bound: 0.800000035,
            confidence: 0.95,
            tolerance: 0.0,
        }];
        let results = BTreeMap::from([(
            "f1".into(),
            MetricResult {
                value: 0.80000004,
                ci_lo: 0.80000003,
                ci_hi: 0.9,
                n: 1,
            },
        )]);
        let report = evaluate(&thresholds, &results);
        assert!(!report.passed);
        let raw = to_evidence_json("suite", &dataset, "provider", &thresholds, &report).unwrap();
        let value: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(value["schema"], 2);
        assert_eq!(value["dataset"]["provenance"], "unit fixture");
        assert_eq!(value["dataset"]["hash"], dataset.hash);
        assert_eq!(value["metrics"][0]["bound"], 0.800000035);
        assert_eq!(value["metrics"][0]["ci_lo"], 0.80000003);
        assert_eq!(value["metrics"][0]["confidence"], 0.95);
        assert_eq!(value["metrics"][0]["tolerance"], 0.0);
        assert_eq!(value["passed"], false);
        let legacy = to_json("suite", &dataset.hash, "provider", &report);
        assert!(!legacy.contains("confidence") && !legacy.contains("schema"));
        let mut mismatched = report.clone();
        mismatched.passed = true;
        assert!(to_evidence_json("suite", &dataset, "provider", &thresholds, &mismatched).is_err());
        let mut changed_data = dataset.clone();
        changed_data.examples.clear();
        assert!(
            to_evidence_json("suite", &changed_data, "provider", &thresholds, &report).is_err()
        );
    }

    /// Valid evidence inputs, and a way to build the matching gate report for any thresholds, so a
    /// test can break exactly one rule without the report check rejecting the input first.
    mod evidence {
        use super::*;
        use crate::dataset::{Dataset, Example, Split};
        use crate::gate::{Threshold, evaluate};
        use std::collections::BTreeMap;

        pub(super) fn dataset() -> Dataset {
            Dataset::new(
                "d",
                1,
                Split::Test,
                "fixture",
                vec![Example {
                    input: "i".into(),
                    expected: vec!["l".into()],
                }],
            )
        }

        pub(super) fn threshold(name: &str) -> Threshold {
            Threshold {
                name: name.into(),
                lower_bound: 0.5,
                confidence: 0.95,
                tolerance: 0.0,
            }
        }

        pub(super) fn result(n: usize) -> MetricResult {
            MetricResult {
                value: 0.9,
                ci_lo: 0.8,
                ci_hi: 0.95,
                n,
            }
        }

        /// Runs `to_evidence_json`, building the report from the thresholds and a result of `n`
        /// samples for every threshold name.
        pub(super) fn run(
            suite: &str,
            dataset: &Dataset,
            provider: &str,
            thresholds: &[Threshold],
            n: usize,
        ) -> Result<String, String> {
            let results: BTreeMap<String, MetricResult> = thresholds
                .iter()
                .map(|t| (t.name.clone(), result(n)))
                .collect();
            let report = evaluate(thresholds, &results);
            to_evidence_json(suite, dataset, provider, thresholds, &report)
        }
    }

    #[test]
    fn evidence_json_rejects_each_invalid_input_on_its_own() {
        use evidence::{dataset, run, threshold};
        let ok = vec![threshold("f1")];
        // The baseline is valid, so every refusal below is caused by the one thing changed.
        assert!(run("s", &dataset(), "p", &ok, 1).is_ok());

        assert!(run("", &dataset(), "p", &ok, 1).is_err(), "empty suite");
        assert!(run("s", &dataset(), "", &ok, 1).is_err(), "empty provider");
        let mut d = dataset();
        d.name.clear();
        assert!(run("s", &d, "p", &ok, 1).is_err(), "empty dataset name");
        let mut d = dataset();
        d.provenance.clear();
        assert!(run("s", &d, "p", &ok, 1).is_err(), "empty provenance");
        let mut d = dataset();
        d.version = 0;
        assert!(run("s", &d, "p", &ok, 1).is_err(), "version zero");
        let mut d = dataset();
        d.hash = "0".repeat(d.hash.len());
        assert!(run("s", &d, "p", &ok, 1).is_err(), "wrong dataset hash");

        let bad = |f: &dyn Fn(&mut crate::gate::Threshold)| {
            let mut t = threshold("f1");
            f(&mut t);
            run("s", &dataset(), "p", &[t], 1)
        };
        assert!(bad(&|t| t.name.clear()).is_err(), "empty threshold name");
        assert!(bad(&|t| t.lower_bound = f64::NAN).is_err(), "nan bound");
        assert!(
            bad(&|t| t.tolerance = f64::INFINITY).is_err(),
            "infinite tolerance"
        );
        assert!(bad(&|t| t.tolerance = -0.1).is_err(), "negative tolerance");
        assert!(bad(&|t| t.confidence = 0.0).is_err(), "confidence zero");
        assert!(bad(&|t| t.confidence = 1.0).is_err(), "confidence one");
        assert!(
            bad(&|t| t.confidence = -0.5).is_err(),
            "negative confidence"
        );
        assert!(
            bad(&|t| t.confidence = 1.5).is_err(),
            "confidence above one"
        );
        assert!(bad(&|t| t.confidence = f64::NAN).is_err(), "nan confidence");

        let dup = vec![threshold("f1"), threshold("f1")];
        assert!(
            run("s", &dataset(), "p", &dup, 1).is_err(),
            "duplicate names"
        );
        assert!(run("s", &dataset(), "p", &[], 1).is_err(), "no thresholds");
    }

    #[test]
    fn evidence_json_accepts_a_sample_count_equal_to_the_dataset_and_refuses_one_more() {
        use evidence::{dataset, run, threshold};
        let ok = vec![threshold("f1")];
        let examples = dataset().examples.len();
        assert!(run("s", &dataset(), "p", &ok, examples).is_ok());
        assert!(run("s", &dataset(), "p", &ok, examples + 1).is_err());
    }

    #[test]
    fn json_escapes_control_characters_and_nulls_non_finite_numbers() {
        let j = to_json(
            "s\u{1}",
            "h",
            "p\tq",
            &report("m\"x", "line\nbreak\r", f64::NAN),
        );
        assert!(!j.contains('\u{1}') && !j.contains('\t') && !j.contains('\r'));
        assert!(j.contains("\\u0001") && j.contains("\\t") && j.contains("\"value\":null"));
    }

    #[test]
    fn html_neutralizes_markup_from_provider_suite_and_metric_names() {
        let h = to_html(
            "<script>alert(1)</script>",
            "<img onerror=x>",
            &report("<b>", "<i>", 0.1),
        );
        assert!(
            !h.contains("<script>")
                && !h.contains("<img")
                && !h.contains("<b>")
                && !h.contains("<i>")
        );
        assert!(h.contains("&lt;script&gt;"));
    }
}

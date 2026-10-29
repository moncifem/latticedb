//! Admin and scripting command line tool.
//!
//! Layer L7. Must not: contain server logic.
//!
//! `lattice doctor [--dir DATA_DIR] [--wal NAME]` verifies a data directory without modifying it.
//! `lattice ledger show [--dir DIR]` lists the runs recorded in a results ledger.
//! `lattice lsl check FILE`, `lsl fmt FILE [--write|--check]` and `lsl compat OLD NEW` work on schemas.
#![forbid(unsafe_code)]
#![allow(
    clippy::disallowed_methods,
    clippy::disallowed_types,
    clippy::print_stdout,
    clippy::print_stderr
)]

use lattice_core::ledger;
use lattice_core::scrub::{Issue, scrub};
use lattice_seams::{Io, OsRng, StdIo, SystemClock};
use std::process::ExitCode;

/// Scrubs the log in `wal` and returns the report text and whether the log is healthy.
mod lsl;

fn doctor<I: Io>(io: &I, wal: &str) -> (String, bool) {
    let report = match scrub(io, wal) {
        Ok(r) => r,
        Err(e) => return (format!("cannot read the log: {e}\n"), false),
    };
    let mut out = format!(
        "log `{wal}`: {} segments, {} records, {} change sets\n",
        report.segments, report.records, report.change_sets
    );
    for issue in &report.issues {
        let line = match issue {
            Issue::TornTail {
                segment,
                offset,
                bytes,
            } => {
                format!(
                    "note: torn tail in {segment} at offset {offset} ({bytes} bytes); recovery will truncate it"
                )
            }
            other => format!("PROBLEM: {other:?}"),
        };
        out.push_str(&line);
        out.push('\n');
    }
    out.push_str(if report.is_clean() {
        "result: healthy\n"
    } else {
        "result: damaged\n"
    });
    (out, report.is_clean())
}

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn ledger_show<I: Io>(io: I) -> Result<String, String> {
    let entries = ledger::read(io, SystemClock, OsRng).map_err(|e| format!("{e:?}"))?;
    let mut out = String::new();
    for e in &entries {
        out.push_str(&format!(
            "{:>6}  {:>13}  {:<6} {}\n",
            e.lsn, e.recorded_ms, e.kind, e.json
        ));
    }
    out.push_str(&format!("{} entries\n", entries.len()));
    Ok(out)
}

/// Additive JSON interchange over the existing opaque results payload; no WAL format change.
fn ledger_export<I: Io>(io: I) -> Result<String, String> {
    let entries = ledger::read(io, SystemClock, OsRng).map_err(|e| format!("{e:?}"))?;
    let values: Vec<_> = entries
        .iter()
        .map(|e| {
            serde_json::json!({
                "lsn": e.lsn, "recorded_ms": e.recorded_ms, "kind": e.kind, "json": e.json
            })
        })
        .collect();
    serde_json::to_string(&values).map_err(|e| e.to_string())
}

const MAX_RESULT_BYTES: u64 = 64 * 1024;

fn validate_result(kind: &str, json: &str) -> Result<String, String> {
    if kind.is_empty()
        || kind.len() > 64
        || !kind
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
    {
        return Err("kind must contain 1..64 lowercase ASCII letters, digits, '-' or '_'".into());
    }
    if json.len() as u64 > MAX_RESULT_BYTES {
        return Err("result exceeds 64 KiB".into());
    }
    let value: serde_json::Value = serde_json::from_str(json).map_err(|e| e.to_string())?;
    if !value.is_object() {
        return Err("result must be a JSON object".into());
    }
    serde_json::to_string(&value).map_err(|e| e.to_string())
}

fn ledger_record(args: &[String]) -> Result<(), String> {
    use std::io::Read;
    let kind = arg(args, "--kind").ok_or("missing --kind")?;
    let file = arg(args, "--json").ok_or("missing --json FILE")?;
    // Bounded read also handles a file that grows after being opened.
    let mut json = String::new();
    std::fs::File::open(file)
        .map_err(|e| e.to_string())?
        .take(MAX_RESULT_BYTES + 1)
        .read_to_string(&mut json)
        .map_err(|e| e.to_string())?;
    let json = validate_result(&kind, &json)?;
    let dir = arg(args, "--dir").unwrap_or_else(|| ".".into());
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    ledger::append(StdIo::new(dir), SystemClock, OsRng, &kind, &json)
        .map(|_| ())
        .map_err(|e| format!("{e:?}"))
}

/// One simulation entry read strictly: every count must be a non-negative integer.
struct SimRun {
    runs: u64,
    failing: u64,
    incomplete: u64,
}

impl SimRun {
    fn parse(json: &str) -> Option<Self> {
        let v: serde_json::Value = serde_json::from_str(json).ok()?;
        let count = |k: &str| v.get(k)?.as_u64();
        Some(Self {
            runs: count("runs")?,
            failing: count("failing")?,
            incomplete: count("incomplete")?,
        })
    }

    /// The reasons this run fails the scorecard thresholds; empty when it passes.
    fn failures(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if self.runs == 0 {
            out.push("the latest run verified nothing");
        }
        if self.failing > 0 {
            out.push("the latest run has violations");
        }
        // Compared without overflow: unverified runs must not exceed half of all runs.
        if self.incomplete > self.runs || self.incomplete.saturating_mul(2) > self.runs {
            out.push("more than half of the latest run was not verified");
        }
        out
    }
}

/// The last seven readable simulation runs: how many violations they hold in total, and a warning
/// when the unverified share of the latest run is well above that of the runs before it.
fn trend(sims: &[Option<SimRun>]) -> String {
    let recent: Vec<&SimRun> = sims
        .iter()
        .rev()
        .filter_map(Option::as_ref)
        .take(7)
        .collect();
    let runs: u64 = recent.iter().map(|r| r.runs).sum();
    let failing: u64 = recent.iter().map(|r| r.failing).sum();
    let mut out = format!(
        "last {} run(s): {runs} scenarios, {failing} violation(s) in total\n",
        recent.len()
    );
    if let Some((latest, earlier)) = recent.split_first() {
        let share = |rs: &[&SimRun]| {
            let total: u64 = rs.iter().map(|r| r.runs).sum();
            let inc: u64 = rs.iter().map(|r| r.incomplete).sum();
            (total > 0).then(|| inc as f64 / total as f64)
        };
        if let (Some(now), Some(before)) = (share(&[latest]), share(earlier))
            && now > 0.1
            && now > 2.0 * before
        {
            out.push_str("WARN: the unverified share is rising\n");
        }
    }
    out
}

/// Scorecard thresholds over the ledger: the latest simulation run must pass them, and the report
/// says how many recent runs in a row passed every threshold.
fn ledger_check<I: Io>(io: I) -> Result<(String, bool), String> {
    let entries = ledger::read(io, SystemClock, OsRng).map_err(|e| format!("{e:?}"))?;
    let sims: Vec<Option<SimRun>> = entries
        .iter()
        .filter(|e| e.kind == "sim")
        .map(|e| SimRun::parse(&e.json))
        .collect();
    let Some(latest) = sims.last() else {
        return Ok(("no simulation run in the ledger\n".into(), false));
    };
    let Some(latest) = latest else {
        return Ok(("the latest simulation entry is unreadable\n".into(), false));
    };
    let streak = sims
        .iter()
        .rev()
        .take_while(|r| r.as_ref().is_some_and(|r| r.failures().is_empty()))
        .count();
    let mut out = format!(
        "latest simulation run: {} runs, {} failing, {} unverified\n\
         {streak} clean simulation run(s) in a row\n",
        latest.runs, latest.failing, latest.incomplete
    );
    out.push_str(&trend(&sims));
    let failures = latest.failures();
    for f in &failures {
        out.push_str(&format!("FAIL: {f}\n"));
    }
    Ok((out, failures.is_empty()))
}

fn read_file(path: &str) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))
}

fn lsl_command(args: &[String]) -> ExitCode {
    let files: Vec<&String> = args
        .iter()
        .skip(1)
        .filter(|a| !a.starts_with("--"))
        .collect();
    let flag = |f: &str| args.iter().any(|a| a == f);
    let result: Result<(String, bool), String> =
        match (args.first().map(String::as_str), files.as_slice()) {
            (Some("check"), [file]) => read_file(file)
                .and_then(|s| lsl::check_text(&s))
                .map(|t| (t, true)),
            (Some("fmt"), [file]) => read_file(file).and_then(|src| {
                let out = lsl::format_text(&src)?;
                if flag("--write") {
                    std::fs::write(file.as_str(), &out).map_err(|e| format!("{file}: {e}"))?;
                    Ok((String::new(), true))
                } else if flag("--check") {
                    Ok((String::new(), out == src))
                } else {
                    Ok((out, true))
                }
            }),
            (Some("compat"), [old, new]) => {
                read_file(old).and_then(|o| read_file(new).and_then(|n| lsl::compat_text(&o, &n)))
            }
            _ => {
                eprintln!(
                    "usage: lattice lsl check FILE | fmt FILE [--write|--check] | compat OLD NEW"
                );
                return ExitCode::from(2);
            }
        };
    match result {
        Ok((text, ok)) => {
            print!("{text}");
            if ok {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            }
        }
        Err(e) => {
            eprintln!("{e}");
            ExitCode::from(1)
        }
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--version" | "-V" | "version") => {
            println!("lattice {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Some("doctor") => {
            let dir = arg(&args, "--dir").unwrap_or_else(|| ".".into());
            let wal = arg(&args, "--wal").unwrap_or_else(|| "wal".into());
            let (text, healthy) = doctor(&StdIo::new(dir), &wal);
            print!("{text}");
            if healthy {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            }
        }
        Some("ledger") if args.get(1).map(String::as_str) == Some("show") => {
            let dir = arg(&args, "--dir").unwrap_or_else(|| ".".into());
            match ledger_show(StdIo::new(dir)) {
                Ok(text) => {
                    print!("{text}");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("cannot read the ledger: {e}");
                    ExitCode::from(1)
                }
            }
        }
        Some("ledger") if args.get(1).map(String::as_str) == Some("export") => {
            let dir = arg(&args, "--dir").unwrap_or_else(|| ".".into());
            match ledger_export(StdIo::new(dir)) {
                Ok(text) => {
                    println!("{text}");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("cannot export ledger: {e}");
                    ExitCode::from(1)
                }
            }
        }
        Some("ledger") if args.get(1).map(String::as_str) == Some("record") => {
            match ledger_record(&args) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("cannot record result: {e}");
                    ExitCode::from(1)
                }
            }
        }
        Some("ledger") if args.get(1).map(String::as_str) == Some("check") => {
            let dir = arg(&args, "--dir").unwrap_or_else(|| ".".into());
            match ledger_check(StdIo::new(dir)) {
                Ok((text, ok)) => {
                    print!("{text}");
                    if ok {
                        ExitCode::SUCCESS
                    } else {
                        ExitCode::from(1)
                    }
                }
                Err(e) => {
                    eprintln!("cannot read the ledger: {e}");
                    ExitCode::from(1)
                }
            }
        }
        Some("lsl") => lsl_command(args.get(1..).unwrap_or_default()),
        _ => {
            eprintln!("usage: lattice doctor [--dir DATA_DIR] [--wal NAME]");
            eprintln!("       lattice ledger show [--dir DIR]");
            eprintln!("       lattice ledger check [--dir DIR]");
            eprintln!("       lattice ledger export [--dir DIR]");
            eprintln!("       lattice ledger record --kind KIND --json FILE [--dir DIR]");
            eprintln!(
                "       lattice lsl check FILE | fmt FILE [--write|--check] | compat OLD NEW"
            );
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lattice_core::changeset::ChangeSet;
    use lattice_core::{Config, Kernel};
    use lattice_sim::{FaultProfile, SeededRng, SimClock, SimIo};
    use lattice_testkit::scenario::{SharedClock, SharedIo};
    use std::rc::Rc;

    #[test]
    fn record_payload_is_bounded_and_cannot_inject_a_kind_delimiter() {
        assert!(validate_result("sim\nbench", "{}").is_err());
        assert!(validate_result("sim", "[]").is_err());
        assert!(validate_result("sim", "{broken}").is_err());
        assert!(validate_result("sim", &" ".repeat(MAX_RESULT_BYTES as usize + 1)).is_err());
        assert_eq!(
            validate_result("ci-job", "{ \"n\": 1 }").unwrap(),
            "{\"n\":1}"
        );
    }

    /// A JSON object of exactly `total` bytes.
    fn object_of_len(total: usize) -> String {
        format!("{{\"a\":\"{}\"}}", "x".repeat(total - 8))
    }

    #[test]
    fn the_result_size_limit_is_exact() {
        let max = MAX_RESULT_BYTES as usize;
        assert!(validate_result("sim", &object_of_len(max)).is_ok());
        let over = validate_result("sim", &object_of_len(max + 1)).unwrap_err();
        assert!(over.contains("64 KiB"), "{over}");
    }

    #[test]
    fn each_rule_for_a_result_kind_is_enforced_on_its_own() {
        assert!(validate_result(&"a".repeat(64), "{}").is_ok());
        assert!(validate_result(&"a".repeat(65), "{}").is_err());
        assert!(validate_result("", "{}").is_err());
        for bad in ["Sim", "a b", "a.b", "a/b", "\u{e9}"] {
            assert!(validate_result(bad, "{}").is_err(), "{bad:?}");
        }
        for good in ["a-b", "a_b", "a1", "0", "z"] {
            assert!(validate_result(good, "{}").is_ok(), "{good:?}");
        }
    }

    #[test]
    fn each_failed_threshold_is_named_separately() {
        let failures = |runs, failing, incomplete| {
            SimRun {
                runs,
                failing,
                incomplete,
            }
            .failures()
        };
        assert!(failures(10, 0, 0).is_empty());
        assert_eq!(failures(10, 1, 0), vec!["the latest run has violations"]);
        assert_eq!(failures(0, 0, 0), vec!["the latest run verified nothing"]);
        // Exactly half unverified is acceptable; one more is not.
        assert!(failures(10, 0, 5).is_empty());
        assert_eq!(
            failures(10, 0, 6),
            vec!["more than half of the latest run was not verified"]
        );
    }

    #[test]
    fn the_trend_warns_only_above_ten_percent_and_more_than_double_the_earlier_share() {
        let run = |runs, incomplete| {
            Some(SimRun {
                runs,
                failing: 0,
                incomplete,
            })
        };
        let warns = |sims: &[Option<SimRun>]| trend(sims).contains("WARN");
        // Exactly ten percent, from a clean start: not above it.
        assert!(!warns(&[run(100, 0), run(100, 10)]));
        assert!(warns(&[run(100, 0), run(100, 11)]));
        // Exactly double the earlier share: not more than double.
        assert!(!warns(&[run(100, 10), run(100, 20)]));
        assert!(warns(&[run(100, 10), run(100, 30)]));
        // A small share of many runs is small, whatever the counts multiply to.
        assert!(!warns(&[run(100, 0), run(200, 10)]));
    }

    /// Builds a small log on simulated storage and returns the disk.
    fn disk_with_log() -> SharedIo {
        let disk = SharedIo::new(SimIo::new(1, FaultProfile::clean()));
        let clock = SharedClock(Rc::new(SimClock::new(1)));
        let (mut k, _) =
            Kernel::open(disk.clone(), clock, SeededRng::new(1), &Config::default()).unwrap();
        for _ in 0..3 {
            let id = k.new_id();
            k.commit(ChangeSet::new(id, vec![1; 20])).unwrap();
        }
        drop(k);
        disk
    }

    #[test]
    fn ledger_show_lists_recorded_runs() {
        let disk = SharedIo::new(SimIo::new(1, FaultProfile::clean()));
        let clock = SharedClock(Rc::new(SimClock::new(7)));
        for n in 1..=2u64 {
            let json = format!("{{\"runs\":{n}}}");
            ledger::append(disk.clone(), clock.clone(), SeededRng::new(n), "sim", &json).unwrap();
        }
        let text = ledger_show(disk).unwrap();
        assert!(
            text.contains("{\"runs\":1}") && text.contains("{\"runs\":2}"),
            "{text}"
        );
        assert!(text.ends_with("2 entries\n"), "{text}");
    }

    fn ledger_with(entries: &[&str]) -> SharedIo {
        let disk = SharedIo::new(SimIo::new(1, FaultProfile::clean()));
        let clock = SharedClock(Rc::new(SimClock::new(7)));
        for (n, json) in entries.iter().enumerate() {
            ledger::append(
                disk.clone(),
                clock.clone(),
                SeededRng::new(n as u64 + 1),
                "sim",
                json,
            )
            .unwrap();
        }
        disk
    }

    #[test]
    fn ledger_check_passes_a_clean_latest_run_and_counts_the_streak() {
        let disk = ledger_with(&[
            "{\"runs\":10,\"failing\":1,\"incomplete\":0}",
            "{\"runs\":10,\"failing\":0,\"incomplete\":1}",
            "{\"runs\":10,\"failing\":0,\"incomplete\":2}",
        ]);
        let (text, ok) = ledger_check(disk).unwrap();
        assert!(
            ok && text.contains("2 clean simulation run(s) in a row"),
            "{text}"
        );
    }

    #[test]
    fn ledger_check_fails_on_violations_nothing_run_or_mostly_unverified() {
        for bad in [
            "{\"runs\":10,\"failing\":1,\"incomplete\":0}",
            "{\"runs\":0,\"failing\":0,\"incomplete\":0}",
            "{\"runs\":10,\"failing\":0,\"incomplete\":6}",
            "{\"nonsense\":true}",
        ] {
            let (text, ok) = ledger_check(ledger_with(&[bad])).unwrap();
            assert!(!ok, "{bad}: {text}");
        }
        assert!(!ledger_check(ledger_with(&[])).unwrap().1);
    }

    #[test]
    fn ledger_check_reads_counts_strictly_and_never_overflows() {
        for bad in [
            "{\"runs\":10,\"failing\":0.5,\"incomplete\":0}",
            "{\"runs\":10,\"failing\":0,\"incomplete\":9223372036854775808}",
            "{\"runs\":10,\"failing\":0}",
            "{\"runs\":-1,\"failing\":0,\"incomplete\":0}",
        ] {
            let (text, ok) = ledger_check(ledger_with(&[bad])).unwrap();
            assert!(!ok, "{bad}: {text}");
        }
    }

    #[test]
    fn ledger_check_reports_the_trend_and_warns_when_unverified_runs_rise() {
        let steady = ledger_with(&[
            "{\"runs\":100,\"failing\":0,\"incomplete\":5}",
            "{\"runs\":100,\"failing\":0,\"incomplete\":5}",
        ]);
        let (text, ok) = ledger_check(steady).unwrap();
        assert!(
            ok && text.contains("last 2 run(s): 200 scenarios, 0 violation(s)"),
            "{text}"
        );
        assert!(!text.contains("WARN"), "{text}");
        let rising = ledger_with(&[
            "{\"runs\":100,\"failing\":0,\"incomplete\":2}",
            "{\"runs\":100,\"failing\":0,\"incomplete\":30}",
        ]);
        let (text, ok) = ledger_check(rising).unwrap();
        assert!(
            ok && text.contains("WARN: the unverified share is rising"),
            "{text}"
        );
    }

    #[test]
    fn a_run_that_fails_the_thresholds_does_not_count_toward_the_streak() {
        let disk = ledger_with(&[
            "{\"runs\":10,\"failing\":0,\"incomplete\":0}",
            "{\"runs\":10,\"failing\":0,\"incomplete\":10}",
            "{\"runs\":10,\"failing\":0,\"incomplete\":0}",
        ]);
        let (text, ok) = ledger_check(disk).unwrap();
        assert!(
            ok && text.contains("1 clean simulation run(s) in a row"),
            "{text}"
        );
    }

    #[test]
    fn doctor_reports_a_healthy_log_and_a_damaged_one() {
        let mut io = disk_with_log();
        let (text, ok) = doctor(&io, "wal");
        assert!(
            ok && text.contains("3 change sets") && text.contains("healthy"),
            "{text}"
        );
        // Damage the middle of the log.
        let path = "wal/00000000000000000001.seg";
        let mut bytes = io.read(path).unwrap();
        bytes[30] ^= 0xFF;
        io.truncate(path, 0).unwrap();
        io.append(path, &bytes).unwrap();
        let (text, ok) = doctor(&io, "wal");
        assert!(
            !ok && text.contains("damaged") && text.contains("PROBLEM"),
            "{text}"
        );
    }

    #[test]
    fn doctor_on_a_missing_log_says_so() {
        let (text, ok) = doctor(&SharedIo::new(SimIo::new(1, FaultProfile::clean())), "wal");
        assert!(!ok && text.contains("cannot read the log"), "{text}");
    }
}

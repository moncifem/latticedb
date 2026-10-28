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

/// Reads `"key":N` from the flat JSON the runners write.
fn json_u64(json: &str, key: &str) -> Option<u64> {
    let marker = format!("\"{key}\":");
    let rest = json.get(json.find(&marker)? + marker.len()..)?;
    let end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    rest.get(..end)?.parse().ok()
}

/// Scorecard thresholds over the ledger: the latest simulation run must have verified runs and no
/// violations, and mostly verified runs. Also reports how many recent runs in a row were clean.
fn ledger_check<I: Io>(io: I) -> Result<(String, bool), String> {
    let entries = ledger::read(io, SystemClock, OsRng).map_err(|e| format!("{e:?}"))?;
    let sims: Vec<&ledger::LedgerEntry> = entries.iter().filter(|e| e.kind == "sim").collect();
    let Some(latest) = sims.last() else {
        return Ok(("no simulation run in the ledger\n".into(), false));
    };
    let field = |k: &str| json_u64(&latest.json, k);
    let (Some(runs), Some(failing), Some(incomplete)) =
        (field("runs"), field("failing"), field("incomplete"))
    else {
        return Ok(("the latest simulation entry is unreadable\n".into(), false));
    };
    let clean = |e: &&ledger::LedgerEntry| {
        json_u64(&e.json, "runs").is_some_and(|r| r > 0) && json_u64(&e.json, "failing") == Some(0)
    };
    let streak = sims.iter().rev().take_while(clean).count();
    let mut out = format!(
        "latest simulation run: {runs} runs, {failing} failing, {incomplete} unverified\n\
         {streak} clean simulation run(s) in a row\n"
    );
    let mut ok = true;
    if runs == 0 {
        out.push_str("FAIL: the latest run verified nothing\n");
        ok = false;
    }
    if failing > 0 {
        out.push_str("FAIL: the latest run has violations\n");
        ok = false;
    }
    if incomplete * 2 > runs {
        out.push_str("FAIL: more than half of the latest run was not verified\n");
        ok = false;
    }
    Ok((out, ok))
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

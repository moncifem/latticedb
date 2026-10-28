//! Admin and scripting command line tool.
//!
//! Layer L7. Must not: contain server logic.
//!
//! `lattice doctor [--dir DATA_DIR] [--wal NAME]` verifies a data directory without modifying it.
//! `lattice ledger show [--dir DIR]` lists the runs recorded in a results ledger.
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

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
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
        _ => {
            eprintln!("usage: lattice doctor [--dir DATA_DIR] [--wal NAME]");
            eprintln!("       lattice ledger show [--dir DIR]");
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

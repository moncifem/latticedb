//! `cargo xtask bench`: measures, compares with the stored baseline, and gates.
//!
//! `bench-runner [--profile NAME] [--tolerance F] [--write-baseline] [--scale N] [--ledger DIR]`
#![allow(clippy::print_stdout, clippy::print_stderr, clippy::disallowed_methods)]

use lattice_bench::{compare, parse_results, render_results, suite};
use lattice_seams::{OsRng, StdIo, SystemClock};
use std::process::ExitCode;

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let profile = arg(&args, "--profile").unwrap_or_else(|| "local".into());
    let tolerance: f64 = arg(&args, "--tolerance")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0.25);
    let scale: u64 = arg(&args, "--scale")
        .and_then(|v| v.parse().ok())
        .unwrap_or(1);
    let dir = std::path::Path::new("benches/baselines").join(&profile);
    let file = dir.join("baseline.json");

    let current = suite::run_all(scale);
    for (name, ns) in &current {
        println!("{name:<36} {ns:>12.1} ns/op");
    }
    if args.iter().any(|a| a == "--write-baseline") {
        if let Err(e) = std::fs::create_dir_all(&dir)
            .and_then(|()| std::fs::write(&file, render_results(&current)))
        {
            eprintln!("cannot write {}: {e}", file.display());
            return ExitCode::from(2);
        }
        println!("baseline written to {}", file.display());
        return ExitCode::SUCCESS;
    }
    let baseline = match std::fs::read_to_string(&file)
        .map_err(|e| e.to_string())
        .and_then(|t| parse_results(&t))
    {
        Ok(b) => b,
        Err(e) => {
            eprintln!(
                "no usable baseline for profile `{profile}` ({e}). Record one with --write-baseline."
            );
            return ExitCode::from(2);
        }
    };
    // Record only after comparison. Legacy fields remain readable; old records without a
    // verdict are measurements, never evidence that the comparison passed.
    let comparison = compare(&baseline, &current, tolerance);
    if let Some(ledger) = arg(&args, "--ledger") {
        let verdict = match &comparison {
            Ok(regressions) if regressions.is_empty() => "passed",
            Ok(_) => "failed",
            Err(_) => "error",
        };
        let deltas: std::collections::BTreeMap<_, _> =
            baseline
                .iter()
                .filter_map(|(name, base)| {
                    current.get(name).map(|now| (name, serde_json::json!({
                "baseline_ns": base, "current_ns": now, "slowdown": (now / base) - 1.0
            })))
                })
                .collect();
        let json = serde_json::json!({
            "profile": profile, "scale": scale, "ns_per_op": current,
            "tolerance": tolerance, "verdict": verdict, "deltas": deltas
        })
        .to_string();
        let recorded = std::fs::create_dir_all(&ledger)
            .map_err(|e| e.to_string())
            .and_then(|()| {
                lattice_core::ledger::append(StdIo::new(ledger), SystemClock, OsRng, "bench", &json)
                    .map(|_| ())
                    .map_err(|e| format!("{e:?}"))
            });
        if let Err(e) = recorded {
            eprintln!("could not record the run in the ledger: {e}");
            return ExitCode::from(2);
        }
    }
    match comparison {
        Ok(regs) if regs.is_empty() => {
            println!(
                "no regression beyond {:.0} percent against profile `{profile}`",
                tolerance * 100.0
            );
            ExitCode::SUCCESS
        }
        Ok(regs) => {
            for r in regs {
                println!(
                    "REGRESSION {}: {:.1} -> {:.1} ns/op ({:+.0} percent)",
                    r.name,
                    r.baseline_ns,
                    r.current_ns,
                    r.slowdown() * 100.0
                );
            }
            ExitCode::from(1)
        }
        Err(e) => {
            eprintln!("gate error: {e}");
            ExitCode::from(2)
        }
    }
}

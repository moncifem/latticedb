//! `cargo xtask bench`: measures, compares with the stored baseline, and gates.
//!
//! `bench-runner [--profile NAME] [--tolerance F] [--write-baseline] [--scale N]`
#![allow(clippy::print_stdout, clippy::print_stderr, clippy::disallowed_methods)]

use lattice_bench::{compare, parse_results, render_results, suite};
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
            println!(
                "no usable baseline for profile `{profile}` ({e}); nothing to compare. Record one with --write-baseline."
            );
            return ExitCode::SUCCESS;
        }
    };
    match compare(&baseline, &current, tolerance) {
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

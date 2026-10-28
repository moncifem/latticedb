//! `cargo xtask sim` front end: runs seeded crash-and-recover scenarios and reports violations.
//!
//! Usage: `sim-runner [--seeds N] [--steps N] [--from SEED] [--seed SEED --profile NAME] [--ledger DIR]`
//!
//! `--ledger` appends the run summary to the results ledger in DIR, which must already exist.
#![allow(clippy::print_stdout, clippy::print_stderr)]

use lattice_seams::{OsRng, StdIo, SystemClock};
use lattice_testkit::scenario::{profiles, run};
use std::process::ExitCode;

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let seeds: u64 = arg(&args, "--seeds")
        .and_then(|v| v.parse().ok())
        .unwrap_or(1000);
    let steps: usize = arg(&args, "--steps")
        .and_then(|v| v.parse().ok())
        .unwrap_or(150);
    let from: u64 = arg(&args, "--from")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let only_profile = arg(&args, "--profile");
    let ledger = arg(&args, "--ledger");
    let single = arg(&args, "--seed").and_then(|v| v.parse::<u64>().ok());

    let range: Vec<u64> = match single {
        Some(s) => vec![s],
        None => (from..from + seeds).collect(),
    };
    let (mut runs, mut failures, mut cut_short, mut incomplete, mut cuts, mut recoveries) =
        (0u64, 0u64, 0u64, 0u64, 0u64, 0u64);
    for (name, profile) in profiles() {
        if only_profile.as_deref().is_some_and(|p| p != name) {
            continue;
        }
        let (mut p_fail, mut p_cut, mut p_inc) = (0u64, 0u64, 0u64);
        for &seed in &range {
            let out = run(seed, profile, steps, 2048);
            runs += 1;
            cuts += out.power_cuts;
            recoveries += u64::from(out.recoveries);
            p_cut += u64::from(out.cut_short);
            p_inc += u64::from(out.incomplete);
            if !out.violations.is_empty() {
                p_fail += 1;
                println!(
                    "VIOLATION profile={name} seed={seed}: {:?}",
                    out.violations.first()
                );
                println!("  reproduce: cargo xtask sim --seed {seed} --profile {name}");
            }
        }
        failures += p_fail;
        cut_short += p_cut;
        incomplete += p_inc;
        println!(
            "profile {name}: {} seeds, {p_fail} failing, {p_cut} cut short (still verified), {p_inc} incomplete (not verified)",
            range.len()
        );
    }
    println!(
        "total: {runs} runs, {failures} failing, {cut_short} cut short, {incomplete} incomplete; \
         {recoveries} recoveries, {cuts} power cuts inside durability protocols"
    );
    if let Some(dir) = ledger {
        let json = format!(
            "{{\"runs\":{runs},\"failing\":{failures},\"cut_short\":{cut_short},\"incomplete\":{incomplete},\"recoveries\":{recoveries},\"power_cuts\":{cuts},\"seeds\":{},\"steps\":{steps}}}",
            range.len()
        );
        if let Err(e) =
            lattice_core::ledger::append(StdIo::new(dir), SystemClock, OsRng, "sim", &json)
        {
            eprintln!("could not record the run in the ledger: {e:?}");
            return ExitCode::from(2);
        }
    }
    if failures == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

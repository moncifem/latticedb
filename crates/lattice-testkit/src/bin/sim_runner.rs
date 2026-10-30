//! `cargo xtask sim` front end: runs seeded crash-and-recover scenarios and reports violations.
//!
//! Usage: `sim-runner [--seeds N] [--steps N] [--from SEED] [--seed SEED --profile NAME] [--ledger DIR]`
//!
//! `--ledger` appends the run summary to the results ledger in DIR, which must already exist.
//! The argument handling and the report live in `lattice_testkit::sim_cli`, where they are tested.
#![allow(clippy::print_stdout, clippy::print_stderr)]

use lattice_seams::{OsRng, StdIo, SystemClock};
use lattice_testkit::sim_cli::execute_with;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (code, ledger) = execute_with(&args, &mut |to_stderr, line| {
        if to_stderr {
            eprintln!("{line}");
        } else {
            println!("{line}");
        }
    });
    if code <= 1
        && let Some((dir, json)) = ledger
        && let Err(e) =
            lattice_core::ledger::append(StdIo::new(dir), SystemClock, OsRng, "sim", &json)
    {
        eprintln!("could not record the run in the ledger: {e:?}");
        return ExitCode::from(2);
    }
    ExitCode::from(code)
}

//! The benchmark runner as a program: exit codes decide whether a pull request passes, so they are
//! tested through the real binary.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::disallowed_methods,
    clippy::disallowed_types
)]

use lattice_bench::{compare, render_results};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn workdir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("lattice-bench-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn run(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_bench_runner"))
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap()
}

fn write_baseline(dir: &Path, profile: &str, text: &str) {
    let p = dir.join("benches/baselines").join(profile);
    std::fs::create_dir_all(&p).unwrap();
    std::fs::write(p.join("baseline.json"), text).unwrap();
}

fn code(o: &Output) -> i32 {
    o.status.code().unwrap()
}

#[test]
fn a_missing_baseline_fails_and_says_how_to_record_one() {
    let dir = workdir("missing");
    let o = run(&dir, &["--scale", "1", "--profile", "p"]);
    assert_eq!(code(&o), 2);
    assert!(String::from_utf8_lossy(&o.stderr).contains("--write-baseline"));
}

#[test]
fn writing_a_baseline_then_comparing_against_it_passes_with_room() {
    let dir = workdir("write");
    let o = run(
        &dir,
        &["--scale", "1", "--profile", "p", "--write-baseline"],
    );
    assert_eq!(code(&o), 0, "{}", String::from_utf8_lossy(&o.stderr));
    assert!(dir.join("benches/baselines/p/baseline.json").exists());
    let o = run(
        &dir,
        &["--scale", "1", "--profile", "p", "--tolerance", "1000"],
    );
    assert_eq!(code(&o), 0);
    assert!(String::from_utf8_lossy(&o.stdout).contains("no regression beyond 100000 percent"));
}

#[test]
fn a_slowdown_beyond_the_baseline_fails_and_names_the_benchmark() {
    let dir = workdir("slow");
    write_baseline(&dir, "p", "{\n  \"hlc_tick\": 0.001\n}\n");
    let o = run(&dir, &["--scale", "1", "--profile", "p"]);
    assert_eq!(code(&o), 1);
    assert!(String::from_utf8_lossy(&o.stdout).contains("REGRESSION hlc_tick"));
}

#[test]
fn a_faster_run_passes() {
    let dir = workdir("fast");
    write_baseline(&dir, "p", "{\n  \"hlc_tick\": 1000000000.0\n}\n");
    let o = run(&dir, &["--scale", "1", "--profile", "p"]);
    assert_eq!(code(&o), 0);
    assert!(String::from_utf8_lossy(&o.stdout).contains("no regression"));
}

#[test]
fn a_benchmark_in_the_baseline_that_no_longer_runs_is_a_gate_error() {
    let dir = workdir("vanished");
    write_baseline(&dir, "p", "{\n  \"no_such_benchmark\": 1.0\n}\n");
    let o = run(&dir, &["--scale", "1", "--profile", "p"]);
    assert_eq!(code(&o), 2);
}

#[test]
fn flags_are_read_by_name_and_the_ledger_records_the_run() {
    let dir = workdir("ledger");
    let ledger = dir.join("ledger");
    write_baseline(&dir, "q", "{\n  \"hlc_tick\": 1000000000.0\n}\n");
    let o = run(
        &dir,
        &[
            "--scale",
            "1",
            "--profile",
            "q",
            "--ledger",
            ledger.to_str().unwrap(),
        ],
    );
    assert_eq!(code(&o), 0, "{}", String::from_utf8_lossy(&o.stderr));
    assert!(ledger.exists());
    // The profile flag chose `q`: a `p` baseline does not exist, so asking for it fails.
    assert_eq!(code(&run(&dir, &["--scale", "1", "--profile", "p"])), 2);
}

#[test]
fn the_tolerance_boundary_is_inclusive() {
    let base = BTreeMap::from([("a".to_string(), 100.0)]);
    let cur = BTreeMap::from([("a".to_string(), 125.0)]);
    assert!(compare(&base, &cur, 0.25).unwrap().is_empty());
    assert_eq!(compare(&base, &cur, 0.24).unwrap().len(), 1);
    assert!(render_results(&cur).contains("125.0"));
}

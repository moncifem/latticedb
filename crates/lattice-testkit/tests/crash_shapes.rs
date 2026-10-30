//! The `non-prefix` fault profile (issue 442): power loss that persists unsynced pages out of order
//! or zero-fills a tail. Recovery may refuse such a log (`formats/wal-v1.md` keeps that on purpose,
//! it can reject a repairable log but never silently drop data); the scenario then applies the
//! documented operator procedure and goes on, so a refusal never ends a run unverified, and the
//! oracle still demands every acknowledged record afterwards.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use lattice_core::bugs::{self, Bug};
use lattice_testkit::scenario::{profiles, run, shape_profiles};

const SEEDS: u64 = 300;

fn profile() -> lattice_sim::FaultProfile {
    shape_profiles()
        .iter()
        .find(|(name, _)| *name == "non-prefix")
        .expect("the non-prefix profile is registered")
        .1
}

#[test]
fn the_clean_build_trips_no_checker_under_non_prefix_power_loss() {
    bugs::clear();
    let (mut repairs, mut incomplete, mut recoveries) = (0u64, 0u64, 0u64);
    for seed in 0..SEEDS {
        for segment_limit in [512, 2048] {
            let out = run(seed, profile(), 150, segment_limit);
            assert!(
                out.violations.is_empty(),
                "seed {seed} limit {segment_limit}: {:?}",
                out.violations
            );
            repairs += u64::from(out.repairs);
            incomplete += u64::from(out.incomplete);
            recoveries += u64::from(out.recoveries);
        }
    }
    // The profile must reach the refusal path (36 repairs observed over these seeds, deterministic), and a refusal must not censor the run.
    assert!(
        repairs >= 30,
        "refusals repaired by the procedure: {repairs}"
    );
    assert_eq!(incomplete, 0, "runs that could not be verified");
    assert!(recoveries > 1000, "recoveries: {recoveries}");
}

#[test]
fn the_standard_profiles_never_refuse_into_the_operator_procedure() {
    for seed in 0..SEEDS {
        for (name, p) in profiles() {
            let out = run(seed, p, 150, 512);
            assert_eq!(out.repairs, 0, "profile {name} seed {seed}");
        }
    }
}

#[test]
fn the_planted_zero_tail_bug_is_caught_only_through_the_non_prefix_profile() {
    bugs::enable(Bug::ZeroFilledTailKept);
    let mut found = None;
    for seed in 0..SEEDS * 4 {
        let out = run(seed, profile(), 150, 512);
        if let Some(v) = out.violations.first() {
            found = Some((seed, v.invariant));
            break;
        }
    }
    let mut missed_by_old = None;
    for seed in 0..SEEDS {
        for (name, p) in profiles() {
            let out = run(seed, p, 150, 512);
            if !out.violations.is_empty() {
                missed_by_old = Some((name, seed));
            }
        }
    }
    bugs::clear();
    let (seed, invariant) = found.expect("the non-prefix profile must catch ZeroFilledTailKept");
    eprintln!("ZeroFilledTailKept caught by {invariant} at seed {seed}");
    assert!(
        matches!(invariant, "I1" | "I2" | "I9" | "RECOVERY"),
        "{invariant}"
    );
    assert_eq!(
        missed_by_old, None,
        "the standard profiles already catch it, so it is not a witness for the new shapes"
    );
}

#[test]
fn the_command_line_selects_the_shape_profile_by_name_and_leaves_the_default_sweep_alone() {
    let args = |extra: &[&str]| -> Vec<String> {
        ["--seeds", "3", "--steps", "40"]
            .into_iter()
            .chain(extra.iter().copied())
            .map(String::from)
            .collect()
    };
    let named = lattice_testkit::sim_cli::execute(&args(&["--profile", "non-prefix"]));
    assert_eq!(named.code, 0, "{:?}", named.stdout);
    let lines: Vec<&String> = named.stdout.iter().chain(&named.stderr).collect();
    assert_eq!(
        lines
            .iter()
            .filter(|l| l.starts_with("profile non-prefix: 3 seeds, 0 failing"))
            .count(),
        1,
        "{lines:?}"
    );
    let default = lattice_testkit::sim_cli::execute(&args(&[]));
    assert!(
        !default
            .stdout
            .iter()
            .chain(&default.stderr)
            .any(|l| l.contains("non-prefix")),
        "the default sweep must keep its five profiles"
    );
    let unknown = lattice_testkit::sim_cli::execute(&args(&["--profile", "no-such"]));
    assert_eq!(unknown.code, 2);
}

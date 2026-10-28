//! Testing the tests: every planted defect must be caught by the simulator, under the invariant
//! we expect, within a bounded number of seeds. The clean build must never trip a checker.

use lattice_core::bugs::{self, Bug};
use lattice_testkit::scenario::{profiles, run};

/// Invariants (or `RECOVERY`) that count as a correct detection of each defect.
fn expected(bug: Bug) -> &'static [&'static str] {
    match bug {
        Bug::SkipFsync
        | Bug::AckBeforeSync
        | Bug::RecoveryDropsLastRecord
        | Bug::CrcSkipsPayload => &["I1"],
        Bug::DedupeNotRebuilt => &["I6"],
        // The in-flight check turns a duplicate in one batch into a failed commit, so the scrub (I9)
        // finds the duplicated id in the log before the acknowledgement check (I6) sees anything.
        Bug::NoDedupe => &["I9"],
        Bug::HlcRegress | Bug::HlcCounterStuck => &["I4"],
        Bug::CheckpointIgnoresInFlight => &["I9", "RECOVERY"],
        Bug::AcceptTornTail | Bug::LsnReusedAfterRecovery | Bug::RotateWithoutDirSync => {
            &["I1", "I2", "I9", "RECOVERY"]
        }
        Bug::FenceIgnored | Bug::FenceNotRebuilt => &["I12"],
        Bug::NoParentDirSync => &["I1", "I2", "RECOVERY"],
        Bug::SkipRecoverySync => &["I1"],
        Bug::FsyncFailureNotPoisoning
        | Bug::FsyncErrorSwallowed
        | Bug::RotateHeaderNotSynced
        | Bug::DurableLsnOvershoot => &["I1", "I2", "I9", "RECOVERY"],
        Bug::BatchDuplicateAckedEarly => &["I6"],
        _ => &[],
    }
}

const SEED_BUDGET: u64 = 400;

fn detect(bug: Bug) -> Option<(String, u64, &'static str)> {
    bugs::enable(bug);
    let mut found = None;
    'outer: for seed in 0..SEED_BUDGET {
        for (name, profile) in profiles() {
            let out = run(seed, profile, 150, 512);
            if let Some(v) = out.violations.first() {
                found = Some((name.to_string(), seed, v.invariant));
                break 'outer;
            }
        }
    }
    bugs::clear();
    found
}

#[test]
fn every_planted_bug_is_detected_by_the_expected_invariant() {
    let mut report = Vec::new();
    let mut missed = Vec::new();
    for bug in Bug::ALL {
        match detect(bug) {
            Some((profile, seed, inv)) => {
                let ok = expected(bug).contains(&inv);
                report.push(format!(
                    "{bug:?}: caught by {inv} at profile {profile} seed {seed}{}",
                    if ok { "" } else { " (UNEXPECTED invariant)" }
                ));
                if !ok {
                    missed.push(format!(
                        "{bug:?} caught by {inv}, expected {:?}",
                        expected(bug)
                    ));
                }
            }
            None => {
                report.push(format!("{bug:?}: NOT DETECTED within {SEED_BUDGET} seeds"));
                missed.push(format!("{bug:?} not detected"));
            }
        }
    }
    for line in &report {
        eprintln!("{line}");
    }
    assert!(missed.is_empty(), "detection gaps: {missed:?}");
}

#[test]
fn the_clean_build_trips_no_checker_over_the_same_budget() {
    bugs::clear();
    for seed in 0..SEED_BUDGET {
        for (name, profile) in profiles() {
            let out = run(seed, profile, 150, 512);
            assert!(
                out.violations.is_empty(),
                "false alarm: profile {name} seed {seed}: {:?}",
                out.violations
            );
        }
    }
}

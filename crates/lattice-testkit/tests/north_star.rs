//! North-star scenarios from docs/spec/direction.md. NS1 runs today. NS2 to NS8 are executable
//! specifications marked pending with the phase that makes them pass; `cargo xtask scorecard`
//! lists them.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use lattice_testkit::scenario::{profiles, run};

/// NS1: kill the process during a burst of mixed writes, restart. Every acknowledged write is
/// present, no change set is partial, scrubbers are clean. Here at the log level, under every
/// fault profile; all engines join in phase 2.
#[test]
fn ns1_crash_safety_at_the_log_level() {
    for (name, profile) in profiles() {
        let mut unverified = 0u32;
        for seed in 0..40 {
            let out = run(seed, profile, 120, 2048);
            assert!(
                out.violations.is_empty(),
                "profile {name} seed {seed}: {:?}",
                out.violations.first()
            );
            unverified += u32::from(out.incomplete);
        }
        // Per profile, so one profile cannot hide behind the others.
        assert!(
            unverified * 2 < 40,
            "profile {name}: {unverified} of 40 runs were not verified"
        );
    }
}

#[test]
#[ignore = "phase 7: history question answered exactly from the log, permissions respected"]
fn ns2_history_question() {
    panic!("pending");
}

#[test]
#[ignore = "phase 8: drift detected, proposal on a branch, accepted, claim re-verified"]
fn ns3_drift_fixed() {
    panic!("pending");
}

#[test]
#[ignore = "phase 8: read-only answer cites permitted sources and the agent writes nothing"]
fn ns4_read_only_answer() {
    panic!("pending");
}

#[test]
#[ignore = "phase 8: writes, forbidden reads and injected escalation are blocked and audited"]
fn ns5_agent_cannot_exceed_its_grant() {
    panic!("pending");
}

#[test]
#[ignore = "phase 9: requirement to deployment traceability is one query"]
fn ns6_team_in_one_tool() {
    panic!("pending");
}

#[test]
#[ignore = "phase 3: two offline replicas converge, semantic conflicts surface as objects"]
fn ns7_offline_then_merge() {
    panic!("pending");
}

#[test]
#[ignore = "phase 6: backlog, docs and decisions live in Lattice"]
fn ns8_built_with_lattice() {
    panic!("pending");
}

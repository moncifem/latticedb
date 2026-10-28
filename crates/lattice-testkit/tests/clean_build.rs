//! The unmodified kernel must satisfy every invariant under every fault profile, a run must be
//! exactly reproducible from its seed, and the workload must really reach the interesting places.

use lattice_testkit::scenario::{profiles, run};

#[test]
fn no_violations_across_all_profiles() {
    for (name, profile) in profiles() {
        for seed in 0..300 {
            let out = run(seed, profile, 120, 1024);
            assert!(
                out.violations.is_empty(),
                "profile {name} seed {seed}: {:?}",
                out.violations
            );
        }
    }
}

#[test]
fn inv_i1_acked_commits_survive_crashes_and_mid_protocol_power_cuts() {
    let (mut acked, mut recoveries, mut cuts) = (0, 0, 0);
    for seed in 0..200 {
        let out = run(seed, profiles()[1].1, 150, 1024);
        assert!(
            out.violations.is_empty(),
            "seed {seed}: {:?}",
            out.violations
        );
        acked += out.acked;
        recoveries += out.recoveries;
        cuts += out.power_cuts;
    }
    assert!(acked > 1000, "workload too small to mean anything: {acked}");
    assert!(recoveries > 500, "too few crashes exercised: {recoveries}");
    assert!(
        cuts > 100,
        "too few power cuts inside durability protocols: {cuts}"
    );
}

/// Coverage quota: cutting runs short must not quietly hollow out the verification. Profiles
/// without bit rot must always reach their terminal check, and even with injected faults most runs
/// must complete their workload.
#[test]
fn coverage_quotas_hold_and_censored_runs_are_counted() {
    for (name, profile) in profiles() {
        let (mut incomplete, mut cut_short) = (0, 0);
        let runs = 300;
        for seed in 0..runs {
            let out = run(seed, profile, 120, 1024);
            incomplete += usize::from(out.incomplete);
            cut_short += usize::from(out.cut_short);
        }
        if profile.bit_rot_ppm == 0 {
            assert_eq!(
                incomplete, 0,
                "profile {name}: runs without bit rot must always be verified"
            );
        } else {
            assert!(
                incomplete * 4 < runs as usize,
                "profile {name}: {incomplete} of {runs} runs were censored"
            );
        }
        assert!(
            cut_short * 4 < runs as usize,
            "profile {name}: {cut_short} of {runs} workloads were cut short"
        );
    }
}

#[test]
fn determinism_same_seed_same_trace() {
    for (name, profile) in profiles() {
        for seed in 0..60 {
            let a = run(seed, profile, 100, 1024);
            let b = run(seed, profile, 100, 1024);
            assert_eq!(
                a.trace_hash, b.trace_hash,
                "profile {name} seed {seed} is not deterministic"
            );
            assert_eq!(
                (
                    a.acked,
                    a.recoveries,
                    a.power_cuts,
                    a.cut_short,
                    a.incomplete
                ),
                (
                    b.acked,
                    b.recoveries,
                    b.power_cuts,
                    b.cut_short,
                    b.incomplete
                )
            );
        }
    }
    // And different seeds really do explore different histories.
    let hashes: std::collections::BTreeSet<u64> = (0..40)
        .map(|s| run(s, profiles()[1].1, 100, 1024).trace_hash)
        .collect();
    assert!(hashes.len() > 30, "seeds barely differ: {}", hashes.len());
}

/// The gate's volume: 10,000 seeds per profile, each run twice, with identical traces. Slow, so it
/// runs nightly in release (`cargo test -p lattice-testkit --release -- --ignored`).
#[test]
#[ignore = "volume: 10,000 seeds per profile; run nightly with --ignored"]
fn determinism_at_gate_volume() {
    for (name, profile) in profiles() {
        for seed in 0..10_000 {
            let a = run(seed, profile, 100, 1024);
            let b = run(seed, profile, 100, 1024);
            assert_eq!(
                a.trace_hash, b.trace_hash,
                "profile {name} seed {seed} is not deterministic"
            );
        }
    }
}

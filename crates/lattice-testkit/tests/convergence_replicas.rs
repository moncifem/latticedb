//! N-replica convergence over a simulated network, checked Jepsen-style (WP45): identical digests
//! after quiescence (I5), no lost acknowledged write, an acyclic tree. Planted defects in the
//! harness replicas prove the checker can fail.
#![allow(clippy::print_stdout)]

use lattice_testkit::convergence::{
    Kind, LoseLocal, LoseRemote, NoDedup, OverwriteMerge, RegReplica, Run, SetReplica,
    SkipCycleCheck, Sound, TreeReplica, run_family, run_sound, sweep,
};
use lattice_testkit::scenario::profiles;
use std::collections::BTreeMap;

fn summary(label: &str, seeds: u64, run: &Run) {
    println!(
        "convergence {label}: seeds={seeds} sims={} writes={} delivered={} dropped={} \
         duplicated={} replicas={}..={} violations={}",
        run.sims,
        run.writes,
        run.delivered,
        run.dropped,
        run.duplicated,
        run.replicas_min,
        run.replicas_max,
        run.violations.len()
    );
}

#[test]
fn sound_replicas_converge_in_200_seeds_over_all_profiles() {
    let run = sweep(0..200, run_sound);
    summary("sound", 200, &run);
    assert_eq!(run.sims, 200 * 5 * 3);
    if let Some(v) = run.violations.first() {
        panic!("violation: {v:?}");
    }
    // The network really was hostile and the replica counts really varied.
    assert!(run.dropped > 0 && run.duplicated > 0 && run.writes > 1000);
    assert_eq!((run.replicas_min, run.replicas_max), (2, 5));
}

#[test]
fn every_profile_is_exercised_and_clean_has_no_loss_or_duplication() {
    for (name, _) in profiles() {
        let run = sweep(0..40, |s, p, n| {
            if p == name {
                run_sound(s, p, n)
            } else {
                Run::default()
            }
        });
        println!(
            "convergence profile {name}: sims={} dropped={} duplicated={} violations={}",
            run.sims,
            run.dropped,
            run.duplicated,
            run.violations.len()
        );
        assert_eq!(run.sims, 40 * 3, "{name}");
        assert!(run.violations.is_empty(), "{name}");
        if name == "clean" {
            assert_eq!((run.dropped, run.duplicated), (0, 0));
        } else {
            assert!(run.dropped + run.duplicated > 0, "{name}");
        }
    }
}

#[test]
fn runs_replay_exactly_and_seeds_differ() {
    for seed in 0..30 {
        for (i, (name, _)) in profiles().iter().enumerate() {
            let n = lattice_testkit::convergence::replicas_for(seed, i);
            assert_eq!(
                run_sound(seed, name, n).trace,
                run_sound(seed, name, n).trace,
                "seed {seed} profile {name}"
            );
        }
    }
    let traces: std::collections::BTreeSet<u64> =
        (0..40).map(|s| run_sound(s, "hostile", 3).trace).collect();
    assert!(traces.len() > 35);
}

fn planted(name: &str, seeds: u64, run: &Run, expect: Kind) {
    summary(&format!("planted {name}"), seeds, run);
    let mut by_kind: BTreeMap<Kind, usize> = BTreeMap::new();
    for v in &run.violations {
        *by_kind.entry(v.kind).or_default() += 1;
    }
    println!("convergence planted {name}: by kind {by_kind:?}");
    assert!(
        by_kind.get(&expect).copied().unwrap_or(0) > 0,
        "{name}: checker missed {expect:?}"
    );
}

#[test]
fn checker_detects_every_planted_bug() {
    let s = 200;
    let r = sweep(0..s, run_family::<RegReplica<OverwriteMerge>>);
    planted("register overwrite-merge", s, &r, Kind::Divergence);
    let r = sweep(0..s, run_family::<RegReplica<LoseLocal>>);
    planted("register lost write", s, &r, Kind::LostWrite);
    let r = sweep(0..s, run_family::<SetReplica<OverwriteMerge>>);
    planted("set overwrite-merge", s, &r, Kind::Divergence);
    let r = sweep(0..s, run_family::<SetReplica<LoseLocal>>);
    planted("set lost add", s, &r, Kind::LostWrite);
    let r = sweep(0..s, run_family::<TreeReplica<NoDedup>>);
    planted("tree no duplicate detection", s, &r, Kind::Divergence);
    let r = sweep(0..s, run_family::<TreeReplica<LoseRemote>>);
    planted("tree lost operation", s, &r, Kind::LostWrite);
    let r = sweep(0..s, run_family::<TreeReplica<SkipCycleCheck>>);
    planted("tree cycle check skipped", s, &r, Kind::Cycle);
}

#[test]
fn sound_replicas_pass_where_the_planted_ones_fail() {
    // Same seeds and profiles, no defect: no violation, so detection is not a false alarm.
    let r = sweep(0..100, run_family::<TreeReplica<Sound>>);
    assert!(r.violations.is_empty());
    let r = sweep(0..100, run_family::<TreeReplica<SkipCycleCheck>>);
    assert!(!r.violations.is_empty());
}

/// Volume run: 100,000 seeds across all profiles and families. Left to the nightly job.
#[test]
#[ignore = "volume: 100,000 seeds per profile; run nightly with --ignored"]
fn convergence_at_gate_volume() {
    let run = sweep(0..100_000, run_sound);
    summary("volume", 100_000, &run);
    assert!(run.violations.is_empty(), "{:?}", run.violations.first());
}

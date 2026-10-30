//! Recorded convergence runs. The simulator is deterministic, so a run's trace hash and counters are a
//! fingerprint of every random draw and every message it makes; mutation testing showed that arithmetic
//! inside the network model, the retransmission loop and the hash mixing was not pinned by the property
//! tests, which only look at whether replicas agree.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use lattice_testkit::convergence::{NetModel, replicas_for, run_sound, sweep};
use lattice_testkit::scenario::profiles;

/// profile, seed, replicas, trace, sims, writes, dropped, duplicated, delivered
type Recorded = (&'static str, u64, usize, u64, u64, u64, u64, u64, u64);

#[rustfmt::skip]
const RUNS: [Recorded; 15] = [
    ("clean", 1, 3, 0x26928ade0491381d, 3, 132, 0, 0, 264),
    ("clean", 2, 4, 0xfac29265a7433460, 3, 167, 0, 0, 501),
    ("clean", 11, 5, 0xfd75fa50ca5930a9, 3, 200, 0, 0, 800),
    ("crash-heavy", 1, 4, 0x12a2f3f7ab0cb013, 3, 162, 0, 105, 591),
    ("crash-heavy", 2, 5, 0xd400bcbe7ba19e41, 3, 203, 0, 112, 924),
    ("crash-heavy", 11, 2, 0x834a4b249e3809a5, 3, 85, 0, 8, 93),
    ("disk-faulty", 1, 5, 0x11727008f51c55b9, 3, 234, 239, 83, 1019),
    ("disk-faulty", 2, 2, 0x4b1a744696839450, 3, 84, 13, 10, 94),
    ("disk-faulty", 11, 3, 0x9735f7913e090129, 3, 132, 74, 25, 289),
    ("hostile", 1, 2, 0x1d0bf3e4ceb4ab3e, 3, 86, 36, 26, 112),
    ("hostile", 2, 3, 0xfa9640f55fd79c0b, 3, 118, 125, 68, 304),
    ("hostile", 11, 4, 0x15842515958ca022, 3, 159, 261, 113, 590),
    ("everything", 1, 3, 0x42ae655e11f93d64, 3, 138, 276, 107, 383),
    ("everything", 2, 4, 0x2f0d96bc6f3456c9, 3, 172, 449, 172, 688),
    ("everything", 11, 5, 0x2a61bfc43225a8c7, 3, 206, 697, 303, 1127),
];

/// profile, seed, delay_max, drop_ppm, dup_ppm, retransmit_after
#[rustfmt::skip]
const NETS: [(&str, u64, u64, u32, u32, u64); 15] = [
    ("clean", 0, 2, 0, 0, 3),
    ("clean", 5, 2, 0, 0, 3),
    ("clean", 99, 2, 0, 0, 3),
    ("crash-heavy", 0, 23, 0, 177405, 5),
    ("crash-heavy", 5, 22, 0, 151926, 5),
    ("crash-heavy", 99, 22, 0, 162308, 5),
    ("disk-faulty", 0, 9, 215454, 68910, 8),
    ("disk-faulty", 5, 7, 204287, 66384, 8),
    ("disk-faulty", 99, 9, 219229, 85772, 8),
    ("hostile", 0, 33, 338793, 253165, 8),
    ("hostile", 5, 33, 323719, 276549, 8),
    ("hostile", 99, 32, 328794, 267690, 8),
    ("everything", 0, 53, 459672, 358384, 12),
    ("everything", 5, 51, 474598, 350037, 12),
    ("everything", 99, 53, 451331, 352333, 12),
];

#[test]
fn recorded_runs_reproduce_exactly() {
    for (profile, seed, n, trace, sims, writes, dropped, duplicated, delivered) in RUNS {
        let index = profiles()
            .iter()
            .position(|(name, _)| *name == profile)
            .unwrap();
        assert_eq!(replicas_for(seed, index), n, "{profile} seed {seed}");
        let name = profiles()[index].0;
        let run = run_sound(seed, name, n);
        assert!(
            run.violations.is_empty(),
            "{profile} seed {seed}: {:?}",
            run.violations
        );
        let got = (
            run.trace,
            run.sims,
            run.writes,
            run.dropped,
            run.duplicated,
            run.delivered,
        );
        assert_eq!(
            got,
            (trace, sims, writes, dropped, duplicated, delivered),
            "{profile} seed {seed}"
        );
        assert_eq!((run.replicas_min, run.replicas_max), (n, n));
    }
}

#[test]
fn the_network_model_depends_on_profile_and_seed_as_recorded() {
    for (profile, seed, delay_max, drop_ppm, dup_ppm, retransmit_after) in NETS {
        let m = NetModel::for_profile(profile, seed);
        assert_eq!(
            (m.delay_max, m.drop_ppm, m.dup_ppm, m.retransmit_after),
            (delay_max, drop_ppm, dup_ppm, retransmit_after),
            "{profile} seed {seed}"
        );
    }
}

#[test]
fn a_sweep_sums_its_runs_as_recorded() {
    let total = sweep(0..4, run_sound);
    assert!(total.violations.is_empty());
    assert_eq!(
        (
            total.trace,
            total.sims,
            total.writes,
            total.dropped,
            total.duplicated,
            total.delivered
        ),
        (0xcb7d4cdcd2df31, 60, 2970, 2879, 1489, 10016)
    );
    assert_eq!((total.replicas_min, total.replicas_max), (2, 5));
}

//! Recorded scenario runs. The scenario runner is deterministic, so a run's trace hash and counters
//! fingerprint every operation, fault and recovery it makes; mutation testing showed that the lifecycle
//! steps (stale writes, clock skew, turn handling) and the oracle bookkeeping were not pinned by tests
//! that only look at whether a run passed.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use lattice_testkit::scenario::{profiles, run};

/// profile, seed, trace hash, recoveries, acked, power cuts, no-space events, cut short, incomplete
type Recorded = (&'static str, u64, u64, u32, usize, u64, u64, bool, bool);

#[rustfmt::skip]
const RUNS: [Recorded; 25] = [
    ("clean", 1, 0x4cb371901ae37410, 34, 200, 17, 0, false, false),
    ("clean", 2, 0x621d0a471bf64990, 27, 166, 16, 0, false, false),
    ("clean", 3, 0x934505fc90b5e0f5, 39, 141, 24, 0, false, false),
    ("clean", 6, 0x2b503f889743ca46, 28, 202, 19, 0, false, false),
    ("clean", 40, 0x29c3c0d8c3ac8177, 34, 200, 20, 0, false, false),
    ("crash-heavy", 1, 0xea29e343c73135af, 33, 220, 18, 0, false, false),
    ("crash-heavy", 2, 0x7fce54051b98afd2, 25, 178, 17, 0, false, false),
    ("crash-heavy", 3, 0x24db6902da2c0205, 33, 198, 21, 0, false, false),
    ("crash-heavy", 6, 0xb126ca5694f3363c, 28, 202, 19, 0, false, false),
    ("crash-heavy", 40, 0xb45a024b1941390f, 41, 209, 19, 0, false, false),
    ("disk-faulty", 1, 0x756f667bea0ff84a, 32, 194, 16, 0, false, false),
    ("disk-faulty", 2, 0xe4600c8d66531890, 35, 166, 23, 0, false, false),
    ("disk-faulty", 3, 0xe016e3ec452cc02d, 36, 204, 11, 4, false, false),
    ("disk-faulty", 6, 0x25f3f4036870631b, 35, 200, 20, 9, false, false),
    ("disk-faulty", 40, 0xe54ec9cf4cad9010, 36, 190, 15, 0, false, false),
    ("hostile", 1, 0xe91594fc0dbc64ee, 36, 208, 20, 0, false, false),
    ("hostile", 2, 0xaf7e93b864b1948b, 36, 176, 22, 0, false, false),
    ("hostile", 3, 0xa55f586f00978bcf, 37, 175, 15, 0, false, false),
    ("hostile", 6, 0x5a87b11851a1dc88, 31, 211, 25, 0, false, false),
    ("hostile", 40, 0x3dc6122aa6d5dcc8, 43, 139, 16, 0, false, false),
    ("everything", 1, 0x5950a50c9a3c5284, 54, 138, 16, 32, false, false),
    ("everything", 2, 0x2f78a8f8e716716c, 59, 136, 19, 36, false, false),
    ("everything", 3, 0xddd1a6272abebd2a, 63, 141, 30, 20, false, false),
    ("everything", 6, 0xd6ab8bdac8a0e992, 39, 128, 21, 18, false, true),
    ("everything", 40, 0x40b83693c918b90a, 40, 139, 24, 0, false, false),
];

#[test]
fn recorded_scenario_runs_reproduce_exactly() {
    for (name, seed, trace, recoveries, acked, power_cuts, no_space, cut_short, incomplete) in RUNS
    {
        let profile = profiles().iter().find(|(n, _)| *n == name).unwrap().1;
        let o = run(seed, profile, 150, 2048);
        assert!(
            o.violations.is_empty(),
            "{name} seed {seed}: {:?}",
            o.violations
        );
        assert_eq!(
            (
                o.trace_hash,
                o.recoveries,
                o.acked,
                o.power_cuts,
                o.no_space,
                o.cut_short,
                o.incomplete
            ),
            (
                trace, recoveries, acked, power_cuts, no_space, cut_short, incomplete
            ),
            "{name} seed {seed}"
        );
    }
}

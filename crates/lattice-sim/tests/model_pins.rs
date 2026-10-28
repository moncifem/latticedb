//! Pins the simulator's behaviour. Every durability claim in the project is checked against this
//! model of a disk, a clock-free network and a random stream, so a silent change here would weaken
//! all of them; mutation testing found these behaviours unpinned. The golden values are the model's
//! own output and must only change deliberately.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use lattice_seams::{Io, Net, Rng};
use lattice_sim::{FaultProfile, SeededRng, SimIo, SimNet, TraceHash};

#[test]
fn the_profiles_are_what_their_names_say() {
    let c = FaultProfile::clean();
    assert!(!c.torn_writes && !c.garbage_tail && !c.lose_unsynced_dirs);
    assert_eq!(
        (c.sync_error_ppm, c.append_error_ppm, c.bit_rot_ppm),
        (0, 0, 0)
    );
    assert_eq!(c.disk_full_after, None);

    let h = FaultProfile::crash_heavy();
    assert!(h.torn_writes && h.garbage_tail && h.lose_unsynced_dirs);
    assert_eq!(
        (h.sync_error_ppm, h.append_error_ppm, h.bit_rot_ppm),
        (0, 0, 0)
    );

    let d = FaultProfile::disk_faulty();
    assert!(!d.torn_writes && !d.garbage_tail && !d.lose_unsynced_dirs);
    assert_eq!((d.sync_error_ppm, d.append_error_ppm), (20_000, 20_000));
    assert_eq!(d.disk_full_after, Some(24 * 1024));
    assert_eq!(d.bit_rot_ppm, 0);

    let e = FaultProfile::everything();
    assert!(e.torn_writes && e.garbage_tail && e.lose_unsynced_dirs);
    assert_eq!((e.sync_error_ppm, e.append_error_ppm), (15_000, 15_000));
    assert_eq!(e.disk_full_after, Some(16 * 1024));
    assert_eq!(e.bit_rot_ppm, 5_000);

    let t = e.without_transient_faults();
    assert!(t.torn_writes && t.garbage_tail && t.lose_unsynced_dirs);
    assert_eq!(
        (t.sync_error_ppm, t.append_error_ppm, t.bit_rot_ppm),
        (0, 0, 0)
    );
    assert_eq!(t.disk_full_after, None);
}

#[test]
fn counters_and_switches_report_what_happened() {
    let mut io = SimIo::new(1, FaultProfile::clean());
    io.create_dir_all("d").unwrap();
    assert_eq!(io.mutating_ops(), 0);
    io.append("d/a", b"xy").unwrap();
    io.sync("d/a").unwrap();
    io.truncate("d/a", 1).unwrap();
    io.remove("d/a").unwrap();
    assert_eq!(io.mutating_ops(), 4);
    let _ = io.read("d/a");
    assert_eq!(io.mutating_ops(), 4, "reads are not mutating operations");

    assert!(!io.is_powered_off());
    io.arm_power_cut(0);
    assert!(io.append("d/b", b"z").is_err());
    assert!(io.is_powered_off());
    assert_eq!(io.power_cuts(), 1);
    assert!(
        io.append("d/b", b"z").is_err(),
        "everything fails while the power is off"
    );
    assert_eq!(io.power_cuts(), 1, "a dead machine is not cut again");
    io.crash();
    assert!(!io.is_powered_off());
    assert!(io.append("d/b", b"z").is_ok());

    let mut io = SimIo::new(1, FaultProfile::clean());
    io.set_profile(FaultProfile::disk_faulty());
    assert_eq!(io.profile().sync_error_ppm, 20_000);
}

#[test]
fn a_process_restart_keeps_the_page_cache_and_drops_locks_and_the_armed_cut() {
    let mut io = SimIo::new(1, FaultProfile::clean());
    io.create_dir_all("d").unwrap();
    io.append("d/a", b"keep").unwrap();
    io.lock("d/LOCK").unwrap();
    io.arm_power_cut(0);
    io.restart_process();
    assert_eq!(io.read("d/a").unwrap(), b"keep");
    assert!(io.lock("d/LOCK").is_ok(), "the lock was released");
    assert!(
        io.append("d/a", b"!").is_ok(),
        "the armed power cut was cleared"
    );
}

#[test]
fn a_cut_that_is_n_operations_away_fires_on_the_nth() {
    for n in 0..4u64 {
        let mut io = SimIo::new(2, FaultProfile::clean());
        io.create_dir_all("d").unwrap();
        io.arm_power_cut(n);
        for _ in 0..n {
            io.append("d/a", b"x").unwrap();
        }
        assert!(!io.is_powered_off());
        assert!(io.append("d/a", b"x").is_err());
        assert!(io.is_powered_off());
    }
}

#[test]
fn durable_content_is_what_sync_made_durable() {
    let mut io = SimIo::new(1, FaultProfile::clean());
    io.create_dir_all("d").unwrap();
    assert_eq!(io.durable_content("d/a"), None);
    io.append("d/a", b"abc").unwrap();
    assert_eq!(io.durable_content("d/a"), Some(Vec::new()));
    io.sync("d/a").unwrap();
    assert_eq!(io.durable_content("d/a"), Some(b"abc".to_vec()));
    assert!(io.exists("d/a"));
}

#[test]
fn truncate_cuts_pending_bytes_beyond_the_durable_part_and_defers_durable_cuts() {
    let mut io = SimIo::new(1, FaultProfile::clean());
    io.create_dir_all("d").unwrap();
    io.append("d/a", b"1234").unwrap();
    io.sync("d/a").unwrap();
    io.append("d/a", b"5678").unwrap();
    io.truncate("d/a", 6).unwrap();
    assert_eq!(io.read("d/a").unwrap(), b"123456");
    io.truncate("d/a", 2).unwrap();
    assert_eq!(io.read("d/a").unwrap(), b"12");
    io.crash();
    assert_eq!(
        io.read("d/a").unwrap(),
        b"1234",
        "an unsynced truncate is undone"
    );
}

#[test]
fn sync_dir_makes_only_its_own_children_durable() {
    let profile = FaultProfile {
        lose_unsynced_dirs: true,
        ..FaultProfile::clean()
    };
    let mut io = SimIo::new(1, profile);
    io.create_dir_all("d1").unwrap();
    io.create_dir_all("d2").unwrap();
    io.sync_dir("").unwrap();
    for f in ["d1/a", "d2/a"] {
        io.append(f, b"x").unwrap();
        io.sync(f).unwrap();
    }
    io.sync_dir("d1").unwrap();
    io.crash();
    assert!(io.exists("d1/a"), "its directory was synced");
    assert!(
        !io.exists("d2/a"),
        "another directory's sync does not cover it"
    );
}

#[test]
fn a_synced_entry_in_an_unsynced_directory_is_still_lost() {
    let profile = FaultProfile {
        lose_unsynced_dirs: true,
        ..FaultProfile::clean()
    };
    let mut io = SimIo::new(1, profile);
    io.create_dir_all("d").unwrap();
    io.append("d/a", b"x").unwrap();
    io.sync("d/a").unwrap();
    io.sync_dir("d").unwrap(); // the file's entry is durable, but the directory's own creation is not
    io.crash();
    assert!(!io.exists("d/a"));
    // A sibling directory with a similar name is not mistaken for the parent.
    let mut io = SimIo::new(1, profile);
    io.create_dir_all("d").unwrap();
    io.create_dir_all("dd").unwrap();
    io.sync_dir("").unwrap();
    io.append("dd/a", b"x").unwrap();
    io.sync("dd/a").unwrap();
    io.sync_dir("dd").unwrap();
    io.crash();
    assert!(io.exists("dd/a"));
}

#[test]
fn torn_writes_keep_anything_from_nothing_to_everything() {
    let profile = FaultProfile {
        torn_writes: true,
        ..FaultProfile::clean()
    };
    let mut kept = std::collections::BTreeSet::new();
    for seed in 0..64 {
        let mut io = SimIo::new(seed, profile);
        io.create_dir_all("d").unwrap();
        io.append("d/a", b"ab").unwrap();
        io.crash();
        kept.insert(io.read("d/a").map(|v| v.len()).unwrap_or(0));
    }
    assert_eq!(kept, [0usize, 1, 2].into_iter().collect());
}

#[test]
fn garbage_tails_are_bounded_and_vary() {
    let profile = FaultProfile {
        torn_writes: true,
        garbage_tail: true,
        ..FaultProfile::clean()
    };
    let (mut longest, mut bytes) = (0usize, std::collections::BTreeSet::new());
    for seed in 0..400 {
        let mut io = SimIo::new(seed, profile);
        io.create_dir_all("d").unwrap();
        io.append("d/a", b"ab").unwrap();
        io.crash();
        let content = io.durable_content("d/a").unwrap_or_default();
        longest = longest.max(content.len());
        bytes.extend(content.iter().skip(2).copied());
    }
    assert!(longest > 2 && longest <= 2 + 8 + 2, "{longest}");
    assert!(
        bytes.len() > 40,
        "garbage bytes should look random: {}",
        bytes.len()
    );
    assert!(bytes.iter().any(|b| *b != 0xFF && *b != 0));
}

#[test]
fn bit_rot_flips_exactly_one_bit_of_a_durable_file() {
    let profile = FaultProfile {
        bit_rot_ppm: 1_000_000,
        ..FaultProfile::clean()
    };
    for seed in 0..50 {
        let mut io = SimIo::new(seed, profile);
        io.create_dir_all("d").unwrap();
        io.append("d/a", &[0u8; 16]).unwrap();
        io.sync("d/a").unwrap();
        io.crash();
        assert_eq!(io.rot_events(), 1);
        let ones: u32 = io
            .durable_content("d/a")
            .unwrap()
            .iter()
            .map(|b| b.count_ones())
            .sum();
        assert_eq!(ones, 1, "seed {seed}");
    }
    // No rot without the profile asking for it, and none on an empty disk.
    let mut io = SimIo::new(1, FaultProfile::clean());
    io.crash();
    assert_eq!(io.rot_events(), 0);
    let mut io = SimIo::new(1, profile);
    io.crash();
    assert_eq!(io.rot_events(), 0);
}

#[test]
fn the_whole_disk_model_reproduces_a_golden() {
    fn drive(seed: u64, profile: FaultProfile, cut: Option<u64>) -> u64 {
        let mut io = SimIo::new(seed, profile);
        let mut r = SeededRng::new(seed ^ 0xABCD);
        io.create_dir_all("d/e").unwrap();
        if let Some(c) = cut {
            io.arm_power_cut(c);
        }
        for _ in 0..60u32 {
            let name = format!("d/f{}", r.below(3));
            let data: Vec<u8> = (0..r.below(40)).map(|_| r.next_u64() as u8).collect();
            match r.below(8) {
                0..=2 => {
                    let _ = io.append(&name, &data);
                }
                3 => {
                    let _ = io.sync(&name);
                }
                4 => {
                    let _ = io.sync_dir(if r.below(2) == 0 { "d" } else { "" });
                }
                5 => {
                    let _ = io.truncate(&name, r.below(50));
                }
                6 => {
                    let _ = io.remove(&name);
                }
                _ => io.crash(),
            }
        }
        io.crash();
        let mut h = io.trace_hash()
            ^ io.rot_events().wrapping_mul(31)
            ^ io.power_cuts().wrapping_mul(131)
            ^ io.mutating_ops().wrapping_mul(7919)
            ^ (io.used_bytes() as u64).wrapping_mul(104_729);
        for n in ["d/f0", "d/f1", "d/f2"] {
            if let Some(c) = io.durable_content(n) {
                for b in c {
                    h = h.wrapping_mul(1_099_511_628_211) ^ u64::from(b);
                }
            } else {
                h ^= 0x55;
            }
        }
        h
    }
    let profiles = [
        FaultProfile::clean(),
        FaultProfile::crash_heavy(),
        FaultProfile::disk_faulty(),
        FaultProfile::everything(),
    ];
    let mut fold = 0u64;
    for (i, p) in profiles.iter().enumerate() {
        for seed in 0..60u64 {
            for cut in [None, Some(seed % 13)] {
                fold = fold
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(drive(seed + 1000 * i as u64, *p, cut));
            }
        }
    }
    assert_eq!(fold, 3_372_605_988_142_576_481);
}

#[test]
fn the_random_stream_and_trace_hash_reproduce_goldens() {
    let mut r = SeededRng::new(42);
    assert_eq!(
        (r.next_u64(), r.next_u64(), r.next_u64()),
        (
            13_679_457_532_755_275_413,
            2_949_826_092_126_892_291,
            5_139_283_748_462_763_858
        )
    );
    let mut t = TraceHash::default();
    t.write(b"abc");
    t.write_u64(7);
    assert_eq!(t.value(), 7_052_562_060_837_402_828);
    let mut rng = SeededRng::new(5);
    assert!((0..100).all(|_| !rng.chance_ppm(0)));
    assert!((0..100).all(|_| rng.chance_ppm(1_000_000)));
}

#[test]
fn the_network_delivers_drops_duplicates_and_reorders_reproducibly() {
    let mut n = SimNet::new(9, 200_000, 200_000, 300_000);
    for i in 0..30u8 {
        n.send(1, 2, vec![i]);
    }
    let mut got = Vec::new();
    while let Some((from, b)) = n.recv(2) {
        got.push((from, b[0]));
    }
    let expected: Vec<(u32, u8)> = vec![
        (1, 29),
        (1, 28),
        (1, 19),
        (1, 11),
        (1, 8),
        (1, 7),
        (1, 4),
        (1, 3),
        (1, 6),
        (1, 9),
        (1, 10),
        (1, 12),
        (1, 13),
        (1, 14),
        (1, 16),
        (1, 17),
        (1, 18),
        (1, 20),
        (1, 21),
        (1, 22),
        (1, 23),
        (1, 24),
        (1, 25),
        (1, 26),
        (1, 27),
        (1, 28),
        (1, 29),
    ];
    assert_eq!(got, expected);
    // Without reordering the queue is FIFO; with certain reordering it is LIFO.
    let mut fifo = SimNet::new(1, 0, 0, 0);
    let mut lifo = SimNet::new(1, 0, 0, 1_000_000);
    for i in 0..5u8 {
        fifo.send(1, 2, vec![i]);
        lifo.send(1, 2, vec![i]);
    }
    let drain = |n: &mut SimNet| {
        let mut v = Vec::new();
        while let Some((_, b)) = n.recv(2) {
            v.push(b[0]);
        }
        v
    };
    assert_eq!(drain(&mut fifo), vec![0, 1, 2, 3, 4]);
    assert_eq!(drain(&mut lifo), vec![4, 3, 2, 1, 0]);
}

#[test]
fn switching_reordering_off_leaves_the_random_stream_to_drops_and_duplicates() {
    let mut n = SimNet::new(4, 400_000, 100_000, 0);
    for i in 0..40u8 {
        n.send(1, 2, vec![i]);
    }
    let mut got = Vec::new();
    while let Some((_, b)) = n.recv(2) {
        got.push(b[0]);
    }
    assert_eq!(
        got,
        vec![
            0, 1, 5, 6, 8, 10, 11, 12, 14, 14, 15, 16, 17, 18, 19, 21, 22, 24, 26, 32, 33, 35, 36,
            37, 38, 39
        ]
    );
}

#[test]
fn a_zero_rate_never_fires_even_when_the_draw_is_zero() {
    // The first draw below one million for this seed is exactly zero.
    let mut rng = SeededRng::new(1_147_169);
    assert!(!rng.chance_ppm(0));
    let mut rng = SeededRng::new(1_147_169);
    assert!(rng.chance_ppm(1));
}

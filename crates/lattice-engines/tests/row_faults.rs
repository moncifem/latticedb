//! Row/index histories under every simulated disk profile. The oracle matches recovered changes
//! against submitted transaction envelopes, rather than trusting derived rows to verify themselves.
//! In-doubt commits may exist or not; acknowledged commits must exist and rejected commits must not.
//! Any run touched by destructive bit rot is reported as incomplete, never counted as a clean pass.
#![cfg(feature = "row")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use lattice_core::Config;
use lattice_core::bugs::{self, Bug};
use lattice_core::changeset::ChangeSet;
use lattice_core::record::{Decoded, RecordType, SEGMENT_HEADER_LEN, decode_record};
use lattice_engines::index::IndexDef;
use lattice_engines::mvcc::Snapshot;
use lattice_engines::rowops::{self, RowKey, RowWrite};
use lattice_engines::store::{RowError, RowStore};
use lattice_seams::{Io, IoError};
use lattice_sim::{FaultProfile, SeededRng, SimClock, SimIo, TraceHash};
use lattice_testkit::scenario::{SharedClock, SharedIo, profiles};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

const MARKERS: u32 = 100;
const TABLES: u32 = 3;
const STEPS: u64 = 72;
const SEEDS: u64 = 48;
type Rows = BTreeMap<RowKey, Vec<u8>>;
type Writes = BTreeMap<RowKey, Option<Vec<u8>>>;
type Store = RowStore<ObservedIo, SharedClock, SeededRng>;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Faults {
    append_errors: u64,
    partial_appends: u64,
    fsync_errors: u64,
    no_space: u64,
}

/// Test-side observation, not a replacement disk model. A failed append is counted as partial
/// only when a nonempty strict prefix became visible, so a full failed write is not called torn.
#[derive(Clone)]
struct ObservedIo {
    disk: SharedIo,
    faults: Rc<RefCell<Faults>>,
}

impl Io for ObservedIo {
    fn exists(&self, p: &str) -> bool {
        self.disk.exists(p)
    }
    fn len(&self, p: &str) -> Result<u64, IoError> {
        self.disk.len(p)
    }
    fn read(&self, p: &str) -> Result<Vec<u8>, IoError> {
        self.disk.read(p)
    }
    fn list(&self, p: &str) -> Result<Vec<String>, IoError> {
        self.disk.list(p)
    }
    fn create_dir_all(&mut self, p: &str) -> Result<(), IoError> {
        self.disk.create_dir_all(p)
    }
    fn lock(&mut self, p: &str) -> Result<(), IoError> {
        self.disk.lock(p)
    }
    fn unlock(&mut self, p: &str) {
        self.disk.unlock(p);
    }
    fn truncate(&mut self, p: &str, n: u64) -> Result<(), IoError> {
        self.disk.truncate(p, n)
    }
    fn remove(&mut self, p: &str) -> Result<(), IoError> {
        self.disk.remove(p)
    }
    fn append(&mut self, p: &str, bytes: &[u8]) -> Result<(), IoError> {
        let before = self.disk.len(p).unwrap_or(0);
        let result = self.disk.append(p, bytes);
        let after = self.disk.len(p).unwrap_or(0);
        let mut counts = self.faults.borrow_mut();
        match &result {
            Err(IoError::NoSpace) => counts.no_space += 1,
            Err(IoError::Other(why)) if why == "injected append error" => counts.append_errors += 1,
            _ => {}
        }
        if result.is_err() && after > before && after - before < bytes.len() as u64 {
            counts.partial_appends += 1;
        }
        result
    }
    fn sync(&mut self, p: &str) -> Result<(), IoError> {
        let result = self.disk.sync(p);
        if matches!(&result, Err(IoError::Other(why)) if why == "injected fsync error") {
            self.faults.borrow_mut().fsync_errors += 1;
        }
        result
    }
    fn sync_dir(&mut self, p: &str) -> Result<(), IoError> {
        let result = self.disk.sync_dir(p);
        if matches!(&result, Err(IoError::Other(why)) if why == "injected dir fsync error") {
            self.faults.borrow_mut().fsync_errors += 1;
        }
        result
    }
}

fn config() -> Config {
    Config {
        dir: "wal".into(),
        segment_limit: 4_096,
        node: 1,
    }
}

fn open(io: &ObservedIo, clock: &Rc<SimClock>, seed: u64) -> Result<Store, RowError> {
    RowStore::open_with_indexes(
        io.clone(),
        SharedClock(clock.clone()),
        SeededRng::new(seed),
        &config(),
        (0..TABLES)
            .map(|table| IndexDef {
                id: table,
                table,
                extract: |v| v.first().map(|b| vec![*b]),
            })
            .collect(),
    )
}

#[derive(Debug, Clone)]
struct Attempt {
    writes: Writes,
    acked: Option<u64>,
    rejected: bool,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct Outcome {
    problem: Option<String>,
    incomplete: bool,
    cut_short: bool,
    acked: u64,
    in_doubt: u64,
    conflicts: u64,
    recoveries: u64,
    snapshots: u64,
    power_cuts: u64,
    rot: u64,
    faults: Faults,
    trace: u64,
}

fn writes(id: u64, key: u8, delete: bool, value: u8) -> Writes {
    let mut writes = Writes::new();
    // A large enough value to actually fill both finite-capacity profiles in an ordinary run.
    for table in 0..TABLES {
        writes.insert((table, vec![key]), (!delete).then(|| vec![value; 512]));
    }
    writes.insert(
        (MARKERS, id.to_le_bytes().to_vec()),
        Some(id.to_le_bytes().to_vec()),
    );
    writes
}

fn stage(store: &Store, writes: &Writes) -> lattice_engines::store::RowTx {
    let mut tx = store.begin();
    for (key, value) in writes {
        match value {
            Some(value) => tx.put(key.clone(), value.clone()),
            None => tx.delete(key.clone()),
        }
    }
    tx
}

fn marker(writes: &[RowWrite]) -> Result<u64, String> {
    let markers: Vec<_> = writes
        .iter()
        .filter(|((table, _), _)| *table == MARKERS)
        .collect();
    if markers.len() != 1 {
        return Err(format!(
            "expected one transaction marker, got {}",
            markers.len()
        ));
    }
    let ((_, key), value) = markers[0];
    let id = u64::from_le_bytes(key.as_slice().try_into().map_err(|_| "bad marker key")?);
    if value.as_deref() != Some(id.to_le_bytes().as_slice()) {
        return Err("marker value differs from key".into());
    }
    Ok(id)
}

/// Decode what is visible through the store's acknowledged/recovered prefix, fold submitted
/// envelopes into an independent map, and preserve a model for every commit LSN.
fn history(
    io: &ObservedIo,
    through: u64,
    attempts: &BTreeMap<u64, Attempt>,
) -> Result<Vec<(u64, Rows)>, String> {
    let names = io.list("wal").map_err(|e| format!("list WAL: {e}"))?;
    let mut history = vec![(0, Rows::new())];
    let mut rows = Rows::new();
    let mut expected_lsn = 1;
    let mut present = BTreeSet::new();
    let mut change_ids = BTreeSet::new();
    let mut previous_hlc = None;
    for name in names.into_iter().filter(|n| n.ends_with(".seg")) {
        let bytes = io
            .read(&format!("wal/{name}"))
            .map_err(|e| format!("read {name}: {e}"))?;
        let mut at = SEGMENT_HEADER_LEN;
        while at < bytes.len() && expected_lsn <= through {
            let Decoded::Record(record, next) = decode_record(&bytes, at) else {
                return Err(format!(
                    "record {expected_lsn} in {name} is damaged inside the visible prefix"
                ));
            };
            if record.lsn != expected_lsn {
                return Err(format!("expected LSN {expected_lsn}, got {}", record.lsn));
            }
            expected_lsn += 1;
            at = next;
            if RecordType::from_u8(record.rtype) != Some(RecordType::ChangeSet) {
                return Err(format!("unexpected record type {}", record.rtype));
            }
            let cs = ChangeSet::decode(&record.payload).map_err(|e| format!("change set: {e}"))?;
            if !change_ids.insert(cs.id) {
                return Err("I6: duplicate change set id".into());
            }
            if cs.author != 0 || cs.turn.is_some() || !cs.parents.is_empty() {
                return Err("raw row commit metadata differs from its submitted defaults".into());
            }
            if previous_hlc.is_some_and(|previous| cs.hlc <= previous) {
                return Err("I4: row commit timestamps do not increase".into());
            }
            previous_hlc = Some(cs.hlc);
            let writes = rowops::decode(&cs.payload).map_err(|e| format!("row payload: {e:?}"))?;
            let id = marker(&writes)?;
            if !present.insert(id) {
                return Err(format!("transaction {id} logged twice"));
            }
            let attempt = attempts
                .get(&id)
                .ok_or_else(|| format!("unsubmitted transaction {id} in the log"))?;
            if attempt.rejected {
                return Err(format!("rejected transaction {id} is durable"));
            }
            let envelope: Vec<_> = attempt
                .writes
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            if writes != envelope {
                return Err(format!(
                    "transaction {id} has a changed/partial write envelope"
                ));
            }
            if let Some(acked) = attempt.acked
                && acked != record.lsn
            {
                return Err(format!(
                    "transaction {id} acknowledged LSN {acked}, recovered {}",
                    record.lsn
                ));
            }
            // Fold the independently recorded submitted writes, not a copy of the store's rows.
            for (key, value) in &attempt.writes {
                match value {
                    Some(v) => {
                        rows.insert(key.clone(), v.clone());
                    }
                    None => {
                        rows.remove(key);
                    }
                }
            }
            history.push((record.lsn, rows.clone()));
        }
    }
    if expected_lsn != through + 1 {
        return Err(format!(
            "WAL ends at {}, store claims {through}",
            expected_lsn - 1
        ));
    }
    for (id, attempt) in attempts {
        if attempt.acked.is_some() && !present.contains(id) {
            return Err(format!("I1: acknowledged transaction {id} absent"));
        }
    }
    Ok(history)
}

fn expected_at(history: &[(u64, Rows)], snap: Snapshot) -> &Rows {
    &history
        .iter()
        .rev()
        .find(|(lsn, _)| *lsn <= snap.as_of)
        .unwrap()
        .1
}

fn compare(store: &Store, snap: Snapshot, expected: &Rows) -> Result<(), String> {
    store
        .check_indexes_at(snap)
        .map_err(|e| format!("I9: {e}"))?;
    for table in (0..TABLES).chain([MARKERS]) {
        let want: Vec<_> = expected
            .iter()
            .filter(|((t, _), _)| *t == table)
            .map(|((_, k), v)| (k.clone(), v.clone()))
            .collect();
        let got: Vec<_> = store
            .scan_at(snap, table, ..)
            .into_iter()
            .map(|(k, v)| (k.to_vec(), v.clone()))
            .collect();
        if got != want {
            return Err(format!(
                "I2/I3: table {table} snapshot {} differs from submitted history",
                snap.as_of
            ));
        }
        for (key, value) in &want {
            if store.get_at(snap, table, key) != Some(value) {
                return Err("point read differs from range scan".into());
            }
        }
        if table != MARKERS {
            // Probe absent index keys too, so stale entries cannot hide behind present rows.
            for value in 0..12u8 {
                let want: Vec<_> = expected
                    .iter()
                    .filter(|((t, _), v)| *t == table && v.first() == Some(&value))
                    .map(|((_, k), _)| k.clone())
                    .collect();
                if store.lookup_at(snap, table, &[value]) != want {
                    return Err(format!(
                        "I9: index {table} value {value} differs at {}",
                        snap.as_of
                    ));
                }
            }
        }
    }
    Ok(())
}

fn check(
    store: &Store,
    io: &ObservedIo,
    attempts: &BTreeMap<u64, Attempt>,
    snapshots: &[Snapshot],
) -> Result<(), String> {
    let history = history(io, store.applied_lsn(), attempts)?;
    compare(
        store,
        store.snapshot(),
        expected_at(&history, store.snapshot()),
    )?;
    for snap in snapshots {
        compare(store, *snap, expected_at(&history, *snap))?;
    }
    Ok(())
}

fn attempt_result(
    result: Result<u64, RowError>,
    attempt: &mut Attempt,
    out: &mut Outcome,
    trace: &mut TraceHash,
) -> bool {
    trace.write(format!("{result:?}").as_bytes());
    match result {
        Ok(lsn) => {
            attempt.acked = Some(lsn);
            out.acked += 1;
            true
        }
        Err(RowError::Conflict { .. }) => {
            attempt.rejected = true;
            out.conflicts += 1;
            true
        }
        Err(RowError::Rejected(_) | RowError::Encode(_) | RowError::InvalidIndexes(_)) => {
            attempt.rejected = true;
            true
        }
        Err(RowError::InDoubt(_) | RowError::Stale) => {
            out.in_doubt += 1;
            false
        }
        Err(e) => {
            out.problem = Some(format!("unexpected commit error: {e}"));
            false
        }
    }
}

fn run(seed: u64, profile: FaultProfile, steps: u64) -> Outcome {
    let io = ObservedIo {
        disk: SharedIo::new(SimIo::new(seed, profile)),
        faults: Rc::default(),
    };
    let clock = Rc::new(SimClock::new(1_000));
    let mut rng = SeededRng::new(seed ^ 0x726f775f6661756c);
    let mut attempts = BTreeMap::new();
    let mut snapshots = Vec::new();
    let mut store = None;
    let mut generation = 0;
    let mut next_id = 0;
    let mut out = Outcome::default();
    let mut trace = TraceHash::default();
    let mut recovery_failures = 0;
    for step in 0..steps {
        if store.is_none() {
            // Recovery itself sees the fault profile, including a power cut inside its protocol.
            if rng.below(5) == 0 {
                io.disk.arm_power_cut(rng.below(6));
            }
            generation += 1;
            match open(&io, &clock, seed.wrapping_add(generation * 1_000_003)) {
                Ok(s) => {
                    store = Some(s);
                    out.recoveries += 1;
                    recovery_failures = 0;
                }
                Err(e) => {
                    trace.write(format!("recover:{e:?}").as_bytes());
                    io.disk.crash();
                    recovery_failures += 1;
                    if io.disk.rot_events() > 0 {
                        out.incomplete = true;
                        break;
                    }
                    if recovery_failures >= 12 {
                        out.cut_short = true;
                        break;
                    }
                    continue;
                }
            }
        }
        let s = store.as_mut().unwrap();
        if let Err(e) = check(s, &io, &attempts, &snapshots) {
            out.problem = Some(format!("step {step}: {e}"));
            break;
        }
        match rng.below(100) {
            0..=64 => {
                let key = rng.below(6) as u8;
                let delete = rng.below(5) == 0;
                let value = rng.below(12) as u8;
                let id = next_id;
                next_id += 1;
                let w = writes(id, key, delete, value);
                let tx = stage(s, &w);
                attempts.insert(
                    id,
                    Attempt {
                        writes: w,
                        acked: None,
                        rejected: false,
                    },
                );
                if rng.below(5) == 0 {
                    io.disk.arm_power_cut(rng.below(8));
                }
                if !attempt_result(
                    s.commit(tx),
                    attempts.get_mut(&id).unwrap(),
                    &mut out,
                    &mut trace,
                ) {
                    drop(store.take());
                }
            }
            65..=74 => {
                // Real concurrent snapshots: the second writer must conflict if the first commits.
                let key = rng.below(6) as u8;
                let a = next_id;
                let b = next_id + 1;
                next_id += 2;
                let wa = writes(a, key, false, 10);
                let wb = writes(b, key, false, 11);
                let ta = stage(s, &wa);
                let tb = stage(s, &wb);
                attempts.insert(
                    a,
                    Attempt {
                        writes: wa,
                        acked: None,
                        rejected: false,
                    },
                );
                if attempt_result(
                    s.commit(ta),
                    attempts.get_mut(&a).unwrap(),
                    &mut out,
                    &mut trace,
                ) {
                    if attempts[&a].acked.is_some() {
                        attempts.insert(
                            b,
                            Attempt {
                                writes: wb,
                                acked: None,
                                rejected: false,
                            },
                        );
                        let result = s.commit(tb);
                        if !matches!(result, Err(RowError::Conflict { .. })) {
                            out.problem = Some(format!(
                                "I3: overlapping writer did not conflict: {result:?}"
                            ));
                            break;
                        }
                        attempt_result(result, attempts.get_mut(&b).unwrap(), &mut out, &mut trace);
                    }
                } else {
                    drop(store.take());
                }
            }
            75..=84 => {
                snapshots.push(s.snapshot());
                out.snapshots += 1;
            }
            _ => {
                drop(store.take());
                if rng.below(3) == 0 {
                    io.disk.restart_process();
                } else {
                    io.disk.crash();
                }
                if io.disk.rot_events() > 0 {
                    out.incomplete = true;
                    break;
                }
            }
        }
        clock.advance(1);
        if out.problem.is_some() {
            break;
        }
        if io.disk.is_powered_off() {
            drop(store.take());
            io.disk.crash();
        }
        if io.disk.rot_events() > 0 {
            out.incomplete = true;
            break;
        }
    }
    drop(store);
    // A bounded run always gets a terminal recovery. Bit rot is stopped at its first occurrence
    // and the whole run is censored, rather than exempting later acknowledged writes.
    io.disk.set_profile(profile.without_transient_faults());
    io.disk.crash();
    match open(&io, &clock, seed.wrapping_add(9_000_000_001)) {
        Ok(s) if !out.incomplete => {
            if let Err(e) = check(&s, &io, &attempts, &snapshots) {
                out.problem = Some(format!("terminal: {e}"));
            }
        }
        Ok(_) => {}
        Err(e) if out.incomplete => {
            trace.write(format!("terminal censored:{e:?}").as_bytes());
        }
        Err(e) => {
            out.problem = Some(format!("terminal recovery without transient faults: {e:?}"));
        }
    }
    out.power_cuts = io.disk.power_cuts();
    out.rot = io.disk.rot_events();
    out.faults = *io.faults.borrow();
    trace.write_u64(io.disk.trace_hash());
    trace.write(format!("{out:?}").as_bytes());
    out.trace = trace.value();
    out
}

// Captured by the test runner unless --nocapture is requested for an evidence report.
#[allow(clippy::print_stderr)]
fn verify_profiles(seeds: u64, steps: u64) {
    for (name, profile) in profiles() {
        let mut acked = 0;
        let mut incomplete = 0;
        let mut cut_short = 0;
        let mut cuts = 0;
        let mut recoveries = 0;
        let mut snapshots = 0;
        let mut conflicts = 0;
        let mut faults = Faults::default();
        for seed in 0..seeds {
            let out = run(seed, profile, steps);
            assert!(out.problem.is_none(), "profile={name} seed={seed}: {out:?}");
            incomplete += u64::from(out.incomplete);
            cut_short += u64::from(out.cut_short);
            acked += out.acked;
            cuts += out.power_cuts;
            recoveries += out.recoveries;
            snapshots += out.snapshots;
            conflicts += out.conflicts;
            faults.append_errors += out.faults.append_errors;
            faults.partial_appends += out.faults.partial_appends;
            faults.fsync_errors += out.faults.fsync_errors;
            faults.no_space += out.faults.no_space;
        }
        assert!(
            acked >= seeds,
            "profile {name} produced too little acknowledged work: {acked}"
        );
        assert!(recoveries >= seeds, "profile {name} did not recover enough");
        assert!(
            cuts > 0 && snapshots > 0 && conflicts > 0,
            "profile {name} missed a workload operation"
        );
        assert!(
            cut_short * 4 < seeds,
            "profile {name} lost too much workload: {cut_short}/{seeds}"
        );
        if profile.bit_rot_ppm == 0 {
            assert_eq!(incomplete, 0, "profile {name} censored non-rot runs");
        } else {
            assert!(
                incomplete * 2 < seeds,
                "profile {name} censored too many runs: {incomplete}/{seeds}"
            );
        }
        if profile.append_error_ppm > 0 {
            assert!(
                faults.append_errors > 0 && faults.partial_appends > 0,
                "profile {name} did not exercise partial append errors"
            );
        }
        if profile.sync_error_ppm > 0 {
            assert!(
                faults.fsync_errors > 0,
                "profile {name} did not exercise fsync errors"
            );
        }
        if profile.disk_full_after.is_some() {
            assert!(faults.no_space > 0, "profile {name} never filled its disk");
        }
        eprintln!(
            "row faults profile={name}: {seeds} runs, {} verified, {incomplete} incomplete, {cut_short} cut short (terminal verified), {acked} acknowledgements, {cuts} cuts, {faults:?}",
            seeds - incomplete
        );
    }
}

#[test]
fn inv_i3_row_histories_hold_across_every_fault_profile() {
    verify_profiles(SEEDS, STEPS);
}

#[test]
fn row_fault_traces_replay_exactly() {
    for (name, profile) in profiles() {
        for seed in 0..6 {
            assert_eq!(
                run(seed, profile, 40),
                run(seed, profile, 40),
                "profile={name} seed={seed}"
            );
        }
    }
}

#[test]
fn row_fault_oracle_catches_every_existing_storage_bug() {
    for bug in Bug::ROW {
        bugs::enable(bug);
        let found = (0..24).find_map(|seed| {
            run(seed, FaultProfile::clean(), STEPS)
                .problem
                .map(|p| (seed, p))
        });
        bugs::clear();
        assert!(found.is_some(), "{bug:?} was not detected");
    }
}

#[test]
fn destructive_rot_is_censored_and_never_a_pass() {
    let profile = FaultProfile {
        bit_rot_ppm: 1_000_000,
        ..FaultProfile::crash_heavy()
    };
    let outcomes: Vec<_> = (0..8).map(|seed| run(seed, profile, STEPS)).collect();
    assert!(outcomes.iter().any(|out| out.incomplete));
    assert!(
        outcomes
            .iter()
            .filter(|out| out.rot > 0)
            .all(|out| out.incomplete)
    );
}

#[test]
#[ignore = "volume: 10,000 row histories per profile; suitable for a scheduled or manual gate"]
fn row_fault_profiles_at_gate_volume() {
    verify_profiles(10_000, 150);
}

/// One rotation whose commit and subsequent recovery can be cut at any mutating operation.
/// Baseline operation counts come from the real protocol; the sweep does not assume that the
/// implementation always has exactly five commit operations or a particular recovery sequence.
fn rotation_case(
    seed: u64,
    commit_cut: Option<u64>,
    recovery_cut: Option<u64>,
) -> (Outcome, u64, u64) {
    let io = ObservedIo {
        disk: SharedIo::new(SimIo::new(seed, FaultProfile::crash_heavy())),
        faults: Rc::default(),
    };
    let clock = Rc::new(SimClock::new(1_000));
    let mut attempts = BTreeMap::new();
    let mut snapshots = Vec::new();
    let mut store = open(&io, &clock, seed).unwrap();
    let mut out = Outcome::default();
    let mut trace = TraceHash::default();
    for id in 0..2 {
        let w = writes(id, id as u8, false, id as u8);
        let tx = stage(&store, &w);
        let mut attempt = Attempt {
            writes: w,
            acked: None,
            rejected: false,
        };
        assert!(attempt_result(
            store.commit(tx),
            &mut attempt,
            &mut out,
            &mut trace
        ));
        assert!(attempt.acked.is_some());
        attempts.insert(id, attempt);
        snapshots.push(store.snapshot());
    }
    let w = writes(2, 0, false, 2);
    let tx = stage(&store, &w);
    attempts.insert(
        2,
        Attempt {
            writes: w,
            acked: None,
            rejected: false,
        },
    );
    let before = io.disk.mutating_ops();
    if let Some(cut) = commit_cut {
        io.disk.arm_power_cut(cut);
    }
    attempt_result(
        store.commit(tx),
        attempts.get_mut(&2).unwrap(),
        &mut out,
        &mut trace,
    );
    let commit_ops = io.disk.mutating_ops() - before;
    drop(store);
    io.disk.crash();
    let before = io.disk.mutating_ops();
    if let Some(cut) = recovery_cut {
        io.disk.arm_power_cut(cut);
    }
    match open(&io, &clock, seed.wrapping_add(1_000)) {
        Ok(recovered) => {
            out.recoveries += 1;
            if let Err(e) = check(&recovered, &io, &attempts, &snapshots) {
                out.problem = Some(format!("first recovery: {e}"));
            }
        }
        Err(e) if io.disk.is_powered_off() => trace.write(format!("recovery cut:{e}").as_bytes()),
        Err(e) => out.problem = Some(format!("unexpected recovery failure: {e}")),
    }
    let recovery_ops = io.disk.mutating_ops() - before;
    // Replay again after a fault-free terminal restart, even if the first recovery was interrupted.
    io.disk.set_profile(FaultProfile::crash_heavy());
    io.disk.crash();
    match open(&io, &clock, seed.wrapping_add(2_000)) {
        Ok(recovered) => {
            out.recoveries += 1;
            if let Err(e) = check(&recovered, &io, &attempts, &snapshots) {
                out.problem = Some(format!("terminal recovery: {e}"));
            }
        }
        Err(e) => out.problem = Some(format!("terminal recovery failed: {e}")),
    }
    out.power_cuts = io.disk.power_cuts();
    out.faults = *io.faults.borrow();
    (out, commit_ops, recovery_ops)
}

#[test]
fn inv_i9_row_and_index_history_survive_every_cut_point() {
    for seed in 0..20 {
        let (baseline, commit_ops, recovery_ops) = rotation_case(seed, None, None);
        assert!(baseline.problem.is_none(), "seed={seed}: {baseline:?}");
        assert!(
            commit_ops >= 5,
            "workload did not rotate a segment: {commit_ops}"
        );
        assert!(
            recovery_ops >= 3,
            "recovery did not exercise durability: {recovery_ops}"
        );
        for cut in 0..=commit_ops {
            let (out, _, _) = rotation_case(seed, Some(cut), None);
            assert!(
                out.problem.is_none(),
                "commit seed={seed} cut={cut}: {out:?}"
            );
            if cut < commit_ops {
                assert!(out.power_cuts > 0, "commit cut {cut} was not reached");
            }
        }
        for cut in 0..=recovery_ops {
            let (out, _, _) = rotation_case(seed, None, Some(cut));
            assert!(
                out.problem.is_none(),
                "recovery seed={seed} cut={cut}: {out:?}"
            );
            if cut < recovery_ops {
                assert!(out.power_cuts > 0, "recovery cut {cut} was not reached");
            }
        }
    }
}

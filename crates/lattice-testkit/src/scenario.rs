//! The crash-and-recover scenario: a seeded random workload against a kernel running on the
//! simulated disk, with invariants checked after every recovery.
//!
//! Crashes come in three flavours: a process crash (the page cache survives), a power loss between
//! steps, and a **power cut in the middle of a durability protocol** (between an append and its
//! fsync, between a segment header and its directory sync, inside recovery itself). Every scenario
//! ends with a terminal recovery on a disk whose transient faults are switched off, so cutting a
//! run short never skips the final check.

use crate::invariants::{Expected, LogView, Oracle, Violation, check_all};
use lattice_abi::{Classify, ErrorClass};
use lattice_core::changeset::{ChangeSet, ChangeSetId, TurnRef};
use lattice_core::scrub::Issue;
use lattice_core::{AllowAll, Config, Kernel, LogError, PrepareOutcome, Receipt, TxnError};
use lattice_seams::{Clock, Io, IoError, Rng};
use lattice_sim::{FaultProfile, SeededRng, SimClock, SimIo};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

/// A clock shared between the scenario (which moves time) and the kernel (which reads it).
#[derive(Clone)]
pub struct SharedClock(pub Rc<SimClock>);

impl Clock for SharedClock {
    fn now_ms(&self) -> u64 {
        self.0.now_ms()
    }
}

/// A disk shared between the scenario (which crashes it) and the kernel (which uses it). Dropping
/// the kernel while keeping this handle is exactly a process crash with the disk surviving.
#[derive(Clone)]
pub struct SharedIo(pub Rc<RefCell<SimIo>>);

impl SharedIo {
    /// A new empty disk.
    pub fn new(io: SimIo) -> Self {
        Self(Rc::new(RefCell::new(io)))
    }
    /// Power cut and restart.
    pub fn crash(&self) {
        self.0.borrow_mut().crash();
    }
    /// Process restart without power loss.
    pub fn restart_process(&self) {
        self.0.borrow_mut().restart_process();
    }
    /// Whether a power cut hit and the machine is still dead.
    pub fn is_powered_off(&self) -> bool {
        self.0.borrow().is_powered_off()
    }
    /// Arms a power cut after `ops` more mutating operations.
    pub fn arm_power_cut(&self, ops: u64) {
        self.0.borrow_mut().arm_power_cut(ops);
    }
    /// Mutating operations attempted so far.
    pub fn mutating_ops(&self) -> u64 {
        self.0.borrow().mutating_ops()
    }
    /// Power cuts that hit mid-protocol so far.
    pub fn power_cuts(&self) -> u64 {
        self.0.borrow().power_cuts()
    }
    /// Bit rot events so far.
    pub fn rot_events(&self) -> u64 {
        self.0.borrow().rot_events()
    }
    /// Appends refused for lack of space so far.
    pub fn no_space_events(&self) -> u64 {
        self.0.borrow().no_space_events()
    }
    /// Trace fingerprint.
    pub fn trace_hash(&self) -> u64 {
        self.0.borrow().trace_hash()
    }
    /// Replaces the fault profile.
    pub fn set_profile(&self, p: FaultProfile) {
        self.0.borrow_mut().set_profile(p);
    }
}

impl Io for SharedIo {
    fn exists(&self, path: &str) -> bool {
        self.0.borrow().exists(path)
    }
    fn len(&self, path: &str) -> Result<u64, IoError> {
        self.0.borrow().len(path)
    }
    fn read(&self, path: &str) -> Result<Vec<u8>, IoError> {
        self.0.borrow().read(path)
    }
    fn append(&mut self, path: &str, data: &[u8]) -> Result<(), IoError> {
        self.0.borrow_mut().append(path, data)
    }
    fn truncate(&mut self, path: &str, len: u64) -> Result<(), IoError> {
        self.0.borrow_mut().truncate(path, len)
    }
    fn sync(&mut self, path: &str) -> Result<(), IoError> {
        self.0.borrow_mut().sync(path)
    }
    fn sync_dir(&mut self, dir: &str) -> Result<(), IoError> {
        self.0.borrow_mut().sync_dir(dir)
    }
    fn list(&self, dir: &str) -> Result<Vec<String>, IoError> {
        self.0.borrow().list(dir)
    }
    fn remove(&mut self, path: &str) -> Result<(), IoError> {
        self.0.borrow_mut().remove(path)
    }
    fn create_dir_all(&mut self, dir: &str) -> Result<(), IoError> {
        self.0.borrow_mut().create_dir_all(dir)
    }
    fn lock(&mut self, path: &str) -> Result<(), IoError> {
        self.0.borrow_mut().lock(path)
    }
    fn unlock(&mut self, path: &str) {
        self.0.borrow_mut().unlock(path);
    }
}

/// The named fault profiles every nightly run covers.
pub fn profiles() -> [(&'static str, FaultProfile); 5] {
    [
        ("clean", FaultProfile::clean()),
        ("crash-heavy", FaultProfile::crash_heavy()),
        ("disk-faulty", FaultProfile::disk_faulty()),
        (
            "hostile",
            FaultProfile {
                sync_error_ppm: 30_000,
                ..FaultProfile::crash_heavy()
            },
        ),
        ("everything", FaultProfile::everything()),
    ]
}

/// What one run produced.
#[derive(Debug, Clone, Default)]
pub struct Outcome {
    /// Violated invariants. Empty means the run passed.
    pub violations: Vec<Violation>,
    /// Fingerprint of every storage operation, its data and outcome.
    pub trace_hash: u64,
    /// Number of successful crash-and-recover cycles.
    pub recoveries: u32,
    /// Commits acknowledged.
    pub acked: usize,
    /// Power cuts that landed inside a durability protocol.
    pub power_cuts: u64,
    /// Appends the simulated disk refused because it was full.
    pub no_space: u64,
    /// The random workload stopped early because injected faults kept recovery from completing.
    /// The terminal check still ran, so this is lost workload, not a lost verdict.
    pub cut_short: bool,
    /// No terminal check could run (recovery refused a rotted log, or never completed). The run is
    /// censored: it says nothing about correctness and must be counted, not treated as a pass.
    pub incomplete: bool,
}

type K = Kernel<SharedIo, SharedClock, SeededRng>;

enum Recover {
    Ok(Box<K>),
    /// Recovery refused a log that bit rot damaged (allowed, but the run is censored).
    Refused,
    /// Injected faults kept recovery from completing.
    GaveUp,
}

struct World {
    clock: Rc<SimClock>,
    rng: SeededRng,
    oracle: Oracle,
    expected: BTreeMap<ChangeSetId, Expected>,
    in_doubt: BTreeSet<ChangeSetId>,
    turns: BTreeMap<u64, u64>,
    /// Sessions whose latest `begin_turn` failed ambiguously; their true turn is unknown.
    uncertain_sessions: BTreeSet<u64>,
    /// Sessions that existed when bit rot last struck.
    exempt_sessions: BTreeSet<u64>,
    seen_rot: u64,
    config: Config,
    violations: Vec<Violation>,
    recoveries: u32,
}

fn violation(w: &mut World, invariant: &'static str, detail: String) {
    w.violations.push(Violation { invariant, detail });
}

/// Crashes or restarts the machine and reopens the kernel, retrying through injected faults
/// (including power cuts armed inside recovery).
fn recover(w: &mut World, disk: &SharedIo, kernel_rng: &mut SeededRng) -> Recover {
    for _ in 0..8 {
        if disk.is_powered_off() || w.rng.below(4) != 0 {
            disk.crash();
        } else {
            disk.restart_process();
        }
        if w.rng.below(6) == 0 {
            disk.arm_power_cut(w.rng.below(6));
        }
        let clock = SharedClock(w.clock.clone());
        let mut records = Vec::new();
        match Kernel::open_streaming(
            disk.clone(),
            clock,
            kernel_rng.fork(),
            &w.config,
            |record| records.push(record),
        ) {
            Ok((mut k, _, _)) => {
                w.recoveries += 1;
                if disk.rot_events() > w.seen_rot {
                    w.seen_rot = disk.rot_events();
                    let ids: Vec<ChangeSetId> = w.oracle.acked.keys().copied().collect();
                    w.oracle.exempt.extend(ids);
                    let sessions: Vec<u64> = w.turns.keys().copied().collect();
                    w.exempt_sessions.extend(sessions);
                }
                let view = LogView::from_records(&records);
                // Ids that were in doubt are now resolved by what recovery found. Those present
                // are durable (recovery syncs what it adopts) and count as acknowledged.
                let present: Vec<ChangeSetId> =
                    view.change_sets.iter().map(|(_, c)| c.id).collect();
                for id in std::mem::take(&mut w.in_doubt) {
                    if present.contains(&id)
                        && let Some(e) = w.expected.get(&id)
                    {
                        w.oracle.acked.insert(id, e.clone());
                    }
                }
                let found = check_all(&view, &w.oracle);
                w.violations.extend(found);
                probe_after_recovery(w, &mut k, &present);
                return Recover::Ok(Box::new(k));
            }
            Err(TxnError::Log(LogError::Corrupt(why))) => {
                if disk.rot_events() == 0 {
                    violation(
                        w,
                        "RECOVERY",
                        format!("recovery refused a history without rot: {why}"),
                    );
                }
                return Recover::Refused;
            }
            Err(_) => {}
        }
    }
    Recover::GaveUp
}

/// Runs one scenario and returns its outcome.
pub fn run(seed: u64, profile: FaultProfile, steps: usize, segment_limit: u64) -> Outcome {
    let clock = Rc::new(SimClock::new(1_000_000));
    let mut w = World {
        clock: clock.clone(),
        rng: SeededRng::new(seed ^ 0xA5A5_5A5A_1234_5678),
        oracle: Oracle::default(),
        expected: BTreeMap::new(),
        in_doubt: BTreeSet::new(),
        turns: BTreeMap::new(),
        uncertain_sessions: BTreeSet::new(),
        exempt_sessions: BTreeSet::new(),
        seen_rot: 0,
        config: Config {
            dir: "wal".into(),
            segment_limit,
            node: 1,
        },
        violations: Vec::new(),
        recoveries: 0,
    };
    let mut kernel_rng = SeededRng::new(seed.wrapping_mul(0x9E37_79B9));
    let disk = SharedIo::new(SimIo::new(seed, profile));
    let mut kernel: Option<K> = None;
    let (mut cut_short, mut incomplete) = (false, false);

    for _ in 0..steps {
        if kernel.is_none() {
            match recover(&mut w, &disk, &mut kernel_rng) {
                Recover::Ok(k) => kernel = Some(*k),
                Recover::Refused => {
                    incomplete = true;
                    break;
                }
                Recover::GaveUp => {
                    cut_short = true;
                    break;
                }
            }
        }
        let Some(k) = kernel.as_mut() else { break };
        if k.is_poisoned() {
            kernel = None;
            continue;
        }
        if w.rng.below(5) == 0 {
            disk.arm_power_cut(w.rng.below(10));
        }
        match w.rng.below(100) {
            0..=44 => commit_new(&mut w, k),
            45..=52 => resubmit(&mut w, k),
            53..=57 => resubmit_in_doubt(&mut w, k),
            58..=62 => duplicate_batch(&mut w, k),
            63..=68 => begin_turn(&mut w, k),
            69..=75 => stale_write(&mut w, k),
            76..=79 => skew_clock(&mut w),
            80..=81 => {
                // A failure leaves the log poisoned or the record absent; neither affects the oracle.
                let _ = k.checkpoint();
            }
            82..=87 => lifecycle(&mut w, k),
            _ => kernel = None, // crash: all in-memory state is lost, the disk survives
        }
    }
    drop(kernel.take());
    if !incomplete {
        // Terminal check on a disk without transient faults, so a short run still gets a verdict.
        disk.set_profile(profile.without_transient_faults());
        match recover(&mut w, &disk, &mut kernel_rng) {
            Recover::Ok(_) => {}
            Recover::Refused | Recover::GaveUp => incomplete = true,
        }
    }
    Outcome {
        violations: w.violations,
        trace_hash: disk.trace_hash(),
        recoveries: w.recoveries,
        acked: w.oracle.acked.len(),
        power_cuts: disk.power_cuts(),
        no_space: disk.no_space_events(),
        cut_short,
        incomplete,
    }
}

fn fresh_cs(w: &mut World, k: &mut K, turn: Option<TurnRef>) -> ChangeSet {
    let id = k.new_id();
    let len = w.rng.below(48) as usize;
    let payload: Vec<u8> = (0..len).map(|_| (w.rng.next_u64() & 0xFF) as u8).collect();
    let author = (u128::from(w.rng.next_u64()) << 64) | u128::from(w.rng.next_u64());
    let known: Vec<ChangeSetId> = w.oracle.acked.keys().copied().collect();
    let parents: Vec<ChangeSetId> = (0..w.rng.below(3))
        .filter_map(|_| {
            known
                .get(w.rng.below(known.len().max(1) as u64) as usize)
                .copied()
        })
        .collect();
    w.expected.insert(
        id,
        Expected {
            payload: payload.clone(),
            author,
            turn,
            parents: parents.clone(),
        },
    );
    let mut cs = ChangeSet::new(id, payload);
    cs.author = author;
    cs.turn = turn;
    cs.parents = parents;
    cs
}

fn acknowledge(w: &mut World, id: ChangeSetId) {
    if let Some(e) = w.expected.get(&id) {
        w.oracle.acked.insert(id, e.clone());
    }
    w.in_doubt.remove(&id);
}

fn record(w: &mut World, id: ChangeSetId, result: &Result<Receipt, TxnError>) {
    match result {
        Ok(Receipt::Recorded { .. }) => acknowledge(w, id),
        Ok(Receipt::Duplicate) => {
            if w.in_doubt.contains(&id) || w.oracle.acked.contains_key(&id) {
                // "Already committed" is an acknowledgement: it promises durability.
                acknowledge(w, id);
            } else {
                violation(w, "I6", format!("brand new {id} reported as duplicate"));
            }
        }
        Err(e) => match e.class() {
            ErrorClass::Rejected => w.oracle.rejected.push(id),
            _ => {
                w.in_doubt.insert(id);
            }
        },
    }
}

fn commit_new(w: &mut World, k: &mut K) {
    let n = 1 + w.rng.below(4) as usize;
    let session = w.turns.iter().next().map(|(s, t)| TurnRef {
        session: *s,
        turn: *t,
    });
    let use_turn = session.is_some() && w.rng.below(3) == 0;
    let batch: Vec<ChangeSet> = (0..n)
        .map(|_| fresh_cs(w, k, if use_turn { session } else { None }))
        .collect();
    let ids: Vec<ChangeSetId> = batch.iter().map(|c| c.id).collect();
    let results = k.commit_batch(batch);
    for (id, r) in ids.into_iter().zip(results.iter()) {
        record(w, id, r);
    }
    check_derived_state(w, k);
}

/// Drives the public transaction states by hand instead of through `commit_batch`: log several
/// change sets, try a checkpoint while they are in flight (which must be refused), make them durable
/// and apply them one at a time.
fn lifecycle(w: &mut World, k: &mut K) {
    let n = 1 + w.rng.below(3) as usize;
    let mut logged = Vec::new();
    for _ in 0..n {
        let cs = fresh_cs(w, k, None);
        let id = cs.id;
        let step = k
            .begin(cs)
            .validate(&AllowAll)
            .and_then(|t| t.authorize(&AllowAll))
            .and_then(|t| k.prepare(t))
            .and_then(|o| match o {
                PrepareOutcome::Ready(t) => k.log(t).map(Some),
                PrepareOutcome::Duplicate(_) => Ok(None),
            });
        match step {
            Ok(Some(t)) => logged.push((id, t)),
            Ok(None) => record(w, id, &Ok(Receipt::Duplicate)),
            Err(e) => record(w, id, &Err(e)),
        }
    }
    if !logged.is_empty() && w.rng.below(2) == 0 && k.checkpoint().is_ok() {
        violation(
            w,
            "I9",
            "a checkpoint was accepted with change sets in flight".into(),
        );
    }
    // Sometimes the caller loses its transactions after logging them. The records are in the log, so
    // the ids are in doubt until the kernel settles them; they must then block nothing.
    if !logged.is_empty() && w.rng.below(4) == 0 {
        for (id, _) in &logged {
            w.in_doubt.insert(*id);
        }
        drop(logged);
        let _ = k.settle();
        if k.checkpoint().is_err() && !k.is_poisoned() {
            violation(
                w,
                "I9",
                "a checkpoint was refused after every in-flight change set was settled".into(),
            );
        }
        check_derived_state(w, k);
        return;
    }
    let (ids, txns): (Vec<ChangeSetId>, Vec<_>) = logged.into_iter().unzip();
    match k.sync(txns) {
        Ok(durable) => {
            for (id, t) in ids.into_iter().zip(durable) {
                let outcome = k.apply(t).map(|a| k.publish(a));
                record(w, id, &outcome);
            }
        }
        Err(e) => {
            for id in ids {
                record(w, id, &Err(e.clone()));
            }
        }
    }
    if w.rng.below(3) == 0 {
        let _ = k.checkpoint();
    }
    check_derived_state(w, k);
}

/// I9: what the kernel holds in memory must equal a rebuild from the log it wrote.
fn check_derived_state(w: &mut World, k: &K) {
    if k.is_poisoned() {
        return;
    }
    // A reader sees exactly the durable prefix: contiguous LSNs from 1 up to the durable LSN.
    if let Ok((records, _)) = k.read_from(1, usize::MAX) {
        let contiguous = records
            .iter()
            .enumerate()
            .all(|(i, r)| r.lsn == i as u64 + 1);
        if !contiguous || records.len() as u64 != k.durable_lsn() {
            violation(
                w,
                "I2",
                format!(
                    "a reader saw {} records but the durable LSN is {}",
                    records.len(),
                    k.durable_lsn()
                ),
            );
        }
    }
    if let Ok(report) = k.scrub() {
        for issue in &report.issues {
            // Logical findings are violations wherever they show up. Physical damage (torn or rotted
            // segments) is injected on purpose by the fault profiles, so it is not one here.
            match issue {
                Issue::DerivedStateMismatch { what } => violation(w, "I9", what.clone()),
                Issue::DuplicateId { .. } => {
                    violation(w, "I9", format!("scrub found {issue:?} during a run"));
                }
                Issue::HlcNotIncreasing { .. } => {
                    violation(w, "I4", format!("scrub found {issue:?} during a run"));
                }
                Issue::FencedWriteLogged { .. } => {
                    violation(w, "I12", format!("scrub found {issue:?} during a run"));
                }
                _ => {}
            }
        }
    }
}

fn resubmit(w: &mut World, k: &mut K) {
    let ids: Vec<ChangeSetId> = w.oracle.acked.keys().copied().collect();
    let Some(id) = ids
        .get(w.rng.below(ids.len().max(1) as u64) as usize)
        .copied()
    else {
        return;
    };
    let Some(e) = w.expected.get(&id).cloned() else {
        return;
    };
    let mut cs = ChangeSet::new(id, e.payload);
    cs.author = e.author;
    cs.turn = e.turn;
    cs.parents = e.parents;
    if let Ok(Receipt::Recorded { .. }) = k.commit(cs)
        && !w.oracle.exempt.contains(&id)
    {
        violation(
            w,
            "I6",
            format!("resubmitted {id} was recorded a second time"),
        );
    }
}

/// Retries an id whose earlier commit failed ambiguously. Either outcome is legitimate, but a
/// success or a duplicate must from then on be durable.
fn resubmit_in_doubt(w: &mut World, k: &mut K) {
    let ids: Vec<ChangeSetId> = w.in_doubt.iter().copied().collect();
    let Some(id) = ids
        .get(w.rng.below(ids.len().max(1) as u64) as usize)
        .copied()
    else {
        return;
    };
    let Some(e) = w.expected.get(&id).cloned() else {
        return;
    };
    let mut cs = ChangeSet::new(id, e.payload);
    cs.author = e.author;
    cs.turn = e.turn;
    cs.parents = e.parents;
    let r = k.commit(cs);
    record(w, id, &r);
}

/// A batch that repeats one id. The second occurrence must share the first one's fate: it may only
/// be acknowledged if the original was durable.
fn duplicate_batch(w: &mut World, k: &mut K) {
    let cs = fresh_cs(w, k, None);
    let id = cs.id;
    let results = k.commit_batch(vec![cs.clone(), cs]);
    let (first, second) = (results.first(), results.get(1));
    match (first, second) {
        (Some(Ok(Receipt::Recorded { .. })), Some(Ok(Receipt::Duplicate))) => {}
        (Some(Err(_)), Some(Ok(_))) => violation(
            w,
            "I6",
            format!("repeat of {id} in one batch was acknowledged although the original failed"),
        ),
        (Some(Ok(Receipt::Recorded { .. })), Some(other)) => violation(
            w,
            "I6",
            format!(
                "repeat of {id} in one batch got {other:?} instead of a duplicate acknowledgement"
            ),
        ),
        _ => {}
    }
    if let Some(r) = first {
        record(w, id, r);
    }
}

fn begin_turn(w: &mut World, k: &mut K) {
    let session = 1 + w.rng.below(3);
    if w.uncertain_sessions.contains(&session) {
        return;
    }
    let turn = w.turns.get(&session).copied().unwrap_or(0) + 1;
    match k.begin_turn(TurnRef { session, turn }) {
        Ok(()) => {
            w.turns.insert(session, turn);
        }
        Err(e) if e.class() == ErrorClass::Rejected => {}
        Err(_) => {
            // The marker may or may not have reached the disk; the true turn is now unknown.
            w.turns.remove(&session);
            w.uncertain_sessions.insert(session);
        }
    }
}

fn stale_write(w: &mut World, k: &mut K) {
    let candidates: Vec<(u64, u64)> = w
        .turns
        .iter()
        .filter(|(_, t)| **t > 1)
        .map(|(s, t)| (*s, *t))
        .collect();
    let Some((session, turn)) = candidates
        .get(w.rng.below(candidates.len().max(1) as u64) as usize)
        .copied()
    else {
        return;
    };
    let cs = fresh_cs(
        w,
        k,
        Some(TurnRef {
            session,
            turn: turn - 1,
        }),
    );
    let id = cs.id;
    match k.commit(cs) {
        Ok(Receipt::Recorded { .. }) => {
            acknowledge(w, id);
            if !w.exempt_sessions.contains(&session) {
                violation(
                    w,
                    "I12",
                    format!(
                        "stale write {id} from turn {} was accepted after turn {turn}",
                        turn - 1
                    ),
                );
            }
        }
        Ok(Receipt::Duplicate) => {}
        Err(e) if e.class() == ErrorClass::Rejected => {}
        Err(_) => {
            w.in_doubt.insert(id);
        }
    }
}

fn skew_clock(w: &mut World) {
    let now = w.clock.now_ms();
    if w.rng.below(2) == 0 {
        w.clock.advance(1 + w.rng.below(500));
    } else {
        w.clock.set(now.saturating_sub(1 + w.rng.below(3000)));
    }
}

/// After recovery, probe the behaviours that depend on state rebuilt from the log.
fn probe_after_recovery(w: &mut World, k: &mut K, present: &[ChangeSetId]) {
    // Present ids must still dedupe.
    let ids: Vec<ChangeSetId> = w
        .oracle
        .acked
        .keys()
        .copied()
        .filter(|id| present.contains(id) && !w.oracle.exempt.contains(id))
        .take(3)
        .collect();
    for id in ids {
        let Some(e) = w.expected.get(&id).cloned() else {
            continue;
        };
        let mut cs = ChangeSet::new(id, e.payload);
        cs.author = e.author;
        cs.turn = e.turn;
        cs.parents = e.parents;
        if let Ok(Receipt::Recorded { .. }) = k.commit(cs) {
            violation(w, "I6", format!("after recovery {id} was accepted again"));
        }
    }
    // The current turn must still be writable: losing turn state would lock every client out.
    let current: Vec<(u64, u64)> = w
        .turns
        .iter()
        .filter(|(s, _)| !w.exempt_sessions.contains(s))
        .map(|(s, t)| (*s, *t))
        .take(2)
        .collect();
    for (session, turn) in current {
        let cs = fresh_cs(w, k, Some(TurnRef { session, turn }));
        let id = cs.id;
        match k.commit(cs) {
            Ok(Receipt::Recorded { .. }) => acknowledge(w, id),
            Ok(Receipt::Duplicate) => {}
            Err(e) if e.class() == ErrorClass::Rejected => violation(
                w,
                "I12",
                format!(
                    "after recovery the current turn {turn} of session {session} was rejected: {e}"
                ),
            ),
            Err(_) => {
                w.in_doubt.insert(id);
            }
        }
    }
    // Stale turns must still be fenced.
    let stale: Vec<(u64, u64)> = w
        .turns
        .iter()
        .filter(|(s, t)| **t > 1 && !w.exempt_sessions.contains(s))
        .map(|(s, t)| (*s, *t))
        .take(2)
        .collect();
    for (session, turn) in stale {
        let cs = fresh_cs(
            w,
            k,
            Some(TurnRef {
                session,
                turn: turn - 1,
            }),
        );
        let id = cs.id;
        if let Ok(Receipt::Recorded { .. }) = k.commit(cs) {
            acknowledge(w, id);
            violation(
                w,
                "I12",
                format!(
                    "after recovery a stale write {id} from turn {} was accepted",
                    turn - 1
                ),
            );
        }
    }
}

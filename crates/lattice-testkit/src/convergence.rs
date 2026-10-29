//! N-replica convergence harness with a Jepsen-style checker (WP45).
//!
//! Two to five replicas of each CRDT family (last-writer-wins register, observed-remove set, move
//! tree) take generated local operations while a simulated network delays, reorders, duplicates
//! and drops messages (a dropped message is retransmitted later). Once the network is quiet the
//! checker requires:
//!
//! 1. every replica has the same digest (I5);
//! 2. no acknowledged local write is lost: its effect is present, or it is superseded by a
//!    deterministic rule (a later register write; a recorded remove; the serial order of the tree);
//! 3. the tree is acyclic.
//!
//! The network behaviour is chosen from the name of one of the five simulator fault profiles
//! ([`crate::scenario::profiles`]) plus the seed, so a `(seed, profile)` pair replays exactly.
//!
//! The [`Flaw`] marker types build replicas that deliberately break a rule, to prove the checker
//! can fail. They live here, in the harness; the kernel's own planted bugs are not touched.

use lattice_abi::Hlc;
use lattice_core::HlcClock;
use lattice_core::bugs::{self, Bug};
use lattice_core::crdt::{Crdt, LwwRegister, OrSet};
use lattice_core::crdt_tree::{NodeId, Op, Outcome, Tree};
use lattice_sim::{SeededRng, TraceHash};
use std::collections::BTreeMap;
use std::marker::PhantomData;
use std::ops::Range;

/// Ticks during which replicas generate local operations.
const GEN_TICKS: u64 = 40;
/// Chance per replica per tick of a local operation, parts per million.
const OP_PPM: u32 = 350_000;
/// A message is dropped at most this many times, so the network always quiesces.
const MAX_RETRANSMITS: u32 = 12;
/// Distinct tree nodes the generator uses (ids `2..2 + TREE_NODES`).
const TREE_NODES: u64 = 12;

/// Which rule a violation breaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    /// Replicas hold different state after quiescence.
    Divergence,
    /// An acknowledged write is neither present nor superseded.
    LostWrite,
    /// The tree contains a cycle.
    Cycle,
}

/// One broken rule, with enough to replay it.
#[derive(Debug, Clone)]
pub struct Violation {
    /// Seed of the run.
    pub seed: u64,
    /// Fault profile name of the run.
    pub profile: &'static str,
    /// CRDT family.
    pub family: &'static str,
    /// The rule broken.
    pub kind: Kind,
    /// Human-readable detail.
    pub detail: String,
}

/// Network behaviour derived from a profile name and a seed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetModel {
    /// Largest random delivery delay, in ticks.
    pub delay_max: u64,
    /// Chance a transmission is dropped, parts per million.
    pub drop_ppm: u32,
    /// Chance a delivered message is delivered twice, parts per million.
    pub dup_ppm: u32,
    /// Ticks before a dropped message is sent again.
    pub retransmit_after: u64,
}

impl NetModel {
    /// Maps a profile name and seed to network behaviour. The name sets the base severity, the
    /// seed adds a small deterministic jitter. An unknown name gets the harshest base.
    pub fn for_profile(profile: &str, seed: u64) -> Self {
        let (delay, drop, dup, retransmit_after) = match profile {
            "clean" => (2, 0, 0, 3),
            "crash-heavy" => (20, 0, 150_000, 5),
            "disk-faulty" => (6, 200_000, 50_000, 8),
            "hostile" => (30, 300_000, 250_000, 8),
            _ => (50, 450_000, 350_000, 12),
        };
        let mut r = SeededRng::new(seed ^ name_hash(profile));
        let jitter = |r: &mut SeededRng, base: u32| {
            if base == 0 {
                0
            } else {
                base.saturating_add(u32::try_from(r.below(40_000)).unwrap_or(0))
            }
        };
        Self {
            delay_max: delay + r.below(4),
            drop_ppm: jitter(&mut r, drop),
            dup_ppm: jitter(&mut r, dup),
            retransmit_after,
        }
    }
}

fn name_hash(name: &str) -> u64 {
    let mut h = TraceHash::default();
    h.write(name.as_bytes());
    h.value()
}

/// A replicated data type the harness can drive.
pub trait Replica: Clone {
    /// What is sent to peers after a local operation.
    type Msg: Clone;
    /// What is recorded when a local operation is acknowledged.
    type Write: Clone;
    /// Family name for reports.
    const FAMILY: &'static str;
    /// A fresh replica for `node`.
    fn new(node: u32) -> Self;
    /// Performs one generated local operation; `uniq` is unique across the whole run.
    fn local(&mut self, rng: &mut SeededRng, at: Hlc, uniq: u64) -> (Self::Msg, Self::Write);
    /// Handles a message from a peer (any order, any number of times).
    fn receive(&mut self, msg: &Self::Msg);
    /// Fingerprint of the replicated state.
    fn digest(&self) -> u64;
    /// Whether the structure has no cycle.
    fn is_acyclic(&self) -> bool {
        true
    }
    /// Describes the first acknowledged write whose effect is neither present nor superseded.
    fn lost_write(&self, writes: &[Self::Write]) -> Option<String>;
}

/// Deliberate defects for the planted-bug proofs. A family reads only the flags it knows.
pub trait Flaw: Clone {
    /// Merge replaces local state with the incoming state.
    const OVERWRITE_MERGE: bool = false;
    /// Every third local write is acknowledged but not applied.
    const LOSE_LOCAL: bool = false;
    /// Duplicate detection is missing: a repeat is re-executed as a new operation.
    const NO_DEDUP: bool = false;
    /// The cycle check is skipped.
    const SKIP_CYCLE_CHECK: bool = false;
    /// Some remote operations are silently dropped on receipt.
    const LOSE_REMOTE: bool = false;
}

/// No defect.
#[derive(Debug, Clone)]
pub struct Sound;
impl Flaw for Sound {}

/// Merge overwrites instead of merging.
#[derive(Debug, Clone)]
pub struct OverwriteMerge;
impl Flaw for OverwriteMerge {
    const OVERWRITE_MERGE: bool = true;
}

/// Local writes are acknowledged then lost.
#[derive(Debug, Clone)]
pub struct LoseLocal;
impl Flaw for LoseLocal {
    const LOSE_LOCAL: bool = true;
}

/// No duplicate detection.
#[derive(Debug, Clone)]
pub struct NoDedup;
impl Flaw for NoDedup {
    const NO_DEDUP: bool = true;
}

/// Tree cycle check skipped.
#[derive(Debug, Clone)]
pub struct SkipCycleCheck;
impl Flaw for SkipCycleCheck {
    const SKIP_CYCLE_CHECK: bool = true;
}

/// Remote operations lost on receipt.
#[derive(Debug, Clone)]
pub struct LoseRemote;
impl Flaw for LoseRemote {
    const LOSE_REMOTE: bool = true;
}

// ---------------------------------------------------------------------------------------------
// Register

/// An acknowledged register write.
#[derive(Debug, Clone, Copy)]
pub struct RegWrite {
    /// Timestamp of the write.
    pub at: Hlc,
    /// Value, unique per write.
    pub v: u64,
}

/// A last-writer-wins register replica (state-based gossip).
#[derive(Debug, Clone)]
pub struct RegReplica<F: Flaw = Sound> {
    reg: LwwRegister<u64>,
    _f: PhantomData<F>,
}

impl<F: Flaw> Replica for RegReplica<F> {
    type Msg = LwwRegister<u64>;
    type Write = RegWrite;
    const FAMILY: &'static str = "register";

    fn new(_node: u32) -> Self {
        Self {
            reg: LwwRegister::default(),
            _f: PhantomData,
        }
    }

    fn local(&mut self, _rng: &mut SeededRng, at: Hlc, uniq: u64) -> (Self::Msg, Self::Write) {
        if !(F::LOSE_LOCAL && uniq.is_multiple_of(3)) {
            self.reg.set(at, uniq);
        }
        (self.reg.clone(), RegWrite { at, v: uniq })
    }

    fn receive(&mut self, msg: &Self::Msg) {
        if F::OVERWRITE_MERGE {
            self.reg = msg.clone();
        } else {
            self.reg.merge(msg);
        }
    }

    fn digest(&self) -> u64 {
        let mut h = TraceHash::default();
        h.write_u64(self.reg.get().map_or(u64::MAX, |v| *v));
        h.value()
    }

    fn lost_write(&self, writes: &[RegWrite]) -> Option<String> {
        let cur = self.reg.get().copied();
        // The held value must come from a write at least as late as every acknowledged write.
        let held_at = cur.and_then(|v| writes.iter().find(|w| w.v == v).map(|w| w.at));
        writes.iter().find_map(|w| match held_at {
            Some(at) if at >= w.at => None,
            _ => Some(format!(
                "write {} at {:?} is neither held nor superseded (holds {:?})",
                w.v, w.at, cur
            )),
        })
    }
}

// ---------------------------------------------------------------------------------------------
// Set

/// An acknowledged set operation. Added values are unique, a remove names a value it observed.
#[derive(Debug, Clone, Copy)]
pub enum SetWrite {
    /// Added value.
    Add(u64),
    /// Removed value.
    Remove(u64),
}

/// An observed-remove set replica (state-based gossip).
#[derive(Debug, Clone)]
pub struct SetReplica<F: Flaw = Sound> {
    set: OrSet<u64>,
    _f: PhantomData<F>,
}

impl<F: Flaw> Replica for SetReplica<F> {
    type Msg = OrSet<u64>;
    type Write = SetWrite;
    const FAMILY: &'static str = "set";

    fn new(_node: u32) -> Self {
        Self {
            set: OrSet::default(),
            _f: PhantomData,
        }
    }

    fn local(&mut self, rng: &mut SeededRng, at: Hlc, uniq: u64) -> (Self::Msg, Self::Write) {
        let present = self.set.elements();
        let pick = usize::try_from(rng.below(present.len() as u64)).unwrap_or(0);
        let write = match present.get(pick) {
            Some(v) if rng.chance_ppm(300_000) => {
                self.set.remove(v);
                SetWrite::Remove(*v)
            }
            _ => {
                if !(F::LOSE_LOCAL && uniq.is_multiple_of(3)) {
                    self.set.add(uniq, at);
                }
                SetWrite::Add(uniq)
            }
        };
        (self.set.clone(), write)
    }

    fn receive(&mut self, msg: &Self::Msg) {
        if F::OVERWRITE_MERGE {
            self.set = msg.clone();
        } else {
            self.set.merge(msg);
        }
    }

    fn digest(&self) -> u64 {
        let mut h = TraceHash::default();
        for v in self.set.elements() {
            h.write_u64(v);
        }
        h.value()
    }

    fn lost_write(&self, writes: &[SetWrite]) -> Option<String> {
        let removed: Vec<u64> = writes
            .iter()
            .filter_map(|w| match w {
                SetWrite::Remove(v) => Some(*v),
                SetWrite::Add(_) => None,
            })
            .collect();
        writes.iter().find_map(|w| match w {
            SetWrite::Add(v) if !removed.contains(v) && !self.set.contains(v) => {
                Some(format!("acknowledged add of {v} is missing"))
            }
            SetWrite::Add(v) if removed.contains(v) && self.set.contains(v) => {
                Some(format!("acknowledged remove of {v} was undone"))
            }
            _ => None,
        })
    }
}

// ---------------------------------------------------------------------------------------------
// Tree

/// A move-tree replica (operation-based).
#[derive(Debug, Clone)]
pub struct TreeReplica<F: Flaw = Sound> {
    tree: Tree,
    _f: PhantomData<F>,
}

impl<F: Flaw> TreeReplica<F> {
    fn apply(&mut self, op: Op) -> Option<Outcome> {
        if F::SKIP_CYCLE_CHECK {
            bugs::enable(Bug::TreeCycleCheckSkipped);
        }
        let r = self.tree.apply(op).ok();
        if F::SKIP_CYCLE_CHECK {
            bugs::clear();
        }
        r
    }
}

impl<F: Flaw> Replica for TreeReplica<F> {
    type Msg = Op;
    type Write = Op;
    const FAMILY: &'static str = "tree";

    fn new(_node: u32) -> Self {
        Self {
            tree: Tree::new(),
            _f: PhantomData,
        }
    }

    fn local(&mut self, rng: &mut SeededRng, at: Hlc, _uniq: u64) -> (Op, Op) {
        let node = NodeId(2 + rng.below(TREE_NODES));
        let op = if rng.chance_ppm(150_000) {
            Op::delete(at, node)
        } else {
            let parent = if rng.chance_ppm(250_000) {
                NodeId::ROOT
            } else {
                NodeId(2 + rng.below(TREE_NODES))
            };
            Op::new(at, node, parent, rng.below(4))
        };
        self.apply(op);
        (op, op)
    }

    fn receive(&mut self, op: &Op) {
        if F::LOSE_REMOTE && op.id.wall_ms / 5 % 4 == 1 {
            return;
        }
        if self.apply(*op) == Some(Outcome::Duplicate) && F::NO_DEDUP {
            let mut again = *op;
            again.id.counter = again.id.counter.wrapping_add(7919);
            self.apply(again);
        }
    }

    fn digest(&self) -> u64 {
        let mut h = TraceHash::default();
        for (n, p, s) in self.tree.snapshot().entries {
            h.write_u64(n.0);
            h.write_u64(p.0);
            h.write_u64(s);
        }
        h.value()
    }

    fn is_acyclic(&self) -> bool {
        self.tree.is_acyclic()
    }

    fn lost_write(&self, writes: &[Op]) -> Option<String> {
        // Independent oracle: apply every acknowledged operation once, in id order, to a sound tree.
        let mut ordered = writes.to_vec();
        ordered.sort_by_key(|o| o.id);
        let mut serial = Tree::new();
        for o in &ordered {
            let _ = serial.apply(*o);
        }
        if self.tree.op_count() < serial.op_count() {
            return Some(format!(
                "replica knows {} of {} acknowledged operations",
                self.tree.op_count(),
                serial.op_count()
            ));
        }
        (self.tree.snapshot() != serial.snapshot())
            .then(|| "tree differs from the serial replay of acknowledged operations".to_string())
    }
}

// ---------------------------------------------------------------------------------------------
// Harness

/// What one run (or the sum of several) measured.
#[derive(Debug, Clone, Default)]
pub struct Run {
    /// Broken rules.
    pub violations: Vec<Violation>,
    /// Fingerprint of deliveries and final digests.
    pub trace: u64,
    /// Simulations (one family, one seed, one profile) included.
    pub sims: u64,
    /// Acknowledged local writes.
    pub writes: u64,
    /// Transmissions dropped and retransmitted.
    pub dropped: u64,
    /// Messages delivered twice.
    pub duplicated: u64,
    /// Message copies delivered.
    pub delivered: u64,
    /// Smallest and largest replica count seen (0 when empty).
    pub replicas_min: usize,
    /// Largest replica count seen.
    pub replicas_max: usize,
}

impl Run {
    /// Adds another run into this one.
    pub fn absorb(&mut self, o: Run) {
        self.violations.extend(o.violations);
        self.trace = self.trace.rotate_left(7) ^ o.trace;
        self.sims += o.sims;
        self.writes += o.writes;
        self.dropped += o.dropped;
        self.duplicated += o.duplicated;
        self.delivered += o.delivered;
        self.replicas_min = match (self.replicas_min, o.replicas_min) {
            (0, b) => b,
            (a, 0) => a,
            (a, b) => a.min(b),
        };
        self.replicas_max = self.replicas_max.max(o.replicas_max);
    }
}

/// Runs `n` replicas of family `R` once and checks them.
pub fn run_family<R: Replica>(seed: u64, profile: &'static str, n: usize) -> Run {
    let mut rng = SeededRng::new(seed ^ name_hash(profile) ^ name_hash(R::FAMILY).rotate_left(21));
    let net = NetModel::for_profile(profile, seed);
    let mut replicas: Vec<R> = (0..n).map(|i| R::new(i as u32)).collect();
    let mut clocks: Vec<HlcClock> = (0..n).map(|i| HlcClock::new(i as u32)).collect();
    let skew: Vec<u64> = (0..n).map(|_| rng.below(40)).collect();
    let mut queue: BTreeMap<(u64, u64), (usize, R::Msg)> = BTreeMap::new();
    let mut writes: Vec<R::Write> = Vec::new();
    let mut run = Run {
        sims: 1,
        replicas_min: n,
        replicas_max: n,
        ..Run::default()
    };
    let mut trace = TraceHash::default();
    let (mut seq, mut uniq) = (0u64, 1u64);

    let mut deliver =
        |replicas: &mut Vec<R>, run: &mut Run, key: (u64, u64), to: usize, m: R::Msg| {
            if let Some(r) = replicas.get_mut(to) {
                r.receive(&m);
            }
            trace.write_u64(key.0);
            trace.write_u64(to as u64);
            run.delivered += 1;
        };

    for tick in 0..GEN_TICKS {
        while let Some(entry) = queue.first_entry() {
            if entry.key().0 > tick {
                break;
            }
            let (key, (to, m)) = entry.remove_entry();
            deliver(&mut replicas, &mut run, key, to, m);
        }
        for i in 0..n {
            if !rng.chance_ppm(OP_PPM) {
                continue;
            }
            let now = tick * 5 + skew.get(i).copied().unwrap_or(0);
            let Some(Ok(at)) = clocks.get_mut(i).map(|c| c.tick(now)) else {
                continue;
            };
            let Some(rep) = replicas.get_mut(i) else {
                continue;
            };
            let (msg, w) = rep.local(&mut rng, at, uniq);
            uniq += 1;
            writes.push(w);
            for j in (0..n).filter(|j| *j != i) {
                let mut t = tick;
                let mut tries = 0;
                while tries < MAX_RETRANSMITS && rng.chance_ppm(net.drop_ppm) {
                    run.dropped += 1;
                    tries += 1;
                    t += net.retransmit_after;
                }
                let at1 = t + 1 + rng.below(net.delay_max + 1);
                queue.insert((at1, seq), (j, msg.clone()));
                seq += 1;
                if rng.chance_ppm(net.dup_ppm) {
                    run.duplicated += 1;
                    let at2 = t + 1 + rng.below(net.delay_max * 2 + 2);
                    queue.insert((at2, seq), (j, msg.clone()));
                    seq += 1;
                }
            }
        }
    }
    // Quiescence: nothing new is generated, every retransmission is eventually delivered.
    while let Some(entry) = queue.first_entry() {
        let (key, (to, m)) = entry.remove_entry();
        deliver(&mut replicas, &mut run, key, to, m);
    }

    run.writes = writes.len() as u64;
    let digests: Vec<u64> = replicas.iter().map(Replica::digest).collect();
    for d in &digests {
        trace.write_u64(*d);
    }
    run.trace = trace.value();
    let mut report = |kind, detail: String| {
        run.violations.push(Violation {
            seed,
            profile,
            family: R::FAMILY,
            kind,
            detail,
        });
    };
    if digests.windows(2).any(|w| w.first() != w.get(1)) {
        report(Kind::Divergence, format!("digests {digests:x?}"));
    }
    for (i, r) in replicas.iter().enumerate() {
        if let Some(d) = r.lost_write(&writes) {
            report(Kind::LostWrite, format!("replica {i}: {d}"));
        }
        if !r.is_acyclic() {
            report(Kind::Cycle, format!("replica {i} has a cycle"));
        }
    }
    run
}

/// Runs all three sound families for one seed and profile.
pub fn run_sound(seed: u64, profile: &'static str, n: usize) -> Run {
    let mut run = run_family::<RegReplica>(seed, profile, n);
    run.absorb(run_family::<SetReplica>(seed, profile, n));
    run.absorb(run_family::<TreeReplica>(seed, profile, n));
    run
}

/// The replica count for a seed and profile index: 2 to 5.
pub fn replicas_for(seed: u64, profile_index: usize) -> usize {
    2 + ((seed as usize).wrapping_add(profile_index) % 4)
}

/// Runs `f` for every seed in `seeds` under each of the five fault profile names and sums the result.
pub fn sweep(seeds: Range<u64>, f: impl Fn(u64, &'static str, usize) -> Run) -> Run {
    let mut total = Run::default();
    for seed in seeds {
        for (i, (name, _)) in crate::scenario::profiles().iter().enumerate() {
            total.absorb(f(seed, name, replicas_for(seed, i)));
        }
    }
    total
}

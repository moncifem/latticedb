//! Fencing races (WP46, issue 248): a superseded turn is fenced and writes nothing, also when the
//! takeover lands in the middle of an append or while a watcher is reconnecting.
//!
//! Two scenarios run on the simulated disk under the five fault profiles:
//!
//! * `takeover during append`: a scripted mix of takeovers, writes of the current turn, writes of
//!   superseded turns (single and mixed batches, resubmissions, future turns) and writes whose
//!   typestate steps straddle a takeover (prepare, takeover, log; and log, takeover, sync).
//! * `takeover during watch reconnect`: a writer holds a turn and has unsent writes buffered, its
//!   connection drops, another writer takes over and writes, the watcher reconnects with its cursor
//!   and the old writer then flushes its buffered and unacknowledged writes.
//!
//! The oracle is the scenario script, not the kernel: the script alone decides which turn of which
//! session is current at every logical step, hence which write may be recorded. The assertions are
//! then checked against what the kernel answered and what the log holds. The plugin contract's
//! reference plugin cannot be used here: `lattice-testkit` is not allowed to depend on
//! `lattice-sdk` in `layers.toml`, so the kernel is the system under test and the contract rules
//! (formats/plugin-contract-v1.md, "Turns and fencing") are mapped onto it: fenced is
//! `TxnError::Fenced`, a future turn is `UnknownTurn`, the watch is a cursor over the durable log.
//!
//! Every assertion is proven able to fail: wrapper "buggy kernels" and planted kernel defects must
//! each be caught under the assertion named for them.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::print_stderr
)]

use lattice_abi::{Classify, ErrorClass};
use lattice_core::bugs::{self, Bug};
use lattice_core::changeset::{ChangeSet, ChangeSetId, TurnRef};
use lattice_core::{AllowAll, Config, Kernel, LogError, PrepareOutcome, Receipt, TxnError};
use lattice_seams::Rng as _;
use lattice_sim::{FaultProfile, SeededRng, SimClock, SimIo};
use lattice_testkit::invariants::{Invariant, LogView, Oracle, TurnsFenced};
use lattice_testkit::scenario::{SharedClock, SharedIo, profiles};
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

type K = Kernel<SharedIo, SharedClock, SeededRng>;
type Results = Vec<Result<Receipt, TxnError>>;

// Assertion tags. A failure message starts with one of these, and each mutant below must be caught
// under the tag named for it.
const LATE_ACCEPTED: &str = "LATE_ACCEPTED";
const FUTURE_ACCEPTED: &str = "FUTURE_ACCEPTED";
const NOT_FENCED: &str = "NOT_FENCED";
const WROTE_WHILE_FENCING: &str = "WROTE_WHILE_FENCING";
const DUP_BEFORE_FENCE: &str = "DUP_BEFORE_FENCE";
const CURRENT_REFUSED: &str = "CURRENT_REFUSED";
const LOG_HAS_SUPERSEDED: &str = "LOG_HAS_SUPERSEDED";
const LATE_AFTER_RECOVERY: &str = "LATE_AFTER_RECOVERY";
const LOST_ACKED: &str = "LOST_ACKED";
const WATCH_GAP: &str = "WATCH_GAP";
const WATCH_SUPERSEDED: &str = "WATCH_SUPERSEDED";
const WATCH_MISSED: &str = "WATCH_MISSED";
const TURN_BEGIN: &str = "TURN_BEGIN";
const CROSS_CHECK: &str = "CROSS_CHECK";

// ---------------------------------------------------------------------------------------------
// Systems under test: the honest kernel and wrappers with a planted fencing defect.

struct Ctx<'a> {
    current: &'a BTreeMap<u64, u64>,
    /// Sessions that saw a reconnect since their latest takeover.
    reconnected: &'a BTreeSet<u64>,
}

impl Ctx<'_> {
    fn is_stale(&self, cs: &ChangeSet) -> bool {
        cs.turn
            .is_some_and(|t| t.turn < self.current.get(&t.session).copied().unwrap_or(0))
    }
}

trait Sut {
    fn commit_batch(&mut self, k: &mut K, batch: Vec<ChangeSet>, ctx: &Ctx<'_>) -> Results;
}

struct Honest;
impl Sut for Honest {
    fn commit_batch(&mut self, k: &mut K, batch: Vec<ChangeSet>, _: &Ctx<'_>) -> Results {
        k.commit_batch(batch)
    }
}

/// Accepts a late write: a superseded turn is silently moved to the current one.
struct AcceptLate;
impl Sut for AcceptLate {
    fn commit_batch(&mut self, k: &mut K, batch: Vec<ChangeSet>, ctx: &Ctx<'_>) -> Results {
        k.commit_batch(rewrite_stale(batch, ctx, |_| true))
    }
}

/// Accepts a late write only after a reconnect (the fence is forgotten with the connection).
struct AcceptLateAfterReconnect;
impl Sut for AcceptLateAfterReconnect {
    fn commit_batch(&mut self, k: &mut K, batch: Vec<ChangeSet>, ctx: &Ctx<'_>) -> Results {
        let reconnected = ctx.reconnected;
        k.commit_batch(rewrite_stale(batch, ctx, |s| reconnected.contains(&s)))
    }
}

fn rewrite_stale(
    batch: Vec<ChangeSet>,
    ctx: &Ctx<'_>,
    when: impl Fn(u64) -> bool,
) -> Vec<ChangeSet> {
    batch
        .into_iter()
        .map(|mut cs| {
            if ctx.is_stale(&cs)
                && let Some(t) = cs.turn
                && when(t.session)
            {
                cs.turn = Some(TurnRef {
                    session: t.session,
                    turn: ctx.current[&t.session],
                });
            }
            cs
        })
        .collect()
}

/// Runs `intercept` on every superseded write; the writes it declines go to the kernel as a batch.
fn split_batch(
    k: &mut K,
    batch: Vec<ChangeSet>,
    ctx: &Ctx<'_>,
    mut intercept: impl FnMut(&mut K, ChangeSet) -> Result<Receipt, TxnError>,
) -> Results {
    let mut out: Vec<Option<Result<Receipt, TxnError>>> = vec![None; batch.len()];
    let mut rest = Vec::new();
    for (i, cs) in batch.into_iter().enumerate() {
        if ctx.is_stale(&cs) {
            out[i] = Some(intercept(k, cs));
        } else {
            rest.push((i, cs));
        }
    }
    let res = k.commit_batch(rest.iter().map(|(_, c)| c.clone()).collect());
    for ((i, _), r) in rest.into_iter().zip(res) {
        out[i] = Some(r);
    }
    out.into_iter().map(Option::unwrap).collect()
}

/// The duplicate set also remembers fenced writes and is consulted before the fence: a retry of a
/// fenced write is answered `Duplicate`.
#[derive(Default)]
struct FenceAfterDuplicate(BTreeSet<ChangeSetId>);
impl Sut for FenceAfterDuplicate {
    fn commit_batch(&mut self, k: &mut K, batch: Vec<ChangeSet>, ctx: &Ctx<'_>) -> Results {
        let seen = &mut self.0;
        split_batch(k, batch, ctx, |k, cs| {
            if seen.insert(cs.id) {
                k.commit(cs) // first sight: the real fence answers
            } else {
                Ok(Receipt::Duplicate)
            }
        })
    }
}

/// Reports `Fenced` but writes the record anyway (without its turn).
struct WritesWhileFencing;
impl Sut for WritesWhileFencing {
    fn commit_batch(&mut self, k: &mut K, batch: Vec<ChangeSet>, ctx: &Ctx<'_>) -> Results {
        let current = ctx.current;
        split_batch(k, batch, ctx, |k, cs| {
            let t = cs.turn.unwrap();
            let fenced = TxnError::Fenced {
                session: t.session,
                turn: t.turn,
                current: current[&t.session],
            };
            let mut leak = cs;
            leak.turn = None;
            let _ = k.commit(leak);
            Err(fenced)
        })
    }
}

// ---------------------------------------------------------------------------------------------
// The simulation: script state (the oracle) next to the kernel.

#[derive(Clone)]
struct Attempt {
    cs: ChangeSet,
    /// Some submission of this id happened while its turn was current in the script.
    ever_legit: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Expect {
    Current,
    Superseded,
    Future,
}

impl Expect {
    fn of(t: TurnRef, current: u64) -> Self {
        if current >= 1 && t.turn == current {
            Self::Current
        } else if t.turn < current {
            Self::Superseded
        } else {
            Self::Future
        }
    }
}

struct Sim<S: Sut> {
    sut: S,
    seed: u64,
    profile: &'static str,
    disk: SharedIo,
    clock: Rc<SimClock>,
    config: Config,
    rng: SeededRng,
    kernel_rng: SeededRng,
    faults: bool,
    k: Option<K>,
    // The oracle: current turn per session, from the script alone.
    current: BTreeMap<u64, u64>,
    reconnected: BTreeSet<u64>,
    next_id: u128,
    attempts: BTreeMap<ChangeSetId, Attempt>,
    acked: BTreeSet<ChangeSetId>,
    in_doubt: BTreeSet<ChangeSetId>,
    fails: Vec<String>,
    censored: bool,
    // Counters, reported as measured facts.
    stale_attempts: u64,
    fenced: u64,
    takeovers: u64,
    reconnects: u64,
    // The watcher's cursor and what it has seen.
    cursor: u64,
    feed: Vec<(u64, ChangeSetId)>,
}

impl<S: Sut> Sim<S> {
    fn new(seed: u64, profile: &'static str, fp: FaultProfile, sut: S) -> Self {
        Self {
            sut,
            seed,
            profile,
            disk: SharedIo::new(SimIo::new(seed, fp)),
            clock: Rc::new(SimClock::new(1_000_000)),
            config: Config {
                dir: "wal".into(),
                segment_limit: 512,
                node: 1,
            },
            rng: SeededRng::new(seed ^ 0xF3A1_0C0D_77E5_1234),
            kernel_rng: SeededRng::new(seed.wrapping_mul(0x9E37_79B9) ^ 0x12),
            faults: profile != "clean",
            k: None,
            current: BTreeMap::new(),
            reconnected: BTreeSet::new(),
            next_id: (u128::from(seed) << 64) | 1,
            attempts: BTreeMap::new(),
            acked: BTreeSet::new(),
            in_doubt: BTreeSet::new(),
            fails: Vec::new(),
            censored: false,
            stale_attempts: 0,
            fenced: 0,
            takeovers: 0,
            reconnects: 0,
            cursor: 0,
            feed: Vec::new(),
        }
    }

    fn fail(&mut self, tag: &str, detail: String) {
        self.fails.push(format!(
            "{tag}: profile {} seed {}: {detail}",
            self.profile, self.seed
        ));
    }

    fn cur(&self, session: u64) -> u64 {
        self.current.get(&session).copied().unwrap_or(0)
    }

    fn fresh(&mut self, session: u64, turn: u64) -> ChangeSet {
        let id = ChangeSetId::from_raw(self.next_id);
        self.next_id += 1;
        let mut cs = ChangeSet::new(id, vec![(self.rng.next_u64() & 0xFF) as u8; 4]);
        cs.author = u128::from(session);
        cs.turn = Some(TurnRef { session, turn });
        cs
    }

    /// Opens (or reopens) the kernel through injected faults. False means no kernel could be had and
    /// the run is censored (counted in the totals, never taken for a pass).
    fn ensure_open(&mut self) -> bool {
        if let Some(k) = &self.k
            && !k.is_poisoned()
        {
            return true;
        }
        self.k = None;
        self.reopen(self.faults)
    }

    fn reopen(&mut self, arm: bool) -> bool {
        for _ in 0..8 {
            if self.disk.is_powered_off() || self.rng.below(4) != 0 {
                self.disk.crash();
            } else {
                self.disk.restart_process();
            }
            if arm && self.rng.below(6) == 0 {
                self.disk.arm_power_cut(self.rng.below(6));
            }
            let opened = Kernel::open_streaming(
                self.disk.clone(),
                SharedClock(self.clock.clone()),
                self.kernel_rng.fork(),
                &self.config,
                |_| {},
            );
            match opened {
                Ok((k, _, _)) => {
                    self.k = Some(k);
                    self.reconnects += 1;
                    self.reconnected.extend(self.current.keys().copied());
                    if self.disk.rot_events() > 0 {
                        self.censored = true;
                    }
                    return true;
                }
                Err(TxnError::Log(LogError::Corrupt(_))) => break,
                Err(_) => {}
            }
        }
        self.censored = true;
        false
    }

    /// The client drops the connection: all kernel memory is lost, the disk survives.
    fn disconnect(&mut self) {
        self.k = None;
    }

    // ----- script steps ------------------------------------------------------------------

    /// A takeover: the script says the turn is now `n`; the kernel must agree once the call settles.
    fn take(&mut self, session: u64) {
        let n = self.cur(session) + 1;
        self.current.insert(session, n);
        self.reconnected.remove(&session);
        self.takeovers += 1;
        for _ in 0..16 {
            if !self.ensure_open() {
                return;
            }
            let k = self.k.as_mut().unwrap();
            match k.begin_turn(TurnRef { session, turn: n }) {
                Ok(()) => return,
                // An earlier ambiguous attempt reached the disk.
                Err(TxnError::StaleTurn { current, .. }) if current == n => return,
                Err(e) if e.class() == ErrorClass::Rejected => {
                    if !self.censored {
                        self.fail(TURN_BEGIN, format!("takeover to turn {n} refused: {e}"));
                    }
                    return;
                }
                // Ambiguous: retry, reopening if the log is poisoned.
                Err(_) => {}
            }
        }
        self.censored = true;
    }

    fn submit(&mut self, batch: Vec<ChangeSet>) {
        if !self.ensure_open() {
            return;
        }
        // Expectations come from the script state at this instant.
        let expect: Vec<Expect> = batch
            .iter()
            .map(|cs| {
                let t = cs.turn.unwrap();
                Expect::of(t, self.cur(t.session))
            })
            .collect();
        for (cs, e) in batch.iter().zip(&expect) {
            let a = self.attempts.entry(cs.id).or_insert(Attempt {
                cs: cs.clone(),
                ever_legit: false,
            });
            a.ever_legit |= *e == Expect::Current;
            if *e != Expect::Current {
                self.stale_attempts += 1;
            }
        }
        let lsn_before = self.k.as_ref().unwrap().durable_lsn();
        let was_poisoned = self.k.as_ref().unwrap().is_poisoned();
        let ctx = Ctx {
            current: &self.current,
            reconnected: &self.reconnected,
        };
        let results = self
            .sut
            .commit_batch(self.k.as_mut().unwrap(), batch.clone(), &ctx);
        let mut any_current = false;
        for ((cs, e), r) in batch.iter().zip(&expect).zip(results) {
            any_current |= *e == Expect::Current;
            self.judge(cs, *e, &r, was_poisoned);
        }
        // A batch with no current-turn write writes nothing at all.
        let after = self.k.as_ref().unwrap().durable_lsn();
        if !any_current && !was_poisoned && after != lsn_before {
            self.fail(
                WROTE_WHILE_FENCING,
                format!(
                    "a batch of superseded writes moved the durable lsn {lsn_before} -> {after}"
                ),
            );
        }
    }

    fn judge(&mut self, cs: &ChangeSet, e: Expect, r: &Result<Receipt, TxnError>, poisoned: bool) {
        let t = cs.turn.unwrap();
        let known = self.acked.contains(&cs.id) || self.in_doubt.contains(&cs.id);
        match (e, r) {
            (Expect::Current, Ok(Receipt::Recorded { .. })) => {
                self.in_doubt.remove(&cs.id);
                self.acked.insert(cs.id);
            }
            (Expect::Current, Ok(Receipt::Duplicate)) => {
                if known {
                    self.in_doubt.remove(&cs.id);
                    self.acked.insert(cs.id);
                } else {
                    self.fail(
                        DUP_BEFORE_FENCE,
                        format!("new write {} answered duplicate", cs.id),
                    );
                }
            }
            (Expect::Current, Err(TxnError::Fenced { .. })) => self.fail(
                CURRENT_REFUSED,
                format!("write of current turn {} was fenced", t.turn),
            ),
            (Expect::Current, Err(_)) => {
                self.in_doubt.insert(cs.id);
            }
            (Expect::Superseded, Ok(Receipt::Recorded { .. })) => self.fail(
                LATE_ACCEPTED,
                format!(
                    "{} from superseded turn {} (current {}) was recorded",
                    cs.id,
                    t.turn,
                    self.cur(t.session)
                ),
            ),
            (Expect::Superseded, Ok(Receipt::Duplicate)) => {
                // Contract: fence before duplicate. The kernel answers `Duplicate` for an id it
                // really stored (see the ignored test at the end); an id that was never stored
                // must never be called a duplicate.
                if !known {
                    self.fail(
                        DUP_BEFORE_FENCE,
                        format!(
                            "superseded write {} never stored, answered duplicate",
                            cs.id
                        ),
                    );
                }
            }
            (
                Expect::Superseded,
                Err(TxnError::Fenced {
                    session,
                    turn,
                    current,
                }),
            ) => {
                self.fenced += 1;
                let want = (t.session, t.turn, self.cur(t.session));
                if (*session, *turn, *current) != want {
                    self.fail(
                        NOT_FENCED,
                        format!("fence named {session}/{turn}/{current}, script says {want:?}"),
                    );
                }
            }
            (Expect::Superseded, Err(other)) => {
                if !poisoned {
                    self.fail(
                        NOT_FENCED,
                        format!("superseded write got {other} not Fenced"),
                    );
                }
            }
            (Expect::Future, Ok(Receipt::Recorded { .. })) => self.fail(
                FUTURE_ACCEPTED,
                format!(
                    "{} for turn {} that has not begun was recorded",
                    cs.id, t.turn
                ),
            ),
            (Expect::Future, _) => {}
        }
    }

    fn write_batch(&mut self, session: u64, turns: &[u64]) {
        let batch: Vec<ChangeSet> = turns.iter().map(|t| self.fresh(session, *t)).collect();
        self.submit(batch);
    }

    /// Resubmits an earlier attempt unchanged, as a client does when it saw no receipt.
    fn retry(&mut self) {
        let ids: Vec<ChangeSetId> = self.attempts.keys().copied().collect();
        if ids.is_empty() {
            return;
        }
        let id = ids[self.rng.below(ids.len() as u64) as usize];
        let cs = self.attempts[&id].cs.clone();
        self.submit(vec![cs]);
    }

    /// Takeover in the middle of an append, through the public typestate steps.
    fn straddle(&mut self, session: u64, before_log: bool) {
        if self.cur(session) == 0 || !self.ensure_open() {
            return;
        }
        let turn = self.cur(session);
        let cs = self.fresh(session, turn);
        let id = cs.id;
        self.attempts.insert(
            id,
            Attempt {
                cs: cs.clone(),
                ever_legit: false,
            },
        );
        let k = self.k.as_mut().unwrap();
        let prepared = k
            .begin(cs)
            .validate(&AllowAll)
            .and_then(|t| t.authorize(&AllowAll))
            .and_then(|t| k.prepare(t));
        let ready = match prepared {
            Ok(PrepareOutcome::Ready(t)) => t,
            _ => return, // nothing was logged; nothing to judge
        };
        if before_log {
            // prepare, TAKEOVER, log: the write passed its early fence check but is stale at the
            // linearization point, so the log step must refuse it.
            let opened_before = self.reconnects;
            self.take(session);
            // A prepared value belongs to the kernel instance that made it; if the takeover had
            // to reopen the kernel, the value is foreign and there is nothing to judge.
            let reopened_in_between = self.reconnects != opened_before;
            let Some(k) = self.k.as_mut() else { return };
            match k.log(ready) {
                Ok(_) => self.fail(
                    LATE_ACCEPTED,
                    format!("{id} prepared under turn {turn} was logged after the takeover"),
                ),
                Err(TxnError::Fenced { .. }) => self.fenced += 1,
                Err(TxnError::ForeignTransaction) if reopened_in_between => {}
                Err(e) if e.class() == ErrorClass::Rejected => self.fail(
                    NOT_FENCED,
                    format!("late log of {id} refused with {e}, not Fenced"),
                ),
                Err(_) => {}
            }
        } else {
            // log, TAKEOVER, sync: the write was logged while its turn was current, so it is a
            // legitimate write that the takeover must neither lose nor duplicate.
            self.attempts.get_mut(&id).unwrap().ever_legit = true;
            self.in_doubt.insert(id);
            let logged = match k.log(ready) {
                Ok(l) => l,
                Err(_) => return,
            };
            self.take(session);
            let Some(k) = self.k.as_mut() else { return };
            if k.is_poisoned() {
                return;
            }
            if let Ok(d) = k.sync(vec![logged])
                && let Some(d) = d.into_iter().next()
                && let Ok(a) = k.apply(d)
                && matches!(k.publish(a), Receipt::Recorded { .. })
            {
                self.in_doubt.remove(&id);
                self.acked.insert(id);
            }
        }
    }

    /// The watcher reads from its cursor and checks what it receives.
    fn poll(&mut self) {
        if !self.ensure_open() {
            return;
        }
        let Some(k) = self.k.as_ref() else { return };
        let Ok((records, _)) = k.read_from(self.cursor + 1, 4096) else {
            return;
        };
        for r in records {
            if r.lsn != self.cursor + 1 {
                self.fail(
                    WATCH_GAP,
                    format!("watcher at {} was sent lsn {}", self.cursor, r.lsn),
                );
            }
            self.cursor = r.lsn;
            let view = LogView::from_records(std::slice::from_ref(&r));
            for (lsn, cs) in view.change_sets {
                self.feed.push((lsn, cs.id));
                if !self.attempts.get(&cs.id).is_some_and(|a| a.ever_legit) {
                    self.fail(
                        WATCH_SUPERSEDED,
                        format!("watcher was delivered {} from a superseded turn", cs.id),
                    );
                }
            }
        }
    }

    fn maybe_arm(&mut self) {
        if self.faults
            && self.rng.below(5) == 0
            && let Some(k) = &self.k
            && !k.is_poisoned()
        {
            self.disk.arm_power_cut(self.rng.below(10));
        }
    }

    // ----- end of run ---------------------------------------------------------------------

    /// Terminal check on a disk without transient faults, so a short run still gets a verdict.
    fn finish(&mut self, fp: FaultProfile) {
        self.disk.set_profile(fp.without_transient_faults());
        self.k = None;
        if !self.reopen(false) || self.disk.rot_events() > 0 {
            // Bit rot or a refused recovery can drop turn records: the rest says nothing certain.
            self.censored = true;
            return;
        }
        // Probe the fences rebuilt from the log: a superseded turn and the current one.
        let sessions: Vec<u64> = self.current.keys().copied().collect();
        for s in sessions {
            let cur = self.cur(s);
            if cur >= 2 {
                let cs = self.fresh(s, cur - 1);
                let r = self.k.as_mut().unwrap().commit(cs.clone());
                self.attempts.insert(
                    cs.id,
                    Attempt {
                        cs,
                        ever_legit: false,
                    },
                );
                if !matches!(r, Err(TxnError::Fenced { .. })) {
                    self.fail(
                        LATE_AFTER_RECOVERY,
                        format!("after recovery a write of turn {} got {r:?}", cur - 1),
                    );
                }
            }
            let cs = self.fresh(s, cur);
            let id = cs.id;
            self.attempts.insert(
                id,
                Attempt {
                    cs: cs.clone(),
                    ever_legit: true,
                },
            );
            match self.k.as_mut().unwrap().commit(cs) {
                Ok(Receipt::Recorded { .. }) => {
                    self.acked.insert(id);
                }
                other => self.fail(
                    CURRENT_REFUSED,
                    format!("after recovery the current turn {cur} got {other:?}"),
                ),
            }
        }
        self.poll();
        let Some(k) = self.k.as_ref() else { return };
        let Ok((records, _)) = k.read_from(1, usize::MAX) else {
            self.censored = true;
            return;
        };
        let scrub_issues = k.scrub().map(|r| r.issues).unwrap_or_default();
        let view = LogView::from_records(&records);
        // Oracle: only writes the script made while their turn was current may be in the log.
        let mut in_log = BTreeSet::new();
        for (lsn, cs) in &view.change_sets {
            if !in_log.insert(cs.id) {
                self.fail(LOG_HAS_SUPERSEDED, format!("{} twice in the log", cs.id));
            }
            if !self.attempts.get(&cs.id).is_some_and(|a| a.ever_legit) {
                self.fail(
                    LOG_HAS_SUPERSEDED,
                    format!(
                        "lsn {lsn}: {} (turn {:?}) is a write the script made while superseded",
                        cs.id, cs.turn
                    ),
                );
            }
        }
        for id in self.acked.clone() {
            if !in_log.contains(&id) {
                self.fail(LOST_ACKED, format!("acknowledged {id} is not in the log"));
            }
        }
        // The turn markers in the log are exactly the script's takeovers, in order per session.
        for (s, n) in self.current.clone() {
            let begins: Vec<u64> = view
                .turn_begins
                .iter()
                .filter(|(_, b)| b.session == s)
                .map(|(_, b)| b.turn)
                .collect();
            let want: Vec<u64> = (1..=n).collect();
            if begins != want {
                self.fail(
                    TURN_BEGIN,
                    format!("session {s}: log has {begins:?}, script {want:?}"),
                );
            }
        }
        // A second opinion from the repository's own I12 checker; it is code under test, so the
        // script oracle above is what the verdict rests on.
        for v in TurnsFenced.check(&view, &Oracle::default()) {
            self.fail(CROSS_CHECK, v.detail);
        }
        for issue in &scrub_issues {
            if matches!(issue, lattice_core::scrub::Issue::FencedWriteLogged { .. }) {
                self.fail(LOG_HAS_SUPERSEDED, format!("scrub found {issue:?}"));
            }
        }
        // The watcher saw every recorded write once, in position order.
        let fed: BTreeSet<ChangeSetId> = self.feed.iter().map(|(_, id)| *id).collect();
        if fed.len() != self.feed.len() {
            self.fail(WATCH_GAP, "watcher saw a write twice".into());
        }
        if !self.feed.windows(2).all(|w| w[0].0 < w[1].0) {
            self.fail(WATCH_GAP, "watcher saw positions out of order".into());
        }
        if fed != in_log {
            self.fail(WATCH_MISSED, "watcher feed differs from the log".into());
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The two scenarios.

/// Scenario one: takeover during append.
fn scenario_append<S: Sut>(sim: &mut Sim<S>, steps: usize) {
    sim.take(1);
    sim.take(2);
    for _ in 0..steps {
        sim.maybe_arm();
        let s = 1 + sim.rng.below(2);
        let cur = sim.cur(s);
        match sim.rng.below(100) {
            0..=13 => sim.take(s),
            14..=33 => {
                let n = 1 + sim.rng.below(3) as usize;
                sim.write_batch(s, &vec![cur; n]);
            }
            34..=53 if cur >= 2 => {
                // A mixed batch: each record independently from the current or an older turn.
                let n = 1 + sim.rng.below(4) as usize;
                let turns: Vec<u64> = (0..n)
                    .map(|_| {
                        if sim.rng.below(2) == 0 {
                            cur
                        } else {
                            1 + sim.rng.below(cur - 1)
                        }
                    })
                    .collect();
                sim.write_batch(s, &turns);
            }
            54..=61 => sim.retry(),
            62..=65 => {
                let future = cur + 1 + sim.rng.below(2);
                sim.write_batch(s, &[future]);
            }
            66..=74 => sim.straddle(s, true),
            75..=82 => sim.straddle(s, false),
            83..=89 => sim.disconnect(),
            _ => sim.poll(),
        }
    }
}

/// Scenario two: takeover during a watch reconnect.
fn scenario_reconnect<S: Sut>(sim: &mut Sim<S>) {
    let s = 1;
    sim.take(s);
    let a_turn = sim.cur(s);
    // Writer A works under its turn and watches.
    for _ in 0..1 + sim.rng.below(3) {
        let n = 1 + sim.rng.below(3) as usize;
        sim.write_batch(s, &vec![a_turn; n]);
        sim.poll();
    }
    // A prepares writes it has not sent yet.
    let buffered: Vec<ChangeSet> = (0..1 + sim.rng.below(4))
        .map(|_| sim.fresh(s, a_turn))
        .collect();
    // A's connection drops. While it is away, B takes over and writes.
    sim.disconnect();
    sim.take(s);
    for _ in 0..1 + sim.rng.below(3) {
        let cur = sim.cur(s);
        sim.maybe_arm();
        sim.write_batch(s, &[cur, cur]);
    }
    // A reconnects with its cursor (possibly after more drops) and reads what it missed.
    for _ in 0..sim.rng.below(3) {
        sim.disconnect();
        sim.poll();
    }
    sim.poll();
    // A now flushes its buffered writes and retries everything it has no receipt for.
    sim.submit(buffered.clone());
    sim.submit(buffered);
    for _ in 0..4 {
        sim.retry();
    }
    sim.poll();
}

#[derive(Default)]
struct Totals {
    runs: u64,
    censored: u64,
    stale: u64,
    fenced: u64,
    takeovers: u64,
    reconnects: u64,
}

fn run_seed<S: Sut>(
    seed: u64,
    name: &'static str,
    fp: FaultProfile,
    mk: impl Fn() -> S,
    totals: &mut Totals,
) -> Vec<String> {
    let mut fails = Vec::new();
    for which in 0..2 {
        let mut sim = Sim::new(seed.wrapping_mul(2) + which, name, fp, mk());
        if which == 0 {
            scenario_append(&mut sim, 40);
        } else {
            scenario_reconnect(&mut sim);
        }
        sim.finish(fp);
        totals.runs += 1;
        totals.censored += u64::from(sim.censored);
        totals.stale += sim.stale_attempts;
        totals.fenced += sim.fenced;
        totals.takeovers += sim.takeovers;
        totals.reconnects += sim.reconnects;
        fails.extend(sim.fails);
    }
    fails
}

fn sweep<S: Sut>(
    seeds: u64,
    mk: impl Fn() -> S + Copy,
) -> (Vec<String>, Vec<(&'static str, Totals)>) {
    let mut fails = Vec::new();
    let mut all = Vec::new();
    for (name, fp) in profiles() {
        let mut t = Totals::default();
        for seed in 0..seeds {
            fails.extend(run_seed(seed, name, fp, mk, &mut t));
        }
        all.push((name, t));
    }
    (fails, all)
}

fn report_and_assert(seeds: u64) {
    let (fails, totals) = sweep(seeds, || Honest);
    for (name, t) in &totals {
        eprintln!(
            "fencing_races profile {name}: {} runs, {} censored, {} superseded writes, {} fenced, {} takeovers, {} reconnects",
            t.runs, t.censored, t.stale, t.fenced, t.takeovers, t.reconnects
        );
        assert!(
            t.censored * 2 <= t.runs,
            "profile {name}: most runs censored ({} of {}), the assertions would say little",
            t.censored,
            t.runs
        );
        assert!(t.fenced > 0, "profile {name} never exercised a fence");
    }
    assert!(
        fails.is_empty(),
        "{} failures, first: {:#?}",
        fails.len(),
        &fails[..fails.len().min(5)]
    );
}

// ---------------------------------------------------------------------------------------------
// Tests.

#[test]
fn superseded_turns_are_fenced_and_write_nothing_across_profiles() {
    report_and_assert(200);
}

/// Volume run (100,000 seeds per profile); left to scheduled CI.
#[test]
#[ignore = "volume run: 100,000 seeds per profile, left to scheduled CI"]
fn superseded_turns_are_fenced_across_profiles_volume() {
    report_and_assert(100_000);
}

/// Runs a mutant over a bounded number of seeds and returns the tags of what caught it.
fn caught_by<S: Sut>(mk: impl Fn() -> S + Copy) -> BTreeSet<String> {
    let (fails, _) = sweep(40, mk);
    fails
        .iter()
        .filter_map(|f| f.split(':').next().map(str::to_owned))
        .collect()
}

fn assert_caught(mutant: &str, tags: &BTreeSet<String>, want: &str) {
    assert!(
        tags.contains(want),
        "{mutant}: expected assertion {want} to fail, got {tags:?}"
    );
}

#[test]
fn mutant_accept_late_write_is_caught() {
    let tags = caught_by(|| AcceptLate);
    assert_caught("AcceptLate", &tags, LATE_ACCEPTED);
    assert_caught("AcceptLate", &tags, LOG_HAS_SUPERSEDED);
}

#[test]
fn mutant_accept_late_write_after_reconnect_is_caught() {
    let tags = caught_by(|| AcceptLateAfterReconnect);
    assert_caught("AcceptLateAfterReconnect", &tags, LATE_ACCEPTED);
}

#[test]
fn mutant_fence_check_after_duplicate_check_is_caught() {
    let tags = caught_by(FenceAfterDuplicate::default);
    assert_caught("FenceAfterDuplicate", &tags, DUP_BEFORE_FENCE);
}

#[test]
fn mutant_fenced_reply_but_record_written_is_caught() {
    let tags = caught_by(|| WritesWhileFencing);
    assert_caught("WritesWhileFencing", &tags, WROTE_WHILE_FENCING);
    assert_caught("WritesWhileFencing", &tags, LOG_HAS_SUPERSEDED);
}

#[test]
fn planted_kernel_bug_fence_ignored_is_caught() {
    bugs::enable(Bug::FenceIgnored);
    let tags = caught_by(|| Honest);
    bugs::clear();
    assert_caught("FenceIgnored", &tags, LATE_ACCEPTED);
    // The takeover that lands between prepare and log is caught as well.
    assert_caught("FenceIgnored", &tags, LOG_HAS_SUPERSEDED);
}

#[test]
fn planted_kernel_bug_fence_not_rebuilt_is_caught() {
    bugs::enable(Bug::FenceNotRebuilt);
    let tags = caught_by(|| Honest);
    bugs::clear();
    assert!(
        tags.contains(LATE_AFTER_RECOVERY)
            || tags.contains(LATE_ACCEPTED)
            || tags.contains(LOG_HAS_SUPERSEDED)
            || tags.contains(CURRENT_REFUSED),
        "FenceNotRebuilt not caught: {tags:?}"
    );
}

/// Same seed, same verdict and the same counters.
#[test]
fn runs_are_deterministic() {
    let fp = profiles()[4].1;
    let (mut t1, mut t2) = (Totals::default(), Totals::default());
    let a = run_seed(7, "everything", fp, || Honest, &mut t1);
    let b = run_seed(7, "everything", fp, || Honest, &mut t2);
    assert_eq!(a, b);
    assert_eq!(
        (t1.fenced, t1.takeovers, t1.reconnects),
        (t2.fenced, t2.takeovers, t2.reconnects)
    );
}

/// Divergence from the plugin contract, not run by default. formats/plugin-contract-v1.md says the
/// fence check comes before the duplicate check, "even if a record id is one already stored": the
/// late retry of a write that was recorded under the old turn is `fenced`. The kernel answers
/// `Duplicate` because `prepare` consults the committed-id set before `check_fence`. Nothing is
/// written either way, so I12 holds; the difference is the receipt. Whether the kernel or the
/// contract should change is a design decision for the owner.
#[test]
#[ignore = "kernel answers Duplicate where the plugin contract demands fenced for a stored id; decision pending"]
fn late_retry_of_a_stored_write_is_fenced_per_the_contract() {
    let disk = SharedIo::new(SimIo::new(1, FaultProfile::clean()));
    let config = Config {
        dir: "wal".into(),
        segment_limit: 512,
        node: 1,
    };
    let (mut k, _) = K::open(
        disk,
        SharedClock(Rc::new(SimClock::new(1_000_000))),
        SeededRng::new(1),
        &config,
    )
    .unwrap();
    let turn = |n| TurnRef {
        session: 1,
        turn: n,
    };
    k.begin_turn(turn(1)).unwrap();
    let mut cs = ChangeSet::new(ChangeSetId::from_raw(1), vec![1]);
    cs.turn = Some(turn(1));
    assert!(matches!(k.commit(cs.clone()), Ok(Receipt::Recorded { .. })));
    k.begin_turn(turn(2)).unwrap();
    assert!(matches!(k.commit(cs), Err(TxnError::Fenced { .. })));
}

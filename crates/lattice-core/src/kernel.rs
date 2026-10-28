//! The kernel: owns the log, the clock, the dedupe set and the turn fences, and drives
//! transactions through the typestate.
//!
//! Linearization: a change set's duplicate and fence checks are repeated at the moment it is
//! appended to the log ([`Kernel::log`]), and its timestamp is assigned there, so the order of log
//! records is also the order of timestamps and no prepared transaction can slip past a fence or an
//! id reservation made after it was prepared.

use crate::bugs::{self, Bug};
use crate::changeset::{ChangeSet, ChangeSetId, TurnRef, decode_turn_begin, encode_turn_begin};
use crate::hlc::HlcClock;
use crate::log::{Log, LogError, RecoveryReport};
use crate::metrics::{Metrics, names};
use crate::record::{Record, RecordType};
use crate::txn::{
    AllowAll, Applied, Authorized, Durable, Logged, Open, PrepareOutcome, Prepared, Txn, TxnError,
};
use lattice_abi::Hlc;
use lattice_seams::{Clock, Io, Rng};
use std::collections::{BTreeMap, BTreeSet};

/// Kernel configuration.
#[derive(Debug, Clone)]
pub struct Config {
    /// Directory (inside the `Io` root) holding the log.
    pub dir: String,
    /// Rotate to a new segment after this many bytes.
    pub segment_limit: u64,
    /// This node's id, stamped into hybrid logical clock timestamps.
    pub node: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            dir: "wal".into(),
            segment_limit: 1 << 20,
            node: 1,
        }
    }
}

/// What a commit returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Receipt {
    /// Newly committed and durable.
    Recorded {
        /// Position in the log.
        lsn: u64,
        /// The timestamp assigned.
        hlc: Hlc,
    },
    /// The id was already committed. Nothing was written.
    Duplicate,
}

/// What opening the kernel recovered.
#[derive(Debug, Clone)]
pub struct Recovery {
    /// Every valid record, in order.
    pub records: Vec<Record>,
    /// What the log repair did.
    pub report: RecoveryReport,
}

/// The database kernel for one node.
pub struct Kernel<I: Io, C: Clock, R: Rng> {
    log: Log<I>,
    clock: C,
    rng: R,
    hlc: HlcClock,
    /// Distinguishes this instance (and this recovery generation) from every other kernel.
    owner: u64,
    seen: BTreeSet<ChangeSetId>,
    in_flight: BTreeSet<ChangeSetId>,
    turns: BTreeMap<u64, u64>,
    metrics: Metrics,
}

fn set(
    results: &mut [Option<Result<Receipt, TxnError>>],
    i: usize,
    outcome: Result<Receipt, TxnError>,
) {
    if let Some(slot) = results.get_mut(i) {
        *slot = Some(outcome);
    }
}

fn corrupt(msg: String) -> TxnError {
    TxnError::Log(LogError::Corrupt(msg))
}

impl<I: Io, C: Clock, R: Rng> Kernel<I, C, R> {
    /// Opens (and recovers) the kernel. A CRC-valid record of a known type that does not decode is
    /// corruption, not something to skip: skipping would silently lose a fence or a dedupe entry.
    pub fn open(
        io: I,
        clock: C,
        mut rng: R,
        config: &Config,
    ) -> Result<(Self, Recovery), TxnError> {
        let (log, records, report) = Log::open(io, &config.dir, config.segment_limit)?;
        let mut seen = BTreeSet::new();
        let mut turns: BTreeMap<u64, u64> = BTreeMap::new();
        let mut highest = Hlc::default();
        for r in &records {
            match RecordType::from_u8(r.rtype) {
                Some(RecordType::ChangeSet) => {
                    let cs = ChangeSet::decode(&r.payload)
                        .map_err(|e| corrupt(format!("change set at lsn {}: {e}", r.lsn)))?;
                    seen.insert(cs.id);
                    highest = highest.max(cs.hlc);
                }
                Some(RecordType::TurnBegin) => {
                    let t = decode_turn_begin(&r.payload)
                        .map_err(|e| corrupt(format!("turn begin at lsn {}: {e}", r.lsn)))?;
                    let cur = turns.entry(t.session).or_insert(0);
                    *cur = (*cur).max(t.turn);
                }
                // Unknown record types are preserved as opaque records and have no kernel state.
                _ => {}
            }
        }
        if bugs::active(Bug::DedupeNotRebuilt) {
            seen.clear();
        }
        if bugs::active(Bug::FenceNotRebuilt) {
            turns.clear();
        }
        let owner = rng.next_u64();
        let kernel = Self {
            log,
            clock,
            rng,
            hlc: HlcClock::restore(config.node, highest),
            owner,
            seen,
            in_flight: BTreeSet::new(),
            turns,
            metrics: Metrics::default(),
        };
        Ok((kernel, Recovery { records, report }))
    }

    /// A fresh, unique change set id from the clock and random seams.
    pub fn new_id(&mut self) -> ChangeSetId {
        ChangeSetId::from_parts(
            self.clock.now_ms(),
            self.rng.next_u64(),
            self.rng.next_u64(),
        )
    }

    /// Starts a transaction bound to this kernel instance.
    pub fn begin(&self, cs: ChangeSet) -> Txn<Open> {
        Txn::open(cs, self.owner)
    }

    fn check_owner(&self, owner: u64) -> Result<(), TxnError> {
        if owner == self.owner {
            Ok(())
        } else {
            Err(TxnError::ForeignTransaction)
        }
    }

    fn check_fence(&self, cs: &ChangeSet) -> Result<(), TxnError> {
        let Some(t) = cs.turn else { return Ok(()) };
        if bugs::active(Bug::FenceIgnored) {
            return Ok(());
        }
        match self.turns.get(&t.session) {
            None => Err(TxnError::UnknownTurn(t.session)),
            Some(&cur) if t.turn < cur => Err(TxnError::Fenced {
                session: t.session,
                turn: t.turn,
                current: cur,
            }),
            Some(&cur) if t.turn > cur => Err(TxnError::UnknownTurn(t.session)),
            Some(_) => Ok(()),
        }
    }

    /// Early checks: duplicates and fencing. They are repeated when the change set is logged.
    pub fn prepare(&mut self, txn: Txn<Authorized>) -> Result<PrepareOutcome, TxnError> {
        let (cs, owner) = txn.into_parts();
        self.check_owner(owner)?;
        if !bugs::active(Bug::NoDedupe) {
            if self.seen.contains(&cs.id) {
                return Ok(PrepareOutcome::Duplicate(cs.id));
            }
            if self.in_flight.contains(&cs.id) {
                return Err(TxnError::InFlight(cs.id));
            }
        }
        self.check_fence(&cs)?;
        Ok(PrepareOutcome::Ready(Txn::prepared(cs, owner)))
    }

    /// Queues the change set in the log. This is the linearization point: duplicates and fences
    /// are checked again, and the timestamp is assigned in log order.
    pub fn log(&mut self, txn: Txn<Prepared>) -> Result<Txn<Logged>, TxnError> {
        self.check_owner(txn.owner)?;
        if self.log.is_poisoned() {
            return Err(TxnError::Log(LogError::Poisoned));
        }
        let _span = tracing::debug_span!("log", txn_id = %txn.cs.id).entered();
        let mut cs = txn.cs.clone();
        if !bugs::active(Bug::NoDedupe) {
            if self.seen.contains(&cs.id) {
                return Err(TxnError::Duplicate(cs.id));
            }
            if self.in_flight.contains(&cs.id) {
                return Err(TxnError::InFlight(cs.id));
            }
        }
        self.check_fence(&cs)?;
        cs.hlc = self.hlc.tick(self.clock.now_ms())?;
        let encoded = cs.encode()?;
        let lsn = self.log.append(RecordType::ChangeSet as u8, &encoded)?;
        self.in_flight.insert(cs.id);
        Ok(txn.logged(cs, lsn))
    }

    /// Makes everything logged so far durable (one fsync for the whole group) and checks that
    /// each transaction's record really is covered.
    pub fn sync(&mut self, txns: Vec<Txn<Logged>>) -> Result<Vec<Txn<Durable>>, TxnError> {
        for t in &txns {
            self.check_owner(t.owner)?;
        }
        let durable = self.log.sync()?;
        let mut out = Vec::with_capacity(txns.len());
        for t in txns {
            if t.lsn > durable {
                return Err(TxnError::NotDurable(t.lsn));
            }
            out.push(t.durable());
        }
        Ok(out)
    }

    /// Reflects a durable change set in memory.
    pub fn apply(&mut self, txn: Txn<Durable>) -> Result<Txn<Applied>, TxnError> {
        self.check_owner(txn.owner)?;
        if txn.lsn > self.log.durable_lsn() {
            return Err(TxnError::NotDurable(txn.lsn));
        }
        self.in_flight.remove(&txn.cs.id);
        self.seen.insert(txn.cs.id);
        Ok(txn.applied())
    }

    /// Turns an applied transaction into the receipt for the client.
    pub fn publish(&self, txn: Txn<Applied>) -> Receipt {
        Receipt::Recorded {
            lsn: txn.lsn(),
            hlc: txn.cs.hlc,
        }
    }

    /// Commits one change set through every state with the allow-all stubs.
    pub fn commit(&mut self, cs: ChangeSet) -> Result<Receipt, TxnError> {
        self.commit_batch(vec![cs])
            .into_iter()
            .next()
            .unwrap_or(Err(TxnError::Invalid("empty batch".into())))
    }

    /// Group commit: many change sets, one fsync. Each result is independent, except that a log
    /// failure fails every change set that had been queued. A repeated id inside the batch gets the
    /// same outcome as its first occurrence (so it is never acknowledged ahead of it).
    pub fn commit_batch(&mut self, batch: Vec<ChangeSet>) -> Vec<Result<Receipt, TxnError>> {
        let started = self.clock.now_ms();
        let results = self.commit_batch_inner(batch);
        let elapsed_ms = self.clock.now_ms().saturating_sub(started);
        self.metrics
            .observe(names::COMMIT_LATENCY_SECONDS, elapsed_ms as f64 / 1000.0);
        for r in &results {
            match r {
                Ok(Receipt::Recorded { .. }) => self.metrics.incr(names::COMMIT_TOTAL),
                Ok(Receipt::Duplicate) => self.metrics.incr(names::COMMIT_DUPLICATE_TOTAL),
                Err(_) => self.metrics.incr(names::COMMIT_ERROR_TOTAL),
            }
        }
        results
    }

    fn commit_batch_inner(&mut self, batch: Vec<ChangeSet>) -> Vec<Result<Receipt, TxnError>> {
        let n = batch.len();
        if self.log.is_poisoned() {
            return (0..n)
                .map(|_| Err(TxnError::Log(LogError::Poisoned)))
                .collect();
        }
        let mut results: Vec<Option<Result<Receipt, TxnError>>> = (0..n).map(|_| None).collect();
        let mut logged: Vec<(usize, Txn<Logged>)> = Vec::new();
        let mut first_in_batch: BTreeMap<ChangeSetId, usize> = BTreeMap::new();
        let mut repeats: Vec<(usize, usize)> = Vec::new();
        for (i, cs) in batch.into_iter().enumerate() {
            if !bugs::active(Bug::NoDedupe)
                && let Some(&first) = first_in_batch.get(&cs.id)
            {
                repeats.push((i, first));
                continue;
            }
            let id = cs.id;
            let step = self
                .begin(cs)
                .validate(&AllowAll)
                .and_then(|t| t.authorize(&AllowAll))
                .and_then(|t| self.prepare(t))
                .and_then(|o| match o {
                    PrepareOutcome::Duplicate(_) => Ok(None),
                    PrepareOutcome::Ready(t) => self.log(t).map(Some),
                });
            first_in_batch.insert(id, i);
            match step {
                Ok(None) => set(&mut results, i, Ok(Receipt::Duplicate)),
                Ok(Some(t)) => logged.push((i, t)),
                Err(e) => set(&mut results, i, Err(e)),
            }
        }
        if bugs::active(Bug::AckBeforeSync) {
            // Planted bug: acknowledge before the data is durable.
            for (i, t) in logged {
                let receipt = Receipt::Recorded {
                    lsn: t.lsn,
                    hlc: t.cs.hlc,
                };
                if let Some(slot) = results.get_mut(i) {
                    *slot = Some(Ok(receipt));
                }
            }
        } else {
            let (idxs, txns): (Vec<usize>, Vec<Txn<Logged>>) = logged.into_iter().unzip();
            match self.sync(txns) {
                Ok(durable) => {
                    for (i, t) in idxs.into_iter().zip(durable) {
                        let outcome = self.apply(t).map(|a| self.publish(a));
                        if let Some(slot) = results.get_mut(i) {
                            *slot = Some(outcome);
                        }
                    }
                }
                Err(e) => {
                    for i in idxs {
                        if let Some(slot) = results.get_mut(i) {
                            *slot = Some(Err(e.clone()));
                        }
                    }
                }
            }
        }
        for (i, first) in repeats {
            let derived = match results.get(first).and_then(Option::as_ref) {
                _ if bugs::active(Bug::BatchDuplicateAckedEarly) => Ok(Receipt::Duplicate),
                Some(Ok(_)) => Ok(Receipt::Duplicate),
                Some(Err(e)) => Err(e.clone()),
                None => Err(TxnError::Invalid("first occurrence has no outcome".into())),
            };
            if let Some(slot) = results.get_mut(i) {
                *slot = Some(derived);
            }
        }
        results
            .into_iter()
            .map(|r| r.unwrap_or_else(|| Err(TxnError::Invalid("no outcome recorded".into()))))
            .collect()
    }

    /// Starts a new turn for a session, fencing every older turn. Durable before it returns.
    pub fn begin_turn(&mut self, turn: TurnRef) -> Result<(), TxnError> {
        if self.log.is_poisoned() {
            return Err(TxnError::Log(LogError::Poisoned));
        }
        let current = self.turns.get(&turn.session).copied().unwrap_or(0);
        if turn.turn <= current {
            return Err(TxnError::StaleTurn {
                turn: turn.turn,
                current,
            });
        }
        self.log
            .append(RecordType::TurnBegin as u8, &encode_turn_begin(turn))?;
        self.log.sync()?;
        self.turns.insert(turn.session, turn.turn);
        Ok(())
    }

    /// Counters and latency histograms for this kernel instance.
    pub fn metrics(&self) -> &Metrics {
        &self.metrics
    }

    /// Verifies the log on disk and checks that this kernel's in-memory state (committed ids and
    /// turn fences) equals a rebuild from the log. Read-only. A mismatch is invariant I9 failing.
    pub fn scrub(&self) -> Result<crate::scrub::ScrubReport, LogError> {
        let mut report = crate::scrub::scrub(self.log.io(), self.log.dir())?;
        if report.rebuilt_ids != self.seen {
            let extra = self.seen.difference(&report.rebuilt_ids).count();
            let missing = report.rebuilt_ids.difference(&self.seen).count();
            report.issues.push(crate::scrub::Issue::DerivedStateMismatch {
                what: format!(
                    "{extra} committed ids in memory are not in the log, {missing} ids in the log are not in memory"
                ),
            });
        }
        if report.rebuilt_turns != self.turns {
            report
                .issues
                .push(crate::scrub::Issue::DerivedStateMismatch {
                    what: "turn fences in memory differ from the log".into(),
                });
        }
        Ok(report)
    }

    /// Whether this id has been committed.
    pub fn contains(&self, id: &ChangeSetId) -> bool {
        self.seen.contains(id)
    }

    /// Whether the log has been poisoned by a failure and the kernel must be reopened.
    pub fn is_poisoned(&self) -> bool {
        self.log.is_poisoned()
    }

    /// Highest durable LSN.
    pub fn durable_lsn(&self) -> u64 {
        self.log.durable_lsn()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lattice_sim::{FaultProfile, SeededRng, SimClock, SimIo};
    use std::sync::{Arc, Mutex};
    use tracing::field::{Field, Visit};
    use tracing::span::{Attributes, Id, Record};
    use tracing::{Event, Metadata, Subscriber};

    struct Clk(std::rc::Rc<SimClock>);
    impl Clock for Clk {
        fn now_ms(&self) -> u64 {
            self.0.now_ms()
        }
    }

    fn kernel(clock: std::rc::Rc<SimClock>) -> Kernel<SimIo, Clk, SeededRng> {
        let io = SimIo::new(1, FaultProfile::clean());
        Kernel::open(io, Clk(clock), SeededRng::new(1), &Config::default())
            .unwrap()
            .0
    }

    #[test]
    fn commit_metrics_count_outcomes() {
        let clock = std::rc::Rc::new(SimClock::new(1_000));
        let mut k = kernel(clock);
        let id = k.new_id();
        assert!(matches!(
            k.commit(ChangeSet::new(id, vec![1])),
            Ok(Receipt::Recorded { .. })
        ));
        assert_eq!(
            k.commit(ChangeSet::new(id, vec![1])),
            Ok(Receipt::Duplicate)
        );
        let turn = ChangeSet {
            turn: Some(TurnRef {
                session: 9,
                turn: 1,
            }),
            ..ChangeSet::new(k.new_id(), vec![])
        };
        assert!(k.commit(turn).is_err());
        let m = k.metrics();
        assert_eq!(m.counter(names::COMMIT_TOTAL), 1);
        assert_eq!(m.counter(names::COMMIT_DUPLICATE_TOTAL), 1);
        assert_eq!(m.counter(names::COMMIT_ERROR_TOTAL), 1);
        assert_eq!(m.histogram(names::COMMIT_LATENCY_SECONDS).unwrap().count, 3);
    }

    /// Records the fields of every span so a test can see what the write path attaches.
    #[derive(Clone, Default)]
    struct Spans(Arc<Mutex<Vec<String>>>);
    impl Visit for Spans {
        fn record_debug(&mut self, f: &Field, v: &dyn std::fmt::Debug) {
            if let Ok(mut g) = self.0.lock() {
                g.push(format!("{}={v:?}", f.name()));
            }
        }
    }
    impl Subscriber for Spans {
        fn enabled(&self, _: &Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, a: &Attributes<'_>) -> Id {
            a.record(&mut self.clone());
            Id::from_u64(1)
        }
        fn record(&self, _: &Id, _: &Record<'_>) {}
        fn record_follows_from(&self, _: &Id, _: &Id) {}
        fn event(&self, _: &Event<'_>) {}
        fn enter(&self, _: &Id) {}
        fn exit(&self, _: &Id) {}
    }

    /// Spans are recorded by one process-wide subscriber. A subscriber scoped to this test would
    /// race with the other tests: a callsite first hit by a thread with no subscriber is cached as
    /// disabled for every thread.
    fn recorded_spans() -> &'static Spans {
        static SPANS: std::sync::OnceLock<Spans> = std::sync::OnceLock::new();
        SPANS.get_or_init(|| {
            let spans = Spans::default();
            // Fails only if a global subscriber already exists, and then it is that one that records.
            let _ = tracing::subscriber::set_global_default(spans.clone());
            spans
        })
    }

    #[test]
    fn the_write_path_span_carries_the_transaction_id() {
        let spans = recorded_spans();
        let mut k = kernel(std::rc::Rc::new(SimClock::new(1_000)));
        let id = k.new_id();
        k.commit(ChangeSet::new(id, vec![1])).unwrap();
        let seen = spans.0.lock().unwrap().join(";");
        assert!(seen.contains(&format!("txn_id={id}")), "{seen}");
    }
}

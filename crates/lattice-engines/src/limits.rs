//! Deterministic policy and statement-limit primitives (P2-17/P2-18).
//!
//! The row store consults these hooks before exposing each visible row through point reads, scans
//! and index lookups. A transaction binds its own evaluator, and all final old-to-new transitions
//! are authorized before its change set reaches the WAL. Staging a proposed mutation is not a
//! commit and does not bypass the policy on reads of transaction-local values. An evaluator can
//! carry the authenticated principal; this module neither authenticates nor persists policy.
//! Evaluators are supplied by a trusted host. Binding one to a transaction does not freeze any
//! interior-mutable authorization state; the host controls that state and the explicit allow-all path.
//! Default trait methods deny access. Phase 2's allow-all integration must select [`AllowAll`]
//! explicitly; an omitted policy must never accidentally become an authorization bypass.
//!
//! Executors must poll [`StatementLimits::check`] between bounded units of work and reserve memory
//! *before* allocating row buffers, hash tables or result batches. Reservations account for live
//! bytes and release on drop; they do not intercept allocations or roll back a transaction. The
//! store/executor integration must define statement abort and transaction cleanup separately.
//! Deadlines use only the supplied [`Clock`]. Once observed, expiration is sticky, even if that
//! clock subsequently moves backwards; a backwards jump before expiration can delay observation.

use lattice_seams::Clock;
use std::cell::Cell;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// A row considered for a read. Values retain their physical encoding.
#[derive(Debug, Clone, Copy)]
pub struct RowRef<'a> {
    /// Physical table id.
    pub table: u32,
    /// Primary key.
    pub key: &'a [u8],
    /// Visible row value.
    pub value: &'a [u8],
}

/// A proposed row transition, including insertions and deletions.
#[derive(Debug, Clone, Copy)]
pub struct RowWriteRef<'a> {
    /// Physical table id.
    pub table: u32,
    /// Primary key.
    pub key: &'a [u8],
    /// Current value; absent for an insertion.
    pub before: Option<&'a [u8]>,
    /// Proposed value; absent for a deletion.
    pub after: Option<&'a [u8]>,
}

/// Evaluated once for each row read or written, including both sides of an update.
///
/// Implementations must use explicit context and deterministic seams. Implementing only one method
/// leaves the other denied. Callers must distinguish a hidden read row from a rejected write.
pub trait PolicyEvaluator {
    /// Whether this row may participate in the statement's read operations.
    fn allows_read(&self, _row: RowRef<'_>) -> bool {
        false
    }

    /// Whether the complete old-to-new transition is authorized.
    fn allows_write(&self, _row: RowWriteRef<'_>) -> bool {
        false
    }
}

/// References retain the evaluator's identity when a transaction borrows its actor policy.
impl<P: PolicyEvaluator + ?Sized> PolicyEvaluator for &P {
    fn allows_read(&self, row: RowRef<'_>) -> bool {
        (**self).allows_read(row)
    }

    fn allows_write(&self, row: RowWriteRef<'_>) -> bool {
        (**self).allows_write(row)
    }
}

/// Explicit Phase 2 policy for an integration that has no access restrictions yet.
#[derive(Debug, Clone, Copy, Default)]
pub struct AllowAll;
impl PolicyEvaluator for AllowAll {
    fn allows_read(&self, _row: RowRef<'_>) -> bool {
        true
    }
    fn allows_write(&self, _row: RowWriteRef<'_>) -> bool {
        true
    }
}

/// Fail-closed policy, useful while no evaluator has been installed.
#[derive(Debug, Clone, Copy, Default)]
pub struct DenyAll;
impl PolicyEvaluator for DenyAll {}

/// A policy denial or an observed statement resource limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitError {
    /// The row policy rejected the operation.
    PermissionDenied,
    /// Cancellation was requested.
    Cancelled,
    /// The statement reached its deadline.
    DeadlineExceeded,
    /// Retained for compatibility; deadline overflow now saturates and is never returned.
    DeadlineOverflow,
    /// A reservation would exceed the statement's live-byte ceiling.
    MemoryExceeded {
        /// Bytes requested by this reservation.
        requested: u64,
        /// Bytes already reserved.
        used: u64,
        /// Configured live-byte ceiling.
        limit: u64,
    },
}
impl fmt::Display for LimitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PermissionDenied => f.write_str("row access denied"),
            Self::Cancelled => f.write_str("statement cancelled"),
            Self::DeadlineExceeded => f.write_str("statement deadline exceeded"),
            Self::DeadlineOverflow => f.write_str("statement deadline overflow"),
            Self::MemoryExceeded {
                requested,
                used,
                limit,
            } => {
                write!(
                    f,
                    "statement memory limit exceeded: requested {requested}, used {used}, limit {limit}"
                )
            }
        }
    }
}
impl std::error::Error for LimitError {}

/// Checks one visible row. A caller implementing RLS should hide a denied row before using it.
pub fn authorize_read(
    policy: &(impl PolicyEvaluator + ?Sized),
    row: RowRef<'_>,
) -> Result<(), LimitError> {
    if policy.allows_read(row) {
        Ok(())
    } else {
        Err(LimitError::PermissionDenied)
    }
}

/// Checks a final old-to-new transition before commit reaches the WAL; denial rejects the batch.
pub fn authorize_write(
    policy: &(impl PolicyEvaluator + ?Sized),
    row: RowWriteRef<'_>,
) -> Result<(), LimitError> {
    if policy.allows_write(row) {
        Ok(())
    } else {
        Err(LimitError::PermissionDenied)
    }
}

/// A cloneable, one-way cancellation signal; no background work or ambient state is used.
///
/// The caller decides when to request cancellation. Replaying the same request/poll order gives
/// the same result. Clones share one flag, suitable for a host cancel registry and its query.
#[derive(Debug, Clone, Default)]
pub struct CancellationFlag(Arc<AtomicBool>);
impl CancellationFlag {
    /// Requests cancellation permanently for this statement.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }
    /// Whether cancellation has been requested. This flag publishes no other memory.
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
    /// Fails once cancellation is observed.
    pub fn check(&self) -> Result<(), LimitError> {
        if self.is_cancelled() {
            Err(LimitError::Cancelled)
        } else {
            Ok(())
        }
    }
}

/// An absolute seam-clock deadline, inclusive at its boundary and sticky once expired.
#[derive(Debug)]
pub struct Deadline {
    at_ms: u64,
    never: bool,
    expired: Cell<bool>,
}
impl Deadline {
    /// Starts a statement timeout. Zero expires immediately. A timeout so large that the absolute
    /// deadline would overflow `u64` milliseconds saturates to "never expires" (it is not wrapped
    /// and not an error), because a caller asking for a huge timeout wants no timeout.
    pub fn after(clock: &(impl Clock + ?Sized), timeout_ms: u64) -> Self {
        let (at_ms, never) = match clock.now_ms().checked_add(timeout_ms) {
            Some(at_ms) => (at_ms, false),
            None => (u64::MAX, true),
        };
        Self {
            at_ms,
            never,
            expired: Cell::new(false),
        }
    }
    /// Configured absolute deadline, in seam-clock milliseconds (`u64::MAX` when it never expires).
    pub fn at_ms(&self) -> u64 {
        self.at_ms
    }
    /// Checks the deadline without reading a real clock.
    pub fn check(&self, clock: &(impl Clock + ?Sized)) -> Result<(), LimitError> {
        if self.expired.get() || (!self.never && clock.now_ms() >= self.at_ms) {
            self.expired.set(true);
            Err(LimitError::DeadlineExceeded)
        } else {
            Ok(())
        }
    }
}

/// Live-byte accounting for one statement. Sharing borrows permits multiple simultaneous buffers.
///
/// This is deliberately not `Clone`: copying the accountant would split one statement's ceiling.
/// Forgotten reservations conservatively retain their charge. It does not measure allocator RSS.
#[derive(Debug)]
pub struct MemoryBudget {
    limit: u64,
    used: Cell<u64>,
    peak: Cell<u64>,
}
impl MemoryBudget {
    /// Creates an empty budget; zero allows only zero-byte reservations.
    pub fn new(limit: u64) -> Self {
        Self {
            limit,
            used: Cell::new(0),
            peak: Cell::new(0),
        }
    }
    /// Configured ceiling in bytes.
    pub fn limit(&self) -> u64 {
        self.limit
    }
    /// Currently reserved live bytes.
    pub fn used(&self) -> u64 {
        self.used.get()
    }
    /// Highest simultaneous reservation total observed so far.
    pub fn peak(&self) -> u64 {
        self.peak.get()
    }
    /// Charges before allocation. Failure leaves both live and peak counters unchanged.
    #[must_use = "keep the reservation alive while the allocation is live"]
    pub fn reserve(&self, bytes: u64) -> Result<MemoryReservation<'_>, LimitError> {
        let used = self.used.get();
        if bytes > self.limit - used {
            return Err(LimitError::MemoryExceeded {
                requested: bytes,
                used,
                limit: self.limit,
            });
        }
        let next = used + bytes;
        self.used.set(next);
        self.peak.set(self.peak.get().max(next));
        Ok(MemoryReservation {
            budget: self,
            bytes,
        })
    }
}

/// A non-cloneable live charge. Drop it only after the corresponding allocation is released.
#[derive(Debug)]
#[must_use = "dropping the reservation releases its memory charge"]
pub struct MemoryReservation<'a> {
    budget: &'a MemoryBudget,
    bytes: u64,
}
impl MemoryReservation<'_> {
    /// Bytes held by this reservation.
    pub fn bytes(&self) -> u64 {
        self.bytes
    }
}
impl Drop for MemoryReservation<'_> {
    fn drop(&mut self) {
        self.budget.used.set(self.budget.used.get() - self.bytes);
    }
}

/// One statement's independently owned limits. Cancellation wins over deadline errors.
#[derive(Debug)]
pub struct StatementLimits {
    cancellation: CancellationFlag,
    deadline: Option<Deadline>,
    memory: MemoryBudget,
}
impl StatementLimits {
    /// Captures the start clock once when a timeout is configured; `None` disables only the deadline.
    /// An overflowing timeout means the deadline never expires. Currently never fails; the result
    /// type is kept so a future fallible limit does not change the signature.
    pub fn new(
        clock: &(impl Clock + ?Sized),
        timeout_ms: Option<u64>,
        memory_bytes: u64,
        cancellation: CancellationFlag,
    ) -> Result<Self, LimitError> {
        Ok(Self {
            cancellation,
            deadline: timeout_ms.map(|ms| Deadline::after(clock, ms)),
            memory: MemoryBudget::new(memory_bytes),
        })
    }
    /// Polls cancellation and timeout before a bounded unit of work.
    pub fn check(&self, clock: &(impl Clock + ?Sized)) -> Result<(), LimitError> {
        self.cancellation.check()?;
        if let Some(deadline) = &self.deadline {
            deadline.check(clock)?;
        }
        Ok(())
    }
    /// Checks cancellation/deadline before charging, so a failed statement acquires no new memory.
    pub fn reserve(
        &self,
        clock: &(impl Clock + ?Sized),
        bytes: u64,
    ) -> Result<MemoryReservation<'_>, LimitError> {
        self.check(clock)?;
        self.memory.reserve(bytes)
    }
    /// Read-only counters for reporting; reservations should use [`StatementLimits::reserve`].
    pub fn memory(&self) -> (u64, u64, u64) {
        (self.memory.limit(), self.memory.used(), self.memory.peak())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    struct TestClock(Cell<u64>);
    impl Clock for TestClock {
        fn now_ms(&self) -> u64 {
            self.0.get()
        }
    }

    #[test]
    fn default_policy_denies_and_explicit_allow_all_checks_each_transition() {
        struct Unconfigured;
        impl PolicyEvaluator for Unconfigured {}
        let row = RowRef {
            table: 7,
            key: b"key",
            value: b"secret",
        };
        for policy in [&Unconfigured as &dyn PolicyEvaluator, &DenyAll] {
            assert_eq!(
                authorize_read(policy, row),
                Err(LimitError::PermissionDenied)
            );
            for (before, after) in [
                (None, Some(b"new".as_slice())),
                (Some(b"old".as_slice()), Some(b"new".as_slice())),
                (Some(b"old".as_slice()), None),
            ] {
                assert_eq!(
                    authorize_write(
                        policy,
                        RowWriteRef {
                            table: 7,
                            key: b"key",
                            before,
                            after
                        }
                    ),
                    Err(LimitError::PermissionDenied)
                );
            }
        }
        assert_eq!(authorize_read(&AllowAll, row), Ok(()));
        assert_eq!(
            authorize_write(
                &AllowAll,
                RowWriteRef {
                    table: 7,
                    key: b"key",
                    before: Some(b"old"),
                    after: None
                }
            ),
            Ok(())
        );
    }

    #[test]
    fn policy_sees_old_and_new_values_and_read_permission_does_not_authorize_writes() {
        struct Owned;
        impl PolicyEvaluator for Owned {
            fn allows_read(&self, row: RowRef<'_>) -> bool {
                row.table == 7 && row.key == b"key" && row.value == b"owned"
            }
            fn allows_write(&self, row: RowWriteRef<'_>) -> bool {
                row.table == 7
                    && row.key == b"key"
                    && row.before == Some(b"owned".as_slice())
                    && row.after == Some(b"owned".as_slice())
            }
        }
        let row = RowRef {
            table: 7,
            key: b"key",
            value: b"owned",
        };
        assert!(authorize_read(&Owned, row).is_ok());
        assert_eq!(
            authorize_read(
                &Owned,
                RowRef {
                    value: b"other",
                    ..row
                }
            ),
            Err(LimitError::PermissionDenied)
        );
        assert_eq!(
            authorize_write(
                &Owned,
                RowWriteRef {
                    table: 7,
                    key: b"key",
                    before: Some(b"owned"),
                    after: Some(b"other")
                }
            ),
            Err(LimitError::PermissionDenied)
        );
        struct ReadOnly;
        impl PolicyEvaluator for ReadOnly {
            fn allows_read(&self, _: RowRef<'_>) -> bool {
                true
            }
        }
        assert_eq!(
            authorize_write(
                &ReadOnly,
                RowWriteRef {
                    table: 7,
                    key: b"key",
                    before: None,
                    after: Some(b"owned")
                }
            ),
            Err(LimitError::PermissionDenied)
        );
    }

    #[test]
    fn deadline_boundary_overflow_and_backwards_clock_are_explicit() {
        let clock = TestClock(Cell::new(10));
        let deadline = Deadline::after(&clock, 5);
        clock.0.set(14);
        assert_eq!(deadline.check(&clock), Ok(()));
        clock.0.set(15);
        assert_eq!(deadline.check(&clock), Err(LimitError::DeadlineExceeded));
        clock.0.set(0);
        assert_eq!(deadline.check(&clock), Err(LimitError::DeadlineExceeded));
        assert_eq!(
            Deadline::after(&clock, 0).check(&clock),
            Err(LimitError::DeadlineExceeded)
        );
        clock.0.set(u64::MAX);
        // Overflow saturates to "never expires", even when the clock later reads u64::MAX.
        let never = Deadline::after(&clock, 1);
        assert_eq!(never.at_ms(), u64::MAX);
        assert_eq!(never.check(&clock), Ok(()));
        let huge = Deadline::after(&TestClock(Cell::new(10)), u64::MAX);
        assert_eq!(huge.check(&clock), Ok(()));
        assert_eq!(Deadline::after(&clock, 0).at_ms(), u64::MAX);
        assert_eq!(
            Deadline::after(&clock, 0).check(&clock),
            Err(LimitError::DeadlineExceeded)
        );
    }

    #[test]
    fn reservations_release_on_scope_exit_and_failed_charges_are_atomic() {
        let budget = MemoryBudget::new(u64::MAX);
        let full = budget.reserve(u64::MAX).unwrap();
        assert_eq!(full.bytes(), u64::MAX);
        assert!(matches!(
            budget.reserve(1),
            Err(LimitError::MemoryExceeded { used: u64::MAX, .. })
        ));
        assert_eq!((budget.used(), budget.peak()), (u64::MAX, u64::MAX));
        drop(full);
        assert_eq!(budget.used(), 0);
        let zero = MemoryBudget::new(0);
        assert!(zero.reserve(0).is_ok());
        assert!(zero.reserve(1).is_err());
    }

    #[test]
    fn cancellation_is_shared_sticky_and_prevents_new_charges() {
        let clock = TestClock(Cell::new(100));
        let flag = CancellationFlag::default();
        let limits = StatementLimits::new(&clock, Some(5), 8, flag.clone()).unwrap();
        let live = limits.reserve(&clock, 8).unwrap();
        flag.cancel();
        flag.cancel();
        clock.0.set(105);
        assert_eq!(limits.check(&clock), Err(LimitError::Cancelled));
        assert!(matches!(
            limits.reserve(&clock, 0),
            Err(LimitError::Cancelled)
        ));
        assert_eq!(limits.memory(), (8, 8, 8));
        drop(live);
        assert_eq!(limits.memory(), (8, 0, 8));
        let no_timeout =
            StatementLimits::new(&clock, None, 0, CancellationFlag::default()).unwrap();
        clock.0.set(u64::MAX);
        assert_eq!(no_timeout.check(&clock), Ok(()));
    }

    #[test]
    fn statement_timeout_prevents_a_charge_and_stays_expired_after_clock_rollback() {
        let clock = TestClock(Cell::new(10));
        let limits = StatementLimits::new(&clock, Some(3), 8, CancellationFlag::default()).unwrap();
        clock.0.set(12);
        let live = limits.reserve(&clock, 3).unwrap();
        clock.0.set(13);
        assert_eq!(limits.check(&clock), Err(LimitError::DeadlineExceeded));
        assert!(matches!(
            limits.reserve(&clock, 2),
            Err(LimitError::DeadlineExceeded)
        ));
        assert_eq!(limits.memory(), (8, 3, 3));
        clock.0.set(0);
        assert_eq!(limits.check(&clock), Err(LimitError::DeadlineExceeded));
        drop(live);
        assert_eq!(limits.memory(), (8, 0, 3));
        let saturated = StatementLimits::new(
            &TestClock(Cell::new(u64::MAX)),
            Some(1),
            8,
            CancellationFlag::default(),
        )
        .unwrap();
        assert_eq!(saturated.check(&clock), Ok(()));
    }

    #[test]
    fn authorizers_call_the_evaluator_for_every_row_including_delete_transitions() {
        struct Audit {
            reads: Cell<usize>,
            writes: Cell<usize>,
        }
        impl PolicyEvaluator for Audit {
            fn allows_read(&self, _: RowRef<'_>) -> bool {
                self.reads.set(self.reads.get() + 1);
                false
            }
            fn allows_write(&self, row: RowWriteRef<'_>) -> bool {
                self.writes.set(self.writes.get() + 1);
                row.before == Some(b"owned".as_slice()) && row.after.is_none()
            }
        }
        let policy = Audit {
            reads: Cell::new(0),
            writes: Cell::new(0),
        };
        for key in [b"one".as_slice(), b"two", b"three"] {
            assert_eq!(
                authorize_read(
                    &policy,
                    RowRef {
                        table: 7,
                        key,
                        value: b"owned"
                    }
                ),
                Err(LimitError::PermissionDenied)
            );
            assert_eq!(
                authorize_write(
                    &policy,
                    RowWriteRef {
                        table: 7,
                        key,
                        before: Some(b"owned"),
                        after: None
                    }
                ),
                Ok(())
            );
        }
        assert_eq!((policy.reads.get(), policy.writes.get()), (3, 3));
    }

    proptest! {
        #[test]
        fn arbitrary_reserve_release_sequences_match_a_live_byte_model(limit in any::<u64>(), operations in prop::collection::vec((any::<bool>(), any::<u64>()), 0..100)) {
            let budget = MemoryBudget::new(limit);
            let mut reservations = Vec::new();
            let mut model = Vec::new();
            let mut peak = 0u128;
            for (release, bytes) in operations {
                if release { reservations.pop(); model.pop(); }
                else {
                    let used: u128 = model.iter().map(|n| u128::from(*n)).sum();
                    let allowed = used + u128::from(bytes) <= u128::from(limit);
                    match budget.reserve(bytes) {
                        Ok(reservation) => { prop_assert!(allowed); reservations.push(reservation); model.push(bytes); }
                        Err(LimitError::MemoryExceeded { requested, used: actual, limit: ceiling }) => {
                            prop_assert!(!allowed); prop_assert_eq!(requested, bytes); prop_assert_eq!(u128::from(actual), used); prop_assert_eq!(ceiling, limit);
                        }
                        Err(other) => prop_assert!(false, "unexpected error: {other}"),
                    }
                }
                let used: u128 = model.iter().map(|n| u128::from(*n)).sum();
                peak = peak.max(used);
                prop_assert_eq!(u128::from(budget.used()), used);
                prop_assert_eq!(u128::from(budget.peak()), peak);
            }
            drop(reservations);
            prop_assert_eq!(budget.used(), 0);
        }

        #[test]
        fn deadline_never_revives_after_observed_expiration(start in any::<u64>(), timeout in any::<u64>(), readings in prop::collection::vec(any::<u64>(), 0..50)) {
            let clock = TestClock(Cell::new(start));
            let deadline = Deadline::after(&clock, timeout);
            let absolute = u128::from(start) + u128::from(timeout);
            let never = absolute > u128::from(u64::MAX);
            let mut expired = false;
            for now in readings {
                clock.0.set(now);
                expired |= !never && u128::from(now) >= absolute;
                prop_assert_eq!(deadline.check(&clock), if expired { Err(LimitError::DeadlineExceeded) } else { Ok(()) });
            }
        }
    }
}

//! Planted bugs for "testing the tests".
//!
//! With the `planted-bugs` feature a test can switch on a deliberate defect and then prove that
//! the simulation and invariant checkers notice it. Without the feature every check compiles to
//! `false`, so production builds contain none of this.

/// A deliberate defect that can be switched on in test builds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum Bug {
    /// `Log::sync` skips the fsync (durability).
    SkipFsync,
    /// Duplicate change set ids are recorded again (idempotency).
    NoDedupe,
    /// The hybrid logical clock follows the wall clock backwards (causal order).
    HlcRegress,
    /// Commits are acknowledged before they are synced (durability).
    AckBeforeSync,
    /// Recovery leaves a torn tail in place instead of truncating it (durability).
    AcceptTornTail,
    /// The record checksum does not cover the payload (corruption detection).
    CrcSkipsPayload,
    /// The fence check is skipped (fencing).
    FenceIgnored,
    /// Recovery drops the last valid record (durability).
    RecoveryDropsLastRecord,
    /// Segment rotation does not sync the directory (durability).
    RotateWithoutDirSync,
    /// The next LSN is reused after recovery (ordering).
    LsnReusedAfterRecovery,
    /// The dedupe set is not rebuilt on recovery (idempotency).
    DedupeNotRebuilt,
    /// Turn state is not rebuilt on recovery (fencing).
    FenceNotRebuilt,
    /// Creating the log directory does not make its parents durable (durability).
    NoParentDirSync,
    /// Recovery does not fsync what it adopts as committed (durability).
    SkipRecoverySync,
    /// A repeated id inside a batch is acknowledged whatever happens to its first occurrence.
    BatchDuplicateAckedEarly,
    /// A failed fsync does not poison the log, so a later successful one vouches for lost data.
    FsyncFailureNotPoisoning,
    /// A failed fsync is swallowed and the batch is acknowledged (durability).
    FsyncErrorSwallowed,
    /// A new segment's header is not synced when the log rotates (durability).
    RotateHeaderNotSynced,
    /// The durable LSN runs one ahead of what was written (ordering).
    DurableLsnOvershoot,
    /// The hybrid logical clock does not advance its counter within one millisecond (causal order).
    HlcCounterStuck,
    /// A checkpoint is written while change sets are logged but not applied (recovery then rejects the log).
    CheckpointIgnoresInFlight,
    /// The row store commits without checking for a newer write to the same row (lost update).
    RowConflictCheckSkipped,
    /// The row store applies only the first write of a change set in memory (partial visibility).
    RowPartialApply,
    /// Row recovery drops deletions, so deleted rows come back after a restart (durability).
    RowRecoveryDropsDeletes,
    /// An index keeps the entry for a row's previous value after an update or deletion.
    RowIndexStaleEntries,
    /// Updating a row discards its older versions (immutable snapshots).
    RowHistoryOverwritten,
    /// Visibility searches skip deletion versions (deleted rows resurrect).
    RowTombstoneSkipped,
    /// Transaction reads ignore their own buffered writes and deletes.
    RowReadOwnWriteIgnored,
    /// A row scan treats its exclusive upper bound as inclusive.
    RowScanUpperBoundIncluded,
    /// Index maintenance omits newly indexed entries (missing rows).
    RowIndexMissingInsert,
    /// An index equality lookup matches keys with the requested prefix.
    RowIndexPrefixLookup,
    /// Vacuum also drops the newest version at the horizon, so a snapshot there reads nothing.
    VacuumDropsVisibleVersion,
    /// Query row reads skip the actor policy (authorization).
    RowPolicyReadSkipped,
    /// A row transaction appends without authorizing all final transitions.
    RowPolicyWriteSkipped,
    /// The tree CRDT applies a move without checking that it would create a cycle (convergence).
    TreeCycleCheckSkipped,
    /// A capability-requiring append omits its durable segment guard.
    RequiredGuardOmitted,
    /// Recovery ignores managed index definitions and therefore omits their rebuilt entries.
    ManagedIndexesNotRecovered,
    /// Strict HLC history resolution silently substitutes the vacuum floor for an expired view.
    HlcSnapshotClamped,
}

impl Bug {
    /// Defects in the row store, detected by the row simulation rather than the log simulation.
    pub const ROW: [Bug; 4] = [
        Bug::RowConflictCheckSkipped,
        Bug::RowPartialApply,
        Bug::RowRecoveryDropsDeletes,
        Bug::RowIndexStaleEntries,
    ];

    /// Storage defects exercised by the deterministic public-operation witness suite.
    /// `ROW` remains the four-defect contract of the seeded row simulation workloads.
    pub const STORAGE: [Bug; 10] = [
        Bug::RowConflictCheckSkipped,
        Bug::RowPartialApply,
        Bug::RowRecoveryDropsDeletes,
        Bug::RowIndexStaleEntries,
        Bug::RowHistoryOverwritten,
        Bug::RowTombstoneSkipped,
        Bug::RowReadOwnWriteIgnored,
        Bug::RowScanUpperBoundIncluded,
        Bug::RowIndexMissingInsert,
        Bug::RowIndexPrefixLookup,
    ];

    /// Vacuum defects, detected by the vacuum workload in the row simulation.
    pub const VACUUM: [Bug; 1] = [Bug::VacuumDropsVisibleVersion];

    /// Authorization defects, detected by the independent public row-policy witness suite.
    pub const POLICY: [Bug; 2] = [Bug::RowPolicyReadSkipped, Bug::RowPolicyWriteSkipped];
    /// Catalog-managed definition recovery defects, exercised by the focused fault witness.
    pub const CATALOG_INDEX: [Bug; 1] = [Bug::ManagedIndexesNotRecovered];

    /// Strict HLC snapshot defects, detected by the focused retention witness.
    pub const HLC_SNAPSHOT: [Bug; 1] = [Bug::HlcSnapshotClamped];

    /// Tree CRDT defects, detected by the convergence and acyclicity checks in `crdt_tree`.
    pub const CRDT: [Bug; 1] = [Bug::TreeCycleCheckSkipped];
    /// Guarded-WAL admission defects, exercised by independent context witnesses.
    pub const ADMISSION: [Bug; 1] = [Bug::RequiredGuardOmitted];

    /// Log defects, for the log simulation detection suite.
    pub const ALL: [Bug; 21] = [
        Bug::SkipFsync,
        Bug::NoDedupe,
        Bug::HlcRegress,
        Bug::AckBeforeSync,
        Bug::AcceptTornTail,
        Bug::CrcSkipsPayload,
        Bug::FenceIgnored,
        Bug::RecoveryDropsLastRecord,
        Bug::RotateWithoutDirSync,
        Bug::LsnReusedAfterRecovery,
        Bug::DedupeNotRebuilt,
        Bug::FenceNotRebuilt,
        Bug::NoParentDirSync,
        Bug::SkipRecoverySync,
        Bug::BatchDuplicateAckedEarly,
        Bug::FsyncFailureNotPoisoning,
        Bug::FsyncErrorSwallowed,
        Bug::RotateHeaderNotSynced,
        Bug::DurableLsnOvershoot,
        Bug::HlcCounterStuck,
        Bug::CheckpointIgnoresInFlight,
    ];
}

#[cfg(feature = "planted-bugs")]
mod imp {
    use super::Bug;
    use std::cell::Cell;

    thread_local! {
        static MASK: Cell<u64> = const { Cell::new(0) };
    }

    fn bit(b: Bug) -> u64 {
        1 << (b as u32)
    }

    /// Switches a defect on for the current thread.
    pub fn enable(b: Bug) {
        MASK.with(|m| m.set(m.get() | bit(b)));
    }

    /// Switches every defect off for the current thread.
    pub fn clear() {
        MASK.with(|m| m.set(0));
    }

    /// Whether a defect is switched on for the current thread.
    pub fn active(b: Bug) -> bool {
        MASK.with(|m| m.get() & bit(b) != 0)
    }
}

#[cfg(not(feature = "planted-bugs"))]
mod imp {
    use super::Bug;

    /// Without the feature no defect can be enabled.
    pub fn active(_b: Bug) -> bool {
        false
    }
}

pub use imp::active;
#[cfg(feature = "planted-bugs")]
pub use imp::{clear, enable};

#[cfg(all(test, feature = "planted-bugs"))]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn all_bugs() -> impl Iterator<Item = Bug> {
        Bug::ALL
            .into_iter()
            .chain(Bug::STORAGE)
            .chain(Bug::VACUUM)
            .chain(Bug::POLICY)
            .chain(Bug::CRDT)
            .chain(Bug::ADMISSION)
            .chain(Bug::CATALOG_INDEX)
            .chain(Bug::HLC_SNAPSHOT)
    }

    #[test]
    fn every_log_and_storage_guard_has_a_distinct_mask_bit() {
        let all: BTreeSet<_> = Bug::ALL
            .into_iter()
            .chain(Bug::STORAGE)
            .chain(Bug::VACUUM)
            .chain(Bug::POLICY)
            .chain(Bug::CRDT)
            .chain(Bug::ADMISSION)
            .chain(Bug::CATALOG_INDEX)
            .chain(Bug::HLC_SNAPSHOT)
            .collect();
        assert_eq!(all.len(), 38);
        for bug in all {
            assert!((bug as u32) < u64::BITS, "guard mask exhausted: {bug:?}");
            clear();
            enable(bug);
            assert!(active(bug));
            for other in all_bugs() {
                assert_eq!(active(other), bug == other);
            }
        }
        clear();
    }
}

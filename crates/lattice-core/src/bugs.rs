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
    /// The duplicate check runs before the fence check, so a superseded turn retrying an
    /// already stored record id is answered duplicate instead of fenced (plugin contract: fence first).
    DuplicateBeforeFence,
    /// An empty tail's guard is rewritten in place without a durable staged copy (issue 307).
    GuardRewriteUnstaged,
    /// A catalog or managed-index change commits without asking the actor policy (authorization).
    DdlPolicySkipped,
    /// The sync subscription host reads events for a fixed development identity instead of the
    /// principal its connection authenticated as (authorization).
    SubscribePrincipalDropped,
    /// A peer's operation id is accepted whatever node it carries (forged node id).
    CrdtNodeBindingSkipped,
    /// A peer's operation id is accepted however far ahead of the clock it is (clock drift).
    CrdtDriftNotChecked,
    /// A peer's batch is accepted whatever its length (operation budget).
    CrdtBatchLimitSkipped,
    /// The text CRDT does not count inserts per quota key (lifetime budget exhaustion).
    CrdtKeyBudgetSkipped,
    /// The tree CRDT does not cap the number of nodes (memory exhaustion).
    TreeNodeLimitSkipped,
    /// The tree CRDT does not cap node depth (walk cost).
    TreeDepthLimitSkipped,
    /// The tree CRDT does not share the op log between origins (log exhaustion).
    TreeOriginQuotaSkipped,
    /// The set CRDT accepts a merge of any size (memory exhaustion).
    SetTagLimitSkipped,
    /// Recovery truncates a checksum-valid record with an unexpected LSN as if it were a torn tail
    /// (silent loss of acknowledged data after a gap or repeat).
    GapTailTruncated,
    /// The executor's primary-key range scan treats an inclusive upper bound (`<=`, `BETWEEN`) as
    /// exclusive (missing boundary rows).
    ExecIndexUpperBoundOpen,
    /// The executor's unique-constraint lookup fetches the row without the actor's read policy
    /// (hidden rows revealed).
    ExecIndexPolicySkipped,
    /// A row commit accepts a key longer than the change feed can carry (feed availability).
    RowOversizeKeyAccepted,
    /// The tree CRDT replays any number of live ops for one peer operation (CPU exhaustion).
    TreeReplayBudgetSkipped,
    /// Recovery keeps a zero-filled torn tail in place instead of truncating it, so later
    /// acknowledged records land behind a damaged region (silent loss once that region is repaired).
    ZeroFilledTailKept,
}

impl Bug {
    /// Fence-order defect, detected by the fencing race simulation (`lattice-testkit`), not by the
    /// general log simulation, which does not retry stored writes of superseded turns.
    pub const FENCE_ORDER: [Bug; 1] = [Bug::DuplicateBeforeFence];

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
    pub const CATALOG_INDEX: [Bug; 2] = [Bug::ManagedIndexesNotRecovered, Bug::DdlPolicySkipped];

    /// Key-limit defects, detected by the oversize key witnesses in `lattice-engines`.
    pub const KEY_LIMIT: [Bug; 1] = [Bug::RowOversizeKeyAccepted];

    /// Strict HLC snapshot defects, detected by the focused retention witness.
    pub const HLC_SNAPSHOT: [Bug; 1] = [Bug::HlcSnapshotClamped];

    /// Hostile-peer defects (G-08), detected by the hostile peer witnesses in `crdt_peer_tests`.
    pub const CRDT_PEER: [Bug; 9] = [
        Bug::CrdtNodeBindingSkipped,
        Bug::CrdtDriftNotChecked,
        Bug::CrdtBatchLimitSkipped,
        Bug::CrdtKeyBudgetSkipped,
        Bug::TreeNodeLimitSkipped,
        Bug::TreeDepthLimitSkipped,
        Bug::TreeOriginQuotaSkipped,
        Bug::SetTagLimitSkipped,
        Bug::TreeReplayBudgetSkipped,
    ];

    /// Index-scan defects in the SQL executor, detected by the differential index-scan witnesses.
    pub const EXEC_INDEX: [Bug; 2] = [Bug::ExecIndexUpperBoundOpen, Bug::ExecIndexPolicySkipped];

    /// Tree CRDT defects, detected by the convergence and acyclicity checks in `crdt_tree`.
    pub const CRDT: [Bug; 1] = [Bug::TreeCycleCheckSkipped];

    /// Principal-binding defects, detected by the subscription host witness in `lattice-api`.
    pub const PRINCIPAL: [Bug; 1] = [Bug::SubscribePrincipalDropped];
    /// Guarded-WAL admission defects, exercised by independent context witnesses.
    pub const ADMISSION: [Bug; 2] = [Bug::RequiredGuardOmitted, Bug::GuardRewriteUnstaged];

    /// Recovery-order defect, detected by the focused LSN-gap witnesses in `log.rs`: the log
    /// simulation never writes a gap, so it cannot reach this path.
    pub const RECOVERY_ORDER: [Bug; 1] = [Bug::GapTailTruncated];

    /// Torn-tail defect that only a non-prefix power loss reaches (a zero-filled extent), detected
    /// by the `non-prefix` fault profile and the focused witnesses in `crash_shape_tests`.
    pub const NON_PREFIX: [Bug; 1] = [Bug::ZeroFilledTailKept];

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
            .chain(Bug::PRINCIPAL)
            .chain(Bug::EXEC_INDEX)
            .chain(Bug::CRDT_PEER)
            .chain(Bug::ADMISSION)
            .chain(Bug::CATALOG_INDEX)
            .chain(Bug::HLC_SNAPSHOT)
            .chain(Bug::KEY_LIMIT)
            .chain(Bug::FENCE_ORDER)
            .chain(Bug::RECOVERY_ORDER)
            .chain(Bug::NON_PREFIX)
    }

    #[test]
    fn every_log_and_storage_guard_has_a_distinct_mask_bit() {
        let all: BTreeSet<_> = Bug::ALL
            .into_iter()
            .chain(Bug::STORAGE)
            .chain(Bug::VACUUM)
            .chain(Bug::POLICY)
            .chain(Bug::CRDT)
            .chain(Bug::PRINCIPAL)
            .chain(Bug::EXEC_INDEX)
            .chain(Bug::CRDT_PEER)
            .chain(Bug::ADMISSION)
            .chain(Bug::CATALOG_INDEX)
            .chain(Bug::HLC_SNAPSHOT)
            .chain(Bug::KEY_LIMIT)
            .chain(Bug::FENCE_ORDER)
            .chain(Bug::RECOVERY_ORDER)
            .chain(Bug::NON_PREFIX)
            .collect();
        assert_eq!(all.len(), 56);
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

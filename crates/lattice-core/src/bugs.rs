//! Planted bugs for "testing the tests".
//!
//! With the `planted-bugs` feature a test can switch on a deliberate defect and then prove that
//! the simulation and invariant checkers notice it. Without the feature every check compiles to
//! `false`, so production builds contain none of this.
//!
//! # Adding a planted bug
//!
//! A new bug is two lines: its variant in the `planted_bugs!` list, and its entry in the array of the group that
//! describes how it is detected (extend that array's length, or add a new group const and list it
//! in [`Bug::GROUPS`]). There is no total to bump. The registry test in this file derives
//! everything else from [`Bug::GROUPS`]: every variant is in exactly one group, discriminants are
//! distinct and below `MASK_BITS` (the thread-local mask width). A variant without a group, or in
//! two groups, fails that test. Add the variant to the `planted_bugs!` list; that list also
//! generates [`Bug::VARIANTS`].

/// The integer type of the per-thread mask: one bit per planted bug.
#[cfg(any(test, feature = "planted-bugs"))]
type Mask = u64;
/// How many bugs the mask can hold; discriminants must stay below it.
#[cfg(test)]
const MASK_BITS: u32 = Mask::BITS;

/// Generates [`Bug`] and [`Bug::VARIANTS`] from one list, so the list of all variants cannot
/// drift from the enum. Doc comments and attributes on a variant pass through.
macro_rules! planted_bugs {
    ($($(#[$m:meta])* $name:ident,)*) => {
        /// A deliberate defect that can be switched on in test builds.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        #[non_exhaustive]
        pub enum Bug {
            $($(#[$m])* $name,)*
        }

        impl Bug {
            /// Every variant, in declaration order (generated from the enum's own list).
            pub const VARIANTS: &'static [Bug] = &[$(Bug::$name,)*];
        }
    };
}

planted_bugs! {
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
    /// The kernel-backed event source reads the feed with an allow-all policy instead of the
    /// principal's, so a denied principal sees rows and tables it may not (authorization).
    FeedSourcePolicyDropped,
    /// A table grant that only reads also authorizes writes, so a read-only account changes data
    /// (authorization).
    GrantReadOnlyTreatedAsWrite,
    /// A table grant is matched by name prefix instead of exactly, so a grant on `notes` also
    /// covers `notes_private` (authorization).
    GrantTableMatchByPrefix,
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

    /// Feed-source authorization defect, detected by the principal witness in
    /// `lattice-api::feed_source`.
    pub const FEED_SOURCE: [Bug; 1] = [Bug::FeedSourcePolicyDropped];

    /// Per-table grant defects, detected by the grant matrix in `lattice-engines::grants` and
    /// `lattice-api`.
    pub const TABLE_GRANTS: [Bug; 2] = [
        Bug::GrantReadOnlyTreatedAsWrite,
        Bug::GrantTableMatchByPrefix,
    ];
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

    /// Every group, the single registry of planted bugs: each variant belongs to exactly one of
    /// these. `ROW` is a subset view of `STORAGE` (the seeded row simulation's contract) and is
    /// deliberately not listed. Add a new group const here.
    pub const GROUPS: &'static [&'static [Bug]] = &[
        &Bug::ALL,
        &Bug::STORAGE,
        &Bug::VACUUM,
        &Bug::POLICY,
        &Bug::CRDT,
        &Bug::PRINCIPAL,
        &Bug::FEED_SOURCE,
        &Bug::TABLE_GRANTS,
        &Bug::EXEC_INDEX,
        &Bug::CRDT_PEER,
        &Bug::ADMISSION,
        &Bug::CATALOG_INDEX,
        &Bug::HLC_SNAPSHOT,
        &Bug::KEY_LIMIT,
        &Bug::FENCE_ORDER,
        &Bug::RECOVERY_ORDER,
        &Bug::NON_PREFIX,
    ];
}

#[cfg(feature = "planted-bugs")]
mod imp {
    use super::{Bug, Mask};
    use std::cell::Cell;

    thread_local! {
        static MASK: Cell<Mask> = const { Cell::new(0) };
    }

    fn bit(b: Bug) -> Mask {
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

#[cfg(test)]
mod registry_tests {
    use super::{Bug, MASK_BITS};
    use std::collections::BTreeSet;

    /// Checks that `groups` partition `variants` and that every discriminant fits a mask of
    /// `limit` bits.
    fn check_registry(groups: &[&[Bug]], variants: &[Bug], limit: u32) -> Result<(), String> {
        let mut seen = BTreeSet::new();
        for bug in groups.iter().flat_map(|g| g.iter().copied()) {
            if !seen.insert(bug as u32) {
                return Err(format!("{bug:?} appears in more than one group"));
            }
            if (bug as u32) >= limit {
                return Err(format!("guard mask exhausted: {bug:?}"));
            }
        }
        for bug in variants {
            if !seen.contains(&(*bug as u32)) {
                return Err(format!("{bug:?} is declared but in no group"));
            }
        }
        if seen.len() != variants.len() {
            return Err(format!(
                "{} grouped bugs but {} declared variants",
                seen.len(),
                variants.len()
            ));
        }
        Ok(())
    }

    #[test]
    fn the_groups_partition_every_variant_below_the_mask_bound() {
        assert_eq!(
            check_registry(Bug::GROUPS, Bug::VARIANTS, MASK_BITS),
            Ok(())
        );
    }

    #[test]
    fn a_registry_with_a_missing_duplicate_or_overflowing_bug_is_refused() {
        let declared = Bug::VARIANTS;
        let skip = [Bug::SkipFsync];
        let fence = [Bug::DuplicateBeforeFence];

        let missing: Vec<&[Bug]> = Bug::GROUPS
            .iter()
            .copied()
            .filter(|g| *g != &fence[..])
            .collect();
        let err = check_registry(&missing, declared, MASK_BITS).unwrap_err();
        assert!(err.contains("DuplicateBeforeFence"), "{err}");

        let mut duplicated = Bug::GROUPS.to_vec();
        duplicated.push(&skip);
        let err = check_registry(&duplicated, declared, MASK_BITS).unwrap_err();
        assert!(err.contains("more than one group"), "{err}");

        let err = check_registry(Bug::GROUPS, declared, 8).unwrap_err();
        assert!(err.contains("mask exhausted"), "{err}");

        // A bug listed twice inside one group is refused as well.
        let twice = [Bug::SkipFsync, Bug::SkipFsync];
        let err = check_registry(&[&twice], declared, MASK_BITS).unwrap_err();
        assert!(err.contains("more than one group"), "{err}");
    }
}

#[cfg(all(test, feature = "planted-bugs"))]
mod tests {
    use super::*;

    fn all_bugs() -> impl Iterator<Item = Bug> {
        Bug::GROUPS.iter().flat_map(|g| g.iter().copied())
    }

    #[test]
    fn every_log_and_storage_guard_has_a_distinct_mask_bit() {
        for bug in all_bugs() {
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

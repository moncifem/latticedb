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
}

impl Bug {
    /// Defects in the row store, detected by the row simulation rather than the log simulation.
    pub const ROW: [Bug; 3] = [
        Bug::RowConflictCheckSkipped,
        Bug::RowPartialApply,
        Bug::RowRecoveryDropsDeletes,
    ];

    /// Every defect, for the detection suite.
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
        static MASK: Cell<u32> = const { Cell::new(0) };
    }

    fn bit(b: Bug) -> u32 {
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

//! Admission of operations from an untrusted peer (threat-model gap G-08).
//!
//! The CRDT modules converge on any set of valid operations, which says nothing about how many
//! operations one peer may send, which node id it may use, or how far ahead of the real clock its
//! ids may be. This module holds the shared rules and the named limits; [`crate::crdt_text`],
//! [`crate::crdt_tree`] and [`crate::crdt`] call it at their `*_from` entry points, before they
//! allocate or change any state.
//!
//! A [`PeerContext`] is built by the caller (the sync layer) from facts it authenticated itself:
//! the node id bound to the peer's session and the time it reads from its own clock. Nothing in
//! it comes from the peer's message. The library never reads a clock, so every decision is a
//! function of its arguments and of replica state.
//!
//! What this does not do: it cannot bind a node id to a principal (the caller does that when it
//! builds the context), and it cannot check a causal-stability frontier (see the compaction
//! sections of [`crate::crdt_text`] and [`crate::crdt_tree`]).

use crate::bugs::{self, Bug};
use lattice_abi::Hlc;

/// Most operations one batch from a peer may carry; a longer batch is refused whole.
pub const MAX_BATCH_OPS: usize = 1024;
/// Most tags (live adds plus removed tags) an [`crate::crdt::OrSet`] accepts through its
/// `*_from` entry points.
pub const MAX_SET_TAGS: usize = 1 << 20;

/// What the caller knows about the peer an operation came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerContext {
    /// The node id bound to the authenticated peer. Every operation id the peer sends directly
    /// must carry it.
    pub node: u32,
    /// The caller's own wall clock, in milliseconds.
    pub now_ms: u64,
    /// How far ahead of `now_ms` an operation id may be.
    pub max_drift_ms: u64,
}

/// Why an operation, batch or state from a peer was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PeerError {
    /// An operation id names a node other than the one bound to the peer.
    #[error("operation id carries node {claimed}, peer is bound to node {bound}")]
    ForgedNode {
        /// The node the operation id declares.
        claimed: u32,
        /// The node the peer is bound to.
        bound: u32,
    },
    /// An operation id is further ahead of the receiver's clock than allowed.
    #[error("operation id is {ahead_ms} ms ahead of the clock, more than the allowed {max_ms} ms")]
    DriftTooLarge {
        /// How far ahead the id is.
        ahead_ms: u64,
        /// The configured bound.
        max_ms: u64,
    },
    /// A batch carries more than [`MAX_BATCH_OPS`] operations.
    #[error("batch of {len} operations exceeds the limit of {max}")]
    BatchTooLarge {
        /// Operations in the batch.
        len: usize,
        /// The limit.
        max: usize,
    },
    /// Accepting the state would take a set past [`MAX_SET_TAGS`].
    #[error("set would exceed {max} tags")]
    SetTooLarge {
        /// The limit.
        max: usize,
    },
}

/// A batch from a peer stopped early.
///
/// Every check that needs no replica state (batch length, node binding, drift, operation shape)
/// runs on the whole batch before the first operation is applied, so a refusal there leaves the
/// replica untouched and `applied` is 0. Once application starts, a refusal stops the batch and
/// the operations before it stay applied (each is valid on its own; the CRDTs are idempotent, so
/// the sender may resend the batch).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("batch refused after {applied} applied operations: {error}")]
pub struct BatchError<E: std::error::Error + 'static> {
    /// Operations applied before the refusal; also the index of the refused operation.
    pub applied: usize,
    /// Why the batch stopped.
    pub error: E,
}

impl PeerContext {
    /// Checks an operation id against the node binding and the drift bound.
    pub fn admit(&self, id: Hlc) -> Result<(), PeerError> {
        if id.node != self.node && !bugs::active(Bug::CrdtNodeBindingSkipped) {
            return Err(PeerError::ForgedNode {
                claimed: id.node,
                bound: self.node,
            });
        }
        self.admit_time(id)
    }

    /// Checks only the drift bound; for state that legitimately carries other replicas' ids.
    pub fn admit_time(&self, id: Hlc) -> Result<(), PeerError> {
        let ahead = id.wall_ms.saturating_sub(self.now_ms);
        if ahead > self.max_drift_ms && !bugs::active(Bug::CrdtDriftNotChecked) {
            return Err(PeerError::DriftTooLarge {
                ahead_ms: ahead,
                max_ms: self.max_drift_ms,
            });
        }
        Ok(())
    }

    /// Refuses a batch longer than [`MAX_BATCH_OPS`], before anything is looked at.
    pub fn check_batch(&self, len: usize) -> Result<(), PeerError> {
        if len > MAX_BATCH_OPS && !bugs::active(Bug::CrdtBatchLimitSkipped) {
            return Err(PeerError::BatchTooLarge {
                len,
                max: MAX_BATCH_OPS,
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
    use super::*;

    fn h(wall_ms: u64, node: u32) -> Hlc {
        Hlc {
            wall_ms,
            counter: 0,
            node,
        }
    }

    const CTX: PeerContext = PeerContext {
        node: 7,
        now_ms: 1_000,
        max_drift_ms: 50,
    };

    #[test]
    fn admits_exactly_the_bound_and_refuses_one_past() {
        assert_eq!(CTX.admit(h(1_050, 7)), Ok(()));
        assert_eq!(
            CTX.admit(h(1_051, 7)),
            Err(PeerError::DriftTooLarge {
                ahead_ms: 51,
                max_ms: 50
            })
        );
        assert_eq!(CTX.admit(h(0, 7)), Ok(()), "past ids are never drift");
        assert!(CTX.admit(h(u64::MAX, 7)).is_err());
    }

    #[test]
    fn refuses_a_forged_node_before_looking_at_time() {
        assert_eq!(
            CTX.admit(h(u64::MAX, 8)),
            Err(PeerError::ForgedNode {
                claimed: 8,
                bound: 7
            })
        );
        assert_eq!(CTX.admit_time(h(1_000, 8)), Ok(()));
    }

    #[test]
    fn batch_limit_is_inclusive() {
        assert_eq!(CTX.check_batch(MAX_BATCH_OPS), Ok(()));
        assert_eq!(
            CTX.check_batch(MAX_BATCH_OPS + 1),
            Err(PeerError::BatchTooLarge {
                len: MAX_BATCH_OPS + 1,
                max: MAX_BATCH_OPS
            })
        );
    }
}

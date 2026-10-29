//! Kernel: typestate transactions, the write-ahead log and its format, the hybrid logical clock,
//! idempotent commits with fencing, and the policy and schema validation traits.
//!
//! Layer L3. Must not: open sockets, spawn background loops, know any pack, load models.
//!
//! The kernel is synchronous and generic over the seams, so the same code runs against the real
//! filesystem and against the deterministic simulator in `lattice-sim`.
//!
//! ## The transaction typestate
//!
//! A change set can only reach the log by passing through every state in order. Skipping a state
//! does not compile:
//!
//! ```compile_fail
//! use lattice_core::{ChangeSet, Kernel, Config};
//! use lattice_seams::{Clock, Rng, Io};
//! fn skip<I: Io, C: Clock, R: Rng>(k: &mut Kernel<I, C, R>, cs: ChangeSet) {
//!     let open = k.begin(cs);
//!     // `log` needs a `Txn<Prepared>`, not a `Txn<Open>`.
//!     let _ = k.log(open);
//! }
//! ```
#![forbid(unsafe_code)]

pub mod bugs;
pub mod chain;
pub mod changeset;
pub mod crc;
pub mod crdt;
pub mod crdt_tree;
pub mod hlc;
pub mod kernel;
pub mod ledger;
pub mod log;
pub mod metrics;
pub mod pipeline;
pub mod record;
pub mod scrub;
pub mod txn;

pub use changeset::{ChangeSet, ChangeSetId, ChangeSetTag, DecodeError, EncodeError, TurnRef};
pub use hlc::{HlcClock, HlcError};
pub use kernel::{Config, Kernel, Receipt, Recovery};
pub use log::{Log, LogError, RecoveryReport};
pub use txn::{
    AllowAll, Applied, Authorized, Durable, Logged, Open, Policy, PrepareOutcome, Prepared, Txn,
    TxnError, Validated, Validator,
};

/// Layer marker used by the layer check. Do not remove.
pub const LAYER: u8 = 3;

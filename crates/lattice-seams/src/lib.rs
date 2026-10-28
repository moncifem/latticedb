//! Seams: the only doors through which deterministic crates reach time, storage, randomness,
//! threads and the network. Real implementations live here; simulated ones live in `lattice-sim`.
//!
//! Layer L1. Must not: contain database logic.
//!
//! ## Durability contract of [`Io`]
//!
//! * Bytes written with [`Io::append`] are visible to later reads immediately but are only
//!   **durable** (survive a crash) after [`Io::sync`] on that path returns `Ok`.
//! * Creating a file is only durable after [`Io::sync_dir`] on its parent directory returns `Ok`.
//! * A failed `sync` leaves the durability of everything written since the previous successful
//!   `sync` unknown. Callers must treat the file as poisoned and recover from disk.
#![forbid(unsafe_code)]
#![allow(
    clippy::disallowed_methods,
    clippy::disallowed_types,
    clippy::print_stderr
)]

pub mod conformance;
mod real;
#[cfg(all(feature = "uring", target_os = "linux"))]
mod uring;
#[cfg(all(feature = "uring", target_os = "linux"))]
pub use uring::UringIo;

pub use real::{OsRng, StdIo, SystemClock, ThreadSpawner};

use lattice_abi::{Classify, ErrorClass};

/// Layer marker used by the layer check. Do not remove.
pub const LAYER: u8 = 1;

/// Errors from [`Io`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IoError {
    /// The path does not exist.
    #[error("not found: {0}")]
    NotFound(String),
    /// The device is full.
    #[error("no space left on device")]
    NoSpace,
    /// Any other failure, including injected faults in simulation.
    #[error("io error: {0}")]
    Other(String),
}

impl Classify for IoError {
    fn class(&self) -> ErrorClass {
        match self {
            IoError::NotFound(_) => ErrorClass::Rejected,
            IoError::NoSpace => ErrorClass::Retryable,
            IoError::Other(_) => ErrorClass::Fatal,
        }
    }
}

/// Wall clock in milliseconds since the Unix epoch.
pub trait Clock {
    /// The current time. Simulated clocks may jump backwards to model clock skew.
    fn now_ms(&self) -> u64;
}

/// A source of randomness. Simulation uses a seeded generator so runs replay exactly.
pub trait Rng {
    /// The next 64 random bits.
    fn next_u64(&mut self) -> u64;
}

/// Storage. Paths are `/`-separated and relative to the implementation's root.
pub trait Io {
    /// Whether a file exists.
    fn exists(&self, path: &str) -> bool;
    /// File length in bytes.
    fn len(&self, path: &str) -> Result<u64, IoError>;
    /// Reads the whole file, including bytes that are not yet durable.
    fn read(&self, path: &str) -> Result<Vec<u8>, IoError>;
    /// Appends bytes, creating the file if needed. Not durable until [`Io::sync`].
    fn append(&mut self, path: &str, data: &[u8]) -> Result<(), IoError>;
    /// Shrinks the file to `len` bytes.
    fn truncate(&mut self, path: &str, len: u64) -> Result<(), IoError>;
    /// Makes everything appended to `path` durable.
    fn sync(&mut self, path: &str) -> Result<(), IoError>;
    /// Makes directory entries (file creations, removals) in `dir` durable.
    fn sync_dir(&mut self, dir: &str) -> Result<(), IoError>;
    /// File names (not paths) directly inside `dir`, sorted.
    fn list(&self, dir: &str) -> Result<Vec<String>, IoError>;
    /// Removes a file.
    fn remove(&mut self, path: &str) -> Result<(), IoError>;
    /// Creates a directory and its parents. Like a file creation, the new directories are only
    /// durable after [`Io::sync_dir`] on their parent.
    fn create_dir_all(&mut self, dir: &str) -> Result<(), IoError>;
    /// Takes an exclusive lock named by `path`, failing if another handle or process holds it.
    /// The lock is released by [`Io::unlock`] and automatically when the process dies.
    fn lock(&mut self, path: &str) -> Result<(), IoError>;
    /// Releases a lock taken with [`Io::lock`]. Releasing a lock that is not held is a no-op.
    fn unlock(&mut self, path: &str);
}

/// Spawns background work. Simulation runs tasks on one deterministic scheduler.
pub trait Spawner {
    /// Runs `task` concurrently (real) or at a deterministic point (simulated).
    fn spawn(&mut self, task: Box<dyn FnOnce() + Send + 'static>);
}

/// Node identifier on the simulated or real network.
pub type NodeId = u32;

/// Message-oriented network seam. The real implementation arrives in Phase 3; Phase 1 only needs
/// the simulated one for the replication and sync models.
pub trait Net {
    /// Queues a message from `from` to `to`. Delivery is not guaranteed.
    fn send(&mut self, from: NodeId, to: NodeId, bytes: Vec<u8>);
    /// Takes the next message delivered to `node`, with its sender.
    fn recv(&mut self, node: NodeId) -> Option<(NodeId, Vec<u8>)>;
}

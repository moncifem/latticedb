//! The only crate allowed to contain unsafe code (io_uring, mmap wrappers, stop-signal handlers).
//!
//! Layer L0. Must not: Contain business logic, or be used except through lattice-seams.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

/// Layer marker used by the layer check. Do not remove.
pub const LAYER: u8 = 0;

/// Synchronous ownership boundary around Linux io_uring requests, for the opt-in runtime spike.
#[cfg(all(feature = "uring", target_os = "linux"))]
pub mod uring;

/// SIGTERM and SIGINT counting, so a service can stop cleanly without an async runtime.
#[cfg(unix)]
pub mod signals;

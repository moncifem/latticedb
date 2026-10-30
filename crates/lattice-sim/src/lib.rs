//! Simulated seams and fault injection for deterministic simulation testing.
//!
//! Layer L2. Must not: use real time, threads or the real filesystem.
//!
//! Everything here is driven by a seed, so a failing run replays exactly. [`SimIo`] models the
//! gap between written and durable bytes and injects crashes, torn writes, fsync errors, lost
//! directory entries, disk-full and bit rot. [`SimIo::trace_hash`] fingerprints every operation so
//! the determinism self-check can compare two runs of the same seed.
#![forbid(unsafe_code)]

mod clock;
mod io;
mod net;
mod rng;

pub use clock::SimClock;
pub use io::{FaultProfile, SIM_PAGE_BYTES, SimIo};
pub use net::SimNet;
pub use rng::SeededRng;

/// Layer marker used by the layer check. Do not remove.
pub const LAYER: u8 = 2;

/// FNV-1a accumulator used for trace fingerprints.
#[derive(Debug, Clone, Copy)]
pub struct TraceHash(u64);

impl Default for TraceHash {
    fn default() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }
}

impl TraceHash {
    /// Mixes bytes into the hash.
    pub fn write(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.0 ^= u64::from(*b);
            self.0 = self.0.wrapping_mul(0x0100_0000_01b3);
        }
    }

    /// Mixes a number into the hash.
    pub fn write_u64(&mut self, v: u64) {
        self.write(&v.to_le_bytes());
    }

    /// The current fingerprint.
    pub fn value(&self) -> u64 {
        self.0
    }
}

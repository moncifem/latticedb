//! Bounded PostgreSQL cancel-key registry, without sockets, clocks or ambient randomness.
//!
//! A host registers a connection and sends its [`CancelKey`] in `BackendKeyData`. At each statement
//! start it calls [`CancelRegistry::begin_query`] and passes that token to the executor. A separate
//! connection handling `Startup::Cancel` calls [`CancelRegistry::cancel`], then closes without a
//! protocol response. The executor must poll its token between bounded units of work. This module
//! does not yet wire cancellation into [`crate::session::Session`] or interrupt blocking I/O.
//!
//! Credential lookup scans the entire configured capacity, including inactive slots, and compares
//! both 32-bit words with arithmetic masks instead of an early-exit secret comparison. The final
//! success branch is unavoidable when signaling a matching token. This is fixed-work source code,
//! not a formal guarantee of identical timings on every compiler and processor. Production callers
//! must supply a cryptographically secure [`Rng`]; deterministic tests can supply a seeded seam.
//! PIDs are never reused within a registry, so an expired credential cannot target a new connection
//! even when the RNG repeats a secret. Create one registry for the server lifetime, not per query.

use lattice_seams::Rng;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Hard allocation ceiling, independent of caller configuration.
pub const MAX_ENTRIES: usize = 4096;

/// The signed 32-bit fields used by PostgreSQL's BackendKeyData and CancelRequest messages.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct CancelKey {
    /// Registry-assigned connection id, positive and unique during this registry's lifetime.
    pub pid: i32,
    /// Connection secret; must not be logged.
    pub secret: i32,
}
impl fmt::Debug for CancelKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CancelKey")
            .field("pid", &self.pid)
            .field("secret", &"[redacted]")
            .finish()
    }
}

/// One statement's shared, irreversible cancellation signal.
///
/// This flag publishes no other state. The host determines request/poll ordering; the same order
/// produces the same result. A new statement gets a fresh token instead of resetting an old one.
#[derive(Clone, Default)]
pub struct CancellationToken(Arc<AtomicBool>);
impl CancellationToken {
    /// Whether cancellation has been requested for this statement.
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
    fn request(&self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// Registration or lookup could not produce a live query token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelError {
    /// Capacity must be between one and [`MAX_ENTRIES`], inclusive.
    InvalidCapacity,
    /// A zero lifetime is invalid, rather than creating an already-expired key.
    InvalidLifetime,
    /// The expiry timestamp cannot be represented.
    ExpiryOverflow,
    /// All configured slots are occupied by live connections.
    Full,
    /// Every positive signed 32-bit PID has been issued; PIDs are deliberately not reused.
    PidExhausted,
    /// The credential is unknown, expired or has been unregistered.
    UnknownKey,
}
impl fmt::Display for CancelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidCapacity => "invalid cancel registry capacity",
            Self::InvalidLifetime => "invalid cancel key lifetime",
            Self::ExpiryOverflow => "cancel key expiry overflow",
            Self::Full => "cancel registry full",
            Self::PidExhausted => "cancel registry pid range exhausted",
            Self::UnknownKey => "unknown cancel key",
        })
    }
}
impl std::error::Error for CancelError {}

struct Entry {
    active: bool,
    key: CancelKey,
    expires_at: u64,
    token: CancellationToken,
}
impl Entry {
    fn empty() -> Self {
        Self {
            active: false,
            key: CancelKey { pid: 0, secret: 0 },
            expires_at: 0,
            token: CancellationToken::default(),
        }
    }
    fn deactivate(&mut self) {
        self.token.request();
        self.active = false;
        self.key.secret = 0;
    }
}

/// Fixed-capacity connection keys and their current query tokens.
///
/// Time is an explicit timestamp in caller-selected units; use the same units for lifetimes.
/// Observed expiry is irreversible, even if subsequent timestamps move backwards. Before expiry
/// has been observed, backwards clock movement can extend the effective lifetime. A host serving
/// multiple threads must serialize registry access; tokens themselves can cross thread boundaries.
pub struct CancelRegistry {
    entries: Vec<Entry>,
    next_pid: u32,
}
impl CancelRegistry {
    /// Allocates all slots up front, rejecting an unbounded caller-selected capacity.
    pub fn new(capacity: usize) -> Result<Self, CancelError> {
        if capacity == 0 || capacity > MAX_ENTRIES {
            return Err(CancelError::InvalidCapacity);
        }
        Ok(Self {
            entries: (0..capacity).map(|_| Entry::empty()).collect(),
            next_pid: 1,
        })
    }

    /// Configured capacity, also the number of slots examined for a credential lookup.
    pub fn capacity(&self) -> usize {
        self.entries.len()
    }

    /// Number of active registrations; call [`Self::expire`] to observe a new timestamp first.
    pub fn len(&self) -> usize {
        self.entries.iter().filter(|entry| entry.active).count()
    }

    /// Whether no active registrations remain.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Expires registrations at or before `now`, cancels their tokens, and returns the count removed.
    pub fn expire(&mut self, now: u64) -> usize {
        let mut count = 0;
        for entry in &mut self.entries {
            if entry.active && now >= entry.expires_at {
                entry.deactivate();
                count += 1;
            }
        }
        count
    }

    /// Allocates a connection key after expiry cleanup. Refusal does not consume RNG output or a PID.
    pub fn register(
        &mut self,
        rng: &mut (impl Rng + ?Sized),
        now: u64,
        lifetime: u64,
    ) -> Result<CancelKey, CancelError> {
        if lifetime == 0 {
            return Err(CancelError::InvalidLifetime);
        }
        let expires_at = now
            .checked_add(lifetime)
            .ok_or(CancelError::ExpiryOverflow)?;
        self.expire(now);
        if self.next_pid > i32::MAX as u32 {
            return Err(CancelError::PidExhausted);
        }
        let entry = self
            .entries
            .iter_mut()
            .find(|entry| !entry.active)
            .ok_or(CancelError::Full)?;
        let key = CancelKey {
            pid: self.next_pid as i32,
            secret: rng.next_u64() as i32,
        };
        self.next_pid += 1;
        *entry = Entry {
            active: true,
            key,
            expires_at,
            token: CancellationToken::default(),
        };
        Ok(key)
    }

    /// Starts a new statement under a live connection key. Any previous statement is cancelled.
    ///
    /// A cancel request received while idle does not poison this new token. Existing token clones
    /// remain cancelled and can never become live again. Starting an overlapping statement therefore
    /// cannot orphan its predecessor's signal. The connection key and expiry remain unchanged.
    pub fn begin_query(
        &mut self,
        key: CancelKey,
        now: u64,
    ) -> Result<CancellationToken, CancelError> {
        self.expire(now);
        let index = self.find_slot(key).ok_or(CancelError::UnknownKey)?;
        let entry = self.entries.get_mut(index).ok_or(CancelError::UnknownKey)?;
        entry.token.request();
        entry.token = CancellationToken::default();
        Ok(entry.token.clone())
    }

    /// Signals the current query iff both fields match a live registration. No wire response is due.
    ///
    /// The returned boolean is for local control flow/tests and must not be exposed as a protocol
    /// oracle. The complete credential search runs before the result is acted on.
    pub fn cancel(&mut self, key: CancelKey, now: u64) -> bool {
        self.expire(now);
        if let Some(index) = self.find_slot(key)
            && let Some(entry) = self.entries.get(index)
        {
            entry.token.request();
            return true;
        }
        false
    }

    /// Removes a live connection and cancels its current query. Expired credentials cannot remove
    /// another connection, and a reused storage slot never reuses its PID.
    pub fn unregister(&mut self, key: CancelKey, now: u64) -> bool {
        self.expire(now);
        if let Some(index) = self.find_slot(key)
            && let Some(entry) = self.entries.get_mut(index)
        {
            entry.deactivate();
            return true;
        }
        false
    }

    fn find_slot(&self, key: CancelKey) -> Option<usize> {
        let mut found = 0u32;
        let mut selected = 0usize;
        for (index, entry) in self.entries.iter().enumerate() {
            let equal = equal_mask(key.pid as u32, entry.key.pid as u32)
                & equal_mask(key.secret as u32, entry.key.secret as u32)
                & 0u32.wrapping_sub(u32::from(entry.active));
            let wide = 0usize.wrapping_sub((equal & 1) as usize);
            selected = (selected & !wide) | (index & wide);
            found |= equal;
        }
        if found == 0 { None } else { Some(selected) }
    }
}

// Arithmetic comparison of complete words; no per-byte loop or early mismatch branch.
fn equal_mask(a: u32, b: u32) -> u32 {
    let difference = std::hint::black_box(a ^ b);
    let nonzero = (difference | difference.wrapping_neg()) >> 31;
    0u32.wrapping_sub(nonzero ^ 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    struct Counter(u64);
    impl lattice_seams::Rng for Counter {
        fn next_u64(&mut self) -> u64 {
            self.0 += 1;
            self.0
        }
    }
    struct Constant;
    impl lattice_seams::Rng for Constant {
        fn next_u64(&mut self) -> u64 {
            0
        }
    }

    #[test]
    fn matching_both_words_is_required_and_only_the_selected_query_is_cancelled() {
        let mut registry = CancelRegistry::new(3).unwrap();
        let mut rng = Counter(0);
        let first = registry.register(&mut rng, 10, 20).unwrap();
        let second = registry.register(&mut rng, 10, 20).unwrap();
        let one = registry.begin_query(first, 10).unwrap();
        let two = registry.begin_query(second, 10).unwrap();
        assert_eq!(first, CancelKey { pid: 1, secret: 1 });
        assert_eq!(second, CancelKey { pid: 2, secret: 2 });
        assert!(!registry.cancel(
            CancelKey {
                pid: first.pid,
                secret: second.secret
            },
            11
        ));
        assert!(!registry.cancel(
            CancelKey {
                pid: -1,
                secret: first.secret
            },
            11
        ));
        assert!(!one.is_cancelled());
        assert!(!two.is_cancelled());
        assert!(registry.cancel(second, 11));
        assert!(!one.is_cancelled());
        assert!(two.is_cancelled());
        assert!(registry.cancel(second, 11));
    }

    #[test]
    fn expiration_is_inclusive_and_cannot_be_undone_by_clock_rollback() {
        let mut registry = CancelRegistry::new(1).unwrap();
        let key = registry.register(&mut Constant, 10, 5).unwrap();
        let token = registry.begin_query(key, 14).unwrap();
        assert!(!token.is_cancelled());
        assert!(!registry.cancel(key, 15));
        assert!(token.is_cancelled());
        assert!(registry.is_empty());
        assert!(!registry.cancel(key, 0));
        assert!(matches!(
            registry.begin_query(key, 0),
            Err(CancelError::UnknownKey)
        ));
    }

    #[test]
    fn reused_slots_and_repeated_secrets_cannot_reuse_an_old_credential() {
        let mut registry = CancelRegistry::new(1).unwrap();
        let old = registry.register(&mut Constant, 0, 1).unwrap();
        let previous = registry.begin_query(old, 0).unwrap();
        let new = registry.register(&mut Constant, 1, 5).unwrap();
        assert_eq!(old.secret, new.secret);
        assert_ne!(old.pid, new.pid);
        assert!(previous.is_cancelled());
        let token = registry.begin_query(new, 1).unwrap();
        assert!(!registry.cancel(old, 1));
        assert!(!token.is_cancelled());
        assert!(registry.unregister(new, 1));
        assert!(token.is_cancelled());
        assert!(!registry.unregister(new, 1));
    }

    #[test]
    fn query_tokens_are_fresh_but_previous_tokens_stay_cancelled() {
        let mut registry = CancelRegistry::new(1).unwrap();
        let key = registry.register(&mut Constant, 0, 100).unwrap();
        let first = registry.begin_query(key, 1).unwrap();
        let clone = first.clone();
        assert!(registry.cancel(key, 2));
        assert!(clone.is_cancelled());
        let second = registry.begin_query(key, 3).unwrap();
        assert!(!second.is_cancelled());
        assert!(first.is_cancelled());
        let third = registry.begin_query(key, 4).unwrap();
        assert!(second.is_cancelled());
        assert!(!third.is_cancelled());
        assert!(registry.cancel(key, 5));
        assert!(third.is_cancelled());
    }

    #[test]
    fn invalid_capacity_overflow_full_and_pid_exhaustion_are_bounded() {
        assert!(matches!(
            CancelRegistry::new(0),
            Err(CancelError::InvalidCapacity)
        ));
        assert!(matches!(
            CancelRegistry::new(MAX_ENTRIES + 1),
            Err(CancelError::InvalidCapacity)
        ));
        let mut registry = CancelRegistry::new(1).unwrap();
        let mut rng = Counter(0);
        assert_eq!(
            registry.register(&mut rng, 0, 0),
            Err(CancelError::InvalidLifetime)
        );
        assert_eq!(
            registry.register(&mut rng, u64::MAX, 1),
            Err(CancelError::ExpiryOverflow)
        );
        let key = registry.register(&mut rng, 0, 5).unwrap();
        assert_eq!(registry.register(&mut rng, 0, 5), Err(CancelError::Full));
        assert_eq!(rng.0, 1);
        registry.unregister(key, 0);
        registry.next_pid = i32::MAX as u32;
        let last = registry.register(&mut rng, 0, 5).unwrap();
        assert_eq!(last.pid, i32::MAX);
        registry.unregister(last, 0);
        assert_eq!(
            registry.register(&mut rng, 0, 5),
            Err(CancelError::PidExhausted)
        );
        assert_eq!(rng.0, 2);
    }

    #[test]
    fn debug_output_redacts_the_secret() {
        let rendered = format!(
            "{:?}",
            CancelKey {
                pid: 12,
                secret: 987654321
            }
        );
        assert!(rendered.contains("12"));
        assert!(rendered.contains("[redacted]"));
        assert!(!rendered.contains("987654321"));
    }

    proptest! {
        #[test]
        fn arithmetic_word_comparison_matches_equality(a in any::<u32>(), b in any::<u32>()) {
            prop_assert_eq!(equal_mask(a,b), if a == b { u32::MAX } else { 0 });
            prop_assert_eq!(equal_mask(a,a), u32::MAX);
        }

        #[test]
        fn cancellation_matches_an_independent_connection_model(capacity in 1usize..17, steps in prop::collection::vec((0u8..5, any::<u16>()), 0..100)) {
            let mut registry = CancelRegistry::new(capacity).unwrap();
            let mut rng = Counter(100);
            let mut model: Vec<(CancelKey, CancellationToken, bool)> = Vec::new();
            let mut next_pid = 1;
            for (action, choice) in steps {
                match action {
                    0 => match registry.register(&mut rng, 0, 100) {
                        Ok(key) => {
                            prop_assert!(model.len() < capacity);
                            prop_assert_eq!(key.pid, next_pid); next_pid += 1;
                            let token = registry.begin_query(key, 0).unwrap();
                            model.push((key, token, false));
                        }
                        Err(error) => { prop_assert_eq!(model.len(), capacity); prop_assert_eq!(error, CancelError::Full); }
                    },
                    1..=3 if !model.is_empty() => {
                        let index = usize::from(choice) % model.len();
                        let (key, token, cancelled) = model.get_mut(index).unwrap();
                        match action {
                            1 => { prop_assert!(registry.cancel(*key, 1)); *cancelled = true; }
                            2 => {
                                let old = token.clone();
                                *token = registry.begin_query(*key, 1).unwrap();
                                prop_assert!(old.is_cancelled()); *cancelled = false;
                            }
                            _ => {
                                prop_assert!(registry.unregister(*key, 1)); prop_assert!(token.is_cancelled()); model.remove(index);
                            }
                        }
                    }
                    _ => {
                        let invalid = CancelKey { pid: -1, secret: i32::from(choice) };
                        prop_assert!(!registry.cancel(invalid, 1));
                    }
                }
                prop_assert_eq!(registry.len(), model.len());
                for (_, token, cancelled) in &model { prop_assert_eq!(token.is_cancelled(), *cancelled); }
            }
            let expired = registry.expire(100);
            prop_assert_eq!(expired, model.len()); prop_assert!(registry.is_empty());
            for (key, token, _) in model { prop_assert!(token.is_cancelled()); prop_assert!(!registry.cancel(key, 0)); }
        }
    }
}

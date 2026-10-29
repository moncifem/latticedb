//! Exhaustive two-worker models of the actual registry and instrumented token atomics.
#![cfg(loom)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

#[path = "../../crates/lattice-api/src/cancel.rs"]
mod cancel;
use cancel::{CancelError, CancelRegistry};
use lattice_seams::Rng;
use loom::sync::{Arc, Mutex};
use loom::thread;

struct FixedRng;
impl Rng for FixedRng {
    fn next_u64(&mut self) -> u64 {
        7
    }
}

fn model(f: impl Fn() + Sync + Send + 'static) {
    let mut builder = loom::model::Builder::new();
    builder.max_threads = 3;
    builder.max_branches = 10_000;
    builder.max_permutations = None;
    builder.max_duration = None;
    builder.preemption_bound = None;
    // No permutation, duration or preemption cap: enumerate this bounded model completely.
    builder.check(f);
}

fn replacement_race(omit_cancel: bool) {
    model(move || {
        let mut registry = CancelRegistry::new(1).unwrap();
        let key = registry.register(&mut FixedRng, 0, 10).unwrap();
        let old = registry.begin_query(key, 0).unwrap();
        let state = Arc::new(Mutex::new((registry, false)));
        let starter_state = state.clone();
        let starter = thread::spawn(move || {
            let mut state = starter_state.lock().unwrap();
            let token = state.0.begin_query(key, 0).unwrap();
            state.1 = true;
            token
        });
        let cancel_state = state.clone();
        let canceller = thread::spawn(move || {
            let mut state = cancel_state.lock().unwrap();
            let after_start = state.1;
            // The negative control deliberately loses the host's cancel action.
            if !omit_cancel {
                assert!(state.0.cancel(key, 0));
            }
            after_start
        });
        let fresh = starter.join().unwrap();
        let cancel_after_start = canceller.join().unwrap();
        assert!(
            old.is_cancelled(),
            "replacing a query must cancel its predecessor"
        );
        assert_eq!(
            fresh.is_cancelled(),
            cancel_after_start,
            "cancel follows registry order"
        );
    });
}

#[test]
fn cancellation_and_replacement_follow_serialized_order() {
    replacement_race(false);
}

#[test]
#[should_panic(expected = "cancel follows registry order")]
fn model_detects_a_lost_cancel_action() {
    replacement_race(true);
}

#[test]
fn unregister_cancels_every_issued_token_and_prevents_restart() {
    model(|| {
        let mut registry = CancelRegistry::new(1).unwrap();
        let key = registry.register(&mut FixedRng, 0, 10).unwrap();
        let old = registry.begin_query(key, 0).unwrap();
        let shared = Arc::new(Mutex::new(registry));
        let start_state = shared.clone();
        let starter = thread::spawn(move || start_state.lock().unwrap().begin_query(key, 0));
        let remove_state = shared.clone();
        let remover =
            thread::spawn(move || assert!(remove_state.lock().unwrap().unregister(key, 0)));
        let started = starter.join().unwrap();
        remover.join().unwrap();
        assert!(old.is_cancelled());
        match started {
            Ok(token) => assert!(token.is_cancelled()),
            Err(error) => assert_eq!(error, CancelError::UnknownKey),
        }
        let mut registry = shared.lock().unwrap();
        assert!(registry.is_empty());
        assert!(matches!(
            registry.begin_query(key, 0),
            Err(CancelError::UnknownKey)
        ));
    });
}

#[test]
fn stale_credentials_cannot_cancel_a_reused_slot() {
    model(|| {
        let mut registry = CancelRegistry::new(1).unwrap();
        let stale = registry.register(&mut FixedRng, 0, 10).unwrap();
        assert!(registry.unregister(stale, 0));
        let live = registry.register(&mut FixedRng, 0, 10).unwrap();
        assert_ne!(stale.pid, live.pid);
        assert_eq!(stale.secret, live.secret);
        let shared = Arc::new(Mutex::new(registry));
        let start_state = shared.clone();
        let starter =
            thread::spawn(move || start_state.lock().unwrap().begin_query(live, 0).unwrap());
        let cancel_state = shared.clone();
        let canceller =
            thread::spawn(move || assert!(!cancel_state.lock().unwrap().cancel(stale, 0)));
        let token = starter.join().unwrap();
        canceller.join().unwrap();
        assert!(!token.is_cancelled());
    });
}

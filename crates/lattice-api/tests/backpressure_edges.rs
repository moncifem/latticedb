//! Backpressure edges found by mutation testing: the size accessors of a queue and a hub, memory
//! accounting as a function of the entries held, idempotent acknowledgement, and the hub counters.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use lattice_api::backpressure::{Event, Hub, Limits, Offer, SubscriberQueue};
use std::sync::Arc;

fn limits() -> Limits {
    Limits {
        max_events: 8,
        max_bytes: 1024,
        max_in_flight: 4,
    }
}

fn ev(seq: u64, len: usize) -> Event {
    Event {
        seq,
        payload: Arc::from(vec![7u8; len]),
    }
}

#[test]
fn a_queue_reports_what_it_holds() {
    let mut q = SubscriberQueue::new(limits(), 0);
    assert!(q.is_empty());
    assert_eq!((q.len(), q.queued_bytes()), (0, 0));
    assert_eq!(q.offer(ev(1, 10)), Offer::Queued);
    assert_eq!(q.offer(ev(2, 20)), Offer::Queued);
    assert!(!q.is_empty());
    assert_eq!((q.len(), q.queued_bytes()), (2, 30));
    let mut out = Vec::new();
    assert_eq!(q.drain_into(&mut out, 8), 2);
    q.ack(1).unwrap();
    assert_eq!((q.len(), q.queued_bytes()), (1, 20));
    q.ack(2).unwrap();
    assert!(q.is_empty());
    assert_eq!(q.queued_bytes(), 0);
}

#[test]
fn memory_is_the_entry_array_plus_the_payload_bytes() {
    let entry = std::mem::size_of::<Event>();
    let mut q = SubscriberQueue::new(limits(), 0);
    for seq in 1..=3 {
        q.offer(ev(seq, 100));
    }
    let array = q.memory() - q.queued_bytes();
    assert_eq!(q.queued_bytes(), 300);
    assert_eq!(array % entry, 0, "the array is a whole number of entries");
    assert!(array >= 3 * entry, "it holds at least the three entries");
    assert!(q.memory() <= limits().worst_case_memory());
    assert_eq!(q.limits(), limits());
}

#[test]
fn acknowledging_the_cursor_again_is_allowed_and_going_backwards_is_not() {
    let mut q = SubscriberQueue::new(limits(), 5);
    assert_eq!(q.acked(), 5);
    assert!(
        q.ack(5).is_ok(),
        "an ack at the current cursor changes nothing"
    );
    assert!(q.ack(4).is_err());
    assert!(q.ack(6).is_err(), "nothing has been handed out past 5");
    q.offer(ev(6, 1));
    let mut out = Vec::new();
    q.drain_into(&mut out, 8);
    assert!(q.ack(6).is_ok());
    assert!(q.ack(6).is_ok());
    assert_eq!(q.acked(), 6);
}

#[test]
fn a_hub_counts_publishes_offers_and_shares_and_sums_memory() {
    let mut hub = Hub::new();
    assert!(hub.is_empty());
    assert_eq!(hub.memory(), 0);
    let a = hub.add(limits(), 0);
    let b = hub.add(limits(), 0);
    assert!(!hub.is_empty());
    assert_eq!(hub.len(), 2);
    for seq in 1..=3 {
        let fan = hub.publish(seq, &[1u8; 50]).unwrap();
        assert_eq!((fan.queued, fan.overflowed_now, fan.skipped), (2, 0, 0));
    }
    let stats = hub.stats();
    assert_eq!(
        (
            stats.publishes,
            stats.offers,
            stats.payload_copies,
            stats.shares
        ),
        (3, 6, 3, 6)
    );
    // A shared publish does not copy.
    hub.publish_shared(4, Arc::from(vec![2u8; 10])).unwrap();
    let stats = hub.stats();
    assert_eq!(
        (
            stats.publishes,
            stats.offers,
            stats.payload_copies,
            stats.shares
        ),
        (4, 8, 3, 8)
    );
    let expected = hub.get(a).unwrap().memory() + hub.get(b).unwrap().memory();
    assert_eq!(hub.memory(), expected);
    assert!(hub.memory() > 0);
}

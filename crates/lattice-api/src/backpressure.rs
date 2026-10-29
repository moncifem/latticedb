//! Bounded slow-subscriber backpressure for the sync fan-out (WP38, P3-14).
//!
//! A [`SubscriberQueue`] holds the events one subscriber has been offered and has not yet
//! acknowledged. It is bounded by event count and by payload bytes. An offer never blocks, never
//! waits and never grows a queue past its limits: when the next event does not fit, the queue is
//! emptied, the subscriber is marked [`State::Overflowed`] and the host disconnects it with the
//! resume cursor, which is the sequence of the last event the subscriber acknowledged. Every event
//! after that cursor is still in the log, so reconnecting from the cursor loses nothing (delivery
//! is at least once: events handed out but not acknowledged are sent again). Nothing is dropped
//! without the subscriber being told to resume.
//!
//! A [`Hub`] offers one event to every subscriber queue in O(N). The payload is copied into one
//! shared buffer once per event; each queue stores a reference count increment, not the bytes.
//! Queues are independent, so a stalled subscriber costs the others nothing.
//!
//! There are no threads, clocks or sockets here. Order is the event sequence number, so the module
//! is deterministic and can be driven from a simulator.

use std::collections::VecDeque;
use std::sync::Arc;

/// One event: a strictly increasing sequence number and a shared payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Event {
    /// Position in the log. Also the cursor a subscriber resumes from.
    pub seq: u64,
    /// Payload bytes, shared by every queue that holds the event.
    pub payload: Arc<[u8]>,
}

/// Limits for one subscriber queue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Most unacknowledged events held.
    pub max_events: usize,
    /// Most unacknowledged payload bytes held.
    pub max_bytes: usize,
    /// Most events handed out and not yet acknowledged. A subscriber that does not acknowledge
    /// stops receiving and fills its queue.
    pub max_in_flight: usize,
}

impl Limits {
    /// Upper bound on the memory one queue can use: its entries (the queue's backing array may
    /// grow to twice the event bound plus a small constant) and the payload bytes it references.
    /// Payload bytes are shared between queues, so a hub's real total is usually lower.
    #[must_use]
    pub fn worst_case_memory(&self) -> usize {
        self.max_events
            .saturating_mul(2)
            .saturating_add(4)
            .saturating_mul(std::mem::size_of::<Event>())
            .saturating_add(self.max_bytes)
    }
}

/// Lifecycle of a subscriber queue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// Receiving events.
    Live,
    /// Went over budget. The host must disconnect it and the client resumes after `resume`.
    Overflowed {
        /// Sequence of the last acknowledged event.
        resume: u64,
        /// Events that were queued or offered and not delivered, to be replayed from the log.
        dropped: u64,
    },
}

/// Result of offering one event to one queue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Offer {
    /// Stored.
    Queued,
    /// Did not fit; the queue is now overflowed with this resume cursor.
    Overflowed {
        /// Cursor to resume after.
        resume: u64,
    },
    /// The queue was already overflowed; nothing was stored.
    Disconnected,
    /// The event is at or before the subscriber's cursor; nothing to do.
    Stale,
}

/// Why an acknowledgement was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AckError {
    /// The queue is overflowed.
    Closed,
    /// Acknowledges an event that was not handed out, or goes backwards.
    OutOfRange,
}

/// A bounded outbound queue for one subscriber.
#[derive(Debug)]
pub struct SubscriberQueue {
    limits: Limits,
    queue: VecDeque<Event>,
    bytes: usize,
    /// Entries at the front of `queue` that were handed out and are not acknowledged.
    in_flight: usize,
    acked: u64,
    last_offered: u64,
    state: State,
}

impl SubscriberQueue {
    /// A queue for a subscriber that has seen everything up to and including `cursor`.
    #[must_use]
    pub fn new(limits: Limits, cursor: u64) -> Self {
        Self {
            limits,
            queue: VecDeque::new(),
            bytes: 0,
            in_flight: 0,
            acked: cursor,
            last_offered: cursor,
            state: State::Live,
        }
    }

    /// Offer an event. Constant time except the one overflow that empties the queue. Events must
    /// be offered in increasing sequence order; one at or below the last offered is `Stale`.
    pub fn offer(&mut self, event: Event) -> Offer {
        if let State::Overflowed { .. } = self.state {
            return Offer::Disconnected;
        }
        if event.seq <= self.last_offered {
            return Offer::Stale;
        }
        let new_bytes = self.bytes.saturating_add(event.payload.len());
        if self.queue.len() >= self.limits.max_events || new_bytes > self.limits.max_bytes {
            let dropped = self.queue.len() as u64 + 1;
            self.queue = VecDeque::new();
            self.bytes = 0;
            self.in_flight = 0;
            self.last_offered = event.seq;
            self.state = State::Overflowed {
                resume: self.acked,
                dropped,
            };
            return Offer::Overflowed { resume: self.acked };
        }
        self.last_offered = event.seq;
        self.bytes = new_bytes;
        self.queue.push_back(event);
        Offer::Queued
    }

    /// Hand out up to `max` events not yet handed out, in order, limited by `max_in_flight`.
    /// Appends to `out` and returns how many were added.
    pub fn drain_into(&mut self, out: &mut Vec<Event>, max: usize) -> usize {
        let room = self.limits.max_in_flight.saturating_sub(self.in_flight);
        let waiting = self.queue.len().saturating_sub(self.in_flight);
        let n = max.min(room).min(waiting);
        out.extend(self.queue.iter().skip(self.in_flight).take(n).cloned());
        self.in_flight += n;
        n
    }

    /// Acknowledge every event up to and including `seq`, freeing their budget.
    ///
    /// # Errors
    /// [`AckError::Closed`] after overflow, [`AckError::OutOfRange`] if `seq` goes backwards or is
    /// past the last event handed out.
    pub fn ack(&mut self, seq: u64) -> Result<(), AckError> {
        if let State::Overflowed { .. } = self.state {
            return Err(AckError::Closed);
        }
        if seq < self.acked {
            return Err(AckError::OutOfRange);
        }
        let last_out = self
            .in_flight
            .checked_sub(1)
            .and_then(|i| self.queue.get(i))
            .map(|e| e.seq);
        if seq > self.acked && last_out.is_none_or(|l| seq > l) {
            return Err(AckError::OutOfRange);
        }
        while self.in_flight > 0 && self.queue.front().is_some_and(|e| e.seq <= seq) {
            if let Some(e) = self.queue.pop_front() {
                self.bytes = self.bytes.saturating_sub(e.payload.len());
                self.in_flight -= 1;
            }
        }
        self.acked = seq;
        Ok(())
    }

    /// Current state.
    #[must_use]
    pub fn state(&self) -> State {
        self.state
    }
    /// Sequence of the last acknowledged event (the resume cursor).
    #[must_use]
    pub fn acked(&self) -> u64 {
        self.acked
    }
    /// Events held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.queue.len()
    }
    /// True when no events are held.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }
    /// Payload bytes referenced.
    #[must_use]
    pub fn queued_bytes(&self) -> usize {
        self.bytes
    }
    /// Memory used by this queue's entry array and the payload bytes it references.
    #[must_use]
    pub fn memory(&self) -> usize {
        self.queue.capacity() * std::mem::size_of::<Event>() + self.bytes
    }
    /// The limits in force.
    #[must_use]
    pub fn limits(&self) -> Limits {
        self.limits
    }
}

/// Result of fanning one event out to all subscribers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FanOut {
    /// Queues that stored the event.
    pub queued: usize,
    /// Queues that went over budget on this event.
    pub overflowed_now: usize,
    /// Queues that were already overflowed or stale.
    pub skipped: usize,
}

/// Why a publish was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PublishError {
    /// The sequence is not greater than the last published.
    OutOfOrder,
}

/// Counters for the tests and the gate numbers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Events published.
    pub publishes: u64,
    /// Offers made to individual queues.
    pub offers: u64,
    /// Times a payload was copied into a new buffer (once per publish from borrowed bytes).
    pub payload_copies: u64,
    /// Reference count increments (one per stored offer).
    pub shares: u64,
}

/// Fan-out over many subscriber queues.
#[derive(Debug, Default)]
pub struct Hub {
    subs: Vec<SubscriberQueue>,
    last: u64,
    stats: Stats,
}

impl Hub {
    /// An empty hub.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a subscriber that has seen everything up to `cursor`; returns its index.
    pub fn add(&mut self, limits: Limits, cursor: u64) -> usize {
        self.subs.push(SubscriberQueue::new(limits, cursor));
        self.subs.len() - 1
    }

    /// The queue of subscriber `id`.
    #[must_use]
    pub fn get(&self, id: usize) -> Option<&SubscriberQueue> {
        self.subs.get(id)
    }

    /// Mutable access, for drain and acknowledge.
    pub fn get_mut(&mut self, id: usize) -> Option<&mut SubscriberQueue> {
        self.subs.get_mut(id)
    }

    /// Number of subscribers.
    #[must_use]
    pub fn len(&self) -> usize {
        self.subs.len()
    }

    /// True with no subscribers.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.subs.is_empty()
    }

    /// Counters so far.
    #[must_use]
    pub fn stats(&self) -> Stats {
        self.stats
    }

    /// Total memory used by all queues.
    #[must_use]
    pub fn memory(&self) -> usize {
        self.subs.iter().map(SubscriberQueue::memory).sum()
    }

    /// Publish borrowed bytes: one copy into a shared buffer, then O(N) offers.
    ///
    /// # Errors
    /// [`PublishError::OutOfOrder`] if `seq` is not above the last published.
    pub fn publish(&mut self, seq: u64, payload: &[u8]) -> Result<FanOut, PublishError> {
        if seq <= self.last {
            return Err(PublishError::OutOfOrder);
        }
        self.stats.payload_copies += 1;
        self.publish_shared(seq, Arc::from(payload))
    }

    /// Publish an already shared buffer without copying it.
    ///
    /// # Errors
    /// [`PublishError::OutOfOrder`] if `seq` is not above the last published.
    pub fn publish_shared(&mut self, seq: u64, payload: Arc<[u8]>) -> Result<FanOut, PublishError> {
        if seq <= self.last {
            return Err(PublishError::OutOfOrder);
        }
        self.last = seq;
        self.stats.publishes += 1;
        let mut out = FanOut::default();
        for sub in &mut self.subs {
            self.stats.offers += 1;
            match sub.offer(Event {
                seq,
                payload: Arc::clone(&payload),
            }) {
                Offer::Queued => {
                    out.queued += 1;
                    self.stats.shares += 1;
                }
                Offer::Overflowed { .. } => out.overflowed_now += 1,
                Offer::Disconnected | Offer::Stale => out.skipped += 1,
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(seq: u64, len: usize) -> Event {
        Event {
            seq,
            payload: Arc::from(vec![0u8; len]),
        }
    }
    fn lim(e: usize, b: usize, f: usize) -> Limits {
        Limits {
            max_events: e,
            max_bytes: b,
            max_in_flight: f,
        }
    }

    #[test]
    fn count_limit_overflows_with_acked_cursor() {
        let mut q = SubscriberQueue::new(lim(3, 1000, 10), 5);
        assert_eq!(q.offer(ev(6, 1)), Offer::Queued);
        assert_eq!(q.offer(ev(7, 1)), Offer::Queued);
        assert_eq!(q.offer(ev(8, 1)), Offer::Queued);
        assert_eq!(q.offer(ev(9, 1)), Offer::Overflowed { resume: 5 });
        assert_eq!(
            q.state(),
            State::Overflowed {
                resume: 5,
                dropped: 4
            }
        );
        assert_eq!((q.len(), q.queued_bytes()), (0, 0));
    }

    #[test]
    fn byte_limit_overflows() {
        let mut q = SubscriberQueue::new(lim(100, 10, 10), 0);
        assert_eq!(q.offer(ev(1, 6)), Offer::Queued);
        assert_eq!(q.offer(ev(2, 4)), Offer::Queued);
        assert_eq!(q.offer(ev(3, 1)), Offer::Overflowed { resume: 0 });
    }

    #[test]
    fn single_oversize_event_overflows_at_once() {
        let mut q = SubscriberQueue::new(lim(100, 10, 10), 3);
        assert_eq!(q.offer(ev(4, 11)), Offer::Overflowed { resume: 3 });
    }

    #[test]
    fn ack_frees_budget_and_moves_cursor() {
        let mut q = SubscriberQueue::new(lim(2, 100, 10), 0);
        q.offer(ev(1, 10));
        q.offer(ev(2, 10));
        let mut out = Vec::new();
        assert_eq!(q.drain_into(&mut out, 10), 2);
        q.ack(2).unwrap();
        assert_eq!((q.len(), q.queued_bytes(), q.acked()), (0, 0, 2));
        assert_eq!(q.offer(ev(3, 10)), Offer::Queued);
    }

    #[test]
    fn overflow_resume_is_last_ack_not_last_delivery() {
        let mut q = SubscriberQueue::new(lim(3, 100, 10), 0);
        q.offer(ev(1, 1));
        q.offer(ev(2, 1));
        let mut out = Vec::new();
        q.drain_into(&mut out, 10);
        q.ack(1).unwrap();
        q.offer(ev(3, 1));
        q.offer(ev(4, 1));
        // 2 handed out but unacked, 3 and 4 waiting: full.
        assert_eq!(q.offer(ev(5, 1)), Offer::Overflowed { resume: 1 });
    }

    #[test]
    fn in_flight_window_stops_delivery() {
        let mut q = SubscriberQueue::new(lim(10, 100, 2), 0);
        for s in 1..=5 {
            q.offer(ev(s, 1));
        }
        let mut out = Vec::new();
        assert_eq!(q.drain_into(&mut out, 10), 2);
        assert_eq!(q.drain_into(&mut out, 10), 0);
        q.ack(1).unwrap();
        assert_eq!(q.drain_into(&mut out, 10), 1);
        let seqs: Vec<u64> = out.iter().map(|e| e.seq).collect();
        assert_eq!(seqs, [1, 2, 3]);
    }

    #[test]
    fn ack_errors() {
        let mut q = SubscriberQueue::new(lim(5, 100, 5), 0);
        q.offer(ev(1, 1));
        assert_eq!(q.ack(1), Err(AckError::OutOfRange), "not handed out yet");
        let mut out = Vec::new();
        q.drain_into(&mut out, 1);
        q.ack(1).unwrap();
        assert_eq!(q.ack(0), Err(AckError::OutOfRange), "backwards");
        q.ack(1).unwrap();
        let mut o = SubscriberQueue::new(lim(0, 0, 0), 0);
        o.offer(ev(1, 1));
        assert_eq!(o.ack(0), Err(AckError::Closed));
    }

    #[test]
    fn stale_and_disconnected_offers_store_nothing() {
        let mut q = SubscriberQueue::new(lim(1, 100, 5), 4);
        assert_eq!(q.offer(ev(4, 1)), Offer::Stale);
        assert_eq!(q.offer(ev(5, 1)), Offer::Queued);
        assert!(matches!(q.offer(ev(6, 1)), Offer::Overflowed { .. }));
        assert_eq!(q.offer(ev(7, 1)), Offer::Disconnected);
        assert_eq!(q.len(), 0);
    }

    #[test]
    fn fan_out_shares_one_buffer() {
        let mut hub = Hub::new();
        for _ in 0..50 {
            hub.add(lim(8, 1000, 8), 0);
        }
        let payload: Arc<[u8]> = Arc::from(vec![7u8; 100]);
        let r = hub.publish_shared(1, Arc::clone(&payload)).unwrap();
        assert_eq!(r.queued, 50);
        // our handle plus one per queue: the bytes exist once.
        assert_eq!(Arc::strong_count(&payload), 51);
        assert_eq!(hub.stats().payload_copies, 0);
        hub.publish(2, b"abc").unwrap();
        assert_eq!(hub.stats().payload_copies, 1);
        assert_eq!(hub.publish(2, b"x"), Err(PublishError::OutOfOrder));
    }

    #[test]
    fn slow_subscriber_does_not_affect_others() {
        let mut hub = Hub::new();
        let slow = hub.add(lim(2, 100, 2), 0);
        let fast = hub.add(lim(2, 100, 2), 0);
        let mut got = Vec::new();
        for s in 1..=10u64 {
            hub.publish(s, b"x").unwrap();
            let q = hub.get_mut(fast).unwrap();
            q.drain_into(&mut got, 10);
            q.ack(s).unwrap();
        }
        assert_eq!(got.len(), 10);
        assert_eq!(
            hub.get(slow).unwrap().state(),
            State::Overflowed {
                resume: 0,
                dropped: 3
            }
        );
    }
}

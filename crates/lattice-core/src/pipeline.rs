//! The single ordered commit pipeline: a bounded queue in front of the kernel.
//!
//! Writers [`submit`](Pipeline::submit) and get a [`Ticket`] or, when the queue is full,
//! [`Backpressure`] — they are told to wait, nothing is dropped and nothing grows without bound.
//! [`drain`](Pipeline::drain) commits up to a batch of queued change sets with one fsync, in
//! submission order, and returns each outcome with its ticket. Readers never touch the queue.

use crate::changeset::ChangeSet;
use crate::kernel::{Kernel, Receipt};
use crate::txn::TxnError;
use lattice_seams::{Clock, Io, Rng};
use std::collections::VecDeque;

/// Proof that a change set was queued; matches an outcome from [`Pipeline::drain`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Ticket(pub u64);

/// The queue is full: retry after a drain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Backpressure {
    /// How many change sets are waiting.
    pub queued: usize,
}

/// A bounded, ordered front end to the kernel.
pub struct Pipeline<I: Io, C: Clock, R: Rng> {
    kernel: Kernel<I, C, R>,
    queue: VecDeque<(Ticket, ChangeSet)>,
    capacity: usize,
    batch: usize,
    next_ticket: u64,
}

impl<I: Io, C: Clock, R: Rng> Pipeline<I, C, R> {
    /// Wraps a kernel. `capacity` bounds the queue and `batch` the change sets per fsync; both are
    /// at least one.
    pub fn new(kernel: Kernel<I, C, R>, capacity: usize, batch: usize) -> Self {
        Self {
            kernel,
            queue: VecDeque::new(),
            capacity: capacity.max(1),
            batch: batch.max(1),
            next_ticket: 0,
        }
    }

    /// Queues a change set, or says the queue is full.
    pub fn submit(&mut self, cs: ChangeSet) -> Result<Ticket, Backpressure> {
        if self.queue.len() >= self.capacity {
            return Err(Backpressure {
                queued: self.queue.len(),
            });
        }
        let ticket = Ticket(self.next_ticket);
        self.next_ticket += 1;
        self.queue.push_back((ticket, cs));
        Ok(ticket)
    }

    /// Commits the oldest queued change sets (at most one batch) in submission order.
    pub fn drain(&mut self) -> Vec<(Ticket, Result<Receipt, TxnError>)> {
        let n = self.batch.min(self.queue.len());
        let taken: Vec<(Ticket, ChangeSet)> = self.queue.drain(..n).collect();
        let (tickets, sets): (Vec<Ticket>, Vec<ChangeSet>) = taken.into_iter().unzip();
        tickets
            .into_iter()
            .zip(self.kernel.commit_batch(sets))
            .collect()
    }

    /// Change sets waiting for a drain.
    pub fn queued(&self) -> usize {
        self.queue.len()
    }

    /// The kernel behind the queue, for reads and inspection.
    pub fn kernel(&self) -> &Kernel<I, C, R> {
        &self.kernel
    }

    /// The kernel behind the queue, for turns and other direct operations.
    pub fn kernel_mut(&mut self) -> &mut Kernel<I, C, R> {
        &mut self.kernel
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kernel::Config;
    use lattice_sim::{FaultProfile, SeededRng, SimClock, SimIo};
    use std::rc::Rc;

    struct Clk(Rc<SimClock>);
    impl Clock for Clk {
        fn now_ms(&self) -> u64 {
            self.0.now_ms()
        }
    }

    fn pipeline(capacity: usize, batch: usize) -> Pipeline<SimIo, Clk, SeededRng> {
        let io = SimIo::new(1, FaultProfile::clean());
        let (k, _) = Kernel::open(
            io,
            Clk(Rc::new(SimClock::new(1_000))),
            SeededRng::new(1),
            &Config::default(),
        )
        .unwrap();
        Pipeline::new(k, capacity, batch)
    }

    #[test]
    fn a_full_queue_pushes_back_and_a_drain_makes_room() {
        let mut p = pipeline(2, 8);
        for _ in 0..2 {
            let id = p.kernel_mut().new_id();
            assert!(p.submit(ChangeSet::new(id, vec![1])).is_ok());
        }
        let id = p.kernel_mut().new_id();
        assert_eq!(
            p.submit(ChangeSet::new(id, vec![1])),
            Err(Backpressure { queued: 2 })
        );
        assert_eq!(p.drain().len(), 2);
        assert!(p.submit(ChangeSet::new(id, vec![1])).is_ok());
    }

    #[test]
    fn change_sets_commit_in_submission_order_in_bounded_batches() {
        let mut p = pipeline(10, 3);
        assert_eq!(p.queued(), 0);
        let mut tickets = Vec::new();
        for i in 0..7u8 {
            let id = p.kernel_mut().new_id();
            tickets.push(p.submit(ChangeSet::new(id, vec![i])).unwrap());
        }
        assert_eq!(
            tickets,
            [
                Ticket(0),
                Ticket(1),
                Ticket(2),
                Ticket(3),
                Ticket(4),
                Ticket(5),
                Ticket(6)
            ]
        );
        assert_eq!(p.queued(), 7);
        let mut seen = Vec::new();
        let mut lsns = Vec::new();
        // Expectations come from seven submissions and a batch size of three, not from the queue
        // accessor under test. A broken drain/accessor must fail promptly rather than hang forever.
        for (batch_len, remaining) in [(3, 4), (3, 1), (1, 0)] {
            let out = p.drain();
            assert_eq!(out.len(), batch_len);
            assert_eq!(p.queued(), remaining);
            for (t, r) in out {
                seen.push(t);
                match r {
                    Ok(Receipt::Recorded { lsn, .. }) => lsns.push(lsn),
                    other => panic!("{other:?}"),
                }
            }
        }
        assert_eq!(seen, tickets);
        assert_eq!(lsns, [1, 2, 3, 4, 5, 6, 7]);
        assert!(p.drain().is_empty());
        assert_eq!(p.queued(), 0);
        assert_eq!(p.kernel().durable_lsn(), 7);
    }

    #[test]
    fn a_resubmitted_id_is_a_duplicate_through_the_pipeline() {
        let mut p = pipeline(4, 4);
        let id = p.kernel_mut().new_id();
        p.submit(ChangeSet::new(id, vec![1])).unwrap();
        p.submit(ChangeSet::new(id, vec![1])).unwrap();
        let out = p.drain();
        assert!(matches!(out[0].1, Ok(Receipt::Recorded { .. })));
        assert_eq!(out[1].1, Ok(Receipt::Duplicate));
    }
}

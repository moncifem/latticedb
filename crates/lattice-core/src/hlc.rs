//! Hybrid logical clock (see `formats/hlc.md`).

use crate::bugs::{self, Bug};
use lattice_abi::{Classify, ErrorClass, Hlc};

/// Why observing a remote timestamp failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HlcError {
    /// The remote clock is further ahead of ours than the allowed drift.
    #[error("remote clock is {ahead_ms} ms ahead, more than the allowed {max_ms} ms")]
    DriftTooLarge {
        /// How far ahead the remote timestamp is.
        ahead_ms: u64,
        /// The configured bound.
        max_ms: u64,
    },
    /// Both the physical part and the counter are at their maximum; no later timestamp exists.
    #[error("hybrid logical clock exhausted")]
    Exhausted,
}

impl Classify for HlcError {
    fn class(&self) -> ErrorClass {
        ErrorClass::Rejected
    }
}

/// Generates timestamps that never go backwards, even when the wall clock does.
///
/// Properties (tested): strictly increasing per node; merging a remote timestamp never yields a
/// value below either input; the physical part never runs ahead of the real clock by more than the
/// drift bound.
#[derive(Debug, Clone)]
pub struct HlcClock {
    last: Hlc,
    node: u32,
}

impl HlcClock {
    /// A clock for `node`, starting before any timestamp.
    pub fn new(node: u32) -> Self {
        Self {
            last: Hlc {
                wall_ms: 0,
                counter: 0,
                node,
            },
            node,
        }
    }

    /// Restores state from the highest timestamp found in the log.
    pub fn restore(node: u32, highest: Hlc) -> Self {
        Self {
            last: Hlc {
                wall_ms: highest.wall_ms,
                counter: highest.counter,
                node,
            },
            node,
        }
    }

    /// The most recently issued timestamp.
    pub fn last(&self) -> Hlc {
        self.last
    }

    fn next_from(&mut self, wall: u64, floor_counter: Option<u32>) -> Result<Hlc, HlcError> {
        let next = match floor_counter.map(|c| c.checked_add(1)) {
            None => Hlc {
                wall_ms: wall,
                counter: 0,
                node: self.node,
            },
            Some(Some(c)) => Hlc {
                wall_ms: wall,
                counter: c,
                node: self.node,
            },
            // Counter exhausted within one millisecond: move the physical part forward.
            Some(None) => Hlc {
                wall_ms: wall.checked_add(1).ok_or(HlcError::Exhausted)?,
                counter: 0,
                node: self.node,
            },
        };
        self.last = next;
        Ok(next)
    }

    /// A timestamp for a local event at wall-clock time `now_ms`.
    ///
    /// The result is always after every earlier timestamp. It may run ahead of `now_ms` when the
    /// wall clock went backwards or the clock was restored from a future timestamp; only
    /// [`HlcClock::observe`] enforces a drift bound.
    pub fn tick(&mut self, now_ms: u64) -> Result<Hlc, HlcError> {
        let wall = if bugs::active(Bug::HlcRegress) {
            now_ms
        } else {
            now_ms.max(self.last.wall_ms)
        };
        let floor = (wall == self.last.wall_ms && !bugs::active(Bug::HlcCounterStuck))
            .then_some(self.last.counter);
        self.next_from(wall, floor)
    }

    /// Merges a timestamp received from another node.
    pub fn observe(
        &mut self,
        remote: Hlc,
        now_ms: u64,
        max_drift_ms: u64,
    ) -> Result<Hlc, HlcError> {
        let ahead = remote.wall_ms.saturating_sub(now_ms);
        if ahead > max_drift_ms {
            return Err(HlcError::DriftTooLarge {
                ahead_ms: ahead,
                max_ms: max_drift_ms,
            });
        }
        let wall = now_ms.max(self.last.wall_ms).max(remote.wall_ms);
        let mut floor: Option<u32> = None;
        if wall == self.last.wall_ms {
            floor = Some(self.last.counter);
        }
        if wall == remote.wall_ms {
            floor = Some(floor.map_or(remote.counter, |f| f.max(remote.counter)));
        }
        self.next_from(wall, floor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[derive(Debug, Clone)]
    enum Step {
        Tick(u64),
        Observe(Hlc, u64),
    }

    fn arb_step() -> impl Strategy<Value = Step> {
        prop_oneof![
            (0u64..10_000).prop_map(Step::Tick),
            ((0u64..10_000, 0u32..5, 0u32..4), 0u64..10_000).prop_map(|((w, c, n), now)| {
                Step::Observe(
                    Hlc {
                        wall_ms: w,
                        counter: c,
                        node: n,
                    },
                    now,
                )
            }),
        ]
    }

    proptest! {
        #[test]
        fn timestamps_strictly_increase_whatever_the_wall_clock_does(steps in proptest::collection::vec(arb_step(), 1..60)) {
            let mut c = HlcClock::new(1);
            let mut prev = c.last();
            for s in steps {
                let next = match s {
                    Step::Tick(now) => match c.tick(now) {
                        Ok(h) => h,
                        Err(_) => continue,
                    },
                    Step::Observe(r, now) => match c.observe(r, now, u64::MAX) {
                        Ok(h) => {
                            prop_assert!((h.wall_ms, h.counter) > (r.wall_ms, r.counter));
                            h
                        }
                        Err(_) => continue,
                    },
                };
                prop_assert!(next > prev, "{next:?} !> {prev:?}");
                prev = next;
            }
        }

        #[test]
        fn drift_bound_is_enforced(now in 0u64..1000, ahead in 0u64..5000, max in 0u64..2000) {
            let mut c = HlcClock::new(1);
            let r = Hlc { wall_ms: now + ahead, counter: 0, node: 2 };
            prop_assert_eq!(c.observe(r, now, max).is_err(), ahead > max);
        }
    }

    #[test]
    fn counter_exhaustion_moves_wall_forward() {
        let mut c = HlcClock::restore(
            1,
            Hlc {
                wall_ms: 5,
                counter: u32::MAX,
                node: 1,
            },
        );
        let h = c.tick(5).unwrap();
        assert_eq!((h.wall_ms, h.counter), (6, 0));
    }

    #[test]
    fn exhaustion_is_an_error_not_a_regression() {
        let top = Hlc {
            wall_ms: u64::MAX,
            counter: u32::MAX,
            node: 9,
        };
        let mut c = HlcClock::restore(1, top);
        assert_eq!(c.tick(u64::MAX), Err(HlcError::Exhausted));
        assert_eq!(c.observe(top, u64::MAX, u64::MAX), Err(HlcError::Exhausted));
    }

    #[test]
    fn restore_continues_after_the_highest_timestamp() {
        let mut c = HlcClock::restore(
            1,
            Hlc {
                wall_ms: 100,
                counter: 3,
                node: 1,
            },
        );
        assert!(
            c.tick(50).unwrap()
                > Hlc {
                    wall_ms: 100,
                    counter: 3,
                    node: 1
                }
        );
    }
}

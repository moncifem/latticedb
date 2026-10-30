//! How the `latticed` process decides to stop and how long it lets a stop take.
//!
//! Three things can ask for a stop: a stop signal (SIGTERM from a service manager or container
//! runtime, SIGINT from a terminal), the stdin control channel (`shutdown`, or end-of-file) and the
//! listener dying by itself. A stop is the existing clean one: the listener closes, connections are
//! interrupted and joined, and the database thread ends, which releases the data-directory lock.
//! This module only chooses when to start it and bounds it. The stop runs on its own thread so a
//! second stop signal or the stop timeout can still end the process if a worker never returns; the
//! write-ahead log is crash-safe, so a forced exit loses nothing that was acknowledged.
//!
//! The decision logic takes the signal counter and the listener state as closures so it can be tested
//! without sending real signals; the real wiring is covered by the binary tests.

use std::io::{self, BufRead};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

/// How often the supervising loop looks at the signal counter and the listener.
pub const POLL_INTERVAL: Duration = Duration::from_millis(100);
/// Default bound on a graceful stop before the process gives up and exits with a failure.
pub const DEFAULT_STOP_TIMEOUT: Duration = Duration::from_secs(30);

/// Why the process is stopping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopCause {
    /// SIGTERM or SIGINT.
    Signal,
    /// The line `shutdown` on stdin.
    StdinCommand,
    /// End of file on stdin (only when stdin control is on).
    StdinClosed,
    /// The listener ended without being asked to.
    ListenerExited,
    /// Reading the control channel failed.
    ControlFailed(String),
}

/// What reading the stdin control channel concluded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlEvent {
    /// A line `shutdown` was read.
    Shutdown,
    /// End of file.
    Closed,
    /// A read error.
    Failed(String),
}

/// Starts the thread that forwards the stdin control channel, one event at most.
#[must_use]
pub fn spawn_stdin_control() -> Receiver<ControlEvent> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        for line in io::stdin().lock().lines() {
            match line {
                Ok(line) if line.trim() == "shutdown" => {
                    let _ = sender.send(ControlEvent::Shutdown);
                    return;
                }
                Ok(_) => {}
                Err(error) => {
                    let _ = sender.send(ControlEvent::Failed(error.to_string()));
                    return;
                }
            }
        }
        let _ = sender.send(ControlEvent::Closed);
    });
    receiver
}

/// Blocks until something asks the process to stop.
///
/// `control` is the stdin channel, or `None` when stdin control is off, in which case end-of-file or
/// any input on stdin has no effect. `signals` returns how many stop signals have arrived and
/// `listener_finished` reports whether the listener ended on its own.
pub fn wait_for_stop(
    control: Option<&Receiver<ControlEvent>>,
    signals: impl Fn() -> u32,
    listener_finished: impl Fn() -> bool,
) -> StopCause {
    loop {
        if signals() > 0 {
            return StopCause::Signal;
        }
        match control {
            Some(receiver) => match receiver.recv_timeout(POLL_INTERVAL) {
                Ok(ControlEvent::Shutdown) => return StopCause::StdinCommand,
                Ok(ControlEvent::Closed) | Err(RecvTimeoutError::Disconnected) => {
                    return StopCause::StdinClosed;
                }
                Ok(ControlEvent::Failed(error)) => return StopCause::ControlFailed(error),
                Err(RecvTimeoutError::Timeout) => {}
            },
            None => thread::sleep(POLL_INTERVAL),
        }
        if listener_finished() {
            return StopCause::ListenerExited;
        }
    }
}

/// Why a stop was abandoned instead of finishing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForcedBy {
    /// A further stop signal arrived while the stop was running.
    SecondSignal,
    /// The stop did not finish within the configured bound.
    Timeout,
}

/// How a stop ended.
#[derive(Debug, PartialEq, Eq)]
pub enum StopOutcome {
    /// The shutdown finished and reported no error.
    Clean,
    /// The shutdown finished and reported this error.
    Failed(String),
    /// The shutdown was still running and was abandoned.
    Forced(ForcedBy),
}

/// Runs `shutdown` on its own thread and waits for it, giving up at `timeout` or on the next stop
/// signal.
///
/// "Next" means one that arrives after this call starts, and the second one overall: if the stop was
/// itself started by a signal, one more forces it; if it was started by stdin, two do. When it gives
/// up, the shutdown thread is left running and the caller is expected to exit the process.
pub fn stop_within(
    shutdown: impl FnOnce() -> io::Result<()> + Send + 'static,
    signals: impl Fn() -> u32,
    timeout: Duration,
) -> StopOutcome {
    let at_start = signals();
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let _ = sender.send(shutdown());
    });
    let deadline = Instant::now() + timeout;
    loop {
        let now = signals();
        if now > at_start && now >= 2 {
            return StopOutcome::Forced(ForcedBy::SecondSignal);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return StopOutcome::Forced(ForcedBy::Timeout);
        }
        match receiver.recv_timeout(remaining.min(POLL_INTERVAL)) {
            Ok(Ok(())) => return StopOutcome::Clean,
            Ok(Err(error)) => return StopOutcome::Failed(error.to_string()),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                return StopOutcome::Failed("shutdown thread ended without a result".into());
            }
        }
    }
}

/// Installs the SIGTERM and SIGINT counters. A no-op where there are no unix signals.
pub fn install_stop_signal_handlers() -> io::Result<()> {
    #[cfg(unix)]
    {
        lattice_sys::signals::install_stop_handlers()
    }
    #[cfg(not(unix))]
    {
        Ok(())
    }
}

/// How many stop signals have arrived since [`install_stop_signal_handlers`].
#[must_use]
pub fn stop_signals_received() -> u32 {
    #[cfg(unix)]
    {
        lattice_sys::signals::stop_signals_received()
    }
    #[cfg(not(unix))]
    {
        0
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::{Arc, mpsc};

    fn never() -> bool {
        false
    }

    #[test]
    fn a_signal_wins_even_with_stdin_control_off() {
        let count = AtomicU32::new(1);
        let cause = wait_for_stop(None, || count.load(Ordering::SeqCst), never);
        assert_eq!(cause, StopCause::Signal);
    }

    #[test]
    fn stdin_events_map_to_causes_and_are_ignored_when_control_is_off() {
        for (event, expected) in [
            (ControlEvent::Shutdown, StopCause::StdinCommand),
            (ControlEvent::Closed, StopCause::StdinClosed),
            (
                ControlEvent::Failed("boom".into()),
                StopCause::ControlFailed("boom".into()),
            ),
        ] {
            let (sender, receiver) = mpsc::channel();
            sender.send(event).unwrap();
            assert_eq!(wait_for_stop(Some(&receiver), || 0, never), expected);
        }
        // A closed channel is the end of the control thread: treated as end-of-file.
        let (sender, receiver) = mpsc::channel::<ControlEvent>();
        drop(sender);
        assert_eq!(
            wait_for_stop(Some(&receiver), || 0, never),
            StopCause::StdinClosed
        );
        // With control off nothing but a signal or a dead listener stops the wait.
        let polls = AtomicU32::new(0);
        let cause = wait_for_stop(None, || 0, || polls.fetch_add(1, Ordering::SeqCst) >= 3);
        assert_eq!(cause, StopCause::ListenerExited);
        assert!(polls.load(Ordering::SeqCst) >= 4);
    }

    #[test]
    fn a_clean_and_a_failed_shutdown_are_reported_as_such() {
        let out = stop_within(|| Ok(()), || 0, Duration::from_secs(5));
        assert_eq!(out, StopOutcome::Clean);
        let out = stop_within(
            || Err(io::Error::other("lock still held")),
            || 0,
            Duration::from_secs(5),
        );
        assert_eq!(out, StopOutcome::Failed("lock still held".into()));
    }

    #[test]
    fn a_stuck_shutdown_is_abandoned_at_the_timeout() {
        let (release, parked) = mpsc::channel::<()>();
        let started = Instant::now();
        let out = stop_within(
            move || {
                let _ = parked.recv();
                Ok(())
            },
            || 0,
            Duration::from_millis(250),
        );
        assert_eq!(out, StopOutcome::Forced(ForcedBy::Timeout));
        assert!(started.elapsed() >= Duration::from_millis(250));
        assert!(started.elapsed() < Duration::from_secs(5));
        drop(release);
    }

    #[test]
    fn a_second_signal_forces_a_stuck_stop_that_a_signal_started() {
        let count = Arc::new(AtomicU32::new(1));
        let (release, parked) = mpsc::channel::<()>();
        let bumper = Arc::clone(&count);
        let second = thread::spawn(move || {
            thread::sleep(Duration::from_millis(250));
            bumper.fetch_add(1, Ordering::SeqCst);
        });
        let started = Instant::now();
        let watched = Arc::clone(&count);
        let out = stop_within(
            move || {
                let _ = parked.recv();
                Ok(())
            },
            move || watched.load(Ordering::SeqCst),
            Duration::from_secs(60),
        );
        second.join().unwrap();
        assert_eq!(out, StopOutcome::Forced(ForcedBy::SecondSignal));
        assert!(started.elapsed() < Duration::from_secs(10));
        drop(release);
    }

    #[test]
    fn one_signal_during_a_stdin_started_stop_does_not_force_it_but_two_do() {
        let count = Arc::new(AtomicU32::new(0));
        let (release, parked) = mpsc::channel::<()>();
        let bumper = Arc::clone(&count);
        let signaller = thread::spawn(move || {
            thread::sleep(Duration::from_millis(250));
            bumper.fetch_add(1, Ordering::SeqCst);
            thread::sleep(Duration::from_millis(500));
            bumper.fetch_add(1, Ordering::SeqCst);
        });
        let first_seen = Arc::new(AtomicU32::new(0));
        let watched = Arc::clone(&count);
        let seen = Arc::clone(&first_seen);
        let started = Instant::now();
        let out = stop_within(
            move || {
                let _ = parked.recv();
                Ok(())
            },
            move || {
                let now = watched.load(Ordering::SeqCst);
                if now == 1 {
                    seen.store(1, Ordering::SeqCst);
                }
                now
            },
            Duration::from_secs(60),
        );
        signaller.join().unwrap();
        assert_eq!(out, StopOutcome::Forced(ForcedBy::SecondSignal));
        // The single signal was observed and did not end the stop by itself.
        assert_eq!(first_seen.load(Ordering::SeqCst), 1);
        assert!(started.elapsed() >= Duration::from_millis(700));
        drop(release);
    }

    #[test]
    fn signals_that_arrived_before_the_stop_started_do_not_force_it() {
        // Two signals already counted when the stop begins (they arrived within one poll interval):
        // the stop still gets its full time because nothing arrived after it started.
        let out = stop_within(
            || {
                thread::sleep(Duration::from_millis(250));
                Ok(())
            },
            || 2,
            Duration::from_secs(5),
        );
        assert_eq!(out, StopOutcome::Clean);
    }
}

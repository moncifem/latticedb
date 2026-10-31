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
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
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

/// How long startup (recovery included) may run before the process says that a stop signal ends it.
pub const SLOW_STARTUP_NOTICE: Duration = Duration::from_secs(3);

const WATCH_RUNNING: u8 = 0;
const WATCH_FINISHED: u8 = 1;
const WATCH_FIRED: u8 = 2;

/// Ends the watcher started by [`watch_startup`]. Dropping it has the same effect (see the `Drop` note).
pub struct StartupWatch {
    state: Arc<AtomicU8>,
    settled: bool,
}

impl StartupWatch {
    /// Startup is over: the watcher stops looking at the signal counter. Returns `false` when the
    /// watcher had already decided to end the process; the caller must then not carry on.
    #[must_use]
    pub fn try_finish(mut self) -> bool {
        self.settled = true;
        self.state
            .compare_exchange(
                WATCH_RUNNING,
                WATCH_FINISHED,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_ok()
    }

    /// As [`StartupWatch::try_finish`], and when the watcher won the race (a stop signal arrived
    /// first and its action is ending the process) parks this thread until the process is gone, so
    /// the server is never reported as listening after the stop was decided.
    pub fn finish(self) {
        if !self.try_finish() {
            loop {
                thread::park();
            }
        }
    }
}

/// Dropping an unsettled watch (an early error return from startup) is a finish: if the watcher
/// had already decided to end the process, this parks too, so the exit status is the watcher's and
/// not whatever the error path would have returned.
impl Drop for StartupWatch {
    fn drop(&mut self) {
        if !self.settled
            && self
                .state
                .compare_exchange(
                    WATCH_RUNNING,
                    WATCH_FINISHED,
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                )
                .is_err()
            && self.state.load(Ordering::SeqCst) == WATCH_FIRED
        {
            loop {
                thread::park();
            }
        }
    }
}

/// Watches for a stop signal while startup runs on the calling thread.
///
/// Recovery cannot be cancelled and the signal handler only counts, so a stop signal that arrives
/// before [`StartupWatch::finish`] is acted on here: `on_signal` runs at most once, on the watcher
/// thread, and only if the watcher won the single running-to-fired transition against `finish`;
/// it is expected to end the process. If startup is still running after `slow_after`, `on_slow`
/// runs once so the operator knows a stop signal is honoured. The thread ends on its own when the
/// watch is finished or dropped.
pub fn watch_startup(
    signals: impl Fn() -> u32 + Send + 'static,
    slow_after: Duration,
    on_slow: impl FnOnce() + Send + 'static,
    on_signal: impl FnOnce() + Send + 'static,
) -> StartupWatch {
    let state = Arc::new(AtomicU8::new(WATCH_RUNNING));
    let seen = Arc::clone(&state);
    thread::spawn(move || {
        let slow_at = Instant::now() + slow_after;
        let mut on_slow = Some(on_slow);
        while seen.load(Ordering::SeqCst) == WATCH_RUNNING {
            if signals() > 0 {
                if seen
                    .compare_exchange(
                        WATCH_RUNNING,
                        WATCH_FIRED,
                        Ordering::SeqCst,
                        Ordering::SeqCst,
                    )
                    .is_ok()
                {
                    on_signal();
                }
                return;
            }
            if Instant::now() >= slow_at
                && let Some(notice) = on_slow.take()
            {
                notice();
            }
            thread::sleep(Duration::from_millis(20));
        }
    });
    StartupWatch {
        state,
        settled: false,
    }
}

/// Installs the SIGTERM and SIGINT counters. A no-op where there are no unix signals.
pub fn install_stop_signal_handlers() -> io::Result<()> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        lattice_sys::signals::install_stop_handlers()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        Ok(())
    }
}

/// Installs the SIGHUP counter that asks for a credential reload (ADR 0031). A no-op where there are
/// no unix signals. Called only when a credential file is configured: without it SIGHUP keeps its
/// default action.
pub fn install_reload_signal_handler() -> io::Result<()> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        lattice_sys::signals::install_reload_handler()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        Ok(())
    }
}

/// How many reload requests (SIGHUP) have arrived since [`install_reload_signal_handler`].
#[must_use]
pub fn reload_signals_received() -> u32 {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        lattice_sys::signals::reload_signals_received()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        0
    }
}

/// How many stop signals have arrived since [`install_stop_signal_handlers`].
#[must_use]
pub fn stop_signals_received() -> u32 {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        lattice_sys::signals::stop_signals_received()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
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

    #[test]
    fn a_signal_during_startup_runs_the_stop_action_once() {
        let count = Arc::new(AtomicU32::new(0));
        let (done, fired) = mpsc::channel();
        let seen = Arc::clone(&count);
        let watch = watch_startup(
            move || seen.load(Ordering::SeqCst),
            Duration::from_secs(60),
            || {},
            move || {
                let _ = done.send(());
            },
        );
        count.fetch_add(1, Ordering::SeqCst);
        assert!(fired.recv_timeout(Duration::from_secs(10)).is_ok());
        assert!(fired.recv_timeout(Duration::from_millis(200)).is_err());
        // The watcher won, so finishing must report that it lost and on_signal ran exactly once.
        assert!(!watch.try_finish());
    }

    #[test]
    fn a_signal_after_startup_finished_is_left_to_the_normal_stop() {
        let count = Arc::new(AtomicU32::new(0));
        let (done, fired) = mpsc::channel();
        let seen = Arc::clone(&count);
        let watch = watch_startup(
            move || seen.load(Ordering::SeqCst),
            Duration::from_secs(60),
            || {},
            move || {
                let _ = done.send(());
            },
        );
        assert!(watch.try_finish());
        // The watcher polls every 20 ms; give it time to notice it was finished.
        thread::sleep(Duration::from_millis(300));
        count.fetch_add(1, Ordering::SeqCst);
        assert!(fired.recv_timeout(Duration::from_millis(500)).is_err());
    }

    #[test]
    fn a_slow_startup_is_announced_once() {
        let (slow, told) = mpsc::channel();
        let watch = watch_startup(
            || 0,
            Duration::from_millis(50),
            move || {
                let _ = slow.send(());
            },
            || {},
        );
        assert!(told.recv_timeout(Duration::from_secs(10)).is_ok());
        assert!(told.recv_timeout(Duration::from_millis(200)).is_err());
        drop(watch);
    }

    #[test]
    fn finishing_and_signalling_together_gives_exactly_one_winner() {
        for round in 0..80u32 {
            // The signal is already counted, so the watcher fires on its first iteration; the delay
            // before finishing sweeps from "finish first" to "watcher first".
            let (done, fired) = mpsc::channel();
            let watch = watch_startup(
                || 1,
                Duration::from_secs(60),
                || {},
                move || {
                    let _ = done.send(());
                },
            );
            let delay_us = match round {
                r if r.is_multiple_of(4) => 0,
                r if r.is_multiple_of(5) => 3000,
                r => u64::from(r % 20) * 10,
            };
            thread::sleep(Duration::from_micros(delay_us));
            if watch.try_finish() {
                // Finish won: the watcher sees the finished state and never runs its action.
                thread::sleep(Duration::from_millis(50));
                assert!(
                    fired.try_recv().is_err(),
                    "round {round}: on_signal ran after finish won"
                );
            } else {
                // The watcher won, so its action must run exactly once, however late it is scheduled.
                assert!(
                    fired.recv_timeout(Duration::from_secs(10)).is_ok(),
                    "round {round}: watcher won but on_signal did not run"
                );
                assert!(
                    fired.recv_timeout(Duration::from_millis(5)).is_err(),
                    "round {round}: on_signal ran twice"
                );
            }
        }
    }
}

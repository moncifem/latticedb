//! Server-side subscription state machine for the sync protocol v1 (`formats/sync-protocol-v1.md`).
//!
//! [`SubscriptionHost`] serves one connection and is sans-I/O: the caller decodes frames into
//! [`Message`]s (see [`crate::sync`]), passes them to [`SubscriptionHost::handle`] together with a
//! logical time and an [`EventSource`], writes every [`Output::Send`] it gets back, and calls
//! [`SubscriptionHost::poll`] now and then to push new events and to run the liveness timers. There
//! is no I/O, no thread, no real time, no randomness and no panic path.
//!
//! # What the host does
//!
//! * `Hello` / `Welcome`: the highest common version ([`crate::sync::negotiate`]); a refused range
//!   gets an `Error` with `sub` 0 and the connection is closed.
//! * `Subscribe(sub, scope, cursor)`: the first batch is read at once, so a scope the source does not
//!   serve ([`SourceError::UnknownScope`]) or a cursor it can no longer resume from
//!   ([`SourceError::TooOld`]) is answered with `invalid_scope` or `invalid_cursor` carrying `sub`.
//!   Otherwise the events strictly after `cursor` follow, then live events in the same order.
//!   Replay and live delivery are one code path: the host always asks the source for the events
//!   strictly after the last cursor it sent, so there is no seam where an event can be lost.
//! * `Ack(sub, cursor)`: advances the confirmed cursor. An `Ack` at or behind the confirmed cursor is
//!   a harmless duplicate; one beyond the last cursor sent is a protocol violation.
//! * `Ping` / `Pong`, idle and handshake timeouts on the logical clock.
//!
//! # Resume
//!
//! Delivery is at least once. A client that reconnects subscribes again with its last acknowledged
//! cursor; the host (a fresh one after a reconnect or a server restart) replays from there. Events the
//! client had processed but not yet acknowledged arrive again, and the client drops them because their
//! cursors are not greater than the last one it delivered. Nothing before the acknowledged cursor is
//! ever skipped, so the application sees every event once, in order. Row policy and scope filtering
//! belong to the [`EventSource`]: the host never sees an event the source did not allow for that scope.
//!
//! # Bounds
//!
//! Per connection the host keeps at most [`HostConfig::max_subscriptions`] subscriptions, each a scope
//! (at most 512 bytes) and two cursors (at most 64 bytes each); it never buffers events. One call to
//! `handle` or `poll` reads at most [`HostConfig::max_frames_per_pump`] frames of at most
//! [`HostConfig::batch_events`] events for each subscription. Flow control beyond `Ack` is out of
//! scope for protocol v1.

use crate::principal::Principal;
use crate::sync::{
    ErrorCode, Event, MAX_CURSOR_LEN, MAX_ERROR_MESSAGE_LEN, MAX_EVENTS_PER_FRAME, MAX_NAME_LEN,
    MAX_PAYLOAD, MAX_SCOPE_LEN, MIN_NEGOTIATED_PAYLOAD, Message, negotiate,
};
use lattice_core::bugs::{self, Bug};
use std::collections::BTreeMap;

/// Why an [`EventSource`] could not answer a read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceError {
    /// The source does not serve this scope (or the caller may not see it).
    UnknownScope,
    /// The cursor is older than the history the source still keeps.
    TooOld,
    /// The cursor is not one the source ever issued.
    BadCursor,
    /// The source failed for its own reasons.
    Unavailable,
}

/// Where the events come from. The host reads; it never writes.
///
/// The contract, which the host checks and answers with `internal` when broken:
///
/// * `read_after(principal, scope, after, limit)` returns at most `limit` events of `scope`, in delivery order,
///   whose cursors are all strictly greater than `after` (compared by length, then byte by byte, as
///   the client does), each cursor `1..=64` bytes and strictly increasing.
/// * `principal` is the identity the connection authenticated as, never the `agent` name of the
///   `Hello`. The source decides whether that principal may read `scope`, and applies its row policy
///   to the events it returns; a scope the principal may not read is answered exactly like a scope the
///   source does not serve ([`SourceError::UnknownScope`]), so a probe cannot tell the two apart.
/// * An empty `after` means "from the beginning"; a source that no longer holds the beginning answers
///   [`SourceError::TooOld`].
/// * It is a pure read: the same arguments give a prefix-consistent answer, and an event once returned
///   for a cursor position is never later skipped.
pub trait EventSource {
    /// The events of `scope` strictly after `after`, at most `limit` of them.
    fn read_after(
        &self,
        principal: &Principal,
        scope: &str,
        after: &[u8],
        limit: usize,
    ) -> Result<Vec<Event>, SourceError>;
}

/// Limits and names for one host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostConfig {
    /// Server name sent in `Welcome`, `1..=64` bytes, no control characters.
    pub server: String,
    /// `Welcome.max_payload`: the largest frame the server accepts, `1024..=1_048_576`.
    pub max_payload: u32,
    /// Most subscriptions on one connection, at least 1.
    pub max_subscriptions: usize,
    /// Most events in one `Events` frame, `1..=256`.
    pub batch_events: usize,
    /// Most `Events` frames per subscription per call, at least 1.
    pub max_frames_per_pump: usize,
    /// Idle ticks before the host sends a `Ping`.
    pub ping_interval: u64,
    /// Ticks to wait for a `Pong`, or for the `Hello`, before closing.
    pub ping_timeout: u64,
}

impl HostConfig {
    /// Defaults: 64 subscriptions, 128 events per frame, 4 frames per pump, ping after 30 idle ticks,
    /// 10 ticks to answer.
    pub fn new(server: &str) -> Self {
        HostConfig {
            server: server.to_owned(),
            max_payload: u32::try_from(MAX_PAYLOAD).unwrap_or(u32::MAX),
            max_subscriptions: 64,
            batch_events: 128,
            max_frames_per_pump: 4,
            ping_interval: 30,
            ping_timeout: 10,
        }
    }

    fn validate(&self) -> Result<(), &'static str> {
        if self.server.is_empty()
            || self.server.len() > MAX_NAME_LEN
            || self.server.chars().any(char::is_control)
        {
            return Err("server name");
        }
        if self.max_payload < MIN_NEGOTIATED_PAYLOAD
            || usize::try_from(self.max_payload).map_or(true, |n| n > MAX_PAYLOAD)
        {
            return Err("max_payload");
        }
        if self.max_subscriptions == 0 {
            return Err("max_subscriptions");
        }
        if self.batch_events == 0 || self.batch_events > MAX_EVENTS_PER_FRAME {
            return Err("batch_events");
        }
        if self.max_frames_per_pump == 0 {
            return Err("max_frames_per_pump");
        }
        Ok(())
    }
}

/// A configuration that cannot be used; the field is named.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConfigError(pub &'static str);

/// Why the host asks the caller to close the connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseReason {
    /// The `Hello` was refused (the `Error` was already sent).
    Refused,
    /// The client broke a protocol rule (a `malformed` error was already sent).
    ProtocolViolation(&'static str),
    /// The client sent a connection-level `Error`.
    ClientError,
    /// No `Hello` or no `Pong` in time.
    Timeout,
}

/// What the caller must do after a call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Output {
    /// Encode and write this message, in order.
    Send(Message),
    /// Close the connection after writing everything before this. The host ignores all later input.
    Close(CloseReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    AwaitHello,
    Ready,
    Closed,
}

#[derive(Debug)]
struct Sub {
    scope: String,
    /// Cursor of the last event sent; the next read starts strictly after it.
    sent: Vec<u8>,
    /// Cursor the client confirmed with `Ack`, never beyond `sent`.
    confirmed: Vec<u8>,
}

/// `a` is strictly after `b`: length first, then bytes, the order the client uses.
fn after(a: &[u8], b: &[u8]) -> bool {
    (a.len(), a) > (b.len(), b)
}

fn clip(text: &str) -> String {
    let mut s = String::new();
    for c in text.chars() {
        if s.len() + c.len_utf8() > MAX_ERROR_MESSAGE_LEN || c.is_control() {
            break;
        }
        s.push(c);
    }
    s
}

fn error(code: ErrorCode, sub: u32, text: &str) -> Output {
    Output::Send(Message::Error {
        code,
        sub,
        message: clip(text),
    })
}

/// Encoded size of an `Events` payload holding `events`.
fn events_payload_len<'a>(events: impl Iterator<Item = &'a Event>) -> usize {
    events.fold(6usize, |n, e| {
        n.saturating_add(6)
            .saturating_add(e.cursor.len())
            .saturating_add(e.data.len())
    })
}

/// The server half of one sync connection.
#[derive(Debug)]
pub struct SubscriptionHost {
    cfg: HostConfig,
    /// Fixed for the life of the connection. Passed to every source read.
    principal: Principal,
    phase: Phase,
    version: Option<u16>,
    subs: BTreeMap<u32, Sub>,
    last_rx: u64,
    ping: Option<(u64, u64)>,
    next_nonce: u64,
}

impl SubscriptionHost {
    /// A host for one new connection, waiting for `Hello`. `now` is the logical time of the accept.
    ///
    /// `principal` is the identity the transport authenticated (for example
    /// [`Session::principal`](crate::session::Session::principal)). The host never derives one from
    /// the `Hello`: its `agent` field is a free-text label and grants nothing.
    pub fn new(cfg: HostConfig, now: u64, principal: Principal) -> Result<Self, ConfigError> {
        cfg.validate().map_err(ConfigError)?;
        Ok(SubscriptionHost {
            cfg,
            principal,
            phase: Phase::AwaitHello,
            version: None,
            subs: BTreeMap::new(),
            last_rx: now,
            ping: None,
            next_nonce: 1,
        })
    }

    /// A host for a loopback development connection with the fixed
    /// [`LOCAL_DEVELOPMENT_NAME`](crate::principal::LOCAL_DEVELOPMENT_NAME) principal, which stands
    /// for "a local developer" and proves nothing about who is connected. For tests and benchmarks;
    /// a network transport must call [`SubscriptionHost::new`] with the authenticated principal.
    pub fn local_development(cfg: HostConfig, now: u64) -> Result<Self, ConfigError> {
        Self::new(cfg, now, Principal::local_development())
    }

    /// The principal every source read of this connection is made for.
    pub fn principal(&self) -> &Principal {
        &self.principal
    }

    /// The negotiated version, once `Welcome` was sent.
    pub fn version(&self) -> Option<u16> {
        self.version
    }

    /// True once the host asked for the connection to be closed.
    pub fn is_closed(&self) -> bool {
        self.phase == Phase::Closed
    }

    /// The active subscription ids, ascending.
    pub fn subscriptions(&self) -> Vec<u32> {
        self.subs.keys().copied().collect()
    }

    /// The confirmed (acknowledged) cursor of `sub`; empty until the first `Ack` (or the cursor the
    /// subscription started from).
    pub fn confirmed(&self, sub: u32) -> Option<&[u8]> {
        self.subs.get(&sub).map(|s| s.confirmed.as_slice())
    }

    /// The cursor of the last event sent on `sub`.
    pub fn sent(&self, sub: u32) -> Option<&[u8]> {
        self.subs.get(&sub).map(|s| s.sent.as_slice())
    }

    fn violation(&mut self, rule: &'static str) -> Vec<Output> {
        self.phase = Phase::Closed;
        self.ping = None;
        vec![
            error(ErrorCode::Malformed, 0, rule),
            Output::Close(CloseReason::ProtocolViolation(rule)),
        ]
    }

    /// Feed one decoded message from the client.
    pub fn handle<S: EventSource>(&mut self, msg: Message, now: u64, src: &S) -> Vec<Output> {
        if self.phase == Phase::Closed {
            return Vec::new();
        }
        self.last_rx = now;
        match msg {
            Message::Hello {
                min_version,
                max_version,
                ..
            } => self.on_hello(min_version, max_version),
            _ if self.phase == Phase::AwaitHello => self.violation("hello expected first"),
            Message::Subscribe { sub, scope, cursor } => self.on_subscribe(sub, scope, cursor, src),
            Message::Ack { sub, cursor } => self.on_ack(sub, &cursor),
            Message::Error { sub, .. } => {
                if sub == 0 {
                    self.phase = Phase::Closed;
                    self.ping = None;
                    vec![Output::Close(CloseReason::ClientError)]
                } else {
                    self.subs.remove(&sub);
                    Vec::new()
                }
            }
            Message::Ping { nonce } => vec![Output::Send(Message::Pong { nonce })],
            Message::Pong { nonce } => {
                if matches!(self.ping, Some((n, _)) if n == nonce) {
                    self.ping = None;
                }
                Vec::new()
            }
            Message::Welcome { .. } | Message::Events { .. } => {
                self.violation("server-to-client frame received")
            }
        }
    }

    fn on_hello(&mut self, min: u16, max: u16) -> Vec<Output> {
        if self.phase != Phase::AwaitHello {
            return self.violation("second hello");
        }
        match negotiate(min, max) {
            Ok(version) => {
                self.phase = Phase::Ready;
                self.version = Some(version);
                vec![Output::Send(Message::Welcome {
                    version,
                    max_payload: self.cfg.max_payload,
                    server: self.cfg.server.clone(),
                })]
            }
            Err(code) => {
                self.phase = Phase::Closed;
                vec![
                    error(code, 0, "version range refused"),
                    Output::Close(CloseReason::Refused),
                ]
            }
        }
    }

    fn on_subscribe<S: EventSource>(
        &mut self,
        sub: u32,
        scope: String,
        cursor: Vec<u8>,
        src: &S,
    ) -> Vec<Output> {
        if sub == 0 {
            return self.violation("subscription id 0");
        }
        if self.subs.contains_key(&sub) {
            return self.violation("duplicate subscription id");
        }
        if scope.is_empty() || scope.len() > MAX_SCOPE_LEN || scope.chars().any(char::is_control) {
            return vec![error(ErrorCode::InvalidScope, sub, "invalid scope")];
        }
        if cursor.len() > MAX_CURSOR_LEN {
            return vec![error(ErrorCode::InvalidCursor, sub, "invalid cursor")];
        }
        if self.subs.len() >= self.cfg.max_subscriptions {
            return vec![error(
                ErrorCode::TooManySubscriptions,
                sub,
                "subscription limit reached",
            )];
        }
        let mut state = Sub {
            scope,
            sent: cursor.clone(),
            confirmed: cursor,
        };
        let mut out = Vec::new();
        match pump(sub, &mut state, src, &self.principal, &self.cfg, &mut out) {
            Ok(()) => {
                self.subs.insert(sub, state);
            }
            Err(e) => out.push(e),
        }
        out
    }

    fn on_ack(&mut self, sub: u32, cursor: &[u8]) -> Vec<Output> {
        if cursor.is_empty() || cursor.len() > MAX_CURSOR_LEN {
            return self.violation("ack cursor out of bounds");
        }
        let Some(s) = self.subs.get_mut(&sub) else {
            return vec![error(
                ErrorCode::UnknownSubscription,
                sub,
                "unknown subscription",
            )];
        };
        if after(cursor, &s.sent) {
            return self.violation("ack beyond the last event sent");
        }
        if after(cursor, &s.confirmed) {
            s.confirmed = cursor.to_vec();
        }
        Vec::new()
    }

    /// Advance the logical clock and push new events. Call it whenever the source may have new events
    /// and at least once per tick.
    pub fn poll<S: EventSource>(&mut self, now: u64, src: &S) -> Vec<Output> {
        let mut out = Vec::new();
        match self.phase {
            Phase::Closed => return out,
            Phase::AwaitHello => {
                if now.saturating_sub(self.last_rx) >= self.cfg.ping_timeout {
                    self.phase = Phase::Closed;
                    out.push(Output::Close(CloseReason::Timeout));
                }
                return out;
            }
            Phase::Ready => {}
        }
        let ids: Vec<u32> = self.subs.keys().copied().collect();
        for id in ids {
            let res = match self.subs.get_mut(&id) {
                Some(s) => pump(id, s, src, &self.principal, &self.cfg, &mut out),
                None => continue,
            };
            if let Err(e) = res {
                self.subs.remove(&id);
                out.push(e);
            }
        }
        match self.ping {
            Some((_, sent)) => {
                if now.saturating_sub(sent) >= self.cfg.ping_timeout {
                    self.phase = Phase::Closed;
                    self.ping = None;
                    out.push(Output::Close(CloseReason::Timeout));
                }
            }
            None => {
                if now.saturating_sub(self.last_rx) >= self.cfg.ping_interval {
                    let nonce = self.next_nonce;
                    self.next_nonce = nonce.wrapping_add(1);
                    self.ping = Some((nonce, now));
                    out.push(Output::Send(Message::Ping { nonce }));
                }
            }
        }
        out
    }
}

/// Send the events strictly after `st.sent`, up to the per-call bound. On `Err` the subscription is
/// finished and the returned output is its `Error`.
fn pump<S: EventSource>(
    sub: u32,
    st: &mut Sub,
    src: &S,
    principal: &Principal,
    cfg: &HostConfig,
    out: &mut Vec<Output>,
) -> Result<(), Output> {
    // Planted defect: the read is made for a different identity than the connection's.
    let dev;
    let principal = if bugs::active(Bug::SubscribePrincipalDropped) {
        dev = Principal::local_development();
        &dev
    } else {
        principal
    };
    for _ in 0..cfg.max_frames_per_pump {
        let events = match src.read_after(principal, &st.scope, &st.sent, cfg.batch_events) {
            Ok(e) => e,
            Err(SourceError::UnknownScope) => {
                return Err(error(ErrorCode::InvalidScope, sub, "scope not served"));
            }
            Err(SourceError::TooOld) => {
                return Err(error(ErrorCode::InvalidCursor, sub, "cursor too old"));
            }
            Err(SourceError::BadCursor) => {
                return Err(error(
                    ErrorCode::InvalidCursor,
                    sub,
                    "cursor not recognised",
                ));
            }
            Err(SourceError::Unavailable) => {
                return Err(error(ErrorCode::Internal, sub, "event source unavailable"));
            }
        };
        if events.is_empty() {
            return Ok(());
        }
        let returned = events.len();
        if returned > cfg.batch_events {
            return Err(error(ErrorCode::Internal, sub, "source returned too many"));
        }
        // The source contract, checked: ordered, bounded, strictly after what was already sent.
        let mut prev: &[u8] = &st.sent;
        for e in &events {
            if e.cursor.is_empty() || e.cursor.len() > MAX_CURSOR_LEN || !after(&e.cursor, prev) {
                return Err(error(ErrorCode::Internal, sub, "source order violation"));
            }
            prev = &e.cursor;
        }
        // Take as many as fit in one frame; the rest is read again by the next iteration or call.
        let mut take = 0usize;
        for n in 1..=returned {
            if events_payload_len(events.iter().take(n)) > MAX_PAYLOAD {
                break;
            }
            take = n;
        }
        if take == 0 {
            return Err(error(ErrorCode::Internal, sub, "event too large"));
        }
        let mut batch = events;
        batch.truncate(take);
        if let Some(last) = batch.last() {
            st.sent.clone_from(&last.cursor);
        }
        out.push(Output::Send(Message::Events { sub, events: batch }));
        if take == returned && returned < cfg.batch_events {
            return Ok(());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )]
    use super::*;
    use crate::sync::{decode, encode};
    use proptest::prelude::*;
    use std::collections::VecDeque;

    // ----- a log source: global sequence numbers, cursor = 8-byte big-endian sequence ------------

    #[derive(Clone, Default)]
    struct Log {
        /// (seq, scope, data), seq strictly increasing from 1.
        events: Vec<(u64, String, Vec<u8>)>,
        /// Cursors below this are too old; 0 keeps everything.
        floor: u64,
    }

    impl Log {
        fn append(&mut self, scope: &str) -> u64 {
            let seq = self.events.last().map_or(1, |e| e.0 + 1);
            self.events
                .push((seq, scope.to_owned(), seq.to_be_bytes().to_vec()));
            seq
        }
        fn last(&self) -> u64 {
            self.events.last().map_or(0, |e| e.0)
        }
    }

    fn cur(n: u64) -> Vec<u8> {
        n.to_be_bytes().to_vec()
    }

    impl EventSource for Log {
        fn read_after(
            &self,
            _: &Principal,
            scope: &str,
            after: &[u8],
            limit: usize,
        ) -> Result<Vec<Event>, SourceError> {
            if !scope.starts_with("ws/") {
                return Err(SourceError::UnknownScope);
            }
            let from = if after.is_empty() {
                if self.floor > 0 {
                    return Err(SourceError::TooOld);
                }
                0
            } else {
                let b: [u8; 8] = after.try_into().map_err(|_| SourceError::BadCursor)?;
                let v = u64::from_be_bytes(b);
                if v > self.last() {
                    return Err(SourceError::BadCursor);
                }
                if v < self.floor {
                    return Err(SourceError::TooOld);
                }
                v
            };
            Ok(self
                .events
                .iter()
                .filter(|e| e.0 > from && e.1 == scope)
                .take(limit)
                .map(|e| Event {
                    cursor: cur(e.0),
                    data: e.2.clone(),
                })
                .collect())
        }
    }

    fn host() -> SubscriptionHost {
        SubscriptionHost::local_development(HostConfig::new("lattice"), 0).unwrap()
    }

    fn hello(min: u16, max: u16) -> Message {
        Message::Hello {
            min_version: min,
            max_version: max,
            agent: "t".into(),
        }
    }

    fn sub(id: u32, scope: &str, cursor: Vec<u8>) -> Message {
        Message::Subscribe {
            sub: id,
            scope: scope.into(),
            cursor,
        }
    }

    fn ready(h: &mut SubscriptionHost, log: &Log) {
        let out = h.handle(hello(1, 1), 0, log);
        assert!(matches!(
            out.as_slice(),
            [Output::Send(Message::Welcome { version: 1, .. })]
        ));
    }

    fn seqs(out: &[Output], want_sub: u32) -> Vec<u64> {
        let mut v = Vec::new();
        for o in out {
            if let Output::Send(Message::Events { sub, events }) = o
                && *sub == want_sub
            {
                for e in events {
                    v.push(u64::from_be_bytes(e.cursor.as_slice().try_into().unwrap()));
                }
            }
        }
        v
    }

    fn err_of(out: &[Output]) -> Option<(ErrorCode, u32)> {
        out.iter().find_map(|o| match o {
            Output::Send(Message::Error { code, sub, .. }) => Some((*code, *sub)),
            _ => None,
        })
    }

    // ----- scripted scenarios ----------------------------------------------------------------------

    #[test]
    fn negotiation_table() {
        let log = Log::default();
        let mut h = host();
        let out = h.handle(hello(1, 3), 0, &log);
        assert_eq!(
            out,
            vec![Output::Send(Message::Welcome {
                version: 1,
                max_payload: 1 << 20,
                server: "lattice".into()
            })]
        );
        assert_eq!(h.version(), Some(1));
        for (min, max, code) in [
            (2, 3, ErrorCode::UnsupportedVersion),
            (0, 1, ErrorCode::Malformed),
            (2, 1, ErrorCode::Malformed),
        ] {
            let mut h = host();
            let out = h.handle(hello(min, max), 0, &log);
            assert_eq!(err_of(&out), Some((code, 0)));
            assert_eq!(out.last(), Some(&Output::Close(CloseReason::Refused)));
            assert!(h.is_closed());
            assert!(h.handle(hello(1, 1), 1, &log).is_empty());
        }
    }

    #[test]
    fn frames_before_hello_or_wrong_direction_are_violations() {
        let log = Log::default();
        let mut h = host();
        let out = h.handle(sub(1, "ws/a", vec![]), 0, &log);
        assert_eq!(err_of(&out), Some((ErrorCode::Malformed, 0)));
        assert!(matches!(
            out.last(),
            Some(Output::Close(CloseReason::ProtocolViolation(_)))
        ));
        for msg in [
            Message::Welcome {
                version: 1,
                max_payload: 1024,
                server: "x".into(),
            },
            Message::Events {
                sub: 1,
                events: vec![],
            },
            hello(1, 1),
        ] {
            let mut h = host();
            ready(&mut h, &log);
            let out = h.handle(msg, 1, &log);
            assert!(h.is_closed(), "{out:?}");
        }
    }

    #[test]
    fn replay_then_live_in_order() {
        let mut log = Log::default();
        for _ in 0..3 {
            log.append("ws/a");
            log.append("ws/b");
        }
        let mut h = host();
        ready(&mut h, &log);
        let out = h.handle(sub(1, "ws/a", vec![]), 1, &log);
        assert_eq!(seqs(&out, 1), vec![1, 3, 5]);
        assert!(h.poll(2, &log).is_empty());
        log.append("ws/a");
        log.append("ws/b");
        let out = h.poll(3, &log);
        assert_eq!(seqs(&out, 1), vec![7]);
        assert_eq!(h.sent(1), Some(cur(7).as_slice()));
    }

    #[test]
    fn resume_from_cursor_is_strictly_after() {
        let mut log = Log::default();
        for _ in 0..5 {
            log.append("ws/a");
        }
        let mut h = host();
        ready(&mut h, &log);
        let out = h.handle(sub(9, "ws/a", cur(2)), 1, &log);
        assert_eq!(seqs(&out, 9), vec![3, 4, 5]);
        assert_eq!(h.confirmed(9), Some(cur(2).as_slice()));
    }

    #[test]
    fn ack_advances_confirmed_and_duplicates_are_harmless() {
        let mut log = Log::default();
        for _ in 0..4 {
            log.append("ws/a");
        }
        let mut h = host();
        ready(&mut h, &log);
        h.handle(sub(1, "ws/a", vec![]), 1, &log);
        assert!(
            h.handle(
                Message::Ack {
                    sub: 1,
                    cursor: cur(3)
                },
                2,
                &log
            )
            .is_empty()
        );
        assert_eq!(h.confirmed(1), Some(cur(3).as_slice()));
        h.handle(
            Message::Ack {
                sub: 1,
                cursor: cur(2),
            },
            3,
            &log,
        );
        assert_eq!(h.confirmed(1), Some(cur(3).as_slice()));
        // Beyond what was sent: violation.
        let out = h.handle(
            Message::Ack {
                sub: 1,
                cursor: cur(5),
            },
            4,
            &log,
        );
        assert!(h.is_closed(), "{out:?}");
    }

    #[test]
    fn ack_for_unknown_subscription_is_not_fatal() {
        let log = Log::default();
        let mut h = host();
        ready(&mut h, &log);
        let out = h.handle(
            Message::Ack {
                sub: 7,
                cursor: cur(1),
            },
            1,
            &log,
        );
        assert_eq!(err_of(&out), Some((ErrorCode::UnknownSubscription, 7)));
        assert!(!h.is_closed());
    }

    #[test]
    fn subscribe_errors_are_per_subscription() {
        let mut log = Log::default();
        log.append("ws/a");
        log.append("ws/a");
        log.floor = 1;
        let mut h = host();
        ready(&mut h, &log);
        let out = h.handle(sub(1, "other", vec![]), 1, &log);
        assert_eq!(err_of(&out), Some((ErrorCode::InvalidScope, 1)));
        // Cursor too old, including "from the beginning" once history was trimmed.
        let out = h.handle(sub(2, "ws/a", vec![]), 1, &log);
        assert_eq!(err_of(&out), Some((ErrorCode::InvalidCursor, 2)));
        let out = h.handle(sub(3, "ws/a", cur(0)), 1, &log);
        assert_eq!(err_of(&out), Some((ErrorCode::InvalidCursor, 3)));
        // A cursor the source never issued.
        let out = h.handle(sub(4, "ws/a", vec![1, 2, 3]), 1, &log);
        assert_eq!(err_of(&out), Some((ErrorCode::InvalidCursor, 4)));
        // Control characters in a scope.
        let out = h.handle(sub(5, "ws/\u{1}", vec![]), 1, &log);
        assert_eq!(err_of(&out), Some((ErrorCode::InvalidScope, 5)));
        assert!(h.subscriptions().is_empty());
        assert!(!h.is_closed());
        // At the floor is fine.
        let out = h.handle(sub(6, "ws/a", cur(1)), 1, &log);
        assert_eq!(seqs(&out, 6), vec![2]);
    }

    #[test]
    fn cursor_overtaken_by_retention_ends_only_that_subscription() {
        let mut log = Log::default();
        for _ in 0..3 {
            log.append("ws/a");
        }
        let mut h = host();
        ready(&mut h, &log);
        h.handle(sub(1, "ws/a", vec![]), 1, &log);
        h.handle(sub(2, "ws/a", cur(3)), 1, &log);
        for _ in 0..3 {
            log.append("ws/a");
        }
        log.floor = 5; // sub 1 sent up to 3: now too old; sub 2 is also at 3.
        let out = h.poll(2, &log);
        assert_eq!(
            out.iter()
                .filter(|o| matches!(
                    o,
                    Output::Send(Message::Error {
                        code: ErrorCode::InvalidCursor,
                        ..
                    })
                ))
                .count(),
            2
        );
        assert!(h.subscriptions().is_empty());
        assert!(!h.is_closed());
    }

    #[test]
    fn subscription_limit_and_duplicate_id() {
        let log = Log::default();
        let mut cfg = HostConfig::new("lattice");
        cfg.max_subscriptions = 2;
        let mut h = SubscriptionHost::local_development(cfg, 0).unwrap();
        ready(&mut h, &log);
        h.handle(sub(1, "ws/a", vec![]), 1, &log);
        h.handle(sub(2, "ws/a", vec![]), 1, &log);
        let out = h.handle(sub(3, "ws/a", vec![]), 1, &log);
        assert_eq!(err_of(&out), Some((ErrorCode::TooManySubscriptions, 3)));
        assert_eq!(h.subscriptions(), vec![1, 2]);
        h.handle(sub(1, "ws/a", vec![]), 2, &log);
        assert!(h.is_closed());
    }

    #[test]
    fn client_error_for_a_sub_drops_it_and_connection_error_closes() {
        let log = Log::default();
        let mut h = host();
        ready(&mut h, &log);
        h.handle(sub(1, "ws/a", vec![]), 1, &log);
        let e = |sub| Message::Error {
            code: ErrorCode::Internal,
            sub,
            message: String::new(),
        };
        assert!(h.handle(e(1), 2, &log).is_empty());
        assert!(h.subscriptions().is_empty());
        assert_eq!(
            h.handle(e(0), 3, &log),
            vec![Output::Close(CloseReason::ClientError)]
        );
    }

    #[test]
    fn batches_of_at_most_the_configured_size_and_bounded_per_call() {
        let mut log = Log::default();
        for _ in 0..1000 {
            log.append("ws/a");
        }
        let mut cfg = HostConfig::new("lattice");
        cfg.batch_events = 100;
        cfg.max_frames_per_pump = 3;
        let mut h = SubscriptionHost::local_development(cfg, 0).unwrap();
        ready(&mut h, &log);
        let out = h.handle(sub(1, "ws/a", vec![]), 1, &log);
        assert_eq!(out.len(), 3);
        assert_eq!(seqs(&out, 1).len(), 300);
        let mut all = seqs(&out, 1);
        for t in 2..20 {
            all.extend(seqs(&h.poll(t, &log), 1));
        }
        assert_eq!(all, (1..=1000).collect::<Vec<_>>());
    }

    #[test]
    fn oversized_event_ends_the_subscription_not_the_connection() {
        struct Big;
        impl EventSource for Big {
            fn read_after(
                &self,
                _: &Principal,
                _: &str,
                _: &[u8],
                _: usize,
            ) -> Result<Vec<Event>, SourceError> {
                Ok(vec![Event {
                    cursor: cur(1),
                    data: vec![0; MAX_PAYLOAD],
                }])
            }
        }
        let mut h = host();
        ready(&mut h, &Log::default());
        let out = h.handle(sub(1, "ws/a", vec![]), 1, &Big);
        assert_eq!(err_of(&out), Some((ErrorCode::Internal, 1)));
        assert!(!h.is_closed());
    }

    #[test]
    fn frames_are_split_to_fit_the_payload_limit() {
        struct Fat;
        impl EventSource for Fat {
            fn read_after(
                &self,
                _: &Principal,
                _: &str,
                a: &[u8],
                l: usize,
            ) -> Result<Vec<Event>, SourceError> {
                let from = a.try_into().map(u64::from_be_bytes).unwrap_or(0);
                Ok((from + 1..=6)
                    .take(l)
                    .map(|n| Event {
                        cursor: cur(n),
                        data: vec![0; 400_000],
                    })
                    .collect())
            }
        }
        let mut h = host();
        ready(&mut h, &Log::default());
        let out = h.handle(sub(1, "ws/a", vec![]), 1, &Fat);
        let mut got = Vec::new();
        for o in out.iter().chain(h.poll(2, &Fat).iter()) {
            if let Output::Send(m @ Message::Events { .. }) = o {
                let mut buf = Vec::new();
                encode(m, &mut buf).unwrap();
                got.push(buf.len());
            }
        }
        assert!(got.iter().all(|n| *n <= MAX_PAYLOAD + 5));
        assert_eq!(got.len(), 3); // two 400 kB events per frame
    }

    #[test]
    fn ping_pong_and_timers_on_the_logical_clock() {
        let log = Log::default();
        let mut h = host();
        ready(&mut h, &log);
        assert_eq!(
            h.handle(Message::Ping { nonce: 5 }, 1, &log),
            vec![Output::Send(Message::Pong { nonce: 5 })]
        );
        assert!(h.poll(10, &log).is_empty());
        let out = h.poll(31, &log);
        assert_eq!(out, vec![Output::Send(Message::Ping { nonce: 1 })]);
        // A wrong nonce does not clear it; the right one does.
        h.handle(Message::Pong { nonce: 9 }, 32, &log);
        assert_eq!(h.poll(41, &log), vec![Output::Close(CloseReason::Timeout)]);
        let mut h = host();
        ready(&mut h, &log);
        h.poll(31, &log);
        h.handle(Message::Pong { nonce: 1 }, 32, &log);
        assert!(h.poll(60, &log).is_empty());
        // No hello in time.
        let mut h = host();
        assert!(h.poll(9, &log).is_empty());
        assert_eq!(h.poll(10, &log), vec![Output::Close(CloseReason::Timeout)]);
        assert!(h.poll(11, &log).is_empty());
    }

    #[test]
    fn config_is_validated() {
        let mut c = HostConfig::new("");
        assert_eq!(
            SubscriptionHost::local_development(c.clone(), 0)
                .unwrap_err()
                .0,
            "server name"
        );
        c.server = "s".into();
        c.batch_events = 257;
        assert!(SubscriptionHost::local_development(c.clone(), 0).is_err());
        c.batch_events = 1;
        c.max_payload = 10;
        assert!(SubscriptionHost::local_development(c.clone(), 0).is_err());
        c.max_payload = 2048;
        c.max_subscriptions = 0;
        assert!(SubscriptionHost::local_development(c.clone(), 0).is_err());
        c.max_subscriptions = 1;
        assert!(SubscriptionHost::local_development(c, 0).is_ok());
    }

    #[test]
    fn broken_sources_are_caught() {
        struct Dup;
        impl EventSource for Dup {
            fn read_after(
                &self,
                _: &Principal,
                _: &str,
                a: &[u8],
                _: usize,
            ) -> Result<Vec<Event>, SourceError> {
                // Inclusive of the cursor: the classic off-by-one.
                let n = a.try_into().map(u64::from_be_bytes).unwrap_or(1).max(1);
                Ok(vec![Event {
                    cursor: cur(n),
                    data: vec![],
                }])
            }
        }
        let mut h = host();
        ready(&mut h, &Log::default());
        h.handle(sub(1, "ws/a", cur(1)), 1, &Dup);
        // Resuming from 1 returns 1 again: refused, not delivered.
        assert!(h.subscriptions().is_empty());
        let mut h = host();
        ready(&mut h, &Log::default());
        let out = h.handle(sub(1, "ws/a", cur(1)), 1, &Dup);
        assert_eq!(err_of(&out), Some((ErrorCode::Internal, 1)));
    }

    // ----- simulation: host + a client that follows the SDK rules ---------------------------------

    /// The rules of `lattice_sdk::client::SyncClient` that matter for resume, mirrored here because
    /// `layers.toml` does not let `lattice-api` depend on `lattice-sdk`, even in tests: resubscribe
    /// with the last acknowledged cursor, drop events whose cursor is not strictly after the last
    /// delivered one, acknowledge only what was delivered.
    struct MiniClient {
        scope: String,
        delivered: Vec<u8>,
        acked: Vec<u8>,
        seen: Vec<u64>,
        failed: bool,
    }

    impl MiniClient {
        fn on_connect(&self) -> Message {
            hello(1, 1)
        }
        fn on_message(&mut self, m: Message) -> Vec<Message> {
            match m {
                Message::Welcome { .. } if !self.failed => {
                    vec![sub(1, &self.scope.clone(), self.acked.clone())]
                }
                Message::Events { sub: 1, events } => {
                    for e in events {
                        if after(&e.cursor, &self.delivered) {
                            self.delivered.clone_from(&e.cursor);
                            self.seen
                                .push(u64::from_be_bytes(e.cursor.as_slice().try_into().unwrap()));
                        }
                    }
                    Vec::new()
                }
                Message::Error { sub: 1, .. } => {
                    self.failed = true;
                    Vec::new()
                }
                Message::Ping { nonce } => vec![Message::Pong { nonce }],
                _ => Vec::new(),
            }
        }
        fn ack(&mut self) -> Option<Message> {
            if self.delivered.is_empty() || self.failed {
                return None;
            }
            self.acked.clone_from(&self.delivered);
            Some(Message::Ack {
                sub: 1,
                cursor: self.delivered.clone(),
            })
        }
    }

    struct World<S: EventSource> {
        src: S,
        host: SubscriptionHost,
        client: MiniClient,
        up: VecDeque<Message>,
        down: VecDeque<Message>,
        now: u64,
        /// The oracle: scope-filtered sequence numbers appended, kept independently of the source.
        expected: Vec<u64>,
        next_seq: u64,
        last_events: Option<Message>,
    }

    /// Run a message through the real codec so the simulation exercises the wire format too.
    fn wire(m: &Message) -> Message {
        let mut buf = Vec::new();
        encode(m, &mut buf).unwrap();
        let (back, used) = decode(&buf).unwrap().unwrap();
        assert_eq!(used, buf.len());
        back
    }

    impl<S: EventSource> World<S> {
        fn new(src: S) -> Self {
            let mut w = World {
                src,
                host: host(),
                client: MiniClient {
                    scope: "ws/a".into(),
                    delivered: vec![],
                    acked: vec![],
                    seen: vec![],
                    failed: false,
                },
                up: VecDeque::new(),
                down: VecDeque::new(),
                now: 0,
                expected: vec![],
                next_seq: 1,
                last_events: None,
            };
            w.up.push_back(w.client.on_connect());
            w
        }

        fn flush(&mut self) {
            for _ in 0..1000 {
                if self.up.is_empty() && self.down.is_empty() {
                    return;
                }
                while let Some(m) = self.up.pop_front() {
                    for o in self.host.handle(wire(&m), self.now, &self.src) {
                        if let Output::Send(m) = o {
                            self.down.push_back(m);
                        }
                    }
                }
                while let Some(m) = self.down.pop_front() {
                    if matches!(m, Message::Events { .. }) {
                        self.last_events = Some(m.clone());
                    }
                    for r in self.client.on_message(wire(&m)) {
                        self.up.push_back(r);
                    }
                }
            }
            panic!("flush did not settle");
        }

        fn poll(&mut self) {
            self.now += 1;
            for o in self.host.poll(self.now, &self.src) {
                if let Output::Send(m) = o {
                    self.down.push_back(m);
                }
            }
            self.flush();
        }

        /// A new connection to a fresh host: a disconnect or a server restart. Frames in flight
        /// are lost; the client resumes with its last acknowledged cursor.
        fn reconnect(&mut self) {
            self.up.clear();
            self.down.clear();
            self.now += 1;
            self.host =
                SubscriptionHost::local_development(HostConfig::new("lattice"), self.now).unwrap();
            self.up.push_back(self.client.on_connect());
            self.flush();
        }
    }

    /// Append to the log through a closure, tracking the oracle. `append` receives (seq, scope).
    fn run<S: EventSource>(
        w: &mut World<S>,
        ops: &[u8],
        mut append: impl FnMut(&mut S, &str),
    ) -> Result<(), String> {
        w.flush();
        for &op in ops {
            match op % 8 {
                0 | 1 => {
                    let scope = if op % 2 == 0 { "ws/a" } else { "ws/b" };
                    append(&mut w.src, scope);
                    if scope == "ws/a" {
                        w.expected.push(w.next_seq);
                    }
                    w.next_seq += 1;
                }
                2 | 3 => w.poll(),
                4 => {
                    if let Some(a) = w.client.ack() {
                        w.up.push_back(a);
                        w.flush();
                    }
                }
                5 => {
                    // The ack is lost on the way: the client believes it, the server never hears it.
                    let _ = w.client.ack();
                }
                6 => w.reconnect(),
                _ => {
                    // The network delivers the last Events frame a second time.
                    if let Some(m) = w.last_events.clone() {
                        w.down.push_back(m);
                        w.flush();
                    }
                }
            }
        }
        // Quiesce, then compare with the independent oracle.
        w.reconnect();
        for _ in 0..50 {
            w.poll();
        }
        if w.client.failed {
            return Err("subscription failed".into());
        }
        if w.client.seen != w.expected {
            return Err(format!(
                "gap or duplicate: saw {:?}, expected {:?}",
                w.client.seen, w.expected
            ));
        }
        Ok(())
    }

    fn append_log(l: &mut Log, scope: &str) {
        l.append(scope);
    }

    proptest! {
        #[test]
        fn random_disconnects_restarts_and_duplicates_never_gap_or_duplicate(
            ops in proptest::collection::vec(any::<u8>(), 0..120)
        ) {
            let mut w = World::new(Log::default());
            prop_assert_eq!(run(&mut w, &ops, append_log), Ok(()));
        }

        #[test]
        fn arbitrary_client_messages_never_panic_and_stay_bounded(
            ops in proptest::collection::vec((any::<u8>(), 0u32..6, 0u64..12), 0..150)
        ) {
            let mut log = Log::default();
            for _ in 0..8 { log.append("ws/a"); }
            let mut h = host();
            let mut now = 0;
            for (k, id, n) in ops {
                now += u64::from(k % 5);
                let m = match k % 9 {
                    0 => hello(1, 1 + u16::from(k % 3)),
                    1 => sub(id, "ws/a", if n == 0 { vec![] } else { cur(n) }),
                    2 => Message::Ack { sub: id, cursor: cur(n.max(1)) },
                    3 => Message::Ping { nonce: n },
                    4 => Message::Pong { nonce: n },
                    5 => Message::Error { code: ErrorCode::Internal, sub: id, message: String::new() },
                    6 => Message::Events { sub: id, events: vec![] },
                    7 => sub(id, "nope", vec![]),
                    _ => Message::Ack { sub: id, cursor: vec![] },
                };
                let a = h.handle(m, now, &log);
                let b = h.poll(now, &log);
                prop_assert!(a.len() + b.len() < 100);
                prop_assert!(h.subscriptions().len() <= 64);
            }
        }
    }

    // ----- the check can fail ----------------------------------------------------------------------

    /// A source with an off-by-one on resume: it skips the event right after the cursor.
    struct SkipsFirst(Log);
    impl EventSource for SkipsFirst {
        fn read_after(
            &self,
            p: &Principal,
            s: &str,
            a: &[u8],
            l: usize,
        ) -> Result<Vec<Event>, SourceError> {
            let mut v = self.0.read_after(p, s, a, l.saturating_add(1))?;
            if !a.is_empty() && !v.is_empty() {
                v.remove(0);
            }
            v.truncate(l);
            Ok(v)
        }
    }

    /// A source that reads from the cursor itself instead of strictly after it.
    struct Inclusive(Log);
    impl EventSource for Inclusive {
        fn read_after(
            &self,
            p: &Principal,
            s: &str,
            a: &[u8],
            l: usize,
        ) -> Result<Vec<Event>, SourceError> {
            // Starts at the cursor itself instead of strictly after it.
            let b: Option<[u8; 8]> = a.try_into().ok();
            let from = b.map_or(0, |b| u64::from_be_bytes(b).saturating_sub(1));
            self.0.read_after(p, s, &cur(from), l)
        }
    }

    #[test]
    fn a_source_that_skips_an_event_makes_the_check_fail() {
        let mut w = World::new(SkipsFirst(Log::default()));
        let ops = [0u8, 0, 0, 2, 4, 0, 0, 2];
        let res = run(&mut w, &ops, |s, scope| {
            s.0.append(scope);
        });
        assert!(res.is_err(), "the oracle must notice the gap: {res:?}");
    }

    #[test]
    fn a_source_that_repeats_the_cursor_event_makes_the_check_fail() {
        let mut w = World::new(Inclusive(Log::default()));
        let ops = [0u8, 0, 0, 2, 4, 0, 2, 2];
        let res = run(&mut w, &ops, |s, scope| {
            s.0.append(scope);
        });
        assert!(res.is_err(), "{res:?}");
    }

    #[test]
    fn honest_source_passes_the_same_scripts() {
        for ops in [
            vec![0u8, 0, 0, 2, 4, 0, 0, 2],
            vec![0, 0, 0, 2, 4, 0, 2, 2],
            vec![0, 2, 0, 5, 6, 0, 7, 2, 6, 6, 1, 4, 6],
        ] {
            let mut w = World::new(Log::default());
            assert_eq!(run(&mut w, &ops, append_log), Ok(()), "{ops:?}");
        }
    }

    // ----- principal binding (threat model G-03): reads are made for the authenticated identity ----

    use std::cell::RefCell;

    /// Serves `ws/shared` to everyone and `ws/private` to the account `alice` only, and records the
    /// principal of every read. The expectations are written down here, not taken from the host.
    struct Acl {
        log: Log,
        seen: RefCell<Vec<String>>,
    }

    impl EventSource for Acl {
        fn read_after(
            &self,
            principal: &Principal,
            scope: &str,
            after: &[u8],
            limit: usize,
        ) -> Result<Vec<Event>, SourceError> {
            self.seen.borrow_mut().push(principal.to_string());
            if scope == "ws/private" && principal.to_string() != "account:alice" {
                return Err(SourceError::UnknownScope);
            }
            self.log.read_after(principal, scope, after, limit)
        }
    }

    fn acl() -> Acl {
        let mut log = Log::default();
        log.append("ws/private");
        log.append("ws/shared");
        Acl {
            log,
            seen: RefCell::new(Vec::new()),
        }
    }

    fn host_for(name: &str) -> SubscriptionHost {
        SubscriptionHost::new(HostConfig::new("lattice"), 0, Principal::account(name)).unwrap()
    }

    /// Runs the private-scope subscription for `name` and reports what the connection saw: the
    /// principals the source was asked for, and whether the private scope was delivered.
    fn private_probe(name: &str, agent: &str) -> (Vec<String>, bool) {
        let src = acl();
        let mut h = host_for(name);
        let hi = Message::Hello {
            min_version: 1,
            max_version: 1,
            agent: agent.into(),
        };
        h.handle(hi, 0, &src);
        let out = h.handle(sub(1, "ws/private", Vec::new()), 0, &src);
        let delivered = out
            .iter()
            .any(|o| matches!(o, Output::Send(Message::Events { .. })));
        (src.seen.take(), delivered)
    }

    #[test]
    fn the_source_is_asked_for_the_connections_authenticated_principal() {
        let (seen, delivered) = private_probe("alice", "anything");
        assert_eq!(seen, vec!["account:alice".to_string()]);
        assert!(delivered);
        // A Hello that names another account changes nothing.
        let (seen, delivered) = private_probe("bob", "alice");
        assert_eq!(seen, vec!["account:bob".to_string()]);
        assert!(!delivered);
    }

    #[test]
    fn a_denied_scope_is_answered_like_a_scope_that_does_not_exist() {
        let src = acl();
        let mut h = host_for("bob");
        ready(&mut h, &src.log);
        let denied = h.handle(sub(1, "ws/private", Vec::new()), 0, &src);
        let missing = h.handle(sub(2, "elsewhere", Vec::new()), 0, &src);
        let code = |o: &[Output]| match o {
            [Output::Send(Message::Error { code, .. })] => Some(*code),
            _ => None,
        };
        assert_eq!(code(&denied), Some(ErrorCode::InvalidScope));
        assert_eq!(code(&denied), code(&missing));
        // The refused subscription holds no state and the connection stays usable.
        assert!(h.subscriptions().is_empty());
        let shared = h.handle(sub(3, "ws/shared", Vec::new()), 0, &src);
        assert_eq!(seqs(&shared, 3), vec![2]);
    }

    #[test]
    fn live_polls_use_the_same_principal_as_the_first_read() {
        let src = acl();
        let mut h = host_for("alice");
        ready(&mut h, &src.log);
        h.handle(sub(1, "ws/shared", Vec::new()), 0, &src);
        h.poll(1, &src);
        let seen = src.seen.take();
        assert!(seen.len() >= 2, "{seen:?}");
        assert!(seen.iter().all(|p| p == "account:alice"), "{seen:?}");
        assert_eq!(h.principal().to_string(), "account:alice");
    }

    #[test]
    fn the_development_constructor_is_labelled_as_such() {
        let h = SubscriptionHost::local_development(HostConfig::new("lattice"), 0).unwrap();
        assert_eq!(h.principal().to_string(), "local-development");
    }

    #[test]
    fn a_dropped_principal_is_caught_by_the_witness() {
        use lattice_core::bugs;
        // Both hosts are read for their own identity, and bob never sees alice's scope.
        let witness = || {
            let (a, a_delivered) = private_probe("alice", "x");
            let (b, b_delivered) = private_probe("bob", "x");
            a == ["account:alice"] && a_delivered && b == ["account:bob"] && !b_delivered
        };
        bugs::clear();
        assert!(witness(), "clean build must satisfy the witness");
        bugs::enable(Bug::SubscribePrincipalDropped);
        let caught = !witness();
        bugs::clear();
        assert!(
            caught,
            "a host that drops the principal must fail the witness"
        );
    }
}

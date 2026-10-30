//! Sans-I/O client for the sync protocol v1 (`formats/sync-protocol-v1.md`).
//!
//! [`SyncClient`] is a pure state machine. The caller owns the socket, the framing and the clock:
//! it decodes frames into [`Frame`], passes them to [`SyncClient::handle`] together with a logical
//! time, writes every [`Action::Send`] it gets back, and calls [`SyncClient::tick`] now and then.
//! There is no I/O, no thread, no real time and no panic path: every network input maps to an
//! action or a [`ClientError`].
//!
//! The SDK crate may not depend on `lattice-api` (see `layers.toml`), so [`Frame`], [`Event`] and
//! [`WireError`] are the shared types of `lattice-proto`, the same ones the server codec uses;
//! `lattice_proto::sync::{encode, decode}` turns them into bytes and back. The limits are the ones
//! in the format document, defined once there.
//!
//! # Delivery guarantee
//!
//! The protocol is at-least-once. After a reconnect the client resubscribes with the last cursor it
//! acknowledged, so the server may replay events the application already saw. The client drops every
//! event whose cursor is not strictly greater than the last one it delivered, so the application sees
//! each event once, in order, and a server that honours cursors cannot cause a gap. Cursors are
//! opaque to the protocol; this client assumes only what a change feed gives it: within one
//! subscription, later events have larger cursors when compared by length and then byte by byte
//! (fixed-width big-endian counters satisfy this).

use std::collections::BTreeMap;
use std::fmt;

use lattice_proto::sync::{
    MAX_CURSOR_LEN, MAX_ERROR_MESSAGE_LEN, MAX_NAME_LEN, MAX_SCOPE_LEN, VERSION_MAX, VERSION_MIN,
};

/// The code of an `Error` frame (format document, "Error codes"). The shared definition in
/// `lattice_proto::sync`.
pub use lattice_proto::sync::ErrorCode as WireError;
/// One event: the position after it and its opaque body. The shared definition in
/// `lattice_proto::sync`.
pub use lattice_proto::sync::Event;
/// A protocol message, the one the server codec uses (`lattice_proto::sync::Message`).
pub use lattice_proto::sync::Message as Frame;

/// Lowest protocol version this client can speak.
pub const SUPPORTED_MIN_VERSION: u16 = VERSION_MIN;
/// Highest protocol version this client can speak.
pub const SUPPORTED_MAX_VERSION: u16 = VERSION_MAX;

/// Connection phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// No connection; [`SyncClient::connect`] starts one.
    Disconnected,
    /// `Hello` sent, waiting for `Welcome`.
    HelloSent,
    /// Negotiated, no live subscription.
    Ready,
    /// Negotiated with at least one live subscription.
    Subscribed,
    /// Terminal: the server is incompatible or the application closed the client.
    Closed,
}

/// Why a connection ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CloseReason {
    /// The server speaks no version this client speaks. Reconnecting will not help.
    Incompatible,
    /// The server sent a connection-level error.
    ServerError {
        /// Its code.
        code: WireError,
        /// Its text.
        message: String,
    },
    /// The peer broke the protocol; the text names the rule.
    ProtocolViolation(&'static str),
    /// A ping went unanswered for the configured timeout.
    PingTimeout,
    /// No `Welcome` arrived in time.
    HandshakeTimeout,
    /// The application closed the client.
    ByApplication,
}

/// What the caller must do after an input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Write this frame to the connection.
    Send(Frame),
    /// The handshake finished.
    Ready {
        /// Negotiated version.
        version: u16,
        /// Largest payload the server accepts.
        max_payload: u32,
    },
    /// Hand this event to the application. Each event is delivered once, in order.
    Deliver {
        /// Subscription id.
        sub: u32,
        /// Event position.
        cursor: Vec<u8>,
        /// Event body.
        data: Vec<u8>,
    },
    /// The server refused or dropped one subscription; it is no longer live.
    SubscriptionFailed {
        /// Subscription id.
        sub: u32,
        /// Reason.
        code: WireError,
        /// Server text.
        message: String,
    },
    /// Close the transport. Unless the phase is `Closed`, [`SyncClient::reconnect`] may follow.
    Close(CloseReason),
}

/// A call that does not fit the client's state or limits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientError {
    /// The configuration violates a protocol limit.
    InvalidConfig(&'static str),
    /// The call is not allowed in this phase.
    WrongPhase(Phase),
    /// Scope empty, too long or containing a control character.
    InvalidScope,
    /// Cursor longer than 64 bytes (or empty where one is required).
    InvalidCursor,
    /// No such live subscription.
    UnknownSubscription(u32),
    /// Acknowledged a cursor the client has not delivered.
    AckBeyondDelivered,
    /// Subscription ids are used up.
    SubscriptionIdsExhausted,
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ClientError::InvalidConfig(why) => write!(f, "invalid client configuration: {why}"),
            ClientError::WrongPhase(p) => write!(f, "not allowed in phase {p:?}"),
            ClientError::InvalidScope => f.write_str("invalid scope"),
            ClientError::InvalidCursor => f.write_str("invalid cursor"),
            ClientError::UnknownSubscription(s) => write!(f, "unknown subscription {s}"),
            ClientError::AckBeyondDelivered => f.write_str("ack beyond the last delivered cursor"),
            ClientError::SubscriptionIdsExhausted => f.write_str("subscription ids exhausted"),
        }
    }
}

impl std::error::Error for ClientError {}

/// Client settings. Times are in the caller's logical ticks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientConfig {
    /// Lowest version offered in `Hello`, at least 1.
    pub min_version: u16,
    /// Highest version offered in `Hello`.
    pub max_version: u16,
    /// Agent name, 1 to 64 bytes.
    pub agent: String,
    /// Idle ticks after which the client sends a `Ping`.
    pub ping_interval: u64,
    /// Ticks without an answer (a `Welcome` or a `Pong`) after which the connection is dropped.
    pub ping_timeout: u64,
    /// Number of [`SyncClient::ack`] calls merged into one wire `Ack`, at least 1.
    pub ack_batch: u32,
}

impl ClientConfig {
    /// Defaults: the supported version range, 30 tick interval, 10 tick timeout, no batching.
    pub fn new(agent: &str) -> Self {
        ClientConfig {
            min_version: SUPPORTED_MIN_VERSION,
            max_version: SUPPORTED_MAX_VERSION,
            agent: agent.to_owned(),
            ping_interval: 30,
            ping_timeout: 10,
            ack_batch: 1,
        }
    }

    fn validate(&self) -> Result<(), ClientError> {
        if self.min_version == 0 || self.min_version > self.max_version {
            return Err(ClientError::InvalidConfig("version range"));
        }
        if self.agent.is_empty()
            || self.agent.len() > MAX_NAME_LEN
            || self.agent.chars().any(char::is_control)
        {
            return Err(ClientError::InvalidConfig("agent name"));
        }
        if self.ack_batch == 0 {
            return Err(ClientError::InvalidConfig("ack batch"));
        }
        if self.ping_timeout == 0 {
            return Err(ClientError::InvalidConfig("ping timeout"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
struct Sub {
    scope: String,
    /// Last cursor handed to the application (or the starting cursor).
    delivered: Vec<u8>,
    /// Last cursor sent in an `Ack` (or the starting cursor); the resume point.
    acked: Vec<u8>,
    /// Highest cursor the application acknowledged but that is not yet on the wire.
    processed: Option<Vec<u8>>,
    /// `ack` calls merged into `processed`.
    pending_acks: u32,
}

/// `a` is strictly after `b` (length first, then bytes).
fn cursor_after(a: &[u8], b: &[u8]) -> bool {
    (a.len(), a) > (b.len(), b)
}

fn clip(text: &str) -> String {
    let mut end = text.len().min(MAX_ERROR_MESSAGE_LEN);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

/// The sync client state machine.
#[derive(Debug, Clone)]
pub struct SyncClient {
    cfg: ClientConfig,
    phase: Phase,
    last_rx: u64,
    ping: Option<(u64, u64)>,
    next_nonce: u64,
    subs: BTreeMap<u32, Sub>,
    next_sub: u32,
    version: Option<u16>,
}

impl SyncClient {
    /// Build a client; refuses a configuration that would produce an invalid `Hello`.
    pub fn new(cfg: ClientConfig) -> Result<Self, ClientError> {
        cfg.validate()?;
        Ok(SyncClient {
            cfg,
            phase: Phase::Disconnected,
            last_rx: 0,
            ping: None,
            next_nonce: 1,
            subs: BTreeMap::new(),
            next_sub: 1,
            version: None,
        })
    }

    /// Current phase.
    pub fn phase(&self) -> Phase {
        self.phase
    }

    /// The negotiated version, once the handshake finished on the current connection.
    pub fn version(&self) -> Option<u16> {
        self.version
    }

    /// Ids of the live subscriptions.
    pub fn subscriptions(&self) -> Vec<u32> {
        self.subs.keys().copied().collect()
    }

    /// The cursor a reconnect would resume `sub` from (the last acknowledged one).
    pub fn resume_cursor(&self, sub: u32) -> Option<&[u8]> {
        self.subs.get(&sub).map(|s| s.acked.as_slice())
    }

    /// Start the first connection: returns the `Hello` to send.
    pub fn connect(&mut self, now: u64) -> Result<Vec<Action>, ClientError> {
        if self.phase != Phase::Disconnected {
            return Err(ClientError::WrongPhase(self.phase));
        }
        Ok(self.begin(now))
    }

    /// Start a new connection after the old one ended. Live subscriptions are resubscribed from
    /// their last acknowledged cursor once the new `Welcome` arrives.
    pub fn reconnect(&mut self, now: u64) -> Result<Vec<Action>, ClientError> {
        self.connect(now)
    }

    /// The transport dropped without a protocol-level reason. Subscriptions are kept.
    pub fn disconnected(&mut self) {
        if self.phase != Phase::Closed {
            self.phase = Phase::Disconnected;
            self.ping = None;
            self.version = None;
        }
    }

    /// The application is done. Terminal.
    pub fn close(&mut self) -> Vec<Action> {
        if self.phase == Phase::Closed {
            return Vec::new();
        }
        self.phase = Phase::Closed;
        vec![Action::Close(CloseReason::ByApplication)]
    }

    fn begin(&mut self, now: u64) -> Vec<Action> {
        self.phase = Phase::HelloSent;
        self.last_rx = now;
        self.ping = None;
        self.version = None;
        vec![Action::Send(Frame::Hello {
            min_version: self.cfg.min_version,
            max_version: self.cfg.max_version,
            agent: self.cfg.agent.clone(),
        })]
    }

    /// Start a subscription. `cursor` is empty to start from the beginning. Allowed once the
    /// handshake finished.
    pub fn subscribe(
        &mut self,
        scope: &str,
        cursor: &[u8],
    ) -> Result<(u32, Vec<Action>), ClientError> {
        if !matches!(self.phase, Phase::Ready | Phase::Subscribed) {
            return Err(ClientError::WrongPhase(self.phase));
        }
        if scope.is_empty() || scope.len() > MAX_SCOPE_LEN || scope.chars().any(char::is_control) {
            return Err(ClientError::InvalidScope);
        }
        if cursor.len() > MAX_CURSOR_LEN {
            return Err(ClientError::InvalidCursor);
        }
        let sub = self.next_sub;
        self.next_sub = sub
            .checked_add(1)
            .ok_or(ClientError::SubscriptionIdsExhausted)?;
        self.subs.insert(
            sub,
            Sub {
                scope: scope.to_owned(),
                delivered: cursor.to_vec(),
                acked: cursor.to_vec(),
                processed: None,
                pending_acks: 0,
            },
        );
        self.phase = Phase::Subscribed;
        Ok((
            sub,
            vec![Action::Send(Frame::Subscribe {
                sub,
                scope: scope.to_owned(),
                cursor: cursor.to_vec(),
            })],
        ))
    }

    /// The application finished processing everything up to `cursor` on `sub`. Calls are batched:
    /// a wire `Ack` is produced every `ack_batch` calls, or by [`SyncClient::flush_acks`].
    pub fn ack(&mut self, sub: u32, cursor: &[u8]) -> Result<Vec<Action>, ClientError> {
        let batch = self.cfg.ack_batch;
        let s = self
            .subs
            .get_mut(&sub)
            .ok_or(ClientError::UnknownSubscription(sub))?;
        if cursor.is_empty() || cursor.len() > MAX_CURSOR_LEN {
            return Err(ClientError::InvalidCursor);
        }
        if cursor_after(cursor, &s.delivered) {
            return Err(ClientError::AckBeyondDelivered);
        }
        let newest = match &s.processed {
            Some(p) => cursor_after(cursor, p),
            None => cursor_after(cursor, &s.acked),
        };
        if newest {
            s.processed = Some(cursor.to_vec());
        }
        s.pending_acks = s.pending_acks.saturating_add(1);
        if s.pending_acks >= batch {
            return Ok(Self::take_ack(sub, s, self.phase));
        }
        Ok(Vec::new())
    }

    /// Send the pending acknowledgement of every subscription now.
    pub fn flush_acks(&mut self) -> Vec<Action> {
        let phase = self.phase;
        let mut out = Vec::new();
        for (&id, s) in self.subs.iter_mut() {
            out.extend(Self::take_ack(id, s, phase));
        }
        out
    }

    fn take_ack(id: u32, s: &mut Sub, phase: Phase) -> Vec<Action> {
        s.pending_acks = 0;
        let Some(cursor) = s.processed.take() else {
            return Vec::new();
        };
        // The acknowledgement counts as given even if the connection is down: the resume point
        // moves, and the server learns it from the next Subscribe.
        s.acked = cursor.clone();
        if matches!(phase, Phase::Ready | Phase::Subscribed) {
            vec![Action::Send(Frame::Ack { sub: id, cursor })]
        } else {
            Vec::new()
        }
    }

    /// Advance the logical clock: sends a `Ping` after `ping_interval` idle ticks and drops the
    /// connection when an answer is overdue by `ping_timeout`.
    pub fn tick(&mut self, now: u64) -> Vec<Action> {
        let idle = now.saturating_sub(self.last_rx);
        match self.phase {
            Phase::Disconnected | Phase::Closed => Vec::new(),
            Phase::HelloSent => {
                if idle >= self.cfg.ping_timeout {
                    self.drop_connection(CloseReason::HandshakeTimeout)
                } else {
                    Vec::new()
                }
            }
            Phase::Ready | Phase::Subscribed => match self.ping {
                Some((_, sent)) => {
                    if now.saturating_sub(sent) >= self.cfg.ping_timeout {
                        self.drop_connection(CloseReason::PingTimeout)
                    } else {
                        Vec::new()
                    }
                }
                None => {
                    if idle >= self.cfg.ping_interval {
                        let nonce = self.next_nonce;
                        self.next_nonce = nonce.wrapping_add(1);
                        self.ping = Some((nonce, now));
                        vec![Action::Send(Frame::Ping { nonce })]
                    } else {
                        Vec::new()
                    }
                }
            },
        }
    }

    fn drop_connection(&mut self, reason: CloseReason) -> Vec<Action> {
        self.phase = Phase::Disconnected;
        self.ping = None;
        self.version = None;
        vec![Action::Close(reason)]
    }

    fn violation(&mut self, rule: &'static str) -> Vec<Action> {
        let mut out = vec![Action::Send(Frame::Error {
            code: WireError::Malformed,
            sub: 0,
            message: clip(rule),
        })];
        out.extend(self.drop_connection(CloseReason::ProtocolViolation(rule)));
        out
    }

    /// Feed one decoded frame from the server.
    pub fn handle(&mut self, frame: Frame, now: u64) -> Result<Vec<Action>, ClientError> {
        if matches!(self.phase, Phase::Disconnected | Phase::Closed) {
            return Err(ClientError::WrongPhase(self.phase));
        }
        self.last_rx = now;
        Ok(match frame {
            Frame::Welcome {
                version,
                max_payload,
                ..
            } => self.on_welcome(version, max_payload),
            Frame::Events { sub, events } => {
                if self.phase == Phase::HelloSent {
                    return Ok(self.violation("events before welcome"));
                }
                self.on_events(sub, events)
            }
            Frame::Error { code, sub, message } => self.on_error(code, sub, message),
            Frame::Ping { nonce } => vec![Action::Send(Frame::Pong { nonce })],
            Frame::Pong { nonce } => {
                if matches!(self.ping, Some((n, _)) if n == nonce) {
                    self.ping = None;
                }
                Vec::new()
            }
            Frame::Hello { .. } | Frame::Subscribe { .. } | Frame::Ack { .. } => {
                self.violation("client-to-server frame received")
            }
        })
    }

    fn on_welcome(&mut self, version: u16, max_payload: u32) -> Vec<Action> {
        if self.phase != Phase::HelloSent {
            return self.violation("welcome outside handshake");
        }
        if version < self.cfg.min_version || version > self.cfg.max_version {
            self.phase = Phase::Closed;
            self.ping = None;
            return vec![Action::Close(CloseReason::Incompatible)];
        }
        self.version = Some(version);
        let mut out = vec![Action::Ready {
            version,
            max_payload,
        }];
        // Resume: every live subscription restarts from its last acknowledged cursor. Events the
        // application already saw are filtered out by `on_events`.
        for (&id, s) in &self.subs {
            out.push(Action::Send(Frame::Subscribe {
                sub: id,
                scope: s.scope.clone(),
                cursor: s.acked.clone(),
            }));
        }
        self.phase = if self.subs.is_empty() {
            Phase::Ready
        } else {
            Phase::Subscribed
        };
        out
    }

    fn on_events(&mut self, sub: u32, events: Vec<Event>) -> Vec<Action> {
        let Some(s) = self.subs.get_mut(&sub) else {
            // A late frame for a subscription that failed or never existed: nothing to deliver.
            return Vec::new();
        };
        let mut out = Vec::new();
        for ev in events {
            if ev.cursor.len() > MAX_CURSOR_LEN || !cursor_after(&ev.cursor, &s.delivered) {
                continue;
            }
            s.delivered.clone_from(&ev.cursor);
            out.push(Action::Deliver {
                sub,
                cursor: ev.cursor,
                data: ev.data,
            });
        }
        out
    }

    fn on_error(&mut self, code: WireError, sub: u32, message: String) -> Vec<Action> {
        if sub != 0 {
            return if self.subs.remove(&sub).is_some() {
                if self.subs.is_empty() && self.phase == Phase::Subscribed {
                    self.phase = Phase::Ready;
                }
                vec![Action::SubscriptionFailed { sub, code, message }]
            } else {
                Vec::new()
            };
        }
        if code == WireError::UnsupportedVersion {
            self.phase = Phase::Closed;
            self.ping = None;
            self.version = None;
            return vec![Action::Close(CloseReason::Incompatible)];
        }
        self.drop_connection(CloseReason::ServerError { code, message })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn cur(n: u64) -> Vec<u8> {
        n.to_be_bytes().to_vec()
    }

    fn ev(n: u64) -> Event {
        Event {
            cursor: cur(n),
            data: vec![n as u8],
        }
    }

    fn events(sub: u32, from: u64, to: u64) -> Frame {
        Frame::Events {
            sub,
            events: (from..=to).map(ev).collect(),
        }
    }

    fn welcome(version: u16) -> Frame {
        Frame::Welcome {
            version,
            max_payload: 1 << 20,
            server: "srv".into(),
        }
    }

    fn delivered(actions: &[Action]) -> Vec<u64> {
        actions
            .iter()
            .filter_map(|a| match a {
                Action::Deliver { cursor, .. } => {
                    let mut b = [0u8; 8];
                    b.copy_from_slice(cursor);
                    Some(u64::from_be_bytes(b))
                }
                _ => None,
            })
            .collect()
    }

    fn client(batch: u32) -> SyncClient {
        let mut cfg = ClientConfig::new("sdk-rs");
        cfg.ack_batch = batch;
        SyncClient::new(cfg).unwrap()
    }

    fn ready(c: &mut SyncClient) {
        c.connect(0).unwrap();
        c.handle(welcome(1), 0).unwrap();
    }

    #[test]
    fn happy_path() {
        let mut c = client(1);
        let hello = c.connect(0).unwrap();
        assert_eq!(
            hello,
            vec![Action::Send(Frame::Hello {
                min_version: 1,
                max_version: 1,
                agent: "sdk-rs".into()
            })]
        );
        assert_eq!(c.phase(), Phase::HelloSent);
        let a = c.handle(welcome(1), 1).unwrap();
        assert_eq!(
            a,
            vec![Action::Ready {
                version: 1,
                max_payload: 1 << 20
            }]
        );
        assert_eq!(c.phase(), Phase::Ready);
        let (sub, a) = c.subscribe("ws/main", &[]).unwrap();
        assert_eq!(sub, 1);
        assert_eq!(
            a,
            vec![Action::Send(Frame::Subscribe {
                sub: 1,
                scope: "ws/main".into(),
                cursor: vec![]
            })]
        );
        assert_eq!(c.phase(), Phase::Subscribed);
        let a = c.handle(events(1, 1, 3), 2).unwrap();
        assert_eq!(delivered(&a), vec![1, 2, 3]);
        let a = c.ack(1, &cur(3)).unwrap();
        assert_eq!(
            a,
            vec![Action::Send(Frame::Ack {
                sub: 1,
                cursor: cur(3)
            })]
        );
        assert_eq!(c.resume_cursor(1), Some(cur(3).as_slice()));
    }

    #[test]
    fn subscribe_before_welcome_is_refused() {
        let mut c = client(1);
        assert_eq!(
            c.subscribe("s", &[]),
            Err(ClientError::WrongPhase(Phase::Disconnected))
        );
        c.connect(0).unwrap();
        assert_eq!(
            c.subscribe("s", &[]),
            Err(ClientError::WrongPhase(Phase::HelloSent))
        );
    }

    #[test]
    fn negotiates_down() {
        let mut cfg = ClientConfig::new("a");
        cfg.max_version = 3;
        let mut c = SyncClient::new(cfg).unwrap();
        let hello = c.connect(0).unwrap();
        assert!(matches!(
            hello[0],
            Action::Send(Frame::Hello {
                min_version: 1,
                max_version: 3,
                ..
            })
        ));
        c.handle(welcome(1), 0).unwrap();
        assert_eq!(c.phase(), Phase::Ready);
        assert_eq!(c.version(), Some(1));
    }

    #[test]
    fn welcome_outside_offered_range_is_incompatible() {
        let mut c = client(1);
        c.connect(0).unwrap();
        let a = c.handle(welcome(2), 0).unwrap();
        assert_eq!(a, vec![Action::Close(CloseReason::Incompatible)]);
        assert_eq!(c.phase(), Phase::Closed);
        assert_eq!(c.reconnect(1), Err(ClientError::WrongPhase(Phase::Closed)));
    }

    #[test]
    fn unsupported_version_error_is_terminal() {
        let mut c = client(1);
        c.connect(0).unwrap();
        let a = c
            .handle(
                Frame::Error {
                    code: WireError::UnsupportedVersion,
                    sub: 0,
                    message: "version".into(),
                },
                0,
            )
            .unwrap();
        assert_eq!(a, vec![Action::Close(CloseReason::Incompatible)]);
        assert_eq!(c.phase(), Phase::Closed);
    }

    #[test]
    fn disconnect_resume_drops_replayed_duplicates() {
        let mut c = client(1);
        ready(&mut c);
        let (sub, _) = c.subscribe("ws/main", &[]).unwrap();
        let a = c.handle(events(sub, 1, 5), 1).unwrap();
        assert_eq!(delivered(&a), vec![1, 2, 3, 4, 5]);
        c.ack(sub, &cur(3)).unwrap();
        c.disconnected();
        assert_eq!(c.phase(), Phase::Disconnected);
        c.reconnect(10).unwrap();
        let a = c.handle(welcome(1), 10).unwrap();
        assert_eq!(
            a[1],
            Action::Send(Frame::Subscribe {
                sub,
                scope: "ws/main".into(),
                cursor: cur(3)
            })
        );
        assert_eq!(c.phase(), Phase::Subscribed);
        // The server replays 4 and 5 (already delivered) and then continues.
        let a = c.handle(events(sub, 4, 7), 11).unwrap();
        assert_eq!(delivered(&a), vec![6, 7]);
        // A full duplicate batch yields nothing.
        assert!(delivered(&c.handle(events(sub, 1, 7), 12).unwrap()).is_empty());
    }

    #[test]
    fn ack_batching_and_flush() {
        let mut c = client(3);
        ready(&mut c);
        let (sub, _) = c.subscribe("s", &[]).unwrap();
        c.handle(events(sub, 1, 4), 0).unwrap();
        assert!(c.ack(sub, &cur(1)).unwrap().is_empty());
        assert!(c.ack(sub, &cur(2)).unwrap().is_empty());
        let a = c.ack(sub, &cur(3)).unwrap();
        assert_eq!(
            a,
            vec![Action::Send(Frame::Ack {
                sub,
                cursor: cur(3)
            })]
        );
        assert!(c.ack(sub, &cur(4)).unwrap().is_empty());
        assert_eq!(c.resume_cursor(sub), Some(cur(3).as_slice()));
        let a = c.flush_acks();
        assert_eq!(
            a,
            vec![Action::Send(Frame::Ack {
                sub,
                cursor: cur(4)
            })]
        );
        assert!(c.flush_acks().is_empty());
        assert_eq!(c.ack(sub, &cur(9)), Err(ClientError::AckBeyondDelivered));
        assert_eq!(c.ack(9, &cur(1)), Err(ClientError::UnknownSubscription(9)));
    }

    #[test]
    fn subscription_error_removes_only_that_subscription() {
        let mut c = client(1);
        ready(&mut c);
        let (a, _) = c.subscribe("a", &[]).unwrap();
        let (b, _) = c.subscribe("b", &[]).unwrap();
        let out = c
            .handle(
                Frame::Error {
                    code: WireError::InvalidScope,
                    sub: a,
                    message: "no".into(),
                },
                0,
            )
            .unwrap();
        assert_eq!(
            out,
            vec![Action::SubscriptionFailed {
                sub: a,
                code: WireError::InvalidScope,
                message: "no".into()
            }]
        );
        assert_eq!(c.subscriptions(), vec![b]);
        assert_eq!(c.phase(), Phase::Subscribed);
        c.handle(
            Frame::Error {
                code: WireError::InvalidCursor,
                sub: b,
                message: String::new(),
            },
            0,
        )
        .unwrap();
        assert_eq!(c.phase(), Phase::Ready);
    }

    #[test]
    fn connection_error_drops_but_allows_reconnect() {
        let mut c = client(1);
        ready(&mut c);
        let out = c
            .handle(
                Frame::Error {
                    code: WireError::Internal,
                    sub: 0,
                    message: "boom".into(),
                },
                0,
            )
            .unwrap();
        assert_eq!(
            out,
            vec![Action::Close(CloseReason::ServerError {
                code: WireError::Internal,
                message: "boom".into()
            })]
        );
        assert_eq!(c.phase(), Phase::Disconnected);
        assert!(c.reconnect(1).is_ok());
    }

    #[test]
    fn protocol_violations_send_malformed_and_drop() {
        let mut c = client(1);
        ready(&mut c);
        let out = c.handle(welcome(1), 0).unwrap();
        assert!(matches!(
            out[0],
            Action::Send(Frame::Error {
                code: WireError::Malformed,
                sub: 0,
                ..
            })
        ));
        assert!(matches!(
            out[1],
            Action::Close(CloseReason::ProtocolViolation(_))
        ));
        assert_eq!(c.phase(), Phase::Disconnected);
        assert!(c.handle(welcome(1), 0).is_err());
    }

    #[test]
    fn server_ping_is_answered() {
        let mut c = client(1);
        ready(&mut c);
        assert_eq!(
            c.handle(Frame::Ping { nonce: 77 }, 1).unwrap(),
            vec![Action::Send(Frame::Pong { nonce: 77 })]
        );
    }

    #[test]
    fn ping_timeout_with_logical_clock() {
        let mut c = client(1);
        ready(&mut c);
        assert!(c.tick(29).is_empty());
        assert_eq!(c.tick(30), vec![Action::Send(Frame::Ping { nonce: 1 })]);
        // Waiting: nothing until the timeout.
        assert!(c.tick(39).is_empty());
        assert_eq!(c.tick(40), vec![Action::Close(CloseReason::PingTimeout)]);
        assert_eq!(c.phase(), Phase::Disconnected);
    }

    #[test]
    fn pong_clears_the_ping() {
        let mut c = client(1);
        ready(&mut c);
        c.tick(30);
        // A wrong nonce does not count.
        c.handle(Frame::Pong { nonce: 99 }, 31).unwrap();
        c.handle(Frame::Pong { nonce: 1 }, 32).unwrap();
        assert!(c.tick(40).is_empty());
        assert_eq!(c.phase(), Phase::Ready);
        // The next ping is due 30 ticks after the last frame.
        assert_eq!(c.tick(62), vec![Action::Send(Frame::Ping { nonce: 2 })]);
    }

    #[test]
    fn handshake_timeout() {
        let mut c = client(1);
        c.connect(5).unwrap();
        assert!(c.tick(14).is_empty());
        assert_eq!(
            c.tick(15),
            vec![Action::Close(CloseReason::HandshakeTimeout)]
        );
        assert_eq!(c.phase(), Phase::Disconnected);
    }

    #[test]
    fn invalid_inputs_are_errors_not_panics() {
        assert!(SyncClient::new(ClientConfig::new("")).is_err());
        let mut bad = ClientConfig::new("a");
        bad.min_version = 0;
        assert!(SyncClient::new(bad).is_err());
        let mut c = client(1);
        ready(&mut c);
        assert_eq!(c.subscribe("", &[]), Err(ClientError::InvalidScope));
        assert_eq!(c.subscribe("a\nb", &[]), Err(ClientError::InvalidScope));
        assert_eq!(c.subscribe("a", &[0; 65]), Err(ClientError::InvalidCursor));
        // Unicode text near the error clip boundary does not panic.
        assert!(clip(&"é".repeat(300)).len() <= MAX_ERROR_MESSAGE_LEN);
    }

    // ---- model server -------------------------------------------------------------------

    /// What a server does after a client resubscribes with `acked` (positions 1..).
    #[derive(Clone, Copy)]
    enum Server {
        /// Resumes at or before the cursor: never skips (cursor-honouring).
        Honest,
        /// Resumes one event too late: skips an event.
        SkipsOne,
    }

    /// Positions the application received, in order.
    fn run(server: Server, script: &[(u8, u8)]) -> Vec<u64> {
        let mut c = client(2);
        c.connect(0).unwrap();
        c.handle(welcome(1), 0).unwrap();
        let (sub, _) = c.subscribe("s", &[]).unwrap();
        let mut seen: Vec<u64> = Vec::new();
        let mut next: u64 = 1; // next position the server will send
        let mut now = 1;
        for &(op, arg) in script {
            now += 1;
            let arg = u64::from(arg);
            match op % 4 {
                // send a batch of 1..=4 events from the server's pointer
                0 | 1 => {
                    let n = 1 + arg % 4;
                    let out = c.handle(events(sub, next, next + n - 1), now).unwrap();
                    seen.extend(delivered(&out));
                    next += n;
                }
                // the application acknowledges everything seen so far
                2 => {
                    if let Some(&last) = seen.last() {
                        c.ack(sub, &cur(last)).unwrap();
                        c.flush_acks();
                    }
                }
                // reconnect: the server resumes from the client's cursor, with an overlap
                _ => {
                    c.disconnected();
                    c.reconnect(now).unwrap();
                    let out = c.handle(welcome(1), now).unwrap();
                    let cursor = out
                        .iter()
                        .find_map(|a| match a {
                            Action::Send(Frame::Subscribe { cursor, .. }) => Some(cursor.clone()),
                            _ => None,
                        })
                        .unwrap();
                    let acked = if cursor.is_empty() {
                        0
                    } else {
                        let mut b = [0u8; 8];
                        b.copy_from_slice(&cursor);
                        u64::from_be_bytes(b)
                    };
                    next = match server {
                        Server::Honest => (acked + 1).saturating_sub(arg % 3).max(1),
                        Server::SkipsOne => acked + 2,
                    };
                }
            }
        }
        seen
    }

    /// The application's view must be exactly 1, 2, 3, ... with nothing missing or repeated.
    fn check_exactly_once(seen: &[u64]) -> Result<(), String> {
        for (i, &p) in seen.iter().enumerate() {
            let want = i as u64 + 1;
            if p != want {
                return Err(format!("position {i}: got {p}, want {want}"));
            }
        }
        Ok(())
    }

    #[test]
    fn buggy_server_makes_the_property_fail() {
        // deliver 1..=2, ack, reconnect: the buggy server resumes at 4 instead of 3.
        let script = [(0, 1), (2, 0), (3, 0), (0, 1)];
        let seen = run(Server::SkipsOne, &script);
        assert_eq!(seen, vec![1, 2, 4, 5]);
        assert!(check_exactly_once(&seen).is_err());
        // The same script with an honest server is clean.
        let seen = run(Server::Honest, &script);
        assert_eq!(check_exactly_once(&seen), Ok(()));
    }

    proptest! {
        #[test]
        fn honest_server_never_causes_gap_or_duplicate(
            script in proptest::collection::vec((any::<u8>(), any::<u8>()), 0..80)
        ) {
            let seen = run(Server::Honest, &script);
            prop_assert_eq!(check_exactly_once(&seen), Ok(()));
        }

        #[test]
        fn arbitrary_frames_never_panic(
            frames in proptest::collection::vec((0u8..8, any::<u32>(), any::<u64>(), 0u8..4), 0..40)
        ) {
            let mut c = client(2);
            c.connect(0).unwrap();
            for (i, (kind, sub, n, code)) in frames.into_iter().enumerate() {
                let now = i as u64 * 7;
                let code = [WireError::Malformed, WireError::Internal, WireError::InvalidScope,
                    WireError::UnsupportedVersion][usize::from(code)];
                let f = match kind {
                    0 => welcome((n % 3) as u16),
                    1 => events(sub % 3, n % 5, n % 5 + 2),
                    2 => Frame::Error { code, sub: sub % 3, message: String::new() },
                    3 => Frame::Ping { nonce: n },
                    4 => Frame::Pong { nonce: n },
                    5 => Frame::Ack { sub, cursor: vec![] },
                    _ => Frame::Hello { min_version: 1, max_version: 1, agent: "x".into() },
                };
                let _ = c.handle(f, now);
                let _ = c.tick(now);
                let _ = c.subscribe("s", &[]);
                if c.phase() == Phase::Disconnected {
                    let _ = c.reconnect(now);
                }
            }
        }
    }
}

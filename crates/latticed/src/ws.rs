//! The WebSocket gateway for the sync protocol (issue #524, ADR 0027): a bounded listener that
//! upgrades an HTTP request, authenticates it, and runs one [`SubscriptionHost`] per connection.
//!
//! # What a connection goes through
//!
//! 1. **Admission.** The listener admits at most [`GatewayConfig::max_connections`] connections and at
//!    most [`GatewayConfig::max_handshakes`] that have not finished the upgrade; the next one gets
//!    `503` and is closed. These counters belong to the gateway. They are not the PostgreSQL
//!    listener's worker count, cancel registry or TLS start limiter, which this module never touches.
//!    The principal is only known once the token verified, so the per-principal limits (issue #529)
//!    are applied then, still before the `101`: see [`GatewayConfig::max_connections_per_principal`]
//!    and [`GatewayConfig::reserved_connections`]. A refusal there is a `503` and is counted
//!    ([`GatewayHandle::limit_refusals`]).
//! 2. **Upgrade.** The request head is read under one absolute deadline
//!    ([`GatewayConfig::handshake_timeout`]) and at most [`GatewayConfig::max_head`] bytes, then
//!    judged by `websocket::parse_upgrade_with` (method, version, subprotocol `lattice.sync.v1`,
//!    origin policy). The target must be exactly [`SYNC_PATH`] with no query string: a token never
//!    travels in a URL, and a client that puts one there is told so instead of being served.
//! 3. **Authentication.** With [`GatewayAuth::Bearer`] the `Authorization: Bearer` header is checked
//!    by `lattice_api::bearer::verify_bearer`. Exactly one such header may be present. A missing,
//!    malformed, unknown, revoked or expired token all get the same `401`, and each counts against
//!    the client address (default 10 per minute); an address over its limit gets `429` without a
//!    verification. With [`GatewayAuth::LocalDevelopment`] (loopback only) no header is needed and
//!    the connection is the fixed development principal of ADR 0026.
//! 4. **Binding.** The resulting [`Principal`] is handed to `SubscriptionHost::new` and never
//!    changes; the `agent` name of the `Hello` is a label.
//! 5. **Serving.** Each sync frame is one binary WebSocket message (text is closed with 1003). A
//!    connection thread alternates between a socket read of at most [`GatewayConfig::slice`], the
//!    wake token and the WebSocket timers. When the wake token moved, or the fallback interval
//!    passed, it calls `SubscriptionHost::poll`, which reads through the [`EventSource`] (the
//!    executor's feed source, where row policy is applied). The wake carries no data.
//!
//! # Bounds
//!
//! * Before authentication: the head (default 8 KiB), the number of headers (the codec's 64), one
//!   deadline, and the two admission counters above.
//! * After: one inbound WebSocket frame is at most `HEADER_LEN + INBOUND_PAYLOAD` bytes (the
//!   `Welcome.max_payload` is [`INBOUND_PAYLOAD`], enough for every client frame), a reassembled
//!   message at most one maximal sync frame, subscriptions per connection are capped by
//!   [`GatewayConfig::max_subscriptions`], and every write has a per-call timeout and an absolute
//!   budget, so a client that stops reading is disconnected, never waited for. Output is written
//!   frame by frame, so the gateway holds at most the frames of one pump in memory:
//!   `max_subscriptions * max_frames_per_pump` frames of up to 1 MiB. A byte budget per connection
//!   and per principal is issue #529.
//!
//! # Credentials over time
//!
//! With [`GatewayAuth::BearerFile`] the accept thread checks the credential file every
//! [`GatewayConfig::credential_poll`] and reloads it when it changed, or when SIGHUP asked for a
//! reload (issue #577, ADR 0031). A file that does not load keeps the store in use; after ten minutes
//! of failed loads no token authenticates until one loads. [`GatewayHandle::replace_credentials`]
//! swaps the store directly. Whatever the source, a live connection re-checks the header it presented
//! against the current store when the store changes, and otherwise every
//! [`GatewayConfig::revalidate_every`] (which also catches a token that expires). If it no longer
//! verifies for the same account the connection is closed with 1008.
//!
//! Not reloaded: SCRAM verifiers, the decoy secret and the account policies (the PostgreSQL listener
//! and the row policy read them at start only).
//!
//! # Not here
//!
//! Idle deadlines, a byte budget per principal, the security event log and the same-port protocol
//! sniff are #578, #529, #530. TLS is `GatewayConfig::tls` (issue #578): when it is set, every
//! connection completes the handshake first, under the handshake deadline, and a non-loopback
//! address is refused unless it is set and bearer tokens are used.
//!
//! # The MCP endpoint
//!
//! `POST /mcp` on the same port (issue #527) is served by the `mcp_http` child module. It is routed
//! from the request line before the WebSocket parser runs, and only when a [`McpBackends`] factory was
//! given ([`GatewayHandle::start_with_mcp`]). Its Host, Origin, credential and size rules are those of
//! the module's documentation; the credential store, the failure count per address and the connection
//! places are the ones of this listener.

mod mcp_http;

pub use mcp_http::{AllowedHost, MAX_ALLOWED_HOSTS, check_allowed_hosts};

use crate::mcp_backend::McpBackend;
use crate::tokens::{CredentialWatch, Reload};
use crate::transport::{TlsConfig, Transport};
use lattice_api::bearer::{CredentialStore, FailureLimiter, verify_bearer};
use lattice_api::principal::Principal;
use lattice_api::subscribe::{CloseReason, EventSource, HostConfig, Output, SubscriptionHost};
use lattice_api::sync::{self, ErrorCode, HEADER_LEN};
use lattice_api::websocket::{
    self, Config as WsConfig, Connection, Event as WsEvent, Message as WsMessage, OriginPolicy,
    UpgradeConfig, close_code,
};
use lattice_core::bugs::{self, Bug};
use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use zeroize::Zeroizing;

/// A credential file that has not loaded for this long refuses every token until it loads (ADR 0031).
pub const REFUSE_AFTER: Duration = Duration::from_secs(600);

/// The request target the gateway upgrades to the sync protocol. An unknown path is `404`, never a
/// fallback to the sync protocol.
pub const SYNC_PATH: &str = "/sync";

/// The request target of the MCP tool endpoint (`POST`, issue #527). Served only when a
/// [`McpBackends`] factory was given.
pub const MCP_PATH: &str = "/mcp";

/// The largest payload a client frame may carry, advertised as `Welcome.max_payload`. Every client
/// message (`Hello`, `Subscribe`, `Ack`, `Error`, `Ping`, `Pong`) is far below it.
pub const INBOUND_PAYLOAD: u32 = 4096;

/// How long the gateway keeps reading after it queued a close, so that the peer receives it before a
/// reset could replace it.
const CLOSE_GRACE: Duration = Duration::from_millis(500);

/// The largest request line plus headers that bounds a legitimate bearer header (`Authorization:
/// Bearer lat_...`) is far smaller; this is the floor for [`GatewayConfig::max_head`].
const MIN_HEAD: usize = 512;

/// A number that changes when something may have committed, so a connection reads the feed only when
/// there is a reason to. It carries no row, table or scope.
///
/// The token is compared for inequality with the last one the connection used, so any scheme works
/// as long as a commit changes it: the newest committed LSN, or a counter.
pub trait Wake: Send + Sync {
    /// The current token, or `None` once nothing more will ever be published (the database is
    /// shutting down), which ends every connection with 1001.
    fn token(&self) -> Option<u64>;
}

/// A [`Wake`] backed by the executor's commit notifier: the token is the newest committed LSN.
#[derive(Clone)]
pub struct CommitWake(pub Arc<lattice_api::notify::CommitNotifier>);

impl Wake for CommitWake {
    fn token(&self) -> Option<u64> {
        // A zero-length wait for a value no LSN reaches reports `Closed` once the host is shutting
        // down and times out immediately otherwise; it never sleeps.
        match self.0.wait_after(u64::MAX, Duration::ZERO) {
            Err(lattice_api::notify::WaitError::Closed) => None,
            _ => Some(self.0.latest()),
        }
    }
}

/// A [`Wake`] that never changes: every connection polls at [`GatewayConfig::fallback_poll`].
#[derive(Debug, Clone, Copy, Default)]
pub struct NoWake;

impl Wake for NoWake {
    fn token(&self) -> Option<u64> {
        Some(0)
    }
}

/// Builds the MCP tool backend of one verified principal (issue #526). The gateway calls it once per
/// `POST /mcp`, with the principal the bearer token verified and no other input, so the backend is
/// bound to that principal alone.
pub trait McpBackends: Send + Sync {
    /// The backend for `principal`, or `None` when the executor cannot serve one (answered `503`).
    fn backend(&self, principal: &Principal) -> Option<McpBackend>;
}

/// How the gateway decides who is connected.
#[derive(Debug, Clone)]
pub enum GatewayAuth {
    /// No credential: the connection is the fixed development principal (ADR 0026). The listener
    /// refuses to bind a non-loopback address in this mode.
    LocalDevelopment,
    /// An `Authorization: Bearer` token from this store (ADR 0031). The store is fixed: nothing reads
    /// a file, and [`GatewayHandle::replace_credentials`] is the only way to change it.
    Bearer(CredentialStore),
    /// An `Authorization: Bearer` token from this credential file, which is read at start (the start
    /// fails if it does not load) and then watched: see [`GatewayConfig::credential_poll`].
    BearerFile(PathBuf),
}

/// Settings of the gateway listener.
#[derive(Debug, Clone)]
pub struct GatewayConfig {
    /// Address to bind. A non-loopback address needs both [`GatewayConfig::tls`] and bearer tokens
    /// (issue #578); plaintext is accepted on loopback only.
    pub address: SocketAddr,
    /// Server certificate and key for the gateway port (issue #578). When set, every connection
    /// completes a TLS handshake (within `handshake_timeout`) before the upgrade. The same
    /// [`TlsConfig`] the PostgreSQL listener uses.
    pub tls: Option<TlsConfig>,
    /// Connections admitted at once, including those still upgrading, `1..=1024`.
    pub max_connections: usize,
    /// Connections that have not finished the upgrade, `1..=max_connections`.
    pub max_handshakes: usize,
    /// Connections (WebSocket and `POST /mcp` alike) one principal may hold at once, `1..=1024`. The
    /// next one is refused with `503` before the upgrade. A value above `max_connections` has no
    /// effect beyond the global cap. The development principal is one principal.
    pub max_connections_per_principal: usize,
    /// Places of `max_connections` kept for principals that hold no connection yet, `0..=1024`. A
    /// principal that already holds a connection is refused (`503`) while more than
    /// `max_connections - reserved_connections` connections are open, so a few busy principals cannot
    /// take every place from a principal that is new. A value of `max_connections` or more means
    /// every principal gets one connection and no principal gets a second.
    pub reserved_connections: usize,
    /// `lattice_subscribe` long polls (a `wait_ms` above zero) one principal may have waiting, `1..=1024`.
    /// The next one is answered `503` and does not wait.
    pub max_long_polls_per_principal: usize,
    /// Long polls waiting at once on the whole gateway, `1..=1024`.
    pub max_long_polls: usize,
    /// One absolute budget from accept to the `101` response.
    pub handshake_timeout: Duration,
    /// Largest request head in bytes, `512..=65536`.
    pub max_head: usize,
    /// Which `Origin` values may connect. The default accepts none, so no browser can.
    pub origins: OriginPolicy,
    /// Who may connect.
    pub auth: GatewayAuth,
    /// Timeout of one socket write.
    pub write_timeout: Duration,
    /// Absolute budget for writing everything one step produced.
    pub response_timeout: Duration,
    /// Longest the connection thread blocks on the socket before it looks at the wake token, the
    /// timers and the stop flag. This is also the added delivery latency of a wake.
    pub slice: Duration,
    /// A connection reads the feed at least this often even when the wake token did not move, so a
    /// lost wake costs latency only.
    pub fallback_poll: Duration,
    /// How often a live connection is checked against the current credential store when nothing
    /// replaced the store. A token that expires is therefore closed within this interval.
    pub revalidate_every: Duration,
    /// For [`GatewayAuth::BearerFile`], how often the file is checked, `1 ms..=5 s` (ADR 0031 bounds
    /// the reload at 5 seconds). A change is loaded at the first check after it, and a revoked
    /// connection is closed at once after that.
    pub credential_poll: Duration,
    /// Authentication failures per client address within `failure_window` before further attempts
    /// are refused with `429`.
    pub failure_limit: usize,
    /// The window of `failure_limit`.
    pub failure_window: Duration,
    /// Subscriptions per connection.
    pub max_subscriptions: usize,
    /// Events per `Events` frame, `1..=256`.
    pub batch_events: usize,
    /// `Events` frames per subscription per pump.
    pub max_frames_per_pump: usize,
    /// WebSocket ping interval (ms of silence before a ping); 0 disables.
    pub ws_ping_interval: Duration,
    /// The workspace the MCP endpoint serves: the `scope` every tool call names. A valid MCP scope
    /// path (at most 128 bytes of `[A-Za-z0-9_-]` segments joined by `/`).
    pub mcp_workspace: String,
    /// Extra `Host` names `POST /mcp` answers to, beyond the bound address and `localhost` on the bound
    /// port (`--gateway-allowed-host`). At most [`MAX_ALLOWED_HOSTS`]; none by default.
    pub allowed_hosts: Vec<AllowedHost>,
    /// How long a `POST /mcp` connection keeps reading after its response, so the answer is not
    /// destroyed by a reset, `1 ms..=60 s`. Only tests change it (to hold a lingering connection
    /// without relying on a short window).
    #[doc(hidden)]
    pub mcp_linger: Duration,
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            address: SocketAddr::from(([127, 0, 0, 1], 5433)),
            tls: None,
            max_connections: 32,
            max_handshakes: 8,
            max_connections_per_principal: 8,
            reserved_connections: 4,
            max_long_polls_per_principal: 4,
            max_long_polls: 16,
            handshake_timeout: Duration::from_secs(5),
            max_head: websocket::DEFAULT_MAX_HEAD,
            origins: OriginPolicy::NoBrowsers,
            auth: GatewayAuth::LocalDevelopment,
            write_timeout: Duration::from_secs(5),
            response_timeout: Duration::from_secs(15),
            slice: Duration::from_millis(20),
            fallback_poll: Duration::from_secs(1),
            revalidate_every: Duration::from_secs(5),
            credential_poll: Duration::from_secs(1),
            failure_limit: 10,
            failure_window: Duration::from_secs(60),
            max_subscriptions: 16,
            batch_events: 128,
            max_frames_per_pump: 2,
            ws_ping_interval: Duration::from_secs(30),
            mcp_workspace: "default".to_owned(),
            allowed_hosts: Vec::new(),
            mcp_linger: CLOSE_GRACE,
        }
    }
}

impl GatewayConfig {
    fn validate(&self) -> io::Result<()> {
        let bad = |why: &'static str| Err(io::Error::new(io::ErrorKind::InvalidInput, why));
        let usable = |d: Duration| !d.is_zero() && Instant::now().checked_add(d).is_some();
        // Plaintext is for loopback development only (ADR 0027, issue #578); the same check serves the
        // command line, so the library and the binary refuse the same settings.
        let tokens = !matches!(self.auth, GatewayAuth::LocalDevelopment);
        check_gateway_exposure(self.address, self.tls.is_some(), tokens)
            .map_err(|why| io::Error::new(io::ErrorKind::InvalidInput, why))?;
        check_allowed_hosts(self.address, &self.allowed_hosts)
            .map_err(|why| io::Error::new(io::ErrorKind::InvalidInput, why))?;
        if self.max_connections == 0 || self.max_connections > 1024 {
            return bad("gateway connections must be 1..=1024");
        }
        if self.max_handshakes == 0 || self.max_handshakes > self.max_connections {
            return bad("gateway handshakes must be 1..=max connections");
        }
        if !(1..=1024).contains(&self.max_connections_per_principal) {
            return bad("gateway connections per principal must be 1..=1024");
        }
        if self.reserved_connections > 1024 {
            return bad("gateway reserved connections must be 0..=1024");
        }
        if !(1..=1024).contains(&self.max_long_polls_per_principal)
            || !(1..=1024).contains(&self.max_long_polls)
        {
            return bad("gateway long polls must be 1..=1024");
        }
        if !(MIN_HEAD..=65_536).contains(&self.max_head) {
            return bad("gateway request head limit must be 512..=65536 bytes");
        }
        if !usable(self.handshake_timeout)
            || !usable(self.write_timeout)
            || !usable(self.response_timeout)
            || !usable(self.fallback_poll)
            || !usable(self.revalidate_every)
            || !usable(self.failure_window)
        {
            return bad("gateway timeouts must be nonzero and bounded");
        }
        if self.slice.is_zero() || self.slice > Duration::from_secs(1) {
            return bad("gateway slice must be 1 ms to 1 s");
        }
        if self.credential_poll < Duration::from_millis(1)
            || self.credential_poll > Duration::from_secs(5)
        {
            return bad("gateway credential poll must be 1 ms to 5 s");
        }
        if self.mcp_linger.is_zero() || self.mcp_linger > Duration::from_secs(60) {
            return bad("gateway MCP linger must be 1 ms to 60 s");
        }
        if self.failure_limit == 0 {
            return bad("gateway failure limit must be positive");
        }
        if !lattice_sdk::mcp::is_valid_scope(&self.mcp_workspace) {
            return bad("gateway MCP workspace must be a valid MCP scope path");
        }
        // The host validates the rest when a connection is made; check it now so a bad value fails
        // at start, not on the first client.
        SubscriptionHost::local_development(host_config(self), 0)
            .map(|_| ())
            .map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("invalid gateway setting: {}", e.0),
                )
            })
    }
}

fn host_config(cfg: &GatewayConfig) -> HostConfig {
    let mut host = HostConfig::new(concat!("latticed ", env!("CARGO_PKG_VERSION")));
    host.max_payload = INBOUND_PAYLOAD;
    host.max_subscriptions = cfg.max_subscriptions;
    host.batch_events = cfg.batch_events;
    host.max_frames_per_pump = cfg.max_frames_per_pump;
    host
}

/// Refuses a gateway address that would serve connections without TLS or without bearer tokens
/// (ADR 0027, issue #578). Only a loopback address may serve plaintext or the development principal.
/// Loopback is classified like the PostgreSQL listener does, with `IpAddr::is_loopback`: `127.0.0.0/8`
/// and `::1` only. An IPv4-mapped address such as `[::ffff:127.0.0.1]` is not loopback here.
/// Anywhere else the gateway needs both `tls` (a certificate and key were given) and `tokens` (bearer
/// tokens are used). The error names what is missing, so the command line can report it too.
pub fn check_gateway_exposure(address: SocketAddr, tls: bool, tokens: bool) -> Result<(), String> {
    if address.ip().is_loopback() || (tls && tokens) {
        return Ok(());
    }
    let mut missing = Vec::new();
    if !tls {
        missing.push("--tls-cert FILE and --tls-key FILE");
    }
    if !tokens {
        missing.push("--gateway-tokens FILE");
    }
    Err(format!(
        "{address} is not a loopback address: a sync gateway there needs TLS and bearer tokens; missing {}",
        missing.join(" and ")
    ))
}

// ---------------------------------------------------------------------------------------------
// Pure rules (unit tested; the planted bugs live here because they are switched per thread)
// ---------------------------------------------------------------------------------------------

/// Why the request target is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TargetRefusal {
    /// A query string: a client that sends its token there is told it is not accepted.
    Query,
    /// Not the sync route.
    NotFound,
}

/// The target must be [`SYNC_PATH`] and nothing else.
fn check_target(target: &str) -> Result<(), TargetRefusal> {
    let (route, query) = match target.split_once('?') {
        Some((route, query)) => (route, Some(query)),
        None => (target, None),
    };
    if query.is_some() && !bugs::active(Bug::GatewayQueryStringAccepted) {
        return Err(TargetRefusal::Query);
    }
    if route != SYNC_PATH {
        return Err(TargetRefusal::NotFound);
    }
    Ok(())
}

/// The request carries more than one `Authorization` header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DuplicateAuthorization;

/// The value of the single `Authorization` header in a request head that the codec already checked
/// (CRLF lines, ASCII, no folding). `Ok(None)` when there is none.
fn authorization(head: &[u8]) -> Result<Option<&[u8]>, DuplicateAuthorization> {
    let mut found: Option<&[u8]> = None;
    for line in head.split(|b| *b == b'\n').skip(1) {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let Some(colon) = line.iter().position(|b| *b == b':') else {
            continue;
        };
        let (name, value) = line.split_at(colon);
        if !name.eq_ignore_ascii_case(b"authorization") {
            continue;
        }
        let value = value.get(1..).unwrap_or_default();
        let value = value.trim_ascii();
        if found.is_some() {
            if bugs::active(Bug::GatewayDuplicateAuthorizationAccepted) {
                // Planted defect: the first header silently wins.
                return Ok(found);
            }
            return Err(DuplicateAuthorization);
        }
        found = Some(value);
    }
    Ok(found)
}

/// A refused authentication. Every cause looks the same to the client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AuthRefusal {
    /// The address used up its failures; nothing was verified.
    Blocked,
    /// The credential did not verify.
    Unauthorized,
}

/// What the upgrade authenticated.
enum Identity {
    /// The fixed development principal.
    Development,
    /// An account, and the header it presented (kept to check it again later).
    Account(Principal, Zeroizing<Vec<u8>>),
}

/// Failed attempts per address.
struct AuthGate {
    limiter: Mutex<FailureLimiter>,
}

fn locked<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl AuthGate {
    fn new(limit: usize, window: Duration) -> Self {
        let window_ms = u64::try_from(window.as_millis()).unwrap_or(u64::MAX);
        Self {
            limiter: Mutex::new(FailureLimiter::new(limit, window_ms)),
        }
    }

    fn fail(&self, address: IpAddr, now_ms: u64) {
        if bugs::active(Bug::GatewayAuthFailureNotCounted) {
            return;
        }
        locked(&self.limiter).record_failure(address, now_ms);
    }

    /// Authenticates one upgrade against `store`.
    fn authenticate(
        &self,
        header: Result<Option<&[u8]>, DuplicateAuthorization>,
        store: &CredentialStore,
        address: IpAddr,
        now_ms: u64,
        now_secs: u64,
    ) -> Result<Identity, AuthRefusal> {
        if locked(&self.limiter).is_blocked(address, now_ms) {
            return Err(AuthRefusal::Blocked);
        }
        let verified = match header {
            Ok(header) => verify_bearer(header, store, now_secs).map_err(|_| ()),
            Err(DuplicateAuthorization) => Err(()),
        };
        match (verified, header) {
            (Ok(principal), Ok(Some(value))) => {
                locked(&self.limiter).record_success(address);
                Ok(Identity::Account(principal, Zeroizing::new(value.to_vec())))
            }
            _ => {
                self.fail(address, now_ms);
                Err(AuthRefusal::Unauthorized)
            }
        }
    }
}

/// Whether the header a live connection presented still authenticates the same account.
fn still_valid(header: &[u8], account: &Principal, store: &CredentialStore, now_secs: u64) -> bool {
    if bugs::active(Bug::GatewayRevalidationSkipped) {
        return true;
    }
    verify_bearer(Some(header), store, now_secs).is_ok_and(|p| &p == account)
}

/// Why a connection was not admitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Admission {
    Connections,
    Handshakes,
}

/// Counts live and still-upgrading connections.
struct Counters {
    live: AtomicUsize,
    handshaking: AtomicUsize,
    max_live: usize,
    max_handshaking: usize,
}

impl Counters {
    fn new(max_live: usize, max_handshaking: usize) -> Arc<Self> {
        Arc::new(Self {
            live: AtomicUsize::new(0),
            handshaking: AtomicUsize::new(0),
            max_live,
            max_handshaking,
        })
    }

    fn take(counter: &AtomicUsize, max: usize) -> bool {
        counter
            .try_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < max || bugs::active(Bug::GatewayConnectionCapIgnored)).then(|| n + 1)
            })
            .is_ok()
    }

    fn enter(self: &Arc<Self>) -> Result<Slot, Admission> {
        if !Self::take(&self.live, self.max_live) {
            return Err(Admission::Connections);
        }
        if !Self::take(&self.handshaking, self.max_handshaking) {
            self.live.fetch_sub(1, Ordering::AcqRel);
            return Err(Admission::Handshakes);
        }
        Ok(Slot {
            counters: Arc::clone(self),
            handshaking: true,
        })
    }
}

/// One admitted connection; dropping it frees its places.
struct Slot {
    counters: Arc<Counters>,
    handshaking: bool,
}

impl Slot {
    fn upgraded(&mut self) {
        if std::mem::take(&mut self.handshaking) {
            self.counters.handshaking.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.upgraded();
        self.counters.live.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Why a connection or a long poll was refused after the principal was known.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LimitRefusal {
    /// The principal holds its share of connections already.
    PerPrincipal,
    /// The open connections reached the part kept for principals that hold none.
    Reserved,
    /// The principal has its share of long polls waiting.
    PollsPerPrincipal,
    /// The gateway has its share of long polls waiting.
    PollsGlobal,
}

/// How many requests the gateway refused because of its limits, since it started. Each was answered
/// `503`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LimitRefusals {
    /// Refused at accept, before any token was read: `max_connections` or `max_handshakes` was used up.
    pub global: u64,
    /// Refused because the principal held `max_connections_per_principal` already.
    pub per_principal: u64,
    /// Refused because only the reserved places were left.
    pub reserved_share: u64,
    /// Long polls refused (the per-principal or the global cap).
    pub long_polls: u64,
}

/// What one principal holds.
#[derive(Default)]
struct Held {
    connections: usize,
    polls: usize,
}

#[derive(Default)]
struct QuotaState {
    /// Only principals that hold something: an entry is removed when its last holding is released, so
    /// the map is never larger than the number of open connections.
    held: BTreeMap<String, Held>,
    polls: usize,
}

/// Per-principal connection and long-poll limits (issue #529, part 1). They are counted apart from
/// [`Counters`], which sees no principal, and apart from the PostgreSQL listener's registry.
struct Quotas {
    per_principal: usize,
    reserved: usize,
    max_live: usize,
    polls_per_principal: usize,
    max_polls: usize,
    state: Mutex<QuotaState>,
    refused_per_principal: AtomicU64,
    refused_reserved: AtomicU64,
    refused_polls: AtomicU64,
    refused_global: AtomicU64,
}

impl Quotas {
    fn new(cfg: &GatewayConfig) -> Arc<Self> {
        Arc::new(Self {
            per_principal: cfg.max_connections_per_principal,
            reserved: cfg.reserved_connections,
            max_live: cfg.max_connections,
            polls_per_principal: cfg.max_long_polls_per_principal,
            max_polls: cfg.max_long_polls,
            state: Mutex::new(QuotaState::default()),
            refused_per_principal: AtomicU64::new(0),
            refused_reserved: AtomicU64::new(0),
            refused_polls: AtomicU64::new(0),
            refused_global: AtomicU64::new(0),
        })
    }

    /// Takes one connection for `who`. `live` is the number of connections open now, this one
    /// included. It is read outside the lock, so concurrent upgrades may overshoot the reserved
    /// boundary by the number of upgrades in flight, never the global cap.
    fn enter(self: &Arc<Self>, who: &str, live: usize) -> Result<Share, LimitRefusal> {
        let mut state = locked(&self.state);
        let held = state.held.get(who).map_or(0, |h| h.connections);
        if !bugs::active(Bug::GatewayConnectionCapIgnored) {
            if held >= self.per_principal {
                self.refused_per_principal.fetch_add(1, Ordering::Relaxed);
                return Err(LimitRefusal::PerPrincipal);
            }
            if held > 0 && live > self.max_live.saturating_sub(self.reserved) {
                self.refused_reserved.fetch_add(1, Ordering::Relaxed);
                return Err(LimitRefusal::Reserved);
            }
        }
        state.held.entry(who.to_owned()).or_default().connections += 1;
        Ok(Share {
            quotas: Arc::clone(self),
            who: who.to_owned(),
        })
    }

    /// Takes one long-poll place for `who`.
    fn poll(self: &Arc<Self>, who: &str) -> Result<PollPlace, LimitRefusal> {
        let mut state = locked(&self.state);
        let mine = state.held.get(who).map_or(0, |h| h.polls);
        let refusal = if mine >= self.polls_per_principal {
            Some(LimitRefusal::PollsPerPrincipal)
        } else if state.polls >= self.max_polls {
            Some(LimitRefusal::PollsGlobal)
        } else {
            None
        };
        if let Some(refusal) = refusal
            && !bugs::active(Bug::GatewayConnectionCapIgnored)
        {
            self.refused_polls.fetch_add(1, Ordering::Relaxed);
            return Err(refusal);
        }
        state.polls += 1;
        state.held.entry(who.to_owned()).or_default().polls += 1;
        Ok(PollPlace {
            quotas: Arc::clone(self),
            who: who.to_owned(),
        })
    }

    fn release(&self, who: &str, connection: bool) {
        let mut state = locked(&self.state);
        if !connection {
            state.polls = state.polls.saturating_sub(1);
        }
        let empty = state.held.get_mut(who).is_some_and(|h| {
            if connection {
                h.connections = h.connections.saturating_sub(1);
            } else {
                h.polls = h.polls.saturating_sub(1);
            }
            h.connections == 0 && h.polls == 0
        });
        if empty {
            state.held.remove(who);
        }
    }

    fn refusals(&self) -> LimitRefusals {
        LimitRefusals {
            global: self.refused_global.load(Ordering::Relaxed),
            per_principal: self.refused_per_principal.load(Ordering::Relaxed),
            reserved_share: self.refused_reserved.load(Ordering::Relaxed),
            long_polls: self.refused_polls.load(Ordering::Relaxed),
        }
    }
}

/// One connection of one principal; dropping it (on any exit, a panic included) gives it back.
struct Share {
    quotas: Arc<Quotas>,
    who: String,
}

impl Drop for Share {
    fn drop(&mut self) {
        self.quotas.release(&self.who, true);
    }
}

/// One waiting long poll; dropping it gives the place back.
struct PollPlace {
    quotas: Arc<Quotas>,
    who: String,
}

impl Drop for PollPlace {
    fn drop(&mut self) {
        self.quotas.release(&self.who, false);
    }
}

/// The key a principal is limited under. The development principal and an account cannot collide:
/// an account prints as `account:NAME`.
fn principal_key(principal: &Principal) -> String {
    principal.to_string()
}

/// The key of the fixed development principal.
const DEVELOPMENT_KEY: &str = "local-development";

/// What a limit's `503` says. It names the limit, which is the caller's own, and nothing else.
fn limit_message(refusal: LimitRefusal) -> &'static str {
    match refusal {
        LimitRefusal::PerPrincipal => "this account has too many connections",
        LimitRefusal::Reserved => "the gateway is full",
        LimitRefusal::PollsPerPrincipal => "this account has too many long polls waiting",
        LimitRefusal::PollsGlobal => "the gateway has too many long polls waiting",
    }
}

fn http_response(status: u16, phrase: &str, extra: &str, body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {status} {phrase}\r\n{extra}Connection: close\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

fn unauthorized() -> Vec<u8> {
    http_response(
        401,
        "Unauthorized",
        "WWW-Authenticate: Bearer\r\n",
        "authentication required",
    )
}

// ---------------------------------------------------------------------------------------------
// The listener
// ---------------------------------------------------------------------------------------------

struct Shared {
    cfg: GatewayConfig,
    credentials: Mutex<Arc<CredentialStore>>,
    /// Bumped after every store change; a live connection re-checks its token when it moves.
    generation: AtomicU64,
    /// The credential file, for [`GatewayAuth::BearerFile`].
    watch: Option<Mutex<CredentialWatch>>,
    bearer: bool,
    counters: Arc<Counters>,
    quotas: Arc<Quotas>,
    gate: AuthGate,
    epoch: Instant,
    stopping: AtomicBool,
    wake: Arc<dyn Wake>,
    /// The `Host` values `POST /mcp` answers to, resolved against the bound port.
    allowed: Vec<String>,
    /// The MCP tool backends, when the endpoint is served.
    mcp: Option<Arc<dyn McpBackends>>,
}

impl Shared {
    fn now_ms(&self) -> u64 {
        u64::try_from(self.epoch.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    fn store(&self) -> Arc<CredentialStore> {
        Arc::clone(&locked(&self.credentials))
    }

    fn install(&self, store: CredentialStore) {
        *locked(&self.credentials) = Arc::new(store);
        self.generation.fetch_add(1, Ordering::AcqRel);
    }

    /// Looks at the credential file once its poll interval has passed and applies what it found. The
    /// accept thread calls this on every turn, so a stopped gateway stops reloading too.
    // The stderr line is the only report of a reload until the security event log (#530) exists. It
    // names the file and the reason, never a token.
    #[allow(clippy::print_stderr)]
    fn reload_credentials(&self) {
        let Some(watch) = &self.watch else {
            return;
        };
        let outcome = locked(watch).check(Instant::now(), crate::stop::reload_signals_received());
        match outcome {
            None => {}
            Some(Reload::Loaded(store)) => {
                self.install(store);
                let path = locked(watch).path().display().to_string();
                eprintln!("latticed: bearer-token file {path} reloaded");
            }
            // The reason is an I/O or content error that names no token; the operator sees it here
            // until the security event log (issue #530) exists.
            Some(Reload::Kept(error)) => {
                let path = locked(watch).path().display().to_string();
                eprintln!(
                    "latticed: bearer-token file {path} did not load, keeping the tokens in use: {error}"
                );
            }
            Some(Reload::Refused) => {
                self.install(CredentialStore::empty());
                let path = locked(watch).path().display().to_string();
                eprintln!(
                    "latticed: bearer-token file {path} has not loaded for ten minutes; refusing every bearer token until it does"
                );
            }
        }
    }
}

fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// A running gateway. Shutting down or dropping it closes every connection and joins every thread.
pub struct GatewayHandle {
    address: SocketAddr,
    shared: Arc<Shared>,
    acceptor: Option<JoinHandle<io::Result<()>>>,
}

impl GatewayHandle {
    /// Binds before returning, so a bind or configuration error never hides in a thread.
    ///
    /// `source` makes one [`EventSource`] per connection (for the server, the executor's feed
    /// source). `wake` tells connections when to read; [`NoWake`] means the fallback interval only.
    pub fn start<S, F>(config: GatewayConfig, source: F, wake: Arc<dyn Wake>) -> io::Result<Self>
    where
        S: EventSource + Send + 'static,
        F: Fn() -> S + Send + Sync + 'static,
    {
        Self::start_with_mcp(config, source, wake, None)
    }

    /// As [`GatewayHandle::start`], and when `mcp` is given also serves `POST /mcp` (issue #527): each
    /// call takes its tool backend from that factory, for the principal the bearer token verified.
    pub fn start_with_mcp<S, F>(
        config: GatewayConfig,
        source: F,
        wake: Arc<dyn Wake>,
        mcp: Option<Arc<dyn McpBackends>>,
    ) -> io::Result<Self>
    where
        S: EventSource + Send + 'static,
        F: Fn() -> S + Send + Sync + 'static,
    {
        config.validate()?;
        let (store, bearer, watch) = match &config.auth {
            GatewayAuth::LocalDevelopment => (CredentialStore::empty(), false, None),
            GatewayAuth::Bearer(store) => (store.clone(), true, None),
            GatewayAuth::BearerFile(path) => {
                let (watch, store) = CredentialWatch::open(
                    path,
                    config.credential_poll,
                    REFUSE_AFTER,
                    crate::stop::reload_signals_received(),
                )
                .map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", path.display())))?;
                (store, true, Some(Mutex::new(watch)))
            }
        };
        let listener = TcpListener::bind(config.address)?;
        listener.set_nonblocking(true)?;
        let address = listener.local_addr()?;
        let allowed = mcp_http::allowed_hosts(address, &config.allowed_hosts)
            .map_err(|why| io::Error::new(io::ErrorKind::InvalidInput, why))?;
        let shared = Arc::new(Shared {
            counters: Counters::new(config.max_connections, config.max_handshakes),
            quotas: Quotas::new(&config),
            gate: AuthGate::new(config.failure_limit, config.failure_window),
            credentials: Mutex::new(Arc::new(store)),
            generation: AtomicU64::new(0),
            watch,
            bearer,
            epoch: Instant::now(),
            stopping: AtomicBool::new(false),
            wake,
            allowed,
            mcp,
            cfg: config,
        });
        let state = Arc::clone(&shared);
        let acceptor = thread::Builder::new()
            .name("ws-accept".into())
            .spawn(move || accept(&listener, &state, &Arc::new(source)))?;
        Ok(Self {
            address,
            shared,
            acceptor: Some(acceptor),
        })
    }

    /// The bound address (the chosen port when port zero was asked for).
    pub fn local_addr(&self) -> SocketAddr {
        self.address
    }

    /// Connections admitted now, including those still upgrading.
    pub fn open_connections(&self) -> usize {
        self.shared.counters.live.load(Ordering::Acquire)
    }

    /// `lattice_subscribe` long polls waiting now.
    pub fn long_polls_waiting(&self) -> usize {
        locked(&self.shared.quotas.state).polls
    }

    /// Requests refused by the connection and long-poll limits since the gateway started.
    pub fn limit_refusals(&self) -> LimitRefusals {
        self.shared.quotas.refusals()
    }

    /// True once the accept thread ended, which only follows a shutdown or a fatal listener error.
    pub fn is_finished(&self) -> bool {
        self.acceptor.as_ref().is_none_or(JoinHandle::is_finished)
    }

    /// Replaces the credential store. Live connections are checked against it at once: a token that
    /// no longer verifies for its account closes the connection with 1008. Returns false (and changes
    /// nothing) when the gateway does not use bearer tokens.
    pub fn replace_credentials(&self, store: CredentialStore) -> bool {
        if !self.shared.bearer {
            return false;
        }
        self.shared.install(store);
        true
    }

    /// Stops accepting, tells every connection to close (1001) and waits for all threads.
    pub fn shutdown(mut self) -> io::Result<()> {
        self.stop_and_join()
    }

    fn stop_and_join(&mut self) -> io::Result<()> {
        self.shared.stopping.store(true, Ordering::Release);
        match self.acceptor.take() {
            Some(acceptor) => acceptor
                .join()
                .map_err(|_| io::Error::other("gateway acceptor panicked"))?,
            None => Ok(()),
        }
    }
}

impl Drop for GatewayHandle {
    fn drop(&mut self) {
        let _ = self.stop_and_join();
    }
}

struct Worker {
    socket: TcpStream,
    thread: JoinHandle<()>,
}

fn accept<S, F>(listener: &TcpListener, shared: &Arc<Shared>, source: &Arc<F>) -> io::Result<()>
where
    S: EventSource + Send + 'static,
    F: Fn() -> S + Send + Sync + 'static,
{
    let mut workers: Vec<Worker> = Vec::new();
    let mut failures = 0u32;
    let result = loop {
        if shared.stopping.load(Ordering::Acquire) {
            break Ok(());
        }
        shared.reload_credentials();
        workers.retain_mut(|worker| {
            if worker.thread.is_finished() {
                // A panicked connection ends only itself.
                return false;
            }
            true
        });
        match listener.accept() {
            Ok((socket, peer)) => {
                failures = 0;
                let slot = match shared.counters.enter() {
                    Ok(slot) => slot,
                    Err(_) => {
                        shared.quotas.refused_global.fetch_add(1, Ordering::Relaxed);
                        refuse(&socket, 503, "Service Unavailable", "the gateway is full");
                        continue;
                    }
                };
                let Ok(control) = socket.try_clone() else {
                    continue;
                };
                let state = Arc::clone(shared);
                let make = Arc::clone(source);
                let spawned = thread::Builder::new()
                    .name("ws-conn".into())
                    .spawn(move || serve(socket, peer.ip(), slot, &state, &*make));
                if let Ok(thread) = spawned {
                    workers.push(Worker {
                        socket: control,
                        thread,
                    });
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(5));
            }
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::Interrupted
                        | io::ErrorKind::ConnectionAborted
                        | io::ErrorKind::ConnectionReset
                ) => {}
            Err(e) => {
                // Descriptor exhaustion and similar are transient; a listener that never recovers
                // is fatal after about ten seconds.
                failures += 1;
                if failures >= 200 {
                    break Err(e);
                }
                thread::sleep(Duration::from_millis(50));
            }
        }
    };
    shared.stopping.store(true, Ordering::Release);
    // Connections see the flag within one slice, send 1001 and finish; one stuck in a write to a
    // client that does not read is cut after a grace period instead of its full write budget.
    let grace = Instant::now() + Duration::from_secs(2);
    while Instant::now() < grace && workers.iter().any(|w| !w.thread.is_finished()) {
        thread::sleep(Duration::from_millis(5));
    }
    for worker in &workers {
        if !worker.thread.is_finished() {
            let _ = worker.socket.shutdown(Shutdown::Both);
        }
    }
    let mut result = result;
    for worker in workers {
        if worker.thread.join().is_err() && result.is_ok() {
            result = Err(io::Error::other("gateway connection panicked"));
        }
    }
    result
}

/// Answers a connection that is not admitted: one short non-blocking write, no waiting.
fn refuse(socket: &TcpStream, status: u16, phrase: &str, body: &str) {
    let mut socket = socket;
    let _ = socket.set_write_timeout(Some(Duration::from_millis(100)));
    let _ = socket.write_all(&http_response(status, phrase, "", body));
    let _ = socket.shutdown(Shutdown::Both);
}

// ---------------------------------------------------------------------------------------------
// One connection
// ---------------------------------------------------------------------------------------------

/// Writes all of `bytes` and flushes it (a TLS session buffers plaintext until it is flushed), all
/// within one absolute `deadline`. Every write call is bounded by the transport's write timeout.
fn write_all_by(socket: &mut Transport, bytes: &[u8], deadline: Instant) -> io::Result<()> {
    socket.set_response_deadline(Some(deadline));
    let result = socket.write_all(bytes).and_then(|()| socket.flush());
    socket.set_response_deadline(None);
    result
}

fn serve<S: EventSource>(
    socket: TcpStream,
    address: IpAddr,
    mut slot: Slot,
    shared: &Shared,
    make: &dyn Fn() -> S,
) {
    // A socket accepted from a non-blocking listener may inherit that mode; every wait here is a
    // timeout on a blocking socket.
    if socket.set_nonblocking(false).is_err() {
        return;
    }
    let _ = socket.set_nodelay(true);
    // One absolute budget covers the TLS handshake, the request head and the token check. A stalled
    // handshake is cut here, and the place it held is released when this function returns.
    let Some(deadline) = Instant::now().checked_add(shared.cfg.handshake_timeout) else {
        return;
    };
    let mut socket = Transport::new(socket, deadline, shared.cfg.write_timeout);
    let Some((identity, leftover, _share)) =
        upgrade(&mut socket, deadline, address, shared, &mut slot)
    else {
        let _ = socket.shutdown();
        return;
    };
    socket.authenticated();
    slot.upgraded();
    let now_s = shared.epoch.elapsed().as_secs();
    let cfg = host_config(&shared.cfg);
    let host = match &identity {
        Identity::Development => SubscriptionHost::local_development(cfg, now_s),
        Identity::Account(principal, _) => SubscriptionHost::new(cfg, now_s, principal.clone()),
    };
    let Ok(host) = host else {
        let _ = socket.shutdown();
        return;
    };
    let ws = Connection::new(
        WsConfig {
            max_frame_payload: usize::try_from(INBOUND_PAYLOAD).unwrap_or(0) + HEADER_LEN,
            allow_text: false,
            ping_interval: u64::try_from(shared.cfg.ws_ping_interval.as_millis())
                .unwrap_or(u64::MAX),
            ..WsConfig::default()
        },
        shared.now_ms(),
    );
    let credential = match identity {
        Identity::Development => None,
        Identity::Account(_, header) => Some(header),
    };
    let mut live = Live {
        socket,
        ws,
        host,
        source: make(),
        inbuf: leftover,
        out: Vec::new(),
        credential,
        shared,
        seen: 0,
        last_poll: Instant::now(),
        last_check: Instant::now(),
        checked_generation: None,
        deadline: None,
    };
    live.run();
    // Every exit that can still write sends `close_notify` on a TLS session first (a no-op when
    // `finish` already did), then shuts the socket down.
    let _ = live.socket.shutdown();
}

/// Reads the request, answers refusals, and on success writes the `101`. `None` means the socket
/// was dropped (a refusal was already written when there was one to write). With TLS configured the
/// handshake runs first, under the same `deadline`; a plaintext client fails it and gets nothing.
fn upgrade(
    socket: &mut Transport,
    deadline: Instant,
    address: IpAddr,
    shared: &Shared,
    slot: &mut Slot,
) -> Option<(Identity, Vec<u8>, Share)> {
    let cfg = &shared.cfg;
    if let Some(tls) = &cfg.tls {
        socket.upgrade_in_place(tls).ok()?;
    }
    let reply = |socket: &mut Transport, bytes: &[u8]| {
        let until = Instant::now() + Duration::from_millis(500);
        let _ = write_all_by(socket, bytes, until);
    };
    let codec = UpgradeConfig {
        max_head: cfg.max_head,
        origins: cfg.origins.clone(),
        require_subprotocol: true,
    };
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 2048];
    let (found, used) = loop {
        if shared.stopping.load(Ordering::Acquire) {
            return None;
        }
        // The request line decides the route before the WebSocket parser sees any byte, so `POST /mcp`
        // is never refused as a non-GET upgrade.
        let parsed = match mcp_http::route(&buf, shared.mcp.is_some()) {
            mcp_http::Route::Mcp => {
                mcp_http::answer(socket, buf, deadline, address, shared, slot);
                return None;
            }
            mcp_http::Route::Pending if buf.len() >= cfg.max_head => {
                reply(
                    socket,
                    &http_response(
                        431,
                        "Request Header Fields Too Large",
                        "",
                        "the request head is too large",
                    ),
                );
                return None;
            }
            mcp_http::Route::Pending => Ok(None),
            mcp_http::Route::Sync => websocket::parse_upgrade(&buf, &codec),
        };
        match parsed {
            Ok(Some((upgrade, used))) => break (upgrade, used),
            Ok(None) => {}
            Err(refused) => {
                reply(socket, &refused.response());
                return None;
            }
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            // A slow request head is cut off without a reply: it may not be a client at all.
            return None;
        }
        socket.set_deadline(Instant::now() + cfg.slice.min(left));
        match socket.read(&mut chunk) {
            Ok(0) => return None,
            Ok(n) => buf.extend_from_slice(chunk.get(..n)?),
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) => {}
            Err(_) => return None,
        }
    };
    let head = buf.get(..used)?;
    match check_target(&found.path) {
        Ok(()) => {}
        Err(TargetRefusal::Query) => {
            reply(
                socket,
                &http_response(
                    400,
                    "Bad Request",
                    "",
                    "the request target must not have a query string; send the token in the Authorization header",
                ),
            );
            return None;
        }
        Err(TargetRefusal::NotFound) => {
            reply(socket, &http_response(404, "Not Found", "", "not found"));
            return None;
        }
    }
    let identity = if shared.bearer {
        let store = shared.store();
        match shared.gate.authenticate(
            authorization(head),
            &store,
            address,
            shared.now_ms(),
            unix_secs(),
        ) {
            Ok(identity) => identity,
            Err(AuthRefusal::Unauthorized) => {
                reply(socket, &unauthorized());
                return None;
            }
            Err(AuthRefusal::Blocked) => {
                reply(
                    socket,
                    &http_response(
                        429,
                        "Too Many Requests",
                        "Retry-After: 60\r\n",
                        "too many failed attempts",
                    ),
                );
                return None;
            }
        }
    } else {
        Identity::Development
    };
    // The principal is known now. A refusal here is still before the `101`. The share is dropped on
    // every path out of this function that does not return it.
    let who = match &identity {
        Identity::Development => DEVELOPMENT_KEY.to_owned(),
        Identity::Account(principal, _) => principal_key(principal),
    };
    let share = match shared
        .quotas
        .enter(&who, shared.counters.live.load(Ordering::Acquire))
    {
        Ok(share) => share,
        Err(refusal) => {
            reply(
                socket,
                &http_response(
                    503,
                    "Service Unavailable",
                    "Retry-After: 1\r\n",
                    limit_message(refusal),
                ),
            );
            return None;
        }
    };
    let response = found.response();
    if write_all_by(socket, &response, deadline).is_err() {
        return None;
    }
    let leftover = buf.get(used..).unwrap_or_default().to_vec();
    Some((identity, leftover, share))
}

enum Flow {
    Continue,
    End,
}

struct Live<'a, S: EventSource> {
    socket: Transport,
    ws: Connection,
    host: SubscriptionHost,
    source: S,
    inbuf: Vec<u8>,
    out: Vec<u8>,
    credential: Option<Zeroizing<Vec<u8>>>,
    shared: &'a Shared,
    seen: u64,
    last_poll: Instant,
    last_check: Instant,
    /// The store generation the last credential check ran against; `None` until the first check, so
    /// a connection is checked as soon as it starts.
    checked_generation: Option<u64>,
    /// One absolute deadline for everything a step writes; `None` outside a step.
    deadline: Option<Instant>,
}

impl<S: EventSource> Live<'_, S> {
    fn now_s(&self) -> u64 {
        self.shared.epoch.elapsed().as_secs()
    }

    fn flush(&mut self) -> io::Result<()> {
        let cfg = &self.shared.cfg;
        let deadline = self
            .deadline
            .unwrap_or_else(|| Instant::now() + cfg.response_timeout);
        let result = write_all_by(&mut self.socket, &self.out, deadline);
        self.out.clear();
        result
    }

    /// Queues a close, writes it, and gives the peer a moment to read it.
    fn goodbye(&mut self, code: u16, reason: &str) {
        // The close gets its own short budget, so a reader that used up the step's still cannot
        // hold the thread for another full response budget.
        self.deadline = Some(Instant::now() + self.shared.cfg.write_timeout);
        if self.ws.is_open() {
            let _ = self.ws.close(code, reason, self.shared.now_ms());
        }
        self.finish();
    }

    /// Writes what is queued, half-closes, and drains until the peer is gone or a short grace ends,
    /// so a close frame is not destroyed by a reset.
    fn finish(&mut self) {
        self.ws.drain_output(&mut self.out);
        if self.flush().is_err() {
            return;
        }
        let _ = self.socket.half_close();
        let until = Instant::now() + CLOSE_GRACE;
        let mut sink = [0u8; 512];
        while Instant::now() < until {
            self.socket
                .set_deadline(Instant::now() + self.shared.cfg.slice);
            match self.socket.read(&mut sink) {
                Ok(0) => return,
                Ok(_) => {}
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock
                            | io::ErrorKind::TimedOut
                            | io::ErrorKind::Interrupted
                    ) => {}
                Err(_) => return,
            }
        }
    }

    /// Whether the server is stopping or the credential stopped verifying. Checked at the top of
    /// every turn and between the frames of one step, so a slow reader cannot outlast either.
    fn must_close(&mut self) -> Option<(u16, &'static str)> {
        if self.shared.stopping.load(Ordering::Acquire) {
            return Some((close_code::GOING_AWAY, "server stopping"));
        }
        let header = self.credential.as_ref()?;
        // The generation is read before the store, so a change made after this read is seen at the
        // next turn. A store that changed since the last check is checked now, not at the interval.
        let generation = self.shared.generation.load(Ordering::Acquire);
        let changed = self.checked_generation != Some(generation);
        if !changed && self.last_check.elapsed() < self.shared.cfg.revalidate_every {
            return None;
        }
        self.checked_generation = Some(generation);
        self.last_check = Instant::now();
        let store = self.shared.store();
        if still_valid(header, self.host.principal(), &store, unix_secs()) {
            None
        } else {
            Some((close_code::POLICY, "credential no longer valid"))
        }
    }

    fn run(&mut self) {
        let shared = self.shared;
        let cfg = &shared.cfg;
        loop {
            if let Some((code, reason)) = self.must_close() {
                return self.goodbye(code, reason);
            }
            let Some(token) = self.shared.wake.token() else {
                return self.goodbye(close_code::GOING_AWAY, "server stopping");
            };
            if token != self.seen || self.last_poll.elapsed() >= cfg.fallback_poll {
                // The token is read before the poll, so a commit that lands during the poll moves
                // it again and is read on the next turn.
                self.seen = token;
                self.last_poll = Instant::now();
                let now = self.now_s();
                let outputs = self.host.poll(now, &self.source);
                if matches!(self.emit(outputs), Flow::End) {
                    return;
                }
            }
            if let Some(event) = self.ws.tick(self.shared.now_ms()) {
                self.ws.drain_output(&mut self.out);
                let _ = self.flush();
                if matches!(event, WsEvent::TimedOut) {
                    return;
                }
            }
            self.ws.drain_output(&mut self.out);
            if !self.out.is_empty() && self.flush().is_err() {
                return;
            }
            if self.ws.is_done() {
                return;
            }
            if matches!(self.read_some(), Flow::End) {
                return;
            }
        }
    }

    fn read_some(&mut self) -> Flow {
        let cap = usize::try_from(INBOUND_PAYLOAD).unwrap_or(0) + HEADER_LEN + 14;
        if self.inbuf.len() >= cap {
            // A frame that is complete would have been consumed; this one cannot become valid.
            self.goodbye(close_code::TOO_BIG, "frame above the limit");
            return Flow::End;
        }
        let mut chunk = [0u8; 2048];
        let room = (cap - self.inbuf.len()).min(chunk.len());
        self.socket
            .set_deadline(Instant::now() + self.shared.cfg.slice);
        let Some(window) = chunk.get_mut(..room) else {
            return Flow::End;
        };
        match self.socket.read(window) {
            Ok(0) => Flow::End,
            Ok(n) => {
                if let Some(got) = chunk.get(..n) {
                    self.inbuf.extend_from_slice(got);
                }
                self.pump_input()
            }
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) =>
            {
                Flow::Continue
            }
            Err(_) => Flow::End,
        }
    }

    /// Feeds buffered bytes to the WebSocket layer until it needs more.
    fn pump_input(&mut self) -> Flow {
        loop {
            let step = self.ws.receive(&self.inbuf, self.shared.now_ms());
            self.inbuf.drain(..step.consumed.min(self.inbuf.len()));
            match step.event {
                Some(WsEvent::Message(WsMessage::Binary(bytes))) => {
                    if matches!(self.on_frame(&bytes), Flow::End) {
                        return Flow::End;
                    }
                }
                // `allow_text` is false, so the codec turns text into a 1003 protocol error.
                Some(WsEvent::Message(WsMessage::Text(_))) => {
                    self.goodbye(close_code::UNSUPPORTED, "text messages are not accepted");
                    return Flow::End;
                }
                Some(WsEvent::Closed { .. } | WsEvent::ProtocolError { .. }) => {
                    self.finish();
                    return Flow::End;
                }
                Some(WsEvent::TimedOut) => return Flow::End,
                None if step.consumed == 0 => return Flow::Continue,
                None => {}
            }
        }
    }

    /// One binary message is exactly one sync frame.
    fn on_frame(&mut self, bytes: &[u8]) -> Flow {
        let limit = usize::try_from(INBOUND_PAYLOAD).unwrap_or(0);
        match sync::decode_with_limit(bytes, limit) {
            Ok(Some((message, used))) if used == bytes.len() => {
                let now = self.now_s();
                let outputs = self.host.handle(message, now, &self.source);
                self.emit(outputs)
            }
            _ => {
                let error = sync::Message::Error {
                    code: ErrorCode::Malformed,
                    sub: 0,
                    message: "a message must be exactly one sync frame".into(),
                };
                if self.send(&error).is_ok() {
                    self.goodbye(close_code::PROTOCOL, "malformed sync frame");
                }
                Flow::End
            }
        }
    }

    fn send(&mut self, message: &sync::Message) -> io::Result<()> {
        let mut bytes = Vec::new();
        sync::encode(message, &mut bytes).map_err(|_| io::Error::other("unencodable frame"))?;
        self.ws
            .send_binary(&bytes, &mut self.out)
            .map_err(io::Error::other)?;
        // One frame at a time, so a slow reader holds back this thread and nothing else.
        self.flush()
    }

    fn emit(&mut self, outputs: Vec<Output>) -> Flow {
        self.deadline = Some(Instant::now() + self.shared.cfg.response_timeout);
        let flow = self.emit_all(outputs);
        self.deadline = None;
        flow
    }

    fn emit_all(&mut self, outputs: Vec<Output>) -> Flow {
        for output in outputs {
            match output {
                Output::Send(message) => {
                    if let Some((code, reason)) = self.must_close() {
                        self.goodbye(code, reason);
                        return Flow::End;
                    }
                    if self.send(&message).is_err() {
                        return Flow::End;
                    }
                }
                Output::Close(reason) => {
                    let (code, text) = match reason {
                        CloseReason::Refused => (close_code::POLICY, "refused"),
                        CloseReason::ProtocolViolation(_) => (close_code::PROTOCOL, "protocol"),
                        CloseReason::ClientError => (close_code::NORMAL, "client error"),
                        CloseReason::Timeout => (close_code::POLICY, "timeout"),
                    };
                    self.goodbye(code, text);
                    return Flow::End;
                }
            }
        }
        Flow::Continue
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use lattice_api::bearer::issue;
    use lattice_core::bugs;
    use lattice_seams::OsRng;

    fn head(headers: &[&str]) -> Vec<u8> {
        let mut text = String::from("GET /sync HTTP/1.1\r\nHost: x\r\n");
        for h in headers {
            text.push_str(h);
            text.push_str("\r\n");
        }
        text.push_str("\r\n");
        text.into_bytes()
    }

    fn store_with(account: &str) -> (CredentialStore, String) {
        let (token, entry) = issue(account, 1, None, &mut OsRng).unwrap();
        let mut store = CredentialStore::empty();
        store.insert(entry).unwrap();
        (store, token.expose_secret().to_string())
    }

    const ADDR: IpAddr = IpAddr::V4(std::net::Ipv4Addr::LOCALHOST);

    #[test]
    fn the_target_is_the_sync_route_without_a_query() {
        assert_eq!(check_target("/sync"), Ok(()));
        assert_eq!(check_target("/sync?token=abc"), Err(TargetRefusal::Query));
        assert_eq!(check_target("/sync?"), Err(TargetRefusal::Query));
        assert_eq!(check_target("/other"), Err(TargetRefusal::NotFound));
        assert_eq!(check_target("/sync/"), Err(TargetRefusal::NotFound));
        assert_eq!(check_target("/"), Err(TargetRefusal::NotFound));
    }

    #[test]
    fn a_planted_query_string_is_caught_by_the_witness() {
        bugs::clear();
        bugs::enable(Bug::GatewayQueryStringAccepted);
        let caught = check_target("/sync?token=abc") != Err(TargetRefusal::Query);
        bugs::clear();
        assert!(
            caught,
            "the planted bug must make the query-string witness fail"
        );
    }

    #[test]
    fn exactly_one_authorization_header_is_read() {
        let one = head(&["Authorization: Bearer abc"]);
        assert_eq!(authorization(&one), Ok(Some(&b"Bearer abc"[..])));
        let none = head(&["X-Other: 1"]);
        assert_eq!(authorization(&none), Ok(None));
        let spaced = head(&["authorization:   Bearer abc \t"]);
        assert_eq!(authorization(&spaced), Ok(Some(&b"Bearer abc"[..])));
        let two = head(&["Authorization: Bearer a", "authorization: Bearer b"]);
        assert_eq!(authorization(&two), Err(DuplicateAuthorization));
        // A header whose value mentions the name is not the header.
        let trick = head(&["X-Note: Authorization: Bearer a"]);
        assert_eq!(authorization(&trick), Ok(None));
    }

    #[test]
    fn a_planted_first_header_wins_is_caught_by_the_witness() {
        let two = head(&["Authorization: Bearer a", "Authorization: Bearer b"]);
        bugs::clear();
        bugs::enable(Bug::GatewayDuplicateAuthorizationAccepted);
        let caught = authorization(&two) != Err(DuplicateAuthorization);
        bugs::clear();
        assert!(caught);
    }

    #[test]
    fn failures_are_counted_per_address_and_a_blocked_address_is_not_verified() {
        let (store, token) = store_with("alice");
        let gate = AuthGate::new(2, Duration::from_secs(60));
        let bad = Some(&b"Bearer nope"[..]);
        for _ in 0..2 {
            let refused = gate.authenticate(Ok(bad), &store, ADDR, 0, 10);
            assert!(matches!(refused, Err(AuthRefusal::Unauthorized)));
        }
        // Blocked now: even the right token is not looked at.
        let header = format!("Bearer {token}");
        let blocked = gate.authenticate(Ok(Some(header.as_bytes())), &store, ADDR, 1, 10);
        assert!(matches!(blocked, Err(AuthRefusal::Blocked)));
        // Another address is unaffected and succeeds.
        let other = IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 2));
        let ok = gate.authenticate(Ok(Some(header.as_bytes())), &store, other, 1, 10);
        assert!(matches!(ok, Ok(Identity::Account(p, _)) if p.name() == "alice"));
    }

    #[test]
    fn missing_malformed_and_duplicate_credentials_fail_alike_and_count() {
        let (store, _) = store_with("alice");
        let gate = AuthGate::new(3, Duration::from_secs(60));
        let a = gate.authenticate(Ok(None), &store, ADDR, 0, 10);
        let b = gate.authenticate(Ok(Some(b"Basic abc")), &store, ADDR, 0, 10);
        let c = gate.authenticate(Err(DuplicateAuthorization), &store, ADDR, 0, 10);
        for r in [a, b, c] {
            assert!(matches!(r, Err(AuthRefusal::Unauthorized)));
        }
        let d = gate.authenticate(Ok(None), &store, ADDR, 0, 10);
        assert!(matches!(d, Err(AuthRefusal::Blocked)));
    }

    #[test]
    fn a_planted_uncounted_failure_is_caught_by_the_witness() {
        let (store, _) = store_with("alice");
        bugs::clear();
        bugs::enable(Bug::GatewayAuthFailureNotCounted);
        let gate = AuthGate::new(1, Duration::from_secs(60));
        let _ = gate.authenticate(Ok(None), &store, ADDR, 0, 10);
        let second = gate.authenticate(Ok(None), &store, ADDR, 0, 10);
        bugs::clear();
        assert!(
            !matches!(second, Err(AuthRefusal::Blocked)),
            "must be uncounted"
        );
    }

    #[test]
    fn connections_are_capped_and_places_come_back() {
        let counters = Counters::new(2, 1);
        let first = counters.enter().unwrap();
        // The handshake budget is used up while the first still upgrades.
        assert_eq!(counters.enter().err(), Some(Admission::Handshakes));
        let mut first = first;
        first.upgraded();
        let second = counters.enter().unwrap();
        assert_eq!(counters.enter().err(), Some(Admission::Connections));
        drop(second);
        let third = counters.enter().unwrap();
        drop(third);
        drop(first);
        assert_eq!(counters.live.load(Ordering::Acquire), 0);
        assert_eq!(counters.handshaking.load(Ordering::Acquire), 0);
    }

    #[test]
    fn a_planted_ignored_cap_is_caught_by_the_witness() {
        bugs::clear();
        bugs::enable(Bug::GatewayConnectionCapIgnored);
        let counters = Counters::new(1, 1);
        let _a = counters.enter();
        let second = counters.enter();
        bugs::clear();
        assert!(second.is_ok(), "the planted bug must admit past the cap");
    }

    #[test]
    fn a_live_connection_is_valid_only_while_its_token_verifies_for_its_account() {
        let (store, token) = store_with("alice");
        let header = format!("Bearer {token}");
        let alice = match AuthGate::new(5, Duration::from_secs(60)).authenticate(
            Ok(Some(header.as_bytes())),
            &store,
            ADDR,
            0,
            10,
        ) {
            Ok(Identity::Account(p, _)) => p,
            _ => panic!("expected an account"),
        };
        assert!(still_valid(header.as_bytes(), &alice, &store, 10));
        // The token's account is revoked: an empty store, or another account's store, refuses it.
        assert!(!still_valid(
            header.as_bytes(),
            &alice,
            &CredentialStore::empty(),
            10
        ));
        let (other, _) = store_with("bob");
        assert!(!still_valid(header.as_bytes(), &alice, &other, 10));
    }

    #[test]
    fn a_planted_skipped_revalidation_is_caught_by_the_witness() {
        let (store, token) = store_with("alice");
        let header = format!("Bearer {token}");
        let alice = match AuthGate::new(5, Duration::from_secs(60)).authenticate(
            Ok(Some(header.as_bytes())),
            &store,
            ADDR,
            0,
            10,
        ) {
            Ok(Identity::Account(p, _)) => p,
            _ => panic!("expected an account"),
        };
        bugs::clear();
        bugs::enable(Bug::GatewayRevalidationSkipped);
        let still = still_valid(header.as_bytes(), &alice, &CredentialStore::empty(), 10);
        bugs::clear();
        assert!(still, "the planted bug must keep a revoked connection");
    }

    #[test]
    fn a_planted_revoked_acceptance_is_caught_by_the_revalidation_witness() {
        let (token, entry) = issue("alice", 1, None, &mut OsRng).unwrap();
        let mut store = CredentialStore::empty();
        store.insert(entry).unwrap();
        let header = format!("Bearer {}", token.expose_secret());
        let alice = match AuthGate::new(5, Duration::from_secs(60)).authenticate(
            Ok(Some(header.as_bytes())),
            &store,
            ADDR,
            0,
            10,
        ) {
            Ok(Identity::Account(p, _)) => p,
            _ => panic!("expected an account"),
        };
        assert!(store.revoke(token.id()));
        assert!(!still_valid(header.as_bytes(), &alice, &store, 10));
        bugs::clear();
        bugs::enable(Bug::RevokedTokenStillAccepted);
        let still = still_valid(header.as_bytes(), &alice, &store, 10);
        bugs::clear();
        assert!(still, "the planted bug must keep a revoked connection");
    }

    fn test_tls() -> TlsConfig {
        let generated = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        TlsConfig::from_pem(
            generated.cert.pem().as_bytes(),
            generated.signing_key.serialize_pem().as_bytes(),
        )
        .unwrap()
    }

    #[test]
    fn gateway_plaintext_on_loopback_only() {
        let (store, _) = store_with("alice");
        let config = |address: [u8; 4], tls: bool, bearer: bool| GatewayConfig {
            address: SocketAddr::from((address, 0)),
            tls: tls.then(test_tls),
            auth: if bearer {
                GatewayAuth::Bearer(store.clone())
            } else {
                GatewayAuth::LocalDevelopment
            },
            ..GatewayConfig::default()
        };
        // Plaintext development on loopback, with or without tokens, and TLS on loopback too.
        assert!(config([127, 0, 0, 1], false, false).validate().is_ok());
        assert!(config([127, 0, 0, 1], false, true).validate().is_ok());
        assert!(config([127, 0, 0, 1], true, false).validate().is_ok());
        // Anywhere else plaintext is refused, and so is TLS without tokens.
        let refused = config([0, 0, 0, 0], false, true).validate();
        assert!(refused.unwrap_err().to_string().contains("TLS"));
        let no_tokens = config([0, 0, 0, 0], true, false).validate();
        assert!(no_tokens.unwrap_err().to_string().contains("bearer tokens"));
        assert!(config([0, 0, 0, 0], true, true).validate().is_ok());
        assert!(config([10, 1, 2, 3], false, false).validate().is_err());
    }

    #[test]
    fn the_configuration_is_validated_before_binding() {
        let ok = GatewayConfig {
            address: SocketAddr::from(([127, 0, 0, 1], 0)),
            ..GatewayConfig::default()
        };
        assert!(ok.validate().is_ok());
        let remote = GatewayConfig {
            address: SocketAddr::from(([0, 0, 0, 0], 0)),
            ..ok.clone()
        };
        assert!(
            remote
                .validate()
                .unwrap_err()
                .to_string()
                .contains("loopback")
        );
        let cases = [
            GatewayConfig {
                max_connections: 0,
                ..ok.clone()
            },
            GatewayConfig {
                max_connections: 1025,
                ..ok.clone()
            },
            GatewayConfig {
                max_handshakes: 0,
                ..ok.clone()
            },
            GatewayConfig {
                max_handshakes: 99,
                ..ok.clone()
            },
            GatewayConfig {
                max_head: 10,
                ..ok.clone()
            },
            GatewayConfig {
                handshake_timeout: Duration::ZERO,
                ..ok.clone()
            },
            GatewayConfig {
                slice: Duration::ZERO,
                ..ok.clone()
            },
            GatewayConfig {
                slice: Duration::from_secs(5),
                ..ok.clone()
            },
            GatewayConfig {
                failure_limit: 0,
                ..ok.clone()
            },
            GatewayConfig {
                credential_poll: Duration::ZERO,
                ..ok.clone()
            },
            GatewayConfig {
                credential_poll: Duration::from_secs(6),
                ..ok.clone()
            },
            GatewayConfig {
                max_subscriptions: 0,
                ..ok.clone()
            },
            GatewayConfig {
                batch_events: 0,
                ..ok.clone()
            },
            GatewayConfig {
                batch_events: 257,
                ..ok.clone()
            },
        ];
        for (i, bad) in cases.iter().enumerate() {
            assert!(bad.validate().is_err(), "case {i} must be refused");
        }
    }

    #[test]
    fn allowed_hosts_are_checked_at_start_and_do_not_widen_the_bind_rules() {
        let host = |text: &str| AllowedHost::parse(text).unwrap();
        let loopback = |hosts: Vec<AllowedHost>| GatewayConfig {
            address: SocketAddr::from(([127, 0, 0, 1], 0)),
            allowed_hosts: hosts,
            ..GatewayConfig::default()
        };
        assert!(
            loopback(vec![host("gateway.example.test")])
                .validate()
                .is_ok()
        );
        let twice = loopback(vec![host("a.test"), host("A.TEST")]);
        assert!(
            twice
                .validate()
                .unwrap_err()
                .to_string()
                .contains("listed twice")
        );
        let many: Vec<AllowedHost> = (0..MAX_ALLOWED_HOSTS + 1)
            .map(|i| host(&format!("h{i}.test")))
            .collect();
        assert!(
            loopback(many)
                .validate()
                .unwrap_err()
                .to_string()
                .contains("got 17")
        );
        // A name does not lift the rule that a non-loopback bind needs TLS and tokens.
        let remote = GatewayConfig {
            address: SocketAddr::from(([0, 0, 0, 0], 0)),
            allowed_hosts: vec![host("gateway.example.test")],
            ..GatewayConfig::default()
        };
        assert!(remote.validate().unwrap_err().to_string().contains("TLS"));
    }

    fn quotas(per_principal: usize, reserved: usize, max_live: usize) -> Arc<Quotas> {
        Quotas::new(&GatewayConfig {
            max_connections: max_live,
            max_connections_per_principal: per_principal,
            reserved_connections: reserved,
            max_long_polls_per_principal: 1,
            max_long_polls: 2,
            ..GatewayConfig::default()
        })
    }

    #[test]
    fn one_principal_stops_at_its_cap_and_others_are_not_affected() {
        let quotas = quotas(2, 0, 32);
        let a1 = quotas.enter("account:a", 1).unwrap();
        let _a2 = quotas.enter("account:a", 2).unwrap();
        assert_eq!(
            quotas.enter("account:a", 3).err(),
            Some(LimitRefusal::PerPrincipal)
        );
        // Another principal has its own two places.
        let _b1 = quotas.enter("account:b", 3).unwrap();
        let _b2 = quotas.enter("account:b", 4).unwrap();
        drop(a1);
        let _a3 = quotas.enter("account:a", 4).unwrap();
        assert_eq!(quotas.refusals().per_principal, 1);
    }

    #[test]
    fn the_reserved_places_are_only_for_principals_that_hold_none() {
        // 4 places, 2 reserved: a principal that holds one may take places while at most 2 are open.
        let quotas = quotas(8, 2, 4);
        let _a1 = quotas.enter("account:a", 1).unwrap();
        let _a2 = quotas.enter("account:a", 2).unwrap();
        assert_eq!(
            quotas.enter("account:a", 3).err(),
            Some(LimitRefusal::Reserved)
        );
        // A principal with nothing may use them.
        let _b = quotas.enter("account:b", 3).unwrap();
        let _c = quotas.enter("account:c", 4).unwrap();
        assert_eq!(quotas.refusals().reserved_share, 1);
    }

    #[test]
    fn long_poll_places_are_capped_per_principal_and_globally_and_come_back() {
        let quotas = quotas(8, 0, 32);
        let a = quotas.poll("account:a").unwrap();
        assert_eq!(
            quotas.poll("account:a").err(),
            Some(LimitRefusal::PollsPerPrincipal)
        );
        let _b = quotas.poll("account:b").unwrap();
        assert_eq!(
            quotas.poll("account:c").err(),
            Some(LimitRefusal::PollsGlobal)
        );
        drop(a);
        let _c = quotas.poll("account:c").unwrap();
        assert_eq!(quotas.refusals().long_polls, 2);
    }

    #[test]
    fn nothing_is_kept_for_a_principal_that_holds_nothing() {
        let quotas = quotas(2, 0, 32);
        let share = quotas.enter("account:a", 1).unwrap();
        let place = quotas.poll("account:a").unwrap();
        drop(share);
        // The poll still counts for the principal, so its entry stays until the poll is back.
        assert_eq!(locked(&quotas.state).held.len(), 1);
        drop(place);
        let state = locked(&quotas.state);
        assert!(state.held.is_empty());
        assert_eq!(state.polls, 0);
    }

    #[test]
    fn a_connection_that_panics_gives_its_place_back() {
        let quotas = quotas(1, 0, 32);
        let held = Arc::clone(&quotas);
        let worker = std::thread::spawn(move || {
            let _share = held.enter("account:a", 1).unwrap();
            let _place = held.poll("account:a").unwrap();
            panic!("a connection thread that panics");
        });
        assert!(worker.join().is_err());
        // Both places are free again: one of each is admitted, and the count is that one, not two.
        let _share = quotas.enter("account:a", 1).unwrap();
        let _place = quotas.poll("account:a").unwrap();
        assert_eq!(locked(&quotas.state).polls, 1);
    }

    #[test]
    fn a_planted_ignored_principal_cap_is_caught_by_the_witness() {
        bugs::clear();
        bugs::enable(Bug::GatewayConnectionCapIgnored);
        let quotas = quotas(1, 0, 32);
        let _first = quotas.enter("account:a", 1).unwrap();
        let second = quotas.enter("account:a", 2);
        let poll_first = quotas.poll("account:a").unwrap();
        let poll_second = quotas.poll("account:a");
        bugs::clear();
        drop(poll_first);
        assert!(second.is_ok(), "the planted bug must admit past the cap");
        assert!(
            poll_second.is_ok(),
            "the planted bug must admit past the poll cap"
        );
    }

    #[test]
    fn the_limits_are_validated_before_binding() {
        let ok = GatewayConfig {
            address: SocketAddr::from(([127, 0, 0, 1], 0)),
            ..GatewayConfig::default()
        };
        let cases = [
            GatewayConfig {
                max_connections_per_principal: 0,
                ..ok.clone()
            },
            GatewayConfig {
                max_connections_per_principal: 1025,
                ..ok.clone()
            },
            GatewayConfig {
                reserved_connections: 1025,
                ..ok.clone()
            },
            GatewayConfig {
                max_long_polls_per_principal: 0,
                ..ok.clone()
            },
            GatewayConfig {
                max_long_polls: 0,
                ..ok.clone()
            },
            GatewayConfig {
                max_long_polls: 1025,
                ..ok.clone()
            },
        ];
        for case in cases {
            assert!(case.validate().is_err());
        }
        // The defaults are accepted with a single-connection gateway, as the existing tests use.
        let tiny = GatewayConfig {
            max_connections: 1,
            max_handshakes: 1,
            ..ok
        };
        assert!(tiny.validate().is_ok());
    }
}

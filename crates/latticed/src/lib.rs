//! Host TCP adapter for the sans-I/O PostgreSQL session.
//!
//! This is a bounded listener with local development trust or configured SCRAM over mandatory TLS.
//! The default `SELECT 1` handler does not open a database. Cancel requests signal a cooperative
//! handler; they cannot preempt blocking I/O or undo a committed operation.
//!
//! Timeouts: `read_timeout` bounds one frame once its first byte arrives; `idle_timeout` bounds the
//! wait for the next frame of an authenticated session (it defaults to `read_timeout`);
//! `write_timeout` bounds each socket write and `response_timeout` is an absolute budget for one
//! whole response, so a peer that drains a few bytes per interval cannot hold a worker. A TLS
//! connection is closed with a best-effort `close_notify` on every exit that still can write (orderly
//! end, fatal error replies, timeouts including the startup budget, panics), bounded by a short absolute deadline. CancelRequests are
//! rate limited per client address and globally; an excess request is dropped without a reply, like
//! an unknown key. Wrong keys consume that budget, so the limit can be used to starve cancels (see
//! the README).
//!
//! Robustness: a failure that belongs to one connection (a reset before accept, a socket option
//! that fails on a reset socket, a thread that cannot be spawned) drops that socket and the
//! listener keeps accepting. Only a fatal listener error ends the accept loop. Cancel keys are
//! registered only after authentication succeeds, so unauthenticated clients cannot consume
//! registry slots. Pre-authentication startup packets are capped at [`MAX_STARTUP_PACKET_LEN`]
//! bytes and must arrive within [`PRE_STARTUP_TIMEOUT`].
//!
//! TLS handshakes are limited globally (64 starts per second) and per client address (16 starts per
//! second per IPv4 address or IPv6 /64, in a table of at most 1024 addresses). Residual risks: a
//! client with many source addresses can still exhaust the global budget; a proxy or NAT in front
//! of many clients shares one address and is capped at 16 new TLS starts per second; and
//! unauthenticated clients still hold worker threads (`max_connections` plus the control reserve of
//! 8) until their deadline. The 5 s pre-startup limit applies only to startup-phase packets (SASL
//! frames use `read_timeout`) and drops clients that connect and then wait longer than 5 s before
//! sending. This is a resource bound, not a denial-of-service guarantee; see ADR 0020.
#![forbid(unsafe_code)]
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

mod transport;
pub use transport::TlsConfig;
use transport::Transport;

use lattice_api::auth::Accounts;
use lattice_api::cancel::{CancelKey, CancelRegistry, CancellationToken};
use lattice_api::pgwire::{self, Backend, Column, Frontend, ProtoError, Startup};
use lattice_api::principal::QueryContext;
use lattice_api::session::{Outcome, QueryError, QueryHandler, QueryOutput, Session};
use lattice_seams::OsRng;
use std::collections::{HashMap, VecDeque};
use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv6Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// Authentication is explicit: local development trust or SCRAM over mandatory TLS.
#[derive(Debug, Clone)]
pub enum Authentication {
    /// Plaintext trust is restricted to loopback; it is not a deployment authentication mode.
    DevelopmentTrust,
    /// No plaintext SQL startup or downgrade to trust is accepted.
    ScramTls {
        /// Bounded credentials provisioned before binding; no network password derivation.
        accounts: Arc<Accounts>,
        /// Configured server certificate and matching private key.
        tls: TlsConfig,
    },
}

/// Configuration for the development TCP listener.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// Address to bind. The default listens only on localhost.
    pub address: SocketAddr,
    /// Maximum registered SQL sessions; excess startups receive SQLSTATE 53300. Eight additional
    /// bounded workers let startup/cancel packets be classified when SQL sessions fill the limit.
    pub max_connections: usize,
    /// Maximum time to receive a complete startup or frontend frame, including slow partial reads.
    /// Unless `idle_timeout` is set this also acts as a connection idle timeout: a client that sends
    /// nothing for this long is disconnected, so pools must send keepalive traffic or raise it.
    pub read_timeout: Duration,
    /// How long an authenticated session may wait between frames. `None` uses `read_timeout`. When
    /// set, `read_timeout` then only bounds a frame whose first byte has already arrived.
    pub idle_timeout: Option<Duration>,
    /// Socket write timeout for each write call of a reply.
    pub write_timeout: Duration,
    /// Absolute budget for writing one complete response, however the peer paces its reads.
    pub response_timeout: Duration,
    /// One absolute budget from accept through startup, TLS and SCRAM; not reset per message.
    pub startup_timeout: Duration,
    /// Default is explicitly local development trust; remote binds require SCRAM/TLS.
    pub authentication: Authentication,
}
impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            address: SocketAddr::from(([127, 0, 0, 1], 5432)),
            max_connections: 32,
            read_timeout: Duration::from_secs(30),
            idle_timeout: None,
            write_timeout: Duration::from_secs(5),
            response_timeout: Duration::from_secs(15),
            startup_timeout: Duration::from_secs(10),
            authentication: Authentication::DevelopmentTrust,
        }
    }
}

/// Refuses a secret file (TLS private key, decoy secret) that group or others can access, because a
/// readable key is a leaked key. Unix only; other platforms rely on the operator's ACLs.
pub fn require_private_file(file: &std::fs::File) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            return Err(io::Error::other("secret path is not a regular file"));
        }
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(io::Error::other(
                "secret file is accessible to group or others (use mode 0600)",
            ));
        }
    }
    #[cfg(not(unix))]
    let _ = file;
    Ok(())
}

/// Largest accepted pre-authentication startup packet (PostgreSQL's own limit is 10000 bytes).
pub const MAX_STARTUP_PACKET_LEN: usize = 10_000;

/// Longest a client may take to deliver one pre-authentication startup-phase packet, further
/// bounded by the configured read and startup timeouts.
pub const PRE_STARTUP_TIMEOUT: Duration = Duration::from_secs(5);

const GLOBAL_TLS_STARTS_PER_SECOND: usize = 64;
const PER_IP_TLS_STARTS_PER_SECOND: usize = 16;
const MAX_TRACKED_ADDRESSES: usize = 1024;
/// CancelRequest budgets. A cancel needs a guessed 64-bit key, so these only bound guessing and
/// registry lock traffic. They are a resource bound, not a DoS guarantee: wrong keys consume budget,
/// so about 16 addresses (or a few IPv6 /64s) can use the whole global budget and starve real
/// cancels, and every client behind one NAT shares the per-address budget.
const GLOBAL_CANCELS_PER_SECOND: usize = 256;
const PER_IP_CANCELS_PER_SECOND: usize = 16;

/// Additional transport workers reserved for startup/cancel classification at the SQL-session cap.
/// Flooding these slots can still delay/refuse cancellation; this is a resource bound, not a DoS guarantee.
pub const CONTROL_CONNECTION_RESERVE: usize = 8;

struct RegistryState {
    entries: CancelRegistry,
    closing: bool,
}
impl RegistryState {
    fn new(capacity: usize) -> io::Result<Self> {
        Ok(Self {
            entries: CancelRegistry::new(capacity).map_err(io::Error::other)?,
            closing: false,
        })
    }
    fn ensure_open(&self) -> io::Result<()> {
        if self.closing {
            Err(io::Error::other("server shutting down"))
        } else {
            Ok(())
        }
    }
}

type Registry = Arc<Mutex<RegistryState>>;

/// An active listener. Explicit shutdown and drop both close active sockets and join every worker.
pub struct ServerHandle {
    address: SocketAddr,
    stopping: Arc<AtomicBool>,
    open: Arc<AtomicUsize>,
    acceptor: Option<JoinHandle<io::Result<()>>>,
}
impl ServerHandle {
    /// Binds before returning, so bind/configuration failures never hide in a background thread.
    pub fn start(config: ServerConfig) -> io::Result<Self> {
        Self::start_with_handler(config, || SelectOne)
    }

    /// Starts with one handler per admitted SQL connection. The factory runs outside registry locks.
    /// Cooperative handlers override QueryHandler's cancellation-aware methods and poll their token
    /// before committing work. Legacy blocking handlers cannot be forcibly interrupted.
    pub fn start_with_handler<H, F>(config: ServerConfig, factory: F) -> io::Result<Self>
    where
        H: QueryHandler + 'static,
        F: Fn() -> H + Send + Sync + 'static,
    {
        if config.max_connections == 0
            || config.max_connections > 1024
            || config.read_timeout.is_zero()
            || config.write_timeout.is_zero()
            || config.response_timeout.is_zero()
            || config.idle_timeout.is_some_and(|idle| idle.is_zero())
            || config
                .idle_timeout
                .is_some_and(|idle| Instant::now().checked_add(idle).is_none())
            || Instant::now()
                .checked_add(config.response_timeout)
                .is_none()
            || config.startup_timeout.is_zero()
            || Instant::now().checked_add(config.startup_timeout).is_none()
            || (matches!(config.authentication, Authentication::DevelopmentTrust)
                && !config.address.ip().is_loopback())
            || Instant::now().checked_add(config.read_timeout).is_none()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "nonzero bounded timeouts, 1..=1024 connections, and loopback for development trust required",
            ));
        }
        let registry = Arc::new(Mutex::new(RegistryState::new(config.max_connections)?));
        let listener = TcpListener::bind(config.address)?;
        listener.set_nonblocking(true)?;
        let address = listener.local_addr()?;
        let stopping = Arc::new(AtomicBool::new(false));
        let stop = stopping.clone();
        let open = Arc::new(AtomicUsize::new(0));
        let open_count = open.clone();
        let acceptor = thread::Builder::new()
            .name("pg-accept".into())
            .spawn(move || {
                accept(
                    listener,
                    config,
                    stop,
                    registry,
                    Arc::new(factory),
                    open_count,
                )
            })?;
        Ok(Self {
            address,
            stopping,
            open,
            acceptor: Some(acceptor),
        })
    }

    /// True once the accept thread has exited, which only happens after shutdown or a fatal
    /// listener error. A supervisor should treat an unrequested exit as a failed server.
    pub fn is_finished(&self) -> bool {
        self.acceptor
            .as_ref()
            .is_none_or(|acceptor| acceptor.is_finished())
    }

    /// Number of connection workers the accept thread currently tracks (accepted and not yet
    /// reaped). It lags a closed connection by at most one accept-loop iteration (about 5 ms), and
    /// connections still waiting in the kernel backlog are not counted.
    pub fn open_connections(&self) -> usize {
        self.open.load(Ordering::Acquire)
    }

    /// Actual bound address, including the selected port when binding port zero.
    pub fn local_addr(&self) -> SocketAddr {
        self.address
    }

    /// Stops accepting, interrupts active reads/writes and waits for all workers to exit.
    pub fn shutdown(mut self) -> io::Result<()> {
        self.stop_and_join()
    }

    fn stop_and_join(&mut self) -> io::Result<()> {
        self.stopping.store(true, Ordering::Release);
        if let Some(acceptor) = self.acceptor.take() {
            acceptor
                .join()
                .map_err(|_| io::Error::other("TCP acceptor panicked"))?
        } else {
            Ok(())
        }
    }
}
impl Drop for ServerHandle {
    fn drop(&mut self) {
        let _ = self.stop_and_join();
    }
}

struct Worker {
    socket: TcpStream,
    thread: JoinHandle<()>,
}

/// Consecutive transient accept failures tolerated before the listener gives up (about ten seconds).
const MAX_CONSECUTIVE_ACCEPT_ERRORS: u32 = 200;
const ACCEPT_ERROR_BACKOFF: Duration = Duration::from_millis(50);
const ACCEPT_ERROR_REPORT_INTERVAL: Duration = Duration::from_secs(5);

/// Whether an accept error ends the listener or only delays it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AcceptFailure {
    Fatal,
    Transient,
}

/// Classifies by `ErrorKind` only. Fatal kinds mean the listening socket itself is unusable or
/// misconfigured; everything else (EMFILE, ENOMEM, firewall refusal) may clear by itself.
fn classify(error: &io::Error) -> AcceptFailure {
    use io::ErrorKind::{InvalidData, InvalidInput, NotConnected};
    // accept(2) documents EOPNOTSUPP, ENETDOWN, EPROTO, ENOPROTOOPT, EHOSTDOWN, ENONET,
    // EHOSTUNREACH and ENETUNREACH as "retry like EAGAIN", so those stay transient.
    if matches!(error.kind(), InvalidInput | InvalidData | NotConnected) {
        AcceptFailure::Fatal
    } else {
        AcceptFailure::Transient
    }
}

/// Best-effort FATAL 53300 for a connection refused before any worker serves it. Never blocks the
/// accept loop: the socket is non-blocking and a full send buffer simply drops the message.
/// Best effort only: if the peer's startup bytes are still unread when we close, Linux turns the
/// close into a reset that can discard the reply, so a client may see a reset instead of the error.
/// A CancelRequest that arrives here is dropped, because no worker is free to route it.
fn refuse_at_capacity(socket: &TcpStream) {
    let mut bytes = Vec::new();
    let encoded = fatal("53300", "sorry, too many clients already")
        .messages
        .iter()
        .all(|message| message.encode(&mut bytes).is_ok());
    if encoded && socket.set_nonblocking(true).is_ok() {
        let _ = (&*socket).write(&bytes);
    }
    let _ = socket.shutdown(Shutdown::Both);
}

/// Reports only the error kind and count; accept errors carry no client data.
#[allow(clippy::print_stderr)]
fn report_accept_error(error: &io::Error, consecutive: u32) {
    eprintln!(
        "latticed: accept failing ({:?}, {consecutive} in a row); retrying",
        error.kind()
    );
}

fn accept<H, F>(
    listener: TcpListener,
    config: ServerConfig,
    stopping: Arc<AtomicBool>,
    registry: Registry,
    factory: Arc<F>,
    open: Arc<AtomicUsize>,
) -> io::Result<()>
where
    H: QueryHandler + 'static,
    F: Fn() -> H + Send + Sync + 'static,
{
    let mut workers: Vec<Worker> = Vec::new();
    let limits = Limits {
        handshakes: Mutex::new(HandshakeLimiter::default()),
        cancels: Mutex::new(HandshakeLimiter::with_limits(
            GLOBAL_CANCELS_PER_SECOND,
            PER_IP_CANCELS_PER_SECOND,
        )),
    };
    let limits = Arc::new(limits);
    let mut consecutive = 0u32;
    let mut last_report: Option<Instant> = None;
    let result = (|| {
        while !stopping.load(Ordering::Acquire) {
            let mut index = 0;
            while index < workers.len() {
                if workers
                    .get(index)
                    .is_some_and(|worker| worker.thread.is_finished())
                {
                    // A panicked worker ends only its own connection; the listener keeps accepting.
                    let worker = workers.swap_remove(index);
                    let _ = worker.thread.join(); // A join error never aborts the loop.
                } else {
                    index += 1;
                }
            }
            open.store(workers.len(), Ordering::Release);
            match listener.accept() {
                Ok((socket, _)) => {
                    consecutive = 0;
                    if workers.len() == config.max_connections + CONTROL_CONNECTION_RESERVE {
                        // No worker is free to classify the first packet, so a CancelRequest on this
                        // connection is lost. Like PostgreSQL, answer with FATAL 53300 and close,
                        // without waiting for the peer: one non-blocking write of a few bytes.
                        refuse_at_capacity(&socket);
                        continue;
                    }
                    // A socket that fails setup or a failed spawn drops only that client.
                    if let Ok(worker) = start_worker(socket, &config, &registry, &factory, &limits)
                    {
                        workers.push(worker);
                        open.store(workers.len(), Ordering::Release);
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    consecutive = 0;
                    thread::sleep(Duration::from_millis(5))
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                // The peer can reset a connection before it is accepted; that is not a listener failure.
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::ConnectionAborted | io::ErrorKind::ConnectionReset
                    ) => {}
                Err(error) => {
                    if classify(&error) == AcceptFailure::Fatal {
                        return Err(error);
                    }
                    // Descriptor exhaustion, memory pressure or a firewall refusal: back off and keep
                    // serving admitted connections, but give up if the listener never recovers.
                    consecutive += 1;
                    if consecutive >= MAX_CONSECUTIVE_ACCEPT_ERRORS {
                        return Err(io::Error::new(
                            error.kind(),
                            "listener gave up after repeated accept failures",
                        ));
                    }
                    if last_report.is_none_or(|at| at.elapsed() >= ACCEPT_ERROR_REPORT_INTERVAL) {
                        report_accept_error(&error, consecutive);
                        last_report = Some(Instant::now());
                    }
                    thread::sleep(ACCEPT_ERROR_BACKOFF);
                }
            }
        }
        Ok(())
    })();
    // Cancellation comes before joining CPU-bound cooperative handlers. Recover a poisoned mutex
    // only for shutdown/cleanup; normal admission and query operations fail closed on poison.
    close_registry(&registry);
    // Shutdown is independent of each client's configured timeout, including partial-frame clients.
    for worker in &workers {
        let _ = worker.socket.shutdown(Shutdown::Both);
    }
    let mut result = result;
    for worker in workers {
        if worker.thread.join().is_err() && result.is_ok() {
            result = Err(io::Error::other("connection worker panicked"));
        }
    }
    result
}

fn start_worker<H, F>(
    socket: TcpStream,
    config: &ServerConfig,
    registry: &Registry,
    factory: &Arc<F>,
    limits: &Arc<Limits>,
) -> io::Result<Worker>
where
    H: QueryHandler + 'static,
    F: Fn() -> H + Send + Sync + 'static,
{
    // Accepted sockets can inherit O_NONBLOCK on some hosts (notably macOS).
    // Workers use blocking I/O with explicit deadlines, independently of accept.
    socket.set_nonblocking(false)?;
    socket.set_write_timeout(Some(config.write_timeout))?;
    socket.set_nodelay(true)?;
    let peer = socket.peer_addr()?.ip();
    let controller = socket.try_clone()?;
    let connection_config = config.clone();
    let connection_limits = limits.clone();
    let deadline = Instant::now()
        .checked_add(config.startup_timeout)
        .ok_or_else(|| io::Error::other("startup deadline overflow"))?;
    let connection_registry = registry.clone();
    let connection_factory = factory.clone();
    let thread = thread::Builder::new()
        .name("pg-connection".into())
        .spawn(move || {
            // Disconnects, malformed input and timeouts end this connection, not the listener.
            let _ = connection(
                Transport::new(socket, deadline, connection_config.write_timeout),
                connection_config,
                connection_registry,
                connection_factory,
                connection_limits,
                peer,
            );
        })?;
    Ok(Worker {
        socket: controller,
        thread,
    })
}

fn read_before(
    socket: &mut Transport,
    mut target: &mut [u8],
    deadline: Instant,
) -> io::Result<bool> {
    let mut empty = true;
    while !target.is_empty() {
        let _remaining = deadline
            .checked_duration_since(Instant::now())
            .filter(|duration| !duration.is_zero())
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::TimedOut, "frame read deadline exceeded")
            })?;
        socket.set_deadline(deadline);
        match socket.read(target) {
            Ok(0) if empty => return Ok(false),
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "partial PostgreSQL frame",
                ));
            }
            Ok(count) => {
                empty = false;
                target = target
                    .get_mut(count..)
                    .ok_or_else(|| io::Error::other("invalid socket read count"))?;
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(true)
}

enum FrameError {
    Io(io::Error),
    Protocol(ProtoError),
}
impl From<io::Error> for FrameError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

fn frame_bounded(
    socket: &mut Transport,
    startup: bool,
    timeout: Duration,
    idle: Option<Duration>,
    maximum: usize,
) -> Result<Option<Vec<u8>>, FrameError> {
    let overflow = || io::Error::new(io::ErrorKind::InvalidInput, "read timeout overflow");
    let mut deadline = Instant::now().checked_add(timeout).ok_or_else(overflow)?;
    let mut tag = [0u8; 1];
    if !startup {
        // The wait for a frame's first byte may be longer than the time allowed to finish it.
        let wait = match idle {
            Some(idle) => Instant::now().checked_add(idle).ok_or_else(overflow)?,
            None => deadline,
        };
        if !read_before(socket, &mut tag, wait)? {
            return Ok(None);
        }
        if idle.is_some() {
            deadline = Instant::now().checked_add(timeout).ok_or_else(overflow)?;
        }
    }
    let mut raw_length = [0u8; 4];
    if !read_before(socket, &mut raw_length, deadline)? {
        return if startup {
            Ok(None)
        } else {
            Err(io::Error::new(io::ErrorKind::UnexpectedEof, "missing frame length").into())
        };
    }
    let length = i32::from_be_bytes(raw_length);
    let minimum = if startup { 8 } else { 4 };
    if length < minimum || i64::from(length) > maximum as i64 {
        return Err(FrameError::Protocol(ProtoError::BadLength(i64::from(
            length,
        ))));
    }
    let payload_len = usize::try_from(length - 4)
        .map_err(|_| FrameError::Protocol(ProtoError::BadLength(i64::from(length))))?;
    let mut bytes = Vec::with_capacity(payload_len + 5);
    if !startup {
        bytes.extend_from_slice(&tag);
    }
    bytes.extend_from_slice(&raw_length);
    let offset = bytes.len();
    bytes.resize(offset + payload_len, 0);
    let target = bytes
        .get_mut(offset..)
        .ok_or_else(|| io::Error::other("invalid frame offset"))?;
    if !read_before(socket, target, deadline)? {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "missing frame payload").into());
    }
    Ok(Some(bytes))
}

fn lock_registry(registry: &Mutex<RegistryState>) -> io::Result<MutexGuard<'_, RegistryState>> {
    registry
        .lock()
        .map_err(|_| io::Error::other("cancel registry poisoned"))
}

fn close_registry(registry: &Mutex<RegistryState>) {
    let mut registry = match registry.lock() {
        Ok(registry) => registry,
        Err(poisoned) => poisoned.into_inner(),
    };
    // Serialize the shutdown latch with both admission and query-token allocation. Closing sockets
    // alone cannot prevent an already-classified/pipelined request from starting CPU-bound work.
    registry.closing = true;
    registry.entries.clear();
}

struct Registration {
    registry: Registry,
    key: CancelKey,
}
impl Registration {
    fn new(registry: Registry) -> io::Result<Self> {
        // A key is valid for this connection's lifetime, not a wall-clock TTL that could silently
        // invalidate the client's only BackendKeyData. Drop unregisters it, even on worker unwind.
        let key = {
            let mut state = lock_registry(&registry)?;
            state.ensure_open()?;
            state
                .entries
                .register(&mut OsRng, 0, u64::MAX)
                .map_err(io::Error::other)?
        };
        Ok(Self { registry, key })
    }
    fn begin_query(&self) -> io::Result<CancellationToken> {
        let mut state = lock_registry(&self.registry)?;
        state.ensure_open()?;
        state
            .entries
            .begin_query(self.key, 0)
            .map_err(io::Error::other)
    }
}
impl Drop for Registration {
    fn drop(&mut self) {
        let mut registry = match self.registry.lock() {
            Ok(registry) => registry,
            Err(poisoned) => poisoned.into_inner(),
        };
        registry.entries.unregister(self.key, 0);
    }
}

fn fatal(code: &str, message: impl Into<String>) -> Outcome {
    Outcome {
        messages: vec![Backend::ErrorResponse {
            severity: "FATAL".into(),
            code: code.into(),
            message: message.into(),
        }],
        close: true,
        ..Outcome::default()
    }
}

/// Sliding one-second windows of TLS handshake starts: one global and one per client address.
struct HandshakeLimiter {
    global: VecDeque<Instant>,
    per_address: HashMap<IpAddr, VecDeque<Instant>>,
    global_limit: usize,
    per_address_limit: usize,
}
impl Default for HandshakeLimiter {
    fn default() -> Self {
        Self::with_limits(GLOBAL_TLS_STARTS_PER_SECOND, PER_IP_TLS_STARTS_PER_SECOND)
    }
}

/// Admission windows shared by all connection workers.
struct Limits {
    handshakes: Mutex<HandshakeLimiter>,
    cancels: Mutex<HandshakeLimiter>,
}
const WINDOW: Duration = Duration::from_secs(1);
fn expire(starts: &mut VecDeque<Instant>, now: Instant) {
    while starts
        .front()
        .is_some_and(|t| now.saturating_duration_since(*t) >= WINDOW)
    {
        starts.pop_front();
    }
}
impl HandshakeLimiter {
    fn with_limits(global_limit: usize, per_address_limit: usize) -> Self {
        Self {
            global: VecDeque::new(),
            per_address: HashMap::new(),
            global_limit,
            per_address_limit,
        }
    }

    /// IPv6 clients are keyed by /64 so one host cannot rotate through its whole prefix.
    fn key(address: IpAddr) -> IpAddr {
        // A dual-stack `[::]` listener reports IPv4 peers as ::ffff:a.b.c.d; treat them as IPv4.
        match address.to_canonical() {
            IpAddr::V6(v6) => IpAddr::V6(Ipv6Addr::from(
                u128::from(v6) & 0xffff_ffff_ffff_ffff_0000_0000_0000_0000,
            )),
            v4 => v4,
        }
    }

    /// Admits one handshake start, or records nothing. Memory is bounded by
    /// `MAX_TRACKED_ADDRESSES * PER_IP_TLS_STARTS_PER_SECOND` timestamps.
    fn admit(&mut self, address: IpAddr, now: Instant) -> bool {
        expire(&mut self.global, now);
        if self.global.len() >= self.global_limit {
            return false;
        }
        let key = Self::key(address);
        if !self.per_address.contains_key(&key) && self.per_address.len() >= MAX_TRACKED_ADDRESSES {
            self.per_address.retain(|_, starts| {
                expire(starts, now);
                !starts.is_empty()
            });
            if self.per_address.len() >= MAX_TRACKED_ADDRESSES {
                return false;
            }
        }
        let starts = self.per_address.entry(key).or_default();
        expire(starts, now);
        if starts.len() >= self.per_address_limit {
            return false;
        }
        starts.push_back(now);
        self.global.push_back(now);
        true
    }
}

/// Runs one connection and always ends it through `Transport::shutdown`, so every exit (orderly,
/// fatal error reply, protocol error, timeout, write failure or panic) sends TLS `close_notify` when the
/// connection is TLS and the send can still complete within `CLOSE_TIMEOUT`.
fn connection<H, F>(
    socket: Transport,
    config: ServerConfig,
    registry: Registry,
    factory: Arc<F>,
    limits: Arc<Limits>,
    peer: IpAddr,
) -> io::Result<()>
where
    H: QueryHandler + 'static,
    F: Fn() -> H + Send + Sync + 'static,
{
    // The guard closes on unwind too, so a panicking handler still gets the orderly close.
    let mut socket = Closing(socket);
    serve_connection(&mut socket, config, registry, factory, limits, peer)
}

/// Owns the transport and shuts it down when dropped, on every exit including a panic.
struct Closing(Transport);
impl std::ops::Deref for Closing {
    type Target = Transport;
    fn deref(&self) -> &Transport {
        &self.0
    }
}
impl std::ops::DerefMut for Closing {
    fn deref_mut(&mut self) -> &mut Transport {
        &mut self.0
    }
}
impl Drop for Closing {
    fn drop(&mut self) {
        let _ = self.0.shutdown();
    }
}

/// Applies the CancelRequest budget, then signals the matching query. Over budget the request is
/// dropped without a reply, exactly like an unknown key. The budget is charged before the key is
/// looked up, so wrong guesses cost as much as right ones.
fn route_cancel(
    limits: &Limits,
    registry: &Registry,
    peer: IpAddr,
    now: Instant,
    key: CancelKey,
) -> io::Result<bool> {
    let admitted = limits
        .cancels
        .lock()
        .map_err(|_| io::Error::other("cancel admission unavailable"))?
        .admit(peer, now);
    if admitted {
        lock_registry(registry)?.entries.cancel(key, 0);
    }
    Ok(admitted)
}

fn serve_connection<H, F>(
    socket: &mut Transport,
    config: ServerConfig,
    registry: Registry,
    factory: Arc<F>,
    limits: Arc<Limits>,
    peer: IpAddr,
) -> io::Result<()>
where
    H: QueryHandler + 'static,
    F: Fn() -> H + Send + Sync + 'static,
{
    let timeout = config.read_timeout;
    let pre_startup = timeout.min(PRE_STARTUP_TIMEOUT);
    let response = config.response_timeout;
    // Classify startup before SQL admission: CancelRequest uses its own short-lived connection and
    // must remain routable while all registered SQL sessions are busy.
    loop {
        let input = match frame_bounded(socket, true, pre_startup, None, MAX_STARTUP_PACKET_LEN) {
            Ok(Some(input)) => input,
            Ok(None) => return Ok(()),
            Err(FrameError::Io(error)) => return Err(error),
            Err(FrameError::Protocol(error)) => {
                send(socket, fatal("08P01", error.to_string()), response)?;
                return Ok(());
            }
        };
        let startup = match pgwire::decode_startup(&input) {
            Ok(Some((startup, _))) => startup,
            Ok(None) => {
                send(socket, fatal("08P01", "truncated startup"), response)?;
                return Ok(());
            }
            Err(error) => {
                send(socket, fatal("08P01", error.to_string()), response)?;
                return Ok(());
            }
        };
        match startup {
            Startup::Cancel { pid, secret } => {
                route_cancel(
                    &limits,
                    &registry,
                    peer,
                    Instant::now(),
                    CancelKey { pid, secret },
                )?;
                return Ok(());
            }
            Startup::SslRequest => {
                if socket.is_tls() {
                    return Err(io::Error::other("repeated SSLRequest"));
                }
                match &config.authentication {
                    Authentication::DevelopmentTrust => {
                        socket.write_all(&[pgwire::ENCRYPTION_DECLINED])?
                    }
                    Authentication::ScramTls { tls, .. } => {
                        // Bound expensive TLS starts globally (64/s) and per client address (16/s).
                        {
                            let mut starts = limits
                                .handshakes
                                .lock()
                                .map_err(|_| io::Error::other("TLS admission unavailable"))?;
                            if !starts.admit(peer, Instant::now()) {
                                return Err(io::Error::other("TLS handshake rate exceeded"));
                            }
                        }
                        socket.write_all(b"S")?;
                        socket.flush()?;
                        socket.upgrade_in_place(tls)?;
                    }
                }
            }
            Startup::GssEncRequest => {
                if socket.is_tls() {
                    return Err(io::Error::other("GSS request after TLS"));
                }
                socket.write_all(&[pgwire::ENCRYPTION_DECLINED])?;
            }
            Startup::Params(params) => {
                if matches!(config.authentication, Authentication::ScramTls { .. })
                    && !socket.is_tls()
                {
                    send(socket, fatal("28000", "TLS required"), response)?;
                    return Ok(());
                }
                // The cancel key is registered only once authentication succeeds (see `bind_key`).
                let mut session = match &config.authentication {
                    Authentication::DevelopmentTrust => Session::local_trust(factory(), 0, 0),
                    Authentication::ScramTls { accounts, .. } => {
                        let users: Vec<_> =
                            params.iter().filter(|(key, _)| key == "user").collect();
                        let exchange = if users.len() == 1 {
                            accounts.exchange(
                                users.first().map(|(_, name)| name.as_str()).unwrap_or(""),
                                &mut OsRng,
                            )
                        } else {
                            Err(lattice_api::scram::ScramError::Malformed)
                        };
                        let (exchange, account) = match exchange {
                            Ok(exchange) => exchange,
                            Err(_) => {
                                send(
                                    socket,
                                    fatal("28P01", "password authentication failed"),
                                    response,
                                )?;
                                return Ok(());
                            }
                        };
                        Session::with_scram(factory(), 0, 0, exchange, account)
                    }
                };
                let mut registration = None;
                let outcome = session.on_startup(Startup::Params(params));
                let outcome = bind_key(outcome, &registry, &mut registration);
                let close = outcome.close;
                send(socket, outcome, response)?;
                if session.sasl_phase().is_none() {
                    socket.authenticated();
                }
                if !close {
                    serve(
                        socket,
                        timeout,
                        config.idle_timeout,
                        config.response_timeout,
                        &registry,
                        &mut registration,
                        &mut session,
                    )?;
                }
                drop(session);
                drop(registration);
                return Ok(());
            }
        }
    }
}

/// Registers a cancel key when `outcome` completes authentication (it carries `BackendKeyData`) and
/// substitutes the real key for the placeholder. If registration is unavailable the outcome becomes a
/// fatal 53300 and the client never receives `AuthenticationOk`.
fn bind_key(
    mut outcome: Outcome,
    registry: &Registry,
    registration: &mut Option<Registration>,
) -> Outcome {
    let completes = outcome
        .messages
        .iter()
        .any(|message| matches!(message, Backend::BackendKeyData { .. }));
    if !completes {
        return outcome;
    }
    match Registration::new(registry.clone()) {
        Ok(created) => {
            let key = created.key;
            for message in &mut outcome.messages {
                if let Backend::BackendKeyData { pid, secret } = message {
                    *pid = key.pid;
                    *secret = key.secret;
                }
            }
            *registration = Some(created);
            outcome
        }
        Err(_) => fatal("53300", "connection registration unavailable"),
    }
}

fn serve<H: QueryHandler>(
    socket: &mut Transport,
    timeout: Duration,
    idle: Option<Duration>,
    response_timeout: Duration,
    registry: &Registry,
    registration: &mut Option<Registration>,
    session: &mut Session<H>,
) -> io::Result<()> {
    while !session.is_closed() {
        let input = match frame_bounded(
            socket,
            false,
            timeout,
            // SASL frames belong to the startup budget; only an established session may idle.
            if session.sasl_phase().is_some() {
                None
            } else {
                idle
            },
            if session.sasl_phase().is_some() {
                pgwire::MAX_SASL_BODY_LEN + 4
            } else {
                pgwire::MAX_MESSAGE_LEN
            },
        ) {
            Ok(Some(input)) => input,
            Ok(None) => break,
            Err(FrameError::Io(error)) => return Err(error),
            Err(FrameError::Protocol(error)) => {
                send(socket, session.on_protocol_error(&error), response_timeout)?;
                break;
            }
        };
        let decoded = match session.sasl_phase() {
            Some(phase) => pgwire::decode_frontend_sasl(&input, phase),
            None => pgwire::decode_frontend(&input),
        };
        let outcome = match decoded {
            Ok(Some((message, _))) => {
                let token = if session.sasl_phase().is_none()
                    && matches!(
                        &message,
                        Frontend::Query(_) | Frontend::Parse { .. } | Frontend::Execute { .. }
                    ) {
                    registration
                        .as_ref()
                        .ok_or_else(|| io::Error::other("query before registration"))?
                        .begin_query()?
                } else {
                    CancellationToken::default()
                };
                session.on_message_with_cancellation(message, &token)
            }
            Ok(None) => session.on_protocol_error(&ProtoError::Truncated),
            Err(error) => session.on_protocol_error(&error),
        };
        let outcome = bind_key(outcome, registry, registration);
        let close = outcome.close;
        send(socket, outcome, response_timeout)?;
        if session.sasl_phase().is_none() && !close {
            socket.authenticated();
        }
        if close {
            break;
        }
    }
    Ok(())
}

/// Writes one response under an absolute deadline (`ServerConfig::response_timeout`).
fn send(socket: &mut Transport, outcome: Outcome, budget: Duration) -> io::Result<()> {
    let deadline = Instant::now()
        .checked_add(budget)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "response timeout overflow"))?;
    socket.set_response_deadline(Some(deadline));
    let result = write_outcome(socket, outcome);
    socket.set_response_deadline(None);
    result
}

fn write_outcome(socket: &mut Transport, outcome: Outcome) -> io::Result<()> {
    if let Some(byte) = outcome.encryption_reply {
        socket.write_all(&[byte])?;
    }
    for message in outcome.messages {
        let mut bytes = Vec::new();
        message.encode(&mut bytes).map_err(io::Error::other)?;
        socket.write_all(&bytes)?;
    }
    socket.flush()?;
    Ok(())
}

struct SelectOne;
fn select_one(sql: &str) -> Result<Vec<Column>, QueryError> {
    // This is a development handler, not a SQL parser. Do not silently accept arbitrary statements.
    if sql.len() <= 64
        && sql
            .trim()
            .trim_end_matches(';')
            .trim()
            .eq_ignore_ascii_case("SELECT 1")
    {
        Ok(vec![Column {
            name: "?column?".into(),
            table_oid: 0,
            column_attr: 0,
            type_oid: 23,
            type_size: 4,
            type_modifier: -1,
            format: 0,
        }])
    } else {
        Err(QueryError::new(
            "0A000",
            "only SELECT 1 is implemented by the development handler",
        ))
    }
}
impl QueryHandler for SelectOne {
    fn execute(
        &mut self,
        _: &QueryContext<'_>,
        sql: &str,
        params: &[Option<Vec<u8>>],
    ) -> Result<QueryOutput, QueryError> {
        if !params.is_empty() {
            return Err(QueryError::new(
                "0A000",
                "query parameters are not implemented",
            ));
        }
        Ok(QueryOutput {
            columns: select_one(sql)?,
            rows: vec![vec![Some(b"1".to_vec())]],
            tag: "SELECT 1".into(),
        })
    }
    fn describe(&mut self, _: &QueryContext<'_>, sql: &str) -> Result<Vec<Column>, QueryError> {
        select_one(sql)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;
    #[test]
    fn poisoned_registry_is_fail_closed_normally_but_shutdown_still_cancels_tokens() {
        let registry = Arc::new(Mutex::new(RegistryState::new(1).unwrap()));
        let key = registry
            .lock()
            .unwrap()
            .entries
            .register(&mut OsRng, 0, u64::MAX)
            .unwrap();
        let token = registry
            .lock()
            .unwrap()
            .entries
            .begin_query(key, 0)
            .unwrap();
        let poison = registry.clone();
        assert!(
            thread::spawn(move || {
                let _guard = poison.lock().unwrap();
                panic!("test registry poison");
            })
            .join()
            .is_err()
        );
        assert!(lock_registry(&registry).is_err());
        close_registry(&registry);
        assert!(token.is_cancelled());
        let registration = Registration {
            registry: registry.clone(),
            key,
        };
        drop(registration); // Cleanup must not panic on the same poisoned mutex.
    }

    #[test]
    fn shutdown_serializes_with_delayed_registration_and_query_start() {
        use std::sync::mpsc;
        // Model a classification worker paused after reading a pipelined startup/query, and an
        // admitted worker paused immediately before allocating its next query token.
        for already_registered in [false, true] {
            let registry = Arc::new(Mutex::new(RegistryState::new(1).unwrap()));
            let registration =
                already_registered.then(|| Registration::new(registry.clone()).unwrap());
            let active = registration.as_ref().map(|r| r.begin_query().unwrap());
            let (ready_tx, ready_rx) = mpsc::channel();
            let (release_tx, release_rx) = mpsc::channel();
            let (result_tx, result_rx) = mpsc::channel();
            let worker_registry = registry.clone();
            let worker = thread::spawn(move || {
                ready_tx.send(()).unwrap();
                // Both synchronization points are bounded so a regression cannot hang the suite.
                release_rx.recv_timeout(Duration::from_secs(2)).unwrap();
                let result = match registration {
                    Some(registration) => registration.begin_query(),
                    None => Registration::new(worker_registry).and_then(|r| r.begin_query()),
                };
                result_tx
                    .send(
                        result
                            .map(|token| token.is_cancelled())
                            .map_err(|e| e.to_string()),
                    )
                    .unwrap();
            });
            ready_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            close_registry(&registry);
            if let Some(token) = active {
                assert!(token.is_cancelled());
            }
            release_tx.send(()).unwrap();
            let result = result_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            worker.join().unwrap();
            assert_eq!(result.unwrap_err(), "server shutting down");
            let state = registry.lock().unwrap();
            assert!(state.closing);
        }
    }
    #[test]
    fn tls_start_windows_bound_global_and_per_address_without_sleeping() {
        let now = Instant::now();
        let ip = |n: u8| IpAddr::from([10, 0, 0, n]);
        let mut limiter = HandshakeLimiter::default();
        // One address is limited alone and leaves the global budget for others.
        for _ in 0..PER_IP_TLS_STARTS_PER_SECOND {
            assert!(limiter.admit(ip(1), now));
        }
        assert!(!limiter.admit(ip(1), now + Duration::from_millis(999)));
        assert!(limiter.admit(ip(2), now));
        assert!(limiter.admit(ip(1), now + Duration::from_secs(1)));
        // The global limit still binds many addresses; rejected starts consume nothing.
        let mut limiter = HandshakeLimiter::default();
        for n in 0..GLOBAL_TLS_STARTS_PER_SECOND {
            assert!(limiter.admit(IpAddr::from([10, 1, 0, n as u8]), now));
        }
        assert!(!limiter.admit(ip(9), now));
        assert_eq!(limiter.global.len(), GLOBAL_TLS_STARTS_PER_SECOND);
        assert!(limiter.admit(ip(9), now + Duration::from_secs(1)));
    }

    #[test]
    fn tls_address_table_is_bounded_and_ipv6_is_keyed_by_prefix() {
        let now = Instant::now();
        let mut limiter = HandshakeLimiter::default();
        for n in 0..MAX_TRACKED_ADDRESSES {
            limiter.per_address.insert(
                IpAddr::from([10, 2, (n / 256) as u8, (n % 256) as u8]),
                VecDeque::from([now]),
            );
        }
        assert!(!limiter.admit(IpAddr::from([10, 9, 9, 9]), now));
        assert_eq!(limiter.per_address.len(), MAX_TRACKED_ADDRESSES);
        // Once the recorded starts expire, space is reclaimed.
        assert!(limiter.admit(IpAddr::from([10, 9, 9, 9]), now + Duration::from_secs(1)));
        assert_eq!(limiter.per_address.len(), 1);
        let a: IpAddr = "2001:db8::1".parse().unwrap();
        let b: IpAddr = "2001:db8::ffff".parse().unwrap();
        let c: IpAddr = "2001:db8:0:1::1".parse().unwrap();
        assert_eq!(HandshakeLimiter::key(a), HandshakeLimiter::key(b));
        assert_ne!(HandshakeLimiter::key(a), HandshakeLimiter::key(c));
    }

    #[test]
    fn mapped_ipv4_peers_get_separate_buckets_and_accept_errors_are_classified() {
        let now = Instant::now();
        let a: IpAddr = "::ffff:192.0.2.1".parse().unwrap();
        let b: IpAddr = "::ffff:192.0.2.2".parse().unwrap();
        assert_ne!(HandshakeLimiter::key(a), HandshakeLimiter::key(b));
        assert_eq!(HandshakeLimiter::key(a), IpAddr::from([192, 0, 2, 1]));
        let mut limiter = HandshakeLimiter::default();
        for _ in 0..PER_IP_TLS_STARTS_PER_SECOND {
            assert!(limiter.admit(a, now));
        }
        assert!(!limiter.admit(a, now));
        assert!(limiter.admit(b, now));
        for kind in [
            io::ErrorKind::InvalidInput,
            io::ErrorKind::NotConnected,
            io::ErrorKind::InvalidData,
        ] {
            assert_eq!(classify(&io::Error::from(kind)), AcceptFailure::Fatal);
        }
        for kind in [
            io::ErrorKind::OutOfMemory,
            io::ErrorKind::PermissionDenied,
            io::ErrorKind::Other,
            io::ErrorKind::TimedOut,
            io::ErrorKind::Unsupported,
            io::ErrorKind::NetworkDown,
            io::ErrorKind::NetworkUnreachable,
            io::ErrorKind::HostUnreachable,
            io::ErrorKind::AddrInUse,
        ] {
            assert_eq!(classify(&io::Error::from(kind)), AcceptFailure::Transient);
        }
    }

    #[test]
    fn key_is_bound_only_when_authentication_completes() {
        let registry = Arc::new(Mutex::new(RegistryState::new(1).unwrap()));
        let mut registration = None;
        let pending = bind_key(Outcome::default(), &registry, &mut registration);
        assert!(registration.is_none() && pending.messages.is_empty());
        assert!(registry.lock().unwrap().entries.is_empty());
        let placeholder = || Outcome {
            messages: vec![Backend::BackendKeyData { pid: 0, secret: 0 }],
            ..Outcome::default()
        };
        let done = bind_key(placeholder(), &registry, &mut registration);
        assert_eq!(registration.as_ref().unwrap().key.pid, 1);
        assert!(matches!(
            done.messages.first(),
            Some(Backend::BackendKeyData { pid: 1, .. })
        ));
        // With the only slot held, a second completion is refused with 53300.
        let mut other = None;
        let full = bind_key(placeholder(), &registry, &mut other);
        assert!(other.is_none() && full.close);
    }

    #[test]
    fn cancel_window_is_independent_of_the_tls_window_and_has_its_own_limits() {
        let now = Instant::now();
        let ip = IpAddr::from([10, 3, 0, 1]);
        let mut cancels =
            HandshakeLimiter::with_limits(GLOBAL_CANCELS_PER_SECOND, PER_IP_CANCELS_PER_SECOND);
        for _ in 0..PER_IP_CANCELS_PER_SECOND {
            assert!(cancels.admit(ip, now));
        }
        assert!(!cancels.admit(ip, now));
        assert!(cancels.admit(IpAddr::from([10, 3, 0, 2]), now));
        let mut many = HandshakeLimiter::with_limits(GLOBAL_CANCELS_PER_SECOND, usize::MAX);
        for n in 0..GLOBAL_CANCELS_PER_SECOND {
            let address = IpAddr::from([10, 4, (n / 256) as u8, (n % 256) as u8]);
            assert!(many.admit(address, now));
        }
        assert!(!many.admit(ip, now));
        assert!(many.admit(ip, now + Duration::from_secs(1)));
    }

    #[cfg(unix)]
    #[test]
    fn secret_files_must_be_private_and_regular() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("latticed-perm-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("key");
        std::fs::write(&path, b"x").unwrap();
        for (mode, ok) in [
            (0o600, true),
            (0o400, true),
            (0o640, false),
            (0o604, false),
            (0o644, false),
            (0o660, false),
        ] {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
            let file = std::fs::File::open(&path).unwrap();
            assert_eq!(require_private_file(&file).is_ok(), ok, "{mode:o}");
        }
        assert!(require_private_file(&std::fs::File::open(&dir).unwrap()).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn route_cancel_charges_the_budget_before_the_key_lookup() {
        let now = Instant::now();
        let registry = Arc::new(Mutex::new(RegistryState::new(1).unwrap()));
        let registration = Registration::new(registry.clone()).unwrap();
        let token = registration.begin_query().unwrap();
        let limits = Limits {
            handshakes: Mutex::new(HandshakeLimiter::default()),
            cancels: Mutex::new(HandshakeLimiter::with_limits(
                GLOBAL_CANCELS_PER_SECOND,
                PER_IP_CANCELS_PER_SECOND,
            )),
        };
        let peer = IpAddr::from([10, 5, 0, 1]);
        let wrong = CancelKey {
            pid: registration.key.pid,
            secret: registration.key.secret ^ 1,
        };
        for _ in 0..PER_IP_CANCELS_PER_SECOND {
            assert!(route_cancel(&limits, &registry, peer, now, wrong).unwrap());
        }
        assert!(!token.is_cancelled());
        // Valid key, but the address has spent its budget on wrong ones: dropped.
        assert!(!route_cancel(&limits, &registry, peer, now, registration.key).unwrap());
        assert!(!token.is_cancelled());
        let later = now + Duration::from_secs(1);
        assert!(route_cancel(&limits, &registry, peer, later, registration.key).unwrap());
        assert!(token.is_cancelled());
    }
}

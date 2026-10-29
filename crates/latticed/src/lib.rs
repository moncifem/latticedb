//! Host TCP adapter for the sans-I/O PostgreSQL session.
//!
//! This is a bounded listener with local development trust or configured SCRAM over mandatory TLS.
//! The default `SELECT 1` handler does not open a database. Cancel requests signal a cooperative
//! handler; they cannot preempt blocking I/O or undo a committed operation.
#![forbid(unsafe_code)]
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

mod transport;
pub use transport::TlsConfig;
use transport::Transport;

use lattice_api::auth::Accounts;
use lattice_api::cancel::{CancelKey, CancelRegistry, CancellationToken};
use lattice_api::pgwire::{self, Backend, Column, Frontend, ProtoError, Startup};
use lattice_api::session::{Outcome, QueryError, QueryHandler, QueryOutput, Session};
use lattice_seams::OsRng;
use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
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
    pub read_timeout: Duration,
    /// Socket write timeout for replies.
    pub write_timeout: Duration,
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
            write_timeout: Duration::from_secs(5),
            startup_timeout: Duration::from_secs(10),
            authentication: Authentication::DevelopmentTrust,
        }
    }
}

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
        let acceptor = thread::Builder::new()
            .name("pg-accept".into())
            .spawn(move || accept(listener, config, stop, registry, Arc::new(factory)))?;
        Ok(Self {
            address,
            stopping,
            acceptor: Some(acceptor),
        })
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

fn accept<H, F>(
    listener: TcpListener,
    config: ServerConfig,
    stopping: Arc<AtomicBool>,
    registry: Registry,
    factory: Arc<F>,
) -> io::Result<()>
where
    H: QueryHandler + 'static,
    F: Fn() -> H + Send + Sync + 'static,
{
    let mut workers: Vec<Worker> = Vec::new();
    let handshakes = Arc::new(Mutex::new(std::collections::VecDeque::new()));
    let result = (|| {
        while !stopping.load(Ordering::Acquire) {
            let mut index = 0;
            while index < workers.len() {
                if workers
                    .get(index)
                    .is_some_and(|worker| worker.thread.is_finished())
                {
                    let worker = workers.swap_remove(index);
                    worker
                        .thread
                        .join()
                        .map_err(|_| io::Error::other("connection worker panicked"))?;
                } else {
                    index += 1;
                }
            }
            match listener.accept() {
                Ok((socket, _)) => {
                    // Accepted sockets can inherit O_NONBLOCK on some hosts (notably macOS).
                    // Workers use blocking I/O with explicit deadlines, independently of accept.
                    socket.set_nonblocking(false)?;
                    socket.set_write_timeout(Some(config.write_timeout))?;
                    if workers.len() == config.max_connections + CONTROL_CONNECTION_RESERVE {
                        // With no classification worker available, close without inventing a reply
                        // to a packet that may be CancelRequest. Normal startup admission below
                        // returns 53300 after the first packet has been classified.
                        let _ = socket.shutdown(Shutdown::Both);
                        continue;
                    }
                    socket.set_nodelay(true)?;
                    let controller = socket.try_clone()?;
                    let connection_config = config.clone();
                    let handshake_limit = handshakes.clone();
                    let deadline = Instant::now()
                        .checked_add(config.startup_timeout)
                        .ok_or_else(|| io::Error::other("startup deadline overflow"))?;
                    let connection_registry = registry.clone();
                    let connection_factory = factory.clone();
                    let worker =
                        thread::Builder::new()
                            .name("pg-connection".into())
                            .spawn(move || {
                                // Disconnects, malformed input and timeouts end this connection, not the listener.
                                let _ = connection(
                                    Transport::new(
                                        socket,
                                        deadline,
                                        connection_config.write_timeout,
                                    ),
                                    connection_config,
                                    connection_registry,
                                    connection_factory,
                                    handshake_limit,
                                );
                            })?;
                    workers.push(Worker {
                        socket: controller,
                        thread: worker,
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5))
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
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

fn frame(
    socket: &mut Transport,
    startup: bool,
    timeout: Duration,
) -> Result<Option<Vec<u8>>, FrameError> {
    frame_bounded(socket, startup, timeout, pgwire::MAX_MESSAGE_LEN)
}

fn frame_bounded(
    socket: &mut Transport,
    startup: bool,
    timeout: Duration,
    maximum: usize,
) -> Result<Option<Vec<u8>>, FrameError> {
    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "read timeout overflow"))?;
    let mut tag = [0u8; 1];
    if !startup && !read_before(socket, &mut tag, deadline)? {
        return Ok(None);
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

fn admit_handshake(starts: &mut std::collections::VecDeque<Instant>, now: Instant) -> bool {
    while starts
        .front()
        .is_some_and(|t| now.duration_since(*t) >= Duration::from_secs(1))
    {
        starts.pop_front();
    }
    if starts.len() == 64 {
        return false;
    }
    starts.push_back(now);
    true
}

fn connection<H, F>(
    mut socket: Transport,
    config: ServerConfig,
    registry: Registry,
    factory: Arc<F>,
    handshakes: Arc<Mutex<std::collections::VecDeque<Instant>>>,
) -> io::Result<()>
where
    H: QueryHandler + 'static,
    F: Fn() -> H + Send + Sync + 'static,
{
    let timeout = config.read_timeout;
    // Classify startup before SQL admission: CancelRequest uses its own short-lived connection and
    // must remain routable while all registered SQL sessions are busy.
    loop {
        let input = match frame(&mut socket, true, timeout) {
            Ok(Some(input)) => input,
            Ok(None) => return Ok(()),
            Err(FrameError::Io(error)) => return Err(error),
            Err(FrameError::Protocol(error)) => {
                send(&mut socket, fatal("08P01", error.to_string()))?;
                return Ok(());
            }
        };
        let startup = match pgwire::decode_startup(&input) {
            Ok(Some((startup, _))) => startup,
            Ok(None) => {
                send(&mut socket, fatal("08P01", "truncated startup"))?;
                return Ok(());
            }
            Err(error) => {
                send(&mut socket, fatal("08P01", error.to_string()))?;
                return Ok(());
            }
        };
        match startup {
            Startup::Cancel { pid, secret } => {
                lock_registry(&registry)?
                    .entries
                    .cancel(CancelKey { pid, secret }, 0);
                let _ = socket.shutdown();
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
                        // Bound expensive TLS starts globally: burst 64, at most 64 in any second.
                        {
                            let mut starts = handshakes
                                .lock()
                                .map_err(|_| io::Error::other("TLS admission unavailable"))?;
                            if !admit_handshake(&mut starts, Instant::now()) {
                                return Err(io::Error::other("TLS handshake rate exceeded"));
                            }
                        }
                        socket.write_all(b"S")?;
                        socket.flush()?;
                        socket = socket.upgrade(tls)?;
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
                    send(&mut socket, fatal("28000", "TLS required"))?;
                    return Ok(());
                }
                let registration = match Registration::new(registry) {
                    Ok(registration) => registration,
                    Err(_) => {
                        send(
                            &mut socket,
                            fatal("53300", "connection registration unavailable"),
                        )?;
                        return Ok(());
                    }
                };
                let mut session = match &config.authentication {
                    Authentication::DevelopmentTrust => {
                        Session::new(factory(), registration.key.pid, registration.key.secret)
                    }
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
                        let (exchange, known) = match exchange {
                            Ok(exchange) => exchange,
                            Err(_) => {
                                send(
                                    &mut socket,
                                    fatal("28P01", "password authentication failed"),
                                )?;
                                return Ok(());
                            }
                        };
                        Session::with_scram(
                            factory(),
                            registration.key.pid,
                            registration.key.secret,
                            exchange,
                            known,
                        )
                    }
                };
                let outcome = session.on_startup(Startup::Params(params));
                let close = outcome.close;
                send(&mut socket, outcome)?;
                if session.sasl_phase().is_none() {
                    socket.authenticated();
                }
                if !close {
                    serve(&mut socket, timeout, &registration, &mut session)?;
                }
                drop(session);
                drop(registration);
                let _ = socket.shutdown();
                return Ok(());
            }
        }
    }
}

fn serve<H: QueryHandler>(
    socket: &mut Transport,
    timeout: Duration,
    registration: &Registration,
    session: &mut Session<H>,
) -> io::Result<()> {
    while !session.is_closed() {
        let input = match frame_bounded(
            socket,
            false,
            timeout,
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
                send(socket, session.on_protocol_error(&error))?;
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
                    registration.begin_query()?
                } else {
                    CancellationToken::default()
                };
                session.on_message_with_cancellation(message, &token)
            }
            Ok(None) => session.on_protocol_error(&ProtoError::Truncated),
            Err(error) => session.on_protocol_error(&error),
        };
        let close = outcome.close;
        send(socket, outcome)?;
        if session.sasl_phase().is_none() && !close {
            socket.authenticated();
        }
        if close {
            break;
        }
    }
    Ok(())
}

fn send(socket: &mut Transport, outcome: Outcome) -> io::Result<()> {
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
    fn describe(&mut self, sql: &str) -> Result<Vec<Column>, QueryError> {
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
    fn tls_start_window_bounds_bursts_and_expires_without_sleeping() {
        let now = Instant::now();
        let mut starts = std::collections::VecDeque::new();
        for _ in 0..64 {
            assert!(admit_handshake(&mut starts, now));
        }
        assert!(!admit_handshake(
            &mut starts,
            now + Duration::from_millis(999)
        ));
        assert!(admit_handshake(&mut starts, now + Duration::from_secs(1)));
        assert_eq!(starts.len(), 1);
    }
}

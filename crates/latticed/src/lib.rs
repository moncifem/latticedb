//! Host TCP adapter for the sans-I/O PostgreSQL session.
//!
//! This is a bounded, trust-authenticated development listener with a `SELECT 1` handler. It does
//! not open a database, authenticate users, serve TLS or implement cancellation requests.
#![forbid(unsafe_code)]
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use lattice_api::pgwire::{self, Backend, Column, ProtoError};
use lattice_api::session::{Outcome, QueryError, QueryHandler, QueryOutput, Session};
use lattice_seams::{OsRng, Rng};
use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// Configuration for the development TCP listener.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// Address to bind. The default listens only on localhost.
    pub address: SocketAddr,
    /// Maximum simultaneous connection workers; connections above it receive SQLSTATE 53300.
    pub max_connections: usize,
    /// Maximum time to receive a complete startup or frontend frame, including slow partial reads.
    pub read_timeout: Duration,
    /// Socket write timeout for replies.
    pub write_timeout: Duration,
}
impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            address: SocketAddr::from(([127, 0, 0, 1], 5432)),
            max_connections: 32,
            read_timeout: Duration::from_secs(30),
            write_timeout: Duration::from_secs(5),
        }
    }
}

/// An active listener. Explicit shutdown and drop both close active sockets and join every worker.
pub struct ServerHandle {
    address: SocketAddr,
    stopping: Arc<AtomicBool>,
    acceptor: Option<JoinHandle<io::Result<()>>>,
}
impl ServerHandle {
    /// Binds before returning, so bind/configuration failures never hide in a background thread.
    pub fn start(config: ServerConfig) -> io::Result<Self> {
        if config.max_connections == 0
            || config.max_connections > 1024
            || config.read_timeout.is_zero()
            || config.write_timeout.is_zero()
            || Instant::now().checked_add(config.read_timeout).is_none()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "nonzero timeouts and 1..=1024 connections required",
            ));
        }
        let listener = TcpListener::bind(config.address)?;
        listener.set_nonblocking(true)?;
        let address = listener.local_addr()?;
        let stopping = Arc::new(AtomicBool::new(false));
        let stop = stopping.clone();
        let acceptor = thread::Builder::new()
            .name("pg-accept".into())
            .spawn(move || accept(listener, config, stop))?;
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

fn accept(
    listener: TcpListener,
    config: ServerConfig,
    stopping: Arc<AtomicBool>,
) -> io::Result<()> {
    let mut workers: Vec<Worker> = Vec::new();
    let mut next_pid = 1i32;
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
                Ok((mut socket, _)) => {
                    // Accepted sockets can inherit O_NONBLOCK on some hosts (notably macOS).
                    // Workers use blocking I/O with explicit deadlines, independently of accept.
                    socket.set_nonblocking(false)?;
                    socket.set_write_timeout(Some(config.write_timeout))?;
                    if workers.len() == config.max_connections {
                        let _ = send(
                            &mut socket,
                            Outcome {
                                messages: vec![Backend::ErrorResponse {
                                    severity: "FATAL".into(),
                                    code: "53300".into(),
                                    message: "too many connections".into(),
                                }],
                                close: true,
                                ..Outcome::default()
                            },
                        );
                        let _ = socket.shutdown(Shutdown::Both);
                        continue;
                    }
                    socket.set_nodelay(true)?;
                    let controller = socket.try_clone()?;
                    let timeout = config.read_timeout;
                    let pid = next_pid;
                    next_pid = next_pid.checked_add(1).unwrap_or(1);
                    let secret = OsRng.next_u64() as i32;
                    let worker =
                        thread::Builder::new()
                            .name(format!("pg-{pid}"))
                            .spawn(move || {
                                // Disconnects, malformed input and timeouts end this connection, not the listener.
                                let _ = connection(socket, timeout, pid, secret);
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
    socket: &mut TcpStream,
    mut target: &mut [u8],
    deadline: Instant,
) -> io::Result<bool> {
    let mut empty = true;
    while !target.is_empty() {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .filter(|duration| !duration.is_zero())
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::TimedOut, "frame read deadline exceeded")
            })?;
        socket.set_read_timeout(Some(remaining))?;
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
    socket: &mut TcpStream,
    startup: bool,
    timeout: Duration,
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
    if length < minimum || i64::from(length) > pgwire::MAX_MESSAGE_LEN as i64 {
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

fn connection(mut socket: TcpStream, timeout: Duration, pid: i32, secret: i32) -> io::Result<()> {
    let mut session = Session::new(SelectOne, pid, secret);
    while !session.is_closed() {
        let input = match frame(&mut socket, session.awaiting_startup(), timeout) {
            Ok(Some(input)) => input,
            Ok(None) => break,
            Err(FrameError::Io(error)) => return Err(error),
            Err(FrameError::Protocol(error)) => {
                send(&mut socket, session.on_protocol_error(&error))?;
                break;
            }
        };
        let outcome = if session.awaiting_startup() {
            match pgwire::decode_startup(&input) {
                Ok(Some((message, _))) => session.on_startup(message),
                Ok(None) => session.on_protocol_error(&ProtoError::Truncated),
                Err(error) => session.on_protocol_error(&error),
            }
        } else {
            match pgwire::decode_frontend(&input) {
                Ok(Some((message, _))) => session.on_message(message),
                Ok(None) => session.on_protocol_error(&ProtoError::Truncated),
                Err(error) => session.on_protocol_error(&error),
            }
        };
        let close = outcome.close;
        send(&mut socket, outcome)?;
        if close {
            break;
        }
    }
    let _ = socket.shutdown(Shutdown::Both);
    Ok(())
}

fn send(socket: &mut TcpStream, outcome: Outcome) -> io::Result<()> {
    if let Some(byte) = outcome.encryption_reply {
        socket.write_all(&[byte])?;
    }
    for message in outcome.messages {
        let mut bytes = Vec::new();
        message.encode(&mut bytes).map_err(io::Error::other)?;
        socket.write_all(&bytes)?;
    }
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

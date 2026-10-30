//! TLS and absolute deadlines belong to the host, outside deterministic protocol state.
use rustls::pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};
use rustls::{ServerConnection, StreamOwned};
use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Server certificate/key configuration. No client-certificate authentication or channel binding.
#[derive(Clone)]
pub struct TlsConfig(Arc<rustls::ServerConfig>);
impl std::fmt::Debug for TlsConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TlsConfig { certificate_and_key: [redacted] }")
    }
}
impl TlsConfig {
    /// Imports a bounded PEM chain and exactly one private key before listener startup.
    pub fn from_pem(certificates: &[u8], key: &[u8]) -> io::Result<Self> {
        if certificates.len() > 65536 || key.len() > 16384 {
            return Err(io::Error::other("TLS configuration exceeds bounds"));
        }
        let certs = CertificateDer::pem_slice_iter(certificates)
            .collect::<Result<Vec<_>, _>>()
            .map_err(io::Error::other)?;
        if certs.is_empty() || certs.len() > 8 {
            return Err(io::Error::other("invalid TLS certificate chain"));
        }
        let mut keys = PrivateKeyDer::pem_slice_iter(key);
        let key = keys
            .next()
            .transpose()
            .map_err(io::Error::other)?
            .ok_or_else(|| io::Error::other("missing TLS private key"))?;
        if keys.next().is_some() {
            return Err(io::Error::other("multiple TLS private keys"));
        }
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .map_err(io::Error::other)?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(io::Error::other)?;
        Ok(Self(Arc::new(config)))
    }
}

/// Largest single socket write, so the response deadline is rechecked often.
const MAX_WRITE_CHUNK: usize = 16 * 1024;
/// Longest the `close_notify` flush may take.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(2);

pub(super) struct TimedSocket {
    socket: TcpStream,
    startup_deadline: Option<Instant>,
    read_deadline: Option<Instant>,
    write_timeout: Duration,
    /// Absolute budget for the response being written; `write_timeout` alone restarts per call, so a
    /// peer that accepts one byte per interval could otherwise hold a worker indefinitely.
    response_deadline: Option<Instant>,
}
fn remaining(deadline: Instant) -> io::Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|d| !d.is_zero())
        .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "connection deadline exceeded"))
}
impl TimedSocket {
    fn check_startup(&self) -> io::Result<()> {
        if let Some(deadline) = self.startup_deadline {
            remaining(deadline)?;
        }
        Ok(())
    }
}
impl Read for TimedSocket {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        let deadline = match (self.startup_deadline, self.read_deadline) {
            (Some(a), Some(b)) => a.min(b),
            (Some(a), None) | (None, Some(a)) => a,
            (None, None) => return Err(io::Error::other("missing read deadline")),
        };
        self.socket.set_read_timeout(Some(remaining(deadline)?))?;
        self.socket.read(bytes)
    }
}
impl Write for TimedSocket {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let mut timeout = self.write_timeout;
        for deadline in [self.startup_deadline, self.response_deadline]
            .into_iter()
            .flatten()
        {
            timeout = timeout.min(remaining(deadline)?);
        }
        self.socket.set_write_timeout(Some(timeout))?;
        // A large blocking send restarts its kernel timeout whenever the peer frees a little space,
        // so cap each call and let `write_all` come back through the deadline checks above.
        let chunk = bytes.get(..MAX_WRITE_CHUNK).unwrap_or(bytes);
        self.socket.write(chunk)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.socket.flush()
    }
}

pub(super) enum Transport {
    Plain(TimedSocket),
    Tls(Box<StreamOwned<ServerConnection, TimedSocket>>),
}
impl Transport {
    pub(super) fn new(socket: TcpStream, deadline: Instant, write_timeout: Duration) -> Self {
        Self::Plain(TimedSocket {
            socket,
            startup_deadline: Some(deadline),
            read_deadline: None,
            write_timeout,
            response_deadline: None,
        })
    }
    fn raw(&mut self) -> &mut TimedSocket {
        match self {
            Self::Plain(socket) => socket,
            Self::Tls(stream) => &mut stream.sock,
        }
    }
    pub(super) fn set_deadline(&mut self, deadline: Instant) {
        self.raw().read_deadline = Some(deadline);
    }
    /// Bounds every write until the next call; `None` removes the bound.
    pub(super) fn set_response_deadline(&mut self, deadline: Option<Instant>) {
        self.raw().response_deadline = deadline;
    }
    pub(super) fn authenticated(&mut self) {
        self.raw().startup_deadline = None;
    }
    pub(super) fn is_tls(&self) -> bool {
        matches!(self, Self::Tls(_))
    }
    /// Closes the connection. A TLS session first sends `close_notify`, so the peer can tell an
    /// orderly close from a truncation attack. That send is best effort: it only writes (never
    /// reads) and is bounded by an absolute `CLOSE_TIMEOUT`, so a peer that drains one byte per
    /// interval cannot hold the worker. The socket is always shut down afterwards.
    pub(super) fn shutdown(&mut self) -> io::Result<()> {
        if let Self::Tls(stream) = self {
            // The startup budget may be what ended the connection; the close has its own cap.
            stream.sock.startup_deadline = None;
            stream.sock.response_deadline = Instant::now().checked_add(CLOSE_TIMEOUT);
            stream.conn.send_close_notify();
            while stream.conn.wants_write() {
                if stream.conn.write_tls(&mut stream.sock).is_err() {
                    break;
                }
            }
        }
        self.raw().socket.shutdown(Shutdown::Both)
    }
    /// Upgrades a plaintext transport to TLS in place. On failure the transport stays plaintext
    /// (over a duplicate handle of the same socket), so the caller can still close it.
    pub(super) fn upgrade_in_place(&mut self, config: &TlsConfig) -> io::Result<()> {
        let Self::Plain(socket) = self else {
            return Err(io::Error::other("TLS already active"));
        };
        let duplicate = TimedSocket {
            socket: socket.socket.try_clone()?,
            startup_deadline: socket.startup_deadline,
            read_deadline: socket.read_deadline,
            write_timeout: socket.write_timeout,
            response_deadline: None,
        };
        let mut socket = std::mem::replace(socket, duplicate);
        socket.read_deadline = socket.startup_deadline;
        let mut connection = ServerConnection::new(config.0.clone()).map_err(io::Error::other)?;
        connection.set_buffer_limit(Some(64 * 1024));
        while connection.is_handshaking() {
            connection.complete_io(&mut socket)?;
        }
        *self = Self::Tls(Box::new(StreamOwned::new(connection, socket)));
        Ok(())
    }
}
impl Read for Transport {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        // rustls may satisfy reads from decrypted buffers without touching its underlying socket.
        self.raw().check_startup()?;
        match self {
            Self::Plain(socket) => socket.read(bytes),
            Self::Tls(stream) => stream.read(bytes),
        }
    }
}
impl Write for Transport {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.raw().check_startup()?;
        match self {
            Self::Plain(socket) => socket.write(bytes),
            Self::Tls(stream) => stream.write(bytes),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        self.raw().check_startup()?;
        match self {
            Self::Plain(socket) => socket.flush(),
            Self::Tls(stream) => stream.flush(),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;
    use std::net::TcpListener;
    use std::thread;

    fn pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server, _) = listener.accept().unwrap();
        (server, client)
    }

    #[test]
    fn response_deadline_bounds_a_peer_that_drains_slowly() {
        let (server, mut client) = pair();
        // Every write call makes progress within `write_timeout`, so only the absolute response
        // budget can stop this peer from holding the writer for the whole payload.
        let reader = thread::spawn(move || {
            let mut buffer = [0u8; 65536];
            while let Ok(count) = client.read(&mut buffer) {
                if count == 0 {
                    break;
                }
                thread::sleep(Duration::from_millis(50));
            }
        });
        let far = Instant::now() + Duration::from_secs(600);
        let mut transport = Transport::new(server, far, Duration::from_secs(5));
        transport.set_response_deadline(Some(Instant::now() + Duration::from_millis(400)));
        let started = Instant::now();
        let payload = vec![0u8; 32 * 1024 * 1024];
        let error = transport.write_all(&payload).unwrap_err();
        // Draining the payload at this pace takes over 20 s; the 400 ms budget ends it, so any bound
        // well under that tells the two apart however loaded the runner is.
        assert!(started.elapsed() < Duration::from_secs(15), "{error}");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        transport.shutdown().unwrap();
        reader.join().unwrap();
    }

    #[test]
    fn clearing_the_response_deadline_restores_per_call_timeouts() {
        let (server, mut client) = pair();
        let far = Instant::now() + Duration::from_secs(600);
        let mut transport = Transport::new(server, far, Duration::from_secs(5));
        transport.set_response_deadline(Some(Instant::now() + Duration::from_millis(1)));
        thread::sleep(Duration::from_millis(5));
        assert!(transport.write_all(b"late").is_err());
        transport.set_response_deadline(None);
        transport.write_all(b"ok").unwrap();
        let mut seen = [0u8; 2];
        client.read_exact(&mut seen).unwrap();
        assert_eq!(&seen, b"ok");
    }

    #[test]
    fn plaintext_shutdown_is_immediate_and_sends_nothing() {
        let (server, mut client) = pair();
        client
            .set_read_timeout(Some(Duration::from_secs(20)))
            .unwrap();
        let far = Instant::now() + Duration::from_secs(600);
        let mut transport = Transport::new(server, far, Duration::from_secs(60));
        // A plaintext transport has no close_notify and no bounded write: the socket is shut down
        // and the peer sees a bare end of stream. The check is on bytes, not on elapsed time.
        transport.shutdown().unwrap();
        let mut seen = Vec::new();
        client.read_to_end(&mut seen).unwrap();
        assert!(seen.is_empty());
    }
}

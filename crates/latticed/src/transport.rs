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

pub(super) struct TimedSocket {
    socket: TcpStream,
    startup_deadline: Option<Instant>,
    read_deadline: Option<Instant>,
    write_timeout: Duration,
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
        let timeout = match self.startup_deadline {
            Some(deadline) => self.write_timeout.min(remaining(deadline)?),
            None => self.write_timeout,
        };
        self.socket.set_write_timeout(Some(timeout))?;
        self.socket.write(bytes)
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
    pub(super) fn authenticated(&mut self) {
        self.raw().startup_deadline = None;
    }
    pub(super) fn is_tls(&self) -> bool {
        matches!(self, Self::Tls(_))
    }
    pub(super) fn shutdown(&mut self) -> io::Result<()> {
        self.raw().socket.shutdown(Shutdown::Both)
    }
    pub(super) fn upgrade(self, config: &TlsConfig) -> io::Result<Self> {
        let Self::Plain(mut socket) = self else {
            return Err(io::Error::other("TLS already active"));
        };
        socket.read_deadline = socket.startup_deadline;
        let mut connection = ServerConnection::new(config.0.clone()).map_err(io::Error::other)?;
        connection.set_buffer_limit(Some(64 * 1024));
        while connection.is_handshaking() {
            connection.complete_io(&mut socket)?;
        }
        Ok(Self::Tls(Box::new(StreamOwned::new(connection, socket))))
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

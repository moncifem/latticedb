//! End-to-end harness for `latticed`: the sync gateway and the SQL listener run on loopback sockets
//! over a temporary data directory, with account fixtures, raw clients for both protocols and seeded
//! kill schedules.
//!
//! The server starts in process through `SqlServer::start_with_gateway`, the call the `latticed`
//! binary makes. A crate other than `latticed` cannot name the built binary (`CARGO_BIN_EXE_latticed`
//! is set only for `latticed`'s own tests), and `latticed/tests/ws_sync.rs` already starts it this
//! way. Every byte the clients see crosses a real TCP socket.

pub mod json;
pub mod mcp;
pub mod schedule;

pub use mcp::Mcp;
pub use schedule::kill_points;

use lattice_api::bearer::{CredentialStore, issue};
use lattice_api::exec_host::{Grant, HostLimits, PolicyTable};
use lattice_api::sync::{self, Event, Message};
use lattice_api::websocket::{self, Opcode};
use lattice_seams::OsRng;
use latticed::ws::{GatewayAuth, GatewayConfig};
use latticed::{ServerConfig, SqlConfig, SqlServer};
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::ops::RangeInclusive;
use std::path::PathBuf;
use std::time::Duration;

/// Errors are rendered as text: the harness reports them to the test that called it.
pub type Result<T> = std::result::Result<T, String>;

/// The `Sec-WebSocket-Key` the clients send. The server needs only a well-formed value.
const KEY: &str = "dGhlIHNhbXBsZSBub25jZQ==";

/// One read attempt on a socket. Waits are counted in attempts rather than measured with a clock,
/// so a slow machine stretches a wait instead of failing it, and the harness reads no time.
const POLL: Duration = Duration::from_millis(50);

/// Read attempts a client makes before it reports a timeout (about ten seconds).
const PATIENCE: u32 = 200;

fn text<E: std::fmt::Debug>(error: E) -> String {
    format!("{error:?}")
}

fn mint(store: &mut CredentialStore, account: &str) -> Result<String> {
    let (token, entry) = issue(account, 1, None, &mut OsRng).map_err(text)?;
    store.insert(entry).map_err(text)?;
    Ok(token.expose_secret().to_string())
}

/// The bearer tokens of the principals a fixture issues.
pub struct Tokens {
    /// Holds `*=rwd` in the policy file: every table.
    pub alice: String,
    /// Holds `notes=r,kv=r` in the policy file: read-only on `notes` and `kv`.
    pub bob: String,
}

/// A running gateway and SQL listener over a fresh data directory. Dropping it stops the server and
/// removes the directory.
pub struct Fixture {
    server: Option<SqlServer>,
    /// The PostgreSQL wire listener.
    pub sql: SocketAddr,
    /// The WebSocket sync gateway, path `/sync`.
    pub ws: SocketAddr,
    /// The tokens of alice and bob.
    pub tokens: Tokens,
    dir: PathBuf,
}

impl Fixture {
    /// Starts a server for the scenario `name`. The data directory is keyed by name and process id,
    /// so tests in one binary do not share state.
    pub fn start(name: &str) -> Result<Fixture> {
        Fixture::start_batched(name, GatewayConfig::default().batch_events)
    }

    /// Like [`Fixture::start`], with at most `batch_events` events in each `Events` frame (the
    /// gateway accepts `1..=256`), so a test knows where a frame boundary falls.
    pub fn start_batched(name: &str, batch_events: usize) -> Result<Fixture> {
        let dir = std::env::temp_dir().join(format!("lattice-e2e-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut store = CredentialStore::empty();
        let alice = mint(&mut store, "alice")?;
        let bob = mint(&mut store, "bob")?;
        let accounts =
            PolicyTable::parse_accounts("alice\ttables: *=rwd\nbob\ttables: notes=r,kv=r\n")
                .map_err(text)?;
        let gateway = GatewayConfig {
            address: SocketAddr::from(([127, 0, 0, 1], 0)),
            auth: GatewayAuth::Bearer(store),
            slice: Duration::from_millis(10),
            fallback_poll: Duration::from_millis(50),
            batch_events,
            ..GatewayConfig::default()
        };
        let server = SqlServer::start_with_gateway(
            ServerConfig {
                address: SocketAddr::from(([127, 0, 0, 1], 0)),
                ..ServerConfig::default()
            },
            SqlConfig {
                data_dir: dir.clone(),
                policies: PolicyTable::new(Grant::AllowAll).with_accounts(accounts),
                limits: HostLimits::default(),
                allow_format_upgrade: false,
            },
            Some(gateway),
        )
        .map_err(text)?;
        let sql = server.local_addr();
        let ws = server
            .gateway_addr()
            .ok_or("the sync gateway did not start")?;
        Ok(Fixture {
            server: Some(server),
            sql,
            ws,
            tokens: Tokens { alice, bob },
            dir,
        })
    }

    /// A new SQL connection to the listener (trust authentication, development only).
    pub fn writer(&self) -> Result<Pg> {
        Pg::connect(self.sql)
    }

    /// Creates `notes` if it is missing and writes one row `(n, 'vNNNN')` for each `n` in `rows`.
    pub fn seed(&self, rows: RangeInclusive<u32>) -> Result<()> {
        let mut pg = self.writer()?;
        pg.run("CREATE TABLE IF NOT EXISTS notes (id INT PRIMARY KEY, body TEXT)")?;
        for n in rows {
            pg.run(&format!("INSERT INTO notes VALUES ({n}, 'v{n:04}')"))?;
        }
        Ok(())
    }

    /// Creates `kv` if it is missing: the key/value shape the MCP tools address (`key text PRIMARY
    /// KEY`, `value text`, `version bigint NOT NULL`). Alice may write it and bob may read it.
    pub fn create_kv(&self) -> Result<()> {
        self.writer()?.run(
            "CREATE TABLE IF NOT EXISTS kv (key TEXT PRIMARY KEY, value TEXT, version BIGINT NOT NULL)",
        )
    }

    /// A client for `POST /mcp` on this fixture's gateway, with `token` as the bearer credential.
    /// `None` sends no credential.
    pub fn mcp(&self, token: Option<&str>) -> Mcp {
        Mcp::new(self.ws, token)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(server) = self.server.take() {
            let _ = server.shutdown();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A PostgreSQL connection that speaks enough of protocol 3.0 to run simple queries. It reports the
/// server's error text when a statement fails.
pub struct Pg(TcpStream);

impl Pg {
    /// Connects as user `dev` and waits for the server to be ready for a query.
    pub fn connect(address: SocketAddr) -> Result<Pg> {
        let mut socket =
            TcpStream::connect_timeout(&address, Duration::from_secs(2)).map_err(text)?;
        socket
            .set_read_timeout(Some(Duration::from_secs(10)))
            .map_err(text)?;
        let mut params = vec![0, 3, 0, 0];
        params.extend_from_slice(b"user\0dev\0\0");
        let mut startup = ((params.len() + 4) as u32).to_be_bytes().to_vec();
        startup.extend_from_slice(&params);
        socket.write_all(&startup).map_err(text)?;
        loop {
            let (tag, body) = read_message(&mut socket)?;
            if tag == b'E' {
                return Err(error_text(&body));
            }
            if tag == b'Z' {
                return Ok(Pg(socket));
            }
        }
    }

    /// Runs one statement and waits for the ready message. A server error becomes an `Err` that
    /// carries the statement and the server's text.
    pub fn run(&mut self, sql: &str) -> Result<()> {
        let mut query = sql.as_bytes().to_vec();
        query.push(0);
        self.0.write_all(&frame(b'Q', &query)).map_err(text)?;
        loop {
            let (tag, body) = read_message(&mut self.0)?;
            if tag == b'E' {
                return Err(format!("{sql}: {}", error_text(&body)));
            }
            if tag == b'Z' {
                return Ok(());
            }
        }
    }
}

fn frame(tag: u8, body: &[u8]) -> Vec<u8> {
    let mut bytes = vec![tag];
    bytes.extend_from_slice(&((body.len() + 4) as u32).to_be_bytes());
    bytes.extend_from_slice(body);
    bytes
}

fn read_message(stream: &mut TcpStream) -> Result<(u8, Vec<u8>)> {
    let mut header = [0u8; 5];
    stream.read_exact(&mut header).map_err(text)?;
    let [tag, a, b, c, d] = header;
    let length = u32::from_be_bytes([a, b, c, d]) as usize;
    let size = length
        .checked_sub(4)
        .ok_or("a message shorter than its own length field")?;
    let mut body = vec![0; size];
    stream.read_exact(&mut body).map_err(text)?;
    Ok((tag, body))
}

fn error_text(body: &[u8]) -> String {
    String::from_utf8_lossy(body).into_owned()
}

/// What a sync client receives next.
#[derive(Debug, PartialEq)]
pub enum Got {
    /// A decoded sync message.
    Sync(Message),
    /// A close frame, with its status code when it carries one.
    Close(Option<u16>),
    /// The server closed the socket.
    Eof,
}

/// A WebSocket sync client on a raw socket. It frames messages with the shared codec and judges
/// every reply itself.
pub struct Ws {
    socket: TcpStream,
    buf: Vec<u8>,
}

impl Ws {
    /// Upgrades a connection to `/sync` with `token` as the bearer credential. A refused upgrade is
    /// an `Err` that carries the response head.
    pub fn open(address: SocketAddr, token: &str) -> Result<Ws> {
        let request = format!(
            "GET /sync HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
             Sec-WebSocket-Key: {KEY}\r\nSec-WebSocket-Version: 13\r\n\
             Sec-WebSocket-Protocol: lattice.sync.v1\r\nAuthorization: Bearer {token}\r\n\r\n"
        );
        let (status, head, socket) = http(address, request.as_bytes())?;
        if status != 101 {
            return Err(format!("upgrade refused: {head}"));
        }
        Ok(Ws {
            socket,
            buf: Vec::new(),
        })
    }

    /// Sends the hello and expects a welcome for protocol version 1.
    pub fn hello(&mut self, agent: &str) -> Result<()> {
        self.send(&Message::Hello {
            min_version: 1,
            max_version: 1,
            agent: agent.into(),
        })?;
        match self.next_within(PATIENCE)? {
            Some(Got::Sync(Message::Welcome { version: 1, .. })) => Ok(()),
            other => Err(format!("expected Welcome, got {other:?}")),
        }
    }

    /// Subscribes `sub` to `scope` from the start of its history.
    pub fn subscribe(&mut self, sub: u32, scope: &str) -> Result<()> {
        self.send(&Message::Subscribe {
            sub,
            scope: scope.into(),
            cursor: Vec::new(),
        })
    }

    /// Subscribes `sub` to `scope`, resuming after `cursor` (empty starts from the beginning).
    pub fn subscribe_from(&mut self, sub: u32, scope: &str, cursor: &[u8]) -> Result<()> {
        self.send(&Message::Subscribe {
            sub,
            scope: scope.into(),
            cursor: cursor.to_vec(),
        })
    }

    /// Tells the server that everything up to and including `cursor` of `sub` has been processed.
    pub fn ack(&mut self, sub: u32, cursor: &[u8]) -> Result<()> {
        self.send(&Message::Ack {
            sub,
            cursor: cursor.to_vec(),
        })
    }

    /// The events of the next `Events` frame of `sub`, exactly as the server framed them. Errors
    /// if anything else arrives, or nothing does within the patience of the client.
    pub fn frame_events(&mut self, sub: u32) -> Result<Vec<Event>> {
        match self.next_within(PATIENCE)? {
            Some(Got::Sync(Message::Events { sub: s, events })) if s == sub => Ok(events),
            other => Err(format!("expected an events frame for {sub}, got {other:?}")),
        }
    }

    /// Reads until `count` events of subscription `sub` have arrived and returns them all.
    pub fn events(&mut self, sub: u32, count: usize) -> Result<Vec<Event>> {
        let mut got = Vec::new();
        while got.len() < count {
            match self.next_within(PATIENCE)? {
                Some(Got::Sync(Message::Events { sub: s, events })) if s == sub => {
                    got.extend(events)
                }
                other => return Err(format!("expected events for {sub}, got {other:?}")),
            }
        }
        Ok(got)
    }

    /// The next frame from the server, waiting at most `attempts` read attempts. Pings are answered
    /// and do not count as a frame. `Ok(None)` means nothing arrived in time.
    pub fn next_within(&mut self, attempts: u32) -> Result<Option<Got>> {
        self.socket.set_read_timeout(Some(POLL)).map_err(text)?;
        for _ in 0..attempts {
            if let Some(got) = self.decode_buffered()? {
                return Ok(Some(got));
            }
            let mut chunk = [0u8; 4096];
            match self.socket.read(&mut chunk) {
                Ok(0) => return Ok(Some(Got::Eof)),
                Ok(n) => self
                    .buf
                    .extend_from_slice(chunk.get(..n).unwrap_or_default()),
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) => {}
                Err(_) => return Ok(Some(Got::Eof)),
            }
        }
        Ok(None)
    }

    fn decode_buffered(&mut self) -> Result<Option<Got>> {
        loop {
            let Some((frame, used)) =
                websocket::decode_server_frame(&self.buf, 1 << 22).map_err(text)?
            else {
                return Ok(None);
            };
            self.buf.drain(..used);
            match frame.opcode {
                Opcode::Binary => {
                    let (message, n) = sync::decode(&frame.payload)
                        .map_err(text)?
                        .ok_or("an incomplete sync message")?;
                    if n != frame.payload.len() {
                        return Err("bytes after a sync message".into());
                    }
                    return Ok(Some(Got::Sync(message)));
                }
                Opcode::Close => {
                    let code = frame
                        .payload
                        .get(..2)
                        .and_then(|c| <[u8; 2]>::try_from(c).ok())
                        .map(u16::from_be_bytes);
                    return Ok(Some(Got::Close(code)));
                }
                Opcode::Ping => self.send_raw(Opcode::Pong, &frame.payload)?,
                _ => {}
            }
        }
    }

    fn send(&mut self, message: &Message) -> Result<()> {
        let mut bytes = Vec::new();
        sync::encode(message, &mut bytes).map_err(text)?;
        self.send_raw(Opcode::Binary, &bytes)
    }

    fn send_raw(&mut self, opcode: Opcode, payload: &[u8]) -> Result<()> {
        let mut out = Vec::new();
        websocket::encode_client_frame(true, opcode, payload, [1, 2, 3, 4], &mut out)
            .map_err(text)?;
        self.socket.write_all(&out).map_err(text)
    }
}

/// Sends one HTTP request and reads the response head, byte by byte, so no frame bytes that follow
/// it are consumed. Returns the status, the head and the socket.
fn http(address: SocketAddr, request: &[u8]) -> Result<(u16, String, TcpStream)> {
    let mut socket = TcpStream::connect_timeout(&address, Duration::from_secs(2)).map_err(text)?;
    socket
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(text)?;
    socket.write_all(request).map_err(text)?;
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        socket.read_exact(&mut byte).map_err(text)?;
        head.extend_from_slice(&byte);
    }
    let head = String::from_utf8(head).map_err(text)?;
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| format!("no status line in {head:?}"))?;
    Ok((status, head, socket))
}

/// The row value (`vNNNN`) carried in an event body, if it has one.
pub fn row_value(event: &Event) -> Option<String> {
    fn is_row_value(window: &[u8]) -> bool {
        matches!(window, [b'v', digits @ ..] if digits.len() == 4 && digits.iter().all(u8::is_ascii_digit))
    }
    let at = event.data.windows(5).position(is_row_value)?;
    let bytes = event.data.get(at..at + 5)?;
    String::from_utf8(bytes.to_vec()).ok()
}

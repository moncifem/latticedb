//! The sync gateway over real sockets: a raw WebSocket client written here (it frames messages with
//! the shared codec's client encoder, but builds the HTTP request and judges every response itself)
//! against `SqlServer::start_with_gateway`, with rows written through the PostgreSQL listener.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::disallowed_methods,
    clippy::disallowed_types
)]

use lattice_api::bearer::{CredentialStore, TokenId, issue};
use lattice_api::exec_host::{Grant, HostLimits, PolicyTable};
use lattice_api::sync::{self, ErrorCode, Event, Message};
use lattice_api::websocket::{self, Opcode, close_code};
use lattice_seams::OsRng;
#[cfg(unix)]
use latticed::load_credential_store;
#[cfg(unix)]
use latticed::write_credential_store;
use latticed::ws::{GatewayAuth, GatewayConfig};
use latticed::{ServerConfig, SqlConfig, SqlServer};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

struct Tokens {
    alice: String,
    bob: String,
    carol: String,
}

struct Fixture {
    server: Option<SqlServer>,
    sql: SocketAddr,
    ws: SocketAddr,
    tokens: Tokens,
    dir: PathBuf,
    /// The directory of the credential file, when the test serves tokens from one.
    creds: Option<PathBuf>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(server) = self.server.take() {
            let _ = server.shutdown();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
        if let Some(creds) = &self.creds {
            let _ = std::fs::remove_dir_all(creds);
        }
    }
}

fn store_and_tokens() -> (CredentialStore, Tokens) {
    let mut store = CredentialStore::empty();
    let mut make = |account: &str| {
        let (token, entry) = issue(account, 1, None, &mut OsRng).unwrap();
        store.insert(entry).unwrap();
        token.expose_secret().to_string()
    };
    let tokens = Tokens {
        alice: make("alice"),
        bob: make("bob"),
        carol: make("carol"),
    };
    (store, tokens)
}

fn start(name: &str, tune: impl FnOnce(&mut GatewayConfig)) -> Fixture {
    start_gateway(name, false, tune).0
}

/// A gateway whose tokens come from a credential file (mode 0600 in a 0700 directory). The test
/// edits the file; the gateway has to notice on its own.
#[cfg(unix)]
fn start_file(name: &str, tune: impl FnOnce(&mut GatewayConfig)) -> (Fixture, PathBuf) {
    let (fixture, file) = start_gateway(name, true, tune);
    (fixture, file.expect("a credential file was requested"))
}

fn start_gateway(
    name: &str,
    from_file: bool,
    tune: impl FnOnce(&mut GatewayConfig),
) -> (Fixture, Option<PathBuf>) {
    let dir = std::env::temp_dir().join(format!("latticed-ws-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let (store, tokens) = store_and_tokens();
    // carol has a token but no entry in the policy file, so she is denied everything.
    let accounts =
        PolicyTable::parse_accounts("alice\ttables: *=rwd\nbob\ttables: notes=r\n").unwrap();
    let (auth, creds, file) = if from_file {
        let (creds, path) = credential_file(name, &store);
        (
            GatewayAuth::BearerFile(path.clone()),
            Some(creds),
            Some(path),
        )
    } else {
        (GatewayAuth::Bearer(store), None, None)
    };
    let mut gateway = GatewayConfig {
        address: SocketAddr::from(([127, 0, 0, 1], 0)),
        auth,
        slice: Duration::from_millis(10),
        fallback_poll: Duration::from_millis(50),
        revalidate_every: Duration::from_millis(100),
        credential_poll: Duration::from_millis(50),
        failure_limit: 3,
        batch_events: 3,
        ..GatewayConfig::default()
    };
    tune(&mut gateway);
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
    .unwrap();
    let fixture = Fixture {
        sql: server.local_addr(),
        ws: server.gateway_addr().unwrap(),
        server: Some(server),
        tokens,
        dir,
        creds,
    };
    (fixture, file)
}

/// A fresh 0700 directory holding `bearer-tokens` with `store`; returns the directory and the file.
#[cfg(unix)]
fn credential_file(name: &str, store: &CredentialStore) -> (PathBuf, PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("latticed-ws-{name}-creds-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let file = dir.join("bearer-tokens");
    write_credential_store(&file, store).unwrap();
    (dir, file)
}

#[cfg(not(unix))]
fn credential_file(_name: &str, _store: &CredentialStore) -> (PathBuf, PathBuf) {
    unreachable!("the credential file tests run on unix only (no ACL check yet, ADR 0031)")
}

/// Applies `edit` to the store the file holds, and writes it back the way the CLI does.
#[cfg(unix)]
fn edit_file(file: &Path, edit: impl FnOnce(&mut CredentialStore)) {
    let mut store = load_credential_store(file).unwrap();
    edit(&mut store);
    write_credential_store(file, &store).unwrap();
}

/// The lookup id of a token: the 16 hex digits after `lat_`.
fn id_of(token: &str) -> TokenId {
    TokenId::parse(&token[4..20]).unwrap()
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

// --- a minimal PostgreSQL trust client, enough to run statements ---------------------------------

fn pg_frame(tag: u8, body: &[u8]) -> Vec<u8> {
    let mut bytes = vec![tag];
    bytes.extend_from_slice(&((body.len() + 4) as u32).to_be_bytes());
    bytes.extend_from_slice(body);
    bytes
}

fn pg_message(stream: &mut TcpStream) -> (u8, Vec<u8>) {
    let mut header = [0; 5];
    stream.read_exact(&mut header).unwrap();
    let length = u32::from_be_bytes(header[1..5].try_into().unwrap()) as usize;
    let mut body = vec![0; length - 4];
    stream.read_exact(&mut body).unwrap();
    (header[0], body)
}

struct Pg(TcpStream);

impl Pg {
    fn connect(address: SocketAddr) -> Pg {
        let mut socket = TcpStream::connect_timeout(&address, Duration::from_secs(2)).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut startup = vec![0, 0, 0, 0, 0, 3, 0, 0];
        startup.extend_from_slice(b"user\0dev\0\0");
        let length = (startup.len() as u32).to_be_bytes();
        startup[..4].copy_from_slice(&length);
        socket.write_all(&startup).unwrap();
        loop {
            let (tag, _) = pg_message(&mut socket);
            assert_ne!(tag, b'E');
            if tag == b'Z' {
                return Pg(socket);
            }
        }
    }

    fn run(&mut self, sql: &str) {
        let mut body = sql.as_bytes().to_vec();
        body.push(0);
        self.0.write_all(&pg_frame(b'Q', &body)).unwrap();
        loop {
            let (tag, body) = pg_message(&mut self.0);
            assert_ne!(tag, b'E', "{sql}: {}", String::from_utf8_lossy(&body));
            if tag == b'Z' {
                return;
            }
        }
    }
}

fn seed(fixture: &Fixture, rows: std::ops::RangeInclusive<u32>) {
    let mut pg = Pg::connect(fixture.sql);
    for table in ["notes", "secret"] {
        pg.run(&format!(
            "CREATE TABLE IF NOT EXISTS {table} (id INT PRIMARY KEY, body TEXT)"
        ));
    }
    for n in rows {
        pg.run(&format!("INSERT INTO notes VALUES ({n}, 'v{n:04}')"));
    }
}

// --- a raw WebSocket client ----------------------------------------------------------------------

const KEY: &str = "dGhlIHNhbXBsZSBub25jZQ==";

fn request(path: &str, extra: &str) -> String {
    format!(
        "GET {path} HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {KEY}\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Protocol: lattice.sync.v1\r\n{extra}\r\n"
    )
}

fn bearer(token: &str) -> String {
    format!("Authorization: Bearer {token}\r\n")
}

/// Sends `text` and returns the status code and the whole response head, and the socket.
fn http(address: SocketAddr, text: &[u8]) -> (u16, String, TcpStream) {
    let mut socket = TcpStream::connect_timeout(&address, Duration::from_secs(2)).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    socket.write_all(text).unwrap();
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        socket.read_exact(&mut byte).unwrap();
        head.push(byte[0]);
    }
    let head = String::from_utf8(head).unwrap();
    let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    (status, head, socket)
}

struct Ws {
    socket: TcpStream,
    buf: Vec<u8>,
}

#[derive(Debug, PartialEq)]
enum Got {
    Sync(Message),
    Close(Option<u16>),
    Eof,
}

impl Ws {
    fn open(address: SocketAddr, token: &str) -> Ws {
        let (status, head, socket) = http(address, request("/sync", &bearer(token)).as_bytes());
        assert_eq!(status, 101, "{head}");
        Ws {
            socket,
            buf: Vec::new(),
        }
    }

    fn send_raw(&mut self, opcode: Opcode, payload: &[u8]) {
        let mut out = Vec::new();
        websocket::encode_client_frame(true, opcode, payload, [1, 2, 3, 4], &mut out).unwrap();
        self.socket.write_all(&out).unwrap();
    }

    fn send(&mut self, message: &Message) {
        let mut bytes = Vec::new();
        sync::encode(message, &mut bytes).unwrap();
        self.send_raw(Opcode::Binary, &bytes);
    }

    fn hello(&mut self, agent: &str) {
        self.send(&Message::Hello {
            min_version: 1,
            max_version: 1,
            agent: agent.into(),
        });
        match self.next(Duration::from_secs(5)) {
            Got::Sync(Message::Welcome { version: 1, .. }) => {}
            other => panic!("expected Welcome, got {other:?}"),
        }
    }

    fn subscribe(&mut self, sub: u32, scope: &str, cursor: &[u8]) {
        self.send(&Message::Subscribe {
            sub,
            scope: scope.into(),
            cursor: cursor.to_vec(),
        });
    }

    /// The next sync message, close or end of stream; pongs and pings are skipped.
    fn next(&mut self, within: Duration) -> Got {
        self.next_opt(within)
            .expect("timed out waiting for a message")
    }

    fn next_opt(&mut self, within: Duration) -> Option<Got> {
        let until = Instant::now() + within;
        loop {
            if let Some((frame, used)) = websocket::decode_server_frame(&self.buf, 1 << 22).unwrap()
            {
                self.buf.drain(..used);
                match frame.opcode {
                    Opcode::Binary => {
                        let (message, n) = sync::decode(&frame.payload).unwrap().unwrap();
                        assert_eq!(n, frame.payload.len());
                        return Some(Got::Sync(message));
                    }
                    Opcode::Close => {
                        let code = frame
                            .payload
                            .get(..2)
                            .map(|c| u16::from_be_bytes([c[0], c[1]]));
                        return Some(Got::Close(code));
                    }
                    Opcode::Ping => self.send_raw(Opcode::Pong, &frame.payload),
                    _ => {}
                }
                continue;
            }
            if Instant::now() >= until {
                return None;
            }
            self.socket
                .set_read_timeout(Some(Duration::from_millis(50)))
                .unwrap();
            let mut chunk = [0u8; 4096];
            match self.socket.read(&mut chunk) {
                Ok(0) => return Some(Got::Eof),
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(_) => return Some(Got::Eof),
            }
        }
    }

    /// Events of one subscription until `count` have arrived; acks nothing.
    fn events(&mut self, sub: u32, count: usize) -> Vec<Event> {
        let mut got = Vec::new();
        while got.len() < count {
            match self.next(Duration::from_secs(10)) {
                Got::Sync(Message::Events { sub: s, events }) => {
                    assert_eq!(s, sub);
                    got.extend(events);
                }
                other => panic!("expected events, got {other:?}"),
            }
        }
        got
    }
}

fn row_of(event: &Event) -> String {
    let at = event
        .data
        .windows(5)
        .position(|w| w[0] == b'v' && w[1..].iter().all(u8::is_ascii_digit))
        .expect("a row value in the event");
    String::from_utf8(event.data[at..at + 5].to_vec()).unwrap()
}

// --- the tests -------------------------------------------------------------------------------------

#[test]
fn two_principals_subscribe_and_only_the_allowed_one_receives_events() {
    let f = start("principals", |_| {});
    seed(&f, 1..=2);
    let mut bob = Ws::open(f.ws, &f.tokens.bob);
    bob.hello("bob-client");
    bob.subscribe(1, "table/notes", b"");
    let events = bob.events(1, 2);
    assert_eq!(
        events.iter().map(row_of).collect::<Vec<_>>(),
        ["v0001", "v0002"]
    );
    assert!(events.iter().all(|e| e.cursor.len() == 12));

    // A row committed later reaches the live subscription without a new request.
    Pg::connect(f.sql).run("INSERT INTO notes VALUES (3, 'v0003')");
    assert_eq!(row_of(&bob.events(1, 1)[0]), "v0003");

    // carol holds a valid token and no grant: her answer is the answer for a table that is not there.
    let mut carol = Ws::open(f.ws, &f.tokens.carol);
    carol.hello("carol-client");
    carol.subscribe(1, "table/notes", b"");
    carol.subscribe(2, "table/nothing", b"");
    let mut errors = Vec::new();
    for _ in 0..2 {
        match carol.next(Duration::from_secs(5)) {
            Got::Sync(Message::Error { code, sub, message }) => errors.push((sub, code, message)),
            other => panic!("expected errors, got {other:?}"),
        }
    }
    errors.sort_by_key(|(sub, _, _)| *sub);
    assert_eq!(errors[0].1, ErrorCode::InvalidScope);
    assert_eq!(errors[0].1, errors[1].1);
    assert_eq!(
        errors[0].2, errors[1].2,
        "the text must not tell the two apart"
    );
}

#[test]
fn a_commit_wakes_a_subscriber_without_waiting_for_the_fallback_poll() {
    // The fallback is far longer than the test; only the commit notifier can deliver in time.
    let f = start("wake", |c| c.fallback_poll = Duration::from_secs(60));
    seed(&f, 1..=1);
    let mut ws = Ws::open(f.ws, &f.tokens.alice);
    ws.hello("a");
    ws.subscribe(1, "table/notes", b"");
    assert_eq!(row_of(&ws.events(1, 1)[0]), "v0001");
    Pg::connect(f.sql).run("INSERT INTO notes VALUES (2, 'v0002')");
    let started = Instant::now();
    assert_eq!(row_of(&ws.events(1, 1)[0]), "v0002");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
}

#[test]
fn the_hello_agent_name_does_not_change_the_principal() {
    let f = start("agent", |_| {});
    seed(&f, 1..=1);
    let mut carol = Ws::open(f.ws, &f.tokens.carol);
    // She claims to be the account that may read.
    carol.hello("bob");
    carol.subscribe(1, "table/notes", b"");
    match carol.next(Duration::from_secs(5)) {
        Got::Sync(Message::Error {
            code: ErrorCode::InvalidScope,
            sub: 1,
            ..
        }) => {}
        other => panic!("expected invalid_scope, got {other:?}"),
    }
}

#[test]
fn upgrade_without_valid_token_is_refused_and_counts_against_the_rate_limit() {
    let f = start("ratelimit", |_| {});
    let missing = request("/sync", "");
    let wrong = request(
        "/sync",
        &bearer("lat_0000000000000000_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"),
    );
    let basic = request("/sync", "Authorization: Basic Zm9vOmJhcg==\r\n");
    let mut texts = Vec::new();
    for req in [&missing, &wrong, &basic] {
        let (status, head, _) = http(f.ws, req.as_bytes());
        assert_eq!(status, 401, "{head}");
        assert!(head.contains("WWW-Authenticate: Bearer"));
        texts.push(head);
    }
    assert_eq!(texts[0], texts[1]);
    assert_eq!(texts[1], texts[2]);
    // Three failures used the address up: even the right token is not looked at now.
    let (status, _, _) = http(f.ws, request("/sync", &bearer(&f.tokens.alice)).as_bytes());
    assert_eq!(status, 429);
}

#[test]
fn two_authorization_headers_and_a_token_in_the_url_are_refused() {
    let f = start("urltoken", |c| c.failure_limit = 100);
    let both = format!("{}{}", bearer(&f.tokens.alice), bearer(&f.tokens.alice));
    let (status, _, _) = http(f.ws, request("/sync", &both).as_bytes());
    assert_eq!(status, 401);
    let (status, head, _) = http(
        f.ws,
        request(&format!("/sync?token={}", f.tokens.alice), "").as_bytes(),
    );
    assert_eq!(status, 400, "{head}");
    assert!(!head.contains(&f.tokens.alice));
    let (status, _, _) = http(f.ws, request("/other", &bearer(&f.tokens.alice)).as_bytes());
    assert_eq!(status, 404);
}

#[test]
fn wrong_subprotocol_or_origin_is_refused() {
    let f = start("origin", |_| {});
    let auth = bearer(&f.tokens.alice);
    let (status, _, _) = http(
        f.ws,
        request("/sync", &format!("Origin: https://evil.example\r\n{auth}")).as_bytes(),
    );
    assert_eq!(status, 403);
    let other = request("/sync", &auth).replace("lattice.sync.v1", "chat");
    let (status, _, _) = http(f.ws, other.as_bytes());
    assert_eq!(status, 400);
    let (status, _, _) = http(
        f.ws,
        request("/sync", &auth)
            .replace("Version: 13", "Version: 8")
            .as_bytes(),
    );
    assert_eq!(status, 426);
}

#[test]
fn oversize_header_block_is_refused_before_authentication() {
    let f = start("oversize", |_| {});
    let big = format!("X-Padding: {}\r\n", "a".repeat(9000));
    let (status, _, _) = http(f.ws, request("/sync", &big).as_bytes());
    assert_eq!(status, 431);
    let many: String = (0..70).map(|i| format!("X-H{i}: 1\r\n")).collect();
    let (status, _, _) = http(f.ws, request("/sync", &many).as_bytes());
    assert!(matches!(status, 400 | 431), "{status}");
    // Neither was a failed authentication: the right token still works.
    let mut ok = Ws::open(f.ws, &f.tokens.alice);
    ok.hello("a");
}

#[test]
fn non_binary_message_closes_1003() {
    let f = start("text", |_| {});
    let mut ws = Ws::open(f.ws, &f.tokens.alice);
    ws.send_raw(Opcode::Text, b"hello");
    assert_eq!(
        ws.next(Duration::from_secs(5)),
        Got::Close(Some(close_code::UNSUPPORTED))
    );
}

#[test]
fn a_message_that_is_not_exactly_one_sync_frame_closes_1002() {
    let f = start("frame", |_| {});
    let mut ws = Ws::open(f.ws, &f.tokens.alice);
    let mut two = Vec::new();
    for _ in 0..2 {
        sync::encode(&Message::Ping { nonce: 1 }, &mut two).unwrap();
    }
    ws.send_raw(Opcode::Binary, &two);
    match ws.next(Duration::from_secs(5)) {
        Got::Sync(Message::Error {
            code: ErrorCode::Malformed,
            sub: 0,
            ..
        }) => {}
        other => panic!("expected malformed, got {other:?}"),
    }
    assert_eq!(
        ws.next(Duration::from_secs(5)),
        Got::Close(Some(close_code::PROTOCOL))
    );
}

#[test]
fn slow_upgrade_is_cut_by_the_handshake_timeout() {
    let f = start("slow", |c| c.handshake_timeout = Duration::from_millis(300));
    let mut socket = TcpStream::connect(f.ws).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let started = Instant::now();
    // Trickle a request head that never ends.
    for byte in b"GET /sync HTTP/1.1\r\nHo" {
        if socket.write_all(&[*byte]).is_err() {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let mut sink = [0u8; 64];
    assert_eq!(
        socket.read(&mut sink).unwrap_or(0),
        0,
        "the server must hang up"
    );
    assert!(started.elapsed() < Duration::from_secs(4));
}

#[test]
fn the_connection_cap_refuses_the_extra_upgrade_with_503() {
    let f = start("cap", |c| {
        c.max_connections = 1;
        c.max_handshakes = 1;
    });
    let _first = Ws::open(f.ws, &f.tokens.alice);
    let (status, _, _) = http(f.ws, request("/sync", &bearer(&f.tokens.alice)).as_bytes());
    assert_eq!(status, 503);
}

#[test]
fn a_non_loopback_gateway_address_is_refused_at_start() {
    let dir = std::env::temp_dir().join(format!("latticed-ws-remote-{}", std::process::id()));
    let result = SqlServer::start_with_gateway(
        ServerConfig {
            address: SocketAddr::from(([127, 0, 0, 1], 0)),
            ..ServerConfig::default()
        },
        SqlConfig {
            data_dir: dir.clone(),
            policies: PolicyTable::new(Grant::DenyAll),
            limits: HostLimits::default(),
            allow_format_upgrade: false,
        },
        Some(GatewayConfig {
            address: SocketAddr::from(([0, 0, 0, 0], 0)),
            ..GatewayConfig::default()
        }),
    );
    let error = result.err().expect("must refuse").to_string();
    assert!(error.contains("loopback"), "{error}");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn reconnect_with_cursor_has_no_gap_and_no_duplicate() {
    let f = start("resume", |_| {});
    seed(&f, 1..=20);
    // The whole feed, read once by a connection that stays.
    let mut reference = Ws::open(f.ws, &f.tokens.alice);
    reference.hello("ref");
    reference.subscribe(1, "table/notes", b"");
    let full = reference.events(1, 20);
    assert_eq!(
        full.iter().map(row_of).collect::<Vec<_>>(),
        (1..=20).map(|n| format!("v{n:04}")).collect::<Vec<_>>()
    );

    // A client reads part of the history, acknowledges up to its fifth event, and is killed.
    let mut first = Ws::open(f.ws, &f.tokens.alice);
    first.hello("first");
    first.subscribe(1, "table/notes", b"");
    let seen = first.events(1, 7);
    assert_eq!(
        seen,
        full[..seen.len()].to_vec(),
        "same order, same cursors"
    );
    let acked = seen[4].cursor.clone();
    first.send(&Message::Ack {
        sub: 1,
        cursor: acked.clone(),
    });
    // Let the ack be read, then drop the socket in the middle of a batch.
    std::thread::sleep(Duration::from_millis(100));
    drop(first);

    // New rows arrive while it is away.
    Pg::connect(f.sql).run("INSERT INTO notes VALUES (21, 'v0021')");
    let mut again = Ws::open(f.ws, &f.tokens.alice);
    again.hello("again");
    again.subscribe(1, "table/notes", &acked);
    let resumed = again.events(1, 16);
    let expected: Vec<String> = (6..=21).map(|n| format!("v{n:04}")).collect();
    assert_eq!(resumed.iter().map(row_of).collect::<Vec<_>>(), expected);
    assert_eq!(resumed[..15].to_vec(), full[5..].to_vec());
    // Nothing more is pending.
    assert_eq!(
        again.next_opt(Duration::from_millis(300)),
        None,
        "no duplicate may follow"
    );
}

#[test]
fn replacing_the_credentials_closes_a_live_connection_whose_token_no_longer_verifies() {
    let f = start("revoked", |_| {});
    let mut ws = Ws::open(f.ws, &f.tokens.alice);
    ws.hello("a");
    let server = f.server.as_ref().unwrap();
    assert!(
        server
            .gateway()
            .unwrap()
            .replace_credentials(CredentialStore::empty())
    );
    assert_eq!(
        ws.next(Duration::from_secs(5)),
        Got::Close(Some(close_code::POLICY))
    );
}

#[test]
fn shutdown_closes_live_connections_with_going_away() {
    let mut f = start("shutdown", |_| {});
    let mut ws = Ws::open(f.ws, &f.tokens.alice);
    ws.hello("a");
    let server = f.server.take().unwrap();
    let stopper = std::thread::spawn(move || server.shutdown().unwrap());
    assert_eq!(
        ws.next(Duration::from_secs(5)),
        Got::Close(Some(close_code::GOING_AWAY))
    );
    stopper.join().unwrap();
}

#[test]
fn a_revoked_slow_reader_is_cut_between_frames_not_at_the_end_of_the_step() {
    // The step is the whole table: one pump of ROWS frames, one event each, of about ROW_BYTES. The
    // reader takes frames as they come until it has counted REVOKE_AFTER events, then the credential
    // is replaced. The writer checks the credential before every frame, so after the revocation it
    // sends nothing beyond the frames already written. Nothing here waits on the clock: each wait is
    // bounded per message, and the assertion is a count.
    //
    // The count is derived from constants. Every frame is at least ROW_BYTES, so the frames that can
    // sit unread in the socket buffers of both ends are at most SOCKET_BUFFERS / ROW_BYTES. SOCKET_BUFFERS
    // is an assumption about the OS (not measured on the CI hosts here): 16 MiB is above the default
    // maxima of the common kernels. A step that is not cut delivers all ROWS events, and ROWS is larger
    // than CUT_BOUND, so that case fails.
    const ROWS: usize = 1_000;
    const ROW_BYTES: usize = 30_000;
    const REVOKE_AFTER: usize = 40;
    const SOCKET_BUFFERS: usize = 16 << 20;
    // Frames received when the revocation is seen, plus the frames in the buffers, plus one frame
    // written when the revocation happened and one frame partly read by the reader.
    const CUT_BOUND: usize = REVOKE_AFTER + SOCKET_BUFFERS / ROW_BYTES + 2;
    const _: () = assert!(CUT_BOUND < ROWS);
    let f = start("revoked-slow-reader", |c| {
        c.write_timeout = Duration::from_secs(10);
        c.response_timeout = Duration::from_secs(120);
        c.revalidate_every = Duration::from_millis(100);
        c.batch_events = 1;
        c.max_frames_per_pump = ROWS;
        c.max_subscriptions = 1;
    });
    {
        let mut pg = Pg::connect(f.sql);
        pg.run("CREATE TABLE notes (id INT PRIMARY KEY, body TEXT)");
        let big = "x".repeat(ROW_BYTES);
        for n in 1..=ROWS {
            pg.run(&format!("INSERT INTO notes VALUES ({n}, '{big}')"));
        }
    }
    let mut ws = Ws::open(f.ws, &f.tokens.alice);
    ws.hello("slow");
    ws.subscribe(1, "table/notes", b"");
    let started = Instant::now();
    let mut events = 0usize;
    let mut revoked = false;
    let code = loop {
        assert!(
            started.elapsed() < Duration::from_secs(40),
            "the stream never ended after {events} events"
        );
        // The server builds the whole pump (1,000 frames) before it sends the first one, so the
        // first wait covers that work; the 40 s check above is the only overall bound.
        match ws.next_opt(Duration::from_secs(30)) {
            Some(Got::Sync(Message::Events { events: batch, .. })) => {
                events += batch.len();
                if !revoked && events >= REVOKE_AFTER {
                    // The step is in flight: the writer is at most SOCKET_BUFFERS ahead of this reader.
                    let server = f.server.as_ref().unwrap();
                    assert!(
                        server
                            .gateway()
                            .unwrap()
                            .replace_credentials(CredentialStore::empty())
                    );
                    revoked = true;
                }
            }
            Some(Got::Close(code)) => break code,
            other => panic!("expected events or a close after {events} events, got {other:?}"),
        }
    };
    assert!(revoked, "the stream ended before the revocation");
    assert_eq!(code, Some(close_code::POLICY));
    assert!(
        events <= CUT_BOUND,
        "{events} of {ROWS} events arrived; a connection cut between frames delivers at most {CUT_BOUND}"
    );
}

// --- live credential changes (issue #577, ADR 0031) -------------------------------------------------
//
// The tests below edit the credential file on disk and expect the running gateway to act without a
// restart. The periodic revalidation is set to a minute in the first one, so the only thing that can
// close the connection in time is the file reload.

#[cfg(unix)]
#[test]
fn revoked_token_closes_live_gateway_connections() {
    let (f, file) = start_file("revoke-live", |g| {
        g.revalidate_every = Duration::from_secs(60);
    });
    let mut alice = Ws::open(f.ws, &f.tokens.alice);
    alice.hello("a");
    let mut bob = Ws::open(f.ws, &f.tokens.bob);
    bob.hello("b");
    let alice_id = id_of(&f.tokens.alice);
    edit_file(&file, |store| assert!(store.revoke(alice_id)));
    // The reload is at most one poll interval away; the close follows at once.
    assert_eq!(
        alice.next(Duration::from_secs(5)),
        Got::Close(Some(close_code::POLICY))
    );
    // Bob's token is still valid and his connection stays open.
    assert_eq!(bob.next_opt(Duration::from_millis(400)), None);
    // A new upgrade with the revoked token gets the same 401 as any unknown token.
    let (status, head, _) = http(f.ws, request("/sync", &bearer(&f.tokens.alice)).as_bytes());
    assert_eq!(status, 401, "{head}");
}

#[cfg(unix)]
#[test]
fn an_expiring_token_closes_its_live_connection_at_the_expiry() {
    let (f, file) = start_file("expire-live", |_| {});
    let mut alice = Ws::open(f.ws, &f.tokens.alice);
    alice.hello("a");
    let alice_id = id_of(&f.tokens.alice);
    let expires = unix_now() + 2;
    edit_file(&file, |store| store.set_expiry(alice_id, expires).unwrap());
    assert_eq!(
        alice.next(Duration::from_secs(8)),
        Got::Close(Some(close_code::POLICY))
    );
    // Not before the expiry second: a token that verifies is never cut early.
    assert!(
        unix_now() >= expires,
        "closed before the token expired at {expires}"
    );
}

#[cfg(unix)]
#[test]
fn a_broken_credential_file_keeps_the_tokens_in_use() {
    let (f, file) = start_file("keep-live", |_| {});
    let mut alice = Ws::open(f.ws, &f.tokens.alice);
    alice.hello("a");
    // A garbage file is refused (it is not an empty store, which would revoke everything): several
    // polls pass and alice keeps her connection and her token.
    std::fs::write(&file, b"not a credential file\n").unwrap();
    assert_eq!(alice.next_opt(Duration::from_millis(600)), None);
    let (status, head, _) = http(f.ws, request("/sync", &bearer(&f.tokens.alice)).as_bytes());
    assert_eq!(status, 101, "{head}");
}

#[cfg(unix)]
#[test]
fn a_missing_credential_file_stops_the_start() {
    let dir = std::env::temp_dir().join(format!("latticed-ws-missing-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let result = SqlServer::start_with_gateway(
        ServerConfig {
            address: SocketAddr::from(([127, 0, 0, 1], 0)),
            ..ServerConfig::default()
        },
        SqlConfig {
            data_dir: dir.join("data"),
            policies: PolicyTable::new(Grant::AllowAll),
            limits: HostLimits::default(),
            allow_format_upgrade: false,
        },
        Some(GatewayConfig {
            address: SocketAddr::from(([127, 0, 0, 1], 0)),
            auth: GatewayAuth::BearerFile(dir.join("no-such-file")),
            ..GatewayConfig::default()
        }),
    );
    let message = match result {
        Ok(_) => panic!("a gateway without its token file must not start"),
        Err(error) => error.to_string(),
    };
    assert!(message.contains("no-such-file"), "{message}");
    let _ = std::fs::remove_dir_all(&dir);
}

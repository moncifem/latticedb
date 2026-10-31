//! The gateway's per-principal limits over real sockets (issue #529, part 1): connections per
//! principal, the share of places kept for principals that hold none, and the cap on `lattice_subscribe`
//! long polls.
//!
//! Nothing here asserts a duration. A test waits for a state to be reached (the number of open
//! connections, the number of long polls waiting, both read from the gateway) with a bounded loop and
//! then asserts on the state and on the gateway's refusal counters. The expected numbers are worked out
//! from the configuration of each test, not read from a run.
#![cfg(unix)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::disallowed_methods,
    clippy::disallowed_types
)]

use lattice_api::bearer::{CredentialStore, issue, verify_bearer};
use lattice_api::exec_host::{ExecHost, Grant, HostLimits, PolicyTable};
use lattice_seams::OsRng;
use latticed::ws::{GatewayAuth, GatewayConfig, GatewayHandle, LimitRefusals};
use latticed::{ServerConfig, SqlConfig, SqlServer};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::thread::JoinHandle;
use std::time::Duration;

/// `admin` may do everything (it creates the tables and wakes the long polls); `alice` and `bob` read
/// and write or read `kv`; `carol` holds `other`.
const ACCOUNTS: &str =
    "admin\tallow-all\nalice\ttables: kv=rw\nbob\ttables: kv=r\ncarol\ttables: other=rw\n";
const KV: &str = "CREATE TABLE kv (key TEXT PRIMARY KEY, value TEXT, version BIGINT NOT NULL)";
const OTHER: &str =
    "CREATE TABLE other (key TEXT PRIMARY KEY, value TEXT, version BIGINT NOT NULL)";
const NAMES: [&str; 4] = ["admin", "alice", "bob", "carol"];

const META: &str = r#""_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}"#;
const ZERO_CURSOR: &str = "000000000000000000000000";
const KEY: &str = "dGhlIHNhbXBsZSBub25jZQ==";

struct Fixture {
    server: Option<SqlServer>,
    port: u16,
    tokens: BTreeMap<&'static str, String>,
    dir: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(server) = self.server.take() {
            let _ = server.shutdown();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Fixture {
    fn token(&self, name: &str) -> String {
        self.tokens[name].clone()
    }

    fn gateway(&self) -> &GatewayHandle {
        self.server.as_ref().unwrap().gateway().unwrap()
    }

    fn refusals(&self) -> LimitRefusals {
        self.gateway().limit_refusals()
    }

    /// Waits, in bounded steps, until the gateway holds `n` open connections. The loop only decides
    /// when to look; the assertion is on the state.
    fn settle_connections(&self, n: usize) {
        settle(|| self.gateway().open_connections() == n);
        assert_eq!(self.gateway().open_connections(), n);
    }

    fn settle_polls(&self, n: usize) {
        settle(|| self.gateway().long_polls_waiting() == n);
        assert_eq!(self.gateway().long_polls_waiting(), n);
    }
}

fn settle(done: impl Fn() -> bool) {
    for _ in 0..4000 {
        if done() {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn private_dir(path: &Path) {
    let _ = std::fs::remove_dir_all(path);
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .unwrap();
}

fn policies() -> PolicyTable {
    PolicyTable::new(Grant::DenyAll).with_accounts(PolicyTable::parse_accounts(ACCOUNTS).unwrap())
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn start(name: &str, tune: impl FnOnce(&mut GatewayConfig)) -> Fixture {
    let dir =
        std::env::temp_dir().join(format!("latticed-ws-limits-{name}-{}", std::process::id()));
    private_dir(&dir);
    let mut store = CredentialStore::empty();
    let mut tokens = BTreeMap::new();
    for account in NAMES {
        let (token, entry) = issue(account, 1, None, &mut OsRng).unwrap();
        store.insert(entry).unwrap();
        tokens.insert(account, token.expose_secret().to_owned());
    }
    // Create the tables with the executor, then close it so the server can open the directory.
    let host = ExecHost::start_dir(dir.clone(), policies(), HostLimits::default()).unwrap();
    let header = format!("Bearer {}", tokens["admin"]);
    let admin = verify_bearer(Some(header.as_bytes()), &store, unix_now()).unwrap();
    for ddl in [KV, OTHER] {
        host.sql_for(admin.clone()).run(ddl, &[]).unwrap();
    }
    host.shutdown().unwrap();

    let mut gateway = GatewayConfig {
        address: SocketAddr::from(([127, 0, 0, 1], 0)),
        auth: GatewayAuth::Bearer(store),
        slice: Duration::from_millis(10),
        failure_limit: 100,
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
            policies: policies(),
            limits: HostLimits::default(),
            allow_format_upgrade: false,
        },
        Some(gateway),
    )
    .unwrap();
    Fixture {
        port: server.gateway_addr().unwrap().port(),
        server: Some(server),
        tokens,
        dir,
    }
}

// --- a WebSocket upgrade, judged from its bytes ---------------------------------------------------

struct Upgraded {
    status: u16,
    body: String,
    /// Kept open so the connection keeps its place.
    _socket: TcpStream,
}

fn upgrade(port: u16, token: &str) -> Upgraded {
    let address = SocketAddr::from(([127, 0, 0, 1], port));
    let mut socket = TcpStream::connect_timeout(&address, Duration::from_secs(2)).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let request = format!(
        "GET /sync HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {KEY}\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Protocol: lattice.sync.v1\r\nAuthorization: Bearer {token}\r\n\r\n"
    );
    socket.write_all(request.as_bytes()).unwrap();
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        socket.read_exact(&mut byte).unwrap();
        head.push(byte[0]);
    }
    let head = String::from_utf8(head).unwrap();
    let status: u16 = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    let mut body = String::new();
    if status != 101 {
        let _ = socket.read_to_string(&mut body);
    }
    Upgraded {
        status,
        body,
        _socket: socket,
    }
}

// --- POST /mcp ------------------------------------------------------------------------------------

struct Resp {
    status: u16,
    head: String,
    body: String,
}

fn post(port: u16, token: &str, body: &str) -> Resp {
    let address = SocketAddr::from(([127, 0, 0, 1], port));
    let mut socket = TcpStream::connect_timeout(&address, Duration::from_secs(2)).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    let request = format!(
        "POST /mcp HTTP/1.1\r\nHost: localhost:{port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nAuthorization: Bearer {token}\r\n\r\n{body}",
        body.len()
    );
    let _ = socket.write_all(request.as_bytes());
    let mut raw = Vec::new();
    let _ = socket.read_to_end(&mut raw);
    let text = String::from_utf8_lossy(&raw).into_owned();
    let (head, body) = match text.split_once("\r\n\r\n") {
        Some((h, b)) => (h.to_owned(), b.to_owned()),
        None => (text.clone(), String::new()),
    };
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    Resp { status, head, body }
}

fn tool(id: u32, name: &str, arguments: &str) -> String {
    format!(
        r#"{{"jsonrpc":"2.0","id":{id},"method":"tools/call","params":{{{META},"name":"{name}","arguments":{arguments}}}}}"#
    )
}

fn subscribe(table: &str, wait_ms: u32) -> String {
    tool(
        1,
        "lattice_subscribe",
        &format!(
            r#"{{"scope":"default","table":"{table}","after":"{ZERO_CURSOR}","wait_ms":{wait_ms}}}"#
        ),
    )
}

/// A long poll on its own thread, so the test can look at the gateway while it waits.
fn long_poll(f: &Fixture, who: &str, table: &str) -> JoinHandle<Resp> {
    let (port, token, body) = (f.port, f.token(who), subscribe(table, 30_000));
    std::thread::spawn(move || post(port, &token, &body))
}

/// A write by `admin` to `table`, which wakes every long poll waiting on it.
fn commit(f: &Fixture, table: &str, key: &str) {
    let args = format!(r#"{{"scope":"default","table":"{table}","key":"{key}","value":"v"}}"#);
    let r = post(f.port, &f.token("admin"), &tool(2, "lattice_write", &args));
    assert_eq!(r.status, 200, "{}", r.head);
    assert!(r.body.contains(r#"\"applied\":true"#), "{}", r.body);
}

// --- connections ----------------------------------------------------------------------------------

#[test]
fn one_principal_cannot_exhaust_connections_for_others() {
    let f = start("per-principal", |c| {
        c.max_connections = 8;
        c.max_handshakes = 8;
        c.max_connections_per_principal = 2;
        c.reserved_connections = 0;
    });
    let a1 = upgrade(f.port, &f.token("alice"));
    let a2 = upgrade(f.port, &f.token("alice"));
    assert_eq!((a1.status, a2.status), (101, 101));

    // The third connection of alice is refused before the upgrade, 6 places of the gateway are free.
    let a3 = upgrade(f.port, &f.token("alice"));
    assert_eq!(a3.status, 503);
    assert!(a3.body.contains("too many connections"), "{}", a3.body);
    assert_eq!(
        f.refusals(),
        LimitRefusals {
            per_principal: 1,
            ..LimitRefusals::default()
        }
    );

    // Another principal is not affected by alice's two.
    let b1 = upgrade(f.port, &f.token("bob"));
    assert_eq!(b1.status, 101);

    // A place comes back when a connection closes: alice, bob and the refused one are the only others.
    drop(a1);
    f.settle_connections(2);
    let a4 = upgrade(f.port, &f.token("alice"));
    assert_eq!(a4.status, 101);
    drop((a2, b1, a4));
}

#[test]
fn the_reserved_share_is_kept_for_a_principal_that_holds_no_connection() {
    // 4 places, 2 of them reserved: a principal that holds a connection may take places while at most
    // 2 are open, so alice reaches 2 and no further, and bob and carol still get one each.
    let f = start("reserved", |c| {
        c.max_connections = 4;
        c.max_handshakes = 4;
        c.max_connections_per_principal = 4;
        c.reserved_connections = 2;
    });
    let a1 = upgrade(f.port, &f.token("alice"));
    let a2 = upgrade(f.port, &f.token("alice"));
    assert_eq!((a1.status, a2.status), (101, 101));
    let a3 = upgrade(f.port, &f.token("alice"));
    assert_eq!(a3.status, 503);
    assert!(a3.body.contains("gateway is full"), "{}", a3.body);
    assert_eq!(f.refusals().reserved_share, 1);
    assert_eq!(f.refusals().per_principal, 0);

    // The refused connection must have left before the next one counts the open ones.
    f.settle_connections(2);
    let b = upgrade(f.port, &f.token("bob"));
    let c = upgrade(f.port, &f.token("carol"));
    assert_eq!((b.status, c.status), (101, 101));

    // The fifth connection finds all 4 places used: refused at accept, before any token is read, and
    // counted apart.
    f.settle_connections(4);
    let admin = upgrade(f.port, &f.token("admin"));
    assert_eq!(admin.status, 503);
    assert_eq!(f.refusals().global, 1);
    drop((a1, a2, b, c));
}

// --- long polls -----------------------------------------------------------------------------------

#[test]
fn long_poll_workers_are_capped_per_principal_and_globally() {
    let f = start("long-polls", |c| {
        c.max_long_polls_per_principal = 1;
        c.max_long_polls = 2;
    });
    let alice = long_poll(&f, "alice", "kv");
    f.settle_polls(1);

    // A second long poll of alice is answered 503 at once and does not wait.
    let r = post(f.port, &f.token("alice"), &subscribe("kv", 30_000));
    assert_eq!(r.status, 503, "{}", r.head);
    assert!(r.head.contains("Retry-After: 1"), "{}", r.head);
    assert!(
        r.body.contains("this account has too many long polls"),
        "{}",
        r.body
    );

    // A subscribe that does not wait takes no place, so alice's cap does not touch it.
    let r = post(f.port, &f.token("alice"), &subscribe("kv", 0));
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(f.gateway().long_polls_waiting(), 1);

    // Another principal has its own place; the gateway's two are then used.
    let bob = long_poll(&f, "bob", "kv");
    f.settle_polls(2);
    let r = post(f.port, &f.token("carol"), &subscribe("other", 30_000));
    assert_eq!(r.status, 503, "{}", r.head);
    assert!(
        r.body.contains("gateway has too many long polls"),
        "{}",
        r.body
    );
    assert_eq!(f.refusals().long_polls, 2);

    // A commit ends both waits, and the places come back.
    commit(&f, "kv", "k1");
    for waiting in [alice, bob] {
        let r = waiting.join().unwrap();
        assert_eq!(r.status, 200, "{}", r.head);
        // The wait ended because an event arrived, not because it ran out.
        assert!(r.body.contains(r#"\"events\":["#), "{}", r.body);
        assert!(!r.body.contains(r#"\"events\":[]"#), "{}", r.body);
    }
    f.settle_polls(0);

    // And a new long poll is admitted again. It waits on `other`, which is still empty: a poll on `kv`
    // would answer at once from the events already there.
    let again = long_poll(&f, "carol", "other");
    f.settle_polls(1);
    commit(&f, "other", "k2");
    assert_eq!(again.join().unwrap().status, 200);
    f.settle_polls(0);
    assert_eq!(f.refusals().long_polls, 2);
}

#[test]
fn an_mcp_request_counts_against_the_connections_of_its_principal() {
    let f = start("mcp-connections", |c| {
        c.max_connections_per_principal = 1;
        c.max_long_polls_per_principal = 4;
    });
    // The long poll holds alice's one connection while it waits.
    let alice = long_poll(&f, "alice", "kv");
    f.settle_polls(1);
    let list = format!(r#"{{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{{{META}}}}}"#);
    let r = post(f.port, &f.token("alice"), &list);
    assert_eq!(r.status, 503, "{}", r.head);
    assert!(r.body.contains("too many connections"), "{}", r.body);
    assert_eq!(f.refusals().per_principal, 1);
    // Bob is another principal.
    assert_eq!(post(f.port, &f.token("bob"), &list).status, 200);

    commit(&f, "kv", "k1");
    assert_eq!(alice.join().unwrap().status, 200);
    f.settle_polls(0);
    f.settle_connections(0);
    // The connection came back with the poll.
    assert_eq!(post(f.port, &f.token("alice"), &list).status, 200);
}

#[test]
fn a_client_that_never_reads_its_response_keeps_its_principal_share() {
    // Per-principal cap 1. Alice's first request is answered, but she keeps the socket open and does
    // not close it, so the connection lingers. Its principal share must live as long as its global
    // place does: a second request of alice while it lingers is refused, not served. The response
    // is too small to block the write (it is bounded under the socket buffers), so the linger is what
    // keeps the connection, and the test lengthens it to 60 s instead of racing the default 500 ms.
    let f = start("linger-share", |c| {
        c.max_connections_per_principal = 1;
        // The linger is held open until the first socket is dropped, so no window decides the outcome.
        c.mcp_linger = Duration::from_secs(60);
    });
    let list = format!(r#"{{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{{{META}}}}}"#);
    let address = SocketAddr::from(([127, 0, 0, 1], f.port));
    let mut first = TcpStream::connect_timeout(&address, Duration::from_secs(2)).unwrap();
    first
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let request = format!(
        "POST /mcp HTTP/1.1\r\nHost: localhost:{}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nAuthorization: Bearer {}\r\n\r\n{list}",
        f.port,
        list.len(),
        f.token("alice")
    );
    first.write_all(request.as_bytes()).unwrap();
    // Read the head only: the response is written, the connection is now in its linger.
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        first.read_exact(&mut byte).unwrap();
        head.push(byte[0]);
    }
    assert!(head.starts_with(b"HTTP/1.1 200"));
    let second = post(f.port, &f.token("alice"), &list);
    assert_eq!(second.status, 503, "{}", second.head);
    assert_eq!(f.refusals().per_principal, 1);
    drop(first);
}

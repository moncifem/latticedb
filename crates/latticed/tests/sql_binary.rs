//! SQL through the real `latticed` binary, over TCP, with a plain PostgreSQL v3 client written here
//! (no driver): create, insert and select against a data directory, row-policy denial, the SCRAM/TLS
//! path with per-account policies, and what a stop or a crash leaves behind.
//!
//! The client frames messages itself, so a mistake in the server's framing cannot cancel out a
//! matching mistake in a shared codec.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::disallowed_methods,
    clippy::disallowed_types
)]

use base64::{Engine as _, engine::general_purpose::STANDARD};
use hmac::{Hmac, KeyInit, Mac};
use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned, pki_types::ServerName};
use sha2::{Digest, Sha256};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// A scratch directory removed when the test ends.
struct Scratch(PathBuf);
impl Scratch {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!("latticed-sql-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Scratch(path)
    }
    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
    fn arg(&self, name: &str) -> String {
        self.path(name).to_string_lossy().into_owned()
    }
    /// A file readable by its owner only, as the server requires of key material.
    fn private_file(&self, name: &str, bytes: &[u8]) -> String {
        use std::os::unix::fs::OpenOptionsExt;
        let path = self.path(name);
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        file.write_all(bytes).unwrap();
        path.to_string_lossy().into_owned()
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A running `latticed`. Dropping it kills the process so a failed test leaves nothing behind.
struct Server {
    child: Child,
    stdin: Option<ChildStdin>,
    address: SocketAddr,
    banner: String,
}

fn start(args: &[&str]) -> Result<Server, String> {
    let mut child = Command::new(env!("CARGO_BIN_EXE_latticed"))
        .args(["--listen", "127.0.0.1:0"])
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut banner = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut banner)
        .unwrap();
    if banner.is_empty() {
        let mut error = String::new();
        child
            .stderr
            .take()
            .unwrap()
            .read_to_string(&mut error)
            .unwrap();
        let _ = child.wait();
        return Err(error);
    }
    let address = banner
        .split_whitespace()
        .nth(3)
        .unwrap_or_else(|| panic!("no address in banner {banner:?}"))
        .parse()
        .unwrap();
    let stdin = child.stdin.take();
    Ok(Server {
        child,
        stdin,
        address,
        banner,
    })
}

impl Server {
    /// Asks for a clean stop on stdin and returns what the process wrote to stderr.
    fn stop(mut self) -> String {
        let mut stdin = self.stdin.take().unwrap();
        stdin.write_all(b"shutdown\n").unwrap();
        stdin.flush().unwrap();
        let until = Instant::now() + Duration::from_secs(10);
        let status = loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                break status;
            }
            assert!(Instant::now() < until, "did not stop after shutdown");
            std::thread::sleep(Duration::from_millis(20));
        };
        assert!(status.success(), "unclean exit: {status}");
        let mut error = String::new();
        self.child
            .stderr
            .take()
            .unwrap()
            .read_to_string(&mut error)
            .unwrap();
        error
    }

    /// SIGKILL: no shutdown path runs. A test that uses it shows bytes reached the operating system
    /// (the files can be read back), not that they were synced to disk.
    fn crash(mut self) -> String {
        self.child.kill().unwrap();
        self.child.wait().unwrap();
        let mut error = String::new();
        self.child
            .stderr
            .take()
            .unwrap()
            .read_to_string(&mut error)
            .unwrap();
        error
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// --- a minimal PostgreSQL v3 client -------------------------------------------------------------

fn frame(tag: u8, body: &[u8]) -> Vec<u8> {
    let mut bytes = vec![tag];
    bytes.extend_from_slice(&((body.len() + 4) as u32).to_be_bytes());
    bytes.extend_from_slice(body);
    bytes
}

fn startup(user: &str) -> Vec<u8> {
    let mut bytes = vec![0, 0, 0, 0, 0, 3, 0, 0];
    bytes.extend_from_slice(b"user\0");
    bytes.extend_from_slice(user.as_bytes());
    bytes.extend_from_slice(b"\0\0");
    let length = (bytes.len() as u32).to_be_bytes();
    bytes[..4].copy_from_slice(&length);
    bytes
}

fn message(stream: &mut impl Read) -> (u8, Vec<u8>) {
    let mut header = [0; 5];
    stream.read_exact(&mut header).unwrap();
    let length = u32::from_be_bytes(header[1..5].try_into().unwrap()) as usize;
    assert!((4..=1 << 20).contains(&length));
    let mut body = vec![0; length - 4];
    stream.read_exact(&mut body).unwrap();
    (header[0], body)
}

/// What one simple-query round trip produced.
#[derive(Debug, Default)]
struct Reply {
    columns: Vec<String>,
    rows: Vec<Vec<Option<String>>>,
    tags: Vec<String>,
    /// SQLSTATE and message of an `ErrorResponse`.
    error: Option<(String, String)>,
    /// The `ReadyForQuery` status byte.
    status: u8,
}

impl Reply {
    fn text(&self) -> Vec<Vec<String>> {
        self.rows
            .iter()
            .map(|r| {
                r.iter()
                    .map(|v| v.clone().unwrap_or("NULL".into()))
                    .collect()
            })
            .collect()
    }
    fn code(&self) -> Option<&str> {
        self.error.as_ref().map(|(c, _)| c.as_str())
    }
}

fn cstrings(mut body: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    while !body.is_empty() {
        let end = body.iter().position(|b| *b == 0).unwrap();
        out.push(String::from_utf8(body[..end].to_vec()).unwrap());
        body = &body[end + 1..];
    }
    out
}

/// Sends one simple query and reads up to `ReadyForQuery`.
fn query(stream: &mut (impl Read + Write), sql: &str) -> Reply {
    let mut body = sql.as_bytes().to_vec();
    body.push(0);
    stream.write_all(&frame(b'Q', &body)).unwrap();
    stream.flush().unwrap();
    read_reply(stream)
}

/// Parse plus Describe of the statement, then Sync: what a driver sends to learn result columns.
fn parse_describe(stream: &mut (impl Read + Write), sql: &str) -> Reply {
    let mut parse = vec![0];
    parse.extend_from_slice(sql.as_bytes());
    parse.extend_from_slice(&[0, 0, 0]);
    let mut flight = frame(b'P', &parse);
    flight.extend(frame(b'D', b"S\0"));
    flight.extend(frame(b'S', b""));
    stream.write_all(&flight).unwrap();
    stream.flush().unwrap();
    read_reply(stream)
}

/// Extended protocol: Parse, Bind (text parameters, text results), Execute and Sync in one flight.
fn extended(stream: &mut (impl Read + Write), sql: &str, params: &[Option<&str>]) -> Reply {
    let mut parse = vec![0]; // unnamed statement
    parse.extend_from_slice(sql.as_bytes());
    parse.extend_from_slice(&[0, 0, 0]); // end of text, no declared parameter types
    let mut bind = vec![0, 0]; // unnamed portal and statement
    bind.extend_from_slice(&0u16.to_be_bytes()); // all parameters are text
    bind.extend_from_slice(&(params.len() as u16).to_be_bytes());
    for param in params {
        match param {
            Some(text) => {
                bind.extend_from_slice(&(text.len() as i32).to_be_bytes());
                bind.extend_from_slice(text.as_bytes());
            }
            None => bind.extend_from_slice(&(-1i32).to_be_bytes()),
        }
    }
    bind.extend_from_slice(&0u16.to_be_bytes()); // text results
    let mut flight = frame(b'P', &parse);
    flight.extend(frame(b'B', &bind));
    flight.extend(frame(b'E', &[0, 0, 0, 0, 0])); // unnamed portal, no row limit
    flight.extend(frame(b'S', b""));
    stream.write_all(&flight).unwrap();
    stream.flush().unwrap();
    read_reply(stream)
}

fn read_reply(stream: &mut impl Read) -> Reply {
    let mut reply = Reply::default();
    loop {
        let (tag, body) = message(stream);
        match tag {
            b'T' => {
                let count = u16::from_be_bytes(body[..2].try_into().unwrap());
                let mut at = 2;
                for _ in 0..count {
                    let end = at + body[at..].iter().position(|b| *b == 0).unwrap();
                    reply
                        .columns
                        .push(String::from_utf8(body[at..end].to_vec()).unwrap());
                    at = end + 1 + 18; // table oid, attr, type oid, size, modifier, format
                }
            }
            b'D' => {
                let count = u16::from_be_bytes(body[..2].try_into().unwrap());
                let mut at = 2;
                let mut row = Vec::new();
                for _ in 0..count {
                    let len = i32::from_be_bytes(body[at..at + 4].try_into().unwrap());
                    at += 4;
                    if len < 0 {
                        row.push(None);
                    } else {
                        let end = at + len as usize;
                        row.push(Some(String::from_utf8(body[at..end].to_vec()).unwrap()));
                        at = end;
                    }
                }
                reply.rows.push(row);
            }
            b'C' => reply.tags.push(cstrings(&body).remove(0)),
            b'E' => {
                let fields = body
                    .split(|b| *b == 0)
                    .filter(|f| !f.is_empty())
                    .map(|f| (f[0], String::from_utf8(f[1..].to_vec()).unwrap()))
                    .collect::<Vec<_>>();
                let get = |k: u8| fields.iter().find(|(t, _)| *t == k).unwrap().1.clone();
                reply.error = Some((get(b'C'), get(b'M')));
            }
            b'Z' => {
                reply.status = body[0];
                return reply;
            }
            b'I' | b'N' | b'S' | b'1' | b'2' | b't' | b'n' => {}
            other => panic!("unexpected message {:?}", other as char),
        }
    }
}

fn terminate(mut stream: impl Write) {
    let _ = stream.write_all(&frame(b'X', b""));
}

/// Loopback trust startup: no password, no TLS.
fn trust_connect(address: SocketAddr) -> TcpStream {
    let mut socket = TcpStream::connect_timeout(&address, Duration::from_secs(2)).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    socket.set_nodelay(true).unwrap();
    socket.write_all(&startup("dev")).unwrap();
    loop {
        let (tag, _) = message(&mut socket);
        assert_ne!(tag, b'E', "startup refused");
        if tag == b'Z' {
            return socket;
        }
    }
}

// --- the SCRAM-SHA-256 client over verified TLS ---------------------------------------------------

type Tls = StreamOwned<ClientConnection, TcpStream>;

fn mac(key: &[u8], data: &[u8]) -> [u8; 32] {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).unwrap();
    mac.update(data);
    mac.finalize().into_bytes().into()
}

/// Connects over TLS (verifying `roots`), authenticates as `user` with `password` and reads up to the
/// first `ReadyForQuery`. Returns the error code instead when the server refuses the login.
fn scram_connect(
    address: SocketAddr,
    roots: &Arc<ClientConfig>,
    user: &str,
    password: &[u8],
) -> Result<Tls, String> {
    let mut socket = TcpStream::connect_timeout(&address, Duration::from_secs(2)).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    socket.write_all(&[0, 0, 0, 8, 4, 210, 22, 47]).unwrap();
    let mut reply = [0];
    socket.read_exact(&mut reply).unwrap();
    assert_eq!(reply, *b"S");
    let mut connection =
        ClientConnection::new(roots.clone(), ServerName::try_from("localhost").unwrap()).unwrap();
    connection.complete_io(&mut socket).unwrap();
    let mut client = StreamOwned::new(connection, socket);
    client.write_all(&startup(user)).unwrap();
    client.flush().unwrap();
    let (tag, _) = message(&mut client);
    assert_eq!(tag, b'R');
    let first = b"n,,n=,r=sql-binary-client-nonce";
    let mut initial = b"SCRAM-SHA-256\0".to_vec();
    initial.extend_from_slice(&(first.len() as u32).to_be_bytes());
    initial.extend_from_slice(first);
    client.write_all(&frame(b'p', &initial)).unwrap();
    client.flush().unwrap();
    let (tag, payload) = message(&mut client);
    assert_eq!((tag, &payload[..4]), (b'R', &[0, 0, 0, 11][..]));
    let server_first = String::from_utf8(payload[4..].to_vec()).unwrap();
    let attrs: Vec<_> = server_first.split(',').collect();
    let nonce = attrs[0].strip_prefix("r=").unwrap();
    let salt = STANDARD
        .decode(attrs[1].strip_prefix("s=").unwrap())
        .unwrap();
    let final_bare = format!("c=biws,r={nonce}");
    let auth = format!("n=,r=sql-binary-client-nonce,{server_first},{final_bare}");
    let mut salted = [0; 32];
    pbkdf2::pbkdf2_hmac::<Sha256>(password, &salt, 4096, &mut salted);
    let client_key = mac(&salted, b"Client Key");
    let signature = mac(&Sha256::digest(client_key), auth.as_bytes());
    let proof: Vec<u8> = client_key
        .iter()
        .zip(signature)
        .map(|(a, b)| a ^ b)
        .collect();
    let proof = format!("{final_bare},p={}", STANDARD.encode(proof));
    client.write_all(&frame(b'p', proof.as_bytes())).unwrap();
    client.flush().unwrap();
    loop {
        let (tag, body) = message(&mut client);
        match tag {
            b'E' => {
                let code = body
                    .split(|b| *b == 0)
                    .find(|f| f.first() == Some(&b'C'))
                    .map(|f| String::from_utf8(f[1..].to_vec()).unwrap());
                return Err(code.unwrap());
            }
            b'Z' => return Ok(client),
            _ => {}
        }
    }
}

/// A self-signed certificate for `localhost`, its client trust configuration and the account files.
struct Fixture {
    scratch: Scratch,
    roots: Arc<ClientConfig>,
    args: Vec<String>,
}

fn scram_fixture(name: &str, policies: &str) -> Fixture {
    let scratch = Scratch::new(name);
    let generated = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let cert = scratch.private_file("cert.pem", generated.cert.pem().as_bytes());
    let key = scratch.private_file("key.pem", generated.signing_key.serialize_pem().as_bytes());
    // The published RFC 7677 verifier for the password "pencil".
    let credential = "SCRAM-SHA-256$4096:W22ZaJ0SNY7soEsUEjb6gQ==$WG5d8oPm3OtcPnkdi4Uo7BkeZkBFzpcXkuLmtbsT4qY=:wfPLwcE6nTWhTAmQ7tl2KeoiWGPlZqQxSrmfPwDl2dU=";
    let verifiers = scratch.private_file(
        "accounts.txt",
        format!("writer\t{credential}\nreader\t{credential}\nstranger\t{credential}\n").as_bytes(),
    );
    let decoy = scratch.private_file("decoy.bin", &[9u8; 32]);
    let policy_file = scratch.private_file("policies.txt", policies.as_bytes());
    let mut roots = RootCertStore::empty();
    roots.add(generated.cert.der().clone()).unwrap();
    let client =
        ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
    let data = scratch.arg("data");
    let args = [
        "--auth",
        "scram-tls",
        "--tls-cert",
        &cert,
        "--tls-key",
        &key,
        "--scram-verifiers",
        &verifiers,
        "--scram-decoy-secret",
        &decoy,
        "--account-policies",
        &policy_file,
        "--data-dir",
        &data,
    ]
    .map(String::from)
    .to_vec();
    Fixture {
        scratch,
        roots: Arc::new(client),
        args,
    }
}

fn refs(args: &[String]) -> Vec<&str> {
    args.iter().map(String::as_str).collect()
}

/// Every file under `dir`, concatenated, so a test can look for bytes the log must hold.
fn directory_bytes(dir: &Path) -> Vec<u8> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            out.extend(directory_bytes(&path));
        } else {
            out.extend(std::fs::read(path).unwrap());
        }
    }
    out
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

// --- tests ---------------------------------------------------------------------------------------

#[test]
fn sql_runs_over_the_wire_against_a_data_directory() {
    let scratch = Scratch::new("trust");
    let data = scratch.arg("data");
    let server = start(&["--data-dir", &data, "--development-policy", "allow-all"]).unwrap();
    assert!(
        server.banner.contains("(development-trust; SQL)"),
        "{}",
        server.banner
    );
    let mut a = trust_connect(server.address);
    let r = query(
        &mut a,
        "CREATE TABLE notes (id INT PRIMARY KEY, title TEXT NOT NULL, done BOOLEAN)",
    );
    assert_eq!(
        (r.error, r.tags, r.status),
        (None, vec!["CREATE TABLE".to_string()], b'I')
    );
    let r = query(
        &mut a,
        "INSERT INTO notes VALUES (1, 'write tests', false), (2, 'ship', NULL) RETURNING id",
    );
    assert_eq!(r.tags, ["INSERT 0 2"]);
    assert_eq!(r.text(), [["1"], ["2"]]);
    let r = query(&mut a, "SELECT id, title, done FROM notes ORDER BY id DESC");
    assert_eq!(r.columns, ["id", "title", "done"]);
    assert_eq!(r.text(), [["2", "ship", "NULL"], ["1", "write tests", "f"]]);
    assert_eq!(r.tags, ["SELECT 2"]);

    // A second connection shares the database and sees committed work only.
    let mut b = trust_connect(server.address);
    assert_eq!(query(&mut b, "BEGIN").status, b'T');
    query(&mut b, "INSERT INTO notes VALUES (3, 'later', true)");
    assert_eq!(query(&mut a, "SELECT id FROM notes").rows.len(), 2);
    assert_eq!(query(&mut b, "COMMIT").status, b'I');
    assert_eq!(query(&mut a, "SELECT id FROM notes").rows.len(), 3);

    // Errors carry the SQLSTATE and leave the connection usable.
    let r = query(&mut a, "INSERT INTO notes VALUES (1, 'dup', true)");
    assert_eq!(r.code(), Some("23505"));
    let r = query(&mut a, "INSERT INTO notes VALUES (9, NULL, true)");
    assert_eq!(r.code(), Some("23502"));
    let r = query(&mut a, "SELECT * FROM missing");
    assert_eq!(r.code(), Some("42P01"));
    assert_eq!(
        query(&mut a, "UPDATE notes SET done = true WHERE id = 2").tags,
        ["UPDATE 1"]
    );
    assert_eq!(
        query(&mut a, "DELETE FROM notes WHERE id = 3").tags,
        ["DELETE 1"]
    );
    let r = query(&mut a, "SELECT 1");
    assert_eq!(
        r.code(),
        Some("0A000"),
        "SELECT without FROM is not in the subset"
    );

    // A transaction block that fails stays failed until ROLLBACK.
    query(&mut a, "BEGIN");
    assert_eq!(
        query(&mut a, "INSERT INTO notes VALUES (1, 'dup', true)").status,
        b'E'
    );
    assert_eq!(query(&mut a, "SELECT id FROM notes").code(), Some("25P02"));
    assert_eq!(query(&mut a, "ROLLBACK").status, b'I');

    // A connection that disconnects mid-transaction commits nothing.
    query(&mut b, "BEGIN");
    query(&mut b, "INSERT INTO notes VALUES (7, 'abandoned', true)");
    drop(b);
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(
        query(&mut a, "SELECT id FROM notes WHERE id = 7")
            .rows
            .len(),
        0
    );
    terminate(a);
    assert_eq!(server.stop(), "");
}

#[test]
fn a_deny_all_development_policy_refuses_writes_and_ddl() {
    let scratch = Scratch::new("deny");
    let data = scratch.arg("data");
    let server = start(&["--data-dir", &data, "--development-policy", "deny-all"]).unwrap();
    let mut c = trust_connect(server.address);
    assert_eq!(
        query(&mut c, "CREATE TABLE t (id INT PRIMARY KEY)").code(),
        Some("42501")
    );
    terminate(c);
    server.stop();
}

#[test]
fn the_statement_timeout_ceiling_clamps_a_client_setting() {
    let scratch = Scratch::new("ceiling");
    let data = scratch.arg("data");
    let server = start(&[
        "--data-dir",
        &data,
        "--development-policy",
        "allow-all",
        "--statement-timeout-max-ms",
        "5000",
    ])
    .unwrap();
    let mut c = trust_connect(server.address);
    assert_eq!(query(&mut c, "SHOW statement_timeout").text(), [["5000ms"]]);
    assert_eq!(query(&mut c, "SET statement_timeout = 600000").error, None);
    assert_eq!(query(&mut c, "SHOW statement_timeout").text(), [["5000ms"]]);
    assert_eq!(query(&mut c, "SET statement_timeout = 100").error, None);
    assert_eq!(query(&mut c, "SHOW statement_timeout").text(), [["100ms"]]);
    terminate(c);
    server.stop();
}

#[test]
fn scram_accounts_get_their_own_row_policy_and_unlisted_accounts_are_denied() {
    let fixture = scram_fixture("scram", "writer\tallow-all\nreader\tdeny-all\n");
    let server = start(&refs(&fixture.args)).unwrap();
    assert!(
        server.banner.contains("(scram-tls; SQL)"),
        "{}",
        server.banner
    );
    // The development principal does not exist in this mode, and a wrong password never reaches SQL.
    assert_eq!(
        scram_connect(server.address, &fixture.roots, "writer", b"wrong").err(),
        Some("28P01".to_string())
    );
    let mut writer = scram_connect(server.address, &fixture.roots, "writer", b"pencil").unwrap();
    assert_eq!(
        query(
            &mut writer,
            "CREATE TABLE ledger (id INT PRIMARY KEY, amount INT)"
        )
        .error,
        None
    );
    assert_eq!(
        query(&mut writer, "INSERT INTO ledger VALUES (1, 10), (2, 20)").tags,
        ["INSERT 0 2"]
    );
    assert_eq!(query(&mut writer, "SELECT id FROM ledger").rows.len(), 2);

    // deny-all account: cannot write, cannot change the catalog, and sees no rows.
    let mut reader = scram_connect(server.address, &fixture.roots, "reader", b"pencil").unwrap();
    // The catalog is hidden from it too, so DML on an existing table is `42P01`, as for a missing one;
    // changing the catalog is `42501`.
    let r = query(&mut reader, "INSERT INTO ledger VALUES (3, 30)");
    assert_eq!(r.code(), Some("42P01"), "{r:?}");
    assert_eq!(r.status, b'I');
    assert_eq!(
        query(&mut reader, "CREATE TABLE sneaky (id INT PRIMARY KEY)").code(),
        Some("42501")
    );
    assert_eq!(
        query(&mut reader, "DELETE FROM ledger").code(),
        Some("42P01")
    );
    let r = query(&mut reader, "SELECT id FROM ledger");
    assert_eq!((r.code(), r.rows.len()), (Some("42P01"), 0));
    // The denied account changed nothing.
    assert_eq!(query(&mut writer, "SELECT id FROM ledger").rows.len(), 2);

    // An account with valid credentials that the policy file does not list is denied, not allowed.
    let mut stranger =
        scram_connect(server.address, &fixture.roots, "stranger", b"pencil").unwrap();
    assert_eq!(
        query(&mut stranger, "INSERT INTO ledger VALUES (4, 40)").code(),
        Some("42P01")
    );
    assert_eq!(
        query(&mut stranger, "CREATE TABLE sneaky (id INT PRIMARY KEY)").code(),
        Some("42501")
    );
    assert_eq!(
        query(&mut stranger, "SELECT id FROM ledger").code(),
        Some("42P01")
    );
    for client in [writer, reader, stranger] {
        terminate(client);
    }
    assert_eq!(server.stop(), "");
}

#[test]
fn a_clean_stop_and_a_crash_both_keep_committed_rows_in_the_log_but_not_the_columns() {
    let scratch = Scratch::new("restart");
    let data = scratch.arg("data");
    let args = ["--data-dir", &data, "--development-policy", "allow-all"];

    // First run: commit a row, then stop cleanly.
    let server = start(&args).unwrap();
    let mut c = trust_connect(server.address);
    query(&mut c, "CREATE TABLE kept (id INT PRIMARY KEY, label TEXT)");
    query(
        &mut c,
        "INSERT INTO kept VALUES (1, 'marker-clean-stop-4f1c')",
    );
    terminate(c);
    assert_eq!(server.stop(), "", "nothing to report on a first start");
    assert!(contains(
        &directory_bytes(&scratch.path("data")),
        b"marker-clean-stop-4f1c"
    ));

    // Second run: recovery replays the log. The table name is back, its columns are not.
    let server = start(&args).unwrap();
    let mut c = trust_connect(server.address);
    assert_eq!(query(&mut c, "SELECT id FROM kept").code(), Some("42P01"));
    let r = query(&mut c, "CREATE TABLE kept (id INT PRIMARY KEY, label TEXT)");
    assert_eq!(r.code(), Some("42P07"));
    assert!(
        r.error.unwrap().1.contains("columns are not attached"),
        "the message must say why"
    );
    // New tables work next to the recovered one.
    query(
        &mut c,
        "CREATE TABLE fresh (id INT PRIMARY KEY, label TEXT)",
    );
    query(
        &mut c,
        "INSERT INTO fresh VALUES (1, 'marker-before-crash-9b2e')",
    );
    // Kill the process without a shutdown: the commit above already returned, so it must be on disk.
    terminate(c);
    let report = server.crash();
    assert!(
        report.contains("1 recovered table(s) have no column definitions"),
        "{report:?}"
    );
    let log = directory_bytes(&scratch.path("data"));
    assert!(
        contains(&log, b"marker-before-crash-9b2e"),
        "commit lost in a crash"
    );
    assert!(
        contains(&log, b"marker-clean-stop-4f1c"),
        "earlier commit lost"
    );

    // Third run after a crash: the log recovers (the directory lock is released by the dead process)
    // and both tables are known by name only.
    let server = start(&args).unwrap();
    let report = server.stop();
    assert!(
        report.contains("2 recovered table(s) have no column definitions"),
        "{report:?}"
    );
}

#[test]
fn the_extended_protocol_binds_text_parameters() {
    let scratch = Scratch::new("extended");
    let data = scratch.arg("data");
    let server = start(&["--data-dir", &data, "--development-policy", "allow-all"]).unwrap();
    let mut c = trust_connect(server.address);
    query(&mut c, "CREATE TABLE kv (k INT PRIMARY KEY, v TEXT)");
    let r = extended(
        &mut c,
        "INSERT INTO kv VALUES ($1, $2)",
        &[Some("5"), Some("five")],
    );
    assert_eq!(
        (r.error.as_ref(), r.tags.as_slice()),
        (None, &["INSERT 0 1".to_string()][..]),
        "{r:?}"
    );
    let r = extended(&mut c, "INSERT INTO kv VALUES ($1, $2)", &[Some("6"), None]);
    assert_eq!(r.tags, ["INSERT 0 1"], "{r:?}");
    let r = extended(&mut c, "SELECT v FROM kv WHERE k = $1", &[Some("5")]);
    assert_eq!(r.text(), [["five"]], "{r:?}");
    let r = extended(&mut c, "SELECT v FROM kv WHERE k = $1", &[Some("6")]);
    assert_eq!(r.text(), [["NULL"]], "{r:?}");
    // A bad parameter is an error at Bind/Execute, and Sync restores a usable connection.
    let r = extended(
        &mut c,
        "SELECT v FROM kv WHERE k = $1",
        &[Some("not a number")],
    );
    assert_eq!(r.code(), Some("22P02"), "{r:?}");
    assert_eq!(query(&mut c, "SELECT k FROM kv").rows.len(), 2);
    terminate(c);
    server.stop();
}

#[test]
fn one_server_at_a_time_may_use_a_data_directory() {
    let scratch = Scratch::new("lock");
    let data = scratch.arg("data");
    let args = ["--data-dir", &data, "--development-policy", "allow-all"];
    let first = start(&args).unwrap();
    let second = start(&args);
    let refusal = second
        .err()
        .expect("a second server opened the same directory");
    assert!(
        refusal.starts_with("latticed: cannot open or recover the data directory")
            && refusal.contains("another server may be using the directory"),
        "{refusal:?}"
    );
    // The first server is unaffected.
    let mut c = trust_connect(first.address);
    assert_eq!(
        query(&mut c, "CREATE TABLE t (id INT PRIMARY KEY)").error,
        None
    );
    terminate(c);
    first.stop();
}

#[test]
fn sql_options_are_validated_and_there_is_no_allow_all_default() {
    let run = |args: &[&str]| {
        let o = Command::new(env!("CARGO_BIN_EXE_latticed"))
            .args(args)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(!o.status.success(), "{args:?} should fail");
        String::from_utf8_lossy(&o.stderr).into_owned()
    };
    fn with<'a>(extra: &[&'a str]) -> Vec<&'a str> {
        ["--listen", "127.0.0.1:0"]
            .iter()
            .chain(extra)
            .copied()
            .collect()
    }
    assert_eq!(
        run(&with(&["--data-dir", "/nonexistent-latticed-data"])),
        "latticed: --development-policy required with --data-dir in development-trust mode\n"
    );
    assert_eq!(
        run(&with(&["--development-policy", "allow-all"])),
        "latticed: --development-policy and --account-policies require --data-dir\n"
    );
    assert_eq!(
        run(&with(&[
            "--data-dir",
            "/x",
            "--development-policy",
            "allow"
        ])),
        "latticed: --development-policy must be allow-all or deny-all\n"
    );
    assert_eq!(
        run(&with(&[
            "--data-dir",
            "/x",
            "--account-policies",
            "/y",
            "--development-policy",
            "deny-all"
        ])),
        "latticed: --account-policies requires --auth scram-tls\n"
    );
    assert_eq!(
        run(&with(&[
            "--data-dir",
            "/x",
            "--development-policy",
            "deny-all",
            "--statement-timeout-max-ms",
            "0"
        ])),
        "latticed: invalid statement timeout ceiling: must be positive\n"
    );
    assert_eq!(
        run(&with(&[
            "--data-dir",
            "/x",
            "--development-policy",
            "deny-all",
            "--statement-memory-bytes",
            "x"
        ])),
        "latticed: invalid statement memory budget: invalid digit found in string\n"
    );
    assert_eq!(
        run(&with(&[
            "--auth",
            "scram-tls",
            "--data-dir",
            "/x",
            "--development-policy",
            "allow-all"
        ])),
        "latticed: --development-policy applies only to development-trust\n"
    );
    assert_eq!(
        run(&with(&["--auth", "scram-tls", "--data-dir", "/x"])),
        "latticed: --account-policies required with --data-dir in scram-tls mode\n"
    );
}

#[test]
fn a_malformed_account_policy_file_is_refused_before_binding() {
    let fixture = scram_fixture("badpolicy", "writer\tallow\n");
    let error = start(&refs(&fixture.args)).err().unwrap();
    assert_eq!(
        error,
        "latticed: invalid account policy configuration (line 1)\n"
    );
    // The data directory was not created by a server that refused to start.
    assert!(!fixture.scratch.path("data").exists());
}

#[test]
fn without_a_data_directory_the_placeholder_still_answers_select_one_only() {
    let server = start(&[]).unwrap();
    assert!(server.banner.contains("(development-trust; SELECT 1 only)"));
    let mut c = trust_connect(server.address);
    assert_eq!(query(&mut c, "SELECT 1").text(), [["1"]]);
    assert_eq!(query(&mut c, "SELECT * FROM t").code(), Some("0A000"));
    terminate(c);
    server.stop();
}

#[test]
fn a_denied_account_cannot_learn_the_catalog_by_parse_or_by_error_code() {
    let fixture = scram_fixture("catalog", "writer\tallow-all\nreader\tdeny-all\n");
    let server = start(&refs(&fixture.args)).unwrap();
    let mut writer = scram_connect(server.address, &fixture.roots, "writer", b"pencil").unwrap();
    query(
        &mut writer,
        "CREATE TABLE payroll (id INT PRIMARY KEY, salary INT)",
    );
    // The allowed account can describe it, so the comparison below is meaningful.
    assert_eq!(
        parse_describe(&mut writer, "SELECT salary FROM payroll").error,
        None
    );
    let mut reader = scram_connect(server.address, &fixture.roots, "reader", b"pencil").unwrap();
    for sql in [
        "SELECT salary FROM payroll",
        "SELECT nope FROM payroll",
        "INSERT INTO payroll VALUES (1, 2)",
        "DELETE FROM payroll",
    ] {
        let absent_sql = sql.replace("payroll", "nothing_here");
        let described = parse_describe(&mut reader, sql);
        let described_absent = parse_describe(&mut reader, &absent_sql);
        let ran = query(&mut reader, sql);
        let ran_absent = query(&mut reader, &absent_sql);
        for (existing, absent) in [(&described, &described_absent), (&ran, &ran_absent)] {
            let (code, message) = existing.error.clone().unwrap_or_default();
            assert_eq!(code, "42P01", "{sql}");
            let (absent_code, absent_message) = absent.error.clone().unwrap_or_default();
            assert_eq!(code, absent_code, "{sql}");
            assert_eq!(
                message.replace("payroll", "X"),
                absent_message.replace("nothing_here", "X"),
                "{sql}"
            );
        }
    }
    // DDL is refused before any name is looked up.
    for name in ["payroll", "nothing_here"] {
        assert_eq!(
            query(&mut reader, &format!("DROP TABLE {name}")).code(),
            Some("42501")
        );
    }
    terminate(writer);
    terminate(reader);
    server.stop();
}

#[test]
fn an_existing_data_directory_and_the_policy_file_must_be_private() {
    use std::os::unix::fs::PermissionsExt;
    let scratch = Scratch::new("modes");
    let open_dir = scratch.path("open-data");
    std::fs::create_dir(&open_dir).unwrap();
    std::fs::set_permissions(&open_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    let dir_arg = open_dir.to_string_lossy().into_owned();
    let refusal = start(&["--data-dir", &dir_arg, "--development-policy", "allow-all"])
        .err()
        .unwrap();
    assert!(
        refusal.contains("must not be accessible to group or others"),
        "{refusal:?}"
    );
    // Tightened, the same directory is accepted.
    std::fs::set_permissions(&open_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    start(&["--data-dir", &dir_arg, "--development-policy", "allow-all"])
        .unwrap()
        .stop();

    let fixture = scram_fixture("modes-policy", "writer\tallow-all\n");
    let policy = fixture.scratch.arg("policies.txt");
    std::fs::set_permissions(&policy, std::fs::Permissions::from_mode(0o644)).unwrap();
    let refusal = start(&refs(&fixture.args)).err().unwrap();
    assert!(refusal.contains("policies.txt"), "{refusal:?}");
    assert!(!fixture.scratch.path("data").exists());
}

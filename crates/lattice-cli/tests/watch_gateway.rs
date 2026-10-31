//! `lattice watch` through the real binary against a real gateway: `latticed` runs in this process
//! (loopback, ports chosen by the OS) and rows are committed through its PostgreSQL listener. The
//! tests cover what scripted transports cannot: printed events, the saved cursor, a gateway that is
//! stopped and started again on the same address, a revoked token, and the two 1008 closes the
//! watch tells apart (a timeout is retried, an invalid credential is not).
//!
//! Every wait has a deadline, the child process is killed by a guard, and no token appears as a
//! literal: tokens are issued at run time. Unix only: the gateway's credential file and the
//! watch's cursor file are owner-only files.
#![cfg(unix)]
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
use lattice_api::websocket::{self, Opcode};
use lattice_seams::{OsRng, Rng};
use latticed::ws::{GatewayAuth, GatewayConfig};
use latticed::{ServerConfig, SqlConfig, SqlServer, load_credential_store, write_credential_store};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::{Duration, Instant};

/// How long any single wait for the child or the gateway may take. A healthy step takes well under
/// a second; the longest one in these tests is a reconnect, which waits one to a few seconds.
const WAIT: Duration = Duration::from_secs(15);

// --- a scratch directory and the files in it -----------------------------------------------------

struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Scratch {
        let dir = std::env::temp_dir().join(format!(
            "lattice-watch-gw-{name}-{}-{}",
            std::process::id(),
            random()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        Scratch(dir)
    }

    /// A new subdirectory with mode 0700.
    fn private_dir(&self, name: &str) -> PathBuf {
        let dir = self.0.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        dir
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A random number for directory names, so two runs never share a directory.
fn random() -> u64 {
    OsRng.next_u64()
}

/// The lookup id of a token: the 16 hex digits after `lat_`.
fn id_of(token: &str) -> TokenId {
    TokenId::parse(&token[4..20]).unwrap()
}

/// What a test needs to run `lattice watch` as alice: her token in a mode 0600 file, and the
/// credential file the gateway serves.
struct Account {
    token: String,
    token_file: PathBuf,
    credential_file: PathBuf,
}

fn account(scratch: &Scratch) -> Account {
    let mut store = CredentialStore::empty();
    let (token, entry) = issue("alice", 1, None, &mut OsRng).unwrap();
    store.insert(entry).unwrap();
    let token = token.expose_secret().to_string();

    let credentials = scratch.private_dir("credentials");
    let credential_file = credentials.join("bearer-tokens");
    write_credential_store(&credential_file, &store).unwrap();

    let watch_dir = scratch.private_dir("token");
    let token_file = watch_dir.join("token");
    std::fs::write(&token_file, format!("{token}\n")).unwrap();
    std::fs::set_permissions(&token_file, std::fs::Permissions::from_mode(0o600)).unwrap();
    Account {
        token,
        token_file,
        credential_file,
    }
}

// --- the in-process gateway ----------------------------------------------------------------------

struct Gateway {
    server: Option<SqlServer>,
    sql: SocketAddr,
    ws: SocketAddr,
}

impl Gateway {
    /// Starts the server on `data_dir`. `ws_port` is 0 for a port chosen by the OS, or the port of
    /// an earlier gateway to start again on the same address.
    fn start(data_dir: &Path, credential_file: &Path, ws_port: u16) -> Gateway {
        let accounts = PolicyTable::parse_accounts("alice\ttables: *=rwd\n").unwrap();
        let gateway = GatewayConfig {
            address: SocketAddr::from(([127, 0, 0, 1], ws_port)),
            auth: GatewayAuth::BearerFile(credential_file.to_path_buf()),
            slice: Duration::from_millis(10),
            fallback_poll: Duration::from_millis(50),
            revalidate_every: Duration::from_millis(100),
            credential_poll: Duration::from_millis(50),
            ..GatewayConfig::default()
        };
        let server = SqlServer::start_with_gateway(
            ServerConfig {
                address: SocketAddr::from(([127, 0, 0, 1], 0)),
                ..ServerConfig::default()
            },
            SqlConfig {
                data_dir: data_dir.to_path_buf(),
                policies: PolicyTable::new(Grant::AllowAll).with_accounts(accounts),
                limits: HostLimits::default(),
                allow_format_upgrade: false,
            },
            Some(gateway),
        )
        .unwrap();
        Gateway {
            sql: server.local_addr(),
            ws: server.gateway_addr().unwrap(),
            server: Some(server),
        }
    }

    /// Stops the server: its listeners close and its connections end.
    fn stop(&mut self) {
        if let Some(server) = self.server.take() {
            server.shutdown().unwrap();
        }
    }
}

impl Drop for Gateway {
    fn drop(&mut self) {
        self.stop();
    }
}

// --- a minimal PostgreSQL trust client, enough to run statements ---------------------------------

fn pg_message(stream: &mut TcpStream) -> (u8, Vec<u8>) {
    let mut header = [0; 5];
    stream.read_exact(&mut header).unwrap();
    let length = u32::from_be_bytes(header[1..5].try_into().unwrap()) as usize;
    let mut body = vec![0; length - 4];
    stream.read_exact(&mut body).unwrap();
    (header[0], body)
}

/// Runs each statement on a new connection to `sql` and waits for its `ReadyForQuery`.
fn run_sql(sql: SocketAddr, statements: &[String]) {
    let mut socket = TcpStream::connect_timeout(&sql, Duration::from_secs(2)).unwrap();
    socket.set_read_timeout(Some(WAIT)).unwrap();
    let mut startup = vec![0, 0, 0, 0, 0, 3, 0, 0];
    startup.extend_from_slice(b"user\0dev\0\0");
    let length = (startup.len() as u32).to_be_bytes();
    startup[..4].copy_from_slice(&length);
    socket.write_all(&startup).unwrap();
    loop {
        let (tag, _) = pg_message(&mut socket);
        assert_ne!(tag, b'E');
        if tag == b'Z' {
            break;
        }
    }
    for statement in statements {
        let mut body = statement.as_bytes().to_vec();
        body.push(0);
        let mut frame = vec![b'Q'];
        frame.extend_from_slice(&((body.len() + 4) as u32).to_be_bytes());
        frame.extend_from_slice(&body);
        socket.write_all(&frame).unwrap();
        loop {
            let (tag, body) = pg_message(&mut socket);
            assert_ne!(tag, b'E', "{statement}: {}", String::from_utf8_lossy(&body));
            if tag == b'Z' {
                break;
            }
        }
    }
}

fn create_notes(sql: SocketAddr) {
    run_sql(
        sql,
        &["CREATE TABLE notes (id INT PRIMARY KEY, body TEXT)".to_string()],
    );
}

fn insert_notes(sql: SocketAddr, ids: std::ops::RangeInclusive<u32>) {
    let statements: Vec<String> = ids
        .map(|n| format!("INSERT INTO notes VALUES ({n}, 'v{n:04}')"))
        .collect();
    run_sql(sql, &statements);
}

// --- the child process ---------------------------------------------------------------------------

/// A running `lattice watch`. Dropping it kills the child, so a failed assertion never leaves one.
struct Watch {
    child: Child,
    stdout: Receiver<String>,
    stderr: Receiver<String>,
    /// Standard error read so far.
    notes: Vec<String>,
}

fn lines_of(stream: impl Read + Send + 'static) -> Receiver<String> {
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stream).lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    rx
}

impl Watch {
    fn spawn(url: SocketAddr, token_file: &Path, extra: &[&str]) -> Watch {
        let mut child = Command::new(env!("CARGO_BIN_EXE_lattice"))
            .args(["watch", "table/notes", "--url"])
            .arg(format!("ws://{url}"))
            .arg("--token-file")
            .arg(token_file)
            .args(extra)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stdout = lines_of(child.stdout.take().unwrap());
        let stderr = lines_of(child.stderr.take().unwrap());
        Watch {
            child,
            stdout,
            stderr,
            notes: Vec::new(),
        }
    }

    /// Reads standard error that has arrived, without waiting.
    fn collect_notes(&mut self) {
        while let Ok(line) = self.stderr.try_recv() {
            self.notes.push(line);
        }
    }

    fn notes_text(&mut self) -> String {
        self.collect_notes();
        self.notes.join("\n")
    }

    /// The next printed line, or a panic that shows what the watch said on standard error.
    fn next_line(&mut self) -> String {
        match self.stdout.recv_timeout(WAIT) {
            Ok(line) => line,
            Err(e) => panic!(
                "no event line within {WAIT:?} ({e:?}); standard error:\n{}",
                self.notes_text()
            ),
        }
    }

    fn next_lines(&mut self, count: usize) -> Vec<String> {
        (0..count).map(|_| self.next_line()).collect()
    }

    /// Asserts that nothing is printed for `window`.
    fn assert_quiet(&mut self, window: Duration) {
        match self.stdout.recv_timeout(window) {
            Err(RecvTimeoutError::Timeout) => {}
            other => panic!(
                "an unexpected extra line ({other:?}); standard error:\n{}",
                self.notes_text()
            ),
        }
    }

    /// Waits for the child to exit.
    fn wait_exit(&mut self) -> ExitStatus {
        let deadline = Instant::now() + WAIT;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "the watch did not exit within {WAIT:?}; standard error:\n{}",
                self.notes_text()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Standard error up to its end, once the child has exited.
    fn rest_of_stderr(&mut self) -> String {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match self.stderr.recv_timeout(Duration::from_millis(100)) {
                Ok(line) => self.notes.push(line),
                Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) if Instant::now() >= deadline => break,
                Err(RecvTimeoutError::Timeout) => {}
            }
        }
        self.notes.join("\n")
    }

    /// Kills the child and waits for it, so its lock on the cursor file is released.
    fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        self.kill();
    }
}

// --- reading what the watch prints ---------------------------------------------------------------

/// One event line: `CURSOR AUTHOR TABLE KEY OPERATION`.
struct Line {
    cursor: String,
    key: String,
}

fn parse_line(line: &str) -> Line {
    let words: Vec<&str> = line.split(' ').collect();
    assert!(words.len() >= 5, "not an event line: {line:?}");
    let cursor = words[0];
    assert!(
        cursor.len() == 24
            && cursor
                .bytes()
                .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')),
        "the cursor is not 24 lowercase hex digits: {line:?}"
    );
    assert_eq!(words[1].len(), 32, "{line:?}");
    assert_eq!(words[2], "notes", "{line:?}");
    assert_eq!(words[words.len() - 1], "upsert", "{line:?}");
    Line {
        cursor: cursor.to_string(),
        key: words[3..words.len() - 1].join(" "),
    }
}

/// Asserts that `lines` are events in feed order (cursors strictly increase) with distinct keys,
/// one per row written, and returns them.
fn check_order(lines: &[String]) -> Vec<Line> {
    let parsed: Vec<Line> = lines.iter().map(|l| parse_line(l)).collect();
    for pair in parsed.windows(2) {
        // Fixed-width lowercase hex compares like the numbers it spells.
        assert!(
            pair[0].cursor < pair[1].cursor,
            "cursors do not increase: {} then {}",
            pair[0].cursor,
            pair[1].cursor
        );
    }
    let mut keys: Vec<&str> = parsed.iter().map(|l| l.key.as_str()).collect();
    keys.sort_unstable();
    keys.dedup();
    assert_eq!(keys.len(), parsed.len(), "an event was repeated: {lines:?}");
    parsed
}

/// Waits until the cursor file holds exactly `cursor` and a newline: the save that follows the
/// acknowledgement of the event printed last.
fn wait_for_saved_cursor(file: &Path, cursor: &str) {
    let want = format!("{cursor}\n");
    let deadline = Instant::now() + WAIT;
    loop {
        if std::fs::read_to_string(file).is_ok_and(|have| have == want) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the cursor file never held {cursor}: {:?}",
            std::fs::read_to_string(file)
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

// --- the tests -----------------------------------------------------------------------------------

#[test]
fn events_are_printed_saved_and_resumed_across_a_gateway_restart_and_a_process_kill() {
    let scratch = Scratch::new("resume");
    let who = account(&scratch);
    let data = scratch.private_dir("data");
    let cursor_dir = scratch.private_dir("cursor");
    let cursor_file = cursor_dir.join("notes.cursor");
    let cursor_arg = cursor_file.to_string_lossy().into_owned();

    let mut gateway = Gateway::start(&data, &who.credential_file, 0);
    create_notes(gateway.sql);
    insert_notes(gateway.sql, 1..=3);

    let mut watch = Watch::spawn(gateway.ws, &who.token_file, &["--cursor-file", &cursor_arg]);
    // Three rows were written, so exactly three lines come, in feed order.
    let mut printed = watch.next_lines(3);
    let first = check_order(&printed);
    // A save follows the acknowledgement of each printed event.
    wait_for_saved_cursor(&cursor_file, &first[2].cursor);

    // Stop the gateway and start it again on the same ports and data. The watch must notice, wait,
    // reconnect, and resume after the saved cursor: rows 4 and 5 arrive once, rows 1 to 3 do not.
    let ws_port = gateway.ws.port();
    gateway.stop();
    let gateway = Gateway::start(&data, &who.credential_file, ws_port);
    assert_eq!(gateway.ws.port(), ws_port);
    insert_notes(gateway.sql, 4..=5);
    printed.extend(watch.next_lines(2));
    watch.assert_quiet(Duration::from_millis(1500));
    let all = check_order(&printed);
    assert_eq!(all.len(), 5);
    wait_for_saved_cursor(&cursor_file, &all[4].cursor);
    assert!(
        watch.notes_text().contains("retrying in"),
        "the watch never reported a reconnect:\n{}",
        watch.notes_text()
    );

    // Kill the process itself. A new process on the same cursor file prints only what is new.
    watch.kill();
    insert_notes(gateway.sql, 6..=6);
    let mut again = Watch::spawn(gateway.ws, &who.token_file, &["--cursor-file", &cursor_arg]);
    printed.push(again.next_line());
    again.assert_quiet(Duration::from_millis(1000));
    let all = check_order(&printed);
    assert_eq!(all.len(), 6);
    wait_for_saved_cursor(&cursor_file, &all[5].cursor);
}

#[test]
fn a_revoked_token_ends_a_running_watch_and_is_not_retried() {
    let scratch = Scratch::new("revoked");
    let who = account(&scratch);
    let data = scratch.private_dir("data");
    let gateway = Gateway::start(&data, &who.credential_file, 0);
    create_notes(gateway.sql);
    insert_notes(gateway.sql, 1..=1);

    let mut watch = Watch::spawn(gateway.ws, &who.token_file, &["--from-start"]);
    check_order(&watch.next_lines(1));

    let id = id_of(&who.token);
    let mut store = load_credential_store(&who.credential_file).unwrap();
    assert!(store.revoke(id));
    write_credential_store(&who.credential_file, &store).unwrap();

    // The gateway reloads the file within a poll interval and closes the connection with 1008
    // "credential no longer valid". The watch stops with status 1 and does not wait to retry.
    let status = watch.wait_exit();
    let stderr = watch.rest_of_stderr();
    assert_eq!(status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("no longer verifies"), "{stderr}");
    assert!(!stderr.contains("retrying"), "{stderr}");

    // A new watch with the same token is refused at the upgrade, and also stops at once.
    let mut again = Watch::spawn(gateway.ws, &who.token_file, &["--from-start"]);
    let status = again.wait_exit();
    let stderr = again.rest_of_stderr();
    assert_eq!(status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("refused the token"), "{stderr}");
    assert!(!stderr.contains("retrying"), "{stderr}");
}

/// Answers `closes.len()` WebSocket upgrades, one connection each, and closes every connection at
/// once with 1008 and the given reason. Returns the number of connections served.
fn serve_closes(
    listener: TcpListener,
    closes: Vec<&'static str>,
) -> std::thread::JoinHandle<usize> {
    listener.set_nonblocking(true).unwrap();
    std::thread::spawn(move || {
        let mut served = 0;
        let deadline = Instant::now() + Duration::from_secs(25);
        for reason in closes {
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        if Instant::now() >= deadline {
                            return served;
                        }
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => return served,
                }
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut head = Vec::new();
            let mut byte = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") {
                if stream.read_exact(&mut byte).is_err() {
                    return served;
                }
                head.push(byte[0]);
            }
            let head = String::from_utf8_lossy(&head).into_owned();
            let key = head
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.trim()
                        .eq_ignore_ascii_case("sec-websocket-key")
                        .then(|| value.trim().to_string())
                })
                .unwrap_or_default();
            let response = format!(
                "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {}\r\nSec-WebSocket-Protocol: {}\r\n\r\n",
                websocket::accept_key(&key),
                websocket::SUBPROTOCOL
            );
            let mut payload = 1008u16.to_be_bytes().to_vec();
            payload.extend_from_slice(reason.as_bytes());
            let mut frame = Vec::new();
            websocket::encode_frame(true, Opcode::Close, &payload, &mut frame).unwrap();
            let mut out = response.into_bytes();
            out.extend_from_slice(&frame);
            if stream.write_all(&out).is_err() {
                return served;
            }
            served += 1;
            // Keep the socket open until the client has read the close and hung up, so the close
            // frame is not lost to a reset when the client's Hello arrives after we closed.
            let mut sink = [0u8; 256];
            while matches!(stream.read(&mut sink), Ok(n) if n > 0) {}
        }
        served
    })
}

#[test]
fn a_timeout_close_is_retried_and_an_invalid_credential_close_is_not() {
    let scratch = Scratch::new("closes");
    let who = account(&scratch);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    // The first connection is closed for a timeout, the second for a credential that no longer
    // verifies. The reasons are the texts the gateway sends (crates/latticed/src/ws.rs).
    let server = serve_closes(listener, vec!["timeout", "credential no longer valid"]);

    let mut watch = Watch::spawn(address, &who.token_file, &["--from-start"]);
    let status = watch.wait_exit();
    let stderr = watch.rest_of_stderr();
    let served = server.join().unwrap();

    // Two connections: the first close did not stop the watch, the second did.
    assert_eq!(served, 2, "{stderr}");
    assert_eq!(status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("idle connection on a timeout"), "{stderr}");
    assert!(stderr.contains("retrying in"), "{stderr}");
    assert!(stderr.contains("no longer verifies"), "{stderr}");
}

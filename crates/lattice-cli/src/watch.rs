//! `lattice watch table/NAME`: prints the live changes of one table from the sync gateway.
//!
//! The order for each event is fixed: its line is printed, then it is acknowledged, then the cursor
//! is saved (with `--cursor-file`). A crash between two steps re-prints an event on the next start
//! and never skips one. A dropped connection is retried from the last saved cursor. Connections that
//! deliver nothing count as failures, and after a few failures in a row the watch stops with an error.
//! A refusal that a new connection cannot fix (a bad token, a protocol version the gateway does not
//! speak) stops it at once. A policy close for a timeout is a dropped connection and is retried.
//!
//! The driver is generic over [`Dialer`], [`Connection`], [`Output`] and [`CursorStore`], so the
//! order rule and the resume rule are tested with scripted doubles. The socket transport at the end
//! of this file is the only part that touches the network. The retry policy is the SDK's supervisor
//! (`lattice_sdk::reconnect`): this file reports each outcome to it and waits as it decides.

mod cursor_file;

use cursor_file::{CursorFileError, FileCursorStore};
use lattice_api::sync;
use lattice_api::websocket::{self, Opcode};
use lattice_core::feed::{ChangeEvent, ChangeOp, Cursor};
use lattice_sdk::backoff::Backoff;
use lattice_sdk::client::{Action, ClientConfig, ClientError, CloseReason, Frame, SyncClient};
use lattice_sdk::cursor_store::{CursorStore, MemoryCursorStore};
use lattice_sdk::mcp::FeedCursor;
use lattice_sdk::reconnect::{Decision, Input, StopReason, Supervisor};
use lattice_seams::{OsRng, Rng};
use std::fmt;
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};
use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;
use zeroize::Zeroizing;

/// The largest token file or standard input read, in bytes.
const MAX_TOKEN_BYTES: u64 = 4096;
/// The name this client gives itself in `Hello`.
const AGENT: &str = "lattice-watch";
/// The prefix of a table's sync scope.
const SCOPE_PREFIX: &str = "table/";
/// The sync endpoint of the gateway (formats/sync-protocol-v1.md). A path in `--url` is ignored.
const SYNC_PATH: &str = "/sync";
/// Consecutive failed connections after which the watch gives up.
const MAX_FAILED_CONNECTIONS: u32 = 3;
/// The first retry window of the supervisor's backoff, in milliseconds. Each further failure doubles it.
const RETRY_BASE_MS: u64 = 1_000;
/// The largest retry window, in milliseconds.
const RETRY_CAP_MS: u64 = 8_000;
/// The shortest wait before a reconnect. The jitter may draw less; the watch never waits less.
const RETRY_FLOOR: Duration = Duration::from_millis(RETRY_BASE_MS);
/// The WebSocket close code for a policy violation. The gateway uses it for three cases, told apart
/// by the reason text below.
const POLICY_CLOSE: u16 = 1008;
/// The 1008 reason when the token no longer verifies for its account. A new connection with the same
/// token is refused the same way. Text as the gateway sends it (`crates/latticed/src/ws.rs`); this
/// crate does not depend on the gateway.
const CREDENTIAL_CLOSE_REASON: &str = "credential no longer valid";
/// The 1008 reason when the gateway speaks no protocol version the client offers in its `Hello`.
/// Nothing in the connection is credential-related: a new connection repeats the refusal.
const REFUSED_CLOSE_REASON: &str = "refused";
/// The 1008 reason when no `Hello` or no `Pong` arrived in time. The connection was idle, not refused.
const TIMEOUT_CLOSE_REASON: &str = "timeout";
/// The longest close reason the watch reads. A control frame carries at most 125 payload bytes, and
/// the first two are the close code. A longer reason is not one the watch knows.
const MAX_CLOSE_REASON: usize = 123;
/// How long a connect or the upgrade may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// One logical tick of the sync client. An idle socket wakes up this often, which drives the pings.
const TICK: Duration = Duration::from_secs(1);
const TOKEN_ON_COMMAND_LINE: &str = "a token is not accepted on the command line; write it to a file and pass --token-file FILE (or --token-file - for standard input)";
const CURSOR_HINT: &str =
    "--from takes a cursor of 24 lowercase hex digits, as printed at the start of each line";

const USAGE: &str = "\
usage: lattice watch table/NAME --url ws://HOST:PORT --token-file FILE (--from CURSOR | --from-start | --cursor-file FILE)

  --url URL            a ws:// URL on a loopback address (127.0.0.1, localhost or [::1]); no TLS yet
  --token-file FILE    the bearer token, read from FILE, or from standard input with -
  --from CURSOR        resume after CURSOR: 24 lowercase hex digits, as printed at the start of a line
  --from-start         deliver every event the table holds
  --cursor-file FILE   resume after the cursor saved in FILE, and save the cursor after each
                       acknowledged event; FILE must be owner-only (mode 0600) in a directory that
                       exists and that group and others cannot write to; a second watch on the same
                       FILE is refused; a missing FILE starts from the beginning of the table
  -h, --help           print this text

Each event prints one line: CURSOR AUTHOR TABLE KEY OPERATION
The token is never accepted on the command line. A dropped connection is retried from the last
saved cursor, after a random wait of one to eight seconds whose upper bound doubles with each
failure. Three connections in a row that deliver nothing stop the watch, and a refused token, or a
server that speaks no protocol version this client speaks, stops it at once. A timeout close is retried.
";

/// Where the bearer token comes from.
#[derive(Debug, PartialEq, Eq)]
enum TokenSource {
    /// A file holding the token on one line.
    File(String),
    /// Standard input, written as `-`.
    Stdin,
}

/// Where the watch starts.
#[derive(Debug, PartialEq, Eq)]
enum Start {
    /// `--from-start`: every event the table holds.
    Origin,
    /// `--from CURSOR`: the events after this cursor.
    Cursor(Vec<u8>),
    /// `--cursor-file FILE`: after the cursor saved in FILE, which is saved again as events are
    /// acknowledged.
    File(String),
}

/// A checked `watch` invocation.
#[derive(Debug, PartialEq, Eq)]
struct Options {
    table: String,
    url: String,
    token: TokenSource,
    start: Start,
}

#[derive(Debug, PartialEq, Eq)]
enum Parsed {
    Help,
    Run(Options),
}

/// What one read from a connection produced.
#[derive(Debug, PartialEq, Eq)]
enum Received {
    /// A decoded sync message.
    Frame(Frame),
    /// Nothing arrived within one tick.
    Idle,
    /// The peer closed the connection, with the close code when it sent one and the reason text
    /// (empty when there was none, or when it was not UTF-8 or too long to be a known reason).
    Closed(Option<u16>, String),
}

/// One connection: frames out and frames in.
trait Connection {
    fn send(&mut self, frame: &Frame) -> Result<(), String>;
    fn recv(&mut self) -> Result<Received, String>;
}

/// Opens a connection to the gateway. Each call is one attempt; a failed attempt comes back as a
/// [`Stop::Ended`] carrying the input the supervisor should see.
trait Dialer {
    type Conn: Connection;
    fn dial(&mut self) -> Result<Self::Conn, Stop>;
}

/// Where event lines go and where notes for the operator go. A line is flushed before `line`
/// returns, so an acknowledgement written afterwards can never be ahead of the text it covers.
trait Output {
    fn line(&mut self, text: &str) -> Result<(), String>;
    fn note(&mut self, text: &str);
}

/// How one connection attempt ended.
#[derive(Debug, PartialEq, Eq)]
enum Stop {
    /// The operator asked the watch to stop. Not an error.
    Interrupted,
    /// The connection ended or was refused. The `Input` is what the supervisor is told; the text is
    /// for the operator.
    Ended(Input, String),
    /// A local failure that a new connection would not change: a line that cannot be printed, a
    /// cursor that cannot be saved, an event that does not match its cursor. The supervisor is not
    /// consulted for these.
    Local(String),
}

/// A transport failure with no protocol reason: the supervisor's `TransportClosed`.
fn transport(text: String) -> Stop {
    Stop::Ended(Input::TransportClosed, text)
}

/// A call the client refused, such as an ack beyond what was delivered: the supervisor's `Failed`.
fn call_refused(error: ClientError) -> Stop {
    let text = error.to_string();
    Stop::Ended(Input::Failed(error), text)
}

fn set_once<T>(slot: &mut Option<T>, flag: &str, new: T) -> Result<(), String> {
    if slot.is_some() {
        return Err(format!("{flag} is given twice"));
    }
    *slot = Some(new);
    Ok(())
}

fn value<'a>(rest: &mut std::slice::Iter<'a, String>, flag: &str) -> Result<&'a str, String> {
    match rest.next() {
        Some(v) if !v.starts_with("--") => Ok(v.as_str()),
        _ => Err(format!("{flag} needs a value")),
    }
}

/// `table/NAME` with NAME one to 64 of `a-z`, `0-9` or `_`: the names the catalog holds.
fn table_of(scope: &str) -> Result<String, String> {
    let name = scope
        .strip_prefix(SCOPE_PREFIX)
        .ok_or_else(|| "the table is named table/NAME".to_owned())?;
    let valid = !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
    if valid {
        Ok(name.to_owned())
    } else {
        Err(format!(
            "`{name}` is not a table name: 1 to 64 of a-z, 0-9 or _"
        ))
    }
}

fn nibble(digit: u8) -> Option<u8> {
    match digit {
        b'0'..=b'9' => Some(digit - b'0'),
        b'a'..=b'f' => Some(digit - b'a' + 10),
        _ => None,
    }
}

/// Exactly 24 lowercase hex digits that encode a valid feed cursor.
fn parse_cursor(text: &str) -> Result<Vec<u8>, String> {
    let digits = text.as_bytes();
    if digits.len() != 2 * Cursor::ENCODED_LEN {
        return Err(CURSOR_HINT.into());
    }
    let (pairs, _) = digits.as_chunks::<2>();
    let mut bytes = Vec::with_capacity(Cursor::ENCODED_LEN);
    for &[hi, lo] in pairs {
        match (nibble(hi), nibble(lo)) {
            (Some(h), Some(l)) => bytes.push((h << 4) | l),
            _ => return Err(CURSOR_HINT.into()),
        }
    }
    if Cursor::from_bytes(&bytes).is_err() {
        return Err(CURSOR_HINT.into());
    }
    Ok(bytes)
}

/// `host` and `port` of an authority, where a bracketed IPv6 host keeps its brackets.
fn split_host_port(authority: &str) -> Option<(&str, &str)> {
    if authority.starts_with('[') {
        let end = authority.find(']')?;
        let port = authority.get(end + 1..)?.strip_prefix(':')?;
        Some((authority.get(..=end)?, port))
    } else {
        authority.rsplit_once(':')
    }
}

/// The loopback address a host names: `localhost` is 127.0.0.1, and a literal must be a loopback
/// address. The name is never resolved again, because a second lookup could return a remote address.
fn loopback_ip(host: &str) -> Option<IpAddr> {
    if host == "localhost" {
        return Some(IpAddr::V4(Ipv4Addr::LOCALHOST));
    }
    if let Some(v6) = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')) {
        return v6
            .parse::<Ipv6Addr>()
            .ok()
            .filter(Ipv6Addr::is_loopback)
            .map(IpAddr::V6);
    }
    host.parse::<Ipv4Addr>()
        .ok()
        .filter(Ipv4Addr::is_loopback)
        .map(IpAddr::V4)
}

/// Only a `ws://` URL on a loopback address with an explicit port. There is no TLS on the client
/// yet, so a remote gateway is refused rather than reached in clear text. Returns the address to
/// connect to.
fn check_url(url: &str) -> Result<SocketAddr, String> {
    if url.starts_with("wss://") {
        return Err("wss:// (TLS) is not supported yet; use ws:// on a loopback address".into());
    }
    let Some(rest) = url.strip_prefix("ws://") else {
        return Err("the URL must start with ws:// (for example ws://127.0.0.1:7000)".into());
    };
    if rest.contains(['?', '#', '@']) {
        return Err("the URL must not carry a query, a fragment or user information".into());
    }
    let authority = rest.split('/').next().unwrap_or(rest);
    let Some((host, port)) = split_host_port(authority) else {
        return Err("the URL needs an explicit port, for example ws://127.0.0.1:7000".into());
    };
    let port: u16 = port
        .parse()
        .map_err(|_| format!("`{port}` is not a port number"))?;
    if port == 0 {
        return Err("the port must not be 0".into());
    }
    let ip = loopback_ip(host).ok_or_else(|| {
        format!(
            "refusing `{host}`: watch connects only to a loopback address (127.0.0.1, localhost or [::1]) because it has no TLS yet"
        )
    })?;
    Ok(SocketAddr::new(ip, port))
}

/// Reads the whole argument list. Pure: nothing is opened or read here.
fn parse(args: &[String]) -> Result<Parsed, String> {
    let mut table = None;
    let mut url = None;
    let mut token = None;
    let mut from = None;
    let mut from_start = false;
    let mut cursor_file = None;
    let mut help = false;
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "-h" | "--help" => help = true,
            "--url" => set_once(&mut url, "--url", value(&mut rest, "--url")?.to_owned())?,
            "--token-file" => set_once(
                &mut token,
                "--token-file",
                value(&mut rest, "--token-file")?.to_owned(),
            )?,
            "--from" => set_once(
                &mut from,
                "--from",
                parse_cursor(value(&mut rest, "--from")?)?,
            )?,
            "--from-start" => {
                if from_start {
                    return Err("--from-start is given twice".into());
                }
                from_start = true;
            }
            "--cursor-file" => set_once(
                &mut cursor_file,
                "--cursor-file",
                value(&mut rest, "--cursor-file")?.to_owned(),
            )?,
            "--token" => return Err(TOKEN_ON_COMMAND_LINE.into()),
            other if other.starts_with("--token=") => return Err(TOKEN_ON_COMMAND_LINE.into()),
            other if other.starts_with('-') => return Err(format!("unknown option {other}")),
            other => {
                if table.is_some() {
                    return Err("only one table may be named".into());
                }
                table = Some(table_of(other)?);
            }
        }
    }
    if help {
        return Ok(Parsed::Help);
    }
    let table = table.ok_or("missing the table: watch table/NAME")?;
    let url = url.ok_or("missing --url ws://HOST:PORT")?;
    check_url(&url)?;
    let token = token.ok_or("missing --token-file FILE")?;
    let start = match (from, from_start, cursor_file) {
        (Some(cursor), false, None) => Start::Cursor(cursor),
        (None, true, None) => Start::Origin,
        (None, false, Some(path)) => Start::File(path),
        (None, false, None) => {
            return Err("give one of --from CURSOR, --from-start or --cursor-file FILE".into());
        }
        _ => return Err("give only one of --from, --from-start and --cursor-file".into()),
    };
    let token = if token == "-" {
        TokenSource::Stdin
    } else {
        TokenSource::File(token)
    };
    Ok(Parsed::Run(Options {
        table,
        url,
        token,
        start,
    }))
}

/// The token: one line, printable ASCII without spaces, at most `MAX_TOKEN_BYTES`, with the line
/// ending not part of it. A token file gets the same checks as the other credential files
/// (`latticed::files::read_private`): a regular file, not a symlink, with no group or other access,
/// in a directory that group and others cannot write to. Errors name the path and the rule, never
/// the contents.
fn read_token(source: &TokenSource, stdin: &mut dyn Read) -> Result<Zeroizing<String>, String> {
    let limit = MAX_TOKEN_BYTES as usize;
    let raw: Zeroizing<Vec<u8>> = match source {
        TokenSource::Stdin => {
            let mut bytes: Zeroizing<Vec<u8>> = Zeroizing::new(Vec::new());
            stdin
                .take(MAX_TOKEN_BYTES + 1)
                .read_to_end(&mut bytes)
                .map_err(|e| format!("cannot read the token: {e}"))?;
            bytes
        }
        TokenSource::File(path) => latticed::files::read_private(Path::new(path), limit)
            .map_err(|e| format!("{path}: {e}"))?,
    };
    if raw.len() > limit {
        return Err(format!("the token is larger than {MAX_TOKEN_BYTES} bytes"));
    }
    let not_a_token = "the token must be one line of printable ASCII with no spaces";
    let text = std::str::from_utf8(&raw).map_err(|_| not_a_token.to_owned())?;
    let body = text
        .strip_suffix('\n')
        .map(|s| s.strip_suffix('\r').unwrap_or(s))
        .unwrap_or(text);
    if body.is_empty() || !body.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(not_a_token.into());
    }
    Ok(Zeroizing::new(body.to_owned()))
}

fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            s.push_str(&format!("{b:02x}"));
            s
        })
}

/// A key as a quoted, escaped string, so a key cannot forge a line break or a terminal escape. A key
/// that is not UTF-8 is shown as `x"HEX"`.
fn key_text(key: &[u8]) -> String {
    match std::str::from_utf8(key) {
        Ok(text) => format!("{text:?}"),
        Err(_) => format!("x\"{}\"", hex(key)),
    }
}

/// `CURSOR AUTHOR TABLE KEY OPERATION`. The cursor is the form `--from` takes.
fn format_event(table: &str, event: &ChangeEvent) -> String {
    let operation = match event.change.op {
        ChangeOp::Delete => "delete",
        ChangeOp::Upsert(_) => "upsert",
    };
    format!(
        "{} {:032x} {table} {} {operation}",
        hex(&event.cursor.to_bytes()),
        event.author,
        key_text(&event.change.key),
    )
}

/// Decodes one delivered event and checks that it is the event its frame names.
fn decode_delivery(cursor: &[u8], data: &[u8]) -> Result<ChangeEvent, String> {
    let event = ChangeEvent::decode(data).map_err(|e| format!("undecodable event: {e}"))?;
    if event.cursor.to_bytes().as_slice() != cursor {
        return Err("an event does not carry the cursor of its delivery".into());
    }
    Ok(event)
}

/// One sync client over one connection. Every action runs before the next one starts, so an
/// acknowledgement is only written after its event's line has been printed, and a cursor is only
/// saved after its acknowledgement has been written.
struct Pump<'a, C, O, S> {
    client: SyncClient,
    table: &'a str,
    /// The cursor the next subscription starts after. Moves forward only after a save.
    resume: &'a mut Vec<u8>,
    conn: &'a mut C,
    out: &'a mut O,
    store: &'a mut S,
    /// The supervisor that decides what follows this connection. Healthy steps reach it as they
    /// happen, so a delivered event resets it before the connection can drop.
    sup: &'a mut Supervisor,
}

impl<C: Connection, O: Output, S: CursorStore> Pump<'_, C, O, S>
where
    S::Error: fmt::Display,
{
    fn perform(&mut self, actions: Vec<Action>) -> Result<(), Stop> {
        for action in actions {
            match action {
                Action::Send(frame) => self.conn.send(&frame).map_err(transport)?,
                Action::Ready { .. } => {
                    let scope = format!("{SCOPE_PREFIX}{}", self.table);
                    let (_, subscribe) = self
                        .client
                        .subscribe(&scope, self.resume.as_slice())
                        .map_err(call_refused)?;
                    self.perform(subscribe)?;
                }
                Action::Deliver { sub, cursor, data } => {
                    let event = decode_delivery(&cursor, &data).map_err(Stop::Local)?;
                    self.out
                        .line(&format_event(self.table, &event))
                        .map_err(Stop::Local)?;
                    let acks = self.client.ack(sub, &cursor).map_err(call_refused)?;
                    self.perform(acks)?;
                    self.save(&cursor)?;
                    // Only now is the event delivered: printed, acknowledged and saved.
                    self.report(Input::EventDelivered);
                }
                Action::SubscriptionFailed { code, message, .. } => {
                    // A server refusal with a code: the supervisor stops on the codes a new
                    // connection would repeat, such as an invalid scope or cursor.
                    let text =
                        format!("the server refused the subscription ({code:?}): {message:?}");
                    return Err(Stop::Ended(
                        Input::Closed(CloseReason::ServerError { code, message }),
                        text,
                    ));
                }
                Action::Close(reason) => {
                    let text = format!("the connection closed: {reason:?}");
                    return Err(Stop::Ended(Input::Closed(reason), text));
                }
            }
        }
        Ok(())
    }

    /// Reports a healthy step to the supervisor. A healthy input never schedules a retry or stops,
    /// so no jitter is drawn for it.
    fn report(&mut self, input: Input) {
        let _ = self.sup.on_input(input, 0);
    }

    /// Saves the cursor of an acknowledged event. A failed save stops the watch: continuing would
    /// let a later cursor be saved past an event that was never recorded.
    fn save(&mut self, cursor: &[u8]) -> Result<(), Stop> {
        let saved: FeedCursor = cursor
            .try_into()
            .map_err(|_| Stop::Local("an event cursor is not 12 bytes".into()))?;
        self.store
            .save(&saved)
            .map_err(|e| Stop::Local(format!("cannot save the cursor: {e}")))?;
        *self.resume = cursor.to_vec();
        Ok(())
    }

    /// Runs the connection until it ends. `interrupted` is asked before each read.
    fn run(&mut self, interrupted: &mut dyn FnMut() -> bool) -> Stop {
        let hello = match self.client.connect(0) {
            Ok(actions) => actions,
            Err(e) => return call_refused(e),
        };
        if let Err(stop) = self.perform(hello) {
            return stop;
        }
        self.report(Input::Connected);
        let mut now: u64 = 0;
        loop {
            if interrupted() {
                return Stop::Interrupted;
            }
            let received = match self.conn.recv() {
                Ok(received) => received,
                Err(e) => return transport(e),
            };
            now = now.saturating_add(1);
            let actions = match received {
                Received::Frame(frame) => match self.client.handle(frame, now) {
                    Ok(actions) => {
                        self.report(Input::FrameReceived);
                        actions
                    }
                    Err(e) => return call_refused(e),
                },
                Received::Idle => self.client.tick(now),
                // 1008 carries a reason that tells the cases apart. A token that no longer verifies is
                // refused again by a new connection, so the watch stops. A protocol version the gateway
                // does not speak is refused again too, so it stops as an incompatible server. A timeout
                // closes an idle connection that a new one can resume from the saved cursor, and any other
                // 1008 is retried like a dropped connection.
                Received::Closed(Some(POLICY_CLOSE), reason) => match reason.as_str() {
                    CREDENTIAL_CLOSE_REASON => {
                        return Stop::Ended(
                            Input::Unauthorized,
                            "the token no longer verifies on the gateway (close code 1008); fix the token file and start again".into(),
                        );
                    }
                    REFUSED_CLOSE_REASON => {
                        return Stop::Ended(
                            Input::Closed(CloseReason::Incompatible),
                            "the gateway speaks no protocol version this client speaks (close code 1008)".into(),
                        );
                    }
                    TIMEOUT_CLOSE_REASON => {
                        return transport(
                            "the gateway closed an idle connection on a timeout (close code 1008)"
                                .into(),
                        );
                    }
                    _ => {
                        return transport(
                            "the gateway closed the connection with code 1008 (policy)".into(),
                        );
                    }
                },
                Received::Closed(_, _) => {
                    return transport("the server closed the connection".into());
                }
            };
            if let Err(stop) = self.perform(actions) {
                return stop;
            }
        }
    }
}

/// What the watch takes from its surroundings: the stop request, the jitter for its retry waits and
/// the wait itself. Production reads the stop signals, the OS and the thread; tests script all three.
struct Env<'a> {
    interrupted: &'a mut dyn FnMut() -> bool,
    jitter: &'a mut dyn FnMut() -> u64,
    pause: &'a mut dyn FnMut(Duration),
}

/// Runs the watch until it is interrupted or gives up. Each connection attempt ends one way, and each
/// way is reported to the supervisor, which decides whether to retry and how long to wait. Healthy
/// steps are reported as they happen, so an event that was printed, acknowledged and saved resets the
/// supervisor before its connection drops.
fn watch<D, O, S>(
    table: &str,
    start: Vec<u8>,
    dialer: &mut D,
    out: &mut O,
    store: &mut S,
    env: &mut Env<'_>,
) -> Result<(), String>
where
    D: Dialer,
    O: Output,
    S: CursorStore,
    S::Error: fmt::Display,
{
    let backoff = Backoff::new(RETRY_BASE_MS, RETRY_CAP_MS).map_err(|e| e.to_string())?;
    let mut sup = Supervisor::new(backoff, Some(MAX_FAILED_CONNECTIONS));
    let mut resume = start;
    loop {
        if (env.interrupted)() {
            return Ok(());
        }
        let stop = match dialer.dial() {
            Ok(mut conn) => {
                let mut pump = Pump {
                    client: SyncClient::new(ClientConfig::new(AGENT)).map_err(|e| e.to_string())?,
                    table,
                    resume: &mut resume,
                    conn: &mut conn,
                    out: &mut *out,
                    store: &mut *store,
                    sup: &mut sup,
                };
                pump.run(&mut *env.interrupted)
            }
            Err(stop) => stop,
        };
        match stop {
            Stop::Interrupted => return Ok(()),
            Stop::Local(e) => return Err(e),
            Stop::Ended(input, message) => match sup.on_input(input, (env.jitter)()) {
                Decision::RetryAfter(ms) => {
                    let delay = Duration::from_millis(ms).max(RETRY_FLOOR);
                    out.note(&format!(
                        "connection lost ({message}); retrying in {delay:?}"
                    ));
                    (env.pause)(delay);
                }
                Decision::Stop(StopReason::GaveUp) => {
                    return Err(format!(
                        "giving up after {MAX_FAILED_CONNECTIONS} failed connections in a row: {message}"
                    ));
                }
                // A refusal the supervisor stops on. `Continue` cannot follow a connection that ended,
                // and if it did the watch would still stop rather than retry blindly.
                Decision::Stop(_) | Decision::Continue => return Err(message),
            },
        }
    }
}

/// Event lines on standard output, flushed one by one. Notes go to standard error.
struct Printer;

impl Output for Printer {
    fn line(&mut self, text: &str) -> Result<(), String> {
        let mut stdout = std::io::stdout().lock();
        writeln!(stdout, "{text}").map_err(|e| e.to_string())?;
        stdout.flush().map_err(|e| e.to_string())
    }

    fn note(&mut self, text: &str) {
        eprintln!("lattice watch: {text}");
    }
}

/// What a refused upgrade means. A 401 or 403 is the gateway refusing the token, which a new
/// connection will not change, so it stops the watch. Any other status is retried. The status is the
/// gateway's own text, so it is shown quoted, with control characters escaped.
fn refused_upgrade(status: &str) -> Stop {
    match status {
        "401" | "403" => Stop::Ended(
            Input::Unauthorized,
            format!(
                "the gateway refused the token (status {status:?}); fix the token file and start again"
            ),
        ),
        other => transport(format!(
            "the gateway refused the upgrade with status {other:?}"
        )),
    }
}

/// Opens a loopback socket to the gateway and performs the WebSocket upgrade on `/sync`.
struct TcpDialer {
    address: SocketAddr,
    token: Zeroizing<String>,
}

impl Dialer for TcpDialer {
    type Conn = Socket;

    fn dial(&mut self) -> Result<Socket, Stop> {
        let mut stream = TcpStream::connect_timeout(&self.address, CONNECT_TIMEOUT)
            .map_err(|e| transport(format!("cannot connect to {}: {e}", self.address)))?;
        stream
            .set_read_timeout(Some(CONNECT_TIMEOUT))
            .map_err(|e| transport(format!("cannot set the handshake timeout: {e}")))?;
        let mut rng = OsRng;
        let mut nonce = Vec::with_capacity(16);
        nonce.extend_from_slice(&rng.next_u64().to_be_bytes());
        nonce.extend_from_slice(&rng.next_u64().to_be_bytes());
        let key = base64(&nonce);
        let request = format!(
            "GET {SYNC_PATH} HTTP/1.1\r\nHost: {}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
             Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\
             Sec-WebSocket-Protocol: lattice.sync.v1\r\nAuthorization: Bearer {}\r\n\r\n",
            self.address,
            self.token.as_str(),
        );
        stream
            .write_all(request.as_bytes())
            .map_err(|e| transport(format!("cannot send the upgrade request: {e}")))?;
        let head = read_head(&mut stream).map_err(transport)?;
        let status = head.split_whitespace().nth(1).unwrap_or("none");
        if status != "101" {
            return Err(refused_upgrade(status));
        }
        let expected = websocket::accept_key(&key);
        if header_value(&head, "sec-websocket-accept") != Some(expected.as_str()) {
            return Err(transport(
                "the gateway's Sec-WebSocket-Accept does not match the request".into(),
            ));
        }
        stream
            .set_read_timeout(Some(TICK))
            .map_err(|e| transport(format!("cannot set the read timeout: {e}")))?;
        Ok(Socket {
            stream,
            buf: Vec::new(),
            rng,
        })
    }
}

/// Reads the response head one byte at a time, so no frame bytes that follow it are consumed.
fn read_head(stream: &mut TcpStream) -> Result<String, String> {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() >= websocket::DEFAULT_MAX_HEAD {
            return Err("the gateway's response head is too large".into());
        }
        stream
            .read_exact(&mut byte)
            .map_err(|e| format!("the gateway closed the connection during the upgrade: {e}"))?;
        head.extend_from_slice(&byte);
    }
    String::from_utf8(head).map_err(|_| "the gateway's response head is not text".into())
}

/// The value of a header in a response head, matched without regard to case.
fn header_value<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.lines().skip(1).find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.trim().eq_ignore_ascii_case(name).then(|| value.trim())
    })
}

/// Standard base64 with padding (RFC 4648), for the `Sec-WebSocket-Key` nonce.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk.first().copied().unwrap_or(0);
        let b1 = chunk.get(1).copied();
        let b2 = chunk.get(2).copied();
        let n =
            (u32::from(b0) << 16) | (u32::from(b1.unwrap_or(0)) << 8) | u32::from(b2.unwrap_or(0));
        let symbol = |shift: u32| -> char {
            let index = ((n >> shift) & 63) as usize;
            ALPHABET.get(index).map_or('=', |&c| char::from(c))
        };
        out.push(symbol(18));
        out.push(symbol(12));
        out.push(if b1.is_some() { symbol(6) } else { '=' });
        out.push(if b2.is_some() { symbol(0) } else { '=' });
    }
    out
}

/// One WebSocket connection to the gateway. Client frames are masked with a fresh random key.
struct Socket {
    stream: TcpStream,
    buf: Vec<u8>,
    rng: OsRng,
}

impl Socket {
    fn write(&mut self, opcode: Opcode, payload: &[u8]) -> Result<(), String> {
        let mut out = Vec::new();
        let [a, b, c, d, ..] = self.rng.next_u64().to_be_bytes();
        websocket::encode_client_frame(true, opcode, payload, [a, b, c, d], &mut out)
            .map_err(|e| format!("{e:?}"))?;
        self.stream
            .write_all(&out)
            .map_err(|e| format!("cannot write to the gateway: {e}"))
    }
}

/// The code and the reason text of a close frame's payload. A reason that is not UTF-8, or that is
/// longer than [`MAX_CLOSE_REASON`], comes back empty: it cannot be one the watch knows.
fn close_parts(payload: &[u8]) -> (Option<u16>, String) {
    let code = payload
        .get(..2)
        .and_then(|c| <[u8; 2]>::try_from(c).ok())
        .map(u16::from_be_bytes);
    let reason = payload.get(2..).unwrap_or_default();
    if reason.len() > MAX_CLOSE_REASON {
        return (code, String::new());
    }
    let text = std::str::from_utf8(reason)
        .map(str::to_owned)
        .unwrap_or_default();
    (code, text)
}

impl Connection for Socket {
    fn send(&mut self, frame: &Frame) -> Result<(), String> {
        let mut bytes = Vec::new();
        sync::encode(frame, &mut bytes).map_err(|e| format!("{e:?}"))?;
        self.write(Opcode::Binary, &bytes)
    }

    fn recv(&mut self) -> Result<Received, String> {
        loop {
            let decoded =
                websocket::decode_server_frame(&self.buf, websocket::DEFAULT_MAX_FRAME_PAYLOAD)
                    .map_err(|e| e.to_string())?;
            if let Some((frame, used)) = decoded {
                self.buf.drain(..used);
                match frame.opcode {
                    Opcode::Binary if frame.fin => {
                        let (message, n) = sync::decode(&frame.payload)
                            .map_err(|e| format!("{e:?}"))?
                            .ok_or("an incomplete sync message")?;
                        if n != frame.payload.len() {
                            return Err("bytes after a sync message".into());
                        }
                        return Ok(Received::Frame(message));
                    }
                    Opcode::Ping => self.write(Opcode::Pong, &frame.payload)?,
                    Opcode::Pong => {}
                    Opcode::Close => {
                        let (code, reason) = close_parts(&frame.payload);
                        return Ok(Received::Closed(code, reason));
                    }
                    _ => return Err("the gateway sent a frame this client does not accept".into()),
                }
                continue;
            }
            let mut chunk = [0u8; 4096];
            match self.stream.read(&mut chunk) {
                Ok(0) => return Ok(Received::Closed(None, String::new())),
                Ok(n) => self
                    .buf
                    .extend_from_slice(chunk.get(..n).unwrap_or_default()),
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock
                            | std::io::ErrorKind::TimedOut
                            | std::io::ErrorKind::Interrupted
                    ) =>
                {
                    return Ok(Received::Idle);
                }
                Err(e) => return Err(format!("cannot read from the gateway: {e}")),
            }
        }
    }
}

fn start(opts: Options) -> Result<(), String> {
    let address = check_url(&opts.url)?;
    let token = {
        let stdin = std::io::stdin();
        let mut lock = stdin.lock();
        read_token(&opts.token, &mut lock)?
    };
    latticed::stop::install_stop_signal_handlers()
        .map_err(|e| format!("cannot install the stop signal handlers: {e}"))?;
    let mut dialer = TcpDialer { address, token };
    let mut out = Printer;
    let mut interrupted = || latticed::stop::stop_signals_received() > 0;
    let mut jitter = || OsRng.next_u64();
    let mut pause = |delay: Duration| std::thread::sleep(delay);
    let mut env = Env {
        interrupted: &mut interrupted,
        jitter: &mut jitter,
        pause: &mut pause,
    };
    let table = opts.table.as_str();
    match opts.start {
        Start::Origin => watch(
            table,
            Vec::new(),
            &mut dialer,
            &mut out,
            &mut MemoryCursorStore::new(),
            &mut env,
        ),
        Start::Cursor(from) => watch(
            table,
            from,
            &mut dialer,
            &mut out,
            &mut MemoryCursorStore::new(),
            &mut env,
        ),
        Start::File(path) => {
            let mut store = FileCursorStore::open(Path::new(&path)).map_err(|e| e.to_string())?;
            // Held until the watch returns: a second watch on this file is refused now, before it
            // could print or save anything.
            let _lock = store.lock().map_err(|e| e.to_string())?;
            let from = match store.load() {
                Ok(saved) => saved.map(|cursor| cursor.to_vec()).unwrap_or_default(),
                Err(CursorFileError::Malformed(_)) => {
                    return Err(format!(
                        "{path}: it does not hold a saved cursor (24 lowercase hex digits and an optional newline); move it aside to start again"
                    ));
                }
                Err(e) => return Err(e.to_string()),
            };
            watch(table, from, &mut dialer, &mut out, &mut store, &mut env)
        }
    }
}

/// `lattice watch ...`: exit 0 for help and for a stop that was asked for, 2 for a bad invocation,
/// 1 when the watch cannot run or gives up.
pub(crate) fn command(args: &[String]) -> ExitCode {
    match parse(args) {
        Ok(Parsed::Help) => {
            print!("{USAGE}");
            ExitCode::SUCCESS
        }
        Ok(Parsed::Run(opts)) => match start(opts) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("lattice watch: {e}");
                ExitCode::from(1)
            }
        },
        Err(e) => {
            eprintln!("lattice watch: {e}");
            eprint!("{USAGE}");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;
    use lattice_sdk::client::{Event, SUPPORTED_MAX_VERSION, SUPPORTED_MIN_VERSION, WireError};
    use lattice_sdk::cursor_store::MalformedCursor;
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::rc::Rc;

    /// Hex with spaces allowed, as written in `formats/change-feed-v1.md`.
    fn hex_bytes(text: &str) -> Vec<u8> {
        let digits: Vec<u8> = text.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
        let (pairs, _) = digits.as_chunks::<2>();
        pairs
            .iter()
            .map(|&[hi, lo]| (nibble(hi).unwrap() << 4) | nibble(lo).unwrap())
            .collect()
    }

    /// A stored cursor of 12 bytes from its 24 hex digits.
    fn cursor_of(text: &str) -> FeedCursor {
        hex_bytes(text).try_into().unwrap()
    }

    /// The upsert of key "k" to "vv" at index `index` of a change set of four events, LSN 258. The
    /// index must be below the set size, so the set size is four to cover indices 1 to 3. The bytes
    /// are the worked example of `formats/change-feed-v1.md`, with the index and the set size changed.
    fn upsert_event(index: &str) -> String {
        format!(
            "01 0000000000000102 {index} 04000000 {} 0500000000000000 06000000 07000000 {} 00 02000000 0100 6b 01 02000000 7676",
            "a0".repeat(16),
            "00000000000000000000000000000009"
        )
    }

    /// The delete with a turn from `formats/change-feed-v1.md`: LSN 1, index 0, empty key.
    const DELETE_EVENT: &str = "01 0000000000000001 00000000 01000000 01010101010101010101010101010101 00000000000000000000000000000000 00000000000000000000000000000000 01 1100000000000000 2200000000000000 00000000 0000 00";

    const CURSOR_1: &str = "000000000000010200000001";
    const CURSOR_2: &str = "000000000000010200000002";
    const CURSOR_3: &str = "000000000000010200000003";
    const LINE_1: &str =
        "000000000000010200000001 00000000000000000000000000000009 notes \"k\" upsert";
    const LINE_2: &str =
        "000000000000010200000002 00000000000000000000000000000009 notes \"k\" upsert";
    const LINE_3: &str =
        "000000000000010200000003 00000000000000000000000000000009 notes \"k\" upsert";

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    fn event_frame(sub: u32, events: Vec<(&str, &str)>) -> Received {
        Received::Frame(Frame::Events {
            sub,
            events: events
                .into_iter()
                .map(|(cursor, index)| Event {
                    cursor: hex_bytes(cursor),
                    data: hex_bytes(&upsert_event(index)),
                })
                .collect(),
        })
    }

    fn hello() -> Frame {
        Frame::Hello {
            min_version: SUPPORTED_MIN_VERSION,
            max_version: SUPPORTED_MAX_VERSION,
            agent: AGENT.into(),
        }
    }

    fn welcome() -> Received {
        Received::Frame(Frame::Welcome {
            version: SUPPORTED_MAX_VERSION,
            max_payload: 1 << 20,
            server: "test".into(),
        })
    }

    fn subscribe(cursor: &str) -> Frame {
        Frame::Subscribe {
            sub: 1,
            scope: "table/notes".into(),
            cursor: if cursor.is_empty() {
                Vec::new()
            } else {
                hex_bytes(cursor)
            },
        }
    }

    fn ack(cursor: &str) -> Frame {
        Frame::Ack {
            sub: 1,
            cursor: hex_bytes(cursor),
        }
    }

    #[derive(Debug, PartialEq)]
    enum Step {
        Sent(Frame),
        Printed(String),
        Saved(FeedCursor),
    }

    type Log = Rc<RefCell<Vec<Step>>>;

    /// One scripted connection: what it reads, and where its writes are logged.
    struct Script {
        incoming: VecDeque<Received>,
        log: Log,
    }

    impl Connection for Script {
        fn send(&mut self, frame: &Frame) -> Result<(), String> {
            self.log.borrow_mut().push(Step::Sent(frame.clone()));
            Ok(())
        }

        fn recv(&mut self) -> Result<Received, String> {
            Ok(self
                .incoming
                .pop_front()
                .unwrap_or(Received::Closed(None, String::new())))
        }
    }

    /// Hands out the scripted connections in order. When none is left, a dial fails.
    struct Dialing {
        connections: VecDeque<Vec<Received>>,
        log: Log,
    }

    impl Dialer for Dialing {
        type Conn = Script;

        fn dial(&mut self) -> Result<Script, Stop> {
            let Some(incoming) = self.connections.pop_front() else {
                return Err(transport("no more scripted connections".into()));
            };
            Ok(Script {
                incoming: incoming.into(),
                log: Rc::clone(&self.log),
            })
        }
    }

    struct Recorder {
        log: Log,
    }

    impl Output for Recorder {
        fn line(&mut self, text: &str) -> Result<(), String> {
            self.log.borrow_mut().push(Step::Printed(text.to_owned()));
            Ok(())
        }

        fn note(&mut self, _text: &str) {}
    }

    #[derive(Debug)]
    struct Refused;

    impl From<MalformedCursor> for Refused {
        fn from(_: MalformedCursor) -> Self {
            Refused
        }
    }

    impl fmt::Display for Refused {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("the disk is full")
        }
    }

    /// A store that logs each save, or refuses every save.
    struct Saves {
        log: Log,
        refuse: bool,
    }

    impl CursorStore for Saves {
        type Error = Refused;

        fn load(&self) -> Result<Option<FeedCursor>, Refused> {
            Ok(None)
        }

        fn save(&mut self, cursor: &FeedCursor) -> Result<(), Refused> {
            if self.refuse {
                return Err(Refused);
            }
            self.log.borrow_mut().push(Step::Saved(*cursor));
            Ok(())
        }
    }

    /// Runs one watch over scripted connections with the largest jitter: every wait is the top of its
    /// window. Returns every step in order with the result.
    fn watch_over(
        connections: Vec<Vec<Received>>,
        start: &[u8],
        refuse_saves: bool,
    ) -> (Vec<Step>, Result<(), String>) {
        let (steps, result, _) = run_scripted(connections, start, refuse_saves, &mut || u64::MAX);
        (steps, result)
    }

    /// Runs one watch over scripted connections with the given jitter. Returns every step in order,
    /// the result and each wait the watch took before a retry.
    fn run_scripted(
        connections: Vec<Vec<Received>>,
        start: &[u8],
        refuse_saves: bool,
        jitter: &mut dyn FnMut() -> u64,
    ) -> (Vec<Step>, Result<(), String>, Vec<Duration>) {
        let log: Log = Rc::new(RefCell::new(Vec::new()));
        let mut dialer = Dialing {
            connections: connections.into(),
            log: Rc::clone(&log),
        };
        let mut out = Recorder {
            log: Rc::clone(&log),
        };
        let mut store = Saves {
            log: Rc::clone(&log),
            refuse: refuse_saves,
        };
        let mut waits: Vec<Duration> = Vec::new();
        let result = {
            let mut pause = |delay: Duration| waits.push(delay);
            let mut env = Env {
                interrupted: &mut || false,
                jitter,
                pause: &mut pause,
            };
            watch(
                "notes",
                start.to_vec(),
                &mut dialer,
                &mut out,
                &mut store,
                &mut env,
            )
        };
        let steps = std::mem::take(&mut *log.borrow_mut());
        (steps, result, waits)
    }

    /// A seeded jitter source (splitmix64): the same seed gives the same waits on every run.
    fn seeded(seed: u64) -> impl FnMut() -> u64 {
        let mut state = seed;
        move || {
            state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }
    }

    #[test]
    fn each_event_is_printed_then_acknowledged_then_saved() {
        let incoming = vec![
            welcome(),
            event_frame(1, vec![(CURSOR_1, "00000001")]),
            event_frame(1, vec![(CURSOR_2, "00000002")]),
        ];
        let (steps, result) = watch_over(vec![incoming], &[], false);
        let err = result.unwrap_err();
        assert!(err.contains("giving up"), "{err}");
        // An acknowledgement sent before its line, or a save before its acknowledgement, would
        // change this exact order.
        assert_eq!(
            steps,
            vec![
                Step::Sent(hello()),
                Step::Sent(subscribe("")),
                Step::Printed(LINE_1.into()),
                Step::Sent(ack(CURSOR_1)),
                Step::Saved(cursor_of(CURSOR_1)),
                Step::Printed(LINE_2.into()),
                Step::Sent(ack(CURSOR_2)),
                Step::Saved(cursor_of(CURSOR_2)),
            ]
        );
    }

    #[test]
    fn a_failed_save_stops_the_watch_after_the_acknowledgement_and_before_the_next_event() {
        let incoming = vec![
            welcome(),
            event_frame(1, vec![(CURSOR_1, "00000001"), (CURSOR_2, "00000002")]),
        ];
        let (steps, result) = watch_over(vec![incoming], &[], true);
        let err = result.unwrap_err();
        assert!(err.contains("cannot save the cursor"), "{err}");
        assert_eq!(
            steps,
            vec![
                Step::Sent(hello()),
                Step::Sent(subscribe("")),
                Step::Printed(LINE_1.into()),
                Step::Sent(ack(CURSOR_1)),
            ]
        );
    }

    #[test]
    fn a_restart_resumes_after_the_saved_cursor_with_no_duplicate_and_no_gap() {
        // The first connection delivers events 1 and 2, then drops. The second one resumes after
        // event 2 and replays it: the replay is dropped, so event 3 is the only new line.
        let first = vec![
            welcome(),
            event_frame(1, vec![(CURSOR_1, "00000001"), (CURSOR_2, "00000002")]),
        ];
        let second = vec![
            welcome(),
            event_frame(1, vec![(CURSOR_2, "00000002"), (CURSOR_3, "00000003")]),
        ];
        let (steps, result) = watch_over(vec![first, second], &[], false);
        let err = result.unwrap_err();
        assert!(err.contains("giving up"), "{err}");
        assert_eq!(
            steps,
            vec![
                Step::Sent(hello()),
                Step::Sent(subscribe("")),
                Step::Printed(LINE_1.into()),
                Step::Sent(ack(CURSOR_1)),
                Step::Saved(cursor_of(CURSOR_1)),
                Step::Printed(LINE_2.into()),
                Step::Sent(ack(CURSOR_2)),
                Step::Saved(cursor_of(CURSOR_2)),
                Step::Sent(hello()),
                Step::Sent(subscribe(CURSOR_2)),
                Step::Printed(LINE_3.into()),
                Step::Sent(ack(CURSOR_3)),
                Step::Saved(cursor_of(CURSOR_3)),
            ]
        );
    }

    #[test]
    fn a_new_process_starts_from_the_cursor_it_was_given() {
        let (steps, result) = watch_over(
            vec![vec![
                welcome(),
                event_frame(1, vec![(CURSOR_2, "00000002"), (CURSOR_3, "00000003")]),
            ]],
            &hex_bytes(CURSOR_2),
            false,
        );
        let err = result.unwrap_err();
        assert!(err.contains("giving up"), "{err}");
        assert_eq!(
            steps,
            vec![
                Step::Sent(hello()),
                Step::Sent(subscribe(CURSOR_2)),
                Step::Printed(LINE_3.into()),
                Step::Sent(ack(CURSOR_3)),
                Step::Saved(cursor_of(CURSOR_3)),
            ]
        );
    }

    #[test]
    fn an_interrupted_watch_stops_without_connecting_and_is_not_an_error() {
        let log: Log = Rc::new(RefCell::new(Vec::new()));
        let mut dialer = Dialing {
            connections: vec![vec![welcome()]].into(),
            log: Rc::clone(&log),
        };
        let mut out = Recorder {
            log: Rc::clone(&log),
        };
        let mut store = Saves {
            log: Rc::clone(&log),
            refuse: false,
        };
        let mut env = Env {
            interrupted: &mut || true,
            jitter: &mut || u64::MAX,
            pause: &mut |_: Duration| {},
        };
        let result = watch(
            "notes",
            Vec::new(),
            &mut dialer,
            &mut out,
            &mut store,
            &mut env,
        );
        assert_eq!(result, Ok(()));
        assert!(log.borrow().is_empty());
    }

    #[test]
    fn an_idle_socket_sends_a_ping_once_the_client_has_waited_long_enough() {
        let mut incoming = vec![welcome()];
        incoming.extend(std::iter::repeat_with(|| Received::Idle).take(40));
        let (steps, result) = watch_over(vec![incoming], &[], false);
        let err = result.unwrap_err();
        assert!(err.contains("giving up"), "{err}");
        assert!(
            steps
                .iter()
                .any(|s| matches!(s, Step::Sent(Frame::Ping { .. }))),
            "{steps:?}"
        );
    }

    #[test]
    fn a_delivery_whose_cursor_differs_from_its_event_is_refused_before_anything_is_printed() {
        let incoming = vec![welcome(), event_frame(1, vec![(CURSOR_2, "00000001")])];
        let (steps, result) = watch_over(vec![incoming], &[], false);
        assert!(result.unwrap_err().contains("does not carry the cursor"));
        assert_eq!(steps, vec![Step::Sent(hello()), Step::Sent(subscribe(""))]);
    }

    #[test]
    fn an_undecodable_event_ends_the_watch_without_a_line_or_an_ack() {
        let incoming = vec![
            welcome(),
            Received::Frame(Frame::Events {
                sub: 1,
                events: vec![Event {
                    cursor: hex_bytes(CURSOR_1),
                    data: vec![9, 9, 9],
                }],
            }),
        ];
        let (steps, result) = watch_over(vec![incoming], &[], false);
        assert!(result.unwrap_err().contains("undecodable event"));
        assert!(steps.iter().all(|s| !matches!(
            s,
            Step::Printed(_) | Step::Sent(Frame::Ack { .. }) | Step::Saved(_)
        )));
    }

    #[test]
    fn a_connection_that_is_never_dialed_gives_up_after_bounded_tries() {
        // Nothing is scripted: every dial fails, so the watch stops after the bounded tries.
        let (steps, result) = watch_over(Vec::new(), &[], false);
        let err = result.unwrap_err();
        assert!(
            err.contains("giving up after 3 failed connections"),
            "{err}"
        );
        assert!(steps.is_empty());
    }

    #[test]
    fn a_welcome_then_drop_without_an_event_is_bounded_and_the_wait_grows_each_time() {
        let cycle = || vec![welcome(), Received::Closed(None, String::new())];
        let (steps, result, waits) = run_scripted(
            vec![cycle(), cycle(), cycle(), cycle()],
            &[],
            false,
            &mut || u64::MAX,
        );
        let err = result.unwrap_err();
        assert!(
            err.contains("giving up after 3 failed connections"),
            "{err}"
        );
        // The windows are 1 s then 2 s. The third failure gives up, so no third wait is taken.
        assert_eq!(waits, vec![Duration::from_secs(1), Duration::from_secs(2)]);
        // Three connections were made, each handshaken and subscribed, and none delivered. The
        // fourth scripted connection is never dialed.
        let handshake = || vec![Step::Sent(hello()), Step::Sent(subscribe(""))];
        let mut expected = handshake();
        expected.extend(handshake());
        expected.extend(handshake());
        assert_eq!(steps, expected);
    }

    #[test]
    fn an_event_then_drop_resets_the_wait_to_its_first_window() {
        // Connection 1 drops with no event: window 1 s. Connection 2 delivers event 1 and then drops:
        // the delivery resets the supervisor, so its wait is 1 s again, not 2 s. Connection 3 drops
        // with no event: window 2 s. Connection 4 is the third failure in a row, so the watch gives up.
        let silent = || vec![welcome(), Received::Closed(None, String::new())];
        let (steps, result, waits) = run_scripted(
            vec![
                silent(),
                vec![
                    welcome(),
                    event_frame(1, vec![(CURSOR_1, "00000001")]),
                    Received::Closed(None, String::new()),
                ],
                silent(),
                silent(),
            ],
            &[],
            false,
            &mut || u64::MAX,
        );
        let err = result.unwrap_err();
        assert!(
            err.contains("giving up after 3 failed connections"),
            "{err}"
        );
        assert_eq!(
            waits,
            vec![
                Duration::from_secs(1),
                Duration::from_secs(1),
                Duration::from_secs(2),
            ]
        );
        // The retries after the delivery resume after event 1, which was saved before the drop.
        let handshake = |cursor: &str| vec![Step::Sent(hello()), Step::Sent(subscribe(cursor))];
        let mut expected = handshake("");
        expected.extend(handshake(""));
        expected.extend([
            Step::Printed(LINE_1.into()),
            Step::Sent(ack(CURSOR_1)),
            Step::Saved(cursor_of(CURSOR_1)),
        ]);
        expected.extend(handshake(CURSOR_1));
        expected.extend(handshake(CURSOR_1));
        assert_eq!(steps, expected);
    }

    #[test]
    fn a_seeded_jitter_keeps_each_wait_inside_its_window_and_never_below_one_second() {
        let cycle = || vec![welcome(), Received::Closed(None, String::new())];
        let mut jitter = seeded(7);
        let (_, result, waits) =
            run_scripted(vec![cycle(), cycle(), cycle()], &[], false, &mut jitter);
        assert!(result.unwrap_err().contains("giving up"));
        assert_eq!(waits.len(), 2, "{waits:?}");
        // The first window is 0 to 1 s, so the floor of one second applies. The second is 0 to 2 s.
        assert_eq!(waits[0], Duration::from_secs(1));
        assert!(
            waits[1] >= Duration::from_secs(1) && waits[1] <= Duration::from_secs(2),
            "{waits:?}"
        );
    }

    #[test]
    fn the_supervisor_windows_are_one_second_doubling_to_eight() {
        let mut backoff = Backoff::new(RETRY_BASE_MS, RETRY_CAP_MS).unwrap();
        let windows: Vec<u64> = (0..5).map(|_| backoff.next_delay(u64::MAX)).collect();
        assert_eq!(windows, vec![1_000, 2_000, 4_000, 8_000, 8_000]);
    }

    #[test]
    fn a_credential_close_is_fatal_and_no_further_connection_is_made() {
        // The gateway closes a live connection with 1008 when its token stops verifying.
        let revoked = vec![
            welcome(),
            Received::Closed(Some(POLICY_CLOSE), "credential no longer valid".into()),
        ];
        let (steps, result, waits) =
            run_scripted(vec![revoked, vec![welcome()]], &[], false, &mut || u64::MAX);
        let err = result.unwrap_err();
        assert!(err.contains("1008"), "{err}");
        assert!(err.contains("token"), "{err}");
        assert!(!err.contains("giving up"), "{err}");
        assert!(waits.is_empty(), "{waits:?}");
        assert_eq!(steps, vec![Step::Sent(hello()), Step::Sent(subscribe(""))]);
    }

    #[test]
    fn a_version_refusal_close_is_fatal_and_does_not_blame_the_token() {
        // The gateway refuses the Hello with 1008 and the reason "refused". No credential is involved,
        // so the message says the protocol version is not spoken, and a new connection is not made.
        let refused = vec![Received::Closed(Some(POLICY_CLOSE), "refused".into())];
        let (steps, result, waits) =
            run_scripted(vec![refused, vec![welcome()]], &[], false, &mut || u64::MAX);
        let err = result.unwrap_err();
        assert!(err.contains("1008"), "{err}");
        assert!(err.contains("protocol version"), "{err}");
        assert!(!err.contains("token"), "{err}");
        assert!(!err.contains("giving up"), "{err}");
        assert!(waits.is_empty(), "{waits:?}");
        assert_eq!(steps, vec![Step::Sent(hello())]);
    }

    #[test]
    fn a_timeout_or_unknown_policy_close_is_retried_and_not_blamed_on_the_token() {
        // The gateway closes an idle connection with 1008 and the reason "timeout", or with no reason
        // the watch knows. A new connection from the saved cursor can succeed, so each close is a
        // dropped connection: the supervisor waits and dials again, and gives up after three in a row.
        for reason in ["timeout", ""] {
            let idle = || {
                vec![
                    welcome(),
                    Received::Closed(Some(POLICY_CLOSE), reason.into()),
                ]
            };
            let (steps, result, waits) =
                run_scripted(vec![idle(), idle(), idle()], &[], false, &mut || u64::MAX);
            let err = result.unwrap_err();
            assert!(
                err.contains("giving up after 3 failed connections"),
                "{reason:?}: {err}"
            );
            assert!(!err.contains("token"), "{reason:?}: {err}");
            assert_eq!(
                waits,
                vec![Duration::from_secs(1), Duration::from_secs(2)],
                "{reason:?}"
            );
            let handshake = || vec![Step::Sent(hello()), Step::Sent(subscribe(""))];
            let mut expected = handshake();
            expected.extend(handshake());
            expected.extend(handshake());
            assert_eq!(steps, expected, "{reason:?}");
        }
    }

    #[test]
    fn a_close_frame_carries_its_code_and_its_reason() {
        // 1008 is 0x03 0xF0 in network byte order. The reason is the ASCII text the gateway sends.
        let mut payload = vec![0x03, 0xF0];
        payload.extend_from_slice(b"timeout");
        assert_eq!(close_parts(&payload), (Some(1008), "timeout".to_owned()));
    }

    #[test]
    fn a_close_frame_without_a_code_or_with_a_reason_the_watch_cannot_read_has_no_reason() {
        assert_eq!(close_parts(&[]), (None, String::new()));
        assert_eq!(close_parts(&[0x03]), (None, String::new()));
        assert_eq!(close_parts(&[0x03, 0xF0]), (Some(1008), String::new()));
        // 0xFF never occurs in UTF-8, so this reason is not text.
        assert_eq!(
            close_parts(&[0x03, 0xF0, b'a', 0xFF]),
            (Some(1008), String::new())
        );
    }

    #[test]
    fn a_close_reason_of_123_bytes_is_kept_and_a_longer_one_is_dropped() {
        let mut exact = vec![0x03, 0xF0];
        exact.extend(std::iter::repeat_n(b'a', 123));
        assert_eq!(close_parts(&exact), (Some(1008), "a".repeat(123)));
        let mut long = vec![0x03, 0xF0];
        long.extend(std::iter::repeat_n(b'a', 124));
        assert_eq!(close_parts(&long), (Some(1008), String::new()));
    }

    /// A refusal of subscription 1 by the server, as the gateway sends it.
    fn subscription_error(code: WireError) -> Received {
        Received::Frame(Frame::Error {
            code,
            sub: 1,
            message: "refused".into(),
        })
    }

    #[test]
    fn a_subscription_refused_with_a_permanent_code_stops_at_once() {
        // The codes a new connection would repeat, and the version refusal: no retry, no second
        // connection, and no wait.
        for code in [
            WireError::Malformed,
            WireError::UnknownSubscription,
            WireError::InvalidScope,
            WireError::InvalidCursor,
            WireError::TooManySubscriptions,
            WireError::UnsupportedVersion,
        ] {
            let refused = vec![welcome(), subscription_error(code)];
            let (steps, result, waits) =
                run_scripted(vec![refused, vec![welcome()]], &[], false, &mut || u64::MAX);
            let err = result.unwrap_err();
            assert!(!err.contains("giving up"), "{code:?}: {err}");
            assert!(waits.is_empty(), "{code:?}: {waits:?}");
            assert_eq!(
                steps,
                vec![Step::Sent(hello()), Step::Sent(subscribe(""))],
                "{code:?}"
            );
        }
    }

    #[test]
    fn a_subscription_refused_with_an_internal_code_is_retried_within_the_bound() {
        let refused = || vec![welcome(), subscription_error(WireError::Internal)];
        let (_, result, waits) = run_scripted(
            vec![refused(), refused(), refused()],
            &[],
            false,
            &mut || u64::MAX,
        );
        assert!(
            result
                .unwrap_err()
                .contains("giving up after 3 failed connections"),
        );
        assert_eq!(waits, vec![Duration::from_secs(1), Duration::from_secs(2)]);
    }

    #[test]
    fn a_protocol_violation_is_retried_within_the_bound() {
        // Events before the welcome break the protocol: the client closes with ProtocolViolation.
        let early = || vec![event_frame(1, vec![(CURSOR_1, "00000001")])];
        let (steps, result, waits) =
            run_scripted(vec![early(), early(), early()], &[], false, &mut || {
                u64::MAX
            });
        assert!(
            result
                .unwrap_err()
                .contains("giving up after 3 failed connections"),
        );
        assert_eq!(waits, vec![Duration::from_secs(1), Duration::from_secs(2)]);
        assert!(
            steps
                .iter()
                .all(|s| !matches!(s, Step::Printed(_) | Step::Sent(Frame::Subscribe { .. }))),
            "{steps:?}"
        );
    }

    #[test]
    fn a_refused_token_is_fatal_and_other_statuses_are_retried_with_the_status_escaped() {
        assert!(matches!(
            refused_upgrade("401"),
            Stop::Ended(Input::Unauthorized, _)
        ));
        assert!(matches!(
            refused_upgrade("403"),
            Stop::Ended(Input::Unauthorized, _)
        ));
        assert!(matches!(
            refused_upgrade("503"),
            Stop::Ended(Input::TransportClosed, _)
        ));
        match refused_upgrade("50\x1b\n") {
            Stop::Ended(Input::TransportClosed, text) => {
                assert!(!text.contains('\x1b'), "{text:?}");
                assert!(text.contains("\\n"), "{text:?}");
            }
            other => panic!("{other:?}"),
        }
    }

    /// A real file store that fails after `allowed` saves. To the watch this is a crash between an
    /// acknowledgement and its save.
    struct CrashAfter {
        inner: FileCursorStore,
        allowed: u32,
    }

    impl CursorStore for CrashAfter {
        type Error = CursorFileError;

        fn load(&self) -> Result<Option<FeedCursor>, CursorFileError> {
            self.inner.load()
        }

        fn save(&mut self, cursor: &FeedCursor) -> Result<(), CursorFileError> {
            if self.allowed == 0 {
                return Err(CursorFileError::Io(
                    "simulated crash before the save".into(),
                ));
            }
            self.allowed -= 1;
            self.inner.save(cursor)
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_crash_between_an_ack_and_its_save_reprints_on_restart_and_never_skips() {
        let dir = std::env::temp_dir().join(format!("lattice-watch-crash-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cursor");
        let log: Log = Rc::new(RefCell::new(Vec::new()));

        // First run: event 1 is printed, acknowledged and saved; the save of event 2 never happens.
        let mut dialer = Dialing {
            connections: vec![vec![
                welcome(),
                event_frame(1, vec![(CURSOR_1, "00000001"), (CURSOR_2, "00000002")]),
            ]]
            .into(),
            log: Rc::clone(&log),
        };
        let mut out = Recorder {
            log: Rc::clone(&log),
        };
        let mut store = CrashAfter {
            inner: FileCursorStore::open(&path).unwrap(),
            allowed: 1,
        };
        let mut env = Env {
            interrupted: &mut || false,
            jitter: &mut || u64::MAX,
            pause: &mut |_: Duration| {},
        };
        let first = watch(
            "notes",
            Vec::new(),
            &mut dialer,
            &mut out,
            &mut store,
            &mut env,
        );
        assert!(first.unwrap_err().contains("simulated crash"));

        // Restart: the file holds event 1, so the watch resumes after it and the server replays
        // event 2 before event 3.
        let saved = FileCursorStore::open(&path).unwrap().load().unwrap();
        assert_eq!(saved, Some(cursor_of(CURSOR_1)));
        let mut dialer = Dialing {
            connections: vec![vec![
                welcome(),
                event_frame(1, vec![(CURSOR_2, "00000002"), (CURSOR_3, "00000003")]),
            ]]
            .into(),
            log: Rc::clone(&log),
        };
        let mut out = Recorder {
            log: Rc::clone(&log),
        };
        let mut store = CrashAfter {
            inner: FileCursorStore::open(&path).unwrap(),
            allowed: u32::MAX,
        };
        let resume = saved.map(|c| c.to_vec()).unwrap_or_default();
        let mut env = Env {
            interrupted: &mut || false,
            jitter: &mut || u64::MAX,
            pause: &mut |_: Duration| {},
        };
        let second = watch("notes", resume, &mut dialer, &mut out, &mut store, &mut env);
        let err = second.unwrap_err();
        assert!(err.contains("giving up"), "{err}");

        let printed: Vec<String> = log
            .borrow()
            .iter()
            .filter_map(|step| match step {
                Step::Printed(text) => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            printed,
            vec![
                LINE_1.to_owned(),
                LINE_2.to_owned(),
                LINE_2.to_owned(),
                LINE_3.to_owned(),
            ]
        );
        assert_eq!(
            FileCursorStore::open(&path).unwrap().load().unwrap(),
            Some(cursor_of(CURSOR_3))
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_upsert_line_names_cursor_author_table_key_and_operation() {
        let event = ChangeEvent::decode(&hex_bytes(&upsert_event("00000001"))).unwrap();
        assert_eq!(format_event("notes", &event), LINE_1);
    }

    #[test]
    fn a_delete_line_shows_the_empty_key_as_empty_quotes() {
        let event = ChangeEvent::decode(&hex_bytes(DELETE_EVENT)).unwrap();
        assert_eq!(
            format_event("notes", &event),
            "000000000000000100000000 00000000000000000000000000000000 notes \"\" delete"
        );
    }

    #[test]
    fn keys_are_quoted_and_escaped_so_a_key_cannot_forge_a_line() {
        assert_eq!(key_text(b"a b"), "\"a b\"");
        assert_eq!(key_text(b"x\x1b[2J"), "\"x\\u{1b}[2J\"");
        assert_eq!(key_text(b"line\nbreak"), "\"line\\nbreak\"");
        assert_eq!(key_text(&[0xff]), "x\"ff\"");
        assert_eq!(key_text(b""), "\"\"");
    }

    #[test]
    fn from_takes_exactly_24_lowercase_hex_digits_that_name_a_valid_cursor() {
        assert_eq!(parse_cursor(CURSOR_1).unwrap(), hex_bytes(CURSOR_1));
        assert!(parse_cursor("000000000000010200000001A").is_err());
        assert!(parse_cursor("000000000000010200000001").is_ok());
        assert!(parse_cursor("00000000000001020000000A").is_err());
        assert!(parse_cursor("00000000000001020000000").is_err());
        assert!(parse_cursor("0000000000000102000000011").is_err());
        assert!(parse_cursor("0000000000000102000000g1").is_err());
        // LSN 0 with a non-zero index is not a cursor.
        assert!(parse_cursor("000000000000000000000001").is_err());
    }

    #[test]
    fn only_loopback_ws_urls_with_a_port_are_accepted() {
        for ok in [
            "ws://127.0.0.1:7000",
            "ws://127.0.0.2:9",
            "ws://localhost:7000/",
            "ws://[::1]:7000",
            "ws://127.0.0.1:7000/sync",
        ] {
            assert!(check_url(ok).is_ok(), "{ok}");
        }
        for bad in [
            "wss://127.0.0.1:7000",
            "http://127.0.0.1:7000",
            "ws://192.0.2.10:7000",
            "ws://0.0.0.0:7000",
            "ws://example.test:7000",
            "ws://localhost.example:7000",
            "ws://127.0.0.1",
            "ws://127.0.0.1:0",
            "ws://127.0.0.1:70000",
            "ws://user@127.0.0.1:7000",
            "ws://127.0.0.1:7000/?token=x",
            "ws://127.0.0.1@evil.com",
            "ws://127.0.0.1@evil.com:5433",
            "WS://127.0.0.1:5433/sync",
            "ws://127.0.0.1.evil.com:5433",
        ] {
            assert!(check_url(bad).is_err(), "{bad}");
        }
        let tls = check_url("wss://127.0.0.1:7000").unwrap_err();
        assert!(tls.contains("TLS"), "{tls}");
        let remote = check_url("ws://192.0.2.10:7000").unwrap_err();
        assert!(remote.contains("loopback"), "{remote}");
    }

    #[test]
    fn localhost_connects_to_the_ipv4_loopback_address_it_was_checked_as() {
        assert_eq!(
            check_url("ws://localhost:7000").unwrap(),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 7000)
        );
        assert_eq!(
            check_url("ws://[::1]:7000").unwrap(),
            SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 7000)
        );
    }

    #[test]
    fn the_token_is_one_trimmed_line_of_printable_ascii() {
        let token = |bytes: &[u8]| {
            let mut input = bytes;
            read_token(&TokenSource::Stdin, &mut input).map(|t| t.as_str().to_owned())
        };
        assert_eq!(token(b"abc\n").unwrap(), "abc");
        assert_eq!(token(b"abc\r\n").unwrap(), "abc");
        assert_eq!(token(b"abc").unwrap(), "abc");
        assert!(token(b"").is_err());
        assert!(token(b"\n").is_err());
        assert!(token(b"a b\n").is_err());
        assert!(token(b"a\tb\n").is_err());
        assert!(token(b"\xff\n").is_err());
        let mut long = vec![b'a'; 4096];
        assert!(token(&long).is_ok());
        long.push(b'a');
        assert!(token(&long).is_err());
    }

    #[test]
    fn base64_is_the_standard_alphabet_with_padding() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"ab"), "YWI=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        // The RFC 6455 sample nonce: a 16-byte key, 24 characters.
        assert_eq!(base64(b"the sample nonce"), "dGhlIHNhbXBsZSBub25jZQ==");
        assert_eq!(base64(&[0u8; 16]).len(), 24);
    }

    #[test]
    fn the_accept_key_is_the_rfc_6455_sample() {
        assert_eq!(
            websocket::accept_key("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    #[test]
    fn a_valid_invocation_reads_its_arguments_and_names_the_stdin_token() {
        let parsed = parse(&args(&[
            "table/notes",
            "--url",
            "ws://127.0.0.1:7000",
            "--token-file",
            "-",
            "--from-start",
        ]))
        .unwrap();
        assert_eq!(
            parsed,
            Parsed::Run(Options {
                table: "notes".into(),
                url: "ws://127.0.0.1:7000".into(),
                token: TokenSource::Stdin,
                start: Start::Origin,
            })
        );
        let parsed = parse(&args(&[
            "table/notes",
            "--token-file",
            "/tmp/token",
            "--from",
            CURSOR_1,
            "--url",
            "ws://localhost:7000",
        ]))
        .unwrap();
        assert_eq!(
            parsed,
            Parsed::Run(Options {
                table: "notes".into(),
                url: "ws://localhost:7000".into(),
                token: TokenSource::File("/tmp/token".into()),
                start: Start::Cursor(hex_bytes(CURSOR_1)),
            })
        );
        let parsed = parse(&args(&[
            "table/notes",
            "--token-file",
            "/tmp/token",
            "--cursor-file",
            "/tmp/cursor",
            "--url",
            "ws://127.0.0.1:7000",
        ]))
        .unwrap();
        assert_eq!(
            parsed,
            Parsed::Run(Options {
                table: "notes".into(),
                url: "ws://127.0.0.1:7000".into(),
                token: TokenSource::File("/tmp/token".into()),
                start: Start::File("/tmp/cursor".into()),
            })
        );
    }

    #[test]
    fn a_token_on_the_command_line_is_refused_without_echoing_it() {
        for flag in [vec!["--token", "s3cret"], vec!["--token=s3cret"]] {
            let mut list = vec![
                "table/notes",
                "--url",
                "ws://127.0.0.1:7000",
                "--from-start",
            ];
            list.extend(flag);
            let err = parse(&args(&list)).unwrap_err();
            assert!(err.contains("command line"), "{err}");
            assert!(!err.contains("s3cret"), "{err}");
        }
    }

    #[test]
    fn the_start_position_is_exactly_one_of_from_from_start_and_cursor_file() {
        let base = [
            "table/notes",
            "--url",
            "ws://127.0.0.1:7000",
            "--token-file",
            "-",
        ];
        let neither = parse(&args(&base)).unwrap_err();
        assert!(neither.contains("--from-start"), "{neither}");
        assert!(neither.contains("--cursor-file"), "{neither}");
        let mut two = base.to_vec();
        two.extend(["--from-start", "--from", CURSOR_1]);
        let err = parse(&args(&two)).unwrap_err();
        assert!(err.contains("only one"), "{err}");
        let mut file_and_start = base.to_vec();
        file_and_start.extend(["--cursor-file", "/tmp/c", "--from-start"]);
        assert!(
            parse(&args(&file_and_start))
                .unwrap_err()
                .contains("only one")
        );
    }

    #[test]
    fn unknown_repeated_and_missing_parts_are_refused() {
        let good = [
            "table/notes",
            "--url",
            "ws://127.0.0.1:7000",
            "--token-file",
            "-",
            "--from-start",
        ];
        assert!(parse(&args(&good)).is_ok());
        let mut unknown = good.to_vec();
        unknown.push("--frobnicate");
        assert!(
            parse(&args(&unknown))
                .unwrap_err()
                .contains("unknown option")
        );
        let mut twice = good.to_vec();
        twice.extend(["--url", "ws://127.0.0.1:7001"]);
        assert!(parse(&args(&twice)).unwrap_err().contains("given twice"));
        let mut two_tables = good.to_vec();
        two_tables.push("table/other");
        assert!(
            parse(&args(&two_tables))
                .unwrap_err()
                .contains("only one table")
        );
        let no_prefix = [
            "notes",
            "--url",
            "ws://127.0.0.1:7000",
            "--token-file",
            "-",
            "--from-start",
        ];
        assert!(parse(&args(&no_prefix)).is_err());
        let no_url = ["table/notes", "--token-file", "-", "--from-start"];
        assert!(parse(&args(&no_url)).unwrap_err().contains("--url"));
        let no_token = [
            "table/notes",
            "--url",
            "ws://127.0.0.1:7000",
            "--from-start",
        ];
        assert!(
            parse(&args(&no_token))
                .unwrap_err()
                .contains("--token-file")
        );
        let no_value = ["table/notes", "--url", "--from-start"];
        assert!(
            parse(&args(&no_value))
                .unwrap_err()
                .contains("needs a value")
        );
    }

    #[test]
    fn table_names_follow_the_catalog_rule() {
        assert_eq!(table_of("table/notes_2").unwrap(), "notes_2");
        for bad in ["table/", "table/Notes", "table/a-b", "notes", "table/a.b"] {
            assert!(table_of(bad).is_err(), "{bad}");
        }
        assert!(table_of(&format!("table/{}", "a".repeat(64))).is_ok());
        assert!(table_of(&format!("table/{}", "a".repeat(65))).is_err());
    }

    #[test]
    fn help_is_asked_by_either_spelling_and_needs_no_other_argument() {
        assert_eq!(parse(&args(&["--help"])).unwrap(), Parsed::Help);
        assert_eq!(parse(&args(&["table/notes", "-h"])).unwrap(), Parsed::Help);
    }
}

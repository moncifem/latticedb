//! Socket-level expectations are encoded independently of the production codec.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::disallowed_methods,
    clippy::disallowed_types
)]

use latticed::{ServerConfig, ServerHandle};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

fn server(max_connections: usize, read_timeout: Duration) -> ServerHandle {
    ServerHandle::start(ServerConfig {
        address: SocketAddr::from(([127, 0, 0, 1], 0)),
        max_connections,
        read_timeout,
        write_timeout: Duration::from_secs(2),
        ..ServerConfig::default()
    })
    .unwrap()
}
fn connect(address: SocketAddr) -> TcpStream {
    let stream = TcpStream::connect_timeout(&address, Duration::from_secs(2)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    stream.set_nodelay(true).unwrap();
    stream
}
fn startup() -> Vec<u8> {
    let mut packet = vec![0, 0, 0, 0, 0, 3, 0, 0];
    packet.extend_from_slice(b"user\0socket-test\0database\0test\0\0");
    let len = (packet.len() as i32).to_be_bytes();
    packet[..4].copy_from_slice(&len);
    packet
}
fn frontend(tag: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = vec![tag];
    out.extend_from_slice(&((payload.len() + 4) as i32).to_be_bytes());
    out.extend_from_slice(payload);
    out
}
fn message(stream: &mut TcpStream) -> (u8, Vec<u8>) {
    let mut tag = [0];
    stream.read_exact(&mut tag).unwrap();
    let mut length = [0; 4];
    stream.read_exact(&mut length).unwrap();
    let n = i32::from_be_bytes(length);
    assert!((4..=1_048_576).contains(&n));
    let mut payload = vec![0; n as usize - 4];
    stream.read_exact(&mut payload).unwrap();
    (tag[0], payload)
}
fn ready(stream: &mut TcpStream) {
    assert_eq!(message(stream), (b'R', vec![0, 0, 0, 0]));
    assert_eq!(message(stream).0, b'S');
    assert_eq!(message(stream).0, b'S');
    let (tag, key) = message(stream);
    assert_eq!(tag, b'K');
    assert_eq!(key.len(), 8);
    assert_eq!(message(stream), (b'Z', vec![b'I']));
}
fn data_row() -> Vec<u8> {
    vec![0, 1, 0, 0, 0, 1, b'1']
}
fn column() -> Vec<u8> {
    let mut bytes = vec![0, 1];
    bytes.extend_from_slice(b"?column?\0");
    bytes.extend_from_slice(&0i32.to_be_bytes());
    bytes.extend_from_slice(&0i16.to_be_bytes());
    bytes.extend_from_slice(&23i32.to_be_bytes());
    bytes.extend_from_slice(&4i16.to_be_bytes());
    bytes.extend_from_slice(&(-1i32).to_be_bytes());
    bytes.extend_from_slice(&0i16.to_be_bytes());
    bytes
}
fn closed(stream: &mut TcpStream) {
    let mut byte = [0];
    match stream.read(&mut byte) {
        Ok(0) => {}
        Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => {}
        other => panic!("socket did not close: {other:?}"),
    }
}

#[test]
fn fragmented_startup_simple_query_error_and_terminate_use_the_existing_session() {
    let server = server(4, Duration::from_secs(3));
    let mut client = connect(server.local_addr());
    for byte in startup() {
        client.write_all(&[byte]).unwrap();
    }
    ready(&mut client);
    client.write_all(&frontend(b'Q', b"SELECT 1\0")).unwrap();
    assert_eq!(message(&mut client), (b'T', column()));
    assert_eq!(message(&mut client), (b'D', data_row()));
    assert_eq!(message(&mut client), (b'C', b"SELECT 1\0".to_vec()));
    assert_eq!(message(&mut client), (b'Z', vec![b'I']));
    client.write_all(&frontend(b'Q', b"SELECT 2\0")).unwrap();
    let (tag, error) = message(&mut client);
    assert_eq!(tag, b'E');
    assert!(error.windows(7).any(|bytes| bytes == b"C0A000\0"));
    assert_eq!(message(&mut client), (b'Z', vec![b'I']));
    client.write_all(&frontend(b'X', b"")).unwrap();
    closed(&mut client);
    server.shutdown().unwrap();
}

#[test]
fn pipelined_ssl_startup_and_extended_query_are_framed_without_losing_bytes() {
    let server = server(4, Duration::from_secs(3));
    let mut client = connect(server.local_addr());
    let mut pipeline = vec![0, 0, 0, 8, 4, 210, 22, 47]; // SSLRequest 80877103
    pipeline.extend_from_slice(&startup());
    client.write_all(&pipeline).unwrap();
    let mut declined = [0];
    client.read_exact(&mut declined).unwrap();
    assert_eq!(declined, [b'N']);
    ready(&mut client);
    let mut pipeline = frontend(b'P', b"\0SELECT 1\0\0\0");
    pipeline.extend_from_slice(&frontend(b'B', &[0; 8]));
    pipeline.extend_from_slice(&frontend(b'E', &[0; 5]));
    pipeline.extend_from_slice(&frontend(b'S', b""));
    client.write_all(&pipeline).unwrap();
    assert_eq!(message(&mut client), (b'1', vec![]));
    assert_eq!(message(&mut client), (b'2', vec![]));
    assert_eq!(message(&mut client), (b'D', data_row()));
    assert_eq!(message(&mut client), (b'C', b"SELECT 1\0".to_vec()));
    assert_eq!(message(&mut client), (b'Z', vec![b'I']));
    server.shutdown().unwrap();
    closed(&mut client);
}

#[test]
fn excess_connections_are_rejected_and_finished_workers_release_capacity() {
    let server = server(1, Duration::from_secs(3));
    let mut first = connect(server.local_addr());
    first.write_all(&startup()).unwrap();
    ready(&mut first);
    let mut excess = connect(server.local_addr());
    excess.write_all(&startup()).unwrap();
    let (tag, error) = message(&mut excess);
    assert_eq!(tag, b'E');
    assert!(error.windows(7).any(|bytes| bytes == b"C53300\0"));
    closed(&mut excess);
    first.write_all(&frontend(b'X', b"")).unwrap();
    closed(&mut first);
    // A worker may finish just after accept; poll a bounded interval rather than assume scheduling.
    let until = Instant::now() + Duration::from_secs(3);
    loop {
        assert!(Instant::now() < until, "connection slot was never released");
        let mut next = connect(server.local_addr());
        next.write_all(&startup()).unwrap();
        let (tag, payload) = message(&mut next);
        if tag == b'R' {
            assert_eq!(payload, vec![0; 4]);
            break;
        }
        assert_eq!(tag, b'E');
        std::thread::sleep(Duration::from_millis(5));
    }
    server.shutdown().unwrap();
}

#[test]
fn invalid_frame_lengths_are_rejected_before_waiting_for_or_allocating_the_body() {
    let server = server(4, Duration::from_secs(3));
    for length in [3i32, -1, i32::MAX] {
        let mut client = connect(server.local_addr());
        client.write_all(&length.to_be_bytes()).unwrap();
        let (tag, error) = message(&mut client);
        assert_eq!(tag, b'E');
        assert!(error.windows(7).any(|bytes| bytes == b"C08P01\0"));
        closed(&mut client);
    }
    server.shutdown().unwrap();
}

#[test]
fn partial_frames_time_out_and_shutdown_interrupts_an_active_long_read() {
    let short = server(2, Duration::from_millis(100));
    let mut client = connect(short.local_addr());
    client.write_all(&[0]).unwrap();
    closed(&mut client);
    short.shutdown().unwrap();
    let long = server(2, Duration::from_secs(30));
    let mut client = connect(long.local_addr());
    client.write_all(&startup()).unwrap();
    ready(&mut client);
    client.write_all(&[b'Q', 0]).unwrap();
    let start = Instant::now();
    long.shutdown().unwrap();
    assert!(
        start.elapsed() < Duration::from_secs(3),
        "shutdown waited for the client's 30-second timeout"
    );
    closed(&mut client);
}

#[test]
fn zero_limits_are_refused_and_dropping_the_handle_closes_connections() {
    for config in [
        ServerConfig {
            max_connections: 0,
            ..ServerConfig::default()
        },
        ServerConfig {
            read_timeout: Duration::ZERO,
            ..ServerConfig::default()
        },
        ServerConfig {
            write_timeout: Duration::ZERO,
            ..ServerConfig::default()
        },
    ] {
        assert!(ServerHandle::start(config).is_err());
    }
    let server = server(2, Duration::from_secs(30));
    let mut client = connect(server.local_addr());
    client.write_all(&startup()).unwrap();
    ready(&mut client);
    drop(server);
    closed(&mut client);
}

// These tests send independent protocol bytes to the real host adapter. WAIT exists only in this
// injected handler and cooperatively polls the very token supplied through the session SPI.
struct Cooperative {
    started: std::sync::mpsc::Sender<()>,
}
fn output_one() -> lattice_api::session::QueryOutput {
    lattice_api::session::QueryOutput {
        columns: vec![lattice_api::pgwire::Column {
            name: "?column?".into(),
            table_oid: 0,
            column_attr: 0,
            type_oid: 23,
            type_size: 4,
            type_modifier: -1,
            format: 0,
        }],
        rows: vec![vec![Some(b"1".to_vec())]],
        tag: "SELECT 1".into(),
    }
}
impl lattice_api::session::QueryHandler for Cooperative {
    fn execute(
        &mut self,
        sql: &str,
        _: &[Option<Vec<u8>>],
    ) -> Result<lattice_api::session::QueryOutput, lattice_api::session::QueryError> {
        if sql == "SELECT 1" {
            Ok(output_one())
        } else {
            Err(lattice_api::session::QueryError::new(
                "XX000",
                "legacy execution must not run WAIT",
            ))
        }
    }
    fn execute_cancellable(
        &mut self,
        sql: &str,
        params: &[Option<Vec<u8>>],
        token: &lattice_api::cancel::CancellationToken,
    ) -> Result<lattice_api::session::QueryOutput, lattice_api::session::QueryError> {
        if sql != "WAIT" {
            return self.execute(sql, params);
        }
        self.started.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !token.is_cancelled() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        if token.is_cancelled() {
            Err(lattice_api::session::QueryError::new(
                "57014",
                "cancelled by test request",
            ))
        } else {
            Err(lattice_api::session::QueryError::new(
                "XX000",
                "test handler did not receive cancellation",
            ))
        }
    }
    fn describe(
        &mut self,
        _: &str,
    ) -> Result<Vec<lattice_api::pgwire::Column>, lattice_api::session::QueryError> {
        Ok(output_one().columns)
    }
}
fn cancellable_server(max_connections: usize) -> (ServerHandle, std::sync::mpsc::Receiver<()>) {
    let (started, receiver) = std::sync::mpsc::channel();
    let server = ServerHandle::start_with_handler(
        ServerConfig {
            address: SocketAddr::from(([127, 0, 0, 1], 0)),
            max_connections,
            read_timeout: Duration::from_secs(30),
            write_timeout: Duration::from_secs(2),
            ..ServerConfig::default()
        },
        move || Cooperative {
            started: started.clone(),
        },
    )
    .unwrap();
    (server, receiver)
}
fn registered(stream: &mut TcpStream) -> (i32, i32) {
    stream.write_all(&startup()).unwrap();
    assert_eq!(message(stream), (b'R', vec![0; 4]));
    assert_eq!(message(stream).0, b'S');
    assert_eq!(message(stream).0, b'S');
    let (tag, fields) = message(stream);
    assert_eq!(tag, b'K');
    assert_eq!(fields.len(), 8);
    let pid = i32::from_be_bytes(fields[..4].try_into().unwrap());
    let secret = i32::from_be_bytes(fields[4..].try_into().unwrap());
    assert!(pid > 0);
    assert_eq!(message(stream), (b'Z', vec![b'I']));
    (pid, secret)
}
fn cancel(address: SocketAddr, key: (i32, i32)) {
    let mut connection = connect(address);
    let mut packet = 16i32.to_be_bytes().to_vec();
    packet.extend_from_slice(&80877102i32.to_be_bytes());
    packet.extend_from_slice(&key.0.to_be_bytes());
    packet.extend_from_slice(&key.1.to_be_bytes());
    connection.write_all(&packet).unwrap();
    closed(&mut connection); // Matching, unknown and stale credentials all close without a reply.
}
fn no_reply(stream: &mut TcpStream) {
    stream
        .set_read_timeout(Some(Duration::from_millis(50)))
        .unwrap();
    let mut byte = [0];
    let result = stream.read(&mut byte);
    assert!(
        matches!(result,Err(ref e) if matches!(e.kind(),std::io::ErrorKind::TimedOut|std::io::ErrorKind::WouldBlock)),
        "query unexpectedly returned: {result:?}"
    );
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
}
fn cancellation_error(stream: &mut TcpStream) {
    let (tag, error) = message(stream);
    assert_eq!(tag, b'E');
    assert!(error.windows(7).any(|field| field == b"C57014\0"));
}
fn select_one(stream: &mut TcpStream) {
    stream.write_all(&frontend(b'Q', b"SELECT 1\0")).unwrap();
    assert_eq!(message(stream), (b'T', column()));
    assert_eq!(message(stream), (b'D', data_row()));
    assert_eq!(message(stream), (b'C', b"SELECT 1\0".to_vec()));
    assert_eq!(message(stream), (b'Z', vec![b'I']));
}

#[test]
fn cancel_routes_at_sql_capacity_and_wrong_idle_or_previous_requests_do_not_poison_a_query() {
    let (server, started) = cancellable_server(1);
    let address = server.local_addr();
    let mut client = connect(address);
    let key = registered(&mut client);
    cancel(address, key); // Idle cancellation must not poison the next real query.
    select_one(&mut client);
    client.write_all(&frontend(b'Q', b"WAIT\0")).unwrap();
    started.recv_timeout(Duration::from_secs(3)).unwrap();
    cancel(address, (key.0, key.1 ^ 1));
    no_reply(&mut client);
    cancel(address, (-1, key.1));
    no_reply(&mut client);
    cancel(address, key);
    cancellation_error(&mut client);
    assert_eq!(message(&mut client), (b'Z', vec![b'I']));
    select_one(&mut client); // New token, same connection key.
    server.shutdown().unwrap();
}

#[test]
fn stale_credentials_cannot_cancel_a_later_connection_even_when_the_slot_is_reused() {
    let (server, started) = cancellable_server(1);
    let address = server.local_addr();
    let mut old = connect(address);
    let old_key = registered(&mut old);
    old.write_all(&frontend(b'X', b"")).unwrap();
    closed(&mut old);
    let mut client = connect(address);
    let current = registered(&mut client);
    assert_ne!(old_key.0, current.0);
    client.write_all(&frontend(b'Q', b"WAIT\0")).unwrap();
    started.recv_timeout(Duration::from_secs(3)).unwrap();
    cancel(address, old_key);
    no_reply(&mut client);
    cancel(address, current);
    cancellation_error(&mut client);
    assert_eq!(message(&mut client), (b'Z', vec![b'I']));
    server.shutdown().unwrap();
}

#[test]
fn cancelled_extended_execute_discards_until_sync_and_fresh_queries_work() {
    let (server, started) = cancellable_server(1);
    let address = server.local_addr();
    let mut client = connect(address);
    let key = registered(&mut client);
    client.write_all(&frontend(b'P', b"\0WAIT\0\0\0")).unwrap();
    assert_eq!(message(&mut client), (b'1', vec![]));
    client.write_all(&frontend(b'B', &[0; 8])).unwrap();
    assert_eq!(message(&mut client), (b'2', vec![]));
    client.write_all(&frontend(b'E', &[0; 5])).unwrap();
    started.recv_timeout(Duration::from_secs(3)).unwrap();
    cancel(address, key);
    cancellation_error(&mut client);
    no_reply(&mut client);
    let mut discarded = frontend(b'Q', b"SELECT 1\0");
    discarded.extend_from_slice(&frontend(b'S', b""));
    client.write_all(&discarded).unwrap();
    assert_eq!(message(&mut client), (b'Z', vec![b'I']));
    select_one(&mut client);
    server.shutdown().unwrap();
}

#[test]
fn shutdown_cancels_a_running_cooperative_handler_before_joining_workers() {
    let (server, started) = cancellable_server(1);
    let mut client = connect(server.local_addr());
    registered(&mut client);
    client.write_all(&frontend(b'Q', b"WAIT\0")).unwrap();
    started.recv_timeout(Duration::from_secs(3)).unwrap();
    let start = Instant::now();
    server.shutdown().unwrap();
    assert!(start.elapsed() < Duration::from_secs(3));
    let mut bytes = Vec::new();
    client.read_to_end(&mut bytes).unwrap_or_else(|error| {
        assert_eq!(error.kind(), std::io::ErrorKind::ConnectionReset);
        0
    });
}

#[test]
fn bounded_control_slots_cannot_make_shutdown_wait_for_slow_startup_clients() {
    let server = server(1, Duration::from_secs(30));
    let address = server.local_addr();
    let mut primary = connect(address);
    registered(&mut primary);
    let mut pending = Vec::new();
    for _ in 0..latticed::CONTROL_CONNECTION_RESERVE {
        let mut stream = connect(address);
        stream.write_all(&[0]).unwrap();
        pending.push(stream);
    }
    std::thread::sleep(Duration::from_millis(100));
    let mut excess = connect(address);
    closed(&mut excess);
    let start = Instant::now();
    server.shutdown().unwrap();
    assert!(start.elapsed() < Duration::from_secs(3));
    closed(&mut primary);
    for mut stream in pending {
        closed(&mut stream);
    }
}

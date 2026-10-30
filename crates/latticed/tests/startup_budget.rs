//! The startup budget bounds the connection until it is authenticated and then stops applying: an idle
//! but ready session may outlive it (the read timeout governs it from then on).
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
use std::time::Duration;

fn frame(tag: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = vec![tag];
    out.extend_from_slice(&((payload.len() + 4) as i32).to_be_bytes());
    out.extend_from_slice(payload);
    out
}

fn startup() -> Vec<u8> {
    let mut packet = vec![0, 0, 0, 0, 0, 3, 0, 0];
    packet.extend_from_slice(b"user\0budget-test\0database\0test\0\0");
    let len = (packet.len() as i32).to_be_bytes();
    packet[..4].copy_from_slice(&len);
    packet
}

fn next_tag(stream: &mut TcpStream) -> u8 {
    let mut tag = [0];
    stream.read_exact(&mut tag).unwrap();
    let mut length = [0; 4];
    stream.read_exact(&mut length).unwrap();
    let mut payload = vec![0; i32::from_be_bytes(length) as usize - 4];
    stream.read_exact(&mut payload).unwrap();
    tag[0]
}

fn until_ready(stream: &mut TcpStream) {
    while next_tag(stream) != b'Z' {}
}

fn connect(address: SocketAddr) -> TcpStream {
    let stream = TcpStream::connect_timeout(&address, Duration::from_secs(20)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    stream
}

#[test]
fn an_authenticated_idle_session_outlives_the_startup_budget() {
    let server = ServerHandle::start(ServerConfig {
        address: SocketAddr::from(([127, 0, 0, 1], 0)),
        startup_timeout: Duration::from_millis(400),
        read_timeout: Duration::from_secs(60),
        write_timeout: Duration::from_secs(2),
        ..ServerConfig::default()
    })
    .unwrap();
    let mut client = connect(server.local_addr());
    client.write_all(&startup()).unwrap();
    until_ready(&mut client);
    // Well past the startup budget, but inside the read timeout.
    std::thread::sleep(Duration::from_millis(1500));
    client.write_all(&frame(b'Q', b"SELECT 1\0")).unwrap();
    let mut tags = Vec::new();
    loop {
        let tag = next_tag(&mut client);
        tags.push(tag);
        if tag == b'Z' {
            break;
        }
    }
    assert_eq!(tags, vec![b'T', b'D', b'C', b'Z']);
    server.shutdown().unwrap();
}

#[test]
fn a_connection_that_never_finishes_startup_is_dropped_at_the_budget() {
    let server = ServerHandle::start(ServerConfig {
        address: SocketAddr::from(([127, 0, 0, 1], 0)),
        startup_timeout: Duration::from_millis(400),
        read_timeout: Duration::from_secs(60),
        write_timeout: Duration::from_secs(2),
        ..ServerConfig::default()
    })
    .unwrap();
    let mut client = connect(server.local_addr());
    client.write_all(&startup()[..6]).unwrap(); // a partial startup packet, then silence
    let mut byte = [0];
    match client.read(&mut byte) {
        Ok(0) => {}
        Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => {}
        other => panic!("expected the server to close the connection, got {other:?}"),
    }
    server.shutdown().unwrap();
}

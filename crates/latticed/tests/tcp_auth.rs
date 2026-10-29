//! Independent client framing and SCRAM proof calculation over certificate-verified TLS.
//! Certificates and private keys are generated in memory for tests; none are deployment credentials.
#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::disallowed_methods,
    clippy::disallowed_types
)]
use base64::{Engine as _, engine::general_purpose::STANDARD};
use hmac::{Hmac, KeyInit, Mac};
use lattice_api::auth::Accounts;
use lattice_seams::OsRng;
use latticed::{Authentication, ServerConfig, ServerHandle, TlsConfig};
use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned, pki_types::ServerName};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::{Duration, Instant};

type Client = StreamOwned<ClientConnection, TcpStream>;
fn fixture(budget: Duration) -> (ServerConfig, Arc<ClientConfig>) {
    let generated = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let tls = TlsConfig::from_pem(
        generated.cert.pem().as_bytes(),
        generated.signing_key.serialize_pem().as_bytes(),
    )
    .unwrap();
    let mut roots = RootCertStore::empty();
    roots.add(generated.cert.der().clone()).unwrap();
    let client =
        ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
    // Python stdlib hashlib/hmac independently provisions the published RFC7677 account.
    let credential = "SCRAM-SHA-256$4096:W22ZaJ0SNY7soEsUEjb6gQ==$WG5d8oPm3OtcPnkdi4Uo7BkeZkBFzpcXkuLmtbsT4qY=:wfPLwcE6nTWhTAmQ7tl2KeoiWGPlZqQxSrmfPwDl2dU=";
    let accounts = Accounts::parse(&format!("user\t{credential}"), &mut OsRng).unwrap();
    (
        ServerConfig {
            address: SocketAddr::from(([127, 0, 0, 1], 0)),
            startup_timeout: budget,
            authentication: Authentication::ScramTls {
                accounts: Arc::new(accounts),
                tls,
            },
            ..ServerConfig::default()
        },
        Arc::new(client),
    )
}
fn raw(address: SocketAddr) -> TcpStream {
    let socket = TcpStream::connect_timeout(&address, Duration::from_secs(2)).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    socket
        .set_write_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    socket.set_nodelay(true).unwrap();
    socket
}
fn tls(address: SocketAddr, config: Arc<ClientConfig>) -> Client {
    let mut socket = raw(address);
    socket.write_all(&[0, 0, 0, 8, 4, 210, 22, 47]).unwrap();
    let mut reply = [0];
    socket.read_exact(&mut reply).unwrap();
    assert_eq!(reply, *b"S");
    let mut connection =
        ClientConnection::new(config, ServerName::try_from("localhost").unwrap()).unwrap();
    connection.complete_io(&mut socket).unwrap();
    StreamOwned::new(connection, socket)
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
fn frame(tag: u8, body: &[u8]) -> Vec<u8> {
    let mut bytes = vec![tag];
    bytes.extend_from_slice(&((body.len() + 4) as u32).to_be_bytes());
    bytes.extend_from_slice(body);
    bytes
}
fn message(stream: &mut impl Read) -> (u8, Vec<u8>) {
    let mut header = [0; 5];
    stream.read_exact(&mut header).unwrap();
    let length = u32::from_be_bytes(header[1..5].try_into().unwrap()) as usize;
    assert!((4..=1048576).contains(&length));
    let mut body = vec![0; length - 4];
    stream.read_exact(&mut body).unwrap();
    (header[0], body)
}
fn mac(key: &[u8], data: &[u8]) -> [u8; 32] {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).unwrap();
    mac.update(data);
    mac.finalize().into_bytes().into()
}
fn begin(client: &mut Client, user: &str) -> String {
    client.write_all(&startup(user)).unwrap();
    client.flush().unwrap();
    assert_eq!(
        message(client),
        (b'R', b"\0\0\0\x0aSCRAM-SHA-256\0\0".to_vec())
    );
    let first = b"n,,n=,r=independent-client-nonce";
    let mut initial = b"SCRAM-SHA-256\0".to_vec();
    initial.extend_from_slice(&(first.len() as u32).to_be_bytes());
    initial.extend_from_slice(first);
    client.write_all(&frame(b'p', &initial)).unwrap();
    client.flush().unwrap();
    let (tag, payload) = message(client);
    assert_eq!(tag, b'R');
    assert_eq!(&payload[..4], &[0, 0, 0, 11]);
    String::from_utf8(payload[4..].to_vec()).unwrap()
}
fn proof(server_first: &str, password: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let attrs: Vec<_> = server_first.split(',').collect();
    let nonce = attrs[0].strip_prefix("r=").unwrap();
    assert!(nonce.starts_with("independent-client-nonce"));
    let salt = STANDARD
        .decode(attrs[1].strip_prefix("s=").unwrap())
        .unwrap();
    assert_eq!(salt.len(), 16);
    assert_eq!(attrs[2], "i=4096");
    let final_bare = format!("c=biws,r={nonce}");
    let auth = format!("n=,r=independent-client-nonce,{server_first},{final_bare}");
    let mut salted = [0; 32];
    pbkdf2::pbkdf2_hmac::<Sha256>(password, &salt, 4096, &mut salted);
    let client_key = mac(&salted, b"Client Key");
    let stored = Sha256::digest(client_key);
    let signature = mac(&stored, auth.as_bytes());
    let proof: Vec<_> = client_key
        .iter()
        .zip(signature)
        .map(|(a, b)| a ^ b)
        .collect();
    let signature = mac(&mac(&salted, b"Server Key"), auth.as_bytes());
    (
        format!("{final_bare},p={}", STANDARD.encode(proof)).into_bytes(),
        format!("v={}", STANDARD.encode(signature)).into_bytes(),
    )
}
fn finish(client: &mut Client, first: &str, password: &[u8]) -> Vec<u8> {
    let (proof, signature) = proof(first, password);
    client.write_all(&frame(b'p', &proof)).unwrap();
    client.flush().unwrap();
    signature
}
fn is_closed(stream: &mut impl Read) {
    let mut byte = [0];
    match stream.read(&mut byte) {
        Ok(0) => {}
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::UnexpectedEof
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionAborted
            ) => {}
        other => panic!("unexpected remaining data {other:?}"),
    }
}
#[test]
fn verified_tls_scram_completes_before_query_and_never_accepts_password_again() {
    let (config, client_config) = fixture(Duration::from_secs(3));
    let server = ServerHandle::start(config).unwrap();
    let mut client = tls(server.local_addr(), client_config);
    let first = begin(&mut client, "user");
    let signature = finish(&mut client, &first, b"pencil");
    let mut final_body = vec![0, 0, 0, 12];
    final_body.extend(signature);
    assert_eq!(message(&mut client), (b'R', final_body));
    assert_eq!(message(&mut client), (b'R', vec![0, 0, 0, 0]));
    assert_eq!(message(&mut client).0, b'S');
    assert_eq!(message(&mut client).0, b'S');
    let (tag, key) = message(&mut client);
    assert_eq!(tag, b'K');
    assert_eq!(key.len(), 8);
    assert_eq!(message(&mut client), (b'Z', vec![b'I']));
    client.write_all(&frame(b'Q', b"SELECT 1\0")).unwrap();
    client.flush().unwrap();
    assert_eq!(message(&mut client).0, b'T');
    assert_eq!(message(&mut client).0, b'D');
    assert_eq!(message(&mut client), (b'C', b"SELECT 1\0".to_vec()));
    assert_eq!(message(&mut client).0, b'Z');
    client.write_all(&frame(b'p', b"pencil\0")).unwrap();
    client.flush().unwrap();
    assert_eq!(message(&mut client).0, b'E');
    is_closed(&mut client);
    server.shutdown().unwrap();
}
#[test]
fn wrong_password_and_unknown_user_have_identical_terminal_failure() {
    let (config, client_config) = fixture(Duration::from_secs(3));
    let server = ServerHandle::start(config).unwrap();
    let mut failures = Vec::new();
    for user in ["user", "missing"] {
        let mut client = tls(server.local_addr(), client_config.clone());
        let first = begin(&mut client, user);
        finish(&mut client, &first, b"wrong-password");
        let (tag, body) = message(&mut client);
        assert_eq!(tag, b'E');
        assert!(body.windows(6).any(|v| v == b"28P01\0"));
        failures.push(body);
        is_closed(&mut client);
    }
    assert_eq!(failures[0], failures[1]);
    server.shutdown().unwrap();
}
#[test]
fn plaintext_startup_and_queries_before_proof_are_refused() {
    let (config, client_config) = fixture(Duration::from_secs(3));
    let server = ServerHandle::start(config).unwrap();
    let mut plaintext = raw(server.local_addr());
    plaintext.write_all(&startup("user")).unwrap();
    assert_eq!(message(&mut plaintext).0, b'E');
    is_closed(&mut plaintext);
    for tag in *b"QSp" {
        let mut client = tls(server.local_addr(), client_config.clone());
        client.write_all(&startup("user")).unwrap();
        client.flush().unwrap();
        assert_eq!(message(&mut client).0, b'R');
        client.write_all(&frame(tag, b"SELECT 1\0")).unwrap();
        client.flush().unwrap();
        assert_eq!(message(&mut client).0, b'E');
        is_closed(&mut client);
    }
    server.shutdown().unwrap();
}
#[test]
fn oversized_auth_header_is_rejected_without_waiting_for_payload() {
    let (config, client_config) = fixture(Duration::from_secs(3));
    let server = ServerHandle::start(config).unwrap();
    let mut client = tls(server.local_addr(), client_config);
    client.write_all(&startup("user")).unwrap();
    client.flush().unwrap();
    assert_eq!(message(&mut client).0, b'R');
    let mut header = vec![b'p'];
    header.extend_from_slice(&8197u32.to_be_bytes());
    client.write_all(&header).unwrap();
    client.flush().unwrap();
    assert_eq!(message(&mut client).0, b'E');
    is_closed(&mut client);
    server.shutdown().unwrap();
}
#[test]
fn one_startup_budget_includes_negotiation_and_partial_tls_and_shutdown_releases_workers() {
    let (config, _) = fixture(Duration::from_millis(250));
    let server = ServerHandle::start(config).unwrap();
    let mut client = raw(server.local_addr());
    let started = Instant::now();
    client.write_all(&[0, 0, 0, 8, 4, 210, 22, 47]).unwrap();
    let mut reply = [0];
    client.read_exact(&mut reply).unwrap();
    assert_eq!(reply, *b"S");
    // No TLS bytes: the same accept-to-auth budget, not read_timeout=30s, must release it.
    is_closed(&mut client);
    assert!(started.elapsed() < Duration::from_secs(2));
    let mut pending = raw(server.local_addr());
    pending.write_all(&[0, 0, 0, 8, 4, 210, 22, 47]).unwrap();
    pending.read_exact(&mut reply).unwrap();
    let started = Instant::now();
    server.shutdown().unwrap();
    assert!(started.elapsed() < Duration::from_secs(1));
    is_closed(&mut pending);
}
#[test]
fn scram_has_one_absolute_budget_and_shutdown_handles_authenticating_clients() {
    let (config, client_config) = fixture(Duration::from_millis(300));
    let server = ServerHandle::start(config).unwrap();
    let mut client = tls(server.local_addr(), client_config.clone());
    let _first = begin(&mut client, "user");
    let started = Instant::now();
    is_closed(&mut client);
    assert!(started.elapsed() < Duration::from_secs(2));
    let mut client = tls(server.local_addr(), client_config);
    begin(&mut client, "user");
    server.shutdown().unwrap();
    is_closed(&mut client);
}
#[test]
fn development_trust_cannot_bind_remotely_and_invalid_tls_config_fails_before_bind() {
    assert!(
        ServerHandle::start(ServerConfig {
            address: SocketAddr::from(([0, 0, 0, 0], 0)),
            ..ServerConfig::default()
        })
        .is_err()
    );
    assert!(TlsConfig::from_pem(b"not a certificate", b"not a key").is_err());
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let other = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    assert!(
        TlsConfig::from_pem(
            cert.cert.pem().as_bytes(),
            other.signing_key.serialize_pem().as_bytes()
        )
        .is_err()
    );
}

#[test]
fn client_rejects_untrusted_certificate_and_wrong_server_name() {
    let (config, client_config) = fixture(Duration::from_secs(3));
    let server = ServerHandle::start(config).unwrap();
    let untrusted =
        ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(RootCertStore::empty())
            .with_no_client_auth();
    for (config, name) in [
        (Arc::new(untrusted), "localhost"),
        (client_config, "different.example"),
    ] {
        let mut socket = raw(server.local_addr());
        socket.write_all(&[0, 0, 0, 8, 4, 210, 22, 47]).unwrap();
        let mut reply = [0];
        socket.read_exact(&mut reply).unwrap();
        assert_eq!(reply, *b"S");
        let mut connection =
            ClientConnection::new(config, ServerName::try_from(name).unwrap()).unwrap();
        assert!(connection.complete_io(&mut socket).is_err());
    }
    server.shutdown().unwrap();
}

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
use lattice_api::auth::{Accounts, DecoySecret};
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
    let accounts = Accounts::parse(
        &format!("user\t{credential}\nanalyst\t{credential}"),
        DecoySecret::from_bytes(&[7; 32]).unwrap(),
    )
    .unwrap();
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
/// Startup budget of every test that is not about the budget. Client sockets give up sooner
/// (`CLIENT_WAIT`), so a server that wrongly waits out its budget fails the test, while a loaded
/// runner that is merely slow does not.
const GENEROUS_BUDGET: Duration = Duration::from_secs(30);
const CLIENT_WAIT: Duration = Duration::from_secs(20);
/// Ceiling for "released at once" assertions: far below the 30-second read timeout the server would
/// otherwise wait out, far above any scheduling delay.
const NOT_THE_READ_TIMEOUT: Duration = Duration::from_secs(20);

fn raw(address: SocketAddr) -> TcpStream {
    let socket = TcpStream::connect_timeout(&address, CLIENT_WAIT).unwrap();
    socket.set_read_timeout(Some(CLIENT_WAIT)).unwrap();
    socket.set_write_timeout(Some(CLIENT_WAIT)).unwrap();
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
    let (config, client_config) = fixture(GENEROUS_BUDGET);
    let server = ServerHandle::start(config).unwrap();
    let mut client = tls(server.local_addr(), client_config);
    let first = begin(&mut client, "user");
    let signature = finish(&mut client, &first, b"pencil");
    let mut final_body = vec![0, 0, 0, 12];
    final_body.extend(signature);
    assert_eq!(message(&mut client), (b'R', final_body));
    assert_eq!(message(&mut client), (b'R', vec![0, 0, 0, 0]));
    // server_version, client_encoding, TimeZone, DateStyle, server_encoding, integer_datetimes and
    // standard_conforming_strings.
    for _ in 0..7 {
        assert_eq!(message(&mut client).0, b'S');
    }
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
    let (config, client_config) = fixture(GENEROUS_BUDGET);
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
    let (config, client_config) = fixture(GENEROUS_BUDGET);
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
    let (config, client_config) = fixture(GENEROUS_BUDGET);
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
    assert!(started.elapsed() < NOT_THE_READ_TIMEOUT);
    let mut pending = raw(server.local_addr());
    pending.write_all(&[0, 0, 0, 8, 4, 210, 22, 47]).unwrap();
    pending.read_exact(&mut reply).unwrap();
    let started = Instant::now();
    server.shutdown().unwrap();
    assert!(started.elapsed() < NOT_THE_READ_TIMEOUT);
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
    assert!(started.elapsed() < NOT_THE_READ_TIMEOUT);
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
    let (config, client_config) = fixture(GENEROUS_BUDGET);
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

#[test]
fn cli_requires_persistent_decoy_secret_and_refuses_it_in_trust_mode() {
    let binary = env!("CARGO_BIN_EXE_latticed");
    let missing = std::process::Command::new(binary)
        .args([
            "--auth",
            "scram-tls",
            "--tls-cert",
            "unused",
            "--tls-key",
            "unused",
            "--scram-verifiers",
            "unused",
        ])
        .output()
        .unwrap();
    assert!(!missing.status.success());
    assert!(
        String::from_utf8(missing.stderr)
            .unwrap()
            .contains("--scram-decoy-secret required")
    );
    let trust = std::process::Command::new(binary)
        .args([
            "--auth",
            "development-trust",
            "--scram-decoy-secret",
            "unused",
        ])
        .output()
        .unwrap();
    assert!(!trust.status.success());
    assert!(
        String::from_utf8(trust.stderr)
            .unwrap()
            .contains("require --auth scram-tls")
    );
    let help = std::process::Command::new(binary)
        .arg("--help")
        .output()
        .unwrap();
    assert!(help.status.success());
    assert!(
        String::from_utf8(help.stdout)
            .unwrap()
            .contains("--scram-decoy-secret FILE")
    );
}

#[test]
fn registry_full_after_scram_success_reaches_the_client_as_53300() {
    let (mut config, client_config) = fixture(GENEROUS_BUDGET);
    config.max_connections = 1;
    let server = ServerHandle::start(config).unwrap();
    let mut first = tls(server.local_addr(), client_config.clone());
    let proof_first = begin(&mut first, "user");
    let signature = finish(&mut first, &proof_first, b"pencil");
    let mut final_body = vec![0, 0, 0, 12];
    final_body.extend(signature);
    assert_eq!(message(&mut first), (b'R', final_body));
    assert_eq!(message(&mut first), (b'R', vec![0, 0, 0, 0]));
    // The first session holds the only registry slot; a second correct login is refused after SCRAM.
    let mut second = tls(server.local_addr(), client_config);
    let proof_second = begin(&mut second, "user");
    finish(&mut second, &proof_second, b"pencil");
    let (tag, error) = message(&mut second);
    assert_eq!(tag, b'E');
    assert!(error.windows(7).any(|bytes| bytes == b"C53300\0"));
    is_closed(&mut second);
    server.shutdown().unwrap();
}

/// Records which principal reached the handler for every call. Shared with the test thread.
#[derive(Clone, Default)]
struct Log(Arc<std::sync::Mutex<Vec<String>>>);
impl Log {
    fn entries(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }
    fn push(&self, entry: String) {
        self.0.lock().unwrap().push(entry);
    }
}
struct Recorder(Log);
impl lattice_api::session::QueryHandler for Recorder {
    fn execute(
        &mut self,
        ctx: &lattice_api::principal::QueryContext<'_>,
        sql: &str,
        _: &[Option<Vec<u8>>],
    ) -> Result<lattice_api::session::QueryOutput, lattice_api::session::QueryError> {
        self.0.push(format!("execute {sql} as {}", ctx.principal));
        Ok(lattice_api::session::QueryOutput {
            tag: "OK".into(),
            ..Default::default()
        })
    }
    fn describe(
        &mut self,
        ctx: &lattice_api::principal::QueryContext<'_>,
        sql: &str,
    ) -> Result<Vec<lattice_api::pgwire::Column>, lattice_api::session::QueryError> {
        self.0.push(format!("describe {sql} as {}", ctx.principal));
        Ok(Vec::new())
    }
}
fn recording_server(config: ServerConfig) -> (ServerHandle, Log) {
    let log = Log::default();
    let handler_log = log.clone();
    let server =
        ServerHandle::start_with_handler(config, move || Recorder(handler_log.clone())).unwrap();
    (server, log)
}
/// Full SCRAM login; returns the client positioned after the first `ReadyForQuery`.
fn login(address: SocketAddr, config: &Arc<ClientConfig>, user: &str) -> Client {
    let mut client = tls(address, config.clone());
    let first = begin(&mut client, user);
    finish(&mut client, &first, b"pencil");
    assert_eq!(message(&mut client).0, b'R'); // SASL final
    assert_eq!(message(&mut client), (b'R', vec![0, 0, 0, 0]));
    loop {
        if message(&mut client).0 == b'Z' {
            return client;
        }
    }
}
fn query(client: &mut Client, sql: &str) {
    client
        .write_all(&frame(b'Q', format!("{sql}\0").as_bytes()))
        .unwrap();
    client.flush().unwrap();
    assert_eq!(message(client).0, b'C');
    assert_eq!(message(client), (b'Z', vec![b'I']));
}
fn parse(client: &mut Client, sql: &str) {
    let mut body = b"\0".to_vec();
    body.extend_from_slice(sql.as_bytes());
    body.extend_from_slice(&[0, 0, 0]);
    client.write_all(&frame(b'P', &body)).unwrap();
    client.write_all(&frame(b'S', b"")).unwrap();
    client.flush().unwrap();
    assert_eq!(message(client).0, b'1');
    assert_eq!(message(client), (b'Z', vec![b'I']));
}

#[test]
fn handler_receives_exactly_the_account_that_completed_scram_over_tls() {
    let (config, client_config) = fixture(GENEROUS_BUDGET);
    let (server, log) = recording_server(config);
    let address = server.local_addr();
    let mut analyst = login(address, &client_config, "analyst");
    let mut user = login(address, &client_config, "user");
    query(&mut analyst, "Q1");
    parse(&mut user, "P1");
    query(&mut user, "Q2");
    parse(&mut analyst, "P2");
    // Each connection keeps the account it proved, whichever order the statements interleave.
    let mut entries = log.entries();
    entries.sort();
    assert_eq!(
        entries,
        [
            "describe P1 as account:user",
            "describe P2 as account:analyst",
            "execute Q1 as account:analyst",
            "execute Q2 as account:user",
        ]
    );
    server.shutdown().unwrap();
}

#[test]
fn nothing_reaches_the_handler_before_a_verified_proof() {
    let (config, client_config) = fixture(GENEROUS_BUDGET);
    let (server, log) = recording_server(config);
    let address = server.local_addr();
    // Queries, parses and a password message sent in place of the proof.
    for tag in *b"QPp" {
        let mut client = tls(address, client_config.clone());
        client.write_all(&startup("user")).unwrap();
        client.flush().unwrap();
        assert_eq!(message(&mut client).0, b'R');
        client.write_all(&frame(tag, b"SELECT 1\0\0\0\0")).unwrap();
        client.flush().unwrap();
        assert_eq!(message(&mut client).0, b'E');
        is_closed(&mut client);
    }
    // A wrong password for a real account, then a query on the same socket.
    let mut client = tls(address, client_config.clone());
    let first = begin(&mut client, "analyst");
    finish(&mut client, &first, b"wrong-password");
    assert_eq!(message(&mut client).0, b'E');
    let _ = client.write_all(&frame(b'Q', b"SELECT 1\0"));
    let _ = client.flush();
    is_closed(&mut client);
    // An unknown account.
    let mut client = tls(address, client_config.clone());
    let first = begin(&mut client, "missing");
    finish(&mut client, &first, b"pencil");
    assert_eq!(message(&mut client).0, b'E');
    is_closed(&mut client);
    assert_eq!(log.entries(), Vec::<String>::new());
    // The listener still serves a correct login afterwards.
    let mut ok = login(address, &client_config, "user");
    query(&mut ok, "AFTER");
    assert_eq!(log.entries(), ["execute AFTER as account:user"]);
    server.shutdown().unwrap();
}

#[test]
fn a_second_startup_user_cannot_select_a_different_account() {
    let (config, client_config) = fixture(GENEROUS_BUDGET);
    let (server, log) = recording_server(config);
    for pair in [("user", "analyst"), ("analyst", "user"), ("user", "user")] {
        let mut client = tls(server.local_addr(), client_config.clone());
        let mut bytes = vec![0, 0, 0, 0, 0, 3, 0, 0];
        for name in [pair.0, pair.1] {
            bytes.extend_from_slice(b"user\0");
            bytes.extend_from_slice(name.as_bytes());
            bytes.push(0);
        }
        bytes.push(0);
        let length = (bytes.len() as u32).to_be_bytes();
        bytes[..4].copy_from_slice(&length);
        client.write_all(&bytes).unwrap();
        client.flush().unwrap();
        let (tag, body) = message(&mut client);
        assert_eq!(tag, b'E');
        assert!(body.windows(6).any(|v| v == b"28P01\0"));
        is_closed(&mut client);
    }
    assert!(log.entries().is_empty());
    server.shutdown().unwrap();
}

#[test]
fn tls_session_ends_with_close_notify_not_a_truncation() {
    let (config, client_config) = fixture(GENEROUS_BUDGET);
    let server = ServerHandle::start(config).unwrap();
    let mut client = tls(server.local_addr(), client_config);
    let first = begin(&mut client, "user");
    finish(&mut client, &first, b"pencil");
    loop {
        if message(&mut client).0 == b'Z' {
            break;
        }
    }
    client.write_all(&frame(b'X', b"")).unwrap();
    client.flush().unwrap();
    // rustls reports a missing close_notify as UnexpectedEof; an orderly TLS close reads as Ok(0).
    let mut byte = [0];
    assert_eq!(client.read(&mut byte).unwrap(), 0);
    server.shutdown().unwrap();
}

#[cfg(unix)]
#[test]
fn cli_refuses_group_or_world_accessible_key_and_decoy_files() {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("latticed-cli-perm-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let write = |name: &str, bytes: &[u8], mode: u32| {
        let path = dir.join(name);
        std::fs::write(&path, bytes).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        path.to_str().unwrap().to_owned()
    };
    let cert = write("cert.pem", b"unused", 0o644);
    let verifiers = write("accounts", b"unused", 0o644);
    let run = |decoy: &str, key: &str| {
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_latticed"))
            .args([
                "--auth",
                "scram-tls",
                "--tls-cert",
                &cert,
                "--tls-key",
                key,
                "--scram-verifiers",
                &verifiers,
                "--scram-decoy-secret",
                decoy,
            ])
            .output()
            .unwrap();
        assert!(!out.status.success());
        String::from_utf8(out.stderr).unwrap()
    };
    let decoy_open = write("decoy-open", &[7; 32], 0o644);
    let decoy_ok = write("decoy-ok", &[7; 32], 0o600);
    let key_open = write("key-open", b"unused", 0o640);
    let key_ok = write("key-ok", b"unused", 0o600);
    assert!(run(&decoy_open, &key_ok).contains("group or others"));
    let error = run(&decoy_ok, &key_open);
    assert!(
        error.contains("group or others") && error.contains("key-open"),
        "{error}"
    );
    // With private files the permission check passes and a later check (bad PEM) fails instead.
    assert!(!run(&decoy_ok, &key_ok).contains("group or others"));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn fatal_error_reply_is_followed_by_close_notify() {
    let (config, client_config) = fixture(GENEROUS_BUDGET);
    let server = ServerHandle::start(config).unwrap();
    let mut client = tls(server.local_addr(), client_config);
    // A wrong password ends in a FATAL reply, not in an orderly Terminate.
    let first = begin(&mut client, "user");
    finish(&mut client, &first, b"wrong");
    let (tag, body) = message(&mut client);
    assert_eq!(tag, b'E');
    assert!(body.windows(7).any(|field| field == b"C28P01\0"));
    let mut byte = [0];
    assert_eq!(client.read(&mut byte).unwrap(), 0);
    server.shutdown().unwrap();
}

#[test]
fn startup_timeout_over_tls_still_ends_with_close_notify() {
    let (config, client_config) = fixture(Duration::from_millis(500));
    let server = ServerHandle::start(config).unwrap();
    let mut client = tls(server.local_addr(), client_config);
    // Say nothing: the startup budget expires while the server waits for the startup packet.
    let mut byte = [0];
    assert_eq!(client.read(&mut byte).unwrap(), 0);
    server.shutdown().unwrap();
}

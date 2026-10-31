//! Listener configuration validation: each bound on its own, and the handle's running state.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::disallowed_methods,
    clippy::disallowed_types
)]

use latticed::{ServerConfig, ServerHandle};
use std::io::ErrorKind;
use std::net::SocketAddr;
use std::time::Duration;

fn base() -> ServerConfig {
    ServerConfig {
        address: SocketAddr::from(([127, 0, 0, 1], 0)),
        ..ServerConfig::default()
    }
}

fn start(config: ServerConfig) -> std::io::Result<ServerHandle> {
    ServerHandle::start(config)
}

fn refused(config: ServerConfig, what: &str) {
    match start(config) {
        Err(e) => assert_eq!(e.kind(), ErrorKind::InvalidInput, "{what}: {e}"),
        Ok(_) => panic!("{what} was accepted"),
    }
}

#[test]
fn the_connection_limit_is_one_to_one_thousand_twenty_four() {
    for ok in [1usize, 1024] {
        let handle = start(ServerConfig {
            max_connections: ok,
            ..base()
        })
        .unwrap();
        handle.shutdown().unwrap();
    }
    refused(
        ServerConfig {
            max_connections: 0,
            ..base()
        },
        "0 connections",
    );
    refused(
        ServerConfig {
            max_connections: 1025,
            ..base()
        },
        "1025 connections",
    );
}

#[test]
fn every_timeout_must_be_nonzero_on_its_own() {
    let zero = Duration::ZERO;
    refused(
        ServerConfig {
            read_timeout: zero,
            ..base()
        },
        "zero read timeout",
    );
    refused(
        ServerConfig {
            write_timeout: zero,
            ..base()
        },
        "zero write timeout",
    );
    refused(
        ServerConfig {
            response_timeout: zero,
            ..base()
        },
        "zero response timeout",
    );
    refused(
        ServerConfig {
            startup_timeout: zero,
            ..base()
        },
        "zero startup timeout",
    );
    refused(
        ServerConfig {
            idle_timeout: Some(zero),
            ..base()
        },
        "zero idle timeout",
    );
    let ok = start(ServerConfig {
        idle_timeout: Some(Duration::from_secs(1)),
        ..base()
    })
    .unwrap();
    ok.shutdown().unwrap();
}

#[test]
fn a_timeout_too_large_to_add_to_the_clock_is_refused() {
    let huge = Duration::MAX;
    refused(
        ServerConfig {
            read_timeout: huge,
            ..base()
        },
        "huge read timeout",
    );
    refused(
        ServerConfig {
            response_timeout: huge,
            ..base()
        },
        "huge response timeout",
    );
    refused(
        ServerConfig {
            startup_timeout: huge,
            ..base()
        },
        "huge startup timeout",
    );
    refused(
        ServerConfig {
            idle_timeout: Some(huge),
            ..base()
        },
        "huge idle timeout",
    );
}

#[test]
fn development_trust_refuses_a_non_loopback_address() {
    refused(
        ServerConfig {
            address: SocketAddr::from(([0, 0, 0, 0], 0)),
            ..ServerConfig::default()
        },
        "development trust on all interfaces",
    );
}

#[test]
fn a_running_handle_is_not_finished_and_reports_its_port() {
    let handle = start(base()).unwrap();
    assert!(!handle.is_finished());
    assert_ne!(handle.local_addr().port(), 0);
    assert_eq!(handle.open_connections(), 0);
    handle.shutdown().unwrap();
}

#[test]
fn a_stopped_listener_no_longer_accepts_connections() {
    let handle = start(base()).unwrap();
    let address = handle.local_addr();
    std::net::TcpStream::connect(address).unwrap();
    handle.shutdown().unwrap();
    assert!(std::net::TcpStream::connect(address).is_err());
}

/// Every startup secret of the binary goes through the one private-file rule, so a SCRAM verifier
/// file that others can read, or a secret in a directory others can write, stops startup before a
/// listener is bound. The expected messages are written out here, not taken from the reader.
#[cfg(unix)]
mod startup_secret_files {
    use rcgen::generate_simple_self_signed;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};

    fn scratch(name: &str, mode: u32) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("latticed-startup-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(mode)).unwrap();
        dir
    }

    fn write(path: &Path, bytes: &[u8], mode: u32) -> String {
        std::fs::write(path, bytes).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
        path.to_string_lossy().into_owned()
    }

    /// The server certificate is public, so only its key file is written owner-only.
    fn tls_files(dir: &Path) -> (String, String) {
        let generated = generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let cert = write(
            &dir.join("cert.pem"),
            generated.cert.pem().as_bytes(),
            0o644,
        );
        let key = write(
            &dir.join("key.pem"),
            generated.signing_key.serialize_pem().as_bytes(),
            0o600,
        );
        (cert, key)
    }

    fn scram_args<'a>(
        cert: &'a str,
        key: &'a str,
        verifiers: &'a str,
        decoy: &'a str,
    ) -> Vec<&'a str> {
        vec![
            "--auth",
            "scram-tls",
            "--tls-cert",
            cert,
            "--tls-key",
            key,
            "--scram-verifiers",
            verifiers,
            "--scram-decoy-secret",
            decoy,
        ]
    }

    /// Runs the binary with `args`, which must be refused at startup; returns its stderr.
    fn refused(args: &[&str]) -> String {
        let output = Command::new(env!("CARGO_BIN_EXE_latticed"))
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .output()
            .unwrap();
        assert!(!output.status.success(), "{args:?} started");
        String::from_utf8(output.stderr).unwrap()
    }

    #[test]
    fn a_verifier_file_that_others_can_read_stops_startup() {
        let dir = scratch("verifiers", 0o700);
        let (cert, key) = tls_files(&dir);
        let decoy = write(&dir.join("decoy"), &[7u8; 32], 0o600);
        let verifiers = write(&dir.join("accounts"), b"unused", 0o644);
        let refused_with = |verifiers: &str| refused(&scram_args(&cert, &key, verifiers, &decoy));
        assert_eq!(
            refused_with(verifiers.as_str()),
            format!(
                "latticed: {verifiers}: secret file is accessible to group or others (use mode 0600)\n"
            )
        );
        // The same file, owner-only, passes the check; the reader then rejects its (unused) text.
        let verifiers = write(&dir.join("accounts"), b"unused", 0o600);
        assert_eq!(
            refused_with(verifiers.as_str()),
            "latticed: invalid credential configuration\n"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_secret_in_a_directory_that_others_can_write_stops_startup() {
        let dir = scratch("writable-dir", 0o777);
        let decoy = write(&dir.join("decoy"), &[7u8; 32], 0o600);
        assert_eq!(
            refused(&["--auth", "scram-tls", "--scram-decoy-secret", &decoy]),
            format!(
                "latticed: {decoy}: credential directory is writable by group or others (use mode 0700)\n"
            )
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_symlinked_secret_is_refused_before_its_target_is_read() {
        let dir = scratch("symlink", 0o700);
        let target = write(&dir.join("decoy"), &[7u8; 32], 0o600);
        let link = dir.join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let link = link.to_string_lossy().into_owned();
        let stderr = refused(&["--auth", "scram-tls", "--scram-decoy-secret", &link]);
        assert!(
            stderr.starts_with(&format!("latticed: {link}: ")),
            "{stderr}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

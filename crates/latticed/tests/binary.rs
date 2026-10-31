//! The `latticed` program through its real binary: option parsing, refusals, security-file bounds and
//! the stdin shutdown channel, which scripts and supervisors depend on.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::disallowed_methods,
    clippy::disallowed_types
)]

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

fn latticed(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_latticed"))
        .args(args)
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn refused(args: &[&str], message: &str) {
    let o = latticed(args);
    assert!(!o.status.success(), "{args:?} should fail");
    assert_eq!(stderr(&o), format!("latticed: {message}\n"), "{args:?}");
}

/// Writes a file with the given unix mode. Key and decoy files must be private (0600). The file
/// lives in a per-process directory with mode 0700: a directory that others can write to is refused
/// as a secret location, and the system temp directory (`/tmp` on Linux) is one.
fn file_with_mode(name: &str, bytes: &[u8], mode: u32) -> String {
    use std::os::unix::fs::PermissionsExt;
    let dir: PathBuf =
        std::env::temp_dir().join(format!("latticed-bin-secrets-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = dir.join(name);
    let _ = std::fs::remove_file(&path);
    std::fs::write(&path, bytes).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
    path.to_string_lossy().into_owned()
}

fn file(name: &str, bytes: &[u8]) -> String {
    file_with_mode(name, bytes, 0o600)
}

#[test]
fn version_and_help_print_and_exit_cleanly() {
    for flag in ["--version", "-V"] {
        let o = latticed(&[flag]);
        assert!(o.status.success());
        assert_eq!(
            stdout(&o),
            format!("latticed {}\n", env!("CARGO_PKG_VERSION"))
        );
    }
    for flag in ["--help", "-h"] {
        let o = latticed(&[flag]);
        assert!(o.status.success());
        assert!(
            stdout(&o).starts_with("latticed [--listen IP:PORT]"),
            "{}",
            stdout(&o)
        );
        assert!(stdout(&o).contains("--gateway-allowed-host NAME[:PORT]"));
    }
}

#[test]
fn gateway_allowed_hosts_need_the_gateway_and_well_formed_names() {
    refused(
        &["--gateway-allowed-host", "gateway.example.test"],
        "--gateway-allowed-host requires --gateway-listen",
    );
    refused(
        &gateway_args(&["--gateway-allowed-host", "a b"]),
        "--gateway-allowed-host \"a b\" is not a valid host: it has a space or a control character",
    );
    refused(
        &gateway_args(&["--gateway-allowed-host", "*.example.test"]),
        "--gateway-allowed-host \"*.example.test\" is not a valid host: a label has a character other than a letter, digit or hyphen (no wildcards)",
    );
    refused(
        &gateway_args(&["--gateway-allowed-host", "10.0.0.0/8"]),
        "--gateway-allowed-host \"10.0.0.0/8\" is not a valid host: it has a path, query or fragment; give the host name and an optional port only",
    );
    refused(
        &gateway_args(&["--gateway-allowed-host", "example.test:0"]),
        "--gateway-allowed-host \"example.test:0\" is not a valid host: the port is not in 1..=65535",
    );
    // The bind is 127.0.0.1:0, so the bound port in the defaults and in the entry is 0.
    refused(
        &gateway_args(&[
            "--gateway-allowed-host",
            "example.test",
            "--gateway-allowed-host",
            "EXAMPLE.test",
        ]),
        "--gateway-allowed-host example.test:0 is listed twice",
    );
    let seventeen: Vec<String> = (0..17).map(|i| format!("h{i}.test")).collect();
    let mut extra: Vec<&str> = Vec::new();
    for host in &seventeen {
        extra.push("--gateway-allowed-host");
        extra.push(host);
    }
    refused(
        &gateway_args(&extra),
        "at most 16 --gateway-allowed-host values are accepted, got 17",
    );
}

/// The arguments of a gateway that would otherwise start, with `extra` appended. The data directory is
/// never opened: these tests stop at the argument checks.
fn gateway_args<'a>(extra: &[&'a str]) -> Vec<&'a str> {
    let mut args = vec![
        "--data-dir",
        "/nonexistent-latticed-allowed-hosts",
        "--development-policy",
        "deny-all",
        "--gateway-listen",
        "127.0.0.1:0",
    ];
    args.extend_from_slice(extra);
    args
}

#[test]
fn every_option_rejects_a_bad_value_with_its_own_message() {
    refused(&["--bogus", "1"], "unknown option --bogus");
    refused(&["--listen"], "missing value for --listen");
    refused(
        &["--listen", "nowhere"],
        "invalid listen address: invalid socket address syntax",
    );
    for (option, message) in [
        (
            "--max-connections",
            "invalid connection limit: invalid digit found in string",
        ),
        (
            "--read-timeout-ms",
            "invalid read timeout: invalid digit found in string",
        ),
        (
            "--write-timeout-ms",
            "invalid write timeout: invalid digit found in string",
        ),
    ] {
        refused(&[option, "x"], message);
    }
    refused(&["--startup-timeout-ms", "x"], "invalid startup timeout");
    refused(
        &["--auth", "magic"],
        "--auth must be development-trust or scram-tls",
    );
}

#[test]
fn tls_and_credential_options_need_scram_mode_one_at_a_time() {
    for option in [
        "--tls-cert",
        "--tls-key",
        "--scram-verifiers",
        "--scram-decoy-secret",
    ] {
        refused(
            &["--listen", "127.0.0.1:0", option, "x"],
            "TLS/credential options require --auth scram-tls",
        );
    }
}

#[test]
fn security_files_are_read_within_exact_bounds() {
    let exact = file("exact", &[7u8; 32]);
    let short = file("short", &[7u8; 31]);
    let long = file("long", &[7u8; 33]);
    let scram = |decoy: &str| latticed(&["--auth", "scram-tls", "--scram-decoy-secret", decoy]);
    // 32 bytes pass the bound and the secret check; the next missing option is then reported.
    assert_eq!(stderr(&scram(&exact)), "latticed: --tls-cert required\n");
    assert_eq!(
        stderr(&scram(&short)),
        "latticed: decoy secret must contain exactly 32 binary bytes\n"
    );
    assert_eq!(
        stderr(&scram(&long)),
        "latticed: configured security file exceeds bounds\n"
    );
    assert_eq!(
        stderr(&scram("/nonexistent/latticed-decoy")),
        "latticed: cannot open configured security file\n"
    );
    refused(&["--auth", "scram-tls"], "--scram-decoy-secret required");
    // The same valid bytes in a group- or world-accessible file are refused before they are read.
    for (name, mode) in [("open644", 0o644), ("open640", 0o640), ("open604", 0o604)] {
        let open = file_with_mode(name, &[7u8; 32], mode);
        assert_eq!(
            stderr(&scram(&open)),
            format!(
                "latticed: {open}: secret file is accessible to group or others (use mode 0600)\n"
            ),
            "{mode:o}"
        );
    }
}

#[test]
fn the_server_ignores_other_stdin_lines_and_stops_on_shutdown_without_end_of_file() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_latticed"))
        .args(["--listen", "127.0.0.1:0"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut banner = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut banner)
        .unwrap();
    assert!(
        banner.starts_with("latticed listening on 127.0.0.1:")
            && banner.contains("(development-trust; SELECT 1 only)"),
        "{banner}"
    );
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(b"noise\n  \nshutdown please\n").unwrap();
    stdin.flush().unwrap();
    // The supervisor loop wakes every half second; after more than one wake-up it is still serving.
    std::thread::sleep(Duration::from_millis(1200));
    assert!(
        child.try_wait().unwrap().is_none(),
        "stopped on a line that is not shutdown"
    );
    stdin.write_all(b"  shutdown  \n").unwrap();
    stdin.flush().unwrap();
    let until = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        assert!(
            Instant::now() < until,
            "did not stop after shutdown with stdin still open"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(status.success());
    drop(stdin);
}

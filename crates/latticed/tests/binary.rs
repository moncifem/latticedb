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

fn file(name: &str, bytes: &[u8]) -> String {
    let path: PathBuf =
        std::env::temp_dir().join(format!("latticed-bin-{name}-{}", std::process::id()));
    std::fs::write(&path, bytes).unwrap();
    path.to_string_lossy().into_owned()
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
    }
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

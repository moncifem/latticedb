//! Composition root: the bounded development PostgreSQL TCP listener.
#![forbid(unsafe_code)]
#![allow(
    clippy::disallowed_methods,
    clippy::disallowed_types,
    clippy::print_stdout,
    clippy::print_stderr
)]

use lattice_api::auth::{Accounts, DecoySecret, MAX_CREDENTIAL_BYTES};
use latticed::{Authentication, ServerConfig, ServerHandle, TlsConfig};
use std::io::Read;
use std::io::{self, BufRead};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;
use zeroize::Zeroizing;

fn run() -> Result<(), String> {
    let mut config = ServerConfig::default();
    let mut auth_mode = String::from("development-trust");
    let mut certificates = None;
    let mut private_key = None;
    let mut credentials = None;
    let mut decoy_secret = None;
    let mut args = std::env::args().skip(1);
    while let Some(option) = args.next() {
        if option == "--version" || option == "-V" {
            println!("latticed {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        if option == "--help" || option == "-h" {
            println!(
                "latticed [--listen IP:PORT] [--max-connections N] [--read-timeout-ms N] [--write-timeout-ms N]\nLoopback development trust by default; SELECT 1 only. SCRAM requires --auth scram-tls --tls-cert FILE --tls-key FILE --scram-verifiers FILE --scram-decoy-secret FILE. Optional --startup-timeout-ms N bounds startup/TLS/authentication. Send 'shutdown' on stdin for a clean stop; end-of-file on stdin also stops the server, so run it with stdin held open or under a supervisor that keeps a pipe open. The read timeout also disconnects idle sessions."
            );
            return Ok(());
        }
        let value = args
            .next()
            .ok_or_else(|| format!("missing value for {option}"))?;
        match option.as_str() {
            "--auth" => auth_mode = value,
            "--tls-cert" => certificates = Some(value),
            "--tls-key" => private_key = Some(value),
            "--scram-verifiers" => credentials = Some(value),
            "--scram-decoy-secret" => decoy_secret = Some(value),
            "--startup-timeout-ms" => {
                config.startup_timeout =
                    Duration::from_millis(value.parse().map_err(|_| "invalid startup timeout")?)
            }
            "--listen" => {
                config.address = value
                    .parse()
                    .map_err(|e| format!("invalid listen address: {e}"))?
            }
            "--max-connections" => {
                config.max_connections = value
                    .parse()
                    .map_err(|e| format!("invalid connection limit: {e}"))?
            }
            "--read-timeout-ms" => {
                config.read_timeout = Duration::from_millis(
                    value
                        .parse()
                        .map_err(|e| format!("invalid read timeout: {e}"))?,
                )
            }
            "--write-timeout-ms" => {
                config.write_timeout = Duration::from_millis(
                    value
                        .parse()
                        .map_err(|e| format!("invalid write timeout: {e}"))?,
                )
            }
            _ => return Err(format!("unknown option {option}")),
        }
    }
    match auth_mode.as_str() {
        "development-trust" => {
            if certificates.is_some()
                || private_key.is_some()
                || credentials.is_some()
                || decoy_secret.is_some()
            {
                return Err("TLS/credential options require --auth scram-tls".into());
            }
        }
        "scram-tls" => {
            let decoy_path = decoy_secret
                .as_deref()
                .ok_or("--scram-decoy-secret required")?;
            let secret = bounded_file(decoy_path, 32)?;
            let secret = DecoySecret::from_bytes(&secret)
                .map_err(|_| "decoy secret must contain exactly 32 binary bytes")?;
            let cert = bounded_file(certificates.as_deref().ok_or("--tls-cert required")?, 65536)?;
            let key = bounded_file(private_key.as_deref().ok_or("--tls-key required")?, 16384)?;
            let verifiers = bounded_file(
                credentials.as_deref().ok_or("--scram-verifiers required")?,
                MAX_CREDENTIAL_BYTES,
            )?;
            let text =
                std::str::from_utf8(&verifiers).map_err(|_| "invalid credential file encoding")?;
            let accounts =
                Accounts::parse(text, secret).map_err(|_| "invalid credential configuration")?;
            let tls = TlsConfig::from_pem(&cert, &key)
                .map_err(|_| "invalid TLS certificate/key configuration")?;
            config.authentication = Authentication::ScramTls {
                accounts: Arc::new(accounts),
                tls,
            };
        }
        _ => return Err("--auth must be development-trust or scram-tls".into()),
    }
    let mode = auth_mode;
    let server = ServerHandle::start(config).map_err(|e| e.to_string())?;
    println!(
        "latticed listening on {} ({mode}; SELECT 1 only)",
        server.local_addr()
    );
    // An explicit host control channel avoids an unsafe signal handler or a new runtime dependency.
    // EOF also stops cleanly, so supervised use must keep stdin open until shutdown is requested.
    // A reader thread forwards stdin so a dead listener can end the process instead of leaving it
    // blocked on stdin while accepting nothing.
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in io::stdin().lock().lines() {
            match line {
                Ok(line) if line.trim() == "shutdown" => break,
                Ok(_) => {}
                Err(error) => {
                    let _ = sender.send(Err(error.to_string()));
                    return;
                }
            }
        }
        let _ = sender.send(Ok(()));
    });
    loop {
        match receiver.recv_timeout(Duration::from_millis(500)) {
            Ok(Ok(())) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            Ok(Err(error)) => return Err(error),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if server.is_finished() {
                    // Surfaces the listener's fatal error, or reports an unexpected exit.
                    return server
                        .shutdown()
                        .map_err(|e| e.to_string())
                        .and_then(|()| Err("listener stopped unexpectedly".into()));
                }
            }
        }
    }
    server.shutdown().map_err(|e| e.to_string())
}

fn bounded_file(path: &str, limit: usize) -> Result<Zeroizing<Vec<u8>>, String> {
    let mut bytes = Zeroizing::new(Vec::new());
    let file = std::fs::File::open(path).map_err(|_| "cannot open configured security file")?;
    file.take((limit + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| "cannot read configured security file")?;
    if bytes.len() > limit {
        return Err("configured security file exceeds bounds".into());
    }
    Ok(bytes)
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("latticed: {error}");
            ExitCode::FAILURE
        }
    }
}

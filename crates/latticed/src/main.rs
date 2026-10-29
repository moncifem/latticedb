//! Composition root: the bounded development PostgreSQL TCP listener.
#![forbid(unsafe_code)]
#![allow(
    clippy::disallowed_methods,
    clippy::disallowed_types,
    clippy::print_stdout,
    clippy::print_stderr
)]

use latticed::{ServerConfig, ServerHandle};
use std::io::{self, BufRead};
use std::process::ExitCode;
use std::time::Duration;

fn run() -> Result<(), String> {
    let mut config = ServerConfig::default();
    let mut args = std::env::args().skip(1);
    while let Some(option) = args.next() {
        if option == "--version" || option == "-V" {
            println!("latticed {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        if option == "--help" || option == "-h" {
            println!(
                "latticed [--listen IP:PORT] [--max-connections N] [--read-timeout-ms N] [--write-timeout-ms N]\nDevelopment trust-auth listener; SELECT 1 only. Send 'shutdown' on stdin for a clean stop."
            );
            return Ok(());
        }
        let value = args
            .next()
            .ok_or_else(|| format!("missing value for {option}"))?;
        match option.as_str() {
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
    let server = ServerHandle::start(config).map_err(|e| e.to_string())?;
    println!(
        "latticed listening on {} (trust authentication; SELECT 1 only)",
        server.local_addr()
    );
    // An explicit host control channel avoids an unsafe signal handler or a new runtime dependency.
    // EOF also stops cleanly, so supervised use must keep stdin open until shutdown is requested.
    for line in io::stdin().lock().lines() {
        if line.map_err(|e| e.to_string())?.trim() == "shutdown" {
            break;
        }
    }
    server.shutdown().map_err(|e| e.to_string())
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

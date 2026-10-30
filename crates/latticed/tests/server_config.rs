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

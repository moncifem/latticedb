//! Sync client edges found by mutation testing: configuration bounds, the printed error text, closing,
//! subscribing and acknowledging at the exact cursor and scope limits, the version check of the
//! welcome, and which events are delivered.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use lattice_sdk::client::{
    Action, ClientConfig, ClientError, CloseReason, Event, Frame, Phase, SyncClient,
};

// The protocol limits of the sync contract: agent names, scopes and cursors.
const MAX_NAME_LEN: usize = 64;
const MAX_SCOPE_LEN: usize = 512;
const MAX_CURSOR_LEN: usize = 64;

fn cfg() -> ClientConfig {
    ClientConfig::new("agent")
}

fn config_error(c: ClientConfig) -> Option<&'static str> {
    match SyncClient::new(c) {
        Err(ClientError::InvalidConfig(why)) => Some(why),
        Err(other) => panic!("{other:?}"),
        Ok(_) => None,
    }
}

#[test]
fn every_configuration_bound_is_checked_on_its_own() {
    assert_eq!(config_error(cfg()), None);
    assert_eq!(
        config_error(ClientConfig {
            min_version: 0,
            ..cfg()
        }),
        Some("version range")
    );
    assert_eq!(
        config_error(ClientConfig {
            min_version: 2,
            max_version: 1,
            ..cfg()
        }),
        Some("version range")
    );
    assert_eq!(
        config_error(ClientConfig {
            min_version: 1,
            max_version: 1,
            ..cfg()
        }),
        None
    );
    assert_eq!(
        config_error(ClientConfig {
            agent: String::new(),
            ..cfg()
        }),
        Some("agent name")
    );
    assert_eq!(
        config_error(ClientConfig {
            agent: "a".repeat(MAX_NAME_LEN),
            ..cfg()
        }),
        None
    );
    assert_eq!(
        config_error(ClientConfig {
            agent: "a".repeat(MAX_NAME_LEN + 1),
            ..cfg()
        }),
        Some("agent name")
    );
    assert_eq!(
        config_error(ClientConfig {
            agent: "a\u{1}b".into(),
            ..cfg()
        }),
        Some("agent name")
    );
    assert_eq!(
        config_error(ClientConfig {
            ack_batch: 0,
            ..cfg()
        }),
        Some("ack batch")
    );
    assert_eq!(
        config_error(ClientConfig {
            ping_timeout: 0,
            ..cfg()
        }),
        Some("ping timeout")
    );
}

#[test]
fn every_client_error_prints_its_text() {
    for (error, text) in [
        (
            ClientError::InvalidConfig("ack batch"),
            "invalid client configuration: ack batch",
        ),
        (
            ClientError::WrongPhase(Phase::Closed),
            "not allowed in phase Closed",
        ),
        (ClientError::InvalidScope, "invalid scope"),
        (ClientError::InvalidCursor, "invalid cursor"),
        (
            ClientError::UnknownSubscription(7),
            "unknown subscription 7",
        ),
        (
            ClientError::AckBeyondDelivered,
            "ack beyond the last delivered cursor",
        ),
        (
            ClientError::SubscriptionIdsExhausted,
            "subscription ids exhausted",
        ),
    ] {
        assert_eq!(error.to_string(), text);
    }
}

fn ready_with(c: ClientConfig) -> SyncClient {
    let mut client = SyncClient::new(c).unwrap();
    client.connect(0).unwrap();
    client
        .handle(
            Frame::Welcome {
                version: 1,
                max_payload: 1 << 20,
                server: "s".into(),
            },
            1,
        )
        .unwrap();
    client
}

fn ready() -> SyncClient {
    ready_with(cfg())
}

#[test]
fn closing_is_terminal_and_reported_once() {
    let mut client = ready();
    assert_eq!(
        client.close(),
        vec![Action::Close(CloseReason::ByApplication)]
    );
    assert_eq!(client.phase(), Phase::Closed);
    assert!(client.close().is_empty());
}

#[test]
fn a_welcome_must_name_a_version_the_client_speaks() {
    let welcome = |version: u16| {
        let mut client = SyncClient::new(cfg()).unwrap();
        client.connect(0).unwrap();
        client
            .handle(
                Frame::Welcome {
                    version,
                    max_payload: 1 << 20,
                    server: "s".into(),
                },
                1,
            )
            .unwrap()
    };
    assert!(
        welcome(1)
            .iter()
            .any(|a| matches!(a, Action::Ready { version: 1, .. }))
    );
    for bad in [0u16, 2] {
        assert_eq!(
            welcome(bad),
            vec![Action::Close(CloseReason::Incompatible)],
            "version {bad}"
        );
    }
}

#[test]
fn a_subscription_scope_and_cursor_may_be_the_maximum_and_no_more() {
    let mut client = ready();
    assert!(
        client
            .subscribe(&"s".repeat(MAX_SCOPE_LEN), &[1; MAX_CURSOR_LEN])
            .is_ok()
    );
    assert_eq!(
        client.subscribe(&"s".repeat(MAX_SCOPE_LEN + 1), &[]).err(),
        Some(ClientError::InvalidScope)
    );
    assert_eq!(
        client.subscribe("", &[]).err(),
        Some(ClientError::InvalidScope)
    );
    assert_eq!(
        client.subscribe("a\nb", &[]).err(),
        Some(ClientError::InvalidScope)
    );
    assert_eq!(
        client.subscribe("s", &[1; MAX_CURSOR_LEN + 1]).err(),
        Some(ClientError::InvalidCursor)
    );
}

fn deliver(client: &mut SyncClient, sub: u32, cursors: &[&[u8]]) -> Vec<Action> {
    let events = cursors
        .iter()
        .map(|c| Event {
            cursor: c.to_vec(),
            data: vec![0],
        })
        .collect();
    client.handle(Frame::Events { sub, events }, 2).unwrap()
}

fn delivered(actions: &[Action]) -> usize {
    actions
        .iter()
        .filter(|a| matches!(a, Action::Deliver { .. }))
        .count()
}

#[test]
fn only_events_after_the_last_delivered_cursor_and_within_the_limit_are_delivered() {
    let mut client = ready();
    let (sub, _) = client.subscribe("s", &[]).unwrap();
    assert_eq!(delivered(&deliver(&mut client, sub, &[&[1]])), 1);
    // The same cursor again, and an older one, are not delivered.
    assert_eq!(delivered(&deliver(&mut client, sub, &[&[1]])), 0);
    assert_eq!(delivered(&deliver(&mut client, sub, &[&[0]])), 0);
    // A cursor of the maximum length is fine; one byte longer is dropped.
    assert_eq!(
        delivered(&deliver(&mut client, sub, &[&[2; MAX_CURSOR_LEN]])),
        1
    );
    assert_eq!(
        delivered(&deliver(&mut client, sub, &[&[3; MAX_CURSOR_LEN + 1]])),
        0
    );
    // Frames for a subscription that does not exist are ignored.
    assert!(deliver(&mut client, 99, &[&[1]]).is_empty());
}

#[test]
fn an_ack_is_checked_against_what_was_delivered() {
    let mut client = ready();
    let (sub, _) = client.subscribe("s", &[]).unwrap();
    deliver(&mut client, sub, &[&[1], &[2]]);
    // At the delivered cursor is fine; beyond it is not; empty and oversized are invalid.
    assert!(client.ack(sub, &[2]).is_ok());
    assert!(
        client.ack(sub, &[1]).is_ok(),
        "an older cursor is allowed (it is merged)"
    );
    assert_eq!(
        client.ack(sub, &[3]).err(),
        Some(ClientError::AckBeyondDelivered)
    );
    assert_eq!(client.ack(sub, &[]).err(), Some(ClientError::InvalidCursor));
    assert_eq!(
        client.ack(sub, &[9; MAX_CURSOR_LEN + 1]).err(),
        Some(ClientError::InvalidCursor)
    );
    assert_eq!(
        client.ack(42, &[1]).err(),
        Some(ClientError::UnknownSubscription(42))
    );
    // A maximal cursor is accepted once delivered.
    deliver(&mut client, sub, &[&[8; MAX_CURSOR_LEN]]);
    assert!(client.ack(sub, &[8; MAX_CURSOR_LEN]).is_ok());
}

#[test]
fn the_version_is_unknown_until_the_welcome_and_a_violation_names_its_rule_on_the_wire() {
    let mut client = SyncClient::new(cfg()).unwrap();
    assert_eq!(client.version(), None);
    client.connect(0).unwrap();
    assert_eq!(client.version(), None);
    client
        .handle(
            Frame::Welcome {
                version: 1,
                max_payload: 1 << 20,
                server: "s".into(),
            },
            1,
        )
        .unwrap();
    assert_eq!(client.version(), Some(1));
    // A second welcome breaks the protocol: the error frame carries the rule as text.
    let out = client
        .handle(
            Frame::Welcome {
                version: 1,
                max_payload: 1 << 20,
                server: "s".into(),
            },
            2,
        )
        .unwrap();
    assert!(
        out.iter().any(|a| matches!(
            a,
            Action::Send(Frame::Error { sub: 0, message, .. }) if message == "welcome outside handshake"
        )),
        "{out:?}"
    );
    assert!(out.iter().any(|a| matches!(
        a,
        Action::Close(CloseReason::ProtocolViolation("welcome outside handshake"))
    )));
}

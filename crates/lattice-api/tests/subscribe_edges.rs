//! Subscription host edges found by mutation testing: every configuration bound on its own, the
//! negotiated version before and after the hello, error text on the wire, and the exact scope and
//! cursor length limits of `Subscribe` and `Ack`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use lattice_api::principal::Principal;
use lattice_api::subscribe::{
    CloseReason, EventSource, HostConfig, Output, SourceError, SubscriptionHost,
};
use lattice_api::sync::{
    ErrorCode, Event, MAX_CURSOR_LEN, MAX_EVENTS_PER_FRAME, MAX_NAME_LEN, MAX_PAYLOAD,
    MAX_SCOPE_LEN, MIN_NEGOTIATED_PAYLOAD, Message,
};

struct Empty;
impl EventSource for Empty {
    fn read_after(
        &self,
        _: &Principal,
        _: &str,
        _: &[u8],
        _: usize,
    ) -> Result<Vec<Event>, SourceError> {
        Ok(Vec::new())
    }
}

fn host(cfg: HostConfig) -> Result<SubscriptionHost, &'static str> {
    SubscriptionHost::local_development(cfg, 0).map_err(|e| e.0)
}

fn base() -> HostConfig {
    HostConfig::new("srv")
}

fn hello() -> Message {
    Message::Hello {
        min_version: 1,
        max_version: 1,
        agent: "a".into(),
    }
}

fn ready() -> SubscriptionHost {
    let mut h = host(base()).unwrap();
    h.handle(hello(), 0, &Empty);
    h
}

#[test]
fn the_server_name_is_one_to_sixty_four_bytes_without_control_characters() {
    assert!(host(HostConfig::new(&"s".repeat(MAX_NAME_LEN))).is_ok());
    assert_eq!(
        host(HostConfig::new(&"s".repeat(MAX_NAME_LEN + 1))).err(),
        Some("server name")
    );
    assert_eq!(host(HostConfig::new("")).err(), Some("server name"));
    assert_eq!(host(HostConfig::new("a\nb")).err(), Some("server name"));
}

#[test]
fn the_maximum_payload_is_between_the_negotiated_minimum_and_the_protocol_maximum() {
    let with = |n: u32| {
        host(HostConfig {
            max_payload: n,
            ..base()
        })
        .err()
    };
    assert_eq!(with(MIN_NEGOTIATED_PAYLOAD - 1), Some("max_payload"));
    assert_eq!(with(MIN_NEGOTIATED_PAYLOAD), None);
    assert_eq!(with(MAX_PAYLOAD as u32), None);
    assert_eq!(with(MAX_PAYLOAD as u32 + 1), Some("max_payload"));
}

#[test]
fn counts_have_their_own_bounds() {
    let sub = |n: usize| {
        host(HostConfig {
            max_subscriptions: n,
            ..base()
        })
        .err()
    };
    assert_eq!(sub(0), Some("max_subscriptions"));
    assert_eq!(sub(1), None);
    let batch = |n: usize| {
        host(HostConfig {
            batch_events: n,
            ..base()
        })
        .err()
    };
    assert_eq!(batch(0), Some("batch_events"));
    assert_eq!(batch(1), None);
    assert_eq!(batch(MAX_EVENTS_PER_FRAME), None);
    assert_eq!(batch(MAX_EVENTS_PER_FRAME + 1), Some("batch_events"));
    let frames = |n: usize| {
        host(HostConfig {
            max_frames_per_pump: n,
            ..base()
        })
        .err()
    };
    assert_eq!(frames(0), Some("max_frames_per_pump"));
    assert_eq!(frames(1), None);
}

#[test]
fn the_version_is_unknown_until_the_hello_is_accepted() {
    let mut h = host(base()).unwrap();
    assert_eq!(h.version(), None);
    h.handle(hello(), 0, &Empty);
    assert_eq!(h.version(), Some(1));
}

fn error_of(out: &[Output]) -> Option<(ErrorCode, String)> {
    out.iter().find_map(|o| match o {
        Output::Send(Message::Error { code, message, .. }) => Some((*code, message.clone())),
        _ => None,
    })
}

#[test]
fn error_text_reaches_the_wire_as_written() {
    let mut h = ready();
    let out = h.handle(
        Message::Subscribe {
            sub: 1,
            scope: String::new(),
            cursor: vec![],
        },
        1,
        &Empty,
    );
    assert_eq!(
        error_of(&out),
        Some((ErrorCode::InvalidScope, "invalid scope".to_string()))
    );
}

#[test]
fn a_scope_may_be_512_bytes_and_a_cursor_64_and_not_one_more() {
    let subscribe = |scope: String, cursor: Vec<u8>| {
        let mut h = ready();
        h.handle(
            Message::Subscribe {
                sub: 1,
                scope,
                cursor,
            },
            1,
            &Empty,
        )
    };
    assert_eq!(
        error_of(&subscribe("s".repeat(MAX_SCOPE_LEN), vec![])),
        None
    );
    assert_eq!(
        error_of(&subscribe("s".repeat(MAX_SCOPE_LEN + 1), vec![])).map(|e| e.0),
        Some(ErrorCode::InvalidScope)
    );
    assert_eq!(
        error_of(&subscribe("s".into(), vec![1; MAX_CURSOR_LEN])),
        None
    );
    assert_eq!(
        error_of(&subscribe("s".into(), vec![1; MAX_CURSOR_LEN + 1])).map(|e| e.0),
        Some(ErrorCode::InvalidCursor)
    );
}

#[test]
fn an_ack_cursor_is_one_to_sixty_four_bytes() {
    let ack = |cursor: Vec<u8>| {
        let mut h = ready();
        h.handle(Message::Ack { sub: 9, cursor }, 1, &Empty)
    };
    // In bounds for an unknown subscription: an error answer, not a protocol violation.
    let ok = ack(vec![1; MAX_CURSOR_LEN]);
    assert_eq!(
        error_of(&ok).map(|e| e.0),
        Some(ErrorCode::UnknownSubscription)
    );
    assert!(!ok.iter().any(|o| matches!(o, Output::Close(_))));
    for bad in [vec![], vec![1; MAX_CURSOR_LEN + 1]] {
        let out = ack(bad);
        assert!(
            out.iter()
                .any(|o| matches!(o, Output::Close(CloseReason::ProtocolViolation(_)))),
            "{out:?}"
        );
    }
}

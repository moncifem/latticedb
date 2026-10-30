//! The subscription pump against scripted event sources: the exact frame-size fit, splitting a batch
//! across frames, and each way a source can break its contract (empty, oversized or unordered cursor,
//! too many events). Mutation testing showed none of these edges was pinned.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use lattice_api::principal::Principal;
use lattice_api::subscribe::{EventSource, HostConfig, Output, SourceError, SubscriptionHost};
use lattice_api::sync::{ErrorCode, Event, MAX_CURSOR_LEN, MAX_PAYLOAD, Message};

/// Events with 8-byte big-endian cursors; reads honour `after` and `limit` like a real log.
struct Log(Vec<Event>);
impl EventSource for Log {
    fn read_after(
        &self,
        _: &Principal,
        _: &str,
        after: &[u8],
        limit: usize,
    ) -> Result<Vec<Event>, SourceError> {
        Ok(self
            .0
            .iter()
            .filter(|e| (e.cursor.len(), e.cursor.as_slice()) > (after.len(), after))
            .take(limit)
            .cloned()
            .collect())
    }
}

/// Answers the first read with these events whatever was asked, then nothing: a source that may
/// break its contract once.
struct Fixed(Vec<Event>, std::cell::Cell<bool>);
impl Fixed {
    fn new(events: Vec<Event>) -> Self {
        Fixed(events, std::cell::Cell::new(false))
    }
}
impl EventSource for Fixed {
    fn read_after(
        &self,
        _: &Principal,
        _: &str,
        _: &[u8],
        _: usize,
    ) -> Result<Vec<Event>, SourceError> {
        if self.1.replace(true) {
            Ok(Vec::new())
        } else {
            Ok(self.0.clone())
        }
    }
}

fn event(seq: u64, data: usize) -> Event {
    Event {
        cursor: seq.to_be_bytes().to_vec(),
        data: vec![0; data],
    }
}

fn subscribe(src: &dyn EventSource, cfg: HostConfig) -> Vec<Output> {
    struct Dyn<'a>(&'a dyn EventSource);
    impl EventSource for Dyn<'_> {
        fn read_after(
            &self,
            p: &Principal,
            s: &str,
            a: &[u8],
            l: usize,
        ) -> Result<Vec<Event>, SourceError> {
            self.0.read_after(p, s, a, l)
        }
    }
    let mut host = SubscriptionHost::local_development(cfg, 0).unwrap();
    let src = Dyn(src);
    host.handle(
        Message::Hello {
            min_version: 1,
            max_version: 1,
            agent: "a".into(),
        },
        0,
        &src,
    );
    host.handle(
        Message::Subscribe {
            sub: 1,
            scope: "s".into(),
            cursor: vec![],
        },
        1,
        &src,
    )
}

fn frames(out: &[Output]) -> Vec<usize> {
    out.iter()
        .filter_map(|o| match o {
            Output::Send(Message::Events { events, .. }) => Some(events.len()),
            _ => None,
        })
        .collect()
}

fn error_text(out: &[Output]) -> Option<(ErrorCode, String)> {
    out.iter().find_map(|o| match o {
        Output::Send(Message::Error { code, message, .. }) => Some((*code, message.clone())),
        _ => None,
    })
}

/// Encoded size of an `Events` payload with one event of an 8-byte cursor: 6 + (6 + 8 + data).
const ONE_EVENT_OVERHEAD: usize = 6 + 6 + 8;

#[test]
fn one_event_may_fill_the_frame_exactly_and_not_one_byte_more() {
    let fits = Log(vec![event(1, MAX_PAYLOAD - ONE_EVENT_OVERHEAD)]);
    let out = subscribe(&fits, HostConfig::new("s"));
    assert_eq!(frames(&out), vec![1]);
    let too_big = Log(vec![event(1, MAX_PAYLOAD - ONE_EVENT_OVERHEAD + 1)]);
    let out = subscribe(&too_big, HostConfig::new("s"));
    assert!(frames(&out).is_empty());
    assert_eq!(
        error_text(&out),
        Some((ErrorCode::Internal, "event too large".into()))
    );
}

#[test]
fn a_batch_that_does_not_fit_one_frame_is_split_across_frames_in_order() {
    let half = (MAX_PAYLOAD - ONE_EVENT_OVERHEAD) / 2 + 1;
    let log = Log(vec![event(1, half), event(2, half), event(3, 10)]);
    let out = subscribe(&log, HostConfig::new("s"));
    // The first two together exceed the frame, so the first goes alone; the next read returns the
    // second and the small third together.
    assert_eq!(frames(&out), vec![1, 2], "{:?}", frames(&out));
    assert_eq!(error_text(&out), None);
    let mut tight = HostConfig::new("s");
    tight.max_frames_per_pump = 1;
    assert_eq!(frames(&subscribe(&log, tight)), vec![1]);
}

#[test]
fn a_source_that_breaks_its_contract_is_answered_with_an_internal_error() {
    let violation =
        |events: Vec<Event>| error_text(&subscribe(&Fixed::new(events), HostConfig::new("s")));
    let internal = |text: &str| Some((ErrorCode::Internal, text.to_string()));
    // Empty cursor, oversized cursor, a repeated cursor, or cursors out of order.
    assert_eq!(
        violation(vec![Event {
            cursor: vec![],
            data: vec![]
        }]),
        internal("source order violation")
    );
    assert_eq!(
        violation(vec![Event {
            cursor: vec![9; MAX_CURSOR_LEN + 1],
            data: vec![]
        }]),
        internal("source order violation")
    );
    assert_eq!(
        violation(vec![event(5, 0), event(5, 0)]),
        internal("source order violation")
    );
    assert_eq!(
        violation(vec![event(6, 0), event(5, 0)]),
        internal("source order violation")
    );
    // A cursor of exactly the maximum length, and strictly increasing, is fine.
    let max = Event {
        cursor: vec![9; MAX_CURSOR_LEN],
        data: vec![],
    };
    assert_eq!(violation(vec![max]), None);
}

#[test]
fn a_source_returning_more_events_than_asked_is_refused_but_exactly_that_many_is_fine() {
    let mut cfg = HostConfig::new("s");
    cfg.batch_events = 2;
    let src = Fixed::new(vec![event(1, 0), event(2, 0), event(3, 0)]);
    assert_eq!(
        error_text(&subscribe(&src, cfg.clone())),
        Some((ErrorCode::Internal, "source returned too many".into()))
    );
    let ok = Fixed::new(vec![event(1, 0), event(2, 0)]);
    let out = subscribe(&ok, cfg);
    assert_eq!(error_text(&out), None);
    assert_eq!(frames(&out)[0], 2);
}

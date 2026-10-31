#![no_main]
use lattice_api::principal::{LOCAL_DEVELOPMENT_NAME, Principal};
use lattice_api::subscribe::{EventSource, HostConfig, Output, SourceError, SubscriptionHost};
use lattice_api::sync::{self, Event, HEADER_LEN, MAX_CURSOR_LEN, MAX_PAYLOAD, Message};
use libfuzzer_sys::fuzz_target;
use std::collections::BTreeMap;

/// A synthetic log of events 1..=`len`, each with an 8-byte big-endian cursor. `mode` picks the
/// behaviour: 0 honest, 1 unavailable, 2 too old, 3 honest but delivered out of order. The host
/// checks the source contract and must never forward the out-of-order batch.
struct Source {
    mode: u8,
    len: u64,
}

impl EventSource for Source {
    fn read_after(
        &self,
        principal: &Principal,
        scope: &str,
        after: &[u8],
        limit: usize,
    ) -> Result<Vec<Event>, SourceError> {
        // Every read is made for the identity the connection authenticated as.
        assert_eq!(principal.name(), LOCAL_DEVELOPMENT_NAME);
        match self.mode {
            1 => return Err(SourceError::Unavailable),
            2 => return Err(SourceError::TooOld),
            _ => {}
        }
        if !scope.starts_with("ws/") {
            return Err(SourceError::UnknownScope);
        }
        let mut events = Vec::new();
        for seq in 1..=self.len {
            let cursor = seq.to_be_bytes().to_vec();
            if (cursor.len(), cursor.as_slice()) <= (after.len(), after) {
                continue;
            }
            if events.len() == limit {
                break;
            }
            events.push(Event {
                cursor: cursor.clone(),
                data: cursor,
            });
        }
        if self.mode == 3 {
            events.reverse();
        }
        Ok(events)
    }
}

/// `a` is strictly after `b`: length first, then bytes, the order the client uses.
fn after(a: &[u8], b: &[u8]) -> bool {
    (a.len(), a) > (b.len(), b)
}

/// What a well-behaved client can see: the cursor each subscription was opened at or last
/// delivered up to. The host's own record must agree with it.
struct Client {
    batch: usize,
    max_subscriptions: usize,
    max_payload: u32,
    delivered: BTreeMap<u32, Vec<u8>>,
}

impl Client {
    /// Called with every message before the host sees it.
    fn request(&mut self, msg: &Message) {
        match msg {
            Message::Subscribe { sub, cursor, .. } => {
                self.delivered.insert(*sub, cursor.clone());
            }
            Message::Error { sub, .. } if *sub != 0 => {
                self.delivered.remove(sub);
            }
            _ => {}
        }
    }

    /// Called with every output of the host.
    fn check(&mut self, host: &SubscriptionHost, outs: &[Output]) {
        for out in outs {
            match out {
                Output::Send(Message::Welcome {
                    version,
                    max_payload,
                    ..
                }) => {
                    assert!((sync::VERSION_MIN..=sync::VERSION_MAX).contains(version));
                    assert_eq!(*max_payload, self.max_payload);
                }
                Output::Send(Message::Events { sub, events }) => {
                    assert!(
                        !events.is_empty() && events.len() <= self.batch,
                        "an events frame holds one to batch_events events"
                    );
                    let mut last = self
                        .delivered
                        .get(sub)
                        .cloned()
                        .expect("events only for an open subscription");
                    for e in events {
                        assert!(!e.cursor.is_empty() && e.cursor.len() <= MAX_CURSOR_LEN);
                        assert!(
                            after(&e.cursor, &last),
                            "a delivered event is not after the cursor before it"
                        );
                        last = e.cursor.clone();
                    }
                    self.delivered.insert(*sub, last);
                }
                Output::Send(msg) => {
                    let mut frame = Vec::new();
                    sync::encode(msg, &mut frame)
                        .expect("the host sends only frames the codec encodes");
                    assert!(frame.len() <= HEADER_LEN + MAX_PAYLOAD);
                }
                Output::Close(_) => {
                    assert!(
                        host.is_closed(),
                        "a close output on a host that is still open"
                    );
                }
            }
        }
        assert!(host.subscriptions().len() <= self.max_subscriptions);
        // A closed host keeps its table but ignores later input, and the client may already have
        // recorded a refused request (a duplicate `Subscribe`), so the agreement is checked only while
        // the connection is open.
        if host.is_closed() {
            return;
        }
        for sub in host.subscriptions() {
            assert_eq!(
                host.sent(sub),
                self.delivered.get(&sub).map(Vec::as_slice),
                "the host and the client disagree on the last cursor sent"
            );
        }
    }
}

fuzz_target!(|data: &[u8]| {
    // The input is `control tuning frames`. Control: bits 0-1 pick the source mode and bits 2-7
    // the log length (0..=63 events). Tuning: the batch size, the subscription limit and the frames
    // per pump, each within the range the host accepts. Then the client's frames, decoded one by
    // one as a stream; a frame that does not decode ends the stream.
    let (Some(&control), Some(&tuning)) = (data.first(), data.get(1)) else {
        return;
    };
    let Some(frames) = data.get(2..) else {
        return;
    };
    let cfg = HostConfig {
        batch_events: 1 + usize::from(tuning & 7),
        max_subscriptions: 1 + usize::from((tuning >> 3) & 3),
        max_frames_per_pump: 1 + usize::from((tuning >> 5) & 3),
        ..HostConfig::new("srv")
    };
    let mut client = Client {
        batch: cfg.batch_events,
        max_subscriptions: cfg.max_subscriptions,
        max_payload: cfg.max_payload,
        delivered: BTreeMap::new(),
    };
    let mut host =
        SubscriptionHost::local_development(cfg, 0).expect("the fuzz configuration is in range");
    let src = Source {
        mode: control & 3,
        len: u64::from(control >> 2),
    };

    let mut now = 0u64;
    let mut at = 0usize;
    while let Some(rest) = frames.get(at..) {
        let Ok(Some((msg, used))) = sync::decode(rest) else {
            break;
        };
        at += used;
        now = now.saturating_add(used as u64);

        client.request(&msg);
        let was_closed = host.is_closed();
        let outs = host.handle(msg, now, &src);
        assert!(!was_closed || outs.is_empty(), "a closed host answered");
        client.check(&host, &outs);

        let was_closed = host.is_closed();
        let outs = host.poll(now, &src);
        assert!(!was_closed || outs.is_empty(), "a closed host answered");
        client.check(&host, &outs);
    }
    // A long silence runs the handshake and ping deadlines.
    let outs = host.poll(now.saturating_add(1_000), &src);
    client.check(&host, &outs);
});

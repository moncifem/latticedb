//! Outbox limits at their exact edges: configuration ranges, the client id, what `enqueue` accepts,
//! how many fenced turns are remembered, the limits applied when a stored queue is restored, and the
//! printed text of the error. Mutation testing showed none of them was pinned.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use lattice_sdk::contract::{Kind, MAX_BODY_BYTES, MAX_INT, MAX_RECORDS, Receipt, Status};
use lattice_sdk::outbox::{
    DecodeError, MAX_BYTES_LIMIT, MAX_CLIENT_ID_BYTES, MAX_FENCED_TURNS, MAX_ITEMS_LIMIT, Outbox,
    OutboxConfig, OutboxError,
};

fn cfg() -> OutboxConfig {
    OutboxConfig {
        max_items: 3,
        max_bytes: 10,
        max_batch: 4,
        base_delay: 1,
        max_delay: 8,
    }
}

fn outbox(cfg: OutboxConfig) -> Outbox {
    Outbox::new("client", cfg).unwrap()
}

#[test]
fn the_configuration_ranges_are_exact() {
    let ok = |c: OutboxConfig| Outbox::new("c", c).is_ok();
    let base = cfg();
    assert!(ok(OutboxConfig {
        max_items: 1,
        ..base
    }));
    assert!(ok(OutboxConfig {
        max_items: MAX_ITEMS_LIMIT,
        ..base
    }));
    assert!(!ok(OutboxConfig {
        max_items: 0,
        ..base
    }));
    assert!(!ok(OutboxConfig {
        max_items: MAX_ITEMS_LIMIT + 1,
        ..base
    }));
    assert!(ok(OutboxConfig {
        max_bytes: 1,
        ..base
    }));
    assert!(ok(OutboxConfig {
        max_bytes: MAX_BYTES_LIMIT,
        ..base
    }));
    assert!(!ok(OutboxConfig {
        max_bytes: 0,
        ..base
    }));
    assert!(!ok(OutboxConfig {
        max_bytes: MAX_BYTES_LIMIT + 1,
        ..base
    }));
    assert!(ok(OutboxConfig {
        max_batch: 1,
        ..base
    }));
    assert!(ok(OutboxConfig {
        max_batch: MAX_RECORDS,
        ..base
    }));
    assert!(!ok(OutboxConfig {
        max_batch: 0,
        ..base
    }));
    assert!(!ok(OutboxConfig {
        max_batch: MAX_RECORDS + 1,
        ..base
    }));
    assert!(!ok(OutboxConfig {
        base_delay: 0,
        ..base
    }));
    assert!(ok(OutboxConfig {
        base_delay: 8,
        max_delay: 8,
        ..base
    }));
    assert!(!ok(OutboxConfig {
        base_delay: 9,
        max_delay: 8,
        ..base
    }));
    assert_eq!(
        Outbox::new(
            "c",
            OutboxConfig {
                max_items: 0,
                ..base
            }
        )
        .err(),
        Some(OutboxError::InvalidConfig)
    );
    let default = OutboxConfig::default();
    assert_eq!(
        (default.max_items, default.max_bytes),
        (1024, 4 * 1024 * 1024)
    );
}

#[test]
fn a_client_id_is_one_to_a_hundred_safe_characters() {
    let id = |s: &str| Outbox::new(s, cfg()).err();
    assert_eq!(id(&"a".repeat(MAX_CLIENT_ID_BYTES)), None);
    assert_eq!(
        id(&"a".repeat(MAX_CLIENT_ID_BYTES + 1)),
        Some(OutboxError::InvalidClientId)
    );
    assert_eq!(id(""), Some(OutboxError::InvalidClientId));
    assert_eq!(id("has space"), Some(OutboxError::InvalidClientId));
    assert_eq!(id("Az09._-"), None);
    assert_eq!(outbox(cfg()).client_id(), "client");
}

#[test]
fn enqueue_accepts_the_edge_values_and_refuses_one_past_them() {
    let mut o = outbox(cfg());
    assert!(o.enqueue(MAX_INT, Kind::Message, "x").is_ok());
    assert_eq!(
        o.enqueue(MAX_INT + 1, Kind::Message, "x"),
        Err(OutboxError::TurnTooLarge)
    );
    // The byte budget is 10: a body of 10 fits, 11 can never be queued.
    let mut o = outbox(cfg());
    assert!(o.enqueue(1, Kind::Message, "0123456789").is_ok());
    let mut o = outbox(cfg());
    assert_eq!(
        o.enqueue(1, Kind::Message, "0123456789a"),
        Err(OutboxError::BodyExceedsBudget)
    );
    // Bigger than the contract allows is reported as such, whatever the budget.
    let mut big = outbox(OutboxConfig {
        max_bytes: MAX_BYTES_LIMIT,
        ..cfg()
    });
    assert_eq!(
        big.enqueue(1, Kind::Message, &"x".repeat(MAX_BODY_BYTES + 1)),
        Err(OutboxError::BodyTooLarge)
    );
    assert_ne!(
        big.enqueue(1, Kind::Message, &"x".repeat(MAX_BODY_BYTES)),
        Err(OutboxError::BodyTooLarge)
    );
}

#[test]
fn the_queue_is_full_at_the_item_limit_and_at_the_byte_limit_exactly() {
    let mut o = outbox(cfg());
    for _ in 0..3 {
        o.enqueue(1, Kind::Message, "a").unwrap();
    }
    assert_eq!(
        o.enqueue(1, Kind::Message, "a"),
        Err(OutboxError::Full { items: 3, bytes: 3 })
    );
    let mut o = outbox(OutboxConfig {
        max_items: 8,
        ..cfg()
    });
    o.enqueue(1, Kind::Message, "123456").unwrap();
    assert_eq!(
        o.enqueue(1, Kind::Message, "12345"),
        Err(OutboxError::Full { items: 1, bytes: 6 })
    );
    o.enqueue(1, Kind::Message, "1234").unwrap();
    assert_eq!((o.len(), o.bytes()), (2, 10));
    assert!(matches!(
        o.enqueue(1, Kind::Message, ""),
        Ok(_) | Err(OutboxError::Full { .. })
    ));
}

#[test]
fn the_oldest_fenced_turns_are_forgotten_beyond_the_limit() {
    let mut o = outbox(OutboxConfig {
        max_items: 64,
        max_bytes: 64,
        ..cfg()
    });
    for turn in 1..=(MAX_FENCED_TURNS as u64 + 1) {
        o.enqueue(turn, Kind::Message, "a").unwrap();
        let rid = o.pending().last().unwrap().rid.clone();
        o.on_receipts(&[Receipt {
            rid,
            status: Status::Fenced,
            pos: None,
        }]);
    }
    let fenced: Vec<u64> = o.fenced_turns().collect();
    assert_eq!(fenced.len(), MAX_FENCED_TURNS);
    assert_eq!(
        fenced.first(),
        Some(&2),
        "turn 1, the oldest, was forgotten"
    );
    assert_eq!(fenced.last(), Some(&(MAX_FENCED_TURNS as u64 + 1)));
}

#[test]
fn a_stored_queue_is_restored_only_within_the_limits_given() {
    let mut o = outbox(OutboxConfig {
        max_bytes: 15,
        ..cfg()
    });
    for _ in 0..3 {
        o.enqueue(1, Kind::Message, "12345").unwrap();
    }
    let stored = o.snapshot().into_bytes();
    let same = OutboxConfig {
        max_bytes: 15,
        ..cfg()
    };
    assert!(Outbox::from_bytes(&stored, same).is_ok());
    assert_eq!(
        Outbox::from_bytes(
            &stored,
            OutboxConfig {
                max_items: 2,
                ..same
            }
        )
        .err(),
        Some(DecodeError::OverLimit)
    );
    assert_eq!(
        Outbox::from_bytes(
            &stored,
            OutboxConfig {
                max_bytes: 14,
                ..same
            }
        )
        .err(),
        Some(DecodeError::OverLimit)
    );
    assert_eq!(
        Outbox::from_bytes(
            &stored,
            OutboxConfig {
                max_items: 0,
                ..same
            }
        )
        .err(),
        Some(DecodeError::InvalidConfig)
    );
}

#[test]
fn every_outbox_error_prints_its_text() {
    for (error, text) in [
        (OutboxError::InvalidConfig, "invalid outbox configuration"),
        (OutboxError::InvalidClientId, "invalid client id"),
        (
            OutboxError::Full { items: 3, bytes: 9 },
            "outbox full (3 items, 9 bytes)",
        ),
        (OutboxError::BodyTooLarge, "body too large"),
        (
            OutboxError::BodyExceedsBudget,
            "body larger than the outbox byte budget",
        ),
        (
            OutboxError::BodyUnsendable,
            "escaped body does not fit in one protocol line",
        ),
        (OutboxError::TurnTooLarge, "turn number too large"),
        (OutboxError::TurnFenced(7), "turn 7 is fenced"),
        (OutboxError::CounterExhausted, "record counter exhausted"),
    ] {
        assert_eq!(error.to_string(), text);
    }
}

// ---------------------------------------------------------------------------------------------
// The stored form, built by hand: magic, version, id, counter, fenced turns, items, CRC-32.

use lattice_sdk::outbox::MAX_STORED_BYTES;

fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

struct Stored {
    id: &'static str,
    counter: u64,
    fenced: Vec<u64>,
    /// (counter, turn, kind byte, body)
    items: Vec<(u64, u64, u8, Vec<u8>)>,
}

impl Stored {
    fn new() -> Self {
        Stored {
            id: "c",
            counter: 10,
            fenced: vec![],
            items: vec![],
        }
    }

    fn bytes(&self) -> Vec<u8> {
        let mut b = b"LXOB".to_vec();
        b.push(1);
        b.extend_from_slice(&(self.id.len() as u16).to_le_bytes());
        b.extend_from_slice(self.id.as_bytes());
        b.extend_from_slice(&self.counter.to_le_bytes());
        b.extend_from_slice(&(self.fenced.len() as u16).to_le_bytes());
        for t in &self.fenced {
            b.extend_from_slice(&t.to_le_bytes());
        }
        b.extend_from_slice(&(self.items.len() as u32).to_le_bytes());
        for (counter, turn, kind, body) in &self.items {
            b.extend_from_slice(&counter.to_le_bytes());
            b.extend_from_slice(&turn.to_le_bytes());
            b.push(*kind);
            b.extend_from_slice(&(body.len() as u32).to_le_bytes());
            b.extend_from_slice(body);
        }
        let crc = crc32(&b);
        b.extend_from_slice(&crc.to_le_bytes());
        b
    }
}

fn restore(s: &Stored) -> Result<Outbox, DecodeError> {
    Outbox::from_bytes(
        &s.bytes(),
        OutboxConfig {
            max_items: 512,
            max_bytes: MAX_BYTES_LIMIT,
            ..cfg()
        },
    )
}

fn noncanonical(what: &'static str) -> Option<DecodeError> {
    Some(DecodeError::NonCanonical(what))
}

#[test]
fn a_stored_queue_is_too_short_below_twenty_five_bytes_and_too_large_above_the_cap() {
    assert_eq!(
        Outbox::from_bytes(&[0u8; 24], cfg()).err(),
        Some(DecodeError::TooShort)
    );
    // Twenty-five bytes are enough to be looked at: the wrong magic is then what is wrong.
    assert_eq!(
        Outbox::from_bytes(&[0u8; 25], cfg()).err(),
        Some(DecodeError::BadMagic)
    );
    assert_eq!(
        Outbox::from_bytes(&vec![0u8; MAX_STORED_BYTES], cfg()).err(),
        Some(DecodeError::BadMagic)
    );
    assert_eq!(
        Outbox::from_bytes(&vec![0u8; MAX_STORED_BYTES + 1], cfg()).err(),
        Some(DecodeError::TooLarge)
    );
}

#[test]
fn the_stored_counter_may_be_the_largest_wire_integer_plus_one_and_no_more() {
    let with = |counter: u64| {
        restore(&Stored {
            counter,
            ..Stored::new()
        })
        .err()
    };
    assert_eq!(with(MAX_INT + 1), None);
    assert_eq!(with(MAX_INT + 2), noncanonical("counter too large"));
}

#[test]
fn fenced_turns_are_limited_in_number_and_value() {
    let turns = |n: u64| (1..=n).collect::<Vec<_>>();
    let with = |fenced: Vec<u64>| {
        restore(&Stored {
            fenced,
            ..Stored::new()
        })
        .err()
    };
    assert_eq!(with(turns(MAX_FENCED_TURNS as u64)), None);
    assert_eq!(
        with(turns(MAX_FENCED_TURNS as u64 + 1)),
        noncanonical("too many fenced turns")
    );
    assert_eq!(with(vec![MAX_INT]), None);
    assert_eq!(with(vec![MAX_INT + 1]), noncanonical("fenced turns"));
    assert_eq!(with(vec![5, 5]), noncanonical("fenced turns"));
    assert_eq!(with(vec![6, 5]), noncanonical("fenced turns"));
}

#[test]
fn stored_items_are_checked_field_by_field_at_the_edge() {
    let one = |counter: u64, turn: u64, body: Vec<u8>| {
        restore(&Stored {
            items: vec![(counter, turn, 0, body)],
            ..Stored::new()
        })
        .err()
    };
    // The counter must be below the next counter.
    assert_eq!(one(9, 1, vec![b'x']), None);
    assert_eq!(one(10, 1, vec![b'x']), noncanonical("item counters"));
    // The turn must be a wire integer and not a fenced one.
    assert_eq!(one(0, MAX_INT, vec![b'x']), None);
    assert_eq!(one(0, MAX_INT + 1, vec![b'x']), noncanonical("item turn"));
    let fenced = restore(&Stored {
        fenced: vec![3],
        items: vec![(0, 3, 0, vec![b'x'])],
        ..Stored::new()
    });
    assert_eq!(fenced.err(), noncanonical("item turn"));
    // Counters strictly increase.
    let twice = restore(&Stored {
        items: vec![(2, 1, 0, vec![]), (2, 1, 0, vec![])],
        ..Stored::new()
    });
    assert_eq!(twice.err(), noncanonical("item counters"));
    // The body is at most the contract limit and valid text.
    assert_eq!(
        one(0, 1, vec![b'x'; MAX_BODY_BYTES + 1]),
        noncanonical("body too large")
    );
    assert_ne!(
        one(0, 1, vec![b'x'; MAX_BODY_BYTES]),
        noncanonical("body too large")
    );
    assert_eq!(one(0, 1, vec![0xff]), noncanonical("body is not UTF-8"));
}

#[test]
fn the_restored_byte_total_may_equal_the_budget_and_not_exceed_it() {
    let items = |n: usize| {
        (0..n as u64)
            .map(|i| (i, 1, 0, vec![b'x'; 5]))
            .collect::<Vec<_>>()
    };
    let bytes = Stored {
        items: items(3),
        ..Stored::new()
    }
    .bytes();
    let cfg = |max_bytes: usize| OutboxConfig {
        max_items: 8,
        max_bytes,
        ..cfg()
    };
    assert!(Outbox::from_bytes(&bytes, cfg(15)).is_ok());
    assert_eq!(
        Outbox::from_bytes(&bytes, cfg(14)).err(),
        Some(DecodeError::OverLimit)
    );
    // The item count may equal the limit too.
    let limit = |max_items: usize| OutboxConfig {
        max_items,
        max_bytes: 100,
        ..cfg(100)
    };
    assert!(Outbox::from_bytes(&bytes, limit(3)).is_ok());
    assert_eq!(
        Outbox::from_bytes(&bytes, limit(2)).err(),
        Some(DecodeError::OverLimit)
    );
}

#[test]
fn every_decode_error_prints_its_text() {
    for error in [
        DecodeError::TooLarge,
        DecodeError::TooShort,
        DecodeError::BadMagic,
        DecodeError::UnsupportedVersion(2),
        DecodeError::BadChecksum,
        DecodeError::Truncated,
        DecodeError::TrailingBytes,
        DecodeError::OverLimit,
        DecodeError::InvalidConfig,
        DecodeError::NonCanonical("item counters"),
    ] {
        let text = error.to_string();
        assert!(!text.is_empty(), "{error:?}");
    }
    assert_eq!(
        DecodeError::OverLimit.to_string(),
        "stored outbox exceeds the configured limits"
    );
    assert!(DecodeError::UnsupportedVersion(2).to_string().contains('2'));
    assert!(
        DecodeError::NonCanonical("item counters")
            .to_string()
            .contains("item counters")
    );
}

// ---------------------------------------------------------------------------------------------
// Line-size edges, with the contract's own length functions as the oracle.

use lattice_sdk::contract::{APPEND_ENVELOPE_MAX_BYTES, MAX_LINE_BYTES, append_record_len};

fn big_cfg() -> OutboxConfig {
    OutboxConfig {
        max_items: 64,
        max_bytes: MAX_BYTES_LIMIT,
        max_batch: MAX_RECORDS,
        base_delay: 1,
        max_delay: 8,
    }
}

#[test]
fn a_write_whose_line_is_exactly_the_limit_is_accepted_and_one_byte_more_is_not() {
    // Control characters escape to six bytes, so `controls` of them plus `plain` ordinary ones can
    // land the line on exactly MAX_LINE_BYTES.
    let rid = "client-0";
    let line = |body: &str| APPEND_ENVELOPE_MAX_BYTES + append_record_len(rid, Kind::Message, body);
    let controls = (MAX_LINE_BYTES - APPEND_ENVELOPE_MAX_BYTES) / 6 - 8;
    let plain = (0..6)
        .find(|plain| {
            line(&format!(
                "{}{}",
                "\u{1}".repeat(controls),
                "x".repeat(*plain)
            )) == MAX_LINE_BYTES
        })
        .or_else(|| {
            (6..400).find(|plain| {
                line(&format!(
                    "{}{}",
                    "\u{1}".repeat(controls),
                    "x".repeat(*plain)
                )) == MAX_LINE_BYTES
            })
        })
        .unwrap();
    let exact = format!("{}{}", "\u{1}".repeat(controls), "x".repeat(plain));
    assert_eq!(line(&exact), MAX_LINE_BYTES);
    assert!(exact.len() <= MAX_BODY_BYTES);
    let mut o = Outbox::new("client", big_cfg()).unwrap();
    assert!(o.enqueue(1, Kind::Message, &exact).is_ok());
    let mut o = Outbox::new("client", big_cfg()).unwrap();
    let one_more = format!("{exact}x");
    assert_eq!(line(&one_more), MAX_LINE_BYTES + 1);
    assert_eq!(
        o.enqueue(1, Kind::Message, &one_more),
        Err(OutboxError::BodyUnsendable)
    );
}

#[test]
fn the_counter_may_reach_the_largest_wire_integer_and_not_pass_it() {
    let stored = Stored {
        counter: MAX_INT,
        ..Stored::new()
    };
    let mut o = restore(&stored).unwrap();
    assert!(o.enqueue(1, Kind::Message, "x").is_ok());
    assert_eq!(
        o.enqueue(1, Kind::Message, "x"),
        Err(OutboxError::CounterExhausted)
    );
}

#[test]
fn a_batch_is_cut_where_its_line_would_pass_the_limit_and_not_before() {
    // Records of 200,000 bytes; the last one is sized so the batch line is exactly MAX_LINE_BYTES.
    let body = |n: usize| "x".repeat(n);
    let record = |i: usize, b: &str| append_record_len(&format!("client-{i}"), Kind::Message, b);
    let first = 4;
    let fixed: usize = (0..first).map(|i| record(i, &body(200_000))).sum();
    // Commas between `first + 1` records: `first` of them.
    let base = APPEND_ENVELOPE_MAX_BYTES + fixed + first + record(first, "");
    let last = MAX_LINE_BYTES - base;
    assert!(last <= MAX_BODY_BYTES);
    let batch_len = |last_len: usize| {
        let mut o = Outbox::new("client", big_cfg()).unwrap();
        for _ in 0..first {
            o.enqueue(1, Kind::Message, &body(200_000)).unwrap();
        }
        o.enqueue(1, Kind::Message, &body(last_len)).unwrap();
        let snapshot = o.snapshot();
        o.mark_persisted(&snapshot);
        o.poll(0, 0).unwrap().records.len()
    };
    assert_eq!(
        batch_len(last),
        first + 1,
        "a line exactly at the limit stays whole"
    );
    assert_eq!(
        batch_len(last + 1),
        first,
        "one byte more moves the last record to the next batch"
    );
}

#[test]
fn a_stored_write_whose_line_is_exactly_the_limit_restores_and_one_byte_more_does_not() {
    let rid = "c-0";
    let line = |body: &str| APPEND_ENVELOPE_MAX_BYTES + append_record_len(rid, Kind::Message, body);
    let controls = (MAX_LINE_BYTES - APPEND_ENVELOPE_MAX_BYTES) / 6 - 8;
    let plain = (0..400)
        .find(|plain| {
            line(&format!(
                "{}{}",
                "\u{1}".repeat(controls),
                "x".repeat(*plain)
            )) == MAX_LINE_BYTES
        })
        .unwrap();
    let exact = format!("{}{}", "\u{1}".repeat(controls), "x".repeat(plain));
    let stored = |body: &str| {
        restore(&Stored {
            items: vec![(0, 1, 0, body.as_bytes().to_vec())],
            ..Stored::new()
        })
        .err()
    };
    assert_eq!(stored(&exact), None);
    assert_eq!(
        stored(&format!("{exact}x")),
        noncanonical("body does not fit in one line")
    );
}

#[test]
fn a_receipt_with_a_non_canonical_record_id_clears_nothing() {
    let mut o = outbox(OutboxConfig {
        max_items: 8,
        max_bytes: 64,
        ..cfg()
    });
    o.enqueue(1, Kind::Message, "a").unwrap();
    o.enqueue(1, Kind::Message, "b").unwrap();
    for rid in [
        "client-",
        "client-01",
        "client-+1",
        "client-1x",
        "client-00000000000000001",
        "other-1",
    ] {
        let out = o.on_receipts(&[Receipt {
            rid: rid.into(),
            status: Status::Recorded,
            pos: Some(1),
        }]);
        assert_eq!(out.unknown, 1, "{rid}");
        assert!(out.done.is_empty(), "{rid}");
    }
    assert_eq!(o.len(), 2);
    let out = o.on_receipts(&[Receipt {
        rid: "client-1".into(),
        status: Status::Recorded,
        pos: Some(1),
    }]);
    assert_eq!(out.done, vec!["client-1".to_string()]);
    assert_eq!(o.len(), 1);
}

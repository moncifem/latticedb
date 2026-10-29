//! Outbox tests: scripted scenarios, the stored format, and a crash/restart property against a
//! server that applies each record id at most once.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use std::collections::{BTreeMap, BTreeSet};

use lattice_sdk::contract::{Kind, Receipt, Status};
use lattice_sdk::outbox::{
    Batch, DecodeError, MAX_STORED_BYTES, Outbox, OutboxConfig, OutboxError,
};
use proptest::prelude::*;

fn cfg() -> OutboxConfig {
    OutboxConfig {
        max_items: 8,
        max_bytes: 64,
        max_batch: 3,
        base_delay: 2,
        max_delay: 16,
    }
}

fn outbox() -> Outbox {
    Outbox::new("c1", cfg()).unwrap()
}

/// Store the queue the way an application would: bytes first, then the barrier.
fn persist(o: &mut Outbox) -> Vec<u8> {
    let upto = o.next_counter();
    let bytes = o.to_bytes();
    o.mark_persisted(upto);
    bytes
}

/// A server that applies each record id at most once and fences chosen turns.
#[derive(Default)]
struct Server {
    /// rid -> (position, body)
    stored: BTreeMap<String, (u64, String)>,
    /// Every effect, in order: the body of each record applied.
    effects: Vec<String>,
    fenced: BTreeSet<u64>,
}

impl Server {
    fn append(&mut self, b: &Batch) -> Vec<Receipt> {
        b.records
            .iter()
            .map(|r| {
                if self.fenced.contains(&b.turn) {
                    return Receipt {
                        rid: r.rid.clone(),
                        status: Status::Fenced,
                        pos: None,
                    };
                }
                if let Some((pos, _)) = self.stored.get(&r.rid) {
                    return Receipt {
                        rid: r.rid.clone(),
                        status: Status::Duplicate,
                        pos: Some(*pos),
                    };
                }
                let pos = self.effects.len() as u64;
                self.effects.push(r.body.clone());
                self.stored.insert(r.rid.clone(), (pos, r.body.clone()));
                Receipt {
                    rid: r.rid.clone(),
                    status: Status::Recorded,
                    pos: Some(pos),
                }
            })
            .collect()
    }
}

fn rids(b: &Batch) -> Vec<&str> {
    b.records.iter().map(|r| r.rid.as_str()).collect()
}

#[test]
fn offline_writes_wait_for_the_persist_barrier_then_go_out_in_order() {
    let mut o = outbox();
    assert_eq!(o.enqueue(1, Kind::Message, "a").unwrap(), "c1-0");
    assert_eq!(o.enqueue(1, Kind::Note, "b").unwrap(), "c1-1");
    assert_eq!(o.poll(0, 0), None, "unpersisted writes are not sent");
    persist(&mut o);
    let b = o.poll(0, 0).unwrap();
    assert_eq!((b.turn, b.attempt), (1, 0));
    assert_eq!(rids(&b), ["c1-0", "c1-1"]);
    assert_eq!(b.records[1].kind, Kind::Note);
    // A write queued after the persist is held back until the next persist.
    assert_eq!(o.enqueue(1, Kind::Message, "c").unwrap(), "c1-2");
    assert_eq!(o.poll(100, 0).unwrap().records.len(), 2);
}

#[test]
fn a_stale_barrier_value_does_not_release_newer_writes() {
    let mut o = outbox();
    o.enqueue(1, Kind::Message, "a").unwrap();
    let upto = o.next_counter();
    let _stored = o.to_bytes();
    o.enqueue(1, Kind::Message, "b").unwrap();
    o.mark_persisted(upto);
    assert_eq!(rids(&o.poll(0, 0).unwrap()), ["c1-0"]);
    o.mark_persisted(99); // beyond the counter: ignored
    o.connection_lost();
    assert_eq!(rids(&o.poll(0, 0).unwrap()), ["c1-0"]);
}

#[test]
fn batches_hold_one_turn_and_respect_max_batch() {
    let mut o = outbox();
    for (t, body) in [(1, "a"), (1, "b"), (2, "c"), (2, "d"), (2, "e"), (2, "f")] {
        o.enqueue(t, Kind::Message, body).unwrap();
    }
    persist(&mut o);
    let b = o.poll(0, 0).unwrap();
    assert_eq!((b.turn, rids(&b)), (1, vec!["c1-0", "c1-1"]));
    let mut s = Server::default();
    let out = o.on_receipts(&s.append(&b));
    assert_eq!(out.done, ["c1-0", "c1-1"]);
    let b = o.poll(0, 0).unwrap();
    assert_eq!((b.turn, rids(&b)), (2, vec!["c1-2", "c1-3", "c1-4"]));
}

#[test]
fn retries_back_off_exponentially_up_to_the_cap_with_caller_jitter() {
    let mut o = outbox();
    o.enqueue(1, Kind::Message, "a").unwrap();
    persist(&mut o);
    assert_eq!(o.poll(0, 0).unwrap().attempt, 0);
    assert_eq!(o.next_deadline(), Some(2));
    assert_eq!(o.poll(1, 0), None);
    // delay 4 after the first resend
    assert_eq!(o.poll(2, 0).unwrap().attempt, 1);
    assert_eq!(o.next_deadline(), Some(6));
    assert_eq!(o.poll(6, 0).unwrap().attempt, 2); // delay 8
    assert_eq!(o.next_deadline(), Some(14));
    assert_eq!(o.poll(14, 0).unwrap().attempt, 3); // delay 16
    assert_eq!(o.next_deadline(), Some(30));
    assert_eq!(o.poll(30, 0).unwrap().attempt, 4); // capped at 16
    assert_eq!(o.next_deadline(), Some(46));
    // Jitter adds jitter % (delay / 2 + 1): delay 16 -> 20 % 9 = 2.
    assert_eq!(o.poll(46, 20).unwrap().attempt, 5);
    assert_eq!(o.next_deadline(), Some(46 + 16 + 2));
    // A huge attempt count and time cannot overflow.
    let mut o = outbox();
    o.enqueue(1, Kind::Message, "a").unwrap();
    persist(&mut o);
    let mut now = u64::MAX - 5;
    for _ in 0..200 {
        let _ = o.poll(now, u64::MAX);
        now = now.saturating_add(1);
    }
}

#[test]
fn reconnect_storm_with_duplicate_delivery_applies_each_write_once_in_order() {
    let mut o = outbox();
    let mut s = Server::default();
    for i in 0..6 {
        o.enqueue(1, Kind::ToolCall, &format!("w{i}")).unwrap();
    }
    persist(&mut o);
    let mut now = 0;
    for round in 0..40 {
        o.connection_lost();
        let Some(b) = o.poll(now, round) else { break };
        let rc = s.append(&b);
        // The server saw the request each time; the receipts arrive on only some rounds, and
        // sometimes twice.
        if round % 3 == 2 {
            let first = o.on_receipts(&rc);
            let second = o.on_receipts(&rc);
            assert_eq!(second.done.len(), 0);
            assert_eq!(second.unknown, rc.len());
            assert_eq!(first.unknown, 0);
        }
        now += 1;
    }
    while let Some(b) = o.poll(now, 0) {
        let rc = s.append(&b);
        o.on_receipts(&rc);
        now += 1;
    }
    assert!(o.is_empty());
    assert_eq!(s.effects, ["w0", "w1", "w2", "w3", "w4", "w5"]);
}

#[test]
fn duplicate_and_foreign_receipts_are_counted_not_applied() {
    let mut o = outbox();
    o.enqueue(1, Kind::Message, "a").unwrap();
    persist(&mut o);
    let b = o.poll(0, 0).unwrap();
    let mut s = Server::default();
    let rc = s.append(&b);
    let foreign = Receipt {
        rid: "other-0".into(),
        status: Status::Recorded,
        pos: Some(9),
    };
    let leading_zero = Receipt {
        rid: "c1-00".into(),
        status: Status::Recorded,
        pos: Some(9),
    };
    let out = o.on_receipts(&[foreign, leading_zero, rc[0].clone(), rc[0].clone()]);
    assert_eq!((out.done, out.unknown), (vec!["c1-0".to_string()], 3));
    assert!(o.is_empty());
    assert_eq!(o.poll(100, 0), None);
}

#[test]
fn a_duplicate_receipt_clears_the_write_like_a_recorded_one() {
    let mut o = outbox();
    o.enqueue(1, Kind::Message, "a").unwrap();
    persist(&mut o);
    o.poll(0, 0).unwrap();
    let dup = Receipt {
        rid: "c1-0".into(),
        status: Status::Duplicate,
        pos: Some(3),
    };
    assert_eq!(o.on_receipts(&[dup]).done, ["c1-0"]);
    assert!(o.is_empty());
    assert_eq!(o.next_deadline(), None);
}

#[test]
fn fenced_is_terminal_for_the_turn_and_surfaced_to_the_application() {
    let mut o = outbox();
    let mut s = Server::default();
    o.enqueue(1, Kind::Message, "t1-a").unwrap();
    o.enqueue(1, Kind::Message, "t1-b").unwrap();
    o.enqueue(2, Kind::Message, "t2-a").unwrap();
    persist(&mut o);
    s.fenced.insert(1);
    let b = o.poll(0, 0).unwrap();
    let out = o.on_receipts(&s.append(&b));
    assert!(out.done.is_empty());
    let dropped: Vec<_> = out.fenced.iter().map(|p| p.body.as_str()).collect();
    assert_eq!(dropped, ["t1-a", "t1-b"]);
    assert_eq!(out.fenced[0].rid, "c1-0");
    assert_eq!(o.fenced_turns().collect::<Vec<_>>(), [1]);
    assert_eq!(
        o.enqueue(1, Kind::Message, "late"),
        Err(OutboxError::TurnFenced(1))
    );
    // Turn 2 is untouched and the fenced writes are never offered again.
    let b = o.poll(0, 0).unwrap();
    assert_eq!((b.turn, rids(&b)), (2, vec!["c1-2"]));
    assert!(s.effects.is_empty());
    // The fenced turn survives a restart.
    let stored = persist(&mut o);
    let mut o = Outbox::from_bytes(&stored, cfg()).unwrap();
    assert_eq!(
        o.enqueue(1, Kind::Message, "late"),
        Err(OutboxError::TurnFenced(1))
    );
    assert_eq!(rids(&o.poll(0, 0).unwrap()), ["c1-2"]);
}

#[test]
fn full_queue_is_an_explicit_error_and_changes_nothing() {
    let mut o = outbox();
    for i in 0..8 {
        o.enqueue(1, Kind::Message, &format!("{i}")).unwrap();
    }
    let before = o.to_bytes();
    assert_eq!(
        o.enqueue(1, Kind::Message, "x"),
        Err(OutboxError::Full { items: 8, bytes: 8 })
    );
    assert_eq!(o.to_bytes(), before);
    // Acking frees room, and the freed id is never reused.
    persist(&mut o);
    let b = o.poll(0, 0).unwrap();
    let mut s = Server::default();
    o.on_receipts(&s.append(&b));
    assert_eq!(o.enqueue(1, Kind::Message, "x").unwrap(), "c1-8");

    let mut o = outbox();
    o.enqueue(1, Kind::Message, &"y".repeat(60)).unwrap();
    assert_eq!(
        o.enqueue(1, Kind::Message, "12345"),
        Err(OutboxError::Full {
            items: 1,
            bytes: 60
        })
    );
    assert_eq!(o.bytes(), 60);
    o.enqueue(1, Kind::Message, "1234").unwrap();
}

#[test]
fn argument_errors() {
    assert_eq!(
        Outbox::new("", cfg()).err(),
        Some(OutboxError::InvalidClientId)
    );
    assert_eq!(
        Outbox::new("a b", cfg()).err(),
        Some(OutboxError::InvalidClientId)
    );
    let bad = OutboxConfig {
        base_delay: 0,
        ..cfg()
    };
    assert_eq!(
        Outbox::new("c", bad).err(),
        Some(OutboxError::InvalidConfig)
    );
    let mut o = outbox();
    assert_eq!(
        o.enqueue(1 << 53, Kind::Note, ""),
        Err(OutboxError::TurnTooLarge)
    );
    let huge = "z".repeat(256 * 1024 + 1);
    assert_eq!(
        o.enqueue(1, Kind::Note, &huge),
        Err(OutboxError::BodyTooLarge)
    );
}

// ---- stored format --------------------------------------------------------------------------

/// Reference CRC-32 (IEEE), written independently of the library: table driven.
fn crc(data: &[u8]) -> u32 {
    let table: Vec<u32> = (0..256u32)
        .map(|n| {
            (0..8).fold(n, |c, _| {
                if c & 1 == 1 {
                    0xEDB8_8320 ^ (c >> 1)
                } else {
                    c >> 1
                }
            })
        })
        .collect();
    !data.iter().fold(!0u32, |c, &b| {
        table[((c ^ u32::from(b)) & 0xFF) as usize] ^ (c >> 8)
    })
}

fn seal(mut body: Vec<u8>) -> Vec<u8> {
    let c = crc(&body);
    body.extend_from_slice(&c.to_le_bytes());
    body
}

struct RawItem<'a> {
    counter: u64,
    turn: u64,
    kind: u8,
    body: &'a [u8],
}

fn raw(id: &str, next: u64, fenced: &[u64], items: &[RawItem<'_>]) -> Vec<u8> {
    let mut b = b"LXOB".to_vec();
    b.push(1);
    b.extend_from_slice(&(id.len() as u16).to_le_bytes());
    b.extend_from_slice(id.as_bytes());
    b.extend_from_slice(&next.to_le_bytes());
    b.extend_from_slice(&(fenced.len() as u16).to_le_bytes());
    for t in fenced {
        b.extend_from_slice(&t.to_le_bytes());
    }
    b.extend_from_slice(&(items.len() as u32).to_le_bytes());
    for i in items {
        b.extend_from_slice(&i.counter.to_le_bytes());
        b.extend_from_slice(&i.turn.to_le_bytes());
        b.push(i.kind);
        b.extend_from_slice(&(i.body.len() as u32).to_le_bytes());
        b.extend_from_slice(i.body);
    }
    seal(b)
}

fn item(counter: u64, turn: u64, kind: u8, body: &[u8]) -> RawItem<'_> {
    RawItem {
        counter,
        turn,
        kind,
        body,
    }
}

#[test]
fn the_stored_form_matches_the_documented_layout() {
    let mut o = outbox();
    o.enqueue(7, Kind::ToolResult, "hi").unwrap();
    o.enqueue(7, Kind::Note, "").unwrap();
    let expected = raw("c1", 2, &[], &[item(0, 7, 2, b"hi"), item(1, 7, 3, b"")]);
    assert_eq!(o.to_bytes(), expected);
    let back = Outbox::from_bytes(&expected, cfg()).unwrap();
    assert_eq!(back.pending(), o.pending());
    assert_eq!(back.next_counter(), 2);
    assert_eq!(back.to_bytes(), expected);
}

#[test]
fn restore_keeps_ids_and_resends_everything_unacked() {
    let mut o = outbox();
    let mut s = Server::default();
    o.enqueue(1, Kind::Message, "a").unwrap();
    o.enqueue(1, Kind::Message, "b").unwrap();
    let stored = persist(&mut o);
    let b = o.poll(0, 0).unwrap();
    s.append(&b); // applied; the receipt never arrives and the process dies
    drop(o);
    let mut o = Outbox::from_bytes(&stored, cfg()).unwrap();
    let b = o.poll(0, 0).unwrap();
    assert_eq!(rids(&b), ["c1-0", "c1-1"]);
    let rc = s.append(&b);
    assert!(rc.iter().all(|r| r.status == Status::Duplicate));
    o.on_receipts(&rc);
    assert!(o.is_empty());
    assert_eq!(s.effects, ["a", "b"]);
    assert_eq!(o.enqueue(1, Kind::Message, "c").unwrap(), "c1-2");
}

#[test]
fn rejects_corrupt_and_non_canonical_bytes() {
    let good = raw(
        "c1",
        3,
        &[4, 9],
        &[item(0, 1, 0, b"x"), item(2, 1, 1, b"yz")],
    );
    assert!(Outbox::from_bytes(&good, cfg()).is_ok());

    // Every single-bit flip is caught (CRC-32 detects all of them).
    for i in 0..good.len() {
        for bit in 0..8 {
            let mut bad = good.clone();
            bad[i] ^= 1 << bit;
            assert!(
                Outbox::from_bytes(&bad, cfg()).is_err(),
                "byte {i} bit {bit}"
            );
        }
    }
    let mut bad = good.clone();
    let last = bad.len() - 1;
    bad[last] ^= 1;
    assert_eq!(
        Outbox::from_bytes(&bad, cfg()).err(),
        Some(DecodeError::BadChecksum)
    );

    // Truncations and growth.
    for n in 0..good.len() {
        assert!(Outbox::from_bytes(&good[..n], cfg()).is_err());
    }
    assert_eq!(
        Outbox::from_bytes(&vec![0; MAX_STORED_BYTES + 1], cfg()).err(),
        Some(DecodeError::TooLarge)
    );

    // Framing problems that carry a valid checksum.
    let mut v2 = good.clone();
    v2[4] = 2;
    assert_eq!(
        Outbox::from_bytes(&v2, cfg()).err(),
        Some(DecodeError::UnsupportedVersion(2))
    );
    let mut magic = good.clone();
    magic[0] = b'X';
    assert_eq!(
        Outbox::from_bytes(&magic, cfg()).err(),
        Some(DecodeError::BadMagic)
    );
    let mut trailing = good[..good.len() - 4].to_vec();
    trailing.push(0);
    assert_eq!(
        Outbox::from_bytes(&seal(trailing), cfg()).err(),
        Some(DecodeError::TrailingBytes)
    );

    let non_canonical = |bytes: Vec<u8>| {
        assert!(
            matches!(
                Outbox::from_bytes(&bytes, cfg()),
                Err(DecodeError::NonCanonical(_))
            ),
            "{bytes:?}"
        );
    };
    non_canonical(raw("c1", 3, &[], &[item(2, 1, 0, b""), item(1, 1, 0, b"")])); // out of order
    non_canonical(raw("c1", 3, &[], &[item(1, 1, 0, b""), item(1, 1, 0, b"")])); // repeated
    non_canonical(raw("c1", 2, &[], &[item(2, 1, 0, b"")])); // counter not below next
    non_canonical(raw("c1", 3, &[5, 5], &[])); // repeated fenced turn
    non_canonical(raw("c1", 3, &[6, 5], &[])); // unsorted fenced turns
    non_canonical(raw("c1", 3, &[1], &[item(0, 1, 0, b"")])); // item on a fenced turn
    non_canonical(raw("c1", 3, &[], &[item(0, 1, 4, b"")])); // unknown kind
    non_canonical(raw("c1", 3, &[], &[item(0, 1, 0, &[0xFF, 0xFE])])); // not UTF-8
    non_canonical(raw("c1", 3, &[], &[item(0, 1 << 53, 0, b"")])); // turn too large
    non_canonical(raw("", 3, &[], &[])); // empty client id
    non_canonical(raw("a b", 3, &[], &[])); // bad character
    non_canonical(raw("c1", u64::MAX, &[], &[])); // counter beyond the wire range

    // Over the limits the caller configured.
    let many: Vec<RawItem<'_>> = (0..9).map(|i| item(i, 1, 0, b"")).collect();
    assert_eq!(
        Outbox::from_bytes(&raw("c1", 9, &[], &many), cfg()).err(),
        Some(DecodeError::OverLimit)
    );
    let fat = [b'a'; 65];
    assert_eq!(
        Outbox::from_bytes(&raw("c1", 1, &[], &[item(0, 1, 0, &fat)]), cfg()).err(),
        Some(DecodeError::OverLimit)
    );
    // An item count that promises more than the data holds cannot make the decoder allocate.
    let mut lie = raw("c1", 3, &[], &[]);
    let at = lie.len() - 8;
    lie[at..at + 4].copy_from_slice(&8u32.to_le_bytes());
    assert!(Outbox::from_bytes(&seal(lie[..lie.len() - 4].to_vec()), cfg()).is_err());
}

proptest! {
    #[test]
    fn decoder_never_panics_and_accepts_only_canonical_input(
        data in proptest::collection::vec(any::<u8>(), 0..300),
    ) {
        if let Ok(o) = Outbox::from_bytes(&data, cfg()) {
            prop_assert_eq!(o.to_bytes(), data);
        }
    }

    #[test]
    fn sealed_garbage_never_panics_and_stays_canonical(
        body in proptest::collection::vec(any::<u8>(), 0..300),
        keep_header in any::<bool>(),
    ) {
        let mut b = if keep_header { b"LXOB\x01".to_vec() } else { Vec::new() };
        b.extend_from_slice(&body);
        let data = seal(b);
        if let Ok(o) = Outbox::from_bytes(&data, cfg()) {
            prop_assert_eq!(o.to_bytes(), data);
        }
    }

    #[test]
    fn a_changed_byte_in_a_stored_queue_is_always_refused(
        bodies in proptest::collection::vec("[a-z]{0,6}", 0..6),
        at in any::<usize>(),
        delta in 1u8..=255,
    ) {
        let mut o = outbox();
        for b in &bodies {
            o.enqueue(1, Kind::Message, b).unwrap();
        }
        let mut bytes = o.to_bytes();
        let i = at % bytes.len();
        bytes[i] ^= delta;
        prop_assert!(Outbox::from_bytes(&bytes, cfg()).is_err());
    }
}

// ---- crash / restart property ---------------------------------------------------------------

#[derive(Debug, Clone)]
enum Fate {
    /// The request never reaches the server.
    Lost,
    /// The server applies it; the receipts are lost.
    ReceiptLost,
    /// The receipts arrive.
    Delivered,
    /// The receipts arrive twice.
    DeliveredTwice,
}

#[derive(Debug, Clone)]
enum Op {
    /// Queue a write; `persist` says whether the application stores the queue right after.
    Enqueue {
        turn: u64,
        persist: bool,
    },
    Persist,
    Poll {
        dt: u64,
        jitter: u64,
        fate: Fate,
    },
    /// Process dies; the queue is restored from the last stored bytes.
    Crash,
    /// Connection drops.
    Disconnect,
    /// The server takes the turn over.
    Fence {
        turn: u64,
    },
}

fn op_strategy() -> impl Strategy<Value = Op> {
    let fate = prop_oneof![
        Just(Fate::Lost),
        Just(Fate::ReceiptLost),
        Just(Fate::Delivered),
        Just(Fate::DeliveredTwice),
    ];
    prop_oneof![
        4 => (0u64..3, any::<bool>()).prop_map(|(turn, persist)| Op::Enqueue { turn, persist }),
        2 => Just(Op::Persist),
        6 => (0u64..20, any::<u64>(), fate).prop_map(|(dt, jitter, fate)| Op::Poll { dt, jitter, fate }),
        3 => Just(Op::Crash),
        1 => Just(Op::Disconnect),
        1 => (0u64..3).prop_map(|turn| Op::Fence { turn }),
    ]
}

/// Run a script and check the delivery guarantees. With `buggy`, a restore re-issues the queued
/// writes under a fresh client id, which is exactly the id instability the design rules out.
fn note_persist(
    o: &mut Outbox,
    stored: &mut Vec<u8>,
    durable: &mut BTreeMap<String, (u64, String)>,
) -> Result<(), String> {
    *stored = persist(o);
    for p in o.pending() {
        let want = (p.turn, p.body.clone());
        if let Some(prev) = durable.insert(p.rid.clone(), want.clone())
            && prev != want
        {
            return Err(format!("rid {} stored for two different writes", p.rid));
        }
    }
    Ok(())
}

fn run(ops: &[Op], buggy: bool) -> Result<(), String> {
    let mut o = Outbox::new("dev", cfg()).map_err(|e| e.to_string())?;
    let mut s = Server::default();
    let mut stored = o.to_bytes();
    let mut now = 0u64;
    let mut written = 0u64;
    let mut restarts = 0u32;
    // rid -> (turn, body) of every write the application was told is stored.
    let mut durable: BTreeMap<String, (u64, String)> = BTreeMap::new();

    let step =
        |o: &mut Outbox, s: &mut Server, now: &mut u64, dt: u64, jitter: u64, fate: &Fate| {
            *now += dt;
            if let Some(b) = o.poll(*now, jitter) {
                match fate {
                    Fate::Lost => {}
                    Fate::ReceiptLost => {
                        s.append(&b);
                    }
                    Fate::Delivered => {
                        let rc = s.append(&b);
                        o.on_receipts(&rc);
                    }
                    Fate::DeliveredTwice => {
                        let rc = s.append(&b);
                        o.on_receipts(&rc);
                        o.on_receipts(&rc);
                    }
                }
            }
        };

    for op in ops {
        match op {
            Op::Enqueue { turn, persist: p } => {
                let body = format!("w{written}");
                written += 1;
                if o.enqueue(*turn, Kind::Message, &body).is_ok() && *p {
                    note_persist(&mut o, &mut stored, &mut durable)?;
                }
            }
            Op::Persist => note_persist(&mut o, &mut stored, &mut durable)?,
            Op::Poll { dt, jitter, fate } => step(&mut o, &mut s, &mut now, *dt, *jitter, fate),
            Op::Disconnect => o.connection_lost(),
            Op::Fence { turn } => {
                s.fenced.insert(*turn);
            }
            Op::Crash => {
                o = Outbox::from_bytes(&stored, cfg()).map_err(|e| e.to_string())?;
                if buggy {
                    restarts += 1;
                    let mut fresh = Outbox::new(&format!("dev-r{restarts}"), cfg())
                        .map_err(|e| e.to_string())?;
                    for p in o.pending() {
                        let _ = fresh.enqueue(p.turn, p.kind, &p.body);
                    }
                    o = fresh;
                    stored = persist(&mut o);
                }
            }
        }
    }

    // Drain: connection healthy, everything persisted.
    let mut guard = 0;
    note_persist(&mut o, &mut stored, &mut durable)?;
    while !o.is_empty() {
        guard += 1;
        if guard > 2000 {
            return Err("outbox did not drain".into());
        }
        step(&mut o, &mut s, &mut now, 100, 0, &Fate::Delivered);
    }

    // No doubled effect: each write body was applied at most once, whatever its id.
    let mut seen = BTreeSet::new();
    for e in &s.effects {
        if !seen.insert(e.clone()) {
            return Err(format!("write {e} applied twice"));
        }
    }
    // No lost write: everything the application stored is applied, or its turn was fenced.
    let applied: BTreeSet<&String> = s.effects.iter().collect();
    let durable_bodies: BTreeSet<&String> = durable.values().map(|(_, b)| b).collect();
    for (rid, (turn, body)) in &durable {
        if !applied.contains(body) && !s.fenced.contains(turn) {
            return Err(format!("write {body} ({rid}) was lost"));
        }
    }
    // Nothing reached the server that the application had not stored.
    for e in &applied {
        if !durable_bodies.contains(e) {
            return Err(format!("write {e} reached the server without being stored"));
        }
    }
    // The server never applied one rid for two bodies.
    for (rid, (_, body)) in &s.stored {
        if let Some((_, want)) = durable.get(rid)
            && want != body
        {
            return Err(format!("rid {rid} applied with the wrong body"));
        }
    }
    Ok(())
}

proptest! {
    #[test]
    fn crashes_and_restarts_neither_lose_nor_double_a_write(
        ops in proptest::collection::vec(op_strategy(), 0..60),
    ) {
        prop_assert_eq!(run(&ops, false), Ok(()));
    }
}

fn losing_script() -> Vec<Op> {
    vec![
        Op::Enqueue {
            turn: 1,
            persist: true,
        },
        Op::Poll {
            dt: 0,
            jitter: 0,
            fate: Fate::ReceiptLost,
        },
        Op::Crash,
    ]
}

#[test]
fn the_property_fails_for_an_outbox_that_changes_ids_on_restore() {
    assert_eq!(run(&losing_script(), false), Ok(()));
    let err = run(&losing_script(), true).unwrap_err();
    assert!(err.contains("applied twice"), "{err}");
}

#[test]
fn the_property_fails_without_the_persist_barrier() {
    // Model of an outbox that sends before the queue is stored: the counter is reused after
    // the crash, so the server's dedupe swallows a different write.
    let mut s = Server::default();
    let mut o = outbox();
    let stored = o.to_bytes();
    o.enqueue(1, Kind::Message, "first").unwrap();
    // The faulty application marks the write sendable without storing it.
    o.mark_persisted(o.next_counter());
    s.append(&o.poll(0, 0).unwrap());
    let mut o = Outbox::from_bytes(&stored, cfg()).unwrap();
    o.enqueue(1, Kind::Message, "second").unwrap();
    persist(&mut o);
    let rc = s.append(&o.poll(0, 0).unwrap());
    // The server calls "second" a duplicate and never applies it: a lost write.
    assert_eq!(rc[0].status, Status::Duplicate);
    assert_eq!(s.effects, ["first"]);
}

#[test]
fn fuzz_seeds_decode_as_their_names_say() {
    macro_rules! seed {
        ($n:literal) => {
            (
                $n,
                &include_bytes!(concat!("../../../fuzz/seeds/outbox_decode/", $n))[..],
            )
        };
    }
    let seeds: [(&str, &[u8]); 15] = [
        seed!("empty"),
        seed!("one-item"),
        seed!("all-kinds"),
        seed!("fenced-turns"),
        seed!("sparse-counters"),
        seed!("bad-checksum"),
        seed!("truncated"),
        seed!("bad-version"),
        seed!("bad-magic"),
        seed!("trailing-byte"),
        seed!("unsorted-counters"),
        seed!("counter-beyond-next"),
        seed!("item-on-fenced-turn"),
        seed!("unknown-kind"),
        seed!("invalid-utf8"),
    ];
    let valid = [
        "empty",
        "one-item",
        "all-kinds",
        "fenced-turns",
        "sparse-counters",
    ];
    for (name, bytes) in seeds {
        let got = Outbox::from_bytes(bytes, OutboxConfig::default());
        assert_eq!(got.is_ok(), valid.contains(&name), "{name}");
        if let Ok(o) = got {
            assert_eq!(o.to_bytes(), bytes, "{name} is canonical");
        }
    }
}

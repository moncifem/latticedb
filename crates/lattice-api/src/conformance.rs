//! Cross-language conformance vectors for the sync protocol and the change feed.
//!
//! `conformance/vectors.json` is the contract between this Rust implementation and the clients in
//! other languages (today `clients/typescript`). It is written from this module: every vector
//! carries bytes written by hand from the format documents (not produced by the encoder), the
//! test checks the Rust codecs against them, and then renders the file. The test fails when the
//! checked-in file differs from what is rendered here, so a Rust change that moves a byte cannot
//! pass until the vectors, and with them the other clients, are updated.
//!
//! To rewrite the file after a deliberate change, run `tools/update-conformance-vectors.sh`
//! (it sets `LATTICE_UPDATE_VECTORS=1` and runs the test, which then prints the new file). The
//! determinism lints forbid this crate to touch the file system, so the script, not the test,
//! writes it.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::print_stdout,
    clippy::vec_init_then_push
)]

use crate::sync::{
    self, ErrorCode, Event, MAX_PAYLOAD, Message, SyncError, decode, decode_with_limit, encode,
};
use lattice_abi::Hlc;
use lattice_core::changeset::{ChangeSetId, TurnRef};
use lattice_core::feed::{
    ChangeEvent, ChangeOp, Cursor, EntityChange, FeedDecodeError, FeedError, MAX_KEY_LEN,
    MAX_VALUE_LEN,
};
use std::fmt::Write as _;

const CHECKED_IN: &str = include_str!("../../../conformance/vectors.json");
const UPDATE: Option<&str> = option_env!("LATTICE_UPDATE_VECTORS");

// ---- hand-assembly helpers (deliberately not the encoder) ---------------------------------

/// Hex with optional spaces and `|` separators.
fn h(text: &str) -> Vec<u8> {
    let digits: Vec<u8> = text
        .bytes()
        .filter(|b| !b.is_ascii_whitespace() && *b != b'|')
        .collect();
    assert!(digits.len().is_multiple_of(2), "odd hex: {text}");
    digits
        .chunks(2)
        .map(|p| u8::from_str_radix(std::str::from_utf8(p).unwrap(), 16).unwrap())
        .collect()
}

fn hex(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        write!(s, "{x:02x}").unwrap();
    }
    s
}

fn cat(parts: &[&[u8]]) -> Vec<u8> {
    parts.concat()
}

fn be16(n: u16) -> Vec<u8> {
    n.to_be_bytes().to_vec()
}

fn be32(n: u32) -> Vec<u8> {
    n.to_be_bytes().to_vec()
}

/// type byte, four-byte big-endian length, payload.
fn frame(ty: u8, payload: &[u8]) -> Vec<u8> {
    cat(&[&[ty], &be32(payload.len() as u32), payload])
}

/// A text or cursor field: two-byte length and the bytes.
fn field(b: &[u8]) -> Vec<u8> {
    cat(&[&be16(b.len() as u16), b])
}

// ---- JSON rendering -------------------------------------------------------------------------

/// A quoted string using only ASCII: everything else is a `\uXXXX` escape.
fn q(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            ' '..='~' => out.push(c),
            _ => {
                let mut units = [0u16; 2];
                for u in c.encode_utf16(&mut units) {
                    write!(out, "\\u{u:04x}").unwrap();
                }
            }
        }
    }
    out.push('"');
    out
}

/// Bytes as hex, or as `{"fill":"ab","len":N}` for a long run of one value, to keep the file small.
fn bytes_json(b: &[u8]) -> String {
    if b.len() > 1024 && b.iter().all(|x| *x == b[0]) {
        return format!("{{\"fill\":\"{:02x}\",\"len\":{}}}", b[0], b.len());
    }
    format!("\"{}\"", hex(b))
}

fn code_name(c: ErrorCode) -> &'static str {
    match c {
        ErrorCode::UnsupportedVersion => "unsupported_version",
        ErrorCode::Malformed => "malformed",
        ErrorCode::UnknownSubscription => "unknown_subscription",
        ErrorCode::InvalidScope => "invalid_scope",
        ErrorCode::InvalidCursor => "invalid_cursor",
        ErrorCode::TooManySubscriptions => "too_many_subscriptions",
        ErrorCode::Internal => "internal",
    }
}

fn message_json(m: &Message) -> String {
    match m {
        Message::Hello {
            min_version,
            max_version,
            agent,
        } => format!(
            "{{\"type\":\"hello\",\"min_version\":{min_version},\"max_version\":{max_version},\"agent\":{}}}",
            q(agent)
        ),
        Message::Welcome {
            version,
            max_payload,
            server,
        } => format!(
            "{{\"type\":\"welcome\",\"version\":{version},\"max_payload\":{max_payload},\"server\":{}}}",
            q(server)
        ),
        Message::Subscribe { sub, scope, cursor } => format!(
            "{{\"type\":\"subscribe\",\"sub\":{sub},\"scope\":{},\"cursor\":{}}}",
            q(scope),
            bytes_json(cursor)
        ),
        Message::Events { sub, events } => {
            let evs: Vec<String> = events
                .iter()
                .map(|e| {
                    format!(
                        "{{\"cursor\":{},\"data\":{}}}",
                        bytes_json(&e.cursor),
                        bytes_json(&e.data)
                    )
                })
                .collect();
            format!(
                "{{\"type\":\"events\",\"sub\":{sub},\"events\":[{}]}}",
                evs.join(",")
            )
        }
        Message::Ack { sub, cursor } => format!(
            "{{\"type\":\"ack\",\"sub\":{sub},\"cursor\":{}}}",
            bytes_json(cursor)
        ),
        Message::Error { code, sub, message } => format!(
            "{{\"type\":\"error\",\"code\":\"{}\",\"sub\":{sub},\"message\":{}}}",
            code_name(*code),
            q(message)
        ),
        // u64 values are decimal strings: JSON numbers are doubles.
        Message::Ping { nonce } => format!("{{\"type\":\"ping\",\"nonce\":\"{nonce}\"}}"),
        Message::Pong { nonce } => format!("{{\"type\":\"pong\",\"nonce\":\"{nonce}\"}}"),
    }
}

fn sync_error_json(e: &SyncError) -> String {
    match e {
        SyncError::BadLength(n) => format!("{{\"kind\":\"bad_length\",\"value\":{n}}}"),
        SyncError::UnknownType(t) => format!("{{\"kind\":\"unknown_type\",\"value\":{t}}}"),
        SyncError::Truncated => "{\"kind\":\"truncated\"}".into(),
        SyncError::Trailing => "{\"kind\":\"trailing\"}".into(),
        SyncError::NotUtf8 => "{\"kind\":\"not_utf8\"}".into(),
        SyncError::TooLarge => "{\"kind\":\"too_large\"}".into(),
        SyncError::InvalidField(f) => format!("{{\"kind\":\"invalid_field\",\"field\":{}}}", q(f)),
    }
}

fn decode_error_json(e: &FeedDecodeError) -> String {
    match e {
        FeedDecodeError::Truncated => "{\"kind\":\"truncated\"}".into(),
        FeedDecodeError::Version(v) => format!("{{\"kind\":\"version\",\"value\":{v}}}"),
        FeedDecodeError::Trailing => "{\"kind\":\"trailing\"}".into(),
        FeedDecodeError::Invalid(f) => format!("{{\"kind\":\"invalid\",\"field\":{}}}", q(f)),
    }
}

fn feed_error_json(e: &FeedError) -> String {
    let kind = match e {
        FeedError::ZeroLsn => "zero_lsn",
        FeedError::IndexOutOfRange { .. } => "index_out_of_range",
        FeedError::KeyTooLarge(_) => "key_too_large",
        FeedError::ValueTooLarge(_) => "value_too_large",
        FeedError::TooManyChanges(_) => panic!("not a vector"),
    };
    format!("{{\"kind\":\"{kind}\"}}")
}

fn cursor_json(c: Cursor) -> String {
    format!("{{\"lsn\":\"{}\",\"index\":{}}}", c.lsn(), c.index())
}

fn event_json(e: &ChangeEvent) -> String {
    let turn = match e.turn {
        None => "null".to_string(),
        Some(t) => format!("{{\"session\":\"{}\",\"turn\":\"{}\"}}", t.session, t.turn),
    };
    let op = match &e.change.op {
        ChangeOp::Delete => "{\"type\":\"delete\"}".to_string(),
        ChangeOp::Upsert(v) => format!("{{\"type\":\"upsert\",\"value\":{}}}", bytes_json(v)),
    };
    format!(
        "{{\"cursor\":{},\"set_len\":{},\"change_set\":\"{}\",\"hlc\":{{\"wall_ms\":\"{}\",\"counter\":{},\"node\":{}}},\"author\":\"{}\",\"turn\":{turn},\"kind\":{},\"key\":{},\"op\":{op}}}",
        cursor_json(e.cursor),
        e.set_len,
        hex(&e.change_set.to_bytes()),
        e.hlc.wall_ms,
        e.hlc.counter,
        e.hlc.node,
        e.author,
        e.change.kind,
        bytes_json(&e.change.key),
    )
}

// ---- sync vectors ---------------------------------------------------------------------------

fn hello(min: u16, max: u16, agent: &str) -> Message {
    Message::Hello {
        min_version: min,
        max_version: max,
        agent: agent.into(),
    }
}

fn subscribe(sub: u32, scope: &str, cursor: &[u8]) -> Message {
    Message::Subscribe {
        sub,
        scope: scope.into(),
        cursor: cursor.to_vec(),
    }
}

fn error_msg(code: ErrorCode, sub: u32, message: &str) -> Message {
    Message::Error {
        code,
        sub,
        message: message.into(),
    }
}

/// Valid frames: the name, the message and the bytes written by hand.
fn sync_valid() -> Vec<(String, Message, Vec<u8>)> {
    let mut v: Vec<(String, Message, Vec<u8>)> = Vec::new();
    let mut add = |name: &str, m: Message, bytes: Vec<u8>| v.push((name.to_string(), m, bytes));

    // The worked examples of formats/sync-protocol-v1.md, bytes copied from the document.
    add(
        "doc_1_hello_1_to_1",
        hello(1, 1, "sdk-rs"),
        h("01 0000000c | 0001 0001 0006 73646b2d7273"),
    );
    add(
        "doc_2_hello_1_to_3",
        hello(1, 3, "a"),
        h("01 00000007 | 0001 0003 0001 61"),
    );
    add(
        "doc_3_welcome",
        Message::Welcome {
            version: 1,
            max_payload: 1_048_576,
            server: "lattice".into(),
        },
        h("02 0000000f | 0001 00100000 0007 6c6174746963 65"),
    );
    add(
        "doc_4_subscribe_from_start",
        subscribe(1, "ws/main", &[]),
        h("03 0000000f | 00000001 0007 77732f6d61696e 0000"),
    );
    add(
        "doc_5_subscribe_with_cursor",
        subscribe(2, "ws/main", &[1, 2, 3, 4, 5, 6, 7, 8]),
        h("03 00000017 | 00000002 0007 77732f6d61696e 0008 0102030405060708"),
    );
    add(
        "doc_6_events_two",
        Message::Events {
            sub: 1,
            events: vec![
                Event {
                    cursor: vec![0, 1],
                    data: vec![0x61],
                },
                Event {
                    cursor: vec![0, 2],
                    data: vec![],
                },
            ],
        },
        h("04 00000017 | 00000001 0002 | 0002 0001 00000001 61 | 0002 0002 00000000"),
    );
    add(
        "doc_7_ack",
        Message::Ack {
            sub: 1,
            cursor: vec![0, 2],
        },
        h("05 00000008 | 00000001 0002 0002"),
    );
    add(
        "doc_8_error_unsupported_version",
        error_msg(ErrorCode::UnsupportedVersion, 0, "version"),
        h("06 0000000f | 0001 00000000 0007 76657273696f6e"),
    );
    add(
        "doc_9_error_unknown_subscription",
        error_msg(ErrorCode::UnknownSubscription, 9, ""),
        h("06 00000008 | 0003 00000009 0000"),
    );
    add(
        "doc_10_ping",
        Message::Ping { nonce: 1 },
        h("07 00000008 | 0000000000000001"),
    );
    add(
        "doc_11_pong",
        Message::Pong {
            nonce: 0x0102_0304_0506_0708,
        },
        h("08 00000008 | 0102030405060708"),
    );

    // Boundaries of every field.
    let a64 = "a".repeat(64);
    add(
        "hello_widest_range_longest_name",
        hello(1, 65_535, &a64),
        frame(1, &cat(&[&h("0001 ffff"), &field(a64.as_bytes())])),
    );
    add(
        "hello_min_equals_max_high",
        hello(65_535, 65_535, "x"),
        frame(1, &cat(&[&h("ffff ffff"), &field(b"x")])),
    );
    add(
        "welcome_smallest_payload_limit",
        Message::Welcome {
            version: 1,
            max_payload: 1024,
            server: "s".into(),
        },
        frame(2, &cat(&[&h("0001 00000400"), &field(b"s")])),
    );
    let x512 = "x".repeat(512);
    add(
        "subscribe_longest_scope_largest_sub",
        subscribe(u32::MAX, &x512, &[]),
        frame(
            3,
            &cat(&[&h("ffffffff"), &field(x512.as_bytes()), &field(&[])]),
        ),
    );
    let cur64: Vec<u8> = (0..64).collect();
    add(
        "subscribe_longest_cursor",
        subscribe(3, "s", &cur64),
        frame(3, &cat(&[&h("00000003"), &field(b"s"), &field(&cur64)])),
    );
    add(
        "ack_longest_cursor",
        Message::Ack {
            sub: 7,
            cursor: cur64.clone(),
        },
        frame(5, &cat(&[&h("00000007"), &field(&cur64)])),
    );
    add(
        "ack_one_byte_cursor",
        Message::Ack {
            sub: u32::MAX,
            cursor: vec![0],
        },
        frame(5, &cat(&[&h("ffffffff"), &field(&[0])])),
    );
    let many: Vec<Event> = (0u16..256)
        .map(|i| Event {
            cursor: i.to_be_bytes().to_vec(),
            data: vec![i as u8],
        })
        .collect();
    let mut many_payload = h("00000005 0100");
    for i in 0u16..256 {
        many_payload.extend(field(&i.to_be_bytes()));
        many_payload.extend(h("00000001"));
        many_payload.push(i as u8);
    }
    add(
        "events_maximum_count",
        Message::Events {
            sub: 5,
            events: many,
        },
        frame(4, &many_payload),
    );
    add(
        "events_longest_cursor_empty_data",
        Message::Events {
            sub: 2,
            events: vec![Event {
                cursor: cur64.clone(),
                data: vec![],
            }],
        },
        frame(
            4,
            &cat(&[&h("00000002 0001"), &field(&cur64), &h("00000000")]),
        ),
    );
    // An Events frame carrying a change feed event as its data, the way a server sends them.
    let feed_event = h(
        "01 | 0000000000000102 00000001 | 03000000 | a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0 \
        | 0500000000000000 06000000 07000000 | 00000000000000000000000000000009 | 00 | 02000000 \
        | 0100 6b | 01 02000000 7676",
    );
    let feed_cursor = h("0000000000000102 00000001");
    add(
        "events_carrying_a_feed_event",
        Message::Events {
            sub: 1,
            events: vec![Event {
                cursor: feed_cursor.clone(),
                data: feed_event.clone(),
            }],
        },
        frame(
            4,
            &cat(&[
                &h("00000001 0001"),
                &field(&feed_cursor),
                &be32(feed_event.len() as u32),
                &feed_event,
            ]),
        ),
    );
    for (i, code) in [
        ErrorCode::UnsupportedVersion,
        ErrorCode::Malformed,
        ErrorCode::UnknownSubscription,
        ErrorCode::InvalidScope,
        ErrorCode::InvalidCursor,
        ErrorCode::TooManySubscriptions,
        ErrorCode::Internal,
    ]
    .into_iter()
    .enumerate()
    {
        let n = (i + 1) as u16;
        add(
            &format!("error_code_{n}_{}", code_name(code)),
            error_msg(code, 4_294_967_295, "m"),
            frame(6, &cat(&[&be16(n), &h("ffffffff"), &field(b"m")])),
        );
    }
    let e256 = "e".repeat(256);
    add(
        "error_longest_message",
        error_msg(ErrorCode::Internal, 0, &e256),
        frame(6, &cat(&[&h("0007 00000000"), &field(e256.as_bytes())])),
    );
    add(
        "ping_zero",
        Message::Ping { nonce: 0 },
        h("07 00000008 | 0000000000000000"),
    );
    add(
        "ping_largest_nonce",
        Message::Ping { nonce: u64::MAX },
        h("07 00000008 | ffffffffffffffff"),
    );
    add(
        "pong_above_2_pow_53",
        Message::Pong {
            nonce: 9_007_199_254_740_993,
        },
        h("08 00000008 | 0020000000000001"),
    );

    // Text: UTF-8 of every width, a byte order mark kept as data, and non-control characters that
    // look like controls.
    add(
        "hello_multibyte_text",
        hello(1, 1, "\u{e9}\u{6f22}\u{1f600}"),
        frame(
            1,
            &cat(&[&h("0001 0001"), &field(&h("c3a9 e6bca2 f09f9880"))]),
        ),
    );
    add(
        "hello_byte_order_mark_is_data",
        hello(1, 1, "\u{feff}a"),
        frame(1, &cat(&[&h("0001 0001"), &field(&h("efbbbf 61"))])),
    );
    add(
        "hello_no_break_space_and_line_separator",
        hello(1, 1, "a\u{a0}b\u{2028}c"),
        frame(
            1,
            &cat(&[&h("0001 0001"), &field(&h("61 c2a0 62 e280a8 63"))]),
        ),
    );
    add(
        "hello_noncharacter",
        hello(1, 1, "\u{fffe}\u{10ffff}"),
        frame(1, &cat(&[&h("0001 0001"), &field(&h("efbfbe f48fbfbf"))])),
    );
    add(
        "subscribe_scope_with_tilde_and_space",
        subscribe(1, "a ~b", &[]),
        frame(3, &cat(&[&h("00000001"), &field(b"a ~b"), &field(&[])])),
    );
    v
}

fn long_text(n: usize) -> Vec<u8> {
    vec![b'a'; n]
}

/// Frames the decoder must refuse: the name, the bytes, an optional payload limit and the error.
fn sync_invalid() -> Vec<(String, Vec<u8>, Option<usize>, SyncError)> {
    let mut v: Vec<(String, Vec<u8>, Option<usize>, SyncError)> = Vec::new();
    let mut add = |name: &str, bytes: Vec<u8>, limit: Option<usize>, e: SyncError| {
        v.push((name.to_string(), bytes, limit, e));
    };
    let hello_ok = h("0001 0001 0006 73646b2d7273");
    let field_err = |what: &'static str| SyncError::InvalidField(what);

    // Header.
    add(
        "doc_13_unknown_type_9",
        h("09 00000000"),
        None,
        SyncError::UnknownType(9),
    );
    add(
        "unknown_type_0",
        h("00 00000000"),
        None,
        SyncError::UnknownType(0),
    );
    add(
        "unknown_type_ff_one_byte",
        h("ff"),
        None,
        SyncError::UnknownType(255),
    );
    add(
        "unknown_type_9_one_byte",
        h("09"),
        None,
        SyncError::UnknownType(9),
    );
    add(
        "doc_14_length_one_above_limit",
        h("07 00100001"),
        None,
        SyncError::BadLength(1_048_577),
    );
    add(
        "length_u32_max",
        h("07 ffffffff"),
        None,
        SyncError::BadLength(u32::MAX),
    );
    add(
        "length_above_lower_limit",
        h("07 000007d0"),
        Some(1024),
        SyncError::BadLength(2000),
    );
    add(
        "length_above_clamped_limit",
        h("07 00100001"),
        Some(1 << 30),
        SyncError::BadLength(1_048_577),
    );
    // Trailing and truncated payloads.
    add(
        "doc_12_hello_trailing_byte",
        cat(&[&h("01 0000000d"), &hello_ok, &[0]]),
        None,
        SyncError::Trailing,
    );
    add(
        "ping_payload_nine_bytes",
        h("07 00000009 | 0000000000000001 00"),
        None,
        SyncError::Trailing,
    );
    add(
        "pong_payload_seven_bytes",
        h("08 00000007 | 00000000000001"),
        None,
        SyncError::Truncated,
    );
    add(
        "hello_payload_two_bytes",
        h("01 00000002 | 0001"),
        None,
        SyncError::Truncated,
    );
    add(
        "ack_without_cursor",
        h("05 00000004 | 00000001"),
        None,
        SyncError::Truncated,
    );
    add(
        "empty_payload_for_error",
        h("06 00000000"),
        None,
        SyncError::Truncated,
    );
    // Hello and Welcome.
    add(
        "doc_15_hello_min_above_max",
        h("01 00000007 | 0002 0001 0001 61"),
        None,
        field_err("version range"),
    );
    add(
        "hello_min_zero",
        h("01 00000007 | 0000 0001 0001 61"),
        None,
        field_err("version range"),
    );
    add(
        "hello_empty_agent",
        h("01 00000006 | 0001 0001 0000"),
        None,
        field_err("agent"),
    );
    add(
        "hello_agent_65_bytes",
        frame(1, &cat(&[&h("0001 0001"), &field(&long_text(65))])),
        None,
        field_err("agent"),
    );
    add(
        "hello_agent_length_65_without_bytes",
        frame(1, &h("0001 0001 0041")),
        None,
        field_err("agent"),
    );
    add(
        "hello_agent_length_beyond_payload",
        h("01 00000008 | 0001 0001 0006 6162"),
        None,
        SyncError::Truncated,
    );
    add(
        "hello_agent_with_newline",
        h("01 00000007 | 0001 0001 0001 0a"),
        None,
        field_err("agent"),
    );
    add(
        "hello_agent_with_del",
        h("01 00000007 | 0001 0001 0001 7f"),
        None,
        field_err("agent"),
    );
    add(
        "hello_agent_with_c1_control",
        h("01 00000008 | 0001 0001 0002 c285"),
        None,
        field_err("agent"),
    );
    add(
        "hello_agent_with_last_c1_control",
        h("01 00000008 | 0001 0001 0002 c29f"),
        None,
        field_err("agent"),
    );
    add(
        "hello_agent_invalid_byte_ff",
        h("01 00000007 | 0001 0001 0001 ff"),
        None,
        SyncError::NotUtf8,
    );
    add(
        "hello_agent_overlong_nul",
        h("01 00000008 | 0001 0001 0002 c080"),
        None,
        SyncError::NotUtf8,
    );
    add(
        "hello_agent_encoded_surrogate",
        h("01 00000009 | 0001 0001 0003 eda080"),
        None,
        SyncError::NotUtf8,
    );
    add(
        "hello_agent_above_max_scalar",
        h("01 0000000a | 0001 0001 0004 f4908080"),
        None,
        SyncError::NotUtf8,
    );
    add(
        "hello_agent_cut_sequence",
        h("01 00000008 | 0001 0001 0002 e6bc"),
        None,
        SyncError::NotUtf8,
    );
    add(
        "hello_agent_stray_continuation",
        h("01 00000007 | 0001 0001 0001 80"),
        None,
        SyncError::NotUtf8,
    );
    add(
        "doc_16_welcome_version_2",
        h("02 00000009 | 0002 00100000 0001 78"),
        None,
        field_err("version"),
    );
    add(
        "welcome_version_0",
        h("02 00000009 | 0000 00100000 0001 78"),
        None,
        field_err("version"),
    );
    add(
        "welcome_max_payload_1023",
        h("02 00000009 | 0001 000003ff 0001 78"),
        None,
        field_err("max payload"),
    );
    add(
        "welcome_max_payload_1048577",
        h("02 00000009 | 0001 00100001 0001 78"),
        None,
        field_err("max payload"),
    );
    add(
        "welcome_max_payload_u32_max",
        h("02 00000009 | 0001 ffffffff 0001 78"),
        None,
        field_err("max payload"),
    );
    add(
        "welcome_empty_server",
        h("02 00000008 | 0001 00100000 0000"),
        None,
        field_err("server"),
    );
    // Subscribe.
    add(
        "subscribe_sub_zero",
        h("03 00000009 | 00000000 0001 78 0000"),
        None,
        field_err("subscription id"),
    );
    add(
        "subscribe_empty_scope",
        h("03 00000008 | 00000001 0000 0000"),
        None,
        field_err("scope"),
    );
    add(
        "subscribe_scope_513_bytes",
        frame(
            3,
            &cat(&[&h("00000001"), &field(&long_text(513)), &field(&[])]),
        ),
        None,
        field_err("scope"),
    );
    add(
        "subscribe_scope_with_tab",
        h("03 00000009 | 00000001 0001 09 0000"),
        None,
        field_err("scope"),
    );
    add(
        "subscribe_cursor_65_bytes",
        frame(
            3,
            &cat(&[&h("00000001"), &field(b"s"), &field(&long_text(65))]),
        ),
        None,
        field_err("cursor"),
    );
    add(
        "subscribe_scope_not_utf8",
        h("03 00000009 | 00000001 0001 ff 0000"),
        None,
        SyncError::NotUtf8,
    );
    add(
        "subscribe_missing_cursor",
        h("03 00000007 | 00000001 0001 78"),
        None,
        SyncError::Truncated,
    );
    // Events.
    add(
        "events_sub_zero",
        h("04 0000000d | 00000000 0001 0001 01 00000000"),
        None,
        field_err("subscription id"),
    );
    add(
        "events_count_zero",
        h("04 00000006 | 00000001 0000"),
        None,
        field_err("event count"),
    );
    add(
        "events_count_257_declared",
        h("04 00000006 | 00000001 0101"),
        None,
        field_err("event count"),
    );
    add(
        "events_event_cursor_empty",
        h("04 0000000c | 00000001 0001 0000 00000000"),
        None,
        field_err("cursor"),
    );
    add(
        "events_event_cursor_65_bytes",
        frame(
            4,
            &cat(&[&h("00000001 0001"), &field(&long_text(65)), &h("00000000")]),
        ),
        None,
        field_err("cursor"),
    );
    add(
        "events_data_length_beyond_payload",
        h("04 0000000e | 00000001 0001 0001 01 00000005 61"),
        None,
        SyncError::Truncated,
    );
    add(
        "events_second_event_missing",
        h("04 0000000d | 00000001 0002 0001 01 00000000"),
        None,
        SyncError::Truncated,
    );
    add(
        "events_trailing_byte",
        h("04 0000000e | 00000001 0001 0001 01 00000000 00"),
        None,
        SyncError::Trailing,
    );
    // Ack.
    add(
        "doc_17_ack_empty_cursor",
        h("05 00000006 | 00000001 0000"),
        None,
        field_err("cursor"),
    );
    add(
        "ack_sub_zero",
        h("05 00000008 | 00000000 0002 0002"),
        None,
        field_err("subscription id"),
    );
    add(
        "ack_cursor_65_bytes",
        frame(5, &cat(&[&h("00000001"), &field(&long_text(65))])),
        None,
        field_err("cursor"),
    );
    // Error.
    add(
        "doc_18_error_code_8",
        h("06 00000008 | 0008 00000000 0000"),
        None,
        field_err("error code"),
    );
    add(
        "error_code_0",
        h("06 00000008 | 0000 00000000 0000"),
        None,
        field_err("error code"),
    );
    add(
        "error_code_65535",
        h("06 00000008 | ffff 00000000 0000"),
        None,
        field_err("error code"),
    );
    add(
        "error_message_257_bytes",
        frame(6, &cat(&[&h("0002 00000000"), &field(&long_text(257))])),
        None,
        field_err("message"),
    );
    add(
        "error_message_with_nul",
        h("06 00000009 | 0002 00000000 0001 00"),
        None,
        field_err("message"),
    );
    add(
        "error_message_truncated",
        h("06 00000009 | 0002 00000000 0004 61"),
        None,
        SyncError::Truncated,
    );
    v
}

/// Byte strings that are a proper prefix of a frame: the reader must wait for more.
fn sync_incomplete() -> Vec<(String, Vec<u8>)> {
    vec![
        ("empty".into(), vec![]),
        ("type_only".into(), h("01")),
        ("header_missing_last_byte".into(), h("01 000000")),
        (
            "doc_19_ping_cut_after_seven_bytes".into(),
            h("07 00000008 00 00"),
        ),
        ("header_only_for_nonempty_payload".into(), h("08 00000008")),
        (
            "hello_missing_last_byte".into(),
            h("01 0000000c | 0001 0001 0006 73646b2d72"),
        ),
        ("largest_allowed_length_header".into(), h("07 00100000")),
    ]
}

/// Messages the encoder must refuse.
fn sync_encode_errors() -> Vec<(String, Message, SyncError)> {
    let mut v: Vec<(String, Message, SyncError)> = Vec::new();
    let mut add = |name: &str, m: Message, e: SyncError| v.push((name.to_string(), m, e));
    let event = |cursor: &[u8], data: &[u8]| Event {
        cursor: cursor.to_vec(),
        data: data.to_vec(),
    };
    add(
        "hello_min_zero",
        hello(0, 1, "a"),
        SyncError::InvalidField("version range"),
    );
    add(
        "hello_min_above_max",
        hello(2, 1, "a"),
        SyncError::InvalidField("version range"),
    );
    add(
        "hello_empty_agent",
        hello(1, 1, ""),
        SyncError::InvalidField("agent"),
    );
    add(
        "hello_agent_65_bytes",
        hello(1, 1, &"a".repeat(65)),
        SyncError::InvalidField("agent"),
    );
    add(
        "hello_agent_66_bytes_of_three_byte_chars",
        hello(1, 1, &"\u{6f22}".repeat(22)),
        SyncError::InvalidField("agent"),
    );
    add(
        "hello_agent_newline",
        hello(1, 1, "a\nb"),
        SyncError::InvalidField("agent"),
    );
    add(
        "hello_agent_c1_control",
        hello(1, 1, "a\u{85}"),
        SyncError::InvalidField("agent"),
    );
    add(
        "welcome_version_2",
        Message::Welcome {
            version: 2,
            max_payload: 1024,
            server: "s".into(),
        },
        SyncError::InvalidField("version"),
    );
    add(
        "welcome_max_payload_1023",
        Message::Welcome {
            version: 1,
            max_payload: 1023,
            server: "s".into(),
        },
        SyncError::InvalidField("max payload"),
    );
    add(
        "welcome_max_payload_1048577",
        Message::Welcome {
            version: 1,
            max_payload: 1_048_577,
            server: "s".into(),
        },
        SyncError::InvalidField("max payload"),
    );
    add(
        "subscribe_sub_zero",
        subscribe(0, "s", &[]),
        SyncError::InvalidField("subscription id"),
    );
    add(
        "subscribe_empty_scope",
        subscribe(1, "", &[]),
        SyncError::InvalidField("scope"),
    );
    add(
        "subscribe_scope_513_bytes",
        subscribe(1, &"x".repeat(513), &[]),
        SyncError::InvalidField("scope"),
    );
    add(
        "subscribe_scope_tab",
        subscribe(1, "a\tb", &[]),
        SyncError::InvalidField("scope"),
    );
    add(
        "subscribe_cursor_65_bytes",
        subscribe(1, "s", &[0; 65]),
        SyncError::InvalidField("cursor"),
    );
    add(
        "events_sub_zero",
        Message::Events {
            sub: 0,
            events: vec![event(&[1], &[])],
        },
        SyncError::InvalidField("subscription id"),
    );
    add(
        "events_none",
        Message::Events {
            sub: 1,
            events: vec![],
        },
        SyncError::InvalidField("event count"),
    );
    add(
        "events_257",
        Message::Events {
            sub: 1,
            events: (0..257).map(|_| event(&[1], &[])).collect(),
        },
        SyncError::InvalidField("event count"),
    );
    add(
        "events_empty_event_cursor",
        Message::Events {
            sub: 1,
            events: vec![event(&[], &[])],
        },
        SyncError::InvalidField("cursor"),
    );
    add(
        "events_event_cursor_65_bytes",
        Message::Events {
            sub: 1,
            events: vec![event(&[0; 65], &[])],
        },
        SyncError::InvalidField("cursor"),
    );
    add(
        "events_payload_one_byte_over",
        Message::Events {
            sub: 1,
            // 4 (sub) + 2 (count) + 2 + 1 (cursor) + 4 (length) = 13 bytes of overhead.
            events: vec![event(&[1], &vec![0xab; MAX_PAYLOAD - 13 + 1])],
        },
        SyncError::TooLarge,
    );
    add(
        "events_two_events_over_the_payload_limit",
        Message::Events {
            sub: 1,
            events: vec![
                event(&[1], &vec![0xcd; 600_000]),
                event(&[2], &vec![0xcd; 600_000]),
            ],
        },
        SyncError::TooLarge,
    );
    add(
        "ack_sub_zero",
        Message::Ack {
            sub: 0,
            cursor: vec![1],
        },
        SyncError::InvalidField("subscription id"),
    );
    add(
        "ack_empty_cursor",
        Message::Ack {
            sub: 1,
            cursor: vec![],
        },
        SyncError::InvalidField("cursor"),
    );
    add(
        "ack_cursor_65_bytes",
        Message::Ack {
            sub: 1,
            cursor: vec![0; 65],
        },
        SyncError::InvalidField("cursor"),
    );
    add(
        "error_message_257_bytes",
        error_msg(ErrorCode::Internal, 0, &"e".repeat(257)),
        SyncError::InvalidField("message"),
    );
    add(
        "error_message_nul",
        error_msg(ErrorCode::Internal, 0, "a\u{0}"),
        SyncError::InvalidField("message"),
    );
    v
}

/// `(min, max, expected)` from the negotiation table in the format document.
fn negotiate_table() -> Vec<(u16, u16, Result<u16, ErrorCode>)> {
    use ErrorCode::{Malformed, UnsupportedVersion};
    vec![
        (1, 1, Ok(1)),
        (1, 2, Ok(1)),
        (1, 65_535, Ok(1)),
        (2, 2, Err(UnsupportedVersion)),
        (2, 9, Err(UnsupportedVersion)),
        (65_535, 65_535, Err(UnsupportedVersion)),
        (0, 1, Err(Malformed)),
        (0, 0, Err(Malformed)),
        (0, 65_535, Err(Malformed)),
        (2, 1, Err(Malformed)),
        (1, 0, Err(Malformed)),
        (65_535, 1, Err(Malformed)),
    ]
}

// ---- feed vectors ---------------------------------------------------------------------------

/// An event described by plain values, and its bytes assembled field by field.
#[derive(Clone)]
struct Ev {
    lsn: u64,
    index: u32,
    set_len: u32,
    change_set: [u8; 16],
    wall_ms: u64,
    counter: u32,
    node: u32,
    author: u128,
    turn: Option<(u64, u64)>,
    kind: u32,
    key: Vec<u8>,
    value: Option<Vec<u8>>,
}

impl Ev {
    fn raw(&self) -> Vec<u8> {
        let mut o = vec![1];
        o.extend(self.lsn.to_be_bytes());
        o.extend(self.index.to_be_bytes());
        o.extend(self.set_len.to_le_bytes());
        o.extend(self.change_set);
        o.extend(self.wall_ms.to_le_bytes());
        o.extend(self.counter.to_le_bytes());
        o.extend(self.node.to_le_bytes());
        o.extend(self.author.to_be_bytes());
        match self.turn {
            None => o.push(0),
            Some((s, t)) => {
                o.push(1);
                o.extend(s.to_le_bytes());
                o.extend(t.to_le_bytes());
            }
        }
        o.extend(self.kind.to_le_bytes());
        o.extend((self.key.len() as u16).to_le_bytes());
        o.extend(&self.key);
        match &self.value {
            None => o.push(0),
            Some(v) => {
                o.push(1);
                o.extend((v.len() as u32).to_le_bytes());
                o.extend(v);
            }
        }
        o
    }

    fn event(&self) -> ChangeEvent {
        ChangeEvent {
            cursor: if self.lsn == 0 {
                Cursor::ORIGIN
            } else {
                Cursor::new(self.lsn, self.index).unwrap()
            },
            set_len: self.set_len,
            change_set: ChangeSetId::from_bytes(self.change_set),
            hlc: Hlc {
                wall_ms: self.wall_ms,
                counter: self.counter,
                node: self.node,
            },
            author: self.author,
            turn: self.turn.map(|(session, turn)| TurnRef { session, turn }),
            change: EntityChange {
                kind: self.kind,
                key: self.key.clone(),
                op: match &self.value {
                    None => ChangeOp::Delete,
                    Some(v) => ChangeOp::Upsert(v.clone()),
                },
            },
        }
    }
}

fn ev_base() -> Ev {
    Ev {
        lsn: 258,
        index: 1,
        set_len: 3,
        change_set: [0xa0; 16],
        wall_ms: 5,
        counter: 6,
        node: 7,
        author: 9,
        turn: None,
        kind: 2,
        key: b"k".to_vec(),
        value: Some(b"vv".to_vec()),
    }
}

const DOC_UPSERT: &str = "01 | 0000000000000102 00000001 | 03000000 | a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0 \
    | 0500000000000000 06000000 07000000 | 00000000000000000000000000000009 | 00 | 02000000 \
    | 0100 6b | 01 02000000 7676";
const DOC_DELETE: &str = "01 | 0000000000000001 00000000 | 01000000 | 01010101010101010101010101010101 \
    | 00000000000000000000000000000000 | 00000000000000000000000000000000 \
    | 01 1100000000000000 2200000000000000 | 00000000 | 0000 | 00";

fn feed_valid() -> Vec<(String, Ev, Vec<u8>)> {
    let mut v: Vec<(String, Ev, Vec<u8>)> = Vec::new();
    v.push(("doc_upsert_in_the_middle".into(), ev_base(), h(DOC_UPSERT)));
    v.push((
        "doc_delete_with_a_turn".into(),
        Ev {
            lsn: 1,
            index: 0,
            set_len: 1,
            change_set: [1; 16],
            wall_ms: 0,
            counter: 0,
            node: 0,
            author: 0,
            turn: Some((0x11, 0x22)),
            kind: 0,
            key: vec![],
            value: None,
        },
        h(DOC_DELETE),
    ));
    v.push((
        "every_field_at_its_maximum".into(),
        Ev {
            lsn: u64::MAX,
            index: u32::MAX - 1,
            set_len: u32::MAX,
            change_set: *b"\x00\x01\x02\x03\x04\x05\x06\x07\x08\x09\x0a\x0b\x0c\x0d\x0e\xff",
            wall_ms: u64::MAX,
            counter: u32::MAX,
            node: u32::MAX,
            author: u128::MAX,
            turn: Some((u64::MAX, u64::MAX)),
            kind: u32::MAX,
            key: vec![0xff; 4096],
            value: Some(vec![]),
        },
        Vec::new(), // filled below from `Ev::raw`, which is checked against the document bytes
    ));
    v.push((
        "values_above_2_pow_53".into(),
        Ev {
            lsn: 9_007_199_254_740_993,
            index: 0,
            set_len: 1,
            change_set: [0x5a; 16],
            wall_ms: 9_007_199_254_740_993,
            counter: 1,
            node: 2,
            author: u128::MAX - 1,
            turn: Some((9_007_199_254_740_993, 18_446_744_073_709_551_615)),
            kind: 7,
            key: vec![0, 1, 2],
            value: Some(vec![9, 9]),
        },
        Vec::new(),
    ));
    v.push((
        "last_event_of_a_set".into(),
        Ev {
            index: 2,
            ..ev_base()
        },
        Vec::new(),
    ));
    v.push((
        "delete_without_turn_empty_key".into(),
        Ev {
            key: vec![],
            value: None,
            ..ev_base()
        },
        Vec::new(),
    ));
    v.push((
        "upsert_empty_value_with_turn".into(),
        Ev {
            turn: Some((3, 4)),
            value: Some(vec![]),
            ..ev_base()
        },
        Vec::new(),
    ));
    v.push((
        "longer_value".into(),
        Ev {
            value: Some((0..=255u8).collect()),
            ..ev_base()
        },
        Vec::new(),
    ));
    for (_, ev, bytes) in &mut v {
        if bytes.is_empty() {
            *bytes = ev.raw();
        }
    }
    v
}

/// Writes `offset..` of the base event, for the invalid-event vectors.
fn mutate(base: &[u8], f: impl FnOnce(&mut Vec<u8>)) -> Vec<u8> {
    let mut b = base.to_vec();
    f(&mut b);
    b
}

fn feed_invalid() -> Vec<(String, Vec<u8>, FeedDecodeError)> {
    use FeedDecodeError::{Invalid, Trailing, Truncated, Version};
    // Offsets in the 80-byte base event: version 0, lsn 1..9, index 9..13, set_len 13..17,
    // change set 17..33, hlc 33..49, author 49..65, turn flag 65, kind 66..70, key length 70..72,
    // key 72, operation 73, value length 74..78, value 78..80.
    let base = h(DOC_UPSERT);
    assert_eq!(base.len(), 80);
    let mut v: Vec<(String, Vec<u8>, FeedDecodeError)> = Vec::new();
    let mut add = |name: &str, b: Vec<u8>, e: FeedDecodeError| v.push((name.to_string(), b, e));
    add("empty", vec![], Truncated);
    add("version_0", mutate(&base, |b| b[0] = 0), Version(0));
    add("version_2", mutate(&base, |b| b[0] = 2), Version(2));
    add("version_255", mutate(&base, |b| b[0] = 255), Version(255));
    add("cursor_cut_short", base[..8].to_vec(), Truncated);
    add(
        "lsn_zero_index_zero",
        mutate(&base, |b| b[1..13].fill(0)),
        Invalid("event lsn"),
    );
    add(
        "cursor_lsn_zero_index_nonzero",
        mutate(&base, |b| b[1..9].fill(0)),
        Invalid("cursor"),
    );
    add(
        "index_equals_set_len",
        mutate(&base, |b| b[9..13].copy_from_slice(&3u32.to_be_bytes())),
        Invalid("event index"),
    );
    add(
        "index_above_set_len",
        mutate(&base, |b| b[9..13].copy_from_slice(&u32::MAX.to_be_bytes())),
        Invalid("event index"),
    );
    add(
        "set_len_zero",
        mutate(&base, |b| b[13..17].fill(0)),
        Invalid("event index"),
    );
    add(
        "turn_flag_2",
        mutate(&base, |b| b[65] = 2),
        Invalid("turn flag"),
    );
    add(
        "turn_flag_255",
        mutate(&base, |b| b[65] = 255),
        Invalid("turn flag"),
    );
    add(
        "turn_present_but_cut",
        mutate(&base, |b| {
            b[65] = 1;
            b.truncate(70);
        }),
        Truncated,
    );
    add(
        "key_length_4097",
        mutate(&base, |b| b[70..72].copy_from_slice(&4097u16.to_le_bytes())),
        Invalid("key length"),
    );
    add(
        "key_length_65535",
        mutate(&base, |b| {
            b[70..72].copy_from_slice(&u16::MAX.to_le_bytes())
        }),
        Invalid("key length"),
    );
    add(
        "key_length_beyond_input",
        mutate(&base, |b| b[70..72].copy_from_slice(&100u16.to_le_bytes())),
        Truncated,
    );
    add(
        "operation_2",
        mutate(&base, |b| b[73] = 2),
        Invalid("operation"),
    );
    add(
        "value_length_u32_max",
        mutate(&base, |b| {
            b[74..78].copy_from_slice(&u32::MAX.to_le_bytes())
        }),
        Invalid("value length"),
    );
    add(
        "value_length_16_mib_plus_one",
        mutate(&base, |b| {
            b[74..78].copy_from_slice(&(16 * 1024 * 1024 + 1u32).to_le_bytes())
        }),
        Invalid("value length"),
    );
    add(
        "value_length_16_mib_without_bytes",
        mutate(&base, |b| {
            b[74..78].copy_from_slice(&(16u32 * 1024 * 1024).to_le_bytes())
        }),
        Truncated,
    );
    add(
        "value_length_1000_two_bytes_left",
        mutate(&base, |b| b[74..78].copy_from_slice(&1000u32.to_le_bytes())),
        Truncated,
    );
    add("trailing_byte", mutate(&base, |b| b.push(0)), Trailing);
    add(
        "delete_with_value_bytes_left",
        mutate(&base, |b| b[73] = 0),
        Trailing,
    );
    v
}

fn feed_cursors() -> Vec<(String, Cursor, Vec<u8>)> {
    let c = |lsn, index| Cursor::new(lsn, index).unwrap();
    vec![
        (
            "origin".into(),
            Cursor::ORIGIN,
            h("000000000000000000000000"),
        ),
        (
            "lsn_1_index_0".into(),
            c(1, 0),
            h("000000000000000100000000"),
        ),
        (
            "lsn_1_index_1".into(),
            c(1, 1),
            h("000000000000000100000001"),
        ),
        (
            "lsn_2_index_0".into(),
            c(2, 0),
            h("000000000000000200000000"),
        ),
        (
            "lsn_258_index_u32_max".into(),
            c(258, u32::MAX),
            h("0000000000000102ffffffff"),
        ),
        (
            "lsn_u64_max_index_u32_max".into(),
            c(u64::MAX, u32::MAX),
            h("ffffffffffffffffffffffff"),
        ),
        (
            "lsn_above_2_pow_53".into(),
            c(9_007_199_254_740_993, 0),
            h("0020000000000001 00000000"),
        ),
    ]
}

fn feed_cursors_invalid() -> Vec<(String, Vec<u8>, FeedDecodeError)> {
    vec![
        ("empty".into(), vec![], FeedDecodeError::Truncated),
        (
            "eleven_bytes".into(),
            vec![0; 11],
            FeedDecodeError::Truncated,
        ),
        (
            "thirteen_bytes".into(),
            vec![0; 13],
            FeedDecodeError::Trailing,
        ),
        (
            "lsn_zero_index_one".into(),
            h("000000000000000000000001"),
            FeedDecodeError::Invalid("cursor"),
        ),
    ]
}

fn feed_encode_errors() -> Vec<(String, ChangeEvent, FeedError)> {
    let base = ev_base().event();
    let with = |f: &dyn Fn(&mut ChangeEvent)| {
        let mut e = base.clone();
        f(&mut e);
        e
    };
    vec![
        (
            "origin_cursor".into(),
            with(&|e| e.cursor = Cursor::ORIGIN),
            FeedError::ZeroLsn,
        ),
        (
            "index_equals_set_len".into(),
            with(&|e| e.set_len = 1),
            FeedError::IndexOutOfRange {
                index: 1,
                set_len: 1,
            },
        ),
        (
            "set_len_zero".into(),
            with(&|e| e.set_len = 0),
            FeedError::IndexOutOfRange {
                index: 1,
                set_len: 0,
            },
        ),
        (
            "key_4097_bytes".into(),
            with(&|e| e.change.key = vec![7; MAX_KEY_LEN + 1]),
            FeedError::KeyTooLarge(MAX_KEY_LEN + 1),
        ),
        (
            "value_16_mib_plus_one".into(),
            with(&|e| e.change.op = ChangeOp::Upsert(vec![0x42; MAX_VALUE_LEN + 1])),
            FeedError::ValueTooLarge(MAX_VALUE_LEN + 1),
        ),
    ]
}

// ---- the file --------------------------------------------------------------------------------

fn list<T>(items: &[T], f: impl Fn(&T) -> String) -> String {
    let rows: Vec<String> = items.iter().map(|i| format!("      {}", f(i))).collect();
    format!("[\n{}\n    ]", rows.join(",\n"))
}

/// Checks every vector against the Rust codecs, then renders the file.
fn render() -> String {
    // Sync, valid: the hand-written bytes are what the encoder produces and the decoder reads back.
    let valid = sync_valid();
    for (name, msg, bytes) in &valid {
        let mut out = Vec::new();
        encode(msg, &mut out).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(
            &out, bytes,
            "{name}: encoder output differs from the hand-written bytes"
        );
        assert_eq!(
            decode(bytes),
            Ok(Some((msg.clone(), bytes.len()))),
            "{name}: decode"
        );
        for cut in 0..bytes.len() {
            assert_eq!(decode(&bytes[..cut]), Ok(None), "{name}: prefix {cut}");
        }
    }
    for (name, bytes, limit, want) in &sync_invalid() {
        assert_eq!(
            decode_with_limit(bytes, limit.unwrap_or(MAX_PAYLOAD)),
            Err(want.clone()),
            "{name}"
        );
    }
    for (name, bytes) in &sync_incomplete() {
        assert_eq!(decode(bytes), Ok(None), "{name}");
    }
    for (name, msg, want) in &sync_encode_errors() {
        let mut out = vec![0xee];
        assert_eq!(encode(msg, &mut out), Err(want.clone()), "{name}");
        assert_eq!(out, vec![0xee], "{name}: output must be left unchanged");
    }
    for (min, max, want) in negotiate_table() {
        assert_eq!(sync::negotiate(min, max), want, "negotiate {min}..={max}");
    }

    // Feed. The assembly helper is itself checked against the bytes printed in the document.
    assert_eq!(ev_base().raw(), h(DOC_UPSERT));
    let events = feed_valid();
    for (name, ev, bytes) in &events {
        assert_eq!(&ev.event().encode().unwrap(), bytes, "{name}: encode");
        assert_eq!(
            &ChangeEvent::decode(bytes).unwrap(),
            &ev.event(),
            "{name}: decode"
        );
        for cut in 0..bytes.len() {
            assert_eq!(
                ChangeEvent::decode(&bytes[..cut]),
                Err(FeedDecodeError::Truncated),
                "{name}: prefix {cut}"
            );
        }
    }
    for (name, bytes, want) in &feed_invalid() {
        assert_eq!(ChangeEvent::decode(bytes), Err(want.clone()), "{name}");
    }
    for (name, c, bytes) in &feed_cursors() {
        assert_eq!(&c.to_bytes()[..], &bytes[..], "{name}");
        assert_eq!(Cursor::from_bytes(bytes), Ok(*c), "{name}");
    }
    for (name, bytes, want) in &feed_cursors_invalid() {
        assert_eq!(Cursor::from_bytes(bytes), Err(want.clone()), "{name}");
    }
    for (name, ev, want) in &feed_encode_errors() {
        assert_eq!(ev.encode(), Err(want.clone()), "{name}");
    }

    let mut out = String::new();
    out.push_str("{\n");
    out.push_str("  \"format\": \"lattice-conformance-vectors\",\n");
    out.push_str("  \"version\": 1,\n");
    out.push_str("  \"generated_by\": \"crates/lattice-api/src/conformance.rs\",\n");
    out.push_str("  \"notes\": [\n");
    out.push_str(
        "    \"Bytes are lowercase hex. 64-bit and 128-bit integers are decimal strings.\",\n",
    );
    out.push_str("    \"A bytes value may be {\\\"fill\\\":\\\"ab\\\",\\\"len\\\":N}: N copies of one byte.\",\n");
    out.push_str(
        "    \"Sync invalid vectors may carry a payload limit, passed to decode_with_limit.\"\n",
    );
    out.push_str("  ],\n");
    out.push_str("  \"sync\": {\n");
    let _ = writeln!(
        out,
        "    \"valid\": {},",
        list(&valid, |(n, m, b)| format!(
            "{{\"name\":{},\"message\":{},\"bytes\":\"{}\"}}",
            q(n),
            message_json(m),
            hex(b)
        ))
    );
    let _ = writeln!(
        out,
        "    \"invalid\": {},",
        list(&sync_invalid(), |(n, b, limit, e)| {
            let limit = limit.map_or(String::new(), |l| format!("\"limit\":{l},"));
            format!(
                "{{\"name\":{},\"bytes\":\"{}\",{limit}\"error\":{}}}",
                q(n),
                hex(b),
                sync_error_json(e)
            )
        })
    );
    let _ = writeln!(
        out,
        "    \"incomplete\": {},",
        list(&sync_incomplete(), |(n, b)| format!(
            "{{\"name\":{},\"bytes\":\"{}\"}}",
            q(n),
            hex(b)
        ))
    );
    let _ = writeln!(
        out,
        "    \"encode_errors\": {},",
        list(&sync_encode_errors(), |(n, m, e)| format!(
            "{{\"name\":{},\"message\":{},\"error\":{}}}",
            q(n),
            message_json(m),
            sync_error_json(e)
        ))
    );
    let _ = writeln!(
        out,
        "    \"negotiate\": {}",
        list(&negotiate_table(), |(min, max, r)| {
            let result = match r {
                Ok(v) => format!("{{\"version\":{v}}}"),
                Err(c) => format!("{{\"error\":\"{}\"}}", code_name(*c)),
            };
            format!("{{\"min\":{min},\"max\":{max},\"result\":{result}}}")
        })
    );
    out.push_str("  },\n");
    out.push_str("  \"feed\": {\n");
    let _ = writeln!(
        out,
        "    \"cursors\": {},",
        list(&feed_cursors(), |(n, c, b)| format!(
            "{{\"name\":{},\"cursor\":{},\"bytes\":\"{}\"}}",
            q(n),
            cursor_json(*c),
            hex(b)
        ))
    );
    let _ = writeln!(
        out,
        "    \"cursors_invalid\": {},",
        list(&feed_cursors_invalid(), |(n, b, e)| format!(
            "{{\"name\":{},\"bytes\":\"{}\",\"error\":{}}}",
            q(n),
            hex(b),
            decode_error_json(e)
        ))
    );
    let _ = writeln!(
        out,
        "    \"events\": {},",
        list(&events, |(n, ev, b)| format!(
            "{{\"name\":{},\"event\":{},\"bytes\":\"{}\"}}",
            q(n),
            event_json(&ev.event()),
            hex(b)
        ))
    );
    let _ = writeln!(
        out,
        "    \"events_invalid\": {},",
        list(&feed_invalid(), |(n, b, e)| format!(
            "{{\"name\":{},\"bytes\":\"{}\",\"error\":{}}}",
            q(n),
            hex(b),
            decode_error_json(e)
        ))
    );
    let _ = writeln!(
        out,
        "    \"encode_errors\": {}",
        list(&feed_encode_errors(), |(n, ev, e)| format!(
            "{{\"name\":{},\"event\":{},\"error\":{}}}",
            q(n),
            event_json(ev),
            feed_error_json(e)
        ))
    );
    out.push_str("  }\n");
    out.push_str("}\n");
    out
}

#[test]
fn vectors_file_matches_rust_encoder() {
    let fresh = render();
    if UPDATE == Some("1") {
        // Update mode skips the comparison, so it must never be able to turn a CI run green.
        assert!(
            option_env!("CI").is_none(),
            "LATTICE_UPDATE_VECTORS=1 is refused when CI is set; unset one of them"
        );
        // This crate may not touch the file system (see the module comment), so the new file goes
        // to standard output between markers and tools/update-conformance-vectors.sh saves it.
        println!("\n-----BEGIN LATTICE CONFORMANCE VECTORS-----");
        print!("{fresh}");
        println!("-----END LATTICE CONFORMANCE VECTORS-----");
        return;
    }
    assert!(
        fresh == CHECKED_IN,
        "conformance/vectors.json is out of date with the Rust codecs. If the change is \
         deliberate, run tools/update-conformance-vectors.sh, review the diff and commit it \
         together with the clients that read it."
    );
}

#[test]
fn every_vector_name_is_unique_within_its_list() {
    fn unique<'a>(names: impl Iterator<Item = &'a str>) {
        let mut seen = std::collections::BTreeSet::new();
        for n in names {
            assert!(seen.insert(n.to_string()), "duplicate vector name {n}");
        }
    }
    unique(sync_valid().iter().map(|v| v.0.as_str()));
    unique(sync_invalid().iter().map(|v| v.0.as_str()));
    unique(sync_incomplete().iter().map(|v| v.0.as_str()));
    unique(sync_encode_errors().iter().map(|v| v.0.as_str()));
    unique(feed_valid().iter().map(|v| v.0.as_str()));
    unique(feed_invalid().iter().map(|v| v.0.as_str()));
    unique(feed_cursors().iter().map(|v| v.0.as_str()));
    unique(feed_cursors_invalid().iter().map(|v| v.0.as_str()));
    unique(feed_encode_errors().iter().map(|v| v.0.as_str()));
}

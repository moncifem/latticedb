//! Codec tests: rejections the format promises, and properties over generated input.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use lattice_sdk::contract::{
    ContractError, ErrorCode, Framer, Kind, MAX_BODY_BYTES, MAX_LINE_BYTES, Message, Receipt,
    Record, Status, TurnState, decode_line, encode_line,
};
use proptest::prelude::*;

fn err(line: &str) -> ContractError {
    decode_line(line.as_bytes()).unwrap_err()
}

const TAKE: &str = r#"{"v":1,"t":"turn.take","id":"r1","session":"s1"}"#;

#[test]
fn accepts_the_canonical_form_and_loose_spacing() {
    let m = decode_line(TAKE.as_bytes()).unwrap();
    assert_eq!(encode_line(&m).unwrap(), TAKE);
    let loose = "  {\t\"session\" : \"s1\" , \"id\":\"r1\",\"t\":\"turn.take\",\"v\":1 }  ";
    assert_eq!(decode_line(loose.as_bytes()).unwrap(), m);
}

#[test]
fn rejects_versions_other_than_one() {
    assert_eq!(
        err(&TAKE.replace("\"v\":1", "\"v\":2")),
        ContractError::UnsupportedVersion(2)
    );
    assert_eq!(
        err(&TAKE.replace("\"v\":1", "\"v\":0")),
        ContractError::UnsupportedVersion(0)
    );
    assert_eq!(
        err(&TAKE.replace("\"v\":1", "\"v\":\"1\"")),
        ContractError::WrongType("v")
    );
    assert_eq!(
        err(r#"{"t":"turn.take","id":"r1","session":"s1"}"#),
        ContractError::MissingField("v")
    );
    // Malformed JSON is refused before the version is looked at.
    assert_eq!(
        err(r#"{"v":9,"t":"nonsense","zzz":[}"#).code(),
        ErrorCode::BadRequest
    );
    assert_eq!(
        err(r#"{"v":9,"t":"nonsense"}"#),
        ContractError::UnsupportedVersion(9)
    );
    assert_eq!(
        ContractError::UnsupportedVersion(9).code(),
        ErrorCode::UnsupportedVersion
    );
}

#[test]
fn rejects_unknown_fields_types_and_values() {
    assert_eq!(
        err(&TAKE.replace('}', ",\"extra\":1}")),
        ContractError::UnknownField("extra".into())
    );
    assert!(matches!(
        err(&TAKE.replace("turn.take", "turn.steal")),
        ContractError::UnknownType(_)
    ));
    assert_eq!(
        err(r#"{"v":1,"t":"turn.take","id":"r1"}"#),
        ContractError::MissingField("session")
    );
    assert_eq!(
        err(&TAKE.replace("\"s1\"", "5")),
        ContractError::WrongType("session")
    );
    assert_eq!(
        err(
            r#"{"v":1,"t":"append","id":"a","session":"s1","turn":1,"records":[{"rid":"m","kind":"poem","body":""}]}"#
        ),
        ContractError::InvalidValue("kind")
    );
    assert_eq!(
        err(
            r#"{"v":1,"t":"append","id":"a","session":"s1","turn":1,"records":[{"rid":"m","kind":"note","body":"","x":1}]}"#
        ),
        ContractError::UnknownField("x".into())
    );
    assert_eq!(
        err(r#"{"v":1,"t":"append","id":"a","session":"s1","turn":1,"records":[]}"#),
        ContractError::InvalidValue("records")
    );
    assert_eq!(
        err(
            r#"{"v":1,"t":"append","id":"a","session":"s1","turn":0,"records":[{"rid":"m","kind":"note","body":""}]}"#
        ),
        ContractError::InvalidValue("turn")
    );
    // Fenced receipts have no position, the others must have one.
    let fenced = |pos: &str| {
        format!(
            r#"{{"v":1,"t":"append.receipt","id":"a","session":"s1","turn":1,"receipts":[{{"rid":"m","status":"fenced"{pos}}}]}}"#
        )
    };
    assert!(decode_line(fenced("").as_bytes()).is_ok());
    assert!(decode_line(fenced(",\"pos\":3").as_bytes()).is_err());
    let recorded = r#"{"v":1,"t":"append.receipt","id":"a","session":"s1","turn":1,"receipts":[{"rid":"m","status":"recorded"}]}"#;
    assert!(decode_line(recorded.as_bytes()).is_err());
}

#[test]
fn rejects_duplicate_keys_anywhere() {
    assert_eq!(
        err(r#"{"v":1,"v":1,"t":"turn.take","id":"r1","session":"s1"}"#),
        ContractError::DuplicateKey("v".into())
    );
    assert_eq!(
        err(r#"{"v":1,"t":"turn.take","id":"r1","session":"s1","id":"r2"}"#),
        ContractError::DuplicateKey("id".into())
    );
    assert_eq!(
        err(
            r#"{"v":1,"t":"append","id":"a","session":"s1","turn":1,"records":[{"rid":"m","rid":"n","kind":"note","body":""}]}"#
        ),
        ContractError::DuplicateKey("rid".into())
    );
    // Equal after unescaping is equal.
    assert_eq!(
        err(r#"{"v":1,"v":1,"t":"turn.take","id":"r1","session":"s1"}"#),
        ContractError::DuplicateKey("v".into())
    );
}

#[test]
fn rejects_bad_numbers() {
    for n in [
        "1.5",
        "1e2",
        "-1",
        "01",
        "9007199254740992",
        "99999999999999999999",
        "1.0",
    ] {
        let line = format!(r#"{{"v":1,"t":"watch","id":"w","session":"s1","cursor":{n}}}"#);
        assert_eq!(err(&line), ContractError::InvalidNumber, "{n}");
    }
    let max = r#"{"v":1,"t":"watch","id":"w","session":"s1","cursor":9007199254740991}"#;
    assert!(decode_line(max.as_bytes()).is_ok());
    let zero = r#"{"v":1,"t":"watch","id":"w","session":"s1","cursor":0}"#;
    assert!(decode_line(zero.as_bytes()).is_ok());
}

#[test]
fn rejects_malformed_json_and_bad_encodings() {
    for line in [
        "",
        "   ",
        "{",
        "}",
        "[]",
        "null",
        "\"x\"",
        "{\"v\":1,}",
        "{,}",
        "{\"v\" 1}",
        "{\"v\":1} {\"v\":1}",
        "{\"v\":1}x",
        "{\"v\":x}",
        "{'v':1}",
        "{\"v\":1,\"t\":\"turn.take\",\"id\":\"r\\q\",\"session\":\"s\"}",
    ] {
        assert!(decode_line(line.as_bytes()).is_err(), "{line:?}");
    }
    assert_eq!(err(""), ContractError::EmptyLine);
    // Raw newline, carriage return and BOM are not part of a line.
    assert!(decode_line(format!("{TAKE}\r").as_bytes()).is_err());
    assert!(decode_line(format!("{TAKE}\n").as_bytes()).is_err());
    assert!(decode_line(format!("\u{feff}{TAKE}").as_bytes()).is_err());
    // Invalid UTF-8.
    assert_eq!(
        decode_line(&[b'{', 0xff, b'}']),
        Err(ContractError::InvalidUtf8)
    );
    // Raw control character in a string, and lone surrogates.
    assert!(decode_line(TAKE.replace("r1", "r\u{1}").as_bytes()).is_err());
    let note = |body: &str| {
        format!(
            r#"{{"v":1,"t":"append","id":"a","session":"s1","turn":1,"records":[{{"rid":"m","kind":"note","body":"{body}"}}]}}"#
        )
    };
    assert!(decode_line(note("\\ud83d").as_bytes()).is_err());
    assert!(decode_line(note("\\ude00").as_bytes()).is_err());
    assert!(decode_line(note("\\ud83d\\u0041").as_bytes()).is_err());
    assert!(decode_line(note("\\ud83d\\ude00").as_bytes()).is_ok());
    assert!(decode_line(note("a\nb").as_bytes()).is_err());
    assert!(decode_line(note("a\\nb").as_bytes()).is_ok());
}

#[test]
fn bounds_size_depth_and_width() {
    let long = format!("{TAKE}{}", " ".repeat(MAX_LINE_BYTES));
    assert_eq!(decode_line(long.as_bytes()), Err(ContractError::TooLong));
    let deep = format!("{}{}", "[".repeat(100_000), "]".repeat(100_000));
    assert_eq!(decode_line(deep.as_bytes()), Err(ContractError::TooDeep));
    let deep = format!("{}{}", "[".repeat(1000), "]".repeat(1000));
    assert_eq!(decode_line(deep.as_bytes()), Err(ContractError::TooDeep));
    let wide = format!("[{}]", vec!["1"; 65].join(","));
    assert_eq!(decode_line(wide.as_bytes()), Err(ContractError::TooMany));
    let keys: Vec<String> = (0..17).map(|i| format!("\"k{i}\":1")).collect();
    assert_eq!(
        decode_line(format!("{{{}}}", keys.join(",")).as_bytes()),
        Err(ContractError::TooMany)
    );
    // 65 records and an over-long body.
    let rec = r#"{"rid":"m","kind":"note","body":""}"#;
    let many = format!(
        r#"{{"v":1,"t":"append","id":"a","session":"s1","turn":1,"records":[{}]}}"#,
        vec![rec; 65].join(",")
    );
    assert_eq!(err(&many), ContractError::TooMany);
    let fits = Message::Append {
        id: "a".into(),
        session: "s1".into(),
        turn: 1,
        records: vec![Record {
            rid: "m".into(),
            kind: Kind::Note,
            body: "x".repeat(MAX_BODY_BYTES),
        }],
    };
    assert!(encode_line(&fits).is_ok());
    let Message::Append {
        id,
        session,
        turn,
        mut records,
    } = fits
    else {
        panic!()
    };
    records[0].body.push('x');
    assert_eq!(
        encode_line(&Message::Append {
            id,
            session,
            turn,
            records
        }),
        Err(ContractError::InvalidValue("body too long"))
    );
}

#[test]
fn id_charsets_and_lengths() {
    let id = |s: &str| format!(r#"{{"v":1,"t":"watch.stop","id":"{s}"}}"#);
    assert!(decode_line(id(&"a".repeat(64)).as_bytes()).is_ok());
    assert!(decode_line(id(&"a".repeat(65)).as_bytes()).is_err());
    assert!(decode_line(id("").as_bytes()).is_err());
    assert!(decode_line(id("has space").as_bytes()).is_err());
    assert!(decode_line(id("a.b_c:d-E9").as_bytes()).is_ok());
    let create = |src: &str, ext: &str| {
        format!(r#"{{"v":1,"t":"session.create","id":"r","source":"{src}","external_id":"{ext}"}}"#)
    };
    assert!(decode_line(create("claude-code", "x y/é").as_bytes()).is_ok());
    assert!(decode_line(create("Claude", "x").as_bytes()).is_err());
    assert!(decode_line(create("a", "").as_bytes()).is_err());
    assert!(decode_line(create("a", &"é".repeat(129)).as_bytes()).is_err());
    assert!(decode_line(create("a", &"é".repeat(128)).as_bytes()).is_ok());
    assert!(decode_line(create("a", "tab\\there").as_bytes()).is_err());
}

#[test]
fn error_messages_may_have_null_id() {
    let line = r#"{"v":1,"t":"error","id":null,"code":"bad_request","message":"no"}"#;
    let m = decode_line(line.as_bytes()).unwrap();
    assert_eq!(encode_line(&m).unwrap(), line);
    // null is not allowed anywhere else.
    assert!(decode_line(TAKE.replace("\"s1\"", "null").as_bytes()).is_err());
}

#[test]
fn encoder_refuses_what_the_decoder_refuses() {
    let bad = Message::TurnEnd {
        id: "r".into(),
        session: "s1".into(),
        turn: 0,
    };
    assert!(encode_line(&bad).is_err());
    let bad = Message::AppendReceipt {
        id: "r".into(),
        session: "s1".into(),
        turn: 1,
        receipts: vec![Receipt {
            rid: "m".into(),
            status: Status::Fenced,
            pos: Some(1),
        }],
    };
    assert!(encode_line(&bad).is_err());
    let bad = Message::Watch {
        id: "w".into(),
        session: "s1".into(),
        cursor: 1 << 53,
    };
    assert!(encode_line(&bad).is_err());
}

#[test]
fn framer_splits_limits_and_recovers() {
    let mut f = Framer::new();
    let out = f.push(b"one\ntwo\nthr");
    assert_eq!(out, vec![Ok(b"one".to_vec()), Ok(b"two".to_vec())]);
    assert_eq!(f.push(b"ee\n"), vec![Ok(b"three".to_vec())]);
    assert_eq!(f.push(b"\n"), vec![Ok(Vec::new())]);
    assert_eq!(f.finish(), None);
    // Exactly at the limit is fine, one past is reported once and the stream continues.
    let mut ok = vec![b'a'; MAX_LINE_BYTES];
    ok.push(b'\n');
    let out = f.push(&ok);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].as_ref().unwrap().len(), MAX_LINE_BYTES);
    let mut over = vec![b'a'; MAX_LINE_BYTES + 1];
    over.extend_from_slice(b"\nnext\n");
    assert_eq!(
        f.push(&over),
        vec![Err(ContractError::TooLong), Ok(b"next".to_vec())]
    );
    // A trailing partial line is reported at the end, an unfinished over-long one as too long.
    f.push(b"partial");
    assert_eq!(f.finish(), Some(Err(ContractError::Unterminated)));
    f.push(&vec![b'a'; MAX_LINE_BYTES + 10]);
    assert_eq!(f.finish(), Some(Err(ContractError::TooLong)));
    assert_eq!(f.finish(), None);
}

// ---------------------------------------------------------------------------------------------
// Properties
// ---------------------------------------------------------------------------------------------

fn token(max: usize) -> impl Strategy<Value = String> {
    proptest::string::string_regex(&format!("[A-Za-z0-9._:-]{{1,{max}}}")).unwrap()
}

fn text(max: usize) -> impl Strategy<Value = String> {
    // Any scalar value, including control characters, quotes, backslashes and astral characters.
    proptest::collection::vec(any::<char>(), 0..max).prop_map(|v| v.into_iter().collect())
}

fn kind() -> impl Strategy<Value = Kind> {
    prop_oneof![
        Just(Kind::Message),
        Just(Kind::ToolCall),
        Just(Kind::ToolResult),
        Just(Kind::Note)
    ]
}

fn state() -> impl Strategy<Value = TurnState> {
    prop_oneof![
        Just(TurnState::Running),
        Just(TurnState::Idle),
        Just(TurnState::Superseded)
    ]
}

const MAX_INT: u64 = (1 << 53) - 1;

fn pos() -> impl Strategy<Value = u64> {
    prop_oneof![1..=10u64, 1..=MAX_INT, Just(MAX_INT)]
}

fn record() -> impl Strategy<Value = Record> {
    (token(128), kind(), text(40)).prop_map(|(rid, kind, body)| Record { rid, kind, body })
}

fn receipt() -> impl Strategy<Value = Receipt> {
    prop_oneof![
        (token(128), pos()).prop_map(|(rid, p)| Receipt {
            rid,
            status: Status::Recorded,
            pos: Some(p)
        }),
        (token(128), pos()).prop_map(|(rid, p)| Receipt {
            rid,
            status: Status::Duplicate,
            pos: Some(p)
        }),
        token(128).prop_map(|rid| Receipt {
            rid,
            status: Status::Fenced,
            pos: None
        }),
    ]
}

fn code() -> impl Strategy<Value = ErrorCode> {
    prop_oneof![
        Just(ErrorCode::BadRequest),
        Just(ErrorCode::UnsupportedVersion),
        Just(ErrorCode::UnknownSession),
        Just(ErrorCode::UnknownTurn),
        Just(ErrorCode::UnknownWatch),
        Just(ErrorCode::Conflict),
        Just(ErrorCode::TooLarge),
        Just(ErrorCode::Internal),
    ]
}

fn message() -> impl Strategy<Value = Message> {
    let id = || token(64);
    let a = prop_oneof![
        (
            id(),
            proptest::string::string_regex("[a-z0-9._-]{1,64}").unwrap(),
            proptest::string::string_regex("\\PC{1,64}").unwrap(),
            proptest::option::of(id())
        )
            .prop_map(|(id, source, external_id, parent)| Message::SessionCreate {
                id,
                source,
                external_id,
                parent
            }),
        (id(), id(), any::<bool>(), proptest::option::of(id())).prop_map(
            |(id, session, created, parent)| Message::SessionCreated {
                id,
                session,
                created,
                parent
            }
        ),
        (id(), id(), pos(), proptest::collection::vec(record(), 1..5)).prop_map(
            |(id, session, turn, records)| Message::Append {
                id,
                session,
                turn,
                records
            }
        ),
        (
            id(),
            id(),
            pos(),
            proptest::collection::vec(receipt(), 1..5)
        )
            .prop_map(|(id, session, turn, receipts)| Message::AppendReceipt {
                id,
                session,
                turn,
                receipts
            }),
        (id(), id()).prop_map(|(id, session)| Message::TurnTake { id, session }),
        (id(), id(), pos()).prop_map(|(id, session, turn)| Message::TurnEnd { id, session, turn }),
    ];
    let b = prop_oneof![
        (id(), id(), pos(), state()).prop_map(|(id, session, turn, state)| {
            Message::TurnStateReply {
                id,
                session,
                turn,
                state,
            }
        }),
        (id(), id(), 0..=MAX_INT).prop_map(|(id, session, cursor)| Message::Watch {
            id,
            session,
            cursor
        }),
        id().prop_map(|id| Message::WatchStop { id }),
        id().prop_map(|id| Message::WatchStopped { id }),
        (id(), id(), 0..=MAX_INT).prop_map(|(id, session, head)| Message::Ready {
            id,
            session,
            head
        }),
        (id(), id(), pos(), pos(), token(128), kind(), text(40)).prop_map(
            |(id, session, pos, turn, rid, kind, body)| Message::Item {
                id,
                session,
                pos,
                turn,
                rid,
                kind,
                body
            }
        ),
        (id(), id(), pos(), pos(), state()).prop_map(|(id, session, pos, turn, state)| {
            Message::TurnEvent {
                id,
                session,
                pos,
                turn,
                state,
            }
        }),
        (id(), id()).prop_map(|(id, session)| Message::Live { id, session }),
        (proptest::option::of(id()), code(), text(60))
            .prop_map(|(id, code, message)| Message::Error { id, code, message }),
    ];
    prop_oneof![a, b]
}

proptest! {
    #[test]
    fn encode_then_decode_round_trips(m in message()) {
        let line = encode_line(&m).unwrap();
        prop_assert!(!line.contains('\n') && !line.contains('\r'));
        let back = decode_line(line.as_bytes()).unwrap();
        prop_assert_eq!(&back, &m);
        // The encoding is canonical: decoding then encoding gives the same bytes.
        prop_assert_eq!(encode_line(&back).unwrap(), line);
    }

    #[test]
    fn arbitrary_bytes_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..400)) {
        if let Ok(m) = decode_line(&bytes) {
            let line = encode_line(&m).unwrap();
            prop_assert_eq!(decode_line(line.as_bytes()).unwrap(), m);
        }
        let mut f = Framer::new();
        let _ = f.push(&bytes);
        let _ = f.finish();
    }

    #[test]
    fn damaged_valid_lines_never_panic(m in message(), cut in any::<usize>(), flip in any::<usize>(), byte in any::<u8>()) {
        let mut line = encode_line(&m).unwrap().into_bytes();
        let at = flip % line.len();
        line[at] = byte;
        if let Ok(m2) = decode_line(&line) {
            prop_assert_eq!(decode_line(encode_line(&m2).unwrap().as_bytes()).unwrap(), m2);
        }
        line.truncate(cut % (line.len() + 1));
        let _ = decode_line(&line);
    }

    #[test]
    fn framer_is_independent_of_chunking(
        lines in proptest::collection::vec(proptest::collection::vec(any::<u8>().prop_filter("no newline", |b| *b != b'\n'), 0..30), 0..6),
        cuts in proptest::collection::vec(any::<usize>(), 0..6),
    ) {
        let mut stream = Vec::new();
        for l in &lines {
            stream.extend_from_slice(l);
            stream.push(b'\n');
        }
        let mut whole = Framer::new();
        let expect = whole.push(&stream);
        let mut pieces = Framer::new();
        let mut got = Vec::new();
        let mut from = 0;
        let mut points: Vec<usize> = cuts.iter().map(|c| c % (stream.len() + 1)).collect();
        points.sort_unstable();
        for p in points.into_iter().chain([stream.len()]) {
            got.extend(pieces.push(&stream[from..p]));
            from = p;
        }
        prop_assert_eq!(got, expect);
    }
}

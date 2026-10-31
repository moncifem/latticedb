//! Exact edges of the contract's limits: the last value that fits and the first that does not.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use lattice_sdk::contract::{
    ContractError, ErrorCode, Kind, MAX_BODY_BYTES, MAX_LINE_BYTES, Message, Record, TurnState,
    decode_line, encode_line,
};

fn err(line: &str) -> ContractError {
    decode_line(line.as_bytes()).unwrap_err()
}

const TAKE: &str = r#"{"v":1,"t":"turn.take","id":"r1","session":"s1"}"#;

fn item(body: String) -> Message {
    Message::Item {
        id: "w".into(),
        session: "s1".into(),
        pos: 1,
        turn: 1,
        rid: "m".into(),
        kind: Kind::Note,
        body,
    }
}

fn error(id: Option<&str>, message: String) -> Message {
    Message::Error {
        id: id.map(String::from),
        code: ErrorCode::BadRequest,
        message,
    }
}

fn append(bodies: &[usize]) -> Message {
    Message::Append {
        id: "a".into(),
        session: "s1".into(),
        turn: 1,
        records: bodies
            .iter()
            .map(|n| Record {
                rid: "m".into(),
                kind: Kind::Note,
                body: "x".repeat(*n),
            })
            .collect(),
    }
}

#[test]
fn the_body_limit_is_256_kib() {
    assert_eq!(MAX_BODY_BYTES, 262_144);
}

#[test]
fn only_client_messages_are_requests() {
    assert!(decode_line(TAKE.as_bytes()).unwrap().is_request());
    let live = Message::Live {
        id: "w".into(),
        session: "s1".into(),
    };
    assert!(!live.is_request());
    assert!(!item(String::new()).is_request());
    let event = Message::TurnEvent {
        id: "w".into(),
        session: "s1".into(),
        pos: 1,
        turn: 1,
        state: TurnState::Running,
    };
    assert!(!event.is_request());
}

#[test]
fn an_item_body_may_fill_the_limit_and_no_more() {
    assert!(encode_line(&item("x".repeat(MAX_BODY_BYTES))).is_ok());
    assert_eq!(
        encode_line(&item("x".repeat(MAX_BODY_BYTES + 1))),
        Err(ContractError::InvalidValue("body too long"))
    );
}

#[test]
fn an_error_message_may_fill_512_bytes_and_no_more() {
    assert!(encode_line(&error(None, "x".repeat(512))).is_ok());
    assert_eq!(
        encode_line(&error(None, "x".repeat(513))),
        Err(ContractError::InvalidValue("message too long"))
    );
}

#[test]
fn an_error_id_is_checked_when_present() {
    assert!(encode_line(&error(Some("r.1"), "no".into())).is_ok());
    assert_eq!(
        encode_line(&error(Some("has space"), "no".into())),
        Err(ContractError::InvalidValue("id"))
    );
}

#[test]
fn an_append_holds_at_most_64_records() {
    assert!(encode_line(&append(&[0; 64])).is_ok());
    assert!(encode_line(&append(&[0; 65])).is_err());
    assert!(encode_line(&append(&[])).is_err());
}

#[test]
fn nesting_is_allowed_to_seven_levels_and_not_to_eight() {
    let arrays = |n: usize| format!("{}{}", "[".repeat(n), "]".repeat(n));
    assert_ne!(err(&arrays(7)), ContractError::TooDeep);
    assert_eq!(err(&arrays(8)), ContractError::TooDeep);
    let objects = |n: usize| format!("{}{{}}{}", "{\"a\":".repeat(n - 1), "}".repeat(n - 1));
    assert_ne!(err(&objects(7)), ContractError::TooDeep);
    assert_eq!(err(&objects(8)), ContractError::TooDeep);
}

#[test]
fn an_empty_object_is_consumed_whole() {
    let with = |v: &str| format!(r#"{{"v":1,"t":"turn.take","id":"r1","session":"s1","x":{v}}}"#);
    assert_eq!(err(&with("{}")), err(&with("1")));
}

fn note(body: &str) -> String {
    format!(
        r#"{{"v":1,"t":"append","id":"a","session":"s1","turn":1,"records":[{{"rid":"m","kind":"note","body":"{body}"}}]}}"#
    )
}

fn body_of(line: &str) -> String {
    let Message::Append { mut records, .. } = decode_line(line.as_bytes()).unwrap() else {
        panic!("not an append");
    };
    records.remove(0).body
}

#[test]
fn every_simple_escape_decodes_to_its_character() {
    assert_eq!(
        body_of(&note(r#"\"\\\/\b\f\n\r\t"#)),
        "\"\\/\u{8}\u{c}\n\r\t"
    );
}

/// A JSON `\\u` escape for each four-digit hex group, joined with nothing between them.
fn esc(groups: &[&str]) -> String {
    groups.iter().map(|g| format!("{}u{g}", '\\')).collect()
}

#[test]
fn surrogate_pairs_decode_to_their_exact_code_points() {
    assert_eq!(body_of(&note(&esc(&["d83d", "de00"]))), "\u{1F600}");
    assert_eq!(body_of(&note(&esc(&["d800", "dc00"]))), "\u{10000}");
    assert_eq!(body_of(&note(&esc(&["dbff", "dfff"]))), "\u{10FFFF}");
    assert_eq!(body_of(&note(&esc(&["d801", "dc37"]))), "\u{10437}");
}

#[test]
fn a_lone_surrogate_is_named_as_such() {
    let lone = ContractError::Syntax("lone surrogate");
    assert_eq!(err(&note(&esc(&["de00"]))), lone);
    assert_eq!(err(&note(&esc(&["dfff"]))), lone);
    assert_eq!(err(&note(&esc(&["d83d"]))), lone);
    assert_eq!(err(&note(&esc(&["d83d", "0041"]))), lone);
}

#[test]
fn a_line_may_be_exactly_the_size_limit() {
    let padded = format!("{TAKE}{}", " ".repeat(MAX_LINE_BYTES - TAKE.len()));
    assert_eq!(padded.len(), MAX_LINE_BYTES);
    assert!(decode_line(padded.as_bytes()).is_ok());
    assert_eq!(
        decode_line(format!("{padded} ").as_bytes()),
        Err(ContractError::TooLong)
    );
}

#[test]
fn an_encoded_line_may_be_exactly_the_size_limit() {
    let base = encode_line(&append(&[0; 4])).unwrap().len();
    let rest = MAX_LINE_BYTES - base - 3 * MAX_BODY_BYTES;
    assert!(rest <= MAX_BODY_BYTES);
    let fits = append(&[MAX_BODY_BYTES, MAX_BODY_BYTES, MAX_BODY_BYTES, rest]);
    assert_eq!(encode_line(&fits).unwrap().len(), MAX_LINE_BYTES);
    let over = append(&[MAX_BODY_BYTES, MAX_BODY_BYTES, MAX_BODY_BYTES, rest + 1]);
    assert_eq!(encode_line(&over), Err(ContractError::TooLong));
}

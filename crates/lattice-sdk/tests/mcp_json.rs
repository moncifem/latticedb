//! The bounded JSON parser of the MCP server: literals, numbers, string escapes with surrogate pairs,
//! and every limit at its exact edge. Mutation testing showed these were not pinned.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use lattice_sdk::mcp::{
    Json, MAX_DEPTH, MAX_ITEMS, MAX_MESSAGE_BYTES, MAX_NODES, ParseError, parse_json,
};

fn parse(text: &str) -> Result<Json, ParseError> {
    parse_json(text.as_bytes())
}

#[test]
fn literals_and_numbers_parse_to_their_values() {
    assert_eq!(parse("true"), Ok(Json::Bool(true)));
    assert_eq!(parse("false"), Ok(Json::Bool(false)));
    assert_eq!(parse("null"), Ok(Json::Null));
    assert_eq!(parse("123"), Ok(Json::Int(123)));
    assert_eq!(parse("-45"), Ok(Json::Int(-45)));
    assert_eq!(parse("1.5e2"), Ok(Json::Float(150.0)));
    assert_eq!(parse("2E-1"), Ok(Json::Float(0.2)));
    assert_eq!(parse("9223372036854775807"), Ok(Json::Int(i64::MAX)));
    // An integer too large for i64 is a float, and a number beyond f64 is refused.
    assert_eq!(
        parse("9223372036854775808"),
        Ok(Json::Float(9.223_372_036_854_776e18))
    );
    assert_eq!(parse("1e999"), Err(ParseError::Syntax));
    for bad in ["tru", "nul", "fals", "-", "1.", "1e", "1e+", ".5", "+1"] {
        assert_eq!(parse(bad), Err(ParseError::Syntax), "{bad}");
    }
}

/// A JSON string literal built from `parts`, where `u(hex)` is a `\u` escape. The backslash is made
/// from its code so that no editor or tool rewrites the escapes while the file is handled.
fn bs() -> char {
    char::from(92u8)
}

fn u(hex: &str) -> String {
    format!("{}u{hex}", bs())
}

fn quoted(body: &str) -> String {
    format!("\"{body}\"")
}

#[test]
fn string_escapes_decode_exactly() {
    assert_eq!(
        parse(&quoted(&format!("a{}x", u("0041")))),
        Ok(Json::Str("aAx".into()))
    );
    assert_eq!(parse(&quoted(&u("00e9"))), Ok(Json::Str("\u{e9}".into())));
    let simple: String = ['b', 'f', 'n', 'r', 't', '"', '\\', '/']
        .iter()
        .map(|c| format!("{}{c}", bs()))
        .collect();
    assert_eq!(
        parse(&quoted(&simple)),
        Ok(Json::Str("\u{8}\u{c}\n\r\t\"\\/".into()))
    );
    // A surrogate pair is one character, with the arithmetic of the UTF-16 rule.
    let pair = |hi: &str, lo: &str| parse(&quoted(&format!("{}{}", u(hi), u(lo))));
    assert_eq!(pair("d83d", "de00"), Ok(Json::Str("\u{1F600}".into())));
    assert_eq!(pair("d800", "dc00"), Ok(Json::Str("\u{10000}".into())));
    assert_eq!(pair("dbff", "dfff"), Ok(Json::Str("\u{10FFFF}".into())));
    assert_eq!(pair("d801", "dc37"), Ok(Json::Str("\u{10437}".into())));
}

#[test]
fn unpaired_or_misordered_surrogates_are_refused() {
    let alone = |hex: &str| parse(&quoted(&u(hex)));
    assert_eq!(
        alone("d83d"),
        Err(ParseError::Syntax),
        "a high surrogate alone"
    );
    assert_eq!(
        alone("de00"),
        Err(ParseError::Syntax),
        "a low surrogate alone"
    );
    assert_eq!(
        alone("dc00"),
        Err(ParseError::Syntax),
        "the first low surrogate alone"
    );
    assert_eq!(
        parse(&quoted(&format!("{}x", u("d83d")))),
        Err(ParseError::Syntax)
    );
    assert_eq!(
        parse(&quoted(&format!("{}{}", u("d83d"), u("0041")))),
        Err(ParseError::Syntax)
    );
    assert_eq!(
        parse(&quoted(&format!("{}{}", u("d83d"), u("d83d")))),
        Err(ParseError::Syntax)
    );
    assert_eq!(alone("12g4"), Err(ParseError::Syntax), "not hex");
    assert_eq!(
        parse(&quoted(&format!("{}12", u("")))),
        Err(ParseError::Syntax),
        "too short"
    );
}

fn nested_arrays(n: usize) -> String {
    format!("{}1{}", "[".repeat(n), "]".repeat(n))
}

fn nested_objects(n: usize) -> String {
    format!("{}1{}", "{\"a\":".repeat(n), "}".repeat(n))
}

#[test]
fn nesting_is_allowed_to_the_limit_for_arrays_and_objects() {
    assert!(parse(&nested_arrays(MAX_DEPTH)).is_ok());
    assert_eq!(
        parse(&nested_arrays(MAX_DEPTH + 1)),
        Err(ParseError::TooDeep)
    );
    assert!(parse(&nested_objects(MAX_DEPTH)).is_ok());
    assert_eq!(
        parse(&nested_objects(MAX_DEPTH + 1)),
        Err(ParseError::TooDeep)
    );
}

#[test]
fn a_document_has_at_most_max_nodes_values() {
    // A root array of 63 arrays of 63 ones and a last array of `r` ones: 1 + 63 * 64 + 1 + r values.
    let nodes = |r: usize| {
        let inner = format!("[{}]", vec!["1"; 63].join(","));
        let mut members = vec![inner; 63];
        members.push(format!("[{}]", vec!["1"; r].join(",")));
        format!("[{}]", members.join(","))
    };
    assert_eq!(1 + 63 * 64 + 1 + 62, MAX_NODES);
    assert!(parse(&nodes(62)).is_ok());
    assert_eq!(parse(&nodes(63)), Err(ParseError::TooMany));
}

#[test]
fn an_array_or_object_has_at_most_max_items_members() {
    let array = |n: usize| format!("[{}]", vec!["1"; n].join(","));
    assert!(parse(&array(MAX_ITEMS)).is_ok());
    assert_eq!(parse(&array(MAX_ITEMS + 1)), Err(ParseError::TooMany));
    let object = |n: usize| {
        let members: Vec<String> = (0..n).map(|i| format!("\"k{i}\":1")).collect();
        format!("{{{}}}", members.join(","))
    };
    assert!(parse(&object(MAX_ITEMS)).is_ok());
    assert_eq!(parse(&object(MAX_ITEMS + 1)), Err(ParseError::TooMany));
    assert_eq!(parse(r#"{"a":1,"a":2}"#), Err(ParseError::DuplicateKey));
}

#[test]
fn a_message_may_be_exactly_the_size_limit() {
    let padded = |total: usize| format!("1{}", " ".repeat(total - 1));
    assert!(parse_json(padded(MAX_MESSAGE_BYTES).as_bytes()).is_ok());
    assert_eq!(
        parse_json(padded(MAX_MESSAGE_BYTES + 1).as_bytes()),
        Err(ParseError::TooLarge)
    );
    assert_eq!(parse_json(&[0xff, 0xfe]), Err(ParseError::InvalidUtf8));
}

#[test]
fn the_documented_limits_are_the_numbers_in_the_documentation() {
    assert_eq!(MAX_MESSAGE_BYTES, 262_144);
    assert_eq!((MAX_DEPTH, MAX_ITEMS, MAX_NODES), (12, 64, 4096));
}

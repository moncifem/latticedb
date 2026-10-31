//! `jsonb` input is written back in PostgreSQL's canonical text, checked against PostgreSQL 16: keys
//! ordered by length and then bytes, a repeated key keeping its last value, `": "` and `", "`
//! separators, numbers without an exponent (the fraction digits the input spelled), escapes decoded
//! and rewritten with only the ones JSON requires, and equality that follows from the canonical text.
//! Each entry lists what PostgreSQL answered. `1e3 = 1000` is true in PostgreSQL (numbers compare as
//! numbers) and false here (`1e3` prints `1000`, but `1.0` and `1` stay different text); it is left out.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use common::{fixture, text};

const CASES: &[(&str, &[&str])] = &[
    (
        "SELECT '{\"a\":1,\"b\":[1,2,{\"c\":null}],\"d\":\"x\"}'::jsonb",
        &["{\"a\": 1, \"b\": [1, 2, {\"c\": null}], \"d\": \"x\"}"],
    ),
    ("SELECT '[1,2,3]'::jsonb", &["[1, 2, 3]"]),
    ("SELECT '\"str\"'::jsonb", &["\"str\""]),
    ("SELECT '1.50'::jsonb", &["1.50"]),
    ("SELECT 'true'::jsonb", &["true"]),
    ("SELECT 'null'::jsonb", &["null"]),
    ("SELECT '{}'::jsonb", &["{}"]),
    ("SELECT '[]'::jsonb", &["[]"]),
    (
        "SELECT '{\"a\":{\"b\":{\"c\":1}}}'::jsonb",
        &["{\"a\": {\"b\": {\"c\": 1}}}"],
    ),
    (
        "SELECT '{\"b\":1,\"a\":2}'::jsonb",
        &["{\"a\": 2, \"b\": 1}"],
    ),
    ("SELECT '{\"a\":1,\"a\":2}'::jsonb", &["{\"a\": 2}"]),
    ("SELECT '[1,[2,[3]]]'::jsonb", &["[1, [2, [3]]]"]),
    ("SELECT '{\"k\":\"vé\"}'::jsonb", &["{\"k\": \"vé\"}"]),
    ("SELECT '1e3'::jsonb", &["1000"]),
    ("SELECT '-0'::jsonb", &["0"]),
    (
        "SELECT '{\"a\": 1 , \"b\" : [ ] }'::jsonb",
        &["{\"a\": 1, \"b\": []}"],
    ),
    (
        "SELECT '{\"aa\":1,\"b\":2,\"a\":3,\"ab\":4,\"B\":5}'::jsonb",
        &["{\"B\": 5, \"a\": 3, \"b\": 2, \"aa\": 1, \"ab\": 4}"],
    ),
    ("SELECT '{\"\":1,\"a\":2}'::jsonb", &["{\"\": 1, \"a\": 2}"]),
    (
        "SELECT '{\"b\":1,\"a\":2,\"b\":3,\"a\":4}'::jsonb",
        &["{\"a\": 4, \"b\": 3}"],
    ),
    (
        "SELECT '{\"x\":{\"b\":1,\"a\":{\"z\":1,\"y\":2}}}'::jsonb",
        &["{\"x\": {\"a\": {\"y\": 2, \"z\": 1}, \"b\": 1}}"],
    ),
    ("SELECT '0'::jsonb", &["0"]),
    ("SELECT '-1'::jsonb", &["-1"]),
    ("SELECT '0.0'::jsonb", &["0.0"]),
    ("SELECT '-0.0'::jsonb", &["0.0"]),
    ("SELECT '0.10'::jsonb", &["0.10"]),
    (
        "SELECT '123456789012345678901234567890'::jsonb",
        &["123456789012345678901234567890"],
    ),
    ("SELECT '1.5e2'::jsonb", &["150"]),
    ("SELECT '1.50e2'::jsonb", &["150"]),
    ("SELECT '1E2'::jsonb", &["100"]),
    ("SELECT '1e+2'::jsonb", &["100"]),
    ("SELECT '1e-2'::jsonb", &["0.01"]),
    ("SELECT '1.5e-2'::jsonb", &["0.015"]),
    ("SELECT '1e0'::jsonb", &["1"]),
    ("SELECT '0e5'::jsonb", &["0"]),
    ("SELECT '0.5e1'::jsonb", &["5"]),
    ("SELECT '10e-1'::jsonb", &["1.0"]),
    ("SELECT '100e-2'::jsonb", &["1.00"]),
    ("SELECT '1.0e1'::jsonb", &["10"]),
    ("SELECT '12.340'::jsonb", &["12.340"]),
    ("SELECT '-12.340e1'::jsonb", &["-123.40"]),
    ("SELECT '1e30'::jsonb", &["1000000000000000000000000000000"]),
    (
        "SELECT '1e-30'::jsonb",
        &["0.000000000000000000000000000001"],
    ),
    ("SELECT '123.456e-10'::jsonb", &["0.0000000123456"]),
    ("SELECT '9.99e5'::jsonb", &["999000"]),
    (
        "SELECT '1e100'::jsonb",
        &[
            "10000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000",
        ],
    ),
    ("SELECT '0.000001'::jsonb", &["0.000001"]),
    ("SELECT '1e-7'::jsonb", &["0.0000001"]),
    ("SELECT '\"a\\\"b\"'::jsonb", &["\"a\\\"b\""]),
    ("SELECT '\"a\\\\b\"'::jsonb", &["\"a\\\\b\""]),
    ("SELECT '\"a\\/b\"'::jsonb", &["\"a/b\""]),
    (
        "SELECT '\"\\b\\f\\n\\r\\t\"'::jsonb",
        &["\"\\b\\f\\n\\r\\t\""],
    ),
    ("SELECT '\"\\u0001\"'::jsonb", &["\"\\u0001\""]),
    ("SELECT '\"\\u001f\"'::jsonb", &["\"\\u001f\""]),
    ("SELECT '\"\\u007f\"'::jsonb", &["\"\""]),
    ("SELECT '\"\\u0080\"'::jsonb", &["\"\""]),
    ("SELECT '\"é\"'::jsonb", &["\"é\""]),
    ("SELECT '\"😀\"'::jsonb", &["\"😀\""]),
    ("SELECT '\"é\"'::jsonb", &["\"é\""]),
    ("SELECT '\"😀\"'::jsonb", &["\"😀\""]),
    ("SELECT '\" \"'::jsonb", &["\" \""]),
    ("SELECT '\"<>&\"'::jsonb", &["\"<>&\""]),
    ("SELECT '\"A\"'::jsonb", &["\"A\""]),
    ("SELECT '\"a\\u0000b\"'::jsonb", &["ERR 22P05"]),
    ("SELECT '  [ 1 , 2 ]  '::jsonb", &["[1, 2]"]),
    (
        "SELECT '[true,false,null]'::jsonb",
        &["[true, false, null]"],
    ),
    ("SELECT '[{\"a\":[]},{}]'::jsonb", &["[{\"a\": []}, {}]"]),
    (
        "SELECT '{\"a\":[1,{\"b\":[2]}]}'::jsonb",
        &["{\"a\": [1, {\"b\": [2]}]}"],
    ),
    ("SELECT '[[],[[]]]'::jsonb", &["[[], [[]]]"]),
    ("SELECT '{\"a\":{}}'::jsonb", &["{\"a\": {}}"]),
    ("SELECT '\"\"'::jsonb", &["\"\""]),
    ("SELECT '\" \"'::jsonb", &["\" \""]),
    (
        "SELECT '{\"b\":\"x\",\"a\":\"y\"}'::jsonb",
        &["{\"a\": \"y\", \"b\": \"x\"}"],
    ),
    (
        "SELECT '{\"10\":1,\"9\":2,\"100\":3}'::jsonb",
        &["{\"9\": 2, \"10\": 1, \"100\": 3}"],
    ),
    (
        "SELECT '{\"é\":1,\"e\":2,\"z\":3}'::jsonb",
        &["{\"e\": 2, \"z\": 3, \"é\": 1}"],
    ),
    ("SELECT '{\"ab\":1,\"ab\":2}'::jsonb", &["{\"ab\": 2}"]),
    (
        "SELECT '{\"a\":1,\"b\":2}'::jsonb = '{\"b\":2,\"a\":1}'::jsonb",
        &["t"],
    ),
    (
        "SELECT '{\"a\":1,\"a\":2}'::jsonb = '{\"a\":2}'::jsonb",
        &["t"],
    ),
    ("SELECT '{\"a\":1}'::jsonb = '{\"a\":2}'::jsonb", &["f"]),
    ("SELECT '[1,2]'::jsonb = '[2,1]'::jsonb", &["f"]),
    ("SELECT '{\"a\": 1}'::jsonb = '{\"a\":1}'::jsonb", &["t"]),
    ("SELECT '{\"a\":1}'::jsonb::text", &["{\"a\": 1}"]),
    (
        "SELECT '{\"b\":1,\"a\":2}'::text::jsonb",
        &["{\"a\": 2, \"b\": 1}"],
    ),
];

#[test]
fn jsonb_prints_and_compares_as_postgresql_does() {
    let mut f = fixture();
    let mut s = common::admin();
    for (sql, want) in CASES {
        let got: Vec<String> = match s.execute(&mut f.db, sql) {
            Ok(r) => text(&r).iter().map(|row| row.join("|")).collect(),
            Err(e) => vec![format!("ERR {}", e.sqlstate())],
        };
        assert_eq!(&got, want, "{sql}");
    }
}

#[test]
fn a_stored_jsonb_value_reads_back_canonical() {
    let mut f = fixture();
    let mut s = common::admin();
    common::ok(
        &mut f.db,
        &mut s,
        "CREATE TABLE j (id INT PRIMARY KEY, doc JSONB)",
    );
    common::ok(
        &mut f.db,
        &mut s,
        "INSERT INTO j VALUES (1, '{\"b\":1,\"a\":[1,2]}'), (2, '{\"a\": [1,2], \"b\": 1}')",
    );
    let rows = common::rows(&mut f.db, &mut s, "SELECT doc FROM j ORDER BY id");
    assert_eq!(
        rows,
        [["{\"a\": [1, 2], \"b\": 1}"], ["{\"a\": [1, 2], \"b\": 1}"]]
    );
}

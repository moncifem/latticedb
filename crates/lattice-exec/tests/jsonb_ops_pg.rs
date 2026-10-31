//! The jsonb operators `->`, `->>` and `@>` checked against PostgreSQL 16: members by key and by
//! index (negative from the end, a scalar read as a one element array, `NULL` for a miss), text
//! extraction, containment of objects, arrays and scalars, chains, and `NULL` operands. Each entry lists
//! what PostgreSQL answered (`NULL` is an empty field). Left out: `1.0 = 1` (true in PostgreSQL, text
//! equality here) and an index of type `bigint` (`42883` in PostgreSQL, accepted here).
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
    ("SELECT '{\"k\":\"v\\u00e9\"}'::jsonb", &["{\"k\": \"vé\"}"]),
    ("SELECT '1e3'::jsonb", &["1000"]),
    ("SELECT '-0'::jsonb", &["0"]),
    (
        "SELECT '{\"a\": 1 , \"b\" : [ ] }'::jsonb",
        &["{\"a\": 1, \"b\": []}"],
    ),
    (
        "SELECT '{\"a\":1,\"b\":[1,2,{\"c\":null}],\"d\":\"x\"}'::jsonb -> 'a'",
        &["1"],
    ),
    (
        "SELECT '{\"a\":1,\"b\":[1,2,{\"c\":null}],\"d\":\"x\"}'::jsonb -> 0",
        &[""],
    ),
    (
        "SELECT '{\"a\":1,\"b\":[1,2,{\"c\":null}],\"d\":\"x\"}'::jsonb -> 1",
        &[""],
    ),
    (
        "SELECT '{\"a\":1,\"b\":[1,2,{\"c\":null}],\"d\":\"x\"}'::jsonb ->> 'a'",
        &["1"],
    ),
    (
        "SELECT '{\"a\":1,\"b\":[1,2,{\"c\":null}],\"d\":\"x\"}'::jsonb ->> 0",
        &[""],
    ),
    (
        "SELECT '{\"a\":1,\"b\":[1,2,{\"c\":null}],\"d\":\"x\"}'::jsonb -> 'zz'",
        &[""],
    ),
    (
        "SELECT '{\"a\":1,\"b\":[1,2,{\"c\":null}],\"d\":\"x\"}'::jsonb -> 5",
        &[""],
    ),
    (
        "SELECT '{\"a\":1,\"b\":[1,2,{\"c\":null}],\"d\":\"x\"}'::jsonb ->> 'b'",
        &["[1, 2, {\"c\": null}]"],
    ),
    ("SELECT '[1,2,3]'::jsonb -> 'a'", &[""]),
    ("SELECT '[1,2,3]'::jsonb -> 0", &["1"]),
    ("SELECT '[1,2,3]'::jsonb -> 1", &["2"]),
    ("SELECT '[1,2,3]'::jsonb ->> 'a'", &[""]),
    ("SELECT '[1,2,3]'::jsonb ->> 0", &["1"]),
    ("SELECT '[1,2,3]'::jsonb -> 'zz'", &[""]),
    ("SELECT '[1,2,3]'::jsonb -> 5", &[""]),
    ("SELECT '[1,2,3]'::jsonb ->> 'b'", &[""]),
    ("SELECT '\"str\"'::jsonb -> 'a'", &[""]),
    ("SELECT '\"str\"'::jsonb -> 0", &["\"str\""]),
    ("SELECT '\"str\"'::jsonb -> 1", &[""]),
    ("SELECT '\"str\"'::jsonb ->> 'a'", &[""]),
    ("SELECT '\"str\"'::jsonb ->> 0", &["str"]),
    ("SELECT '\"str\"'::jsonb -> 'zz'", &[""]),
    ("SELECT '\"str\"'::jsonb -> 5", &[""]),
    ("SELECT '\"str\"'::jsonb ->> 'b'", &[""]),
    ("SELECT '1.50'::jsonb -> 'a'", &[""]),
    ("SELECT '1.50'::jsonb -> 0", &["1.50"]),
    ("SELECT '1.50'::jsonb -> 1", &[""]),
    ("SELECT '1.50'::jsonb ->> 'a'", &[""]),
    ("SELECT '1.50'::jsonb ->> 0", &["1.50"]),
    ("SELECT '1.50'::jsonb -> 'zz'", &[""]),
    ("SELECT '1.50'::jsonb -> 5", &[""]),
    ("SELECT '1.50'::jsonb ->> 'b'", &[""]),
    (
        "SELECT '{\"a\":{\"b\":{\"c\":1}}}'::jsonb -> 'a'",
        &["{\"b\": {\"c\": 1}}"],
    ),
    ("SELECT '{\"a\":{\"b\":{\"c\":1}}}'::jsonb -> 0", &[""]),
    ("SELECT '{\"a\":{\"b\":{\"c\":1}}}'::jsonb -> 1", &[""]),
    (
        "SELECT '{\"a\":{\"b\":{\"c\":1}}}'::jsonb ->> 'a'",
        &["{\"b\": {\"c\": 1}}"],
    ),
    ("SELECT '{\"a\":{\"b\":{\"c\":1}}}'::jsonb ->> 0", &[""]),
    ("SELECT '{\"a\":{\"b\":{\"c\":1}}}'::jsonb -> 'zz'", &[""]),
    ("SELECT '{\"a\":{\"b\":{\"c\":1}}}'::jsonb -> 5", &[""]),
    ("SELECT '{\"a\":{\"b\":{\"c\":1}}}'::jsonb ->> 'b'", &[""]),
    ("SELECT '{\"b\":1,\"a\":2}'::jsonb -> 'a'", &["2"]),
    ("SELECT '{\"b\":1,\"a\":2}'::jsonb -> 0", &[""]),
    ("SELECT '{\"b\":1,\"a\":2}'::jsonb -> 1", &[""]),
    ("SELECT '{\"b\":1,\"a\":2}'::jsonb ->> 'a'", &["2"]),
    ("SELECT '{\"b\":1,\"a\":2}'::jsonb ->> 0", &[""]),
    ("SELECT '{\"b\":1,\"a\":2}'::jsonb -> 'zz'", &[""]),
    ("SELECT '{\"b\":1,\"a\":2}'::jsonb -> 5", &[""]),
    ("SELECT '{\"b\":1,\"a\":2}'::jsonb ->> 'b'", &["1"]),
    ("SELECT '{\"a\":1}'::jsonb @> '{\"a\":1}'::jsonb", &["t"]),
    (
        "SELECT '{\"a\":1,\"b\":2}'::jsonb @> '{\"a\":1}'::jsonb",
        &["t"],
    ),
    (
        "SELECT '{\"a\":1}'::jsonb @> '{\"a\":1,\"b\":2}'::jsonb",
        &["f"],
    ),
    ("SELECT '[1,2,3]'::jsonb @> '[2]'::jsonb", &["t"]),
    ("SELECT '[1,2,3]'::jsonb @> '[4]'::jsonb", &["f"]),
    (
        "SELECT '{\"a\":[1,2]}'::jsonb @> '{\"a\":[2]}'::jsonb",
        &["t"],
    ),
    ("SELECT '1'::jsonb @> '1'::jsonb", &["t"]),
    ("SELECT '{\"a\":1}'::jsonb = '{\"a\":1}'::jsonb", &["t"]),
    (
        "SELECT '{\"a\":1,\"b\":2}'::jsonb = '{\"b\":2,\"a\":1}'::jsonb",
        &["t"],
    ),
    ("SELECT '[1,2]'::jsonb = '[2,1]'::jsonb", &["f"]),
    ("SELECT '{\"a\":1}'::jsonb <> '{\"a\":2}'::jsonb", &["t"]),
    ("SELECT '{\"a\":1}'::jsonb::text", &["{\"a\": 1}"]),
    ("SELECT '{\"a\":1}'::text::jsonb", &["{\"a\": 1}"]),
    ("SELECT NULL::jsonb", &[""]),
    ("SELECT '{\"a\":null}'::jsonb -> 'a'", &["null"]),
    ("SELECT '{\"a\":null}'::jsonb ->> 'a'", &[""]),
    ("SELECT '{\"a\":\"x\"}'::jsonb ->> 'a'", &["x"]),
    ("SELECT '{\"a\":\"x\"}'::jsonb -> 'a'", &["\"x\""]),
    ("SELECT '{\"a\":[1]}'::jsonb ->> 'a'", &["[1]"]),
    ("SELECT '{\"a\":1.50}'::jsonb ->> 'a'", &["1.50"]),
    ("SELECT '{\"a\":true}'::jsonb ->> 'a'", &["true"]),
    ("SELECT '[10,20,30]'::jsonb -> -1", &["30"]),
    ("SELECT '[10,20,30]'::jsonb -> -2", &["20"]),
    ("SELECT '[10,20,30]'::jsonb -> -4", &[""]),
    ("SELECT '[10,20,30]'::jsonb ->> -1", &["30"]),
    ("SELECT '[10,20,30]'::jsonb -> 2", &["30"]),
    ("SELECT '[10,20,30]'::jsonb ->> 2", &["30"]),
    ("SELECT '[10,20,30]'::jsonb -> 0", &["10"]),
    ("SELECT '[10,20,30]'::jsonb -> 'a'", &[""]),
    ("SELECT '[10,20,30]'::jsonb ->> 'a'", &[""]),
    ("SELECT '[10,20,30]'::jsonb ->> 'c'", &[""]),
    ("SELECT '[10,20,30]'::jsonb -> 'b'", &[""]),
    (
        "SELECT '{\"a\":[1,{\"b\":2}],\"c\":\"x\"}'::jsonb -> -1",
        &[""],
    ),
    (
        "SELECT '{\"a\":[1,{\"b\":2}],\"c\":\"x\"}'::jsonb -> -2",
        &[""],
    ),
    (
        "SELECT '{\"a\":[1,{\"b\":2}],\"c\":\"x\"}'::jsonb -> -4",
        &[""],
    ),
    (
        "SELECT '{\"a\":[1,{\"b\":2}],\"c\":\"x\"}'::jsonb ->> -1",
        &[""],
    ),
    (
        "SELECT '{\"a\":[1,{\"b\":2}],\"c\":\"x\"}'::jsonb -> 2",
        &[""],
    ),
    (
        "SELECT '{\"a\":[1,{\"b\":2}],\"c\":\"x\"}'::jsonb ->> 2",
        &[""],
    ),
    (
        "SELECT '{\"a\":[1,{\"b\":2}],\"c\":\"x\"}'::jsonb -> 0",
        &[""],
    ),
    (
        "SELECT '{\"a\":[1,{\"b\":2}],\"c\":\"x\"}'::jsonb -> 'a'",
        &["[1, {\"b\": 2}]"],
    ),
    (
        "SELECT '{\"a\":[1,{\"b\":2}],\"c\":\"x\"}'::jsonb ->> 'a'",
        &["[1, {\"b\": 2}]"],
    ),
    (
        "SELECT '{\"a\":[1,{\"b\":2}],\"c\":\"x\"}'::jsonb ->> 'c'",
        &["x"],
    ),
    (
        "SELECT '{\"a\":[1,{\"b\":2}],\"c\":\"x\"}'::jsonb -> 'b'",
        &[""],
    ),
    ("SELECT '[[1,2],[3]]'::jsonb -> -1", &["[3]"]),
    ("SELECT '[[1,2],[3]]'::jsonb -> -2", &["[1, 2]"]),
    ("SELECT '[[1,2],[3]]'::jsonb -> -4", &[""]),
    ("SELECT '[[1,2],[3]]'::jsonb ->> -1", &["[3]"]),
    ("SELECT '[[1,2],[3]]'::jsonb -> 2", &[""]),
    ("SELECT '[[1,2],[3]]'::jsonb ->> 2", &[""]),
    ("SELECT '[[1,2],[3]]'::jsonb -> 0", &["[1, 2]"]),
    ("SELECT '[[1,2],[3]]'::jsonb -> 'a'", &[""]),
    ("SELECT '[[1,2],[3]]'::jsonb ->> 'a'", &[""]),
    ("SELECT '[[1,2],[3]]'::jsonb ->> 'c'", &[""]),
    ("SELECT '[[1,2],[3]]'::jsonb -> 'b'", &[""]),
    (
        "SELECT '[{\"a\":1},{\"a\":2,\"b\":3}]'::jsonb -> -1",
        &["{\"a\": 2, \"b\": 3}"],
    ),
    (
        "SELECT '[{\"a\":1},{\"a\":2,\"b\":3}]'::jsonb -> -2",
        &["{\"a\": 1}"],
    ),
    ("SELECT '[{\"a\":1},{\"a\":2,\"b\":3}]'::jsonb -> -4", &[""]),
    (
        "SELECT '[{\"a\":1},{\"a\":2,\"b\":3}]'::jsonb ->> -1",
        &["{\"a\": 2, \"b\": 3}"],
    ),
    ("SELECT '[{\"a\":1},{\"a\":2,\"b\":3}]'::jsonb -> 2", &[""]),
    ("SELECT '[{\"a\":1},{\"a\":2,\"b\":3}]'::jsonb ->> 2", &[""]),
    (
        "SELECT '[{\"a\":1},{\"a\":2,\"b\":3}]'::jsonb -> 0",
        &["{\"a\": 1}"],
    ),
    (
        "SELECT '[{\"a\":1},{\"a\":2,\"b\":3}]'::jsonb -> 'a'",
        &[""],
    ),
    (
        "SELECT '[{\"a\":1},{\"a\":2,\"b\":3}]'::jsonb ->> 'a'",
        &[""],
    ),
    (
        "SELECT '[{\"a\":1},{\"a\":2,\"b\":3}]'::jsonb ->> 'c'",
        &[""],
    ),
    (
        "SELECT '[{\"a\":1},{\"a\":2,\"b\":3}]'::jsonb -> 'b'",
        &[""],
    ),
    ("SELECT '\"s\"'::jsonb -> -1", &["\"s\""]),
    ("SELECT '\"s\"'::jsonb -> -2", &[""]),
    ("SELECT '\"s\"'::jsonb -> -4", &[""]),
    ("SELECT '\"s\"'::jsonb ->> -1", &["s"]),
    ("SELECT '\"s\"'::jsonb -> 2", &[""]),
    ("SELECT '\"s\"'::jsonb ->> 2", &[""]),
    ("SELECT '\"s\"'::jsonb -> 0", &["\"s\""]),
    ("SELECT '\"s\"'::jsonb -> 'a'", &[""]),
    ("SELECT '\"s\"'::jsonb ->> 'a'", &[""]),
    ("SELECT '\"s\"'::jsonb ->> 'c'", &[""]),
    ("SELECT '\"s\"'::jsonb -> 'b'", &[""]),
    ("SELECT '5'::jsonb -> -1", &["5"]),
    ("SELECT '5'::jsonb -> -2", &[""]),
    ("SELECT '5'::jsonb -> -4", &[""]),
    ("SELECT '5'::jsonb ->> -1", &["5"]),
    ("SELECT '5'::jsonb -> 2", &[""]),
    ("SELECT '5'::jsonb ->> 2", &[""]),
    ("SELECT '5'::jsonb -> 0", &["5"]),
    ("SELECT '5'::jsonb -> 'a'", &[""]),
    ("SELECT '5'::jsonb ->> 'a'", &[""]),
    ("SELECT '5'::jsonb ->> 'c'", &[""]),
    ("SELECT '5'::jsonb -> 'b'", &[""]),
    ("SELECT 'null'::jsonb -> -1", &["null"]),
    ("SELECT 'null'::jsonb -> -2", &[""]),
    ("SELECT 'null'::jsonb -> -4", &[""]),
    ("SELECT 'null'::jsonb ->> -1", &[""]),
    ("SELECT 'null'::jsonb -> 2", &[""]),
    ("SELECT 'null'::jsonb ->> 2", &[""]),
    ("SELECT 'null'::jsonb -> 0", &["null"]),
    ("SELECT 'null'::jsonb -> 'a'", &[""]),
    ("SELECT 'null'::jsonb ->> 'a'", &[""]),
    ("SELECT 'null'::jsonb ->> 'c'", &[""]),
    ("SELECT 'null'::jsonb -> 'b'", &[""]),
    ("SELECT 'true'::jsonb -> -1", &["true"]),
    ("SELECT 'true'::jsonb -> -2", &[""]),
    ("SELECT 'true'::jsonb -> -4", &[""]),
    ("SELECT 'true'::jsonb ->> -1", &["true"]),
    ("SELECT 'true'::jsonb -> 2", &[""]),
    ("SELECT 'true'::jsonb ->> 2", &[""]),
    ("SELECT 'true'::jsonb -> 0", &["true"]),
    ("SELECT 'true'::jsonb -> 'a'", &[""]),
    ("SELECT 'true'::jsonb ->> 'a'", &[""]),
    ("SELECT 'true'::jsonb ->> 'c'", &[""]),
    ("SELECT 'true'::jsonb -> 'b'", &[""]),
    (
        "SELECT '{\"a\":null,\"b\":\"\",\"c\":\"é\\n\"}'::jsonb -> -1",
        &[""],
    ),
    (
        "SELECT '{\"a\":null,\"b\":\"\",\"c\":\"é\\n\"}'::jsonb -> -2",
        &[""],
    ),
    (
        "SELECT '{\"a\":null,\"b\":\"\",\"c\":\"é\\n\"}'::jsonb -> -4",
        &[""],
    ),
    (
        "SELECT '{\"a\":null,\"b\":\"\",\"c\":\"é\\n\"}'::jsonb ->> -1",
        &[""],
    ),
    (
        "SELECT '{\"a\":null,\"b\":\"\",\"c\":\"é\\n\"}'::jsonb -> 2",
        &[""],
    ),
    (
        "SELECT '{\"a\":null,\"b\":\"\",\"c\":\"é\\n\"}'::jsonb ->> 2",
        &[""],
    ),
    (
        "SELECT '{\"a\":null,\"b\":\"\",\"c\":\"é\\n\"}'::jsonb -> 0",
        &[""],
    ),
    (
        "SELECT '{\"a\":null,\"b\":\"\",\"c\":\"é\\n\"}'::jsonb -> 'a'",
        &["null"],
    ),
    (
        "SELECT '{\"a\":null,\"b\":\"\",\"c\":\"é\\n\"}'::jsonb ->> 'a'",
        &[""],
    ),
    (
        "SELECT '{\"a\":null,\"b\":\"\",\"c\":\"é\\n\"}'::jsonb ->> 'c'",
        &["é\n"],
    ),
    (
        "SELECT '{\"a\":null,\"b\":\"\",\"c\":\"é\\n\"}'::jsonb -> 'b'",
        &["\"\""],
    ),
    ("SELECT '[1,2,3]'::jsonb @> '[1]'::jsonb", &["t"]),
    ("SELECT '[1,2,3]'::jsonb @> '[]'::jsonb", &["t"]),
    ("SELECT '[]'::jsonb @> '[]'::jsonb", &["t"]),
    ("SELECT '[1,[2,3]]'::jsonb @> '[[2]]'::jsonb", &["t"]),
    ("SELECT '[1,[2,3]]'::jsonb @> '[2]'::jsonb", &["f"]),
    ("SELECT '[[1,2],[3]]'::jsonb @> '[[1],[3]]'::jsonb", &["t"]),
    (
        "SELECT '[{\"a\":1},{\"a\":2,\"b\":3}]'::jsonb @> '[{\"b\":3}]'::jsonb",
        &["t"],
    ),
    (
        "SELECT '[{\"a\":1}]'::jsonb @> '[{\"a\":1,\"b\":2}]'::jsonb",
        &["f"],
    ),
    (
        "SELECT '{\"a\":{\"b\":1,\"c\":2}}'::jsonb @> '{\"a\":{\"b\":1}}'::jsonb",
        &["t"],
    ),
    (
        "SELECT '{\"a\":{\"b\":1}}'::jsonb @> '{\"a\":{\"b\":1,\"c\":2}}'::jsonb",
        &["f"],
    ),
    (
        "SELECT '{\"a\":[1,2]}'::jsonb @> '{\"a\":[1]}'::jsonb",
        &["t"],
    ),
    ("SELECT '{\"a\":1}'::jsonb @> '{\"b\":1}'::jsonb", &["f"]),
    ("SELECT '{}'::jsonb @> '{}'::jsonb", &["t"]),
    ("SELECT '{\"a\":1}'::jsonb @> '{}'::jsonb", &["t"]),
    ("SELECT '[1,2]'::jsonb @> '\"x\"'::jsonb", &["f"]),
    ("SELECT '[\"x\",\"y\"]'::jsonb @> '\"x\"'::jsonb", &["t"]),
    ("SELECT '[\"x\",\"y\"]'::jsonb @> '\"z\"'::jsonb", &["f"]),
    ("SELECT '[null]'::jsonb @> 'null'::jsonb", &["t"]),
    ("SELECT 'null'::jsonb @> 'null'::jsonb", &["t"]),
    ("SELECT '\"a\"'::jsonb @> '\"a\"'::jsonb", &["t"]),
    ("SELECT '\"a\"'::jsonb @> '\"b\"'::jsonb", &["f"]),
    ("SELECT '1'::jsonb @> '1'::jsonb", &["t"]),
    ("SELECT '1'::jsonb @> '2'::jsonb", &["f"]),
    ("SELECT 'true'::jsonb @> 'true'::jsonb", &["t"]),
    ("SELECT '[1,2]'::jsonb @> '{\"a\":1}'::jsonb", &["f"]),
    ("SELECT '{\"a\":1}'::jsonb @> '[1]'::jsonb", &["f"]),
    ("SELECT '{\"a\":[]}'::jsonb @> '{\"a\":[]}'::jsonb", &["t"]),
    (
        "SELECT '{\"a\":\"1\"}'::jsonb @> '{\"a\":1}'::jsonb",
        &["f"],
    ),
    ("SELECT '[true]'::jsonb @> 'true'::jsonb", &["t"]),
    ("SELECT '[1,2,3]'::jsonb @> '3'::jsonb", &["t"]),
    ("SELECT '[[1]]'::jsonb @> '1'::jsonb", &["f"]),
    (
        "SELECT '{\"a\":[[1,2]]}'::jsonb @> '{\"a\":[[2]]}'::jsonb",
        &["t"],
    ),
    ("SELECT '[1,2,3]'::jsonb @> '[3,2,1]'::jsonb", &["t"]),
    ("SELECT '[1,1,1]'::jsonb @> '[1,1]'::jsonb", &["t"]),
    ("SELECT NULL::jsonb -> 'a'", &[""]),
    ("SELECT '{\"a\":1}'::jsonb -> NULL::text", &[""]),
    ("SELECT NULL::jsonb @> '{}'::jsonb", &[""]),
    ("SELECT '{}'::jsonb @> NULL::jsonb", &[""]),
    (
        "SELECT '{\"a\":{\"b\":[1,2,3]}}'::jsonb -> 'a' -> 'b' -> 1",
        &["2"],
    ),
    (
        "SELECT '{\"a\":{\"b\":[1,2,3]}}'::jsonb -> 'a' ->> 'b'",
        &["[1, 2, 3]"],
    ),
    ("SELECT ('{\"a\":1}'::jsonb -> 'a') = '1'::jsonb", &["t"]),
    ("SELECT '{\"a\":1}'::jsonb ->> 1", &[""]),
    ("SELECT '[1]'::jsonb -> 'x'::text", &[""]),
    ("SELECT '[1,2]'::jsonb -> 1::smallint", &["2"]),
];

#[test]
fn jsonb_operators_answer_as_postgresql_does() {
    let mut f = fixture();
    let mut s = common::admin();
    for (sql, want) in CASES {
        let got: Vec<String> = match s.execute(&mut f.db, sql) {
            Ok(r) => text(&r)
                .iter()
                .map(|row| {
                    row.iter()
                        .map(|c| if c == "NULL" { "" } else { c.as_str() })
                        .collect::<Vec<_>>()
                        .join("|")
                })
                .collect(),
            Err(e) => vec![format!("ERR {}", e.sqlstate())],
        };
        assert_eq!(&got, want, "{sql}");
    }
}

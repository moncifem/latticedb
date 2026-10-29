//! The declared-subset scripts under `tests/slt/` stay readable by the runner's reader, so a
//! typo in a script is caught before an engine is wired in.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use lattice_testkit::slt::{Record, parse};

const SCRIPTS: [(&str, &str); 21] = [
    ("basics.test", include_str!("slt/basics.test")),
    ("joins.test", include_str!("slt/joins.test")),
    ("aggregates.test", include_str!("slt/aggregates.test")),
    ("changes.test", include_str!("slt/changes.test")),
    ("transactions.test", include_str!("slt/transactions.test")),
    ("nulls_logic.test", include_str!("slt/nulls_logic.test")),
    (
        "nulls_functions.test",
        include_str!("slt/nulls_functions.test"),
    ),
    (
        "arithmetic_errors.test",
        include_str!("slt/arithmetic_errors.test"),
    ),
    ("strings.test", include_str!("slt/strings.test")),
    ("order_nulls.test", include_str!("slt/order_nulls.test")),
    ("limit_offset.test", include_str!("slt/limit_offset.test")),
    (
        "empty_aggregates.test",
        include_str!("slt/empty_aggregates.test"),
    ),
    ("joins_small.test", include_str!("slt/joins_small.test")),
    ("returning.test", include_str!("slt/returning.test")),
    (
        "transactions_visibility.test",
        include_str!("slt/transactions_visibility.test"),
    ),
    ("constraints.test", include_str!("slt/constraints.test")),
    (
        "ddl_statements.test",
        include_str!("slt/ddl_statements.test"),
    ),
    (
        "prepared_forms.test",
        include_str!("slt/prepared_forms.test"),
    ),
    ("subqueries.test", include_str!("slt/subqueries.test")),
    ("unsupported.test", include_str!("slt/unsupported.test")),
    (
        "types_and_casts.test",
        include_str!("slt/types_and_casts.test"),
    ),
];

#[test]
fn every_subset_script_parses_and_has_records() {
    for (name, text) in SCRIPTS {
        let records = parse(text, "lattice").unwrap_or_else(|e| panic!("{name}: {e:?}"));
        assert!(
            records.iter().any(|r| matches!(r, Record::Query { .. })),
            "{name} has no query"
        );
        assert!(
            records
                .iter()
                .any(|r| matches!(r, Record::StatementError { .. })),
            "{name} has no error case"
        );
    }
}

#[test]
fn every_generated_query_parses_with_the_sql_crate() {
    use lattice_sql::parse_statement;
    use lattice_testkit::slt::generate;
    let mut queries = 0;
    for seed in 0..100 {
        for record in parse(&generate(seed, 25), "lattice").unwrap() {
            let sql = match record {
                Record::StatementOk { sql, .. } | Record::Query { sql, .. } => sql,
                _ => continue,
            };
            parse_statement(&sql).unwrap_or_else(|e| panic!("seed {seed}: {sql}: {e:?}"));
            queries += 1;
        }
    }
    assert!(queries > 2500);
}

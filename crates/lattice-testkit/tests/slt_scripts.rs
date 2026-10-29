//! The declared-subset scripts under `tests/slt/` stay readable by the runner's reader, so a
//! typo in a script is caught before an engine is wired in.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use lattice_testkit::slt::{Record, parse};

const SCRIPTS: [(&str, &str); 1] = [("basics.test", include_str!("slt/basics.test"))];

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

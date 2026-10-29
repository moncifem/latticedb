//! The declared-subset sqllogictest scripts run through the executor.
//!
//! Every script is read from `lattice-testkit` unchanged and run against a fresh database. The
//! records that are expected not to pass are listed in `slt_expected_failures.txt` with the reason;
//! the test fails when a record outside that list fails and also when a listed record passes, so the
//! list can only shrink as the executor grows.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use std::collections::BTreeMap;

use common::{Db, fixture};
use lattice_exec::{Datum, ExecSession};
use lattice_testkit::slt::{ExecError, Executor, Outcome, Record, parse, run};

const SCRIPTS: [(&str, &str); 5] = [
    (
        "basics.test",
        include_str!("../../lattice-testkit/tests/slt/basics.test"),
    ),
    (
        "joins.test",
        include_str!("../../lattice-testkit/tests/slt/joins.test"),
    ),
    (
        "aggregates.test",
        include_str!("../../lattice-testkit/tests/slt/aggregates.test"),
    ),
    (
        "changes.test",
        include_str!("../../lattice-testkit/tests/slt/changes.test"),
    ),
    (
        "transactions.test",
        include_str!("../../lattice-testkit/tests/slt/transactions.test"),
    ),
];

const EXPECTED: &str = include_str!("slt_expected_failures.txt");

struct Runner {
    db: Db,
    session: ExecSession,
}

impl Executor for Runner {
    fn execute(&mut self, sql: &str) -> Result<Outcome, ExecError> {
        match self.session.execute(&mut self.db, sql) {
            Err(e) => Err(ExecError {
                code: e.sqlstate().to_string(),
                message: e.message().to_string(),
            }),
            Ok(r) if r.columns.is_empty() => Ok(Outcome::Done),
            Ok(r) => Ok(Outcome::Rows(
                r.rows
                    .iter()
                    .map(|row| {
                        row.iter()
                            .map(|d| match d {
                                Datum::Null => "NULL".to_string(),
                                Datum::Float(f) => format!("{f:.3}"),
                                other => match other.to_pg_text() {
                                    Some(t) if t.is_empty() => "(empty)".to_string(),
                                    Some(t) => t,
                                    None => "NULL".to_string(),
                                },
                            })
                            .collect()
                    })
                    .collect(),
            )),
        }
    }
}

/// `script:line reason` lines of the expected-failure file, by (script, line).
fn expected() -> BTreeMap<(String, usize), String> {
    EXPECTED
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
        .map(|l| {
            let (key, reason) = l.split_once(' ').unwrap();
            let (script, line) = key.split_once(':').unwrap();
            (
                (script.to_string(), line.parse().unwrap()),
                reason.to_string(),
            )
        })
        .collect()
}

/// Runs one script and returns (records passed, failing lines with detail).
fn run_script(text: &str) -> (usize, Vec<(usize, String)>) {
    let f = fixture();
    let mut runner = Runner {
        db: f.db,
        session: ExecSession::allow_all(),
    };
    let records = parse(text, "lattice").unwrap();
    let report = run(&records, &mut runner);
    (
        report.passed,
        report
            .mismatches
            .into_iter()
            .map(|m| (m.line, m.detail))
            .collect(),
    )
}

#[test]
fn scripts_pass_except_the_listed_records() {
    let expected = expected();
    let mut problems = Vec::new();
    let mut summary = Vec::new();
    for (name, text) in SCRIPTS {
        let (passed, failed) = run_script(text);
        summary.push(format!("{name}: {passed} passed, {} failed", failed.len()));
        let failed_lines: Vec<usize> = failed.iter().map(|(l, _)| *l).collect();
        for (line, detail) in &failed {
            if !expected.contains_key(&(name.to_string(), *line)) {
                problems.push(format!("{name}:{line} fails and is not listed: {detail}"));
            }
        }
        for ((script, line), reason) in &expected {
            if script == name && !failed_lines.contains(line) {
                problems.push(format!("{name}:{line} is listed ({reason}) but passes"));
            }
        }
    }
    assert!(problems.is_empty(), "{summary:#?}\n{problems:#?}");
}

#[test]
fn every_listed_record_exists_in_its_script() {
    let known: BTreeMap<&str, Vec<usize>> = SCRIPTS
        .iter()
        .map(|(n, t)| {
            let lines = parse(t, "lattice")
                .unwrap()
                .iter()
                .filter_map(|r| match r {
                    Record::StatementOk { line, .. }
                    | Record::StatementError { line, .. }
                    | Record::Query { line, .. } => Some(*line),
                    Record::Halt { .. } => None,
                })
                .collect();
            (*n, lines)
        })
        .collect();
    for (script, line) in expected().keys() {
        assert!(
            known.get(script.as_str()).is_some_and(|l| l.contains(line)),
            "{script}:{line} is not a record"
        );
    }
}

/// Prints the per-script tally when run with `--nocapture`, so a pull request can quote it.
#[test]
fn tally() {
    for (name, text) in SCRIPTS {
        let (passed, failed) = run_script(text);
        let lines: Vec<usize> = failed.iter().map(|(l, _)| *l).collect();
        println!("{name}: {passed} passed, {} failed {lines:?}", failed.len());
        for (l, d) in &failed {
            println!("    {name}:{l} {d}");
        }
    }
}

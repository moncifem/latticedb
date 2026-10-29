//! A sqllogictest reader and runner.
//!
//! The reader turns script text into records; the runner feeds each record to an [`Executor`] and
//! reports every mismatch with its line. The subset is the part of the format a SQL engine needs
//! first: `statement ok`, `statement error`, `query` with `nosort`, `rowsort` or `valuesort`,
//! `skipif`/`onlyif`, `halt` and comments. `hash-threshold` and "values hashing to" results are
//! refused with a clear error rather than half-supported.

/// One executable record of a script.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Record {
    /// A statement that must succeed.
    StatementOk {
        /// 1-based line of the record.
        line: usize,
        /// The SQL text.
        sql: String,
    },
    /// A statement that must fail, optionally with a given error code.
    StatementError {
        /// 1-based line of the record.
        line: usize,
        /// The SQL text.
        sql: String,
        /// The stable error code the failure must carry, when the script names one.
        code: Option<String>,
    },
    /// A query with its column types, sort mode and expected values.
    Query {
        /// 1-based line of the record.
        line: usize,
        /// The SQL text.
        sql: String,
        /// One letter per result column (`I` integer, `T` text, `R` real).
        types: String,
        /// How the result is ordered before comparison.
        sort: Sort,
        /// Expected values, one string per value, row by row.
        expected: Vec<String>,
    },
    /// Stop reading the script.
    Halt {
        /// 1-based line of the record.
        line: usize,
    },
}

/// How a query's result is ordered before comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sort {
    /// Compare in the order returned.
    NoSort,
    /// Sort whole rows.
    RowSort,
    /// Sort every value individually.
    ValueSort,
}

/// A script the reader could not understand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    /// 1-based line of the offending record.
    pub line: usize,
    /// What was wrong.
    pub message: String,
}

/// Reads a script. Records guarded by `skipif`/`onlyif` for `engine` are left out.
pub fn parse(script: &str, engine: &str) -> Result<Vec<Record>, ParseError> {
    let lines: Vec<&str> = script.lines().collect();
    let mut out = Vec::new();
    let mut i = 0;
    let mut skip_next = false;
    while let Some(raw) = lines.get(i) {
        let line = i + 1;
        let head = raw.trim();
        i += 1;
        if head.is_empty() || head.starts_with('#') {
            continue;
        }
        let mut words = head.split_whitespace();
        let first = words.next().unwrap_or("");
        let err = |message: &str| ParseError {
            line,
            message: message.to_string(),
        };
        match first {
            "skipif" | "onlyif" => {
                let target = words.next().ok_or_else(|| err("missing engine name"))?;
                if (first == "skipif") == (target == engine) {
                    skip_next = true;
                }
            }
            "halt" => {
                if !std::mem::take(&mut skip_next) {
                    out.push(Record::Halt { line });
                }
                break;
            }
            "hash-threshold" => return Err(err("hash-threshold is not supported")),
            "statement" => {
                let kind = words
                    .next()
                    .ok_or_else(|| err("statement needs ok or error"))?;
                let code = words.next().map(str::to_string);
                let (sql, next) = block(&lines, i, false);
                i = next;
                if sql.is_empty() {
                    return Err(err("statement has no SQL"));
                }
                let record = match kind {
                    "ok" => Record::StatementOk { line, sql },
                    "error" => Record::StatementError { line, sql, code },
                    _ => return Err(err("statement must be ok or error")),
                };
                if !std::mem::take(&mut skip_next) {
                    out.push(record);
                }
            }
            "query" => {
                let types = words
                    .next()
                    .ok_or_else(|| err("query needs column types"))?
                    .to_string();
                let sort = match words.next() {
                    None | Some("nosort") => Sort::NoSort,
                    Some("rowsort") => Sort::RowSort,
                    Some("valuesort") => Sort::ValueSort,
                    Some(_) => return Err(err("unknown sort mode")),
                };
                let (sql, next) = block(&lines, i, true);
                i = next;
                if sql.is_empty() {
                    return Err(err("query has no SQL"));
                }
                if lines.get(i).map(|l| l.trim()) != Some("----") {
                    return Err(err("query is missing its ---- separator"));
                }
                let (body, next) = block(&lines, i + 1, false);
                i = next;
                if body.contains("values hashing to") {
                    return Err(err("hashed results are not supported"));
                }
                let expected: Vec<String> =
                    body.lines().map(|l| l.trim_end().to_string()).collect();
                let record = Record::Query {
                    line,
                    sql,
                    types,
                    sort,
                    expected,
                };
                if !std::mem::take(&mut skip_next) {
                    out.push(record);
                }
            }
            _ => return Err(err("unknown record")),
        }
    }
    Ok(out)
}

/// Reads lines from `from` up to a blank line (or, when `stop_at_separator`, a `----` line).
fn block(lines: &[&str], from: usize, stop_at_separator: bool) -> (String, usize) {
    let mut i = from;
    let mut text: Vec<&str> = Vec::new();
    while let Some(l) = lines.get(i) {
        if l.trim().is_empty() || (stop_at_separator && l.trim() == "----") {
            break;
        }
        text.push(l);
        i += 1;
    }
    (text.join("\n"), i)
}

/// What an engine under test answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// A statement ran.
    Done,
    /// A query returned rows of rendered values.
    Rows(Vec<Vec<String>>),
}

/// A stable error: the code is what tests match on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecError {
    /// Stable error code, for example a SQLSTATE.
    pub code: String,
    /// Human-readable message.
    pub message: String,
}

/// The engine under test.
pub trait Executor {
    /// Runs one SQL text.
    fn execute(&mut self, sql: &str) -> Result<Outcome, ExecError>;
}

/// One record that did not behave as the script says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mismatch {
    /// Line of the record in the script.
    pub line: usize,
    /// What differed.
    pub detail: String,
}

/// The result of running a script.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Report {
    /// Records that behaved as written.
    pub passed: usize,
    /// Records that did not.
    pub mismatches: Vec<Mismatch>,
}

/// Runs `records` against `exec`.
pub fn run(records: &[Record], exec: &mut dyn Executor) -> Report {
    let mut report = Report::default();
    for record in records {
        let result = match record {
            Record::Halt { .. } => break,
            Record::StatementOk { line, sql } => match exec.execute(sql) {
                Ok(_) => Ok(()),
                Err(e) => Err((
                    *line,
                    format!("expected success, got {}: {}", e.code, e.message),
                )),
            },
            Record::StatementError { line, sql, code } => match exec.execute(sql) {
                Ok(_) => Err((
                    *line,
                    "expected an error, the statement succeeded".to_string(),
                )),
                Err(e) => match code {
                    Some(want) if *want != e.code => {
                        Err((*line, format!("expected error {want}, got {}", e.code)))
                    }
                    _ => Ok(()),
                },
            },
            Record::Query {
                line,
                sql,
                types,
                sort,
                expected,
            } => match exec.execute(sql) {
                Err(e) => Err((
                    *line,
                    format!("query failed with {}: {}", e.code, e.message),
                )),
                Ok(Outcome::Done) => Err((*line, "query returned no result set".to_string())),
                Ok(Outcome::Rows(rows)) => compare(*line, types, *sort, rows, expected),
            },
        };
        match result {
            Ok(()) => report.passed += 1,
            Err((line, detail)) => report.mismatches.push(Mismatch { line, detail }),
        }
    }
    report
}

/// A generated script over a fixed table: the setup statements followed by `queries` seeded
/// `SELECT`s using only constructs of the declared subset (projection, comparison filters,
/// `ORDER BY` with a unique tie-break, `LIMIT`, `count`). The same seed always yields the same
/// text. Expected values are left empty; compare two engines with [`differential`].
pub fn generate(seed: u64, queries: usize) -> String {
    use lattice_sim::SeededRng;
    let mut rng = SeededRng::new(seed);
    let mut out = String::from(
        "statement ok\nCREATE TABLE g (id INT PRIMARY KEY, grp INT, score INT, name TEXT)\n\n",
    );
    let rows = 5 + rng.below(20);
    for id in 1..=rows {
        let (grp, score) = (rng.below(4), rng.below(100));
        out.push_str(&format!(
            "statement ok\nINSERT INTO g VALUES ({id}, {grp}, {score}, 'n{id}')\n\n"
        ));
    }
    const OPS: [&str; 5] = ["=", "<", "<=", ">", ">="];
    const COLS: [&str; 3] = ["id", "grp", "score"];
    for _ in 0..queries {
        let col = COLS.get(rng.below(3) as usize).copied().unwrap_or("id");
        let op = OPS.get(rng.below(5) as usize).copied().unwrap_or("=");
        let bound = rng.below(100);
        let filter = format!("WHERE {col} {op} {bound}");
        let sql = match rng.below(4) {
            0 => format!("SELECT id, score FROM g {filter}"),
            1 => format!("SELECT count(*) FROM g {filter}"),
            2 => format!(
                "SELECT id FROM g {filter} ORDER BY score DESC, id LIMIT {}",
                1 + rng.below(8)
            ),
            _ => format!("SELECT name FROM g {filter} ORDER BY id"),
        };
        let sort = if sql.contains("ORDER BY") {
            "nosort"
        } else {
            "rowsort"
        };
        let types = if sql.starts_with("SELECT id, score") {
            "II"
        } else if sql.contains("name") {
            "T"
        } else {
            "I"
        };
        out.push_str(&format!("query {types} {sort}\n{sql}\n----\n\n"));
    }
    out
}

/// Runs every record's SQL against a reference engine and the engine under test and reports each
/// record where they disagree: on success versus failure, on the error code, or on the rows (after
/// the record's sort mode). The script's own expected values are not consulted, so generated
/// scripts can be compared without writing results by hand.
pub fn differential(
    records: &[Record],
    reference: &mut dyn Executor,
    under_test: &mut dyn Executor,
) -> Report {
    let mut report = Report::default();
    for record in records {
        let (line, sql, sort) = match record {
            Record::Halt { .. } => break,
            Record::StatementOk { line, sql } | Record::StatementError { line, sql, .. } => {
                (*line, sql, Sort::NoSort)
            }
            Record::Query {
                line, sql, sort, ..
            } => (*line, sql, *sort),
        };
        let normalise = |r: Result<Outcome, ExecError>| -> Result<Outcome, String> {
            match r.map_err(|e| e.code)? {
                Outcome::Rows(mut rows) => {
                    match sort {
                        Sort::NoSort => {}
                        Sort::RowSort => rows.sort(),
                        Sort::ValueSort => {
                            let mut all: Vec<String> = rows.into_iter().flatten().collect();
                            all.sort();
                            rows = all.into_iter().map(|v| vec![v]).collect();
                        }
                    }
                    Ok(Outcome::Rows(rows))
                }
                Outcome::Done => Ok(Outcome::Done),
            }
        };
        let a = normalise(reference.execute(sql));
        let b = normalise(under_test.execute(sql));
        if a == b {
            report.passed += 1;
        } else {
            report.mismatches.push(Mismatch {
                line,
                detail: format!("reference {a:?}, engine {b:?}"),
            });
        }
    }
    report
}

fn compare(
    line: usize,
    types: &str,
    sort: Sort,
    mut rows: Vec<Vec<String>>,
    expected: &[String],
) -> Result<(), (usize, String)> {
    if let Some(bad) = rows.iter().find(|r| r.len() != types.chars().count()) {
        return Err((
            line,
            format!(
                "expected {} columns, a row has {}",
                types.chars().count(),
                bad.len()
            ),
        ));
    }
    if sort == Sort::RowSort {
        rows.sort();
    }
    let mut got: Vec<String> = rows.into_iter().flatten().collect();
    let mut want = expected.to_vec();
    if sort == Sort::ValueSort {
        got.sort();
        want.sort();
    }
    if got == want {
        Ok(())
    } else {
        Err((line, format!("expected {want:?}, got {got:?}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    struct Fake(BTreeMap<String, Result<Outcome, ExecError>>);
    impl Executor for Fake {
        fn execute(&mut self, sql: &str) -> Result<Outcome, ExecError> {
            self.0.get(sql).cloned().unwrap_or(Ok(Outcome::Done))
        }
    }
    fn fail(code: &str) -> Result<Outcome, ExecError> {
        Err(ExecError {
            code: code.into(),
            message: "m".into(),
        })
    }
    fn rows(r: &[&[&str]]) -> Result<Outcome, ExecError> {
        Ok(Outcome::Rows(
            r.iter()
                .map(|x| x.iter().map(|s| s.to_string()).collect())
                .collect(),
        ))
    }

    const SCRIPT: &str = "# demo\nstatement ok\nCREATE TABLE t (a INT)\n\nstatement error 42P01\nSELECT * FROM nope\n\nquery IT rowsort\nSELECT a, b FROM t\n----\n1\none\n2\ntwo\n\n";

    #[test]
    fn reads_every_kind_of_record_with_its_line() {
        let r = parse(SCRIPT, "lattice").unwrap();
        assert_eq!(r.len(), 3);
        assert!(matches!(&r[0], Record::StatementOk { line: 2, .. }));
        assert!(
            matches!(&r[1], Record::StatementError { line: 5, code: Some(c), .. } if c == "42P01")
        );
        assert!(
            matches!(&r[2], Record::Query { line: 8, sort: Sort::RowSort, expected, .. } if expected.len() == 4)
        );
    }

    #[test]
    fn engine_guards_skip_or_keep_the_next_record() {
        let s = "skipif lattice\nstatement ok\nA\n\nonlyif postgres\nstatement ok\nB\n\nonlyif lattice\nstatement ok\nC\n";
        let r = parse(s, "lattice").unwrap();
        assert_eq!(r.len(), 1);
        assert!(matches!(&r[0], Record::StatementOk { sql, .. } if sql == "C"));
    }

    #[test]
    fn halt_stops_the_script() {
        let r = parse("statement ok\nA\n\nhalt\n\nstatement ok\nB\n", "x").unwrap();
        assert_eq!(r.len(), 2);
        assert!(matches!(r[1], Record::Halt { line: 4 }));
    }

    #[test]
    fn unsupported_or_malformed_input_is_refused_with_its_line() {
        for (s, line) in [
            ("hash-threshold 8\n", 1),
            ("bogus\n", 1),
            ("statement maybe\nX\n", 1),
            ("statement ok\n", 1),
            ("query I\nSELECT 1\n", 1),
            ("query I\nSELECT 1\n----\n4 values hashing to abc\n", 1),
            ("query I sideways\nSELECT 1\n----\n1\n", 1),
            ("\nskipif\n", 2),
        ] {
            assert_eq!(parse(s, "x").unwrap_err().line, line, "{s:?}");
        }
    }

    #[test]
    fn a_matching_script_passes_and_a_wrong_one_names_its_line() {
        let r = parse(SCRIPT, "lattice").unwrap();
        let mut ok = Fake(BTreeMap::from([
            ("SELECT * FROM nope".to_string(), fail("42P01")),
            (
                "SELECT a, b FROM t".to_string(),
                rows(&[&["2", "two"], &["1", "one"]]),
            ),
        ]));
        assert_eq!(
            run(&r, &mut ok),
            Report {
                passed: 3,
                mismatches: vec![]
            }
        );

        let mut bad = Fake(BTreeMap::from([
            ("SELECT * FROM nope".to_string(), fail("XX000")),
            ("SELECT a, b FROM t".to_string(), rows(&[&["1", "one"]])),
        ]));
        let rep = run(&r, &mut bad);
        assert_eq!(rep.passed, 1);
        assert_eq!(
            rep.mismatches.iter().map(|m| m.line).collect::<Vec<_>>(),
            vec![5, 8]
        );
    }

    #[test]
    fn sort_modes_and_shape_checks() {
        let q = |sort, got: Result<Outcome, ExecError>, want: &[&str]| {
            let rec = Record::Query {
                line: 1,
                sql: "q".into(),
                types: "II".into(),
                sort,
                expected: want.iter().map(|s| s.to_string()).collect(),
            };
            run(&[rec], &mut Fake(BTreeMap::from([("q".to_string(), got)])))
                .mismatches
                .len()
        };
        let data = || rows(&[&["2", "1"], &["1", "9"]]);
        assert_eq!(q(Sort::NoSort, data(), &["2", "1", "1", "9"]), 0);
        assert_eq!(q(Sort::NoSort, data(), &["1", "9", "2", "1"]), 1);
        assert_eq!(q(Sort::RowSort, data(), &["1", "9", "2", "1"]), 0);
        assert_eq!(q(Sort::ValueSort, data(), &["1", "1", "2", "9"]), 0);
        assert_eq!(q(Sort::NoSort, rows(&[&["1"]]), &["1"]), 1);
        assert_eq!(q(Sort::NoSort, Ok(Outcome::Done), &[]), 1);
        assert_eq!(q(Sort::NoSort, fail("X"), &[]), 1);
    }

    #[test]
    fn the_differential_run_flags_each_kind_of_disagreement() {
        let recs = parse(
            "query I rowsort\nq1\n----\n\nquery I rowsort\nq2\n----\n\nstatement ok\ns1\n\nstatement ok\ns2\n\nquery I\nq3\n----\n",
            "x",
        )
        .unwrap();
        let mut reference = Fake(BTreeMap::from([
            ("q1".to_string(), rows(&[&["1"], &["2"]])),
            ("q2".to_string(), rows(&[&["1"]])),
            ("s1".to_string(), fail("A")),
            ("q3".to_string(), rows(&[&["1"]])),
        ]));
        let mut engine = Fake(BTreeMap::from([
            ("q1".to_string(), rows(&[&["2"], &["1"]])),
            ("q2".to_string(), rows(&[&["9"]])),
            ("s1".to_string(), fail("B")),
            ("q3".to_string(), rows(&[&["1"]])),
        ]));
        let rep = differential(&recs, &mut reference, &mut engine);
        assert_eq!(rep.passed, 3);
        assert_eq!(
            rep.mismatches.iter().map(|m| m.line).collect::<Vec<_>>(),
            vec![5, 9]
        );
    }

    #[test]
    fn generated_scripts_are_deterministic_and_readable() {
        assert_eq!(generate(7, 30), generate(7, 30));
        assert_ne!(generate(7, 30), generate(8, 30));
        let recs = parse(&generate(7, 30), "x").unwrap();
        let queries = recs
            .iter()
            .filter(|r| matches!(r, Record::Query { .. }))
            .count();
        assert_eq!(queries, 30);
        assert!(recs.len() >= 30 + 1 + 5);
    }

    #[test]
    fn an_error_statement_that_succeeds_and_an_ok_statement_that_fails_are_mismatches() {
        let recs = parse("statement ok\nA\n\nstatement error\nB\n", "x").unwrap();
        let mut f = Fake(BTreeMap::from([("A".to_string(), fail("E"))]));
        let rep = run(&recs, &mut f);
        assert_eq!(rep.mismatches.len(), 2);
        let mut any = Fake(BTreeMap::from([("B".to_string(), fail("E"))]));
        assert_eq!(run(&recs[1..], &mut any).passed, 1);
    }
}

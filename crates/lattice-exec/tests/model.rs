//! Model test: random single-table workloads, run through the executor and through a plain in-memory
//! oracle, must agree on every result, every error code and the table contents after every step.
//!
//! The oracle is written from the SQL rules, not from the executor: a `BTreeMap` of rows, a copy of it
//! for an open transaction block, and the constraint rules spelled out in Rust.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use std::collections::BTreeMap;

use common::{Db, fixture};
use lattice_exec::ExecSession;
use proptest::prelude::*;

/// `a`, `b`, `u` of one row; the key is the map key.
type Row = (Option<i32>, Option<String>, Option<i32>);
type Table = BTreeMap<i64, Row>;

#[derive(Clone, Debug)]
enum Op {
    Insert {
        id: i64,
        a: Option<i32>,
        b: Option<String>,
        u: Option<i32>,
    },
    UpdateAdd {
        lo: i64,
        hi: i64,
        k: i32,
    },
    UpdateB {
        min_a: i32,
    },
    UpdateU {
        id: i64,
        u: Option<i32>,
    },
    DeleteRange {
        lo: i64,
        hi: i64,
    },
    DeleteNullA,
    Select {
        min_a: i32,
        limit: usize,
        offset: usize,
    },
    SelectLike {
        prefix: String,
    },
    Begin,
    Commit,
    Rollback,
}

fn lit<T: ToString>(v: &Option<T>, quote: bool) -> String {
    match v {
        None => "NULL".into(),
        Some(x) if quote => format!("'{}'", x.to_string()),
        Some(x) => x.to_string(),
    }
}

impl Op {
    fn sql(&self) -> String {
        match self {
            Op::Insert { id, a, b, u } => format!(
                "INSERT INTO m (id, a, b, u) VALUES ({id}, {}, {}, {})",
                lit(a, false),
                lit(b, true),
                lit(u, false)
            ),
            Op::UpdateAdd { lo, hi, k } => {
                format!("UPDATE m SET a = a + {k} WHERE id BETWEEN {lo} AND {hi}")
            }
            Op::UpdateB { min_a } => format!("UPDATE m SET b = upper(b) || '!' WHERE a >= {min_a}"),
            Op::UpdateU { id, u } => format!("UPDATE m SET u = {} WHERE id = {id}", lit(u, false)),
            Op::DeleteRange { lo, hi } => format!("DELETE FROM m WHERE id >= {lo} AND id <= {hi}"),
            Op::DeleteNullA => "DELETE FROM m WHERE a IS NULL".into(),
            Op::Select {
                min_a,
                limit,
                offset,
            } => format!(
                "SELECT id, a FROM m WHERE a >= {min_a} ORDER BY a DESC, id LIMIT {limit} OFFSET {offset}"
            ),
            Op::SelectLike { prefix } => {
                format!(
                    "SELECT id, b FROM m WHERE b LIKE '{prefix}%' OR a IS NULL ORDER BY id DESC"
                )
            }
            Op::Begin => "BEGIN".into(),
            Op::Commit => "COMMIT".into(),
            Op::Rollback => "ROLLBACK".into(),
        }
    }
}

fn small_a() -> impl Strategy<Value = i32> {
    prop_oneof![
        8 => -5i32..6,
        1 => Just(i32::MAX - 1),
        1 => Just(i32::MIN + 1),
    ]
}

fn word() -> impl Strategy<Value = String> {
    "[a-c]{0,3}"
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        6 => (0i64..14, prop::option::of(small_a()), prop::option::of(word()), prop::option::of(0i32..7))
            .prop_map(|(id, a, b, u)| Op::Insert { id, a, b, u }),
        2 => (0i64..14, 0i64..14, prop_oneof![-3i32..4, Just(i32::MAX)])
            .prop_map(|(lo, hi, k)| Op::UpdateAdd { lo, hi, k }),
        1 => (-5i32..6).prop_map(|min_a| Op::UpdateB { min_a }),
        2 => (0i64..14, prop::option::of(0i32..7)).prop_map(|(id, u)| Op::UpdateU { id, u }),
        2 => (0i64..14, 0i64..14).prop_map(|(lo, hi)| Op::DeleteRange { lo, hi }),
        1 => Just(Op::DeleteNullA),
        3 => (-5i32..6, 0usize..6, 0usize..4).prop_map(|(min_a, limit, offset)| Op::Select { min_a, limit, offset }),
        1 => "[a-c]{0,2}".prop_map(|prefix| Op::SelectLike { prefix }),
        2 => Just(Op::Begin),
        2 => Just(Op::Commit),
        1 => Just(Op::Rollback),
    ]
}

/// What the oracle says a statement does: new table and rows, or an error code.
enum Expect {
    Rows(Vec<Vec<String>>),
    Done,
    Fail(&'static str),
}

fn cell<T: ToString>(v: &Option<T>) -> String {
    v.as_ref().map_or("NULL".into(), ToString::to_string)
}

/// Applies a data statement to a copy of the table; the copy replaces the table only on success.
fn apply(t: &Table, op: &Op) -> (Option<Table>, Expect) {
    let mut m = t.clone();
    match op {
        Op::Insert { id, a, b, u } => {
            if m.contains_key(id) || u.is_some_and(|u| m.values().any(|r| r.2 == Some(u))) {
                return (None, Expect::Fail("23505"));
            }
            m.insert(*id, (*a, b.clone(), *u));
            (Some(m), Expect::Done)
        }
        Op::UpdateAdd { lo, hi, k } => {
            for (_, r) in m.iter_mut().filter(|(k, _)| lo <= *k && *k <= hi) {
                if let Some(a) = r.0 {
                    match a.checked_add(*k) {
                        Some(n) => r.0 = Some(n),
                        None => return (None, Expect::Fail("22003")),
                    }
                }
            }
            (Some(m), Expect::Done)
        }
        Op::UpdateB { min_a } => {
            for r in m.values_mut() {
                if r.0.is_some_and(|a| a >= *min_a) {
                    r.1 = r.1.as_ref().map(|b| format!("{}!", b.to_uppercase()));
                }
            }
            (Some(m), Expect::Done)
        }
        Op::UpdateU { id, u } => {
            if m.contains_key(id) {
                if u.is_some_and(|u| m.iter().any(|(k, r)| k != id && r.2 == Some(u))) {
                    return (None, Expect::Fail("23505"));
                }
                if let Some(r) = m.get_mut(id) {
                    r.2 = *u;
                }
            }
            (Some(m), Expect::Done)
        }
        Op::DeleteRange { lo, hi } => {
            m.retain(|k, _| !(lo..=hi).contains(&k));
            (Some(m), Expect::Done)
        }
        Op::DeleteNullA => {
            m.retain(|_, r| r.0.is_some());
            (Some(m), Expect::Done)
        }
        Op::Select {
            min_a,
            limit,
            offset,
        } => {
            let mut hits: Vec<(i32, i64)> = m
                .iter()
                .filter_map(|(k, r)| r.0.filter(|a| a >= min_a).map(|a| (a, *k)))
                .collect();
            hits.sort_by(|x, y| y.0.cmp(&x.0).then(x.1.cmp(&y.1)));
            let out = hits
                .into_iter()
                .skip(*offset)
                .take(*limit)
                .map(|(a, k)| vec![k.to_string(), a.to_string()])
                .collect();
            (None, Expect::Rows(out))
        }
        Op::SelectLike { prefix } => {
            let out = m
                .iter()
                .rev()
                .filter(|(_, r)| {
                    r.0.is_none() || r.1.as_ref().is_some_and(|b| b.starts_with(prefix.as_str()))
                })
                .map(|(k, r)| vec![k.to_string(), cell(&r.1)])
                .collect();
            (None, Expect::Rows(out))
        }
        Op::Begin | Op::Commit | Op::Rollback => (None, Expect::Done),
    }
}

fn table_rows(t: &Table) -> Vec<Vec<String>> {
    t.iter()
        .map(|(k, r)| vec![k.to_string(), cell(&r.0), cell(&r.1), cell(&r.2)])
        .collect()
}

const DUMP: &str = "SELECT id, a, b, u FROM m ORDER BY id";

/// The oracle's view of the session.
struct Model {
    committed: Table,
    block: Option<Table>,
    failed: bool,
}

/// Runs the workload and returns a description of the first disagreement, if any.
fn check(ops: &[Op], mutate_oracle: bool) -> Result<(), String> {
    let f = fixture();
    let (mut db, mut s, mut viewer): (Db, _, _) =
        (f.db, ExecSession::allow_all(), ExecSession::allow_all());
    s.execute(
        &mut db,
        "CREATE TABLE m (id BIGINT PRIMARY KEY, a INT, b TEXT, c INT NOT NULL DEFAULT 0, u INT UNIQUE)",
    )
    .map_err(|e| e.to_string())?;
    let mut model = Model {
        committed: Table::new(),
        block: None,
        failed: false,
    };
    for (step, op) in ops.iter().enumerate() {
        let sql = op.sql();
        let got = s.execute(&mut db, &sql);
        let fail = |why: String| Err(format!("step {step} `{sql}`: {why}"));
        // The oracle.
        let expect: Expect = match op {
            _ if model.failed => match op {
                Op::Rollback | Op::Commit => {
                    model.block = None;
                    model.failed = false;
                    Expect::Done
                }
                _ => Expect::Fail("25P02"),
            },
            Op::Begin => {
                if model.block.is_none() {
                    model.block = Some(model.committed.clone());
                }
                Expect::Done
            }
            Op::Commit => {
                if let Some(b) = model.block.take() {
                    model.committed = b;
                }
                Expect::Done
            }
            Op::Rollback => {
                model.block = None;
                Expect::Done
            }
            _ => {
                let base = model.block.as_ref().unwrap_or(&model.committed);
                let (next, expect) = apply(base, op);
                match (&expect, next) {
                    (Expect::Fail(_), _) => {
                        if model.block.is_some() {
                            model.failed = true;
                        }
                    }
                    (_, Some(mut t)) => {
                        if mutate_oracle {
                            // A planted oracle bug: the last row's `a` is off by one.
                            if let Some(r) = t.values_mut().next_back() {
                                r.0 = Some(r.0.unwrap_or(0).wrapping_add(1));
                            }
                        }
                        match model.block.as_mut() {
                            Some(b) => *b = t,
                            None => model.committed = t,
                        }
                    }
                    _ => {}
                }
                expect
            }
        };
        match (expect, got) {
            (Expect::Done, Ok(_)) => {}
            (Expect::Rows(want), Ok(r)) => {
                let have = common::text(&r);
                if have != want {
                    return fail(format!("rows {have:?}, oracle {want:?}"));
                }
            }
            (Expect::Fail(code), Err(e)) => {
                if e.sqlstate() != code {
                    return fail(format!("error {}, oracle {code}", e.sqlstate()));
                }
            }
            (Expect::Fail(code), Ok(r)) => {
                return fail(format!("succeeded with {}, oracle {code}", r.tag));
            }
            (_, Err(e)) => return fail(format!("failed with {e}")),
        }
        // Contents, as the writing session sees them (unless its block failed) and as everybody
        // else sees them (committed state only).
        if !model.failed {
            let have = common::text(&s.execute(&mut db, DUMP).map_err(|e| e.to_string())?);
            let want = table_rows(model.block.as_ref().unwrap_or(&model.committed));
            if have != want {
                return fail(format!("table {have:?}, oracle {want:?}"));
            }
        }
        let have = common::text(&viewer.execute(&mut db, DUMP).map_err(|e| e.to_string())?);
        if have != table_rows(&model.committed) {
            return fail(format!(
                "committed view {have:?}, oracle {:?}",
                table_rows(&model.committed)
            ));
        }
    }
    Ok(())
}

proptest! {
    #[test]
    fn random_workloads_agree_with_the_oracle(ops in prop::collection::vec(op(), 1..40)) {
        if let Err(why) = check(&ops, false) {
            prop_assert!(false, "{why}");
        }
    }
}

#[test]
fn a_fixed_workload_exercises_every_operation() {
    use Op::*;
    let ops = vec![
        Insert {
            id: 1,
            a: Some(5),
            b: Some("ab".into()),
            u: Some(1),
        },
        Insert {
            id: 2,
            a: None,
            b: None,
            u: None,
        },
        Insert {
            id: 3,
            a: Some(i32::MAX - 1),
            b: Some("c".into()),
            u: Some(2),
        },
        Insert {
            id: 1,
            a: None,
            b: None,
            u: None,
        },
        Insert {
            id: 4,
            a: Some(0),
            b: None,
            u: Some(2),
        },
        UpdateAdd { lo: 0, hi: 5, k: 1 },
        UpdateAdd { lo: 0, hi: 5, k: 1 },
        Begin,
        UpdateB { min_a: 0 },
        UpdateU { id: 1, u: Some(2) },
        Select {
            min_a: 0,
            limit: 5,
            offset: 0,
        },
        Rollback,
        Begin,
        DeleteNullA,
        Insert {
            id: 9,
            a: Some(1),
            b: Some("a".into()),
            u: None,
        },
        SelectLike { prefix: "a".into() },
        Commit,
        DeleteRange { lo: 0, hi: 3 },
        Select {
            min_a: -5,
            limit: 5,
            offset: 1,
        },
    ];
    check(&ops, false).unwrap();
}

#[test]
fn the_check_catches_a_wrong_oracle() {
    // Negative control: the same workload against an oracle with a planted bug must disagree, so a
    // passing run above means agreement and not an assertion that cannot fail.
    use Op::*;
    let ops = vec![
        Insert {
            id: 1,
            a: Some(5),
            b: None,
            u: None,
        },
        Insert {
            id: 2,
            a: Some(6),
            b: None,
            u: None,
        },
    ];
    let why = check(&ops, true).unwrap_err();
    assert!(why.contains("oracle"), "{why}");
}

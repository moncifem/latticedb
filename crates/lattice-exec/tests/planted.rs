//! Planted-bug checks: the executor's guarantees about conflicts and row policy rest on engine
//! behaviour, so each is checked by a scenario function that is run twice. Run clean it must hold;
//! run with the matching engine defect planted (`lattice_core::bugs`) it must report a violation.
//! A scenario that cannot fail would let a regression through unnoticed.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use std::rc::Rc;

use common::{fixture, ok};
use lattice_core::bugs::{self, Bug};
use lattice_engines::limits::{PolicyEvaluator, RowRef, RowWriteRef};
use lattice_exec::{ActorPolicy, ExecSession};

/// Switches a defect off again however the test ends.
struct Planted;

impl Planted {
    fn on(b: Bug) -> Self {
        bugs::enable(b);
        Planted
    }
}

impl Drop for Planted {
    fn drop(&mut self) {
        bugs::clear();
    }
}

/// Two blocks update one row; the second to commit must be refused with `40001`.
fn first_committer_wins() -> Result<(), String> {
    let mut f = fixture();
    let mut a = common::admin();
    let mut b = common::admin();
    ok(
        &mut f.db,
        &mut a,
        "CREATE TABLE t (id INT PRIMARY KEY, v INT)",
    );
    ok(&mut f.db, &mut a, "INSERT INTO t VALUES (1, 0)");
    ok(&mut f.db, &mut a, "BEGIN");
    ok(&mut f.db, &mut b, "BEGIN");
    ok(&mut f.db, &mut a, "UPDATE t SET v = 1");
    ok(&mut f.db, &mut b, "UPDATE t SET v = 2");
    ok(&mut f.db, &mut a, "COMMIT");
    match b.execute(&mut f.db, "COMMIT") {
        Err(e) if e.sqlstate() == "40001" => Ok(()),
        Err(e) => Err(format!("wrong error {e}")),
        Ok(_) => Err("a lost update was committed".into()),
    }
}

struct EvenIds;

impl PolicyEvaluator for EvenIds {
    fn allows_read(&self, row: RowRef<'_>) -> bool {
        row.table & 0x8000_0000 != 0 || row.key.last().is_some_and(|b| b % 2 == 0)
    }
    fn allows_write(&self, row: RowWriteRef<'_>) -> bool {
        row.table & 0x8000_0000 != 0 || row.key.last().is_some_and(|b| b % 2 == 0)
    }
}

fn actor_fixture() -> (common::Fixture, ExecSession, ExecSession) {
    let mut f = fixture();
    let mut admin = common::admin();
    ok(
        &mut f.db,
        &mut admin,
        "CREATE TABLE t (id INT PRIMARY KEY, v INT)",
    );
    ok(
        &mut f.db,
        &mut admin,
        "INSERT INTO t VALUES (1, 10), (2, 20), (3, 30)",
    );
    (
        f,
        admin,
        ExecSession::new(ActorPolicy::new(Rc::new(EvenIds))),
    )
}

/// The actor must not see rows its policy hides.
fn hidden_rows_stay_hidden() -> Result<(), String> {
    let (mut f, _admin, mut actor) = actor_fixture();
    let seen = actor
        .execute(&mut f.db, "SELECT id FROM t")
        .map_err(|e| e.to_string())?;
    if seen.rows.len() == 1 {
        Ok(())
    } else {
        Err(format!(
            "the actor saw {} rows instead of 1",
            seen.rows.len()
        ))
    }
}

/// The actor must not be able to write a row its policy forbids.
fn forbidden_writes_are_refused() -> Result<(), String> {
    let (mut f, mut admin, mut actor) = actor_fixture();
    let attempt = actor.execute(&mut f.db, "INSERT INTO t VALUES (5, 50)");
    let rows = common::text(&ok(&mut f.db, &mut admin, "SELECT id FROM t WHERE id = 5"));
    match (attempt, rows.is_empty()) {
        (Err(e), true) if e.sqlstate() == "42501" => Ok(()),
        (Err(e), _) => Err(format!("wrong outcome {e}")),
        (Ok(_), _) => Err("a forbidden row was written".into()),
    }
}

#[test]
fn clean_engine_passes_every_scenario() {
    assert_eq!(first_committer_wins(), Ok(()));
    assert_eq!(hidden_rows_stay_hidden(), Ok(()));
    assert_eq!(forbidden_writes_are_refused(), Ok(()));
}

#[test]
fn skipped_conflict_check_is_caught() {
    let _g = Planted::on(Bug::RowConflictCheckSkipped);
    assert!(first_committer_wins().is_err());
}

#[test]
fn skipped_read_policy_is_caught() {
    let _g = Planted::on(Bug::RowPolicyReadSkipped);
    assert!(hidden_rows_stay_hidden().is_err());
}

#[test]
fn skipped_write_policy_is_caught() {
    let _g = Planted::on(Bug::RowPolicyWriteSkipped);
    assert!(forbidden_writes_are_refused().is_err());
}

#[test]
fn a_partial_apply_is_caught_by_the_atomic_commit_check() {
    // A transaction of three writes must show all or none of them to readers; the planted defect
    // applies only the first.
    fn all_or_nothing() -> Result<(), String> {
        let mut f = fixture();
        let mut s = common::admin();
        ok(&mut f.db, &mut s, "CREATE TABLE t (id INT PRIMARY KEY)");
        ok(&mut f.db, &mut s, "INSERT INTO t VALUES (1), (2), (3)");
        let n = ok(&mut f.db, &mut s, "SELECT id FROM t").rows.len();
        if n == 3 {
            Ok(())
        } else {
            Err(format!("{n} of 3 rows are visible"))
        }
    }
    assert_eq!(all_or_nothing(), Ok(()));
    let _g = Planted::on(Bug::RowPartialApply);
    assert!(all_or_nothing().is_err());
}

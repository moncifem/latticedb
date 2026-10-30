//! Shared fixtures for the executor tests: a database on the simulated disk and small helpers.
#![allow(
    dead_code,
    unreachable_pub,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::rc::Rc;

use lattice_core::Config;
use lattice_engines::limits::AllowAll;
use lattice_exec::{ActorPolicy, Database, ExecError, ExecResult, ExecSession};
use lattice_sim::{FaultProfile, SeededRng, SimClock, SimIo};
use lattice_testkit::scenario::{SharedClock, SharedIo};

pub type Db = Database<SharedIo, SharedClock, SeededRng>;

pub fn config() -> Config {
    Config {
        dir: "wal".into(),
        segment_limit: 1 << 20,
        node: 1,
    }
}

/// A database on a clean simulated disk, with the disk and clock handles kept for restarts and for
/// moving time.
pub struct Fixture {
    pub db: Db,
    pub disk: SharedIo,
    pub clock: Rc<SimClock>,
}

pub fn fixture() -> Fixture {
    let disk = SharedIo::new(SimIo::new(7, FaultProfile::clean()));
    let clock = Rc::new(SimClock::new(1_000));
    let db = Database::open(
        disk.clone(),
        SharedClock(clock.clone()),
        SeededRng::new(11),
        &config(),
    )
    .unwrap();
    Fixture { db, disk, clock }
}

/// Runs a statement that must succeed.
pub fn ok(db: &mut Db, s: &mut ExecSession, sql: &str) -> ExecResult {
    s.execute(db, sql).unwrap_or_else(|e| panic!("{sql}: {e}"))
}

/// Runs a statement that must fail and returns its error.
pub fn err(db: &mut Db, s: &mut ExecSession, sql: &str) -> ExecError {
    match s.execute(db, sql) {
        Ok(r) => panic!("{sql}: expected an error, got {}", r.tag),
        Err(e) => e,
    }
}

/// The rows of a result in PostgreSQL text format, `NULL` for null.
pub fn text(r: &ExecResult) -> Vec<Vec<String>> {
    r.rows
        .iter()
        .map(|row| {
            row.iter()
                .map(|d| d.to_pg_text().unwrap_or_else(|| "NULL".into()))
                .collect()
        })
        .collect()
}

pub fn rows(db: &mut Db, s: &mut ExecSession, sql: &str) -> Vec<Vec<String>> {
    text(&ok(db, s, sql))
}

/// A session for a trusted administrator: the allow-all policy is chosen explicitly here.
pub fn admin() -> ExecSession {
    ExecSession::new(ActorPolicy::new(Rc::new(AllowAll)))
}

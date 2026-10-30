//! A deterministic, bounded executor for the single-table SQL subset.
//!
//! Layer L5. Must not: depend on a network front door, read real time or use threads. It sits on
//! the bound plans of `lattice-sql` and the row store of `lattice-engines`:
//! [`ExecSession::execute`] parses a statement, binds it against the database's catalog, checks the
//! plan's catalog version, and runs `Scan`, `Filter`, `Project`, `Sort`, `Limit`, `Insert`, `Update`
//! and `Delete` inside a snapshot-isolated row-store transaction. `CREATE TABLE`, `DROP TABLE`,
//! `BEGIN`, `COMMIT`, `ROLLBACK`, `SET` and `SHOW` are handled by the session.
//!
//! ```
//! use std::rc::Rc;
//! use lattice_core::Config;
//! use lattice_exec::{Database, ExecSession};
//! use lattice_sim::{FaultProfile, SeededRng, SimClock, SimIo};
//! use lattice_testkit::scenario::{SharedClock, SharedIo};
//!
//! let disk = SharedIo::new(SimIo::new(1, FaultProfile::clean()));
//! let clock = SharedClock(Rc::new(SimClock::new(1_000)));
//! let mut db = Database::open(disk, clock, SeededRng::new(1), &Config::default()).unwrap();
//! let mut session = ExecSession::allow_all();
//! session.execute(&mut db, "CREATE TABLE notes (id INT PRIMARY KEY, title TEXT)").unwrap();
//! let r = session.execute(&mut db, "INSERT INTO notes VALUES (1, 'a'), (2, 'b') RETURNING id").unwrap();
//! assert_eq!(r.tag, "INSERT 0 2");
//! let r = session.execute(&mut db, "SELECT title FROM notes WHERE id > 1").unwrap();
//! assert_eq!(r.text_rows(), vec![vec![Some("b".to_string())]]);
//! ```
//!
//! Every statement is bounded by [`ExecLimits`]: a deadline on the database clock, a memory budget
//! reserved before rows are kept, and a cancellation poll between rows. Nothing here panics on any
//! bound plan; failures are [`ExecError`]s carrying a SQLSTATE.
#![forbid(unsafe_code)]

mod db;
mod decimal;
mod error;
mod eval;
mod exec;
pub mod schema;
mod session;
mod value;

pub use db::Database;
pub use decimal::Decimal;
pub use error::{ErrorKind, ExecError};
pub use exec::ActorPolicy;
pub use schema::{ColumnDef, ExecCatalog, Schema, TableDef};
pub use session::{ExecLimits, ExecResult, ExecSession, ResultColumn, TxStatus, decode_params};
pub use value::Datum;

/// Layer marker used by the layer check. Do not remove.
pub const LAYER: u8 = 5;

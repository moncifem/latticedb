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
//! use lattice_exec::{Database, ExecSession};
//! # use lattice_core::Config;
//! # use lattice_sim::{FaultProfile, SeededRng, SimClock, SimIo};
//! # use std::cell::RefCell;
//! # use std::rc::Rc;
//! # #[derive(Clone)] struct Io(Rc<RefCell<SimIo>>);
//! # fn main() {}
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

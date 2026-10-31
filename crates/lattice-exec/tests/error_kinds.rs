//! The coarse kind and SQLSTATE of executor errors, and the PostgreSQL type object ids and sizes of
//! result columns. The ids and sizes are the values in PostgreSQL's `pg_type`; the kinds follow the
//! documentation on `ErrorKind`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use common::{err, fixture};
use lattice_core::txn::TxnError;
use lattice_engines::store::RowError;
use lattice_exec::{ErrorKind, ExecError, ResultColumn};
use lattice_sql::plan::SqlType;

fn column(ty: SqlType) -> ResultColumn {
    ResultColumn {
        name: "c".into(),
        ty,
    }
}

#[test]
fn every_type_reports_its_pg_type_oid_and_size() {
    // (type, pg_type.oid, pg_type.typlen)
    let expected = [
        (SqlType::Boolean, 16, 1),
        (SqlType::SmallInt, 21, 2),
        (SqlType::Integer, 23, 4),
        (SqlType::BigInt, 20, 8),
        (SqlType::Numeric, 1700, -1),
        (SqlType::DoublePrecision, 701, 8),
        (SqlType::Bytea, 17, -1),
        (SqlType::Timestamptz, 1184, 8),
        (SqlType::Date, 1082, 4),
        (SqlType::Uuid, 2950, 16),
        (SqlType::Jsonb, 3802, -1),
        (SqlType::Text, 25, -1),
        // PostgreSQL 10+ resolves unknown literals to text (25); its own unknown type is 705.
        (SqlType::Unknown, 25, -1),
    ];
    for (ty, oid, size) in expected {
        let c = column(ty);
        assert_eq!((c.type_oid(), c.type_size()), (oid, size), "{ty:?}");
    }
}

#[test]
fn statements_fail_with_the_documented_kind() {
    let mut f = fixture();
    let mut s = common::admin();
    let cases = [
        ("SELECT 1 FROM FROM", "42601", ErrorKind::Syntax),
        (
            "SELECT * FROM a RIGHT JOIN b ON 1 = 1",
            "0A000",
            ErrorKind::Unsupported,
        ),
        ("SELECT nope FROM nope", "42P01", ErrorKind::Bind),
        ("SELECT 2147483647 + 1", "22003", ErrorKind::Evaluation),
        ("SELECT 1 / 0", "22012", ErrorKind::Evaluation),
    ];
    for (sql, state, kind) in cases {
        let e = err(&mut f.db, &mut s, sql);
        assert_eq!((e.sqlstate(), e.kind()), (state, kind), "{sql}");
    }
}

#[test]
fn a_refused_ddl_statement_carries_the_lattice_code() {
    let mut f = fixture();
    let mut s = common::admin();
    common::ok(&mut f.db, &mut s, "CREATE TABLE k (id INT PRIMARY KEY)");
    for sql in [
        "ALTER TABLE k ADD COLUMN z text",
        "CREATE INDEX k_id ON k (id)",
        "DROP INDEX k_id",
    ] {
        let e = err(&mut f.db, &mut s, sql);
        assert_eq!(
            (e.sqlstate(), e.kind()),
            ("0A000", ErrorKind::Unsupported),
            "{sql}"
        );
        assert!(
            e.message().starts_with("LSQL-0002 unsupported feature: "),
            "{sql}: {}",
            e.message()
        );
    }
}

#[test]
fn a_constant_null_for_a_not_null_column_is_a_constraint_error() {
    let mut f = fixture();
    let mut s = common::admin();
    common::ok(
        &mut f.db,
        &mut s,
        "CREATE TABLE k (id INT PRIMARY KEY, v INT NOT NULL)",
    );
    let e = err(&mut f.db, &mut s, "INSERT INTO k VALUES (1, NULL)");
    assert_eq!((e.sqlstate(), e.kind()), ("23502", ErrorKind::Constraint));
}

#[test]
fn storage_errors_map_to_their_sqlstate_and_kind_without_engine_detail() {
    let conflict = ExecError::from(RowError::Conflict { newer_lsn: 9 });
    assert_eq!(
        (conflict.sqlstate(), conflict.kind()),
        ("40001", ErrorKind::Conflict)
    );

    let old = ExecError::from(RowError::SnapshotTooOld { floor: 5 });
    assert_eq!(
        (old.sqlstate(), old.kind()),
        ("72000", ErrorKind::SnapshotTooOld)
    );

    let doubt = ExecError::from(RowError::InDoubt(TxnError::Invalid("secret".into())));
    assert_eq!(
        (doubt.sqlstate(), doubt.kind()),
        ("40003", ErrorKind::Storage)
    );
    assert!(!doubt.message().contains("secret"));

    let corrupt = ExecError::from(RowError::Corrupt("/var/data/segment-7 offset 12".into()));
    assert_eq!(
        (corrupt.sqlstate(), corrupt.kind()),
        ("XX001", ErrorKind::Storage)
    );
    assert!(!corrupt.message().contains("segment"));

    let stale = ExecError::from(RowError::Stale);
    assert_eq!(
        (stale.sqlstate(), stale.kind()),
        ("XX000", ErrorKind::Storage)
    );

    let denied = ExecError::from(RowError::PermissionDenied);
    assert_eq!(
        (denied.sqlstate(), denied.kind()),
        ("42501", ErrorKind::Permission)
    );
}

#[test]
fn an_error_prints_its_message_and_sqlstate() {
    let e = ExecError::from(RowError::PermissionDenied);
    assert_eq!(e.to_string(), "row write denied (42501)");
}

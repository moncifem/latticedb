//! The text of the engine error types and the by-reference policy pass-through, pinned because
//! mutation testing showed nothing checked them.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use lattice_engines::catalog::CatalogError;
use lattice_engines::limits::{
    AllowAll, DenyAll, LimitError, PolicyEvaluator, RowRef, RowWriteRef, authorize_read,
    authorize_write,
};

#[test]
fn catalog_errors_print_their_variant_and_fields() {
    for (error, text) in [
        (CatalogError::IdUsed(7), "IdUsed(7)"),
        (
            CatalogError::NameTaken("notes".into()),
            "NameTaken(\"notes\")",
        ),
        (CatalogError::NoSuchTable(3), "NoSuchTable(3)"),
        (CatalogError::BadName, "BadName"),
        (CatalogError::OutOfOrder, "OutOfOrder"),
        (CatalogError::Empty, "Empty"),
        (CatalogError::Truncated, "Truncated"),
        (CatalogError::BadKind(9), "BadKind(9)"),
        (CatalogError::UnsupportedVersion(2), "UnsupportedVersion(2)"),
        (CatalogError::Trailing, "Trailing"),
    ] {
        assert_eq!(error.to_string(), text);
    }
}

#[test]
fn limit_errors_say_what_happened() {
    for (error, text) in [
        (LimitError::PermissionDenied, "row access denied"),
        (LimitError::Cancelled, "statement cancelled"),
        (LimitError::DeadlineExceeded, "statement deadline exceeded"),
        (LimitError::DeadlineOverflow, "statement deadline overflow"),
        (
            LimitError::MemoryExceeded {
                requested: 5,
                used: 3,
                limit: 7,
            },
            "statement memory limit exceeded: requested 5, used 3, limit 7",
        ),
    ] {
        assert_eq!(error.to_string(), text);
    }
}

#[test]
fn a_policy_behind_a_reference_keeps_its_answer() {
    let row = RowRef {
        table: 1,
        key: b"k",
        value: b"v",
    };
    let write = RowWriteRef {
        table: 1,
        key: b"k",
        before: None,
        after: Some(b"v"),
    };
    let deny = &DenyAll;
    let allow = &AllowAll;
    assert_eq!(
        authorize_read(&deny, row),
        Err(LimitError::PermissionDenied)
    );
    assert_eq!(
        authorize_write(&deny, write),
        Err(LimitError::PermissionDenied)
    );
    assert_eq!(authorize_read(&allow, row), Ok(()));
    assert_eq!(authorize_write(&allow, write), Ok(()));
    assert!(!(&deny).allows_read(row));
    assert!((&allow).allows_read(row));
}

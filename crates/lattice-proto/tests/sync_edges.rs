//! The printed text of the sync frame error, which reaches operators and logs.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use lattice_proto::sync::SyncError;

#[test]
fn every_sync_error_prints_its_text() {
    for (error, text) in [
        (
            SyncError::BadLength(70_000),
            "payload length 70000 is above the limit",
        ),
        (SyncError::UnknownType(0x0a), "unknown frame type 0x0a"),
        (SyncError::Truncated, "payload is truncated"),
        (SyncError::Trailing, "unexpected bytes after the last field"),
        (SyncError::NotUtf8, "text is not valid UTF-8"),
        (SyncError::TooLarge, "frame does not fit the size limit"),
        (SyncError::InvalidField("cursor"), "invalid cursor"),
    ] {
        assert_eq!(error.to_string(), text);
    }
}

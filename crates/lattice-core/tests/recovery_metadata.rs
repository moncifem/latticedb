//! Recovery metadata is a public contract, including after repairing a torn tail.
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use lattice_core::log::Log;
use lattice_core::record::{encode_record, segment_header};
use lattice_seams::Io;
use lattice_sim::{FaultProfile, SimIo};

#[test]
fn repaired_tail_reports_exact_bytes_and_the_durable_prefix() {
    let mut io = SimIo::new(1, FaultProfile::clean());
    let mut bytes = segment_header();
    for (lsn, payload) in [(1, b"one"), (2, b"two"), (3, b"tri")] {
        bytes.extend_from_slice(&encode_record(lsn, 1, payload));
    }
    // Nine bytes cannot contain a record header. They are the entire damaged suffix.
    bytes.extend_from_slice(&[0xFF; 9]);
    let path = "wal/00000000000000000001.seg";
    io.create_dir_all("wal").unwrap();
    io.append(path, &bytes).unwrap();
    io.sync(path).unwrap();
    io.sync_dir("wal").unwrap();

    let (mut log, report) = Log::open(io, "wal", 4096).unwrap();
    assert_eq!(report.records, 3);
    assert_eq!(report.segments, 1);
    assert_eq!(report.truncated_bytes, 9);
    assert_eq!(log.durable_lsn(), 3);
    assert_eq!(log.next_lsn(), 4);
    let records = log.read_from(1, 10).unwrap();
    assert_eq!(records.len(), 3);
    assert_eq!(records.iter().map(|r| r.lsn).collect::<Vec<_>>(), [1, 2, 3]);
    assert_eq!(records[0].payload, b"one");
    assert_eq!(records[1].payload, b"two");
    assert_eq!(records[2].payload, b"tri");
    assert_eq!(log.io().read(path).unwrap(), bytes[..bytes.len() - 9]);

    assert_eq!(log.append(1, b"four").unwrap(), 4);
    assert_eq!(log.durable_lsn(), 3);
    assert!(log.read_from(4, 1).unwrap().is_empty());
    assert_eq!(log.sync().unwrap(), 4);
    assert_eq!(log.durable_lsn(), 4);
    assert_eq!(log.next_lsn(), 5);
    let next = log.read_from(4, 1).unwrap();
    assert_eq!(next.len(), 1);
    assert_eq!(next[0].payload, b"four");
}

//! The reconnect proof over real sockets: a client is cut in the middle of an `Events` frame, resumes
//! from the cursor of the last event it processed, and in the end holds every committed row once and
//! in order. The expected rows are written out by hand from the inserts, not read back from the
//! gateway. Every wait is a bounded socket read (the final absence check is 10 reads of 50 ms); no
//! assertion compares elapsed time.
//!
//! This has the shape of evidence E3-3 asks for (a seeded kill schedule, resume by cursor, no gap, no
//! duplicate) over loopback sockets and one table. It is not E3-3: that needs the SDK supervisor and
//! the volume the gate sets.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use lattice_e2e::{Fixture, Ws, kill_points, row_value};

/// Events per `Events` frame. The history is committed before the first connection, so the first
/// connection gets full frames starting at the first row: a first kill point that is not a multiple
/// of this falls inside a frame. After a resume frames start at the cursor, so later boundaries shift.
const BATCH: usize = 16;

/// Rows committed before the first connection.
const HISTORY: u32 = 100;
/// Rows in all, once the tail is written while no client is connected.
const TOTAL: u32 = 110;

fn expected() -> Vec<String> {
    (1..=TOTAL).map(|n| format!("v{n:04}")).collect()
}

/// Runs one client through the kill schedule of `seed` and returns the rows it processed, in order,
/// and how many kills fell strictly inside a frame (the client had processed some of its events and
/// threw away the rest).
fn run(seed: u64) -> (Vec<String>, usize) {
    let fixture = Fixture::start_batched(&format!("reconnect-kill-{seed}"), BATCH).unwrap();
    fixture.seed(1..=HISTORY).unwrap();
    let kills: Vec<usize> = kill_points(seed, 4)
        .into_iter()
        .map(|p| usize::try_from(p).unwrap())
        .filter(|p| *p < TOTAL as usize)
        .collect();
    assert!(!kills.is_empty(), "seed {seed} schedules no kill");

    let mut processed: Vec<String> = Vec::new();
    let mut cursor: Vec<u8> = Vec::new();
    let mut mid_frame = 0;
    let mut kills = kills.into_iter();
    let mut tail_written = false;
    while processed.len() < TOTAL as usize {
        let mut ws = Ws::open(fixture.ws, &fixture.tokens.alice).unwrap();
        ws.hello("reconnect-kill").unwrap();
        ws.subscribe_from(1, "table/notes", &cursor).unwrap();
        let kill_at = kills.next();
        'connection: while processed.len() < TOTAL as usize {
            let events = ws.frame_events(1).unwrap();
            for (i, event) in events.iter().enumerate() {
                if Some(processed.len()) == kill_at {
                    // The kill falls here: the rest of this frame was delivered but never
                    // processed, so it is not acknowledged and must come again after the resume.
                    if i > 0 {
                        mid_frame += 1;
                    }
                    ws.ack(1, &cursor).unwrap();
                    break 'connection;
                }
                processed.push(row_value(event).unwrap());
                cursor.clone_from(&event.cursor);
            }
        }
        drop(ws);
        if !tail_written {
            // Rows committed while no client is connected come with the resume.
            fixture.seed(HISTORY + 1..=TOTAL).unwrap();
            tail_written = true;
        }
    }

    // Nothing more arrives: no duplicate is still in flight after the last row.
    let mut ws = Ws::open(fixture.ws, &fixture.tokens.alice).unwrap();
    ws.hello("reconnect-kill-check").unwrap();
    ws.subscribe_from(1, "table/notes", &cursor).unwrap();
    assert_eq!(ws.next_within(10).unwrap(), None);
    (processed, mid_frame)
}

#[test]
fn reconnect_with_cursor_has_no_gap_and_no_duplicate_after_a_kill_mid_batch() {
    for seed in 1..=4 {
        let (processed, mid_frame) = run(seed);
        assert_eq!(processed, expected(), "seed {seed}");
        // The first kill of seeds 1 to 4 is at event 2, 15, 46 and 11. None is a multiple of 16, and
        // the first connection gets full frames from row 1, so each seed is cut inside a frame at
        // least once.
        assert!(mid_frame >= 1, "seed {seed}: no kill fell inside a frame");
    }
}

#[test]
fn without_a_kill_one_connection_delivers_each_row_once_in_order() {
    // Control: the same expected rows with no kill, so the test above does not pass only because of
    // what a resume does to the order.
    let fixture = Fixture::start_batched("reconnect-control", BATCH).unwrap();
    fixture.seed(1..=TOTAL).unwrap();
    let mut ws = Ws::open(fixture.ws, &fixture.tokens.alice).unwrap();
    ws.hello("control").unwrap();
    ws.subscribe(1, "table/notes").unwrap();
    let rows: Vec<String> = ws
        .events(1, TOTAL as usize)
        .unwrap()
        .iter()
        .filter_map(row_value)
        .collect();
    assert_eq!(rows, expected());
}

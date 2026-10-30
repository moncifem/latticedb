//! The server decodes sync frames through `lattice-proto`, the crate the SDK client shares.
//!
//! The bytes are the worked examples of `formats/sync-protocol-v1.md`, the same ones
//! `lattice-sdk/tests/shared_codec.rs` decodes; both sides must read them as the same values.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use lattice_api::sync::{self, ErrorCode, Event, Message};

const WELCOME: [u8; 20] = [
    0x02, 0, 0, 0, 0x0f, 0, 1, 0, 0x10, 0, 0, 0, 7, b'l', b'a', b't', b't', b'i', b'c', b'e',
];
const EVENTS: [u8; 28] = [
    0x04, 0, 0, 0, 0x17, 0, 0, 0, 1, 0, 2, 0, 2, 0, 1, 0, 0, 0, 1, b'a', 0, 2, 0, 2, 0, 0, 0, 0,
];
const ERROR: [u8; 13] = [0x06, 0, 0, 0, 8, 0, 3, 0, 0, 0, 9, 0, 0];

type SharedDecode =
    fn(
        &[u8],
    )
        -> Result<Option<(lattice_proto::sync::Message, usize)>, lattice_proto::sync::SyncError>;

#[test]
fn the_api_paths_are_the_shared_crate() {
    // Coercing to a fn pointer over the shared crate's types only compiles when the re-export is
    // the same item, not a second copy.
    let shared: SharedDecode = sync::decode;
    assert_eq!(
        shared(&EVENTS).unwrap(),
        lattice_proto::sync::decode(&EVENTS).unwrap()
    );
}

#[test]
fn the_server_reads_the_example_frames_as_the_client_does() {
    let (m, n) = sync::decode(&WELCOME).unwrap().unwrap();
    assert_eq!(n, WELCOME.len());
    assert_eq!(
        m,
        Message::Welcome {
            version: 1,
            max_payload: 1 << 20,
            server: "lattice".into()
        }
    );
    let (m, _) = sync::decode(&EVENTS).unwrap().unwrap();
    assert_eq!(
        m,
        Message::Events {
            sub: 1,
            events: vec![
                Event {
                    cursor: vec![0, 1],
                    data: b"a".to_vec()
                },
                Event {
                    cursor: vec![0, 2],
                    data: vec![]
                },
            ]
        }
    );
    let (m, _) = sync::decode(&ERROR).unwrap().unwrap();
    assert_eq!(
        m,
        Message::Error {
            code: ErrorCode::UnknownSubscription,
            sub: 9,
            message: String::new()
        }
    );
}

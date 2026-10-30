//! The SDK client and the server codec share one definition of the sync frames (`lattice-proto`).
//!
//! The bytes are the worked examples of `formats/sync-protocol-v1.md`, written by hand. The same
//! bytes are decoded in `lattice-api/tests/shared_proto.rs` through `lattice_api::sync`; both must
//! give the same values, and here the decoded value goes into the client with no conversion.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use lattice_proto::sync::{self, ErrorCode, Event, Message};
use lattice_sdk::client::{Action, ClientConfig, Frame, SyncClient, WireError};

/// Welcome, version 1, max payload 1,048,576, server `lattice` (example 3).
const WELCOME: [u8; 20] = [
    0x02, 0, 0, 0, 0x0f, 0, 1, 0, 0x10, 0, 0, 0, 7, b'l', b'a', b't', b't', b'i', b'c', b'e',
];
/// Events, sub 1, two events (example 6).
const EVENTS: [u8; 28] = [
    0x04, 0, 0, 0, 0x17, 0, 0, 0, 1, 0, 2, 0, 2, 0, 1, 0, 0, 0, 1, b'a', 0, 2, 0, 2, 0, 0, 0, 0,
];
/// Error, unknown_subscription, sub 9, empty message (example 9).
const ERROR: [u8; 13] = [0x06, 0, 0, 0, 8, 0, 3, 0, 0, 0, 9, 0, 0];

fn decode_one(bytes: &[u8]) -> Message {
    let (msg, used) = sync::decode(bytes).unwrap().unwrap();
    assert_eq!(used, bytes.len());
    msg
}

#[test]
fn the_sdk_frame_is_the_codec_message() {
    // No conversion: this only compiles when `Frame` is the very type the codec returns.
    let frame: Frame = decode_one(&WELCOME);
    assert_eq!(
        frame,
        Message::Welcome {
            version: 1,
            max_payload: 1 << 20,
            server: "lattice".into()
        }
    );
    let code: WireError = ErrorCode::UnknownSubscription;
    assert_eq!(
        decode_one(&ERROR),
        Message::Error {
            code,
            sub: 9,
            message: String::new()
        }
    );
}

#[test]
fn bytes_decoded_by_the_codec_drive_the_client_and_its_frames_encode_back() {
    let mut c = SyncClient::new(ClientConfig::new("sdk-rs")).unwrap();
    let hello = c.connect(0).unwrap();
    let Action::Send(frame) = &hello[0] else {
        panic!("first action must send Hello")
    };
    // The Hello the client sends is encoded by the same codec the server decodes with
    // (example 1 of the format document).
    let mut wire = Vec::new();
    sync::encode(frame, &mut wire).unwrap();
    assert_eq!(
        wire,
        [
            0x01, 0, 0, 0, 12, 0, 1, 0, 1, 0, 6, b's', b'd', b'k', b'-', b'r', b's'
        ]
    );

    let a = c.handle(decode_one(&WELCOME), 0).unwrap();
    assert_eq!(
        a,
        vec![Action::Ready {
            version: 1,
            max_payload: 1 << 20
        }]
    );
    let (sub, _) = c.subscribe("ws/main", &[]).unwrap();
    assert_eq!(sub, 1);
    let a = c.handle(decode_one(&EVENTS), 1).unwrap();
    let delivered: Vec<(Vec<u8>, Vec<u8>)> = a
        .into_iter()
        .filter_map(|x| match x {
            Action::Deliver { cursor, data, .. } => Some((cursor, data)),
            _ => None,
        })
        .collect();
    assert_eq!(
        delivered,
        vec![(vec![0, 1], b"a".to_vec()), (vec![0, 2], Vec::new())]
    );
    assert_eq!(
        decode_one(&EVENTS),
        Message::Events {
            sub: 1,
            events: vec![
                Event {
                    cursor: vec![0, 1],
                    data: b"a".to_vec(),
                },
                Event {
                    cursor: vec![0, 2],
                    data: vec![],
                },
            ],
        }
    );
}

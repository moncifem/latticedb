//! WebSocket edges found by mutation testing: the documented size defaults, the printed text of every
//! error type, control-frame limits of the client encoder, and a one-byte close payload.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use lattice_api::sync::{HEADER_LEN, MAX_PAYLOAD};
use lattice_api::websocket::{
    Config, Connection, DEFAULT_MAX_FRAME_PAYLOAD, DEFAULT_MAX_HEAD, DEFAULT_MAX_MESSAGE,
    EncodeError, Event, HandshakeError, MAX_CONTROL_PAYLOAD, Opcode, SendError, Violation,
    encode_client_frame, encode_frame,
};

#[test]
fn the_size_defaults_are_the_documented_numbers() {
    assert_eq!(DEFAULT_MAX_HEAD, 8192);
    assert_eq!(DEFAULT_MAX_FRAME_PAYLOAD, HEADER_LEN + MAX_PAYLOAD);
    assert_eq!(DEFAULT_MAX_MESSAGE, HEADER_LEN + MAX_PAYLOAD);
    let cfg = Config::default();
    assert_eq!(
        (cfg.max_frame_payload, cfg.max_message),
        (DEFAULT_MAX_FRAME_PAYLOAD, DEFAULT_MAX_MESSAGE)
    );
}

#[test]
fn every_error_type_prints_its_text() {
    let handshake = HandshakeError {
        status: 403,
        reason: "origin not allowed",
    };
    assert_eq!(
        handshake.to_string(),
        "websocket upgrade refused (403): origin not allowed"
    );
    let violation = Violation {
        code: 1002,
        reason: "unmasked frame",
    };
    assert_eq!(
        violation.to_string(),
        "websocket violation (1002): unmasked frame"
    );
    assert_eq!(
        EncodeError::BadControlFrame.to_string(),
        "control frames must be final and at most 125 bytes"
    );
    assert_eq!(SendError::NotOpen.to_string(), "the connection is not open");
    assert_eq!(
        SendError::TooLarge.to_string(),
        "the message is above the configured limit"
    );
    assert_eq!(
        SendError::BadClose.to_string(),
        "invalid close code or reason"
    );
}

#[test]
fn the_client_encoder_refuses_a_bad_control_frame_for_each_reason_alone() {
    let key = [1, 2, 3, 4];
    let mut out = Vec::new();
    // Not final, payload fine.
    assert_eq!(
        encode_client_frame(false, Opcode::Ping, b"hi", key, &mut out),
        Err(EncodeError::BadControlFrame)
    );
    // Final, payload one over the limit.
    assert_eq!(
        encode_client_frame(
            true,
            Opcode::Ping,
            &[0; MAX_CONTROL_PAYLOAD + 1],
            key,
            &mut out
        ),
        Err(EncodeError::BadControlFrame)
    );
    assert!(out.is_empty(), "nothing is appended on refusal");
    // Exactly at the limit is fine, and a fragmented data frame is fine.
    assert!(
        encode_client_frame(true, Opcode::Ping, &[0; MAX_CONTROL_PAYLOAD], key, &mut out).is_ok()
    );
    assert!(encode_client_frame(false, Opcode::Text, b"part", key, &mut out).is_ok());
    // The server encoder follows the same rule.
    let mut server = Vec::new();
    assert_eq!(
        encode_frame(false, Opcode::Pong, b"", &mut server),
        Err(EncodeError::BadControlFrame)
    );
    assert_eq!(
        encode_frame(
            true,
            Opcode::Close,
            &[0; MAX_CONTROL_PAYLOAD + 1],
            &mut server
        ),
        Err(EncodeError::BadControlFrame)
    );
    assert!(server.is_empty());
}

#[test]
fn frame_headers_use_the_shortest_length_form() {
    let mut out = Vec::new();
    encode_frame(true, Opcode::Binary, &[7; 5], &mut out).unwrap();
    assert_eq!(&out[..2], &[0x82, 5]);
    out.clear();
    encode_frame(false, Opcode::Text, &[0; 126], &mut out).unwrap();
    assert_eq!(&out[..4], &[0x01, 126, 0, 126]);
    out.clear();
    encode_frame(true, Opcode::Binary, &vec![0; 65_536], &mut out).unwrap();
    assert_eq!(&out[..10], &[0x82, 127, 0, 0, 0, 0, 0, 1, 0, 0]);
    out.clear();
    encode_client_frame(true, Opcode::Text, b"ab", [9, 9, 9, 9], &mut out).unwrap();
    assert_eq!(&out[..2], &[0x81, 0x80 | 2]);
}

#[test]
fn a_close_frame_with_a_one_byte_payload_is_a_protocol_error() {
    let mut frame = Vec::new();
    encode_client_frame(true, Opcode::Close, &[0x03], [5, 6, 7, 8], &mut frame).unwrap();
    let mut c = Connection::new(Config::default(), 0);
    let step = c.receive(&frame, 0);
    match step.event {
        Some(Event::ProtocolError { code, reason }) => {
            assert_eq!(code, 1002);
            assert_eq!(reason, "close payload of one byte");
        }
        other => panic!("{other:?}"),
    }
    // An empty close payload is a normal close without a code.
    let mut empty = Vec::new();
    encode_client_frame(true, Opcode::Close, b"", [5, 6, 7, 8], &mut empty).unwrap();
    let mut d = Connection::new(Config::default(), 0);
    assert!(matches!(
        d.receive(&empty, 0).event,
        Some(Event::Closed { code: None, .. })
    ));
}

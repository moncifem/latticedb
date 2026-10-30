//! Wire codec edges found by mutation testing: exact length limits in both directions, SASL body
//! and mechanism limits, negative SASL response lengths, and the printed error text.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use lattice_api::pgwire::{
    Backend, Frontend, MAX_MESSAGE_LEN, MAX_SASL_BODY_LEN, ProtoError, SaslPhase,
    decode_frontend_sasl, decode_startup,
};

#[test]
fn a_startup_length_at_the_limit_waits_for_more_bytes_and_one_over_is_refused() {
    let at_limit = (MAX_MESSAGE_LEN as i32).to_be_bytes();
    assert_eq!(decode_startup(&at_limit), Ok(None));
    let over = (MAX_MESSAGE_LEN as i32 + 1).to_be_bytes();
    assert_eq!(
        decode_startup(&over),
        Err(ProtoError::BadLength(MAX_MESSAGE_LEN as i64 + 1))
    );
    // The smallest legal startup message is eight bytes.
    assert_eq!(
        decode_startup(&7i32.to_be_bytes()),
        Err(ProtoError::BadLength(7))
    );
}

#[test]
fn a_partial_startup_message_asks_for_more_and_a_complete_one_reports_its_size() {
    let mut full = 8i32.to_be_bytes().to_vec();
    full.extend_from_slice(&80_877_103i32.to_be_bytes()); // SSLRequest
    for cut in 0..full.len() {
        assert_eq!(decode_startup(&full[..cut]), Ok(None), "cut at {cut}");
    }
    assert_eq!(
        decode_startup(&full).unwrap().map(|(_, used)| used),
        Some(8)
    );
    // Bytes after the message are left alone.
    full.extend_from_slice(b"next");
    assert_eq!(
        decode_startup(&full).unwrap().map(|(_, used)| used),
        Some(8)
    );
}

fn sasl_frame(body: &[u8]) -> Vec<u8> {
    let mut frame = vec![b'p'];
    frame.extend_from_slice(&((body.len() + 4) as i32).to_be_bytes());
    frame.extend_from_slice(body);
    frame
}

#[test]
fn a_sasl_body_at_the_limit_is_read_and_one_byte_more_is_refused() {
    let at_limit = sasl_frame(&vec![b'x'; MAX_SASL_BODY_LEN]);
    let (message, used) = decode_frontend_sasl(&at_limit, SaslPhase::Response)
        .unwrap()
        .unwrap();
    assert_eq!(
        message,
        Frontend::SaslResponse(vec![b'x'; MAX_SASL_BODY_LEN])
    );
    assert_eq!(used, at_limit.len());
    // At the limit with only the header present, the decoder waits instead of refusing.
    assert_eq!(
        decode_frontend_sasl(&at_limit[..5], SaslPhase::Response),
        Ok(None)
    );
    let over = sasl_frame(&vec![b'x'; MAX_SASL_BODY_LEN + 1]);
    assert_eq!(
        decode_frontend_sasl(&over[..5], SaslPhase::Response),
        Err(ProtoError::TooLarge)
    );
}

fn initial_response(length: i32, data: &[u8]) -> Vec<u8> {
    let mut body = b"SCRAM-SHA-256\0".to_vec();
    body.extend_from_slice(&length.to_be_bytes());
    body.extend_from_slice(data);
    sasl_frame(&body)
}

#[test]
fn the_initial_response_length_is_minus_one_zero_or_a_size_and_nothing_else() {
    let decode = |frame: &[u8]| decode_frontend_sasl(frame, SaslPhase::InitialResponse);
    let data = |frame: Vec<u8>| match decode(&frame).unwrap().unwrap().0 {
        Frontend::SaslInitialResponse { data, .. } => data,
        other => panic!("{other:?}"),
    };
    assert_eq!(data(initial_response(-1, b"")), None);
    assert_eq!(data(initial_response(0, b"")), Some(Vec::new()));
    assert_eq!(data(initial_response(3, b"abc")), Some(b"abc".to_vec()));
    for bad in [-2, i32::MIN] {
        assert_eq!(
            decode(&initial_response(bad, b"")),
            Err(ProtoError::InvalidField("SASL initial response length")),
            "length {bad}"
        );
    }
    assert_eq!(
        decode(&initial_response(0, b"x")),
        Err(ProtoError::Trailing)
    );
}

#[test]
fn sasl_mechanism_lists_and_bodies_have_exact_encoding_limits() {
    let encode = |message: Backend| {
        let mut out = Vec::new();
        message.encode(&mut out).map(|()| out)
    };
    let names = |n: usize| {
        (0..n)
            .map(|_| "SCRAM-SHA-256".to_string())
            .collect::<Vec<_>>()
    };
    assert!(encode(Backend::AuthenticationSasl(names(1))).is_ok());
    assert!(encode(Backend::AuthenticationSasl(names(8))).is_ok());
    for n in [0, 9] {
        assert_eq!(
            encode(Backend::AuthenticationSasl(names(n))),
            Err(ProtoError::InvalidField("SASL mechanism count")),
            "{n} mechanisms"
        );
    }
    assert!(
        encode(Backend::AuthenticationSaslContinue(vec![
            0;
            MAX_SASL_BODY_LEN
        ]))
        .is_ok()
    );
    assert!(encode(Backend::AuthenticationSaslFinal(vec![0; MAX_SASL_BODY_LEN])).is_ok());
    assert_eq!(
        encode(Backend::AuthenticationSaslContinue(vec![
            0;
            MAX_SASL_BODY_LEN
                + 1
        ])),
        Err(ProtoError::TooLarge)
    );
}

#[test]
fn an_encoded_message_may_reach_the_length_limit_and_not_pass_it() {
    let encode = |text: String| {
        let mut out = Vec::new();
        Backend::CommandComplete(text)
            .encode(&mut out)
            .map(|()| out)
    };
    // The length field counts itself (4), the text and its terminator (1).
    let longest = encode("x".repeat(MAX_MESSAGE_LEN - 5)).unwrap();
    assert_eq!(longest.len(), MAX_MESSAGE_LEN + 1);
    assert_eq!(
        encode("x".repeat(MAX_MESSAGE_LEN - 4)),
        Err(ProtoError::TooLarge)
    );
}

#[test]
fn protocol_errors_print_what_went_wrong() {
    for (error, text) in [
        (ProtoError::BadLength(-3), "invalid message length -3"),
        (
            ProtoError::UnknownTag(0x5a),
            "unsupported message type 0x5a",
        ),
        (
            ProtoError::UnsupportedProtocol(7),
            "unsupported protocol code 7",
        ),
        (ProtoError::NotUtf8, "string is not valid UTF-8"),
        (ProtoError::Unterminated, "string is not terminated"),
        (ProtoError::Truncated, "message is truncated"),
        (
            ProtoError::Trailing,
            "unexpected bytes after the last field",
        ),
        (ProtoError::TooLarge, "value does not fit the wire format"),
        (
            ProtoError::InvalidField("format code"),
            "invalid format code",
        ),
    ] {
        assert_eq!(error.to_string(), text);
    }
}

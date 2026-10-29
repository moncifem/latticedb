#![no_main]
use lattice_api::sync::{HEADER_LEN, MAX_PAYLOAD, decode, decode_with_limit, encode};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // A frame that decodes re-encodes to exactly the bytes it came from: one encoding per message.
    if let Ok(Some((msg, used))) = decode(data) {
        assert!(used <= data.len() && used <= HEADER_LEN + MAX_PAYLOAD);
        let mut out = Vec::new();
        encode(&msg, &mut out).expect("a decoded message encodes");
        assert_eq!(out, &data[..used]);
        // The same frame is incomplete one byte short and unaffected by what follows it.
        assert_eq!(decode(&data[..used - 1]), Ok(None));
        assert_eq!(decode(&out), Ok(Some((msg, used))));
    }
    // A reader with a tiny negotiated limit never panics either.
    let _ = decode_with_limit(data, 16);
    // Treat the input as a stream: decode frame after frame until the decoder stops.
    let mut at = 0;
    while let Some(rest) = data.get(at..) {
        match decode(rest) {
            Ok(Some((_, used))) => at += used,
            _ => break,
        }
    }
});

#![no_main]
use lattice_sdk::contract::{Framer, MAX_LINE_BYTES, decode_line, encode_line};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // A line that decodes re-encodes to a line that decodes to the same message, and that line is
    // canonical: encoding it again changes nothing.
    if let Ok(msg) = decode_line(data) {
        let line = encode_line(&msg).expect("a decoded message encodes");
        assert!(line.len() <= MAX_LINE_BYTES);
        let again = decode_line(line.as_bytes()).expect("the canonical line decodes");
        assert_eq!(again, msg);
        assert_eq!(encode_line(&again).expect("encodes"), line);
    }
    // Treat the input as a stream: every line the framer yields is within the limit and
    // decoding it never panics.
    let mut framer = Framer::new();
    let mut lines = framer.push(data);
    lines.extend(framer.finish());
    for line in lines.into_iter().flatten() {
        assert!(line.len() <= MAX_LINE_BYTES);
        let _ = decode_line(&line);
    }
});

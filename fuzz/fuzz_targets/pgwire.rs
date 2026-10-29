#![no_main]
use lattice_api::pgwire::{Backend, decode_frontend, decode_startup};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Neither decoder may panic, and a decoded message never claims more bytes than it was given.
    if let Ok(Some((_, used))) = decode_frontend(data) {
        assert!(used <= data.len());
    }
    if let Ok(Some((_, used))) = decode_startup(data) {
        assert!(used <= data.len());
    }
    // Whatever text arrives as a query can be echoed back in a command tag or an error without
    // breaking the encoder: it either encodes with a correct length or is refused cleanly.
    if let Ok(text) = std::str::from_utf8(data) {
        for msg in [
            Backend::CommandComplete(text.to_string()),
            Backend::ErrorResponse {
                severity: "ERROR".into(),
                code: "42601".into(),
                message: text.to_string(),
            },
        ] {
            let mut out = Vec::new();
            if msg.encode(&mut out).is_ok() {
                let declared = u32::from_be_bytes([out[1], out[2], out[3], out[4]]) as usize;
                assert_eq!(declared, out.len() - 1);
            }
        }
    }
});

#![no_main]
use latticed::TlsConfig;
use libfuzzer_sys::fuzz_target;

/// The byte bounds `TlsConfig::from_pem` checks before it parses anything
/// (crates/latticed/src/transport.rs).
const MAX_CERTIFICATES_LEN: usize = 65536;
const MAX_KEY_LEN: usize = 16384;

fuzz_target!(|data: &[u8]| {
    // The input is `certificates NUL key`. Without a NUL the first half is the chain and the rest,
    // after one byte, is the key, so both parts are reached without a separator.
    let at = data.iter().position(|b| *b == 0).unwrap_or(data.len() / 2);
    let certificates = data.get(..at).unwrap_or_default();
    let key = data.get(at + 1..).unwrap_or_default();

    let result = TlsConfig::from_pem(certificates, key);
    // The size bounds are judged first, so an over-long chain or key is always refused.
    if certificates.len() > MAX_CERTIFICATES_LEN || key.len() > MAX_KEY_LEN {
        assert!(
            result.is_err(),
            "an over-long certificate chain or key was accepted"
        );
    }
    // Anything else either parses or is refused. Neither outcome may panic.
});

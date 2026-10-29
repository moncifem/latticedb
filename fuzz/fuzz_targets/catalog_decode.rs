#![no_main]
use lattice_engines::catalog;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(decoded) = catalog::decode(data) {
        let encoded = catalog::encode(&decoded).expect("a decoded payload must be encodable");
        assert_eq!(
            encoded, data,
            "successful decoding must preserve canonical bytes"
        );
        assert_eq!(catalog::decode(&encoded), Ok(decoded));
    }
});

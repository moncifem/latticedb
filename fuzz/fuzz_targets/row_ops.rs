#![no_main]
use lattice_engines::rowops;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(decoded) = rowops::decode(data) {
        let encoded = rowops::encode(&decoded).expect("a decoded payload must be encodable");
        assert_eq!(
            encoded, data,
            "successful decoding must preserve canonical bytes"
        );
        assert_eq!(rowops::decode(&encoded), Ok(decoded));
    }
});

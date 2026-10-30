//! TLS configuration import: exact size and chain-length bounds, key count, and redacted debug text.
//! Mutation testing showed none of these edges was pinned.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use latticed::TlsConfig;

fn material() -> (String, String) {
    let generated = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    (generated.cert.pem(), generated.signing_key.serialize_pem())
}

fn padded(pem: &str, total: usize) -> Vec<u8> {
    assert!(pem.len() <= total);
    let mut bytes = pem.as_bytes().to_vec();
    bytes.resize(total, b'\n');
    bytes
}

fn message(result: std::io::Result<TlsConfig>) -> String {
    result
        .map(|_| "accepted".to_string())
        .unwrap_or_else(|e| e.to_string())
}

#[test]
fn the_debug_text_never_shows_certificate_or_key_material() {
    let (cert, key) = material();
    let config = TlsConfig::from_pem(cert.as_bytes(), key.as_bytes()).unwrap();
    assert_eq!(
        format!("{config:?}"),
        "TlsConfig { certificate_and_key: [redacted] }"
    );
}

#[test]
fn certificate_and_key_sizes_are_bounded_exactly() {
    let (cert, key) = material();
    assert_eq!(
        message(TlsConfig::from_pem(&padded(&cert, 65_536), key.as_bytes())),
        "accepted"
    );
    assert_eq!(
        message(TlsConfig::from_pem(&padded(&cert, 65_537), key.as_bytes())),
        "TLS configuration exceeds bounds"
    );
    assert_eq!(
        message(TlsConfig::from_pem(cert.as_bytes(), &padded(&key, 16_384))),
        "accepted"
    );
    assert_eq!(
        message(TlsConfig::from_pem(cert.as_bytes(), &padded(&key, 16_385))),
        "TLS configuration exceeds bounds"
    );
    // Either input being too large is enough, with the other one fine.
    assert_eq!(
        message(TlsConfig::from_pem(
            &padded(&cert, 65_537),
            &padded(&key, 100 + key.len())
        )),
        "TLS configuration exceeds bounds"
    );
}

#[test]
fn a_chain_has_between_one_and_eight_certificates() {
    let (cert, key) = material();
    let chain = |n: usize| cert.repeat(n);
    assert_eq!(
        message(TlsConfig::from_pem(chain(1).as_bytes(), key.as_bytes())),
        "accepted"
    );
    assert_eq!(
        message(TlsConfig::from_pem(chain(8).as_bytes(), key.as_bytes())),
        "accepted"
    );
    for n in [0, 9] {
        assert_eq!(
            message(TlsConfig::from_pem(chain(n).as_bytes(), key.as_bytes())),
            "invalid TLS certificate chain",
            "{n} certificates"
        );
    }
}

#[test]
fn exactly_one_private_key_is_required() {
    let (cert, key) = material();
    let (_, other_key) = material();
    assert_eq!(
        message(TlsConfig::from_pem(cert.as_bytes(), b"")),
        "missing TLS private key"
    );
    assert_eq!(
        message(TlsConfig::from_pem(
            cert.as_bytes(),
            format!("{key}{other_key}").as_bytes()
        )),
        "multiple TLS private keys"
    );
}

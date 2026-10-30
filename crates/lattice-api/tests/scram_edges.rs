//! SCRAM edges found by mutation testing: credential bounds, nonce length, which malformed
//! message attributes are refused, message size, and the printed error text. The exchange itself is
//! checked against the RFC 7677 vector in the module's own tests.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use lattice_api::scram::{
    MAX_ITERATIONS, MAX_MESSAGE_BYTES, MECHANISM, MIN_ITERATIONS, ScramError, ScramServer, Verifier,
};

const SALT: [u8; 16] = *b"0123456789abcdef";
const SUFFIX: &str = "%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0";

fn verifier() -> Verifier {
    Verifier::from_prepared_password(b"pencil", &SALT, MIN_ITERATIONS).unwrap()
}

fn server() -> ScramServer {
    ScramServer::with_nonce_suffix(verifier(), SUFFIX).unwrap()
}

#[test]
fn credential_inputs_are_bounded_on_every_side() {
    let ok = |password: &[u8], salt: &[u8], iterations: u32| {
        Verifier::from_prepared_password(password, salt, iterations).is_ok()
    };
    assert!(ok(&[b'p'; 1024], &SALT, MIN_ITERATIONS));
    assert!(!ok(&[b'p'; 1025], &SALT, MIN_ITERATIONS));
    assert!(!ok(b"p", &[0u8; 15], MIN_ITERATIONS));
    assert!(ok(b"p", &[0u8; 16], MIN_ITERATIONS));
    assert!(ok(b"p", &[0u8; 64], MIN_ITERATIONS));
    assert!(!ok(b"p", &[0u8; 65], MIN_ITERATIONS));
    assert!(!ok(b"p", &SALT, MIN_ITERATIONS - 1));
    assert!(!ok(b"p", &SALT, MAX_ITERATIONS + 1));
    // The bound is checked before any key derivation: an absurd cost is refused at once, not after
    // billions of PBKDF2 rounds (a second check inside `from_keys` would give the same answer, much later).
    assert!(!ok(b"p", &SALT, u32::MAX));
    assert!(!ok(&[b'p'; 1025], &SALT, u32::MAX));
    assert!(!ok(b"p", &[0u8; 8], u32::MAX));
    // Any one violation is enough, with the other two fine.
    assert!(!ok(&[b'p'; 1025], &SALT, MIN_ITERATIONS));
    assert!(!ok(b"p", &[0u8; 8], MIN_ITERATIONS));
    assert!(!ok(b"p", &SALT, 1));
}

#[test]
fn the_server_nonce_suffix_has_a_minimum_length() {
    let suffix = |s: &str| ScramServer::with_nonce_suffix(verifier(), s).is_ok();
    assert!(!suffix(&"n".repeat(15)));
    assert!(suffix(&"n".repeat(16)));
    assert!(!suffix("short"));
    assert!(!suffix(&format!("{}{}", "n".repeat(16), ","))); // a comma is not a legal nonce byte
}

fn initial(message: &str) -> Result<(), ScramError> {
    server().initial(MECHANISM, message.as_bytes()).map(|_| ())
}

#[test]
fn the_first_message_must_have_a_username_then_a_clean_nonce() {
    assert_eq!(initial("n,,n=user,r=abc"), Ok(()));
    assert_eq!(initial("y,,n=user,r=abc"), Ok(()));
    assert_eq!(initial("n,,n=,r=abc"), Ok(())); // libpq sends an empty user
    // Wrong first attribute, wrong second attribute, bad name escape, bad nonce byte: each alone fails.
    assert_eq!(initial("n,,a=user,r=abc"), Err(ScramError::Malformed));
    assert_eq!(initial("n,,n=user,a=abc"), Err(ScramError::Malformed));
    assert_eq!(initial("n,,n=a=b,r=abc"), Err(ScramError::Malformed));
    assert_eq!(initial("n,,n=user,r=a b"), Err(ScramError::Malformed));
    assert_eq!(initial("n,,n=a=2C,r=abc"), Ok(()));
    assert_eq!(initial("n,,n=a=3D,r=abc"), Ok(()));
}

#[test]
fn attribute_grammar_refusals_each_stand_alone() {
    assert_eq!(initial("n,,nn=user,r=abc"), Err(ScramError::Malformed)); // key longer than one letter
    assert_eq!(initial("n,,1=user,r=abc"), Err(ScramError::Malformed)); // key not a letter
    assert_eq!(initial("n,,n=user,r="), Err(ScramError::Malformed)); // empty value outside n=
    assert_eq!(
        initial("n,,n=user,n=other,r=abc"),
        Err(ScramError::Malformed)
    ); // repeated key
    // Only the first two attributes are used, so a bad key in a later position is the only thing that
    // makes the grammar check matter: none of these may be silently ignored.
    assert_eq!(initial("n,,n=user,r=abc,1=x"), Err(ScramError::Malformed)); // key not a letter
    assert_eq!(initial("n,,n=user,r=abc,x1=x"), Err(ScramError::Malformed)); // key too long
    assert_eq!(initial("n,,n=user,r=abc,x="), Err(ScramError::Malformed)); // empty value
    assert_eq!(initial("n,,n=user,r=abc,n=y"), Err(ScramError::Malformed)); // repeated key
    assert_eq!(initial("n,,n=user,r=abc,x=y"), Ok(())); // a harmless extra attribute is allowed
    assert_eq!(
        initial("n,,n=user,r=abc,m=ext"),
        Err(ScramError::UnsupportedExtension)
    );
}

#[test]
fn message_size_is_limited_exactly() {
    let long_nonce = |total: usize| format!("n,,n=u,r={}", "a".repeat(total - "n,,n=u,r=".len()));
    // At the limit the size check passes and the nonce check refuses; one byte more is out of bounds.
    assert_eq!(
        initial(&long_nonce(MAX_MESSAGE_BYTES)),
        Err(ScramError::Malformed)
    );
    assert_eq!(
        initial(&long_nonce(MAX_MESSAGE_BYTES + 1)),
        Err(ScramError::OutOfBounds)
    );
}

#[test]
fn the_final_message_checks_each_field_and_the_echoed_nonce() {
    let nonce = format!("abc{SUFFIX}");
    let proof = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";
    let run = |message: &str| {
        let mut s = server();
        s.initial(MECHANISM, b"n,,n=user,r=abc").unwrap();
        s.finish(message.as_bytes()).map(|_| ())
    };
    // Too few attributes, then each of the three fixed positions wrong.
    assert_eq!(
        run(&format!("c=biws,p={proof}")),
        Err(ScramError::Malformed)
    );
    assert_eq!(
        run(&format!("x=biws,r={nonce},p={proof}")),
        Err(ScramError::Malformed)
    );
    assert_eq!(
        run(&format!("c=biws,x={nonce},p={proof}")),
        Err(ScramError::Malformed)
    );
    assert_eq!(
        run(&format!("c=biws,r={nonce},x={proof}")),
        Err(ScramError::Malformed)
    );
    // A wrong nonce and a wrong channel-binding header are transcript mismatches, not bad proofs.
    assert_eq!(
        run(&format!("c=biws,r=other,p={proof}")),
        Err(ScramError::TranscriptMismatch)
    );
    assert_eq!(
        run(&format!("c=eSws,r={nonce},p={proof}")),
        Err(ScramError::TranscriptMismatch)
    );
    // With everything echoed correctly, a proof that is not the client's fails as an invalid proof.
    assert_eq!(
        run(&format!("c=biws,r={nonce},p={proof}")),
        Err(ScramError::InvalidProof)
    );
}

#[test]
fn error_text_never_carries_credential_material() {
    for (error, text) in [
        (ScramError::InvalidState, "invalid SCRAM exchange state"),
        (
            ScramError::UnsupportedMechanism,
            "unsupported SASL mechanism",
        ),
        (
            ScramError::OutOfBounds,
            "SCRAM input exceeds configured bounds",
        ),
        (ScramError::Malformed, "malformed SCRAM message"),
        (
            ScramError::UnsupportedExtension,
            "unsupported SCRAM extension",
        ),
        (ScramError::TranscriptMismatch, "SCRAM transcript mismatch"),
        (ScramError::InvalidProof, "invalid SCRAM proof"),
        (ScramError::Crypto, "SCRAM primitive rejected parameters"),
    ] {
        assert_eq!(error.to_string(), text);
    }
}

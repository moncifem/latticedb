//! Credential file edges found by mutation testing: size and name limits, decoding the stored and
//! server keys (checked by a complete SCRAM login computed independently in the test), and the
//! startup name length limit.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use base64::{Engine as _, engine::general_purpose::STANDARD};
use hmac::{Hmac, KeyInit, Mac};
use lattice_api::auth::{Accounts, DecoySecret, MAX_ACCOUNTS, MAX_CREDENTIAL_BYTES};
use lattice_api::pgwire::Backend;
use lattice_api::scram::{MECHANISM, ScramError};
use lattice_seams::Rng;
use sha2::{Digest, Sha256};

struct Counter(u64);
impl Rng for Counter {
    fn next_u64(&mut self) -> u64 {
        self.0 += 1;
        self.0
    }
}

fn hmac(key: &[u8], data: &[u8]) -> [u8; 32] {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).unwrap();
    mac.update(data);
    mac.finalize().into_bytes().into()
}

const SALT: [u8; 16] = *b"0123456789abcdef";

/// A credential line for `password`, derived independently of the crate under test.
fn line(name: &str, password: &[u8]) -> String {
    let mut salted = [0u8; 32];
    pbkdf2::pbkdf2_hmac::<Sha256>(password, &SALT, 4096, &mut salted);
    let client_key = hmac(&salted, b"Client Key");
    let stored: [u8; 32] = Sha256::digest(client_key).into();
    let server = hmac(&salted, b"Server Key");
    format!(
        "{name}\tSCRAM-SHA-256$4096:{}${}:{}",
        STANDARD.encode(SALT),
        STANDARD.encode(stored),
        STANDARD.encode(server)
    )
}

fn accounts(text: &str) -> Result<Accounts, ScramError> {
    Accounts::parse(text, DecoySecret::from_bytes(&[7u8; 32]).unwrap())
}

#[test]
fn the_documented_limits_are_the_numbers_in_the_documentation() {
    assert_eq!(MAX_CREDENTIAL_BYTES, 262_144);
    assert_eq!(MAX_ACCOUNTS, 1024);
}

#[test]
fn a_complete_login_proves_both_keys_were_decoded_exactly() {
    let accounts = accounts(&line("user", b"pencil")).unwrap();
    let (mut server, _) = accounts.exchange("user", &mut Counter(10)).unwrap();
    let client_first_bare = "n=user,r=clientnonce";
    let Backend::AuthenticationSaslContinue(first) = server
        .initial(MECHANISM, format!("n,,{client_first_bare}").as_bytes())
        .unwrap()
    else {
        panic!("expected a SASL continue message");
    };
    let server_first = String::from_utf8(first).unwrap();
    let nonce = server_first
        .split(',')
        .next()
        .unwrap()
        .strip_prefix("r=")
        .unwrap();

    let mut salted = [0u8; 32];
    pbkdf2::pbkdf2_hmac::<Sha256>(b"pencil", &SALT, 4096, &mut salted);
    let client_key = hmac(&salted, b"Client Key");
    let stored: [u8; 32] = Sha256::digest(client_key).into();
    let without_proof = format!("c=biws,r={nonce}");
    let auth = format!("{client_first_bare},{server_first},{without_proof}");
    let signature = hmac(&stored, auth.as_bytes());
    let proof: Vec<u8> = client_key
        .iter()
        .zip(signature)
        .map(|(a, b)| a ^ b)
        .collect();
    let message = format!("{without_proof},p={}", STANDARD.encode(proof));

    let Backend::AuthenticationSaslFinal(final_message) =
        server.finish(message.as_bytes()).unwrap()
    else {
        panic!("expected a SASL final message");
    };
    let expected = hmac(&hmac(&salted, b"Server Key"), auth.as_bytes());
    assert_eq!(
        String::from_utf8(final_message).unwrap(),
        format!("v={}", STANDARD.encode(expected))
    );
}

#[test]
fn account_names_may_be_sixty_three_bytes_and_not_sixty_four() {
    let ok = "a".repeat(63);
    let too_long = "a".repeat(64);
    let parsed = accounts(&line(&ok, b"pw")).unwrap();
    assert!(parsed.exchange(&ok, &mut Counter(1)).is_ok());
    assert_eq!(
        accounts(&line(&too_long, b"pw")).map(|_| ()),
        Err(ScramError::OutOfBounds)
    );
    // The startup name is bounded the same way before any credential lookup.
    assert_eq!(
        parsed.exchange(&too_long, &mut Counter(1)).map(|_| ()),
        Err(ScramError::Malformed)
    );
    assert_eq!(
        parsed.exchange("", &mut Counter(1)).map(|_| ()),
        Err(ScramError::Malformed)
    );
}

#[test]
fn the_account_count_and_file_size_limits_are_exact() {
    let many = |n: usize| {
        (0..n)
            .map(|i| line(&format!("u{i}"), b"pw"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert!(accounts(&many(MAX_ACCOUNTS)).is_ok());
    assert_eq!(
        accounts(&many(MAX_ACCOUNTS + 1)).map(|_| ()),
        Err(ScramError::OutOfBounds)
    );
    let one = line("user", b"pw");
    let padded = format!(
        "{one}\n{}",
        "\n".repeat(MAX_CREDENTIAL_BYTES - one.len() - 1)
    );
    assert_eq!(padded.len(), MAX_CREDENTIAL_BYTES);
    assert!(accounts(&padded).is_ok());
    assert_eq!(
        accounts(&format!("{padded}\n")).map(|_| ()),
        Err(ScramError::OutOfBounds)
    );
}

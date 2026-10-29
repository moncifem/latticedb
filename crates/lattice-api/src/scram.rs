//! Bounded, sans-I/O SCRAM-SHA-256 server exchange (RFC 5802 / RFC 7677).
//!
//! A host selects a verifier using the *startup* user name, then supplies a fresh server nonce
//! through the RNG seam. PostgreSQL ignores the SCRAM user name for account selection. Provisioning
//! accepts already prepared password bytes: SASLprep and PostgreSQL's raw-password fallback belong
//! to the credential-management boundary, not this exchange. No password database, session wiring,
//! TLS, SCRAM-SHA-256-PLUS or channel binding is implemented here. Do not log authentication input.
//!
//! Bounds apply before parsing or password derivation. Verifiers carry stored/server keys rather
//! than plaintext passwords. Proof validation compares fixed 32-byte hashes using `subtle`;
//! malformed public framing, nonce and channel-binding fields can fail before that comparison.
//! Secret arrays owned here are zeroized on drop; dependencies and caller-owned input can retain
//! their own copies. Every exchange error is terminal, so a proof cannot be retried in one state.
//!
//! References: <https://www.rfc-editor.org/rfc/rfc5802> and
//! <https://www.rfc-editor.org/rfc/rfc7677>. Authentication still requires a protected transport.

use crate::pgwire::Backend;
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD};
use hmac::{Hmac, KeyInit, Mac};
use lattice_seams::Rng;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fmt;
use subtle::ConstantTimeEq;
use zeroize::Zeroizing;

/// The only advertised mechanism; channel binding is not supported.
pub const MECHANISM: &str = "SCRAM-SHA-256";
/// Maximum UTF-8 authentication message, before parsing or copying.
pub const MAX_MESSAGE_BYTES: usize = 8192;
/// Maximum client nonce and server nonce suffix, independently.
pub const MAX_NONCE_BYTES: usize = 256;
/// Smallest configured derivation count accepted for a SHA-256 verifier.
pub const MIN_ITERATIONS: u32 = 4096;
/// Provisioning work ceiling; network messages never select or trigger this work.
pub const MAX_ITERATIONS: u32 = 1_000_000;
const MAX_ATTRIBUTES: usize = 32;
const KEY_BYTES: usize = 32;

/// Failure without user input, credential material or transcript data in its display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScramError {
    /// The exchange is complete, failed, or received a message in the wrong phase.
    InvalidState,
    /// Only SCRAM-SHA-256 without channel binding is available.
    UnsupportedMechanism,
    /// Authentication message or credential configuration exceeds a bound.
    OutOfBounds,
    /// Invalid UTF-8, attribute grammar, ordering, duplicate fields or encoding.
    Malformed,
    /// A mandatory extension, authorization identity or channel binding is unsupported.
    UnsupportedExtension,
    /// The client did not echo the exact combined nonce or GS2 header.
    TranscriptMismatch,
    /// Client proof does not authenticate against the configured verifier.
    InvalidProof,
    /// A cryptographic primitive rejected its parameters.
    Crypto,
}
impl fmt::Display for ScramError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidState => "invalid SCRAM exchange state",
            Self::UnsupportedMechanism => "unsupported SASL mechanism",
            Self::OutOfBounds => "SCRAM input exceeds configured bounds",
            Self::Malformed => "malformed SCRAM message",
            Self::UnsupportedExtension => "unsupported SCRAM extension",
            Self::TranscriptMismatch => "SCRAM transcript mismatch",
            Self::InvalidProof => "invalid SCRAM proof",
            Self::Crypto => "SCRAM primitive rejected parameters",
        })
    }
}
impl std::error::Error for ScramError {}

/// Provisioned SCRAM credential. Debug output never includes the keys.
#[derive(Clone)]
pub struct Verifier {
    salt: Vec<u8>,
    iterations: u32,
    stored_key: Zeroizing<[u8; KEY_BYTES]>,
    server_key: Zeroizing<[u8; KEY_BYTES]>,
}
impl fmt::Debug for Verifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Verifier")
            .field("iterations", &self.iterations)
            .field("keys", &"[redacted]")
            .finish_non_exhaustive()
    }
}
impl Verifier {
    /// Imports keys produced by trusted credential provisioning, checking public work/size bounds.
    pub fn from_keys(
        salt: &[u8],
        iterations: u32,
        stored_key: [u8; KEY_BYTES],
        server_key: [u8; KEY_BYTES],
    ) -> Result<Self, ScramError> {
        let stored_key = Zeroizing::new(stored_key);
        let server_key = Zeroizing::new(server_key);
        if !(16..=64).contains(&salt.len())
            || !(MIN_ITERATIONS..=MAX_ITERATIONS).contains(&iterations)
        {
            return Err(ScramError::OutOfBounds);
        }
        Ok(Self {
            salt: salt.to_vec(),
            iterations,
            stored_key,
            server_key,
        })
    }

    /// Derives a verifier during provisioning, never while processing an authentication message.
    ///
    /// Caller must apply its password-preparation policy first. This function treats bytes exactly
    /// as supplied and does not implement SASLprep or infer a PostgreSQL encoding.
    pub fn from_prepared_password(
        password: &[u8],
        salt: &[u8],
        iterations: u32,
    ) -> Result<Self, ScramError> {
        if password.len() > 1024
            || !(16..=64).contains(&salt.len())
            || !(MIN_ITERATIONS..=MAX_ITERATIONS).contains(&iterations)
        {
            return Err(ScramError::OutOfBounds);
        }
        let mut salted = Zeroizing::new([0u8; KEY_BYTES]);
        pbkdf2::pbkdf2_hmac::<Sha256>(password, salt, iterations, salted.as_mut());
        let client_key = Zeroizing::new(mac(&salted, b"Client Key")?);
        let stored_key: [u8; KEY_BYTES] = Sha256::digest(client_key.as_ref()).into();
        let server_key = mac(&salted, b"Server Key")?;
        Self::from_keys(salt, iterations, stored_key, server_key)
    }
}

fn mac(key: &[u8; KEY_BYTES], bytes: &[u8]) -> Result<[u8; KEY_BYTES], ScramError> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).map_err(|_| ScramError::Crypto)?;
    mac.update(bytes);
    Ok(mac.finalize().into_bytes().into())
}

struct Pending {
    gs2: String,
    nonce: String,
    client_first_bare: String,
    server_first: String,
}
enum State {
    First,
    Final(Pending),
    Complete,
    Closed,
}

/// A single-use server exchange; no ambient clock, randomness, filesystem or network operations.
pub struct ScramServer {
    verifier: Verifier,
    nonce_suffix: String,
    state: State,
}
impl ScramServer {
    /// Draws a 192-bit server nonce suffix. Production must supply a cryptographically secure RNG.
    pub fn new(verifier: Verifier, rng: &mut (impl Rng + ?Sized)) -> Self {
        let mut nonce = [0u8; 24];
        for chunk in nonce.as_chunks_mut::<8>().0 {
            chunk.copy_from_slice(&rng.next_u64().to_be_bytes());
        }
        Self {
            verifier,
            nonce_suffix: STANDARD_NO_PAD.encode(nonce),
            state: State::First,
        }
    }

    /// Uses an explicit nonce suffix for a deterministic host or published known-answer vector.
    /// Caller is responsible for fresh, unpredictable production nonces; validation is not entropy.
    pub fn with_nonce_suffix(verifier: Verifier, nonce_suffix: &str) -> Result<Self, ScramError> {
        if nonce_suffix.len() < 16 || !valid_nonce(nonce_suffix) {
            return Err(ScramError::OutOfBounds);
        }
        Ok(Self {
            verifier,
            nonce_suffix: nonce_suffix.to_string(),
            state: State::First,
        })
    }

    /// AuthenticationSASL offer. No PLUS mechanism is advertised without a TLS binding.
    pub fn offer() -> Backend {
        Backend::AuthenticationSasl(vec![MECHANISM.to_string()])
    }

    /// Whether the final proof has been accepted. Sending AuthenticationOk is the host's next step.
    pub fn is_authenticated(&self) -> bool {
        matches!(self.state, State::Complete)
    }

    /// Validates SASLInitialResponse and returns AuthenticationSASLContinue.
    pub fn initial(&mut self, mechanism: &str, bytes: &[u8]) -> Result<Backend, ScramError> {
        let previous = std::mem::replace(&mut self.state, State::Closed);
        if !matches!(previous, State::First) {
            return Err(ScramError::InvalidState);
        }
        if mechanism != MECHANISM {
            return Err(ScramError::UnsupportedMechanism);
        }
        let message = text(bytes)?;
        let (flag, rest) = message.split_once(',').ok_or(ScramError::Malformed)?;
        let (authz, bare) = rest.split_once(',').ok_or(ScramError::Malformed)?;
        if !matches!(flag, "n" | "y") || !authz.is_empty() {
            return Err(ScramError::UnsupportedExtension);
        }
        let attrs = attributes(bare)?;
        let first = attrs.first().ok_or(ScramError::Malformed)?;
        let second = attrs.get(1).ok_or(ScramError::Malformed)?;
        if first.0 != "n" || second.0 != "r" || !valid_sasl_name(first.1) || !valid_nonce(second.1)
        {
            return Err(ScramError::Malformed);
        }
        let nonce = format!("{}{}", second.1, self.nonce_suffix);
        let server_first = format!(
            "r={nonce},s={},i={}",
            STANDARD.encode(&self.verifier.salt),
            self.verifier.iterations
        );
        self.state = State::Final(Pending {
            gs2: format!("{flag},,"),
            nonce,
            client_first_bare: bare.to_string(),
            server_first: server_first.clone(),
        });
        Ok(Backend::AuthenticationSaslContinue(
            server_first.into_bytes(),
        ))
    }

    /// Validates SASLResponse and returns AuthenticationSASLFinal only for a correct proof.
    pub fn finish(&mut self, bytes: &[u8]) -> Result<Backend, ScramError> {
        let previous = std::mem::replace(&mut self.state, State::Closed);
        let State::Final(pending) = previous else {
            return Err(ScramError::InvalidState);
        };
        let message = text(bytes)?;
        let attrs = attributes(message)?;
        let binding = attrs.first().ok_or(ScramError::Malformed)?;
        let nonce = attrs.get(1).ok_or(ScramError::Malformed)?;
        let proof = attrs.last().ok_or(ScramError::Malformed)?;
        if attrs.len() < 3 || binding.0 != "c" || nonce.0 != "r" || proof.0 != "p" {
            return Err(ScramError::Malformed);
        }
        if nonce.1 != pending.nonce
            || STANDARD
                .decode(binding.1)
                .map_err(|_| ScramError::Malformed)?
                != pending.gs2.as_bytes()
        {
            return Err(ScramError::TranscriptMismatch);
        }
        let mut decoded_proof = Zeroizing::new([0u8; KEY_BYTES]);
        if STANDARD
            .decode_slice(proof.1, decoded_proof.as_mut())
            .map_err(|_| ScramError::Malformed)?
            != KEY_BYTES
        {
            return Err(ScramError::Malformed);
        }
        let (without_proof, _) = message.rsplit_once(",p=").ok_or(ScramError::Malformed)?;
        let auth = format!(
            "{},{},{}",
            pending.client_first_bare, pending.server_first, without_proof
        );
        let signature = Zeroizing::new(mac(&self.verifier.stored_key, auth.as_bytes())?);
        let mut client_key = Zeroizing::new([0u8; KEY_BYTES]);
        for ((output, proof), signature) in client_key
            .iter_mut()
            .zip(decoded_proof.iter())
            .zip(signature.iter())
        {
            *output = proof ^ signature;
        }
        let candidate =
            Zeroizing::new(<[u8; KEY_BYTES]>::from(Sha256::digest(client_key.as_ref())));
        if !bool::from(candidate.as_ref().ct_eq(self.verifier.stored_key.as_ref())) {
            return Err(ScramError::InvalidProof);
        }
        let server_signature = Zeroizing::new(mac(&self.verifier.server_key, auth.as_bytes())?);
        let final_message = format!("v={}", STANDARD.encode(server_signature.as_ref()));
        self.state = State::Complete;
        Ok(Backend::AuthenticationSaslFinal(final_message.into_bytes()))
    }
}

fn text(bytes: &[u8]) -> Result<&str, ScramError> {
    if bytes.len() > MAX_MESSAGE_BYTES {
        return Err(ScramError::OutOfBounds);
    }
    if bytes.contains(&0) {
        return Err(ScramError::Malformed);
    }
    std::str::from_utf8(bytes).map_err(|_| ScramError::Malformed)
}
fn valid_nonce(nonce: &str) -> bool {
    !nonce.is_empty()
        && nonce.len() <= MAX_NONCE_BYTES
        && nonce
            .bytes()
            .all(|b| (0x21..=0x7e).contains(&b) && b != b',')
}
fn valid_sasl_name(name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    let mut bytes = name.bytes();
    while let Some(byte) = bytes.next() {
        if byte == b'='
            && !matches!(
                (bytes.next(), bytes.next()),
                (Some(b'2'), Some(b'C')) | (Some(b'3'), Some(b'D'))
            )
        {
            return false;
        }
    }
    true
}
fn attributes(message: &str) -> Result<Vec<(&str, &str)>, ScramError> {
    let mut seen = BTreeSet::new();
    let mut result = Vec::new();
    for part in message.split(',') {
        if result.len() == MAX_ATTRIBUTES {
            return Err(ScramError::OutOfBounds);
        }
        let (key, value) = part.split_once('=').ok_or(ScramError::Malformed)?;
        if key.len() != 1
            || !key.bytes().all(|b| b.is_ascii_alphabetic())
            || value.is_empty()
            || !seen.insert(key)
        {
            return Err(ScramError::Malformed);
        }
        if key == "m" {
            return Err(ScramError::UnsupportedExtension);
        }
        result.push((key, value));
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )]
    use super::*;
    use proptest::prelude::*;
    const CLIENT: &str = "n,,n=user,r=rOprNGfwEbeRWgbNEkqO";
    const SUFFIX: &str = "%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0";
    const FIRST: &str =
        "r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,s=W22ZaJ0SNY7soEsUEjb6gQ==,i=4096";
    const FINAL: &str = "c=biws,r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,p=dHzbZapWIk4jUhN+Ute9ytag9zjfMHgsqmmiz7AndVQ=";
    fn verifier() -> Verifier {
        Verifier::from_prepared_password(
            b"pencil",
            &STANDARD.decode("W22ZaJ0SNY7soEsUEjb6gQ==").unwrap(),
            4096,
        )
        .unwrap()
    }
    fn server() -> ScramServer {
        ScramServer::with_nonce_suffix(verifier(), SUFFIX).unwrap()
    }

    #[test]
    fn rfc7677_sha256_exchange_matches_published_server_signature() {
        let mut server = server();
        assert_eq!(
            server.initial(MECHANISM, CLIENT.as_bytes()).unwrap(),
            Backend::AuthenticationSaslContinue(FIRST.as_bytes().to_vec())
        );
        assert!(!server.is_authenticated());
        assert_eq!(
            server.finish(FINAL.as_bytes()).unwrap(),
            Backend::AuthenticationSaslFinal(
                b"v=6rriTRBi23WpRR/wtup+mMhUZUn/dB5nLTJRsjl95G4=".to_vec()
            )
        );
        assert!(server.is_authenticated());
        assert_eq!(
            server.finish(FINAL.as_bytes()),
            Err(ScramError::InvalidState)
        );
        assert!(!server.is_authenticated());
    }

    #[test]
    fn rfc5802_gs2_and_attribute_grammar_are_checked_before_authentication() {
        for first in [
            "n,,n=user,r=fyko+d2lbbFgONRv9qkxdawL",
            "y,,n=user=2Cname=3D,r=nonce,x=extension",
        ] {
            assert!(server().initial(MECHANISM, first.as_bytes()).is_ok());
        }
        for first in [
            "n,,n=user,r=a,r=b",
            "n,,m=required,n=user,r=a",
            "n,,n=user=2c,r=a",
            "n,,r=a,n=user",
            "n,,n=user,r=has space",
            "n,,n=user,r=",
            "n,,n=user,r=a,",
        ] {
            assert!(
                server().initial(MECHANISM, first.as_bytes()).is_err(),
                "{first}"
            );
        }
        for first in ["p=tls-server-end-point,,n=user,r=a", "n,a=user,n=user,r=a"] {
            assert_eq!(
                server().initial(MECHANISM, first.as_bytes()),
                Err(ScramError::UnsupportedExtension)
            );
        }
    }

    #[test]
    fn invalid_proof_nonce_binding_and_encoding_are_terminal() {
        for replacement in [
            "c=eSws",
            "r=wrong",
            "p=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
            "p=not-base64",
            "p=AA==",
            "p=dHzbZapWIk4jUhN+Ute9ytag9zjfMHgsqmmiz7AndVR=",
        ] {
            let mut server = server();
            server.initial(MECHANISM, CLIENT.as_bytes()).unwrap();
            let position = replacement.get(..2).unwrap();
            let changed = FINAL
                .split(',')
                .map(|part| {
                    if part.starts_with(position) {
                        replacement
                    } else {
                        part
                    }
                })
                .collect::<Vec<_>>()
                .join(",");
            assert!(server.finish(changed.as_bytes()).is_err(), "{replacement}");
            assert!(!server.is_authenticated());
            assert_eq!(
                server.finish(FINAL.as_bytes()),
                Err(ScramError::InvalidState)
            );
        }
    }

    #[test]
    fn changing_any_client_proof_byte_is_rejected() {
        let credential = verifier();
        let (prefix, encoded) = FINAL.rsplit_once(",p=").unwrap();
        let proof = STANDARD.decode(encoded).unwrap();
        for index in 0..32 {
            let mut changed = proof.clone();
            changed[index] ^= 1;
            let mut server = ScramServer::with_nonce_suffix(credential.clone(), SUFFIX).unwrap();
            server.initial(MECHANISM, CLIENT.as_bytes()).unwrap();
            let message = format!("{prefix},p={}", STANDARD.encode(changed));
            assert_eq!(
                server.finish(message.as_bytes()),
                Err(ScramError::InvalidProof)
            );
        }
    }

    #[test]
    fn bounds_and_config_errors_are_refused_without_derivation() {
        for iterations in [0, 4095, MAX_ITERATIONS + 1, u32::MAX] {
            assert!(matches!(
                Verifier::from_keys(&[1; 16], iterations, [0; 32], [0; 32]),
                Err(ScramError::OutOfBounds)
            ));
        }
        for salt in [vec![], vec![0; 15], vec![0; 65]] {
            assert!(Verifier::from_keys(&salt, 4096, [0; 32], [0; 32]).is_err());
        }
        assert!(Verifier::from_prepared_password(&[0; 1025], &[1; 16], 4096).is_err());
        assert!(ScramServer::with_nonce_suffix(verifier(), &"x".repeat(257)).is_err());
        assert!(ScramServer::with_nonce_suffix(verifier(), "has,comma,but-long").is_err());
        assert!(matches!(
            server().initial(MECHANISM, &vec![b'x'; MAX_MESSAGE_BYTES + 1]),
            Err(ScramError::OutOfBounds)
        ));
        assert_eq!(
            server().initial("SCRAM-SHA-256-PLUS", CLIENT.as_bytes()),
            Err(ScramError::UnsupportedMechanism)
        );
    }

    #[test]
    fn every_initial_and_final_truncation_fails_without_a_panic_or_authentication() {
        // A client nonce can legitimately end earlier, so truncation is not always invalid initial syntax.
        for cut in 0..CLIENT.len() {
            let _ = server().initial(MECHANISM, &CLIENT.as_bytes()[..cut]);
        }
        for cut in 0..FINAL.len() {
            let mut s = server();
            s.initial(MECHANISM, CLIENT.as_bytes()).unwrap();
            assert!(s.finish(&FINAL.as_bytes()[..cut]).is_err());
            assert!(!s.is_authenticated());
        }
    }

    #[test]
    fn verifier_debug_redacts_all_credential_keys() {
        assert_eq!(
            format!("{:?}", verifier()),
            "Verifier { iterations: 4096, keys: \"[redacted]\", .. }"
        );
    }

    #[test]
    fn rng_nonce_uses_all_three_seam_draws_without_ambient_randomness() {
        struct Counter(u64);
        impl lattice_seams::Rng for Counter {
            fn next_u64(&mut self) -> u64 {
                self.0 += 1;
                self.0
            }
        }
        let mut rng = Counter(0);
        let mut server = ScramServer::new(verifier(), &mut rng);
        assert_eq!(rng.0, 3);
        assert_eq!(
            server.initial(MECHANISM, b"n,,n=user,r=nonce").unwrap(),
            Backend::AuthenticationSaslContinue(
                b"r=nonceAAAAAAAAAAEAAAAAAAAAAgAAAAAAAAAD,s=W22ZaJ0SNY7soEsUEjb6gQ==,i=4096"
                    .to_vec()
            )
        );
    }

    #[test]
    fn independently_derived_extension_transcript_includes_raw_first_and_final_attributes() {
        // Python stdlib hashlib.pbkdf2_hmac/hmac independently derives this SHA-256 fixture.
        let mut server = server();
        server
            .initial(MECHANISM, b"y,,n=user=2Cname=3D,r=clientNonce,x=first")
            .unwrap();
        assert_eq!(server.finish(b"c=eSws,r=clientNonce%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,z=final,p=yzX2HnUAkoDBxkZBxttk7IdnUpY2Zj21al6WuK6Y20Y=" ).unwrap(),
            Backend::AuthenticationSaslFinal(b"v=Bk7NhlxPh5oKvQU3YoHe9WvcBG4YnmgO4x1DBrVizgQ=".to_vec()));
        assert!(server.is_authenticated());
    }

    proptest! {
        #[test]
        fn arbitrary_inputs_never_authenticate_or_panic(bytes in prop::collection::vec(any::<u8>(), 0..500)) {
            let verifier = Verifier::from_keys(&[1;16],4096,[0;32],[0;32]).unwrap();
            let mut server = ScramServer::with_nonce_suffix(verifier, "fixed-test-nonce-suffix").unwrap();
            let _ = server.initial(MECHANISM,&bytes);
            let _ = server.finish(&bytes);
            prop_assert!(!server.is_authenticated());
            let verifier = Verifier::from_keys(&[1;16],4096,[0;32],[0;32]).unwrap();
            let mut server = ScramServer::with_nonce_suffix(verifier, "fixed-test-nonce-suffix").unwrap();
            server.initial(MECHANISM,CLIENT.as_bytes()).unwrap();
            let _ = server.finish(&bytes);
            prop_assert!(!server.is_authenticated());
        }
    }
}

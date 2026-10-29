//! Bounded, provisioned SCRAM account lookup; no password derivation on the network path.
use crate::scram::{ScramError, ScramServer, Verifier};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use hmac::{Hmac, KeyInit, Mac};
use lattice_seams::Rng;
use sha2::Sha256;
use std::{collections::BTreeMap, fmt};
use zeroize::Zeroizing;

/// Maximum credential file size, including account names and encoded verifiers.
pub const MAX_CREDENTIAL_BYTES: usize = 256 * 1024;
/// Maximum configured accounts.
pub const MAX_ACCOUNTS: usize = 1024;

/// A provisioned persistent 32-byte secret for synthetic account derivation.
/// Keep the same secret across reloads/restarts and protect it like the verifier file. Rotation
/// changes unknown-account challenges, so it is observable and needs a separate operational policy.
/// This checks length, not entropy: provisioning must supply cryptographically random bytes.
pub struct DecoySecret(Zeroizing<[u8; 32]>);
impl DecoySecret {
    /// Imports exactly 32 binary bytes; no secret generation or textual decoding occurs here.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, ScramError> {
        let mut secret = Zeroizing::new([0; 32]);
        if bytes.len() != secret.len() {
            return Err(ScramError::OutOfBounds);
        }
        secret.copy_from_slice(bytes);
        Ok(Self(secret))
    }
}
impl fmt::Debug for DecoySecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("DecoySecret([redacted])")
    }
}

/// Immutable credentials with one public cost profile: 4096 iterations and a 16-byte salt.
/// Unknown users get a stable, keyed synthetic verifier across account reloads and restarts using the same provisioned secret and startup name.
/// This avoids distinct offers/failure messages, but does not claim measured timing resistance.
pub struct Accounts {
    entries: BTreeMap<String, Verifier>,
    decoy_key: DecoySecret,
}
impl fmt::Debug for Accounts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Accounts")
            .field("count", &self.entries.len())
            .finish_non_exhaustive()
    }
}
impl Accounts {
    /// Parses `username<TAB>SCRAM-SHA-256$4096:base64salt$storedkey:serverkey` lines.
    /// Provision externally using prepared password bytes; never put plaintext passwords here.
    /// Empty files, duplicate/invalid names, mixed cost profiles and malformed keys are refused.
    /// The persistent decoy secret is mandatory; no ephemeral fallback is generated on load.
    pub fn parse(text: &str, decoy_key: DecoySecret) -> Result<Self, ScramError> {
        if text.len() > MAX_CREDENTIAL_BYTES {
            return Err(ScramError::OutOfBounds);
        }
        let mut entries = BTreeMap::new();
        for line in text.lines() {
            if line.is_empty() {
                continue;
            }
            let (name, encoded) = line.split_once('\t').ok_or(ScramError::Malformed)?;
            if name.is_empty()
                || name.len() > 63
                || name.chars().any(char::is_control)
                || entries.len() == MAX_ACCOUNTS
            {
                return Err(ScramError::OutOfBounds);
            }
            let encoded = encoded
                .strip_prefix("SCRAM-SHA-256$4096:")
                .ok_or(ScramError::Malformed)?;
            let (salt, keys) = encoded.split_once('$').ok_or(ScramError::Malformed)?;
            let (stored, server) = keys.split_once(':').ok_or(ScramError::Malformed)?;
            let salt = STANDARD.decode(salt).map_err(|_| ScramError::Malformed)?;
            if salt.len() != 16 {
                return Err(ScramError::OutOfBounds);
            }
            let stored = decode_key(stored)?;
            let server = decode_key(server)?;
            let verifier = Verifier::from_keys(&salt, 4096, *stored, *server)?;
            if entries.insert(name.to_string(), verifier).is_some() {
                return Err(ScramError::Malformed);
            }
        }
        if entries.is_empty() {
            return Err(ScramError::Malformed);
        }
        Ok(Self { entries, decoy_key })
    }

    /// Selects only the startup account; SASL's client-first name never selects credentials.
    pub fn exchange(
        &self,
        name: &str,
        rng: &mut (impl Rng + ?Sized),
    ) -> Result<(ScramServer, bool), ScramError> {
        if name.is_empty() || name.len() > 63 || name.chars().any(char::is_control) {
            return Err(ScramError::Malformed);
        }
        let known = self.entries.get(name);
        // Perform the synthetic work for known users too. Lookup still has public-data branches;
        // equal message shape is not a constant-time/timing-audit claim.
        let salt = self.derive(b"salt", name)?;
        let stored = self.derive(b"stored", name)?;
        let server = self.derive(b"server", name)?;
        let decoy = Verifier::from_keys(
            salt.get(..16).ok_or(ScramError::Crypto)?,
            4096,
            *stored,
            *server,
        )?;
        Ok((
            ScramServer::new(known.cloned().unwrap_or(decoy), rng),
            known.is_some(),
        ))
    }
    fn derive(&self, label: &[u8], name: &str) -> Result<Zeroizing<[u8; 32]>, ScramError> {
        let mut mac = Hmac::<Sha256>::new_from_slice(self.decoy_key.0.as_ref())
            .map_err(|_| ScramError::Crypto)?;
        mac.update(label);
        mac.update(&[0]);
        mac.update(name.as_bytes());
        Ok(Zeroizing::new(mac.finalize().into_bytes().into()))
    }
}
fn decode_key(encoded: &str) -> Result<Zeroizing<[u8; 32]>, ScramError> {
    let mut key = Zeroizing::new([0; 32]);
    if STANDARD
        .decode_slice(encoded, key.as_mut())
        .map_err(|_| ScramError::Malformed)?
        != 32
    {
        return Err(ScramError::Malformed);
    }
    Ok(key)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;
    struct Counter(u64);
    impl Rng for Counter {
        fn next_u64(&mut self) -> u64 {
            self.0 += 1;
            self.0
        }
    }
    const CREDENTIAL: &str = "SCRAM-SHA-256$4096:W22ZaJ0SNY7soEsUEjb6gQ==$WG5d8oPm3OtcPnkdi4Uo7BkeZkBFzpcXkuLmtbsT4qY=:wfPLwcE6nTWhTAmQ7tl2KeoiWGPlZqQxSrmfPwDl2dU=";
    #[test]
    fn credentials_are_bounded_strict_and_redacted() {
        let line = format!("user\t{CREDENTIAL}");
        let accounts = Accounts::parse(&line, DecoySecret::from_bytes(&[7; 32]).unwrap()).unwrap();
        assert_eq!(format!("{accounts:?}"), "Accounts { count: 1, .. }");
        for invalid in [
            String::new(),
            format!("{line}\n{line}"),
            line.replace("4096:", "8192:"),
            line.replace("user", &"x".repeat(64)),
            line.replace("user", "bad\rname"),
            line.replace("SHA-256", "SHA-1"),
            "x".repeat(MAX_CREDENTIAL_BYTES + 1),
        ] {
            assert!(Accounts::parse(&invalid, DecoySecret::from_bytes(&[7; 32]).unwrap()).is_err());
        }
    }
    #[test]
    fn unknown_accounts_get_stable_distinct_name_keyed_profiles() {
        let accounts = Accounts::parse(
            &format!("user\t{CREDENTIAL}"),
            DecoySecret::from_bytes(&[7; 32]).unwrap(),
        )
        .unwrap();
        let challenge = |name| {
            let (mut exchange, known) = accounts.exchange(name, &mut Counter(10)).unwrap();
            (
                exchange
                    .initial(crate::scram::MECHANISM, b"n,,n=,r=clientnonce")
                    .unwrap(),
                known,
            )
        };
        let unknown = challenge("missing");
        assert!(!unknown.1);
        assert_eq!(unknown, challenge("missing"));
        assert_ne!(unknown.0, challenge("othermissing").0);
        assert_ne!(unknown.0, challenge("user").0);
        assert!(challenge("user").1);
        assert!(accounts.exchange("", &mut Counter(0)).is_err());
        assert!(accounts.exchange(&"x".repeat(64), &mut Counter(0)).is_err());
    }
    #[test]
    fn persistent_decoy_secret_is_required_bounded_and_redacted() {
        for size in [0, 31, 33, 1024] {
            assert!(DecoySecret::from_bytes(&vec![7; size]).is_err());
        }
        assert_eq!(
            format!("{:?}", DecoySecret::from_bytes(&[7; 32]).unwrap()),
            "DecoySecret([redacted])"
        );
    }

    #[test]
    fn persisted_secret_keeps_known_and_unknown_salts_stable_across_instances() {
        let line = format!("user\t{CREDENTIAL}");
        let secret: Vec<u8> = (0..32).collect();
        let first = Accounts::parse(&line, DecoySecret::from_bytes(&secret).unwrap()).unwrap();
        let reloaded = Accounts::parse(&line, DecoySecret::from_bytes(&secret).unwrap()).unwrap();
        let rotated = Accounts::parse(&line, DecoySecret::from_bytes(&[4; 32]).unwrap()).unwrap();
        let challenge = |accounts: &Accounts, user: &str, seed| {
            let (mut exchange, _) = accounts.exchange(user, &mut Counter(seed)).unwrap();
            let crate::pgwire::Backend::AuthenticationSaslContinue(bytes) = exchange
                .initial(crate::scram::MECHANISM, b"n,,n=,r=clientnonce")
                .unwrap()
            else {
                panic!("expected challenge");
            };
            let challenge = String::from_utf8(bytes).unwrap();
            let profile = challenge.split_once(',').unwrap().1.to_string();
            (challenge, profile)
        };
        // Independent Python stdlib HMAC-SHA-256 fixture for secret bytes0..31 and salt domain.
        for (user, expected) in [
            ("user", "s=W22ZaJ0SNY7soEsUEjb6gQ==,i=4096"),
            ("missing", "s=D1xH2aiGaEY3qHuaGlIiiw==,i=4096"),
        ] {
            let a = challenge(&first, user, 10);
            let b = challenge(&reloaded, user, 1000);
            assert_ne!(a.0, b.0); // Fresh nonce is independent from the persistent salt/key derivation.
            assert_eq!(a.1, expected);
            assert_eq!(b.1, expected);
            let c = challenge(&rotated, user, 2000);
            if user == "user" {
                assert_eq!(c.1, expected);
            } else {
                assert_ne!(c.1, expected);
            }
        }
    }
}

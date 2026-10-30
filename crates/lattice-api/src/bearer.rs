//! Bearer-token credentials for the HTTP and WebSocket front doors (ADR 0031, issue #521).
//!
//! A token is `lat_<id>_<secret>`: a fixed-width non-secret lookup id (16 lowercase hex digits)
//! and a 256-bit secret in unpadded base64url (43 characters), 64 bytes in all. The server
//! generates it from random bytes ([`issue`]); there is no way to import a token a person chose,
//! and the parser refuses any other length. The credential store keeps only a salted SHA-256
//! verifier of the secret. [`verify_bearer`] is pure: it takes the `Authorization` header value, the
//! parsed [`CredentialStore`] and the current time, and returns the [`Principal`] of the token's
//! account or an [`AuthError`]. It logs nothing; the host decides what to do with the error (count it
//! in a [`FailureLimiter`], write a security event).
//!
//! Unknown and wrong tokens run the same hash and the same comparison and give the same error. Shape
//! errors return before any hashing: the format is public. A token that is revoked or expired is
//! reported as such only after its secret verified, so the distinction leaks nothing to someone who
//! does not hold the secret. No token, secret, salt or verifier appears in `Debug`, `Display` or error
//! text. Secret arrays owned here are zeroized on drop; the caller's header bytes and the token string
//! handed out by [`issue`] are the caller's to protect. The comparison is constant time over the 32
//! verifier bytes (`subtle`); no end-to-end timing claim is made, and lookup by the public id is a
//! bounded `BTreeMap` search.

use crate::auth::valid_account_name;
use crate::principal::Principal;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use lattice_core::bugs::{self, Bug};
use lattice_seams::Rng;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, VecDeque};
use std::fmt::{self, Write as _};
use std::net::{IpAddr, Ipv6Addr};
use subtle::{Choice, ConstantTimeEq};
use zeroize::Zeroizing;

/// Every token starts with this.
pub const TOKEN_PREFIX: &str = "lat_";
/// Bytes of the non-secret lookup id.
pub const ID_BYTES: usize = 8;
/// Bytes of the per-token salt.
pub const SALT_BYTES: usize = 16;
/// Bytes of secret entropy in a token: the minimum and the only accepted length.
pub const SECRET_BYTES: usize = 32;
/// Length of every token in characters: prefix, 16 hex digits, `_`, 43 base64url characters.
pub const TOKEN_LEN: usize = 4 + 2 * ID_BYTES + 1 + 43;
/// Longest `Authorization` header value looked at, before any parsing.
pub const MAX_HEADER_BYTES: usize = 128;
/// First line of the credential file.
pub const STORE_HEADER: &str = "lattice-bearer-tokens v1";
/// Largest credential file, checked before parsing.
pub const MAX_STORE_BYTES: usize = 256 * 1024;
/// Most entries in a credential file, revoked ones included.
pub const MAX_ENTRIES: usize = 1024;
/// Most active tokens of one account: the current one and one rotation overlap.
pub const MAX_ACTIVE_PER_ACCOUNT: usize = 2;
/// Longest time the older of two active tokens of an account may outlive the newer one's creation.
pub const MAX_OVERLAP_SECS: u64 = 7 * 24 * 3600;

const HASH_DOMAIN: &[u8] = b"lattice-bearer-token-v1\0";
/// Used when the id is unknown, so that the same work runs and the comparison fails.
const DECOY_SALT: [u8; SALT_BYTES] = [0x5a; SALT_BYTES];
const DECOY_VERIFIER: [u8; 32] = [0xa5; 32];

/// The non-secret lookup key of a token.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct TokenId([u8; ID_BYTES]);
impl TokenId {
    /// Reads the 16 lowercase hex digits `Display` writes; anything else is `None`.
    pub fn parse(text: &str) -> Option<Self> {
        Self::from_hex(text.as_bytes())
    }
    fn from_hex(text: &[u8]) -> Option<Self> {
        let mut id = [0; ID_BYTES];
        decode_hex(text, &mut id)?;
        Some(Self(id))
    }
}
impl fmt::Display for TokenId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_hex(f, &self.0)
    }
}
impl fmt::Debug for TokenId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TokenId({self})")
    }
}

fn write_hex(f: &mut impl fmt::Write, bytes: &[u8]) -> fmt::Result {
    bytes.iter().try_for_each(|b| write!(f, "{b:02x}"))
}

/// Strict lowercase hex of exactly `out.len()` bytes.
fn decode_hex(text: &[u8], out: &mut [u8]) -> Option<()> {
    if text.len() != out.len() * 2 {
        return None;
    }
    let nibble = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    };
    for (slot, [high, low]) in out.iter_mut().zip(text.as_chunks::<2>().0) {
        *slot = nibble(*high)? << 4 | nibble(*low)?;
    }
    Some(())
}

/// A freshly issued token. The only copy of the secret: show it once and drop it.
pub struct BearerToken {
    id: TokenId,
    text: Zeroizing<String>,
}
impl BearerToken {
    /// The id, which is not secret.
    pub fn id(&self) -> TokenId {
        self.id
    }
    /// The full token text to hand to its owner. Never log it.
    pub fn expose_secret(&self) -> &str {
        &self.text
    }
}
impl fmt::Debug for BearerToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "BearerToken(id={}, secret=[redacted])", self.id)
    }
}
impl fmt::Display for BearerToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{TOKEN_PREFIX}{}_[redacted]", self.id)
    }
}

/// Whether a stored token can authenticate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenStatus {
    /// Authenticates until it expires.
    Active,
    /// Never authenticates again. The line stays so that its id is never reissued.
    Revoked,
}

/// One line of the credential file: everything about a token except its secret.
#[derive(Clone, PartialEq, Eq)]
pub struct TokenEntry {
    id: TokenId,
    account: String,
    status: TokenStatus,
    created: u64,
    expires: Option<u64>,
    salt: [u8; SALT_BYTES],
    verifier: [u8; 32],
}
impl TokenEntry {
    /// The token's lookup id.
    pub fn id(&self) -> TokenId {
        self.id
    }
    /// The account the token authenticates as (the SCRAM account namespace).
    pub fn account(&self) -> &str {
        &self.account
    }
    /// Active or revoked.
    pub fn status(&self) -> TokenStatus {
        self.status
    }
    /// Creation time, seconds since the Unix epoch.
    pub fn created(&self) -> u64 {
        self.created
    }
    /// Expiry, seconds since the Unix epoch, if the token has one.
    pub fn expires(&self) -> Option<u64> {
        self.expires
    }
}
impl fmt::Debug for TokenEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TokenEntry")
            .field("id", &self.id)
            .field("account", &self.account)
            .field("status", &self.status)
            .finish_non_exhaustive()
    }
}

/// Why a credential file or an issue request was refused. Carries a line number at most, never
/// file content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreError {
    /// More than [`MAX_STORE_BYTES`].
    TooLarge,
    /// More than [`MAX_ENTRIES`] entries.
    TooManyEntries,
    /// The first line is not [`STORE_HEADER`], or the file does not end in a newline.
    BadFraming,
    /// The given line (1-based) is not a well-formed entry.
    Malformed(usize),
    /// Two entries have the same id.
    DuplicateId,
    /// An account has more than [`MAX_ACTIVE_PER_ACCOUNT`] active tokens.
    TooManyActive,
    /// An account's older active token has no expiry, or outlives the newer one's creation by more
    /// than [`MAX_OVERLAP_SECS`].
    OverlapNotBounded,
    /// The account name breaks the account-name rule.
    InvalidAccount,
    /// The expiry is not after the creation time.
    InvalidExpiry,
}
impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge => f.write_str("credential file is too large"),
            Self::TooManyEntries => f.write_str("credential file has too many entries"),
            Self::BadFraming => f.write_str("credential file header or final newline is wrong"),
            Self::Malformed(line) => write!(f, "credential file line {line} is malformed"),
            Self::DuplicateId => f.write_str("credential file repeats a token id"),
            Self::TooManyActive => f.write_str("an account has too many active tokens"),
            Self::OverlapNotBounded => f.write_str("an account's token overlap is not bounded"),
            Self::InvalidAccount => f.write_str("invalid account name"),
            Self::InvalidExpiry => f.write_str("expiry is not after creation"),
        }
    }
}
impl std::error::Error for StoreError {}

/// The salted verifier of a secret: SHA-256 over the domain, salt, id and secret, all fixed width.
/// The secret has 256 bits of entropy, so a fast hash is enough; the salt keeps equal secrets and
/// equal files from sharing verifiers.
fn token_hash(
    salt: &[u8; SALT_BYTES],
    id: &TokenId,
    secret: &[u8; SECRET_BYTES],
) -> Zeroizing<[u8; 32]> {
    let mut hasher = Sha256::new();
    hasher.update(HASH_DOMAIN);
    hasher.update(salt);
    hasher.update(id.0);
    // Planted defect: only the first bytes of the secret reach the hash (a prefix is enough).
    let used = if bugs::active(Bug::TokenAcceptsPrefix) {
        secret.get(..8).unwrap_or(secret.as_slice())
    } else {
        secret.as_slice()
    };
    hasher.update(used);
    Zeroizing::new(hasher.finalize().into())
}

/// Creates a token and the file entry that verifies it.
///
/// `rng` must be the operating system's (`lattice_seams::OsRng`) outside tests: seven 64-bit draws
/// become an 8-byte id, a 16-byte salt and the 32-byte secret. `account` must satisfy the account
/// name rule shared with SCRAM, and `expires`, when given, must be after `created`.
pub fn issue(
    account: &str,
    created: u64,
    expires: Option<u64>,
    rng: &mut (impl Rng + ?Sized),
) -> Result<(BearerToken, TokenEntry), StoreError> {
    if !valid_account_name(account) {
        return Err(StoreError::InvalidAccount);
    }
    if expires.is_some_and(|e| e <= created) {
        return Err(StoreError::InvalidExpiry);
    }
    let mut draw = || rng.next_u64().to_le_bytes();
    let id = TokenId(draw());
    let mut salt = [0u8; SALT_BYTES];
    for chunk in salt.as_chunks_mut::<8>().0 {
        *chunk = draw();
    }
    let mut secret = Zeroizing::new([0u8; SECRET_BYTES]);
    for chunk in secret.as_chunks_mut::<8>().0 {
        *chunk = draw();
    }
    let verifier = token_hash(&salt, &id, &secret);
    let mut text = Zeroizing::new(String::with_capacity(TOKEN_LEN));
    text.push_str(TOKEN_PREFIX);
    let _ = write_hex(&mut *text, &id.0);
    text.push('_');
    URL_SAFE_NO_PAD.encode_string(secret.as_slice(), &mut text);
    let entry = TokenEntry {
        id,
        account: account.to_string(),
        status: TokenStatus::Active,
        created,
        expires,
        salt,
        verifier: *verifier,
    };
    Ok((BearerToken { id, text }, entry))
}

/// A token split into its two parts after the strict shape check.
struct Parsed {
    id: TokenId,
    secret: Zeroizing<[u8; SECRET_BYTES]>,
}
fn parse_token(text: &[u8]) -> Option<Parsed> {
    if text.len() != TOKEN_LEN {
        return None;
    }
    let rest = text.strip_prefix(TOKEN_PREFIX.as_bytes())?;
    let (id, rest) = rest.split_at_checked(2 * ID_BYTES)?;
    let encoded = rest.strip_prefix(b"_")?;
    let id = TokenId::from_hex(id)?;
    // The decoder wants room for a whole last group; canonical only, so the padding bits of the
    // last character must be zero.
    let mut buffer = Zeroizing::new([0u8; SECRET_BYTES + 4]);
    let n = URL_SAFE_NO_PAD
        .decode_slice(encoded, buffer.as_mut())
        .ok()?;
    if n != SECRET_BYTES {
        return None;
    }
    let mut secret = Zeroizing::new([0u8; SECRET_BYTES]);
    secret.copy_from_slice(buffer.get(..SECRET_BYTES)?);
    Some(Parsed { id, secret })
}

/// Why an authentication attempt failed. The host maps every variant to one HTTP 401 answer; the
/// distinctions are for its security event log and its failure counter only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthError {
    /// There was no `Authorization` header.
    Missing,
    /// The header is not `Bearer` followed by a token of the exact shape.
    Malformed,
    /// The token did not authenticate.
    Rejected(RejectReason),
}
/// Detail of [`AuthError::Rejected`] for the host's own log. Never send it to the client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectReason {
    /// The id is unknown or the secret is wrong. These two are the same value on purpose.
    Credential,
    /// The secret verified but the token is revoked.
    Revoked,
    /// The secret verified but the token has expired.
    Expired,
}
impl fmt::Display for AuthError {
    /// One text for every variant, so no message tells a caller which check failed.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("invalid bearer credentials")
    }
}
impl std::error::Error for AuthError {}

/// Authenticates an `Authorization` header value (`None` when the header is absent).
///
/// `header` must be the value of exactly one header: the host refuses repeated `Authorization`
/// headers itself. `now_secs` is the current time in seconds since the Unix epoch.
pub fn verify_bearer(
    header: Option<&[u8]>,
    store: &CredentialStore,
    now_secs: u64,
) -> Result<Principal, AuthError> {
    let header = header.ok_or(AuthError::Missing)?;
    if header.len() > MAX_HEADER_BYTES {
        return Err(AuthError::Malformed);
    }
    let (scheme, token) = header.split_at_checked(7).ok_or(AuthError::Malformed)?;
    if !scheme.eq_ignore_ascii_case(b"Bearer ") {
        return Err(AuthError::Malformed);
    }
    let parsed = parse_token(token).ok_or(AuthError::Malformed)?;
    let entry = store.entries.get(&parsed.id);
    let (salt, expected) = entry.map_or((&DECOY_SALT, &DECOY_VERIFIER), |e| (&e.salt, &e.verifier));
    let got = token_hash(salt, &parsed.id, &parsed.secret);
    let matched =
        got.as_slice().ct_eq(expected.as_slice()) & Choice::from(u8::from(entry.is_some()));
    match entry {
        Some(entry) if bool::from(matched) => match entry.status {
            // Planted defect: a revoked line authenticates like an active one.
            TokenStatus::Revoked if !bugs::active(Bug::RevokedTokenStillAccepted) => {
                Err(AuthError::Rejected(RejectReason::Revoked))
            }
            TokenStatus::Revoked => Ok(Principal::account(&entry.account)),
            TokenStatus::Active if entry.expires.is_some_and(|at| now_secs >= at) => {
                Err(AuthError::Rejected(RejectReason::Expired))
            }
            TokenStatus::Active => Ok(Principal::account(&entry.account)),
        },
        _ => Err(AuthError::Rejected(RejectReason::Credential)),
    }
}

/// The parsed credential file: every token's non-secret record, keyed by id.
#[derive(Clone, Default)]
pub struct CredentialStore {
    entries: BTreeMap<TokenId, TokenEntry>,
}
impl fmt::Debug for CredentialStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CredentialStore")
            .field("count", &self.entries.len())
            .finish_non_exhaustive()
    }
}

/// A strict canonical decimal: digits only, no sign, no leading zero.
fn decimal(text: &str) -> Option<u64> {
    let canonical = !text.is_empty()
        && text.bytes().all(|b| b.is_ascii_digit())
        && (text.len() == 1 || !text.starts_with('0'));
    if canonical { text.parse().ok() } else { None }
}

fn parse_entry(line: &str) -> Option<TokenEntry> {
    let mut fields = line.split('\t');
    let id = TokenId::from_hex(fields.next()?.as_bytes())?;
    let account = fields.next()?;
    let status = match fields.next()? {
        "active" => TokenStatus::Active,
        "revoked" => TokenStatus::Revoked,
        _ => return None,
    };
    let created = decimal(fields.next()?)?;
    let expires = match fields.next()? {
        "-" => None,
        text => Some(decimal(text)?),
    };
    let mut salt = [0; SALT_BYTES];
    decode_hex(fields.next()?.as_bytes(), &mut salt)?;
    let mut verifier = [0; 32];
    decode_hex(fields.next()?.as_bytes(), &mut verifier)?;
    if fields.next().is_some()
        || !valid_account_name(account)
        || expires.is_some_and(|e| e <= created)
    {
        return None;
    }
    Some(TokenEntry {
        id,
        account: account.to_string(),
        status,
        created,
        expires,
        salt,
        verifier,
    })
}

impl CredentialStore {
    /// A store without tokens: nothing authenticates.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Parses the credential file: [`STORE_HEADER`], then one line per token
    /// `id TAB account TAB active|revoked TAB created TAB expires|- TAB salt TAB verifier`, each line
    /// ending in a newline, in ascending id order. Strict and bounded: no blank lines, comments,
    /// carriage returns, trailing fields, repeated ids, out-of-order lines or unbounded rotation
    /// overlap (see [`StoreError`]). The accepted text is the one [`CredentialStore::render`] writes.
    pub fn parse(text: &str) -> Result<Self, StoreError> {
        if text.len() > MAX_STORE_BYTES {
            return Err(StoreError::TooLarge);
        }
        let body = text.strip_suffix('\n').ok_or(StoreError::BadFraming)?;
        let mut lines = body.split('\n');
        if lines.next() != Some(STORE_HEADER) {
            return Err(StoreError::BadFraming);
        }
        let mut entries = BTreeMap::new();
        for (index, line) in lines.enumerate() {
            if entries.len() == MAX_ENTRIES {
                return Err(StoreError::TooManyEntries);
            }
            let entry = parse_entry(line).ok_or(StoreError::Malformed(index + 2))?;
            // One canonical text: a line must sort after the one before it, as `render` writes them.
            if entries
                .keys()
                .next_back()
                .is_some_and(|last| *last > entry.id)
            {
                return Err(StoreError::Malformed(index + 2));
            }
            if entries.insert(entry.id, entry).is_some() {
                return Err(StoreError::DuplicateId);
            }
        }
        check_rotation(&entries)?;
        Ok(Self { entries })
    }

    /// The file text; `parse(render())` gives back the same store.
    pub fn render(&self) -> String {
        let mut out = format!("{STORE_HEADER}\n");
        for e in self.entries.values() {
            let status = match e.status {
                TokenStatus::Active => "active",
                TokenStatus::Revoked => "revoked",
            };
            let expires = e.expires.map_or_else(|| "-".to_string(), |t| t.to_string());
            let _ = write!(
                out,
                "{}\t{}\t{status}\t{}\t{expires}\t",
                e.id, e.account, e.created
            );
            let _ = write_hex(&mut out, &e.salt);
            out.push('\t');
            let _ = write_hex(&mut out, &e.verifier);
            out.push('\n');
        }
        out
    }

    /// Adds an entry, refusing a repeated id, a full file or a broken rotation rule. The store is
    /// unchanged on error.
    pub fn insert(&mut self, entry: TokenEntry) -> Result<(), StoreError> {
        if self.entries.contains_key(&entry.id) {
            return Err(StoreError::DuplicateId);
        }
        if self.entries.len() >= MAX_ENTRIES {
            return Err(StoreError::TooManyEntries);
        }
        let mut next = self.entries.clone();
        next.insert(entry.id, entry);
        check_rotation(&next)?;
        self.entries = next;
        Ok(())
    }

    /// The entry with `id`, if any.
    pub fn get(&self, id: TokenId) -> Option<&TokenEntry> {
        self.entries.get(&id)
    }

    /// Revokes the token `id` (the line stays, so the id is never reissued). Returns whether the
    /// id exists; revoking a revoked token is not an error.
    pub fn revoke(&mut self, id: TokenId) -> bool {
        match self.entries.get_mut(&id) {
            Some(entry) => {
                entry.status = TokenStatus::Revoked;
                true
            }
            None => false,
        }
    }

    /// Revokes every active token of `account` and returns how many it revoked.
    pub fn revoke_account(&mut self, account: &str) -> usize {
        // Planted defect: removing an account leaves its tokens working.
        if bugs::active(Bug::AccountRemoveKeepsTokens) {
            return 0;
        }
        let mut revoked = 0;
        for entry in self.entries.values_mut() {
            if entry.account == account && entry.status == TokenStatus::Active {
                entry.status = TokenStatus::Revoked;
                revoked += 1;
            }
        }
        revoked
    }

    /// Revokes every active token whose expiry has passed at `now_secs` and returns how many.
    /// Expired tokens never authenticate; this lets them leave the two-active-per-account count
    /// and become eligible for [`CredentialStore::prune`].
    pub fn revoke_expired(&mut self, now_secs: u64) -> usize {
        let mut revoked = 0;
        for entry in self.entries.values_mut() {
            if entry.status == TokenStatus::Active && entry.expires.is_some_and(|e| e <= now_secs) {
                entry.status = TokenStatus::Revoked;
                revoked += 1;
            }
        }
        revoked
    }

    /// Removes entries that can no longer authenticate (revoked, or expired by `now_secs`) and
    /// whose reference time is at least `older_than_secs` old, and returns how many. The reference
    /// time is the expiry when it has passed, else the creation time. A removed id could in theory
    /// be issued again (64 random bits), so prune only what the log no longer needs.
    pub fn prune(&mut self, now_secs: u64, older_than_secs: u64) -> usize {
        let before = self.entries.len();
        self.entries.retain(|_, e| {
            let expired = e.expires.is_some_and(|at| at <= now_secs);
            if e.status == TokenStatus::Active && !expired {
                return true;
            }
            let reference = e.expires.filter(|at| *at <= now_secs).unwrap_or(e.created);
            reference.saturating_add(older_than_secs) > now_secs
        });
        before - self.entries.len()
    }

    /// Sets the expiry of token `id`, the way a rotation ends the older token. The expiry must be
    /// after the creation time and keep the rotation rule; the store is unchanged on error.
    pub fn set_expiry(&mut self, id: TokenId, expires: u64) -> Result<(), StoreError> {
        let mut next = self.entries.clone();
        let entry = next.get_mut(&id).ok_or(StoreError::Malformed(0))?;
        if expires <= entry.created {
            return Err(StoreError::InvalidExpiry);
        }
        entry.expires = Some(expires);
        check_rotation(&next)?;
        self.entries = next;
        Ok(())
    }

    /// Number of entries, revoked ones included.
    pub fn len(&self) -> usize {
        self.entries.len()
    }
    /// Whether there are no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
    /// The entries in id order.
    pub fn entries(&self) -> impl Iterator<Item = &TokenEntry> {
        self.entries.values()
    }
}

/// At most two active tokens per account, and when there are two the older one expires within
/// [`MAX_OVERLAP_SECS`] of the newer one's creation.
fn check_rotation(entries: &BTreeMap<TokenId, TokenEntry>) -> Result<(), StoreError> {
    let mut active: BTreeMap<&str, Vec<&TokenEntry>> = BTreeMap::new();
    for e in entries.values().filter(|e| e.status == TokenStatus::Active) {
        active.entry(e.account.as_str()).or_default().push(e);
    }
    for mut tokens in active.into_values() {
        if tokens.len() > MAX_ACTIVE_PER_ACCOUNT {
            return Err(StoreError::TooManyActive);
        }
        tokens.sort_by_key(|e| (e.created, e.id));
        if let [older, newer] = tokens.as_slice() {
            let bounded = older
                .expires
                .is_some_and(|at| at <= newer.created.saturating_add(MAX_OVERLAP_SECS));
            if !bounded {
                return Err(StoreError::OverlapNotBounded);
            }
        }
    }
    Ok(())
}

/// Counts failed attempts per client address in a sliding window, so a host can stop verifying for an
/// address that keeps failing. Pure: the caller passes the time in milliseconds. IPv6 clients are
/// keyed by /64 and IPv4-mapped addresses as IPv4, like the handshake limiter of `latticed`.
///
/// This is a resource bound, not what protects a token (a guess needs 256 bits). Memory is bounded
/// by [`FailureLimiter::MAX_TRACKED`] addresses of at most `limit` timestamps each. When the table is
/// full of addresses with live failures a new address is not tracked: failing open here costs only
/// an uncounted failure, while failing closed would let an attacker lock every real client out.
#[derive(Debug)]
pub struct FailureLimiter {
    window_ms: u64,
    limit: usize,
    failures: BTreeMap<IpAddr, VecDeque<u64>>,
}
impl FailureLimiter {
    /// Addresses tracked at once.
    pub const MAX_TRACKED: usize = 1024;

    /// At most `limit` failures per address within `window_ms` before [`Self::is_blocked`].
    pub fn new(limit: usize, window_ms: u64) -> Self {
        Self {
            window_ms,
            limit,
            failures: BTreeMap::new(),
        }
    }

    fn key(address: IpAddr) -> IpAddr {
        match address.to_canonical() {
            IpAddr::V6(v6) => IpAddr::V6(Ipv6Addr::from(
                u128::from(v6) & 0xffff_ffff_ffff_ffff_0000_0000_0000_0000,
            )),
            v4 => v4,
        }
    }

    fn expire(window_ms: u64, times: &mut VecDeque<u64>, now_ms: u64) {
        while times
            .front()
            .is_some_and(|t| now_ms.saturating_sub(*t) >= window_ms)
        {
            times.pop_front();
        }
    }

    /// Whether the address has used up its failures; check it before verifying a token.
    pub fn is_blocked(&mut self, address: IpAddr, now_ms: u64) -> bool {
        let key = Self::key(address);
        let window_ms = self.window_ms;
        let Some(times) = self.failures.get_mut(&key) else {
            return false;
        };
        Self::expire(window_ms, times, now_ms);
        if times.is_empty() {
            self.failures.remove(&key);
            return false;
        }
        times.len() >= self.limit
    }

    /// Counts one failed attempt. An address that is already at its limit adds nothing, so being
    /// refused does not extend its own block.
    pub fn record_failure(&mut self, address: IpAddr, now_ms: u64) {
        let key = Self::key(address);
        let window_ms = self.window_ms;
        if !self.failures.contains_key(&key) && self.failures.len() >= Self::MAX_TRACKED {
            self.failures.retain(|_, times| {
                Self::expire(window_ms, times, now_ms);
                !times.is_empty()
            });
            if self.failures.len() >= Self::MAX_TRACKED {
                return;
            }
        }
        let times = self.failures.entry(key).or_default();
        Self::expire(window_ms, times, now_ms);
        if times.len() < self.limit {
            times.push_back(now_ms);
        }
    }

    /// Forgets the address after a success.
    pub fn record_success(&mut self, address: IpAddr) {
        self.failures.remove(&Self::key(address));
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
    use super::*;
    use lattice_seams::Rng;
    use proptest::prelude::*;

    // Hand-written vectors, produced outside this crate with Python's hashlib and base64:
    //   id = 00..07, salt = 10..1f, secret = a0..bf,
    //   verifier = SHA256("lattice-bearer-token-v1\0" || salt || id || secret).
    const TOKEN: &str = "lat_0001020304050607_oKGio6SlpqeoqaqrrK2ur7CxsrO0tba3uLm6u7y9vr8";
    const LINE: &str = "0001020304050607\talice\tactive\t1000\t-\t101112131415161718191a1b1c1d1e1f\t86e09220183af88b9f685b7a67f89a8099a88993781235e02fd4e2f6fa9dc589";
    // Second vector: id ff.., salt 77.., secret 00..1f.
    const TOKEN_2: &str = "lat_ffffffffffffffff_AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8";
    const LINE_2: &str = "ffffffffffffffff\tbob\tactive\t2000\t3000\t77777777777777777777777777777777\t5f9bda023e66383e1d8bec0de0f3eef30fb6764f3ed608622bb3611a3ddf4681";
    // What the planted defect stores for the first vector: the hash of the first eight secret
    // bytes only. SIBLING has those eight bytes, then zeros.
    const PREFIX_LINE: &str = "0001020304050607\talice\tactive\t1000\t-\t101112131415161718191a1b1c1d1e1f\t365fb06c87cd321849de0021c70222789fb867fc2b39e775108e723db2af3eb5";
    const PREFIX_SIBLING: &str = "lat_0001020304050607_oKGio6SlpqcAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

    fn store(lines: &[&str]) -> CredentialStore {
        CredentialStore::parse(&format!("{STORE_HEADER}\n{}\n", lines.join("\n"))).unwrap()
    }
    fn header(token: &str) -> Vec<u8> {
        format!("Bearer {token}").into_bytes()
    }
    struct Counter(u64);
    impl Rng for Counter {
        fn next_u64(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.0
        }
    }

    #[test]
    fn token_verifier_accepts_only_the_stored_token() {
        let store = store(&[LINE, LINE_2]);
        assert_eq!(
            verify_bearer(Some(&header(TOKEN)), &store, 1500).unwrap(),
            Principal::account("alice")
        );
        assert_eq!(
            verify_bearer(Some(&header(TOKEN_2)), &store, 1500).unwrap(),
            Principal::account("bob")
        );
        // The scheme is case-insensitive (RFC 7235); the token is not.
        let mut lower = b"bEaReR ".to_vec();
        lower.extend_from_slice(TOKEN.as_bytes());
        assert!(verify_bearer(Some(&lower), &store, 1500).is_ok());
        // Changing any one character after the prefix is refused, in the id and in the secret.
        for at in 4..TOKEN.len() {
            let mut bytes = TOKEN.as_bytes().to_vec();
            bytes[at] = if bytes[at] == b'0' { b'1' } else { b'0' };
            let mut h = b"Bearer ".to_vec();
            h.extend_from_slice(&bytes);
            let got = verify_bearer(Some(&h), &store, 1500);
            assert!(
                matches!(got, Err(AuthError::Rejected(_) | AuthError::Malformed)),
                "{at}"
            );
        }
        // Alice's secret under bob's id.
        let swapped = format!("lat_ffffffffffffffff_{}", &TOKEN[21..]);
        assert_eq!(
            verify_bearer(Some(&header(&swapped)), &store, 1500),
            Err(AuthError::Rejected(RejectReason::Credential))
        );
        // The expiry second itself is already expired.
        assert!(verify_bearer(Some(&header(TOKEN_2)), &store, 2999).is_ok());
        assert_eq!(
            verify_bearer(Some(&header(TOKEN_2)), &store, 3000),
            Err(AuthError::Rejected(RejectReason::Expired))
        );
    }

    #[test]
    fn unknown_account_and_wrong_token_take_the_same_path_and_message() {
        let store = store(&[LINE]);
        // Known id, wrong secret.
        let wrong = format!("lat_0001020304050607_{}", &TOKEN_2[21..]);
        // Unknown id, well-formed secret.
        let unknown = format!("lat_0101010101010101_{}", &TOKEN[21..]);
        let a = verify_bearer(Some(&header(&wrong)), &store, 0).unwrap_err();
        let b = verify_bearer(Some(&header(&unknown)), &store, 0).unwrap_err();
        assert_eq!(a, b);
        assert_eq!(a.to_string(), b.to_string());
        assert_eq!(format!("{a:?}"), format!("{b:?}"));
        // The empty store behaves the same.
        let c = verify_bearer(Some(&header(&wrong)), &CredentialStore::empty(), 0).unwrap_err();
        assert_eq!(a, c);
    }

    #[test]
    fn revoked_and_expired_are_told_apart_only_after_the_secret_verified() {
        let revoked = LINE.replace("active", "revoked");
        let store = store(&[&revoked, LINE_2]);
        assert_eq!(
            verify_bearer(Some(&header(TOKEN)), &store, 0),
            Err(AuthError::Rejected(RejectReason::Revoked))
        );
        // A wrong secret for a revoked id looks like any other wrong secret.
        let wrong = format!("lat_0001020304050607_{}", &TOKEN_2[21..]);
        assert_eq!(
            verify_bearer(Some(&header(&wrong)), &store, 0),
            Err(AuthError::Rejected(RejectReason::Credential))
        );
        assert_eq!(
            verify_bearer(Some(&header(TOKEN_2)), &store, u64::MAX),
            Err(AuthError::Rejected(RejectReason::Expired))
        );
    }

    #[test]
    fn missing_and_malformed_headers_are_refused_before_any_lookup() {
        let store = store(&[LINE]);
        assert_eq!(verify_bearer(None, &store, 0), Err(AuthError::Missing));
        let t = TOKEN;
        let cases: Vec<Vec<u8>> = vec![
            vec![],
            b"Bearer ".to_vec(),
            format!("Bearer{t}").into_bytes(),
            format!("Basic {t}").into_bytes(),
            format!("Bearer  {t}").into_bytes(),
            format!("Bearer {t} ").into_bytes(),
            format!("Bearer {t}\n").into_bytes(),
            format!(" Bearer {t}").into_bytes(),
            format!("Bearer {}", t.to_uppercase()).into_bytes(),
            format!("Bearer {}", &t[1..]).into_bytes(),
            format!("Bearer {t}A").into_bytes(),
            // Not canonical base64: the last character has non-zero padding bits.
            format!("Bearer {}", t.replace("vr8", "vr9")).into_bytes(),
            // Upper-case hex digit in the id.
            format!(
                "Bearer {}",
                t.replace("0001020304050607", "000102030405060A")
            )
            .into_bytes(),
            // Wrong separator, wrong prefix.
            format!("Bearer {}", t.replacen('_', "-", 2)).into_bytes(),
            format!("Bearer {}", t.replacen("lat_", "tal_", 1)).into_bytes(),
            vec![0xff; 71],
            vec![b'a'; MAX_HEADER_BYTES + 1],
        ];
        for case in cases {
            assert_eq!(
                verify_bearer(Some(&case), &store, 0),
                Err(AuthError::Malformed),
                "{case:?}"
            );
        }
    }

    #[test]
    fn token_shorter_than_32_bytes_of_entropy_is_refused() {
        let store = store(&[LINE]);
        // 31, 24, 16 and 8 byte secrets in the right frame, and a 33 byte one.
        for bytes in [31usize, 24, 16, 8, 33] {
            let secret = URL_SAFE_NO_PAD.encode(vec![0xa5; bytes]);
            let token = format!("lat_0001020304050607_{secret}");
            assert_eq!(
                verify_bearer(Some(&header(&token)), &store, 0),
                Err(AuthError::Malformed),
                "{bytes}"
            );
        }
        // A person-chosen word is not a token.
        assert_eq!(
            verify_bearer(Some(b"Bearer correct-horse-battery-staple"), &store, 0),
            Err(AuthError::Malformed)
        );
    }

    #[test]
    fn token_never_appears_in_debug_display_or_logs() {
        let (token, entry) = issue("alice", 10, None, &mut Counter(1)).unwrap();
        let secret = token.expose_secret().to_string();
        let tail = &secret[21..];
        let mut store = CredentialStore::empty();
        store.insert(entry.clone()).unwrap();
        let hex = |bytes: &[u8]| bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
        let (verifier, salt) = (hex(&entry.verifier), hex(&entry.salt));
        let mut texts = vec![
            format!("{token:?}"),
            format!("{token}"),
            format!("{entry:?}"),
            format!("{store:?}"),
            format!("{:?}", Principal::account("alice")),
            StoreError::Malformed(3).to_string(),
            format!("{:?}", verify_bearer(Some(&header(&secret)), &store, 0)),
        ];
        for e in [
            AuthError::Missing,
            AuthError::Malformed,
            AuthError::Rejected(RejectReason::Credential),
            AuthError::Rejected(RejectReason::Revoked),
            AuthError::Rejected(RejectReason::Expired),
        ] {
            texts.push(format!("{e:?}"));
            texts.push(e.to_string());
        }
        for text in texts {
            for needle in [tail, secret.as_str(), verifier.as_str(), salt.as_str()] {
                assert!(!text.contains(needle), "{text}");
            }
        }
        // The id is not secret and may be shown.
        assert_eq!(format!("{token}"), format!("lat_{}_[redacted]", token.id()));
    }

    #[test]
    fn issued_tokens_have_the_fixed_shape_and_survive_the_file() {
        let (token, entry) = issue("carol", 5, Some(100), &mut Counter(7)).unwrap();
        assert_eq!(token.expose_secret().len(), TOKEN_LEN);
        assert!(token.expose_secret().starts_with("lat_"));
        let mut s = CredentialStore::empty();
        s.insert(entry).unwrap();
        let s = CredentialStore::parse(&s.render()).unwrap();
        assert_eq!(
            verify_bearer(Some(&header(token.expose_secret())), &s, 99).unwrap(),
            Principal::account("carol")
        );
        let (other, _) = issue("carol", 5, Some(100), &mut Counter(8)).unwrap();
        assert_ne!(token.expose_secret(), other.expose_secret());
        assert!(verify_bearer(Some(&header(other.expose_secret())), &s, 99).is_err());
        for bad in ["", "tab\tname", &"x".repeat(64)] {
            assert_eq!(
                issue(bad, 1, None, &mut Counter(1)).unwrap_err(),
                StoreError::InvalidAccount
            );
        }
        assert_eq!(
            issue("a", 5, Some(5), &mut Counter(1)).unwrap_err(),
            StoreError::InvalidExpiry
        );
    }

    #[test]
    fn the_account_namespace_is_the_scram_namespace() {
        let file = |name: &str| format!("{STORE_HEADER}\n{}\n", LINE.replace("alice", name));
        for (name, ok) in [
            ("user", true),
            ("x", true),
            ("", false),
            ("bad\rname", false),
        ] {
            assert_eq!(valid_account_name(name), ok);
            assert_eq!(CredentialStore::parse(&file(name)).is_ok(), ok, "{name:?}");
        }
        assert!(CredentialStore::parse(&file(&"x".repeat(63))).is_ok());
        assert!(CredentialStore::parse(&file(&"x".repeat(64))).is_err());
    }

    #[test]
    fn the_file_is_strict_and_bounded() {
        let ok = format!("{STORE_HEADER}\n{LINE}\n");
        assert_eq!(CredentialStore::parse(&ok).unwrap().len(), 1);
        assert_eq!(
            CredentialStore::parse(&format!("{STORE_HEADER}\n"))
                .unwrap()
                .len(),
            0
        );
        let bad_framing = [
            String::new(),
            "\n".to_string(),
            STORE_HEADER.to_string(),
            format!("{STORE_HEADER}\n{LINE}"),
            format!("lattice-bearer-tokens v2\n{LINE}\n"),
            format!("{STORE_HEADER}\r\n{LINE}\n"),
        ];
        for text in bad_framing {
            assert_eq!(
                CredentialStore::parse(&text).unwrap_err(),
                StoreError::BadFraming,
                "{text:?}"
            );
        }
        let one = |line: String| format!("{STORE_HEADER}\n{line}\n");
        let malformed = [
            format!("{STORE_HEADER}\n\n"),
            one(format!("# {LINE}")),
            one(format!("{LINE}\textra")),
            one(LINE.replace("active", "Active")),
            one(LINE.replace("\t1000\t", "\t01000\t")),
            one(LINE.replace("\t1000\t", "\t+1000\t")),
            one(LINE.replace("\t1000\t", "\t99999999999999999999\t")),
            // Expiry equal to or before creation.
            one(LINE.replace("\t-\t", "\t1000\t")),
            one(LINE.replace("\t-\t", "\t999\t")),
            one(LINE.replace("0001020304050607", "000102030405060A")),
            one(LINE.replace("86e09", "86E09")),
            one(LINE.replace("1011", "10")),
            one(format!("{LINE}\r")),
        ];
        for text in malformed {
            assert!(
                matches!(CredentialStore::parse(&text), Err(StoreError::Malformed(2))),
                "{text:?}"
            );
        }
        assert_eq!(
            CredentialStore::parse(&format!("{STORE_HEADER}\n{LINE}\n{LINE}\n")).unwrap_err(),
            StoreError::DuplicateId
        );
        assert_eq!(
            CredentialStore::parse(&format!("{STORE_HEADER}\n{LINE}\n\n")).unwrap_err(),
            StoreError::Malformed(3)
        );
        // Out of id order is refused, so a parsed file always renders back to the same bytes.
        let lower = LINE
            .replace("0001020304050607", "0001020304050606")
            .replace("active", "revoked");
        assert_eq!(
            CredentialStore::parse(&format!("{STORE_HEADER}\n{LINE}\n{lower}\n")).unwrap_err(),
            StoreError::Malformed(3)
        );
        let sorted = format!("{STORE_HEADER}\n{lower}\n{LINE}\n");
        assert_eq!(CredentialStore::parse(&sorted).unwrap().render(), sorted);
        assert_eq!(
            CredentialStore::parse(&"x".repeat(MAX_STORE_BYTES + 1)).unwrap_err(),
            StoreError::TooLarge
        );
    }

    #[test]
    fn the_entry_count_and_file_size_bounds_hold_together() {
        // MAX_ENTRIES entries with the longest account names and the widest numbers fit in
        // MAX_STORE_BYTES; one more is refused by count.
        let name = "n".repeat(63);
        let line = |i: usize| {
            format!(
                "{i:016x}\t{name}\trevoked\t18446744073709551614\t18446744073709551615\t{}\t{}\n",
                "00".repeat(SALT_BYTES),
                "00".repeat(32)
            )
        };
        let mut text = format!("{STORE_HEADER}\n");
        for i in 0..MAX_ENTRIES {
            text.push_str(&line(i));
        }
        assert!(text.len() <= MAX_STORE_BYTES, "{}", text.len());
        assert_eq!(CredentialStore::parse(&text).unwrap().len(), MAX_ENTRIES);
        text.push_str(&line(MAX_ENTRIES));
        assert!(text.len() <= MAX_STORE_BYTES);
        assert_eq!(
            CredentialStore::parse(&text).unwrap_err(),
            StoreError::TooManyEntries
        );
    }

    #[test]
    fn rotation_allows_two_active_tokens_for_a_bounded_overlap() {
        let entry = |id: u8, created: u64, expires: &str, status: &str| {
            format!(
                "{id:02x}00000000000000\trota\t{status}\t{created}\t{expires}\t{}\t{}",
                "00".repeat(16),
                "11".repeat(32)
            )
        };
        let parse = |lines: &[String]| {
            CredentialStore::parse(&format!("{STORE_HEADER}\n{}\n", lines.join("\n")))
        };
        let week = MAX_OVERLAP_SECS;
        let at_limit = (100 + week).to_string();
        let past_limit = (100 + week + 1).to_string();
        // The older token expires exactly one overlap after the newer was created: allowed.
        assert!(
            parse(&[
                entry(1, 100, &at_limit, "active"),
                entry(2, 100, "-", "active")
            ])
            .is_ok()
        );
        // One second more is refused.
        for lines in [
            [
                entry(1, 100, &past_limit, "active"),
                entry(2, 100, "-", "active"),
            ],
            // The older token has no expiry at all.
            [entry(1, 50, "-", "active"), entry(2, 100, "-", "active")],
        ] {
            assert_eq!(parse(&lines).unwrap_err(), StoreError::OverlapNotBounded);
        }
        // The same lines out of id order are refused as such, before the overlap is looked at.
        assert_eq!(
            parse(&[
                entry(2, 100, "-", "active"),
                entry(1, 100, &past_limit, "active"),
            ])
            .unwrap_err(),
            StoreError::Malformed(3)
        );
        assert_eq!(
            parse(&[
                entry(1, 1, "10", "active"),
                entry(2, 2, "10", "active"),
                entry(3, 3, "-", "active")
            ])
            .unwrap_err(),
            StoreError::TooManyActive
        );
        // Revoked tokens do not count, however many there are.
        let many: Vec<String> = (1..=5).map(|i| entry(i, 100, "-", "revoked")).collect();
        assert!(parse(&many).is_ok());
        // Different accounts do not share the limit.
        let other = entry(3, 100, "-", "active").replace("rota", "rotb");
        assert!(
            parse(&[
                entry(1, 100, &at_limit, "active"),
                entry(2, 100, "-", "active"),
                other
            ])
            .is_ok()
        );
    }

    #[test]
    fn insert_enforces_the_same_rules_and_leaves_the_store_unchanged_on_error() {
        let mut s = CredentialStore::empty();
        let (_, first) = issue("zed", 100, None, &mut Counter(1)).unwrap();
        let (_, second) = issue("zed", 200, None, &mut Counter(2)).unwrap();
        s.insert(first.clone()).unwrap();
        assert_eq!(s.insert(first).unwrap_err(), StoreError::DuplicateId);
        // The older token has no expiry, so a second active one is not a bounded rotation.
        assert_eq!(s.insert(second).unwrap_err(), StoreError::OverlapNotBounded);
        assert_eq!(s.len(), 1);
        assert_eq!(s.entries().count(), 1);
        assert!(!s.is_empty());
    }

    #[test]
    fn a_prefix_of_the_secret_is_not_enough() {
        // The sibling shares the first eight secret bytes with the real token. A verifier that
        // hashed only those bytes (the planted defect) would accept it.
        let store = store(&[PREFIX_LINE]);
        for token in [PREFIX_SIBLING, TOKEN] {
            assert_eq!(
                verify_bearer(Some(&header(token)), &store, 0),
                Err(AuthError::Rejected(RejectReason::Credential))
            );
        }
        // Freshly issued tokens: a sibling with the same first bytes is refused too.
        let (token, entry) = issue("dan", 1, None, &mut Counter(3)).unwrap();
        let mut issued = CredentialStore::empty();
        issued.insert(entry).unwrap();
        assert!(verify_bearer(Some(&header(&sibling_of(&token))), &issued, 0).is_err());
    }

    /// Same id and first eight secret bytes (11 base64 characters), the rest replaced.
    fn sibling_of(token: &BearerToken) -> String {
        let text = token.expose_secret();
        format!("{}{}", &text[..21 + 11], "A".repeat(TOKEN_LEN - 21 - 11))
    }

    #[test]
    fn a_planted_prefix_match_is_caught_by_the_witness() {
        let prefix_store = store(&[PREFIX_LINE]);
        bugs::clear();
        bugs::enable(Bug::TokenAcceptsPrefix);
        let accepted = verify_bearer(Some(&header(PREFIX_SIBLING)), &prefix_store, 0);
        // The defect also makes freshly issued tokens match on a prefix.
        let (token, entry) = issue("dan", 1, None, &mut Counter(3)).unwrap();
        let mut issued = CredentialStore::empty();
        issued.insert(entry).unwrap();
        let sibling = sibling_of(&token);
        let sibling_accepted = verify_bearer(Some(&header(&sibling)), &issued, 0);
        bugs::clear();
        assert_eq!(accepted.unwrap(), Principal::account("alice"));
        assert_eq!(sibling_accepted.unwrap(), Principal::account("dan"));
        // Defect off again: the sibling is refused, so the defect is what let it in.
        assert!(verify_bearer(Some(&header(&sibling)), &issued, 0).is_err());
    }

    #[test]
    fn a_planted_revoked_acceptance_is_caught_by_the_witness() {
        let revoked = LINE.replace("active", "revoked");
        let store = store(&[&revoked]);
        bugs::clear();
        bugs::enable(Bug::RevokedTokenStillAccepted);
        let accepted = verify_bearer(Some(&header(TOKEN)), &store, 0);
        bugs::clear();
        assert_eq!(accepted.unwrap(), Principal::account("alice"));
        // Defect off again: the same revoked line is refused.
        assert_eq!(
            verify_bearer(Some(&header(TOKEN)), &store, 0),
            Err(AuthError::Rejected(RejectReason::Revoked))
        );
    }

    #[test]
    fn revoke_and_set_expiry_change_exactly_the_named_token() {
        // LINE is alice's token (created 1000, no expiry); LINE_2 is bob's (created 2000, expires 3000).
        let mut s = store(&[LINE, LINE_2]);
        let alice = TokenId::parse("0001020304050607").unwrap();
        let bob = TokenId::parse("ffffffffffffffff").unwrap();
        assert_eq!(TokenId::parse("FFFFFFFFFFFFFFFF"), None);
        assert_eq!(TokenId::parse("ff"), None);
        assert!(!s.revoke(TokenId::parse("1111111111111111").unwrap()));
        assert!(s.revoke(alice));
        assert!(s.revoke(alice), "revoking twice is not an error");
        assert_eq!(s.get(alice).unwrap().status(), TokenStatus::Revoked);
        assert_eq!(s.get(bob).unwrap().status(), TokenStatus::Active);
        assert!(matches!(
            verify_bearer(Some(&header(TOKEN)), &s, 0),
            Err(AuthError::Rejected(RejectReason::Revoked))
        ));
        assert!(verify_bearer(Some(&header(TOKEN_2)), &s, 0).is_ok());
        // An expiry must be after creation; the store is unchanged on error.
        assert_eq!(s.set_expiry(bob, 2000), Err(StoreError::InvalidExpiry));
        assert_eq!(s.get(bob).unwrap().expires(), Some(3000));
        s.set_expiry(bob, 2500).unwrap();
        assert_eq!(s.get(bob).unwrap().expires(), Some(2500));
        assert!(
            s.set_expiry(TokenId::parse("1111111111111111").unwrap(), 9)
                .is_err()
        );
    }

    #[test]
    fn revoke_account_revokes_every_active_token_of_that_account_only() {
        let mut s = store(&[LINE, LINE_2]);
        assert_eq!(s.revoke_account("carol"), 0);
        assert_eq!(s.revoke_account("alice"), 1);
        assert_eq!(s.revoke_account("alice"), 0, "already revoked");
        assert!(verify_bearer(Some(&header(TOKEN)), &s, 0).is_err());
        assert!(verify_bearer(Some(&header(TOKEN_2)), &s, 0).is_ok());
        // A planted defect that revokes nothing is caught by the assertions above.
        let mut kept = store(&[LINE]);
        bugs::clear();
        bugs::enable(Bug::AccountRemoveKeepsTokens);
        let revoked = kept.revoke_account("alice");
        bugs::clear();
        assert_eq!(revoked, 0);
        assert!(verify_bearer(Some(&header(TOKEN)), &kept, 0).is_ok());
    }

    #[test]
    fn revoke_expired_and_prune_remove_only_what_can_no_longer_authenticate() {
        // alice: created 1000, no expiry. bob: created 2000, expires 3000.
        let mut s = store(&[LINE, LINE_2]);
        assert_eq!(
            s.revoke_expired(2999),
            0,
            "bob is still valid one second before"
        );
        assert_eq!(s.revoke_expired(3000), 1, "expired at its expiry second");
        assert_eq!(
            s.get(TokenId::parse("ffffffffffffffff").unwrap())
                .unwrap()
                .status(),
            TokenStatus::Revoked
        );
        assert_eq!(s.revoke_expired(9999), 0);
        // Reference time of bob is his expiry (3000); alice is live and never pruned.
        assert_eq!(s.prune(3000 + 99, 100), 0);
        assert_eq!(s.prune(3000 + 100, 100), 1);
        assert_eq!(s.len(), 1);
        assert_eq!(
            s.prune(u64::MAX, 0),
            0,
            "a live token stays whatever the age"
        );
        // An expired but never revoked entry is dead too; a revoked one is aged by creation.
        let mut t = store(&[LINE, LINE_2]);
        t.revoke(TokenId::parse("0001020304050607").unwrap());
        assert_eq!(t.prune(1000 + 50, 100), 0);
        assert_eq!(t.prune(1000 + 100, 100), 1, "alice: revoked, created 1000");
        assert_eq!(t.prune(3000, 0), 1, "bob: expired, unrevoked");
        assert!(t.is_empty());
    }

    fn ip(n: u8) -> IpAddr {
        IpAddr::from([10, 0, 0, n])
    }

    #[test]
    fn failures_block_an_address_until_the_window_passes() {
        let mut l = FailureLimiter::new(3, 1000);
        for t in 0..3 {
            assert!(!l.is_blocked(ip(1), t));
            l.record_failure(ip(1), t);
        }
        assert!(l.is_blocked(ip(1), 3));
        assert!(!l.is_blocked(ip(2), 3));
        // Being refused does not extend the block: the failures at 0, 1 and 2 still age out.
        l.record_failure(ip(1), 500);
        assert!(l.is_blocked(ip(1), 999));
        assert!(!l.is_blocked(ip(1), 1002));
        l.record_failure(ip(1), 2000);
        l.record_success(ip(1));
        assert!(!l.is_blocked(ip(1), 2001));
    }

    #[test]
    fn limiter_keys_ipv6_by_prefix_and_mapped_v4_as_v4_and_stays_bounded() {
        let mut l = FailureLimiter::new(3, 1000);
        let a: IpAddr = "2001:db8:1:2::1".parse().unwrap();
        let b: IpAddr = "2001:db8:1:2:ffff::9".parse().unwrap();
        let c: IpAddr = "2001:db8:1:3::1".parse().unwrap();
        for _ in 0..3 {
            l.record_failure(a, 0);
        }
        assert!(l.is_blocked(b, 1));
        assert!(!l.is_blocked(c, 1));
        let mapped: IpAddr = "::ffff:10.0.0.1".parse().unwrap();
        for _ in 0..3 {
            l.record_failure(mapped, 0);
        }
        assert!(l.is_blocked(ip(1), 1));
        // The table never grows past its bound, and expired addresses make room.
        let mut l = FailureLimiter::new(3, 1000);
        for i in 0..(FailureLimiter::MAX_TRACKED + 50) {
            l.record_failure(IpAddr::from([172, 16, (i / 256) as u8, (i % 256) as u8]), 0);
        }
        assert_eq!(l.failures.len(), FailureLimiter::MAX_TRACKED);
        l.record_failure(ip(9), 5000);
        assert!(l.failures.len() <= FailureLimiter::MAX_TRACKED);
        assert!(l.failures.contains_key(&ip(9)));
    }

    proptest! {
        #[test]
        fn arbitrary_headers_never_panic_and_never_authenticate(
            header in proptest::collection::vec(any::<u8>(), 0..200),
            now in any::<u64>(),
        ) {
            let s = store(&[LINE, LINE_2]);
            prop_assert!(verify_bearer(Some(&header), &s, now).is_err());
            let _ = CredentialStore::parse(&String::from_utf8_lossy(&header));
        }

        #[test]
        fn well_shaped_wrong_tokens_are_rejected_like_unknown_ones(
            id in proptest::array::uniform8(any::<u8>()),
            secret in proptest::array::uniform32(any::<u8>()),
        ) {
            let s = store(&[LINE]);
            let token = format!("lat_{}_{}", TokenId(id), URL_SAFE_NO_PAD.encode(secret));
            prop_assert_eq!(
                verify_bearer(Some(&header(&token)), &s, 0),
                Err(AuthError::Rejected(RejectReason::Credential))
            );
        }

        #[test]
        fn issued_tokens_always_have_the_shape_and_verify(seed in any::<u64>(), account in "[a-z]{1,20}") {
            let (token, entry) = issue(&account, 1, None, &mut Counter(seed)).unwrap();
            prop_assert_eq!(token.expose_secret().len(), TOKEN_LEN);
            let mut s = CredentialStore::empty();
            s.insert(entry).unwrap();
            let again = CredentialStore::parse(&s.render()).unwrap();
            prop_assert_eq!(again.render(), s.render());
            prop_assert_eq!(
                verify_bearer(Some(&header(token.expose_secret())), &again, 0).unwrap(),
                Principal::account(&account)
            );
        }

        #[test]
        fn any_text_parses_or_fails_without_panicking_and_a_parsed_store_round_trips(text in "\\PC{0,300}") {
            if let Ok(s) = CredentialStore::parse(&text) {
                prop_assert_eq!(s.render(), text);
            }
        }
    }
}

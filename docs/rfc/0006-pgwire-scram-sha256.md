# RFC 0006: PostgreSQL SCRAM-SHA-256 authentication primitives

Status: draft
Comment period ends: 2026-11-05
Decision ADR: proposed ADR 0020

## Motivation

P2-11 requires a PostgreSQL authentication hook. The existing session and development TCP listener
use trust authentication. This proposal adds a pure SCRAM-SHA-256 server exchange and the standard
PostgreSQL SASL messages without silently changing that trust-only session or claiming deployment
security. The draft needs independent review and at least seven full days of comment time after its
PR opens; the calendar date above does not shorten that interval. PR 204 opened at
2026-10-29 12:57:08 UTC, so the earliest acceptance time is **2026-11-05 12:57:08 UTC**, after review.
Explicit acceptance remains required.

## Proposal

Implement the non-channel-binding mechanism defined by [RFC 5802](https://www.rfc-editor.org/rfc/rfc5802)
and [RFC 7677](https://www.rfc-editor.org/rfc/rfc7677), using maintained SHA-256, HMAC, PBKDF2, Base64
and constant-time comparison libraries. Server nonce generation takes the existing RNG seam;
production callers must supply fresh cryptographically secure output. A verifier contains a salt,
iteration count, stored key and server key. Provisioning accepts already-prepared password bytes;
it does not implement SASLprep or PostgreSQL's raw-password fallback.

The server advertises only SCRAM-SHA-256. It validates client-first syntax and returns the combined
nonce, salt and configured iteration count, then validates the echoed GS2 header, exact combined
nonce and client proof before emitting the server signature. Authentication succeeds only after
that proof. All exchange failures are terminal. Unknown mandatory extensions, authorization
identities and channel binding are refused. Optional attributes remain in the authenticated raw
transcript; duplicates, misplaced fields, invalid escapes and noncanonical Base64 fail.

Public bounds are 8192 bytes per authentication input, 256 bytes per client nonce/server suffix,
32 attributes, 16 to 64 salt bytes, 1024 prepared-password bytes, and 4096 to 1,000,000 provisioning
iterations. Network input never triggers PBKDF2. Verifier and explicit temporary key arrays are
zeroized on drop; upstream HMAC/SHA-256 zeroization is enabled. This does not guarantee removal of
caller-owned buffers or every compiler-generated copy. Debug/errors do not include verifier keys.

Use PostgreSQL's existing authentication codes 10/11/12 for AuthenticationSASL, SASLContinue and
SASLFinal. The frontend tag `p` is ambiguous without authentication state: ordinary decoding keeps
returning opaque Password messages, while an explicit SASL-phase decoder selects initial-response
or response layout. It validates lengths before copying. Add enum variants only at their ends.
The current trust-only session refuses these variants just as it refuses unexpected passwords.

## Alternatives

Cleartext passwords depend entirely on transport protection and do not meet the requested challenge
response behavior. A private authentication protocol would sacrifice PostgreSQL driver compatibility.
A complete authentication/session/TLS integration in this patch would widen its review and file scope.
SCRAM-SHA-256-PLUS is deferred until the host can supply a verified TLS channel binding; advertising
PLUS without one would misrepresent the connection's protection.

## Compatibility and migration

This implements established PostgreSQL version 3 message layouts, not a new private wire format.
Existing startup, query and opaque Password decoding remain unchanged. No persisted format changes.
The current TCP listener remains explicitly trust-only. A later host integration must select the
verifier using the startup user name, define unknown-user behavior without an account-existence
oracle, provide transport protection, rate limits and handshake timeouts, and send AuthenticationOk
only after successful proof. It must not fall back to trust after a failed SCRAM exchange.

## Verification and remaining review

Tests compare exact wire bytes for the three backend authentication messages and strict initial
response lengths; RFC 7677 supplies the complete published SHA-256 exchange. RFC 5802 grammar cases,
an independently derived extension transcript, nonce-seam draws, every proof-byte mutation, malformed
and truncated messages, terminal failure and framed arbitrary-input properties cover the pure module.
RFC 5802's SHA-1 transcript is not presented as a SHA-256 known-answer vector.

Independent authentication/codec review, hosted checks and comment-period acceptance remain required.
No external driver authentication, TLS, account lookup, timing audit, simulator planted-bug witness
or phase-gate acceptance is established by these unit/property tests. The Phase 1 gate remains open;
this proposal proceeds under the existing Phase 2 start waiver.

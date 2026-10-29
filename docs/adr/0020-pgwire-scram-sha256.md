# ADR 0020: PostgreSQL SCRAM-SHA-256 primitives with explicit authentication phases

Status: accepted

## Context

P2-11 requires PostgreSQL-compatible authentication. The existing session/TCP development listener
uses trust, and tag `p` carries multiple authentication layouts depending on connection state.
[RFC 0006](../rfc/0006-pgwire-scram-sha256.md) defines a pure, bounded SCRAM-SHA-256 exchange
and standard SASL message support. Independent review found a libpq empty-name interoperability gap,
which now has an independently derived regression fixture. On 2026-10-29 the owner explicitly waived
the seven-day merge wait and approved the reviewed extension. This decision does not approve
production authentication deployment or sign a phase gate.

## Decision

Use maintained SHA-256/HMAC/PBKDF2/Base64 primitives and fixed-size constant-time proof comparison,
with explicit verifier/password-preparation and RNG boundaries. Preserve ordinary opaque Password
decoding; require callers to select the SASL decoding phase rather than guessing from payload bytes.
Support the standard PostgreSQL AuthenticationSASL, SASLContinue and SASLFinal messages. Keep session
and host authentication wiring separate; the current trust session refuses unexpected SASL messages.
Select the verifier from the startup account and accept libpq's empty client-first `n=` as a
PostgreSQL-specific exception, retaining those exact bytes in the proof transcript. Other empty
attribute values remain invalid; nonempty names still use strict SASL escaping.

Bound message sizes, nonce parts, attributes, salt/password inputs and provisioning iteration counts.
Keep PBKDF2 off the network message path, fail exchanges terminally, redact verifier debug output and
zeroize owned key storage. Reject PLUS/channel binding until actual TLS integration can supply it.

## Consequences

The module is deterministic for explicit inputs and can be tested against the RFC 7677 SHA-256
exchange, independent byte/transcript fixtures and malformed-input properties. It adds pinned crypto
and encoding dependencies to the API layer; their licenses/advisories and integration behavior need
hosted checks and independent review. No runtime, thread pool, real randomness or new storage format
enters deterministic engine layers.

Future integration must define prepared-password provisioning, account lookup and unknown-user
behavior, transport protection, rate/timeout bounds, authentication state transitions and driver
conformance. These primitives do not implement those boundaries, change the current listener to
password authentication, establish measured constant-time behavior on every target or pass a phase gate.

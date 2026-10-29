# ADR 0020: PostgreSQL SCRAM-SHA-256 primitives with explicit authentication phases

Status: proposed

## Context

P2-11 requires PostgreSQL-compatible authentication. The existing session/TCP development listener
uses trust, and tag `p` carries multiple authentication layouts depending on connection state.
[Draft RFC 0006](../rfc/0006-pgwire-scram-sha256.md) proposes a pure, bounded SCRAM-SHA-256 exchange
and standard SASL message support. It remains subject to independent review, the full comment period
and explicit acceptance. This ADR does not approve a format or production authentication deployment.

## Decision

Propose maintained SHA-256/HMAC/PBKDF2/Base64 primitives and fixed-size constant-time proof comparison,
with explicit verifier/password-preparation and RNG boundaries. Preserve ordinary opaque Password
decoding; require callers to select the SASL decoding phase rather than guessing from payload bytes.
Support the standard PostgreSQL AuthenticationSASL, SASLContinue and SASLFinal messages. Keep session
and host authentication wiring separate; the current trust session refuses unexpected SASL messages.

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

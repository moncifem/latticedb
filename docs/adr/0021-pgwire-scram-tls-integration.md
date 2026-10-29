# ADR 0021: Require TLS for configured SCRAM sessions

Status: proposed

## Context

Accepted ADR 0020 covers primitives only. The TCP adapter needs transport protection, verifier lookup,
unknown-user behavior and bounded authentication lifecycle. RFC 0007 proposes those integration rules;
its seven-day comment period and independent review are pending, with no inherited waiver.

## Decision

Propose rustls TLS 1.2/1.3 via standard PostgreSQL SSLRequest upgrade before SCRAM. Reject plaintext
SQL startup in SCRAM mode and restrict development trust to loopback. Configure certificates/keys and
bounded pre-provisioned verifier files before binding. Use startup account selection, a uniform public
cost profile and keyed per-name synthetic credentials from a mandatory persistent provisioned 32-byte
decoy secret for unknown accounts; fail terminally without
trust fallback. Complete proof before exposing successful session metadata or executing any query.

Enforce one accept-to-auth deadline at underlying socket I/O, an 8192-byte authentication-body bound,
the existing worker limits and a global 64-per-second TLS-start window. Keep standard cancellation
routing and shutdown cleanup. No client certificates, channel binding, early data or direct TLS startup.

## Consequences

This adds host TLS dependencies and changes remote development-trust startup behavior. A uniform
4096-iteration/16-byte-salt provisioning profile limits credential imports but avoids distinct public
profiles for fake accounts. It does not establish measured timing resistance, a complete abuse-defense
policy or the driver matrix. Reusing the same decoy secret preserves unknown salts across reloads and
restarts. Secret rotation changes those salts while unchanged known salts stay stable, so rotation
can reveal account existence to observers across that event; no unobservable-rotation claim is made. Clients must verify certificates/server names; operators manage file
permissions and certificate rotation. Review and policy acceptance remain prerequisites to merging
this wire/security integration; this ADR is not accepted and does not sign a phase gate.

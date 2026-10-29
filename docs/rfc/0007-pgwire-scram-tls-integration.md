# RFC 0007: PostgreSQL SCRAM sessions over required TLS

Status: draft
Comment period ends: 2026-11-05
Decision ADR: proposed ADR 0021

## Motivation

P2-11 requires actual authentication and TLS, beyond RFC 0006's accepted primitives. The development
listener currently accepts trust authentication. This proposal gates successful startup/query handling
on a configured SCRAM verifier and a protected TLS transport. It does not inherit RFC 0006's wait waiver.
Independent security review and the normal comment period remain required before acceptance/merge.

## Proposal

Use PostgreSQL's standard SSLRequest `S` upgrade followed by rustls TLS 1.2/1.3. Configure a bounded PEM
certificate chain/private key before binding. Clients must verify the certificate and intended server
name using their driver configuration; the server cannot enforce client certificate validation. No
client-certificate authentication, TLS early data, SCRAM-PLUS or channel binding is advertised. A
SCRAM-configured listener refuses plaintext SQL startup and never falls back to trust after failure.
The separate explicit development-trust mode remains plaintext and may bind only loopback addresses.

Select the verifier using exactly one nonempty startup user, never the client's SASL name. Provision
credentials externally as username, tab, PostgreSQL SCRAM verifier. Accept at most 1024 accounts and
256 KiB, names of at most 63 UTF-8 bytes without controls, canonical encoded 32-byte stored/server keys,
16-byte salts and 4096 iterations. Fixing the public cost profile prevents unknown accounts being
identified through different advertised work parameters. Provisioning/SASLprep are outside the listener;
network input never triggers PBKDF2. Credential-file buffers are zeroized on drop, without claiming
removal of compiler copies or upstream allocations.

For unknown users derive a synthetic salt and stored/server keys from a fresh per-process secret,
the startup name and distinct HMAC-SHA-256 domains. This gives stable per-name fake challenges within
a server instance instead of one recognizable shared fake account. Run synthetic derivation for known
users too; always refuse unknown users after proof even if their synthetic proof could be produced.
Known/unknown proof failures use the same terminal `28P01` text and do not expose credentials/transcripts.
This is a message/profile policy, not a measured constant-time or account-enumeration guarantee: map
lookup/branches and cross-restart salt stability remain possible side channels requiring review.

The sans-I/O session explicitly tracks SASL initial/final phases; the host chooses the phase-aware
password decoder. Do not emit AuthenticationOk, parameters, BackendKeyData or ReadyForQuery until a
valid known-account proof. Initial/final ordering violations and failed proof close the connection;
no retry or trust fallback is permitted. Successful auth sends SASLFinal before AuthenticationOk.

Apply one absolute deadline from TCP accept through initial startup, encryption requests, TLS handshake
and SCRAM. The underlying socket checks it before every rustls I/O, so internal retries or partial
records cannot reset the budget. Bound authentication bodies before allocating them to 8192 bytes,
retain existing SQL/transport worker caps, and allow at most 64 TLS starts in any rolling second per
listener. This global burst/rate limit is deliberately small and may deny legitimate bursts; it is
not a distributed/per-account abuse defense. Shutdown retains the registry admission latch and closes
underlying sockets to release partial TLS/auth workers before join.

CancelRequest remains a separate standard packet carrying the existing random key. It may arrive
without TLS for compatibility, has no reply and grants only cancellation; keys are sent only after
successful TLS/SCRAM startup. Its confidentiality still depends on the client's protected SQL session.
No custom wire layout or persisted credential format is introduced; existing PostgreSQL layouts apply.

## Alternatives

Plaintext SCRAM alone does not protect the query session or authenticate the server; it is refused.
Remote trust would bypass authentication and is refused. Requiring mutual TLS would add a separate
identity policy not needed for password authentication. SCRAM-PLUS needs actual channel binding and
is deferred. Uniform profile 4096 is the existing primitive minimum; increasing it requires a uniform
provisioning policy update, not attacker-selected per-message derivation.

## Compatibility and migration

Existing local development defaults keep SELECT 1 and explicit trust mode. Binding trust to a remote
address becomes a startup error. Production provisioning, certificate trust/rotation, secret-file
permissions and remote exposure configuration remain operator responsibilities. TLS/SCRAM tests do
not establish the five-driver matrix, SQL execution, timing audit, penetration testing or phase gates.
This proposal stays draft until independent review and policy acceptance.

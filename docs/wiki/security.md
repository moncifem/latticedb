# Security model

Lattice is pre-alpha and has had no external review, audit or penetration test. Use it for test data only. This
page says what the alpha protects, what it does not, and where the full analysis is. The
[threat model](../threat-model.md) is a first draft that no second maintainer has reviewed.

## Report a vulnerability

Report it privately with GitHub's private vulnerability reporting:
[security advisories](https://github.com/moncifem/latticedb/security/advisories/new). Do not open a public issue
or discussion for it. The [security policy](../../SECURITY.md) gives the response targets. They are targets,
not a service level, because the project has one maintainer.

Supported versions are the newest alpha pre-release, once one is published, and `main`. Nightly builds are not
supported.

## What the alpha protects

**Connections**

- Development trust accepts every connection, and it is allowed only on loopback. The server refuses a
  non-loopback bind in that mode. The `--development-policy allow-all` flag must be given explicitly.
- SCRAM-SHA-256 over TLS is the mode for other hosts. The server refuses to start in that mode without TLS, and
  a password never crosses the wire in plaintext. Clients must verify the certificate with `sslmode=verify-full`.
- Verifiers use 4096 PBKDF2 iterations ([ADR 0021](../adr/0021-pgwire-scram-tls-integration.md)), so keep the
  verifier file owner-only. Unknown accounts get synthetic credentials and the same failure as a wrong password.

**Accounts and grants**

- Each account has table-level `read`, `write` and `ddl` grants, or `allow-all` or `deny-all`. An account that is
  not listed is denied.
- `lattice account` writes its files atomically with mode 0600, inside a directory only the owner may write.
  Bearer tokens are stored as salted hashes ([ADR 0031](../adr/0031-bearer-token-authentication.md)). Revoking a
  token or removing an account closes its live gateway connections.
- There are no row predicates, roles or policy language yet. Those are Phase 4 work.

**Gateway**

- The WebSocket gateway is plaintext on loopback only. Off loopback it needs TLS and tokens, and `latticed` refuses to start without them. Its token goes only in the `Authorization` header, and an
  address that fails 10 times in a minute gets `429`.

## What it does not protect

- **No TLS on the gateway.** Do not expose its port.
- **No channel binding** for SCRAM ([#360](https://github.com/moncifem/latticedb/issues/360)). A client that skips
  certificate verification can be relayed.
- **No security event log and no failed-login backoff** for the PostgreSQL listener
  ([#361](https://github.com/moncifem/latticedb/issues/361)).
- **A filtered change feed** reveals the count of hidden writes ([#363](https://github.com/moncifem/latticedb/issues/363)).
- **Feed reads are not fully bounded** ([#543](https://github.com/moncifem/latticedb/issues/543)).
- **No encryption at rest, identity service, policy compilation or audit chain.** These belong to Phases 4 and 5.
  The `chain.rs` hash-chain primitive exists, but nothing in the server uses it.
- **Stored text can carry prompt injection** into an agent that reads it. See [Agents and live collaboration](agents.md).
- **Container.** The image's base still ships `/bin/sh` and `apt`. A published port needs SCRAM over TLS.
- **Data and formats are not frozen.** A later alpha may not open this alpha's data directory.

For the list of open issues by area, see the [known issues](../alpha.md#known-issues) in the alpha guide. The
[listener guide](../../crates/latticed/README.md) has the exact limits of the listener, the gateway and the
account tool.

# Security policy

## Reporting a vulnerability

Report privately with GitHub's private vulnerability reporting:
<https://github.com/moncifem/latticedb/security/advisories/new>. You need a GitHub account; the page
asks you to sign in. Please do not open a public issue or discussion for a vulnerability. A dedicated
security mailbox will be added once the project has a domain.

Include a description, a proof of concept, the estimated impact and a suggested fix if you have one.
We acknowledge reports within three business days and aim to publish a fix and advisory within ninety
days. There is no bug bounty yet. The project has one maintainer today, so these are targets, not a
service level.

## Supported versions

| Version | Supported |
|---|---|
| Alpha pre-releases (`v0.1.0-alpha.N`, once published) and `main` | Fixes land on `main`; only the newest alpha is considered supported |
| Anything else, including nightly builds | No |

There is no stable release. The alpha is for test data only; do not use it for anything that needs
confidentiality or availability.

## What the alpha does and does not protect

The full analysis is the [threat model](docs/threat-model.md), a first draft that no second maintainer
has reviewed. No external review, audit or penetration test has been done. In short:

- The server speaks the PostgreSQL wire protocol. Loopback development trust accepts every local
  connection and must not be exposed. SCRAM-SHA-256 over TLS is available for other hosts, with
  per-table read, write and ddl grants per account. There are no row predicates, roles or policy
  language yet.
- Disclosed gaps: no SCRAM channel binding ([#360](https://github.com/moncifem/latticedb/issues/360));
  no security event log, and no failed-login backoff on the PostgreSQL listener (the gateway limits failed
  bearer checks per address) ([#361](https://github.com/moncifem/latticedb/issues/361));
  a filtered change feed leaks the count of hidden writes ([#363](https://github.com/moncifem/latticedb/issues/363));
  feed reads are not fully bounded ([#543](https://github.com/moncifem/latticedb/issues/543)); text
  stored in the database can carry prompt injection to an agent that reads it.
- The agent gateway is on `main` behind `--gateway-listen`. Plaintext is for loopback only. Off loopback it
  needs TLS (`--tls-cert`, `--tls-key`) and bearer tokens (`--gateway-tokens`), and the server refuses to start
  without them. `POST /mcp` is served on the gateway port and needs a bearer token; `lattice mcp` is a stdio
  proxy to it; it has no TLS client and reaches only a loopback plaintext gateway. The Host and Origin checks are on `main`; the rest of the browser and DNS-rebinding defences
  (#580) are open, and the threat model lists them.

See [docs/alpha.md](docs/alpha.md) for the known-issues list.

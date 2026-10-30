# ADR 0026: Carry the authenticated principal from the credential store to the query handler

Status: proposed

## Context

Threat model gap G-01 (issue #357): the SCRAM exchange proved who the client is, then the result was
discarded. `Session` checked that a startup `user` parameter existed, `QueryHandler` had no identity
argument and the listener built the handler without it. The first handler that reads stored data
would have served every authenticated login the same rows. Nothing is exposed today because the only
handler is the `SELECT 1` placeholder.

## Decision

`lattice_api::principal::Principal` is the identity of one connection. It has a kind and a name.

- `PrincipalKind::Account`: the account authenticated by SCRAM. The name is the key of the entry in
  the credential store (`auth::Accounts`), copied from the store and never from the client string. The
  constructor is crate-private, so a handler or a host outside `lattice-api` cannot build one.
- `PrincipalKind::LocalDevelopment`: the fixed principal of the loopback trust mode, named
  `local-development`. It is a labeled stand-in, not a user. `Session::local_trust` is the only way to
  request it, and the host refuses trust mode on a non-loopback address.

The credential file has no numeric ids. The account name is the stable identifier in this slice (a
duplicate name is refused when the file is parsed). When `lattice-identity` assigns durable user ids,
`Principal` gains the id and keeps the same call path.

A `Session` holds the principal as pending state until startup completes. It is published only in the
step that sends `AuthenticationOk`: after a verified SCRAM proof for a known account, or immediately
in trust mode. It is never changed afterwards. There is no re-authentication and no `SET ROLE` in this
slice. If the startup `user` differs from the account the exchange was selected for, or `user`
appears more than once, startup is refused with `28000`. An unknown account never has a pending
principal and cannot reach the ready phase.

`QueryHandler` takes a `&QueryContext` as its first argument on `execute`, `describe`,
`execute_cancellable` and `describe_cancellable`. The context carries `&Principal` and is
`#[non_exhaustive]`, so a later field (a policy evaluator, a deadline) is not another breaking change.
A session in the ready phase without a principal answers `28000` instead of calling the handler.
Messages received before authentication completes never reach the handler.

The principal is logged only through `Display` (`account:alice`, `local-development`). No secret,
verifier or nonce is part of it.

This is a public API change to `lattice-api` (trait signatures, `Accounts::exchange` return type,
`Session::new` renamed to `Session::local_trust`). It changes no byte format and no wire message.

### Out of scope

- Selecting a `PolicyEvaluator` from the principal and making an unconfigured handler fail closed
  (`DenyAll`). The handler now receives the principal; the executor change wires the policy. Issue
  #357 stays open for that part and for its row-visibility test through the listener.
- Cancel authorization. A PostgreSQL `CancelRequest` arrives on a new, unauthenticated connection and
  carries only the process id and the secret key. There is no principal on that connection, so a
  same-principal check cannot be applied without a protocol change. The pid and the 32-bit secret,
  issued after authentication, remain the whole capability. This is unchanged and recorded here so it
  is not mistaken for a gap that this change closes.
- SCRAM channel binding (G-04) and failed-login backoff (G-05).

## Consequences

Every handler is written against an authenticated identity from the start; a handler that ignores the
context is visible in review. Tests can assert exactly which principal reached the handler through the
real TCP, TLS and SCRAM path. Adding a field to `QueryContext` does not break implementors that only
read `principal`. The trait change breaks every existing implementor once: three in the workspace
plus a fuzz target.

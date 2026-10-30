Authenticated principal (#357, ADR 0026, proposed): a connection now has a `Principal`, set once when
authentication completes and never changed. SCRAM names it from the credential store's own account key
(not the client's `user` string); loopback trust mode uses the fixed, labeled `local-development`
principal (`Session::local_trust`, which replaces `Session::new`). `Accounts::exchange` returns
`Option<Principal>` instead of a bool, `Session::with_scram` takes it, and a startup `user` that differs
from that account, or a repeated `user`, is refused with `28000`. Every `QueryHandler` method now takes a
`&QueryContext` carrying the principal, so this is a breaking change for handler implementors. The row
policy is not yet selected from the principal (the executor change does that), and a `CancelRequest`
still authenticates by pid and secret alone.

# lattice-core

Layer L3.

**Responsibility.** Kernel: typestate transactions, WAL format, policy traits, engine traits.

**Must not.** Open sockets, spawn background loops, know any pack, load models.

Allowed workspace dependencies: lattice-abi, lattice-seams, lattice-lsl.

**Recovery.** Only an incomplete or checksum-failing tail of the newest segment is truncated. A complete, checksum-valid record with an unexpected LSN (a gap or a repeat), even as the last frame, makes recovery refuse the log and leaves the files untouched; `scrub` reports it as `MidLogDamage`.

**CRDTs.** `crdt`, `crdt_text` and `crdt_tree` converge on any set of valid operations. For input from an
untrusted peer use the `*_from` entry points with a `PeerContext` (`crdt_peer`): they check the node id bound to
the peer, the clock drift bound and the batch length before changing state, and the documents cap inserts per
quota key, tree nodes, tree depth and each origin's share of the tree op log. The limits are named public
constants documented in the module headers and in `docs/threat-model.md` (G-08). The `*_from` entry points accept only a peer's own
operations (the id's node must be the peer's), so a relay forwarding other replicas' operations uses the
trusting `apply` after its own checks, and quotas are charged per direct peer. A refusal by a cap is local to
the replica and the operation is lost unless sent again. `Tree::apply` has no per-origin share unless
`with_origin_quota` is set; `Tree::apply_from` always has one, and refuses an operation ordered before more than
`DEFAULT_PEER_REPLAY_LIMIT` live ops (`ReplayTooDeep`), which a resend to the same replica does not cure. This replay cap is a stopgap (a flood of 4097 new ops makes a replica refuse older honest ops): do not wire `apply_from` into a sync layer before issue 487 (resync API and sender-charged replay budget) is closed. The compaction frontier is still supplied by
the caller and trusted.

//! A tree CRDT with move semantics (P3-05, P3-06; invariant I5).
//!
//! Follows Kleppmann, Mulligan, Gomes and Beresford, "A highly-available move operation for
//! replicated trees". Every change is a *move* of a node under a parent, stamped with a hybrid
//! logical clock timestamp that doubles as the operation id. Moves are applied in the total order of
//! their timestamps. An operation that arrives out of order is handled by undoing every operation
//! ordered after it, applying it, and redoing the undone ones, so the tree is a pure function of the
//! *set* of operations and never of their delivery order. Duplicates are ignored by operation id.
//!
//! Rules every replica applies identically at the operation's place in the order:
//!
//! - A node's identity is its [`NodeId`]; moving it never changes the id, and its subtree follows it.
//!   The first move of an unknown node creates it.
//! - A move whose target parent is unknown, or that would make the node its own ancestor, is a
//!   no-op everywhere. The target may be [`NodeId::ROOT`] or [`NodeId::TRASH`].
//! - Delete is a move under [`NodeId::TRASH`], the tombstone root. The node and its subtree stay
//!   in the tree, so a later (in timestamp order) move can restore them, and a concurrent move into
//!   a deleted subtree lands in the trash too.
//! - Siblings are ordered by `(pos, node id)`; `pos` is chosen by the mover.
//!
//! # Compaction and the causal-stability contract
//!
//! The operation log is needed to undo and redo, so it grows with the number of operations until it
//! is compacted. [`Tree::compact`] takes a *stable frontier* `stable` and drops every logged
//! operation with `id <= stable`. The caller promises, and the tree cannot check:
//!
//! - **Every replica has already applied every operation ordered at or before `stable`**, and no
//!   replica will ever issue or relay a new operation ordered at or before it. The usual source is
//!   the minimum over all replicas of the clock each has fully caught up to (a replica that is
//!   offline or has not reported holds the frontier back).
//! - Under that contract an operation ordered at or before `stable` cannot arrive late, so nothing
//!   at or below it can ever be undone or redone again. Their effect is already in the materialised
//!   tree and stays there; only the undo records are discarded. Every later operation is ordered
//!   after all dropped ones, so it produces the same tree on every replica, compacted or not.
//! - An arrival at or below the frontier is answered [`Outcome::BelowFrontier`] and ignored. Its id
//!   is no longer known, so a harmless re-delivery of a dropped op and a genuine contract violation
//!   (an op that was never seen, which would have changed the tree) look the same here. Each one
//!   increments [`Tree::below_frontier_arrivals`]; a sync layer that tracks what it has delivered
//!   must treat an arrival it never delivered before as a stability bug and raise an alarm. Content
//!   is not compared, so a reused id with different content below the frontier is not detected.
//! - The frontier only moves forward; a lower `stable` is a no-op. `stable` must come from trusted
//!   replica state, never from a peer's unchecked claim: [`Tree::compact`] refuses a `stable` greater
//!   than every id this replica knows ([`TreeError::FrontierBeyondLog`]), but any smaller wrong value
//!   would be accepted and could make replicas diverge.
//!
//! # The cap, and why it can stall writes
//!
//! A tree has a cap on the live (uncompacted) op count ([`Tree::with_op_limit`], default
//! [`DEFAULT_OP_LIMIT`]). A new operation that would exceed it is refused with
//! [`TreeError::OpLimitReached`]; nothing is dropped silently. The cost of a late op is the number of
//! live ops ordered after it, so the worst case is bounded only by the cap. Plainly:
//!
//! - One offline or silent replica holds the stable frontier back, so the log cannot shrink and the
//!   cap can be reached. Writes then stall until that replica catches up, is removed from the
//!   frontier computation by the caller's membership rules, or the cap is raised.
//! - A remote peer can fill the log (about 41 MB of ops for 2^20 entries). To keep local writes
//!   alive, [`Tree::apply`] (remote ops) is refused once the log holds `cap - reserve` ops, while
//!   [`Tree::apply_local`] may use the reserved headroom up to `cap`. The default reserve is 1/16 of
//!   the cap; see [`Tree::with_op_limit_and_reserve`]. This is a single shared pool, not a quota per
//!   origin.
//! - A refused remote op is lost unless it is delivered again. The sync layer must retry it after
//!   compaction; the tree keeps no queue. The caller owns both retrying and compacting.
//!
//! # Limits on what a peer can make a replica hold
//!
//! Besides the op log cap above, a tree bounds three more things, all decided before the tree is
//! changed (G-08):
//!
//! - **Nodes**: at most [`DEFAULT_NODE_LIMIT`] ([`Tree::with_node_limit`]). Nodes survive
//!   compaction, so this is the lifetime bound on tree size; an operation naming a new node past it
//!   is [`TreeError::NodeLimitReached`] and, like an op-log refusal, is lost unless sent again.
//! - **Depth**: a node, and everything below it, may not sit deeper than [`MAX_TREE_DEPTH`]. A move
//!   that would is a no-op everywhere ([`Outcome::Skipped`]), decided at the move's place in the
//!   total order like the cycle rule, so every delivery order agrees. The tree keeps the height of
//!   every node's subtree (updated along the ancestor chain whenever a node moves), so the check
//!   costs O(depth) however wide the moved subtree is. `tla/TreeMove.tla` models the rule
//!   (`MaxDepth`, `CheckDepth`) and `tla/negative/TreeMoveNoDepthCheck.cfg` must violate
//!   `DepthBounded`. [`Tree::with_max_depth`] lowers the limit.
//! - **Share per origin**: a tree has no per-origin share for [`Tree::apply`] (a trusted bulk load
//!   may use the whole log) until [`Tree::with_origin_quota`] sets one. [`Tree::apply_from`] always
//!   enforces one, [`DEFAULT_ORIGIN_OP_LIMIT`] (a quarter of the default log) unless set. The origin
//!   is the id's node, so the share binds a peer only through `apply_from`, which also checks the
//!   node binding and the drift bound. Several hostile origins can still fill the log together.
//! - **Relaying**: `apply_from` requires the id to carry the peer's own node, so it cannot forward
//!   another replica's operations (hub or gossip topologies). A relay uses the trusting
//!   [`Tree::apply`] after its own admission policy, or a future relay entry point; quotas are per
//!   direct peer and so differ between replicas.
//! - **Permanent divergence**: a refusal by the op, node or origin cap is local. The refused
//!   operation is lost on this replica unless it is sent again, so replicas whose caps differ, or
//!   that received operations in a different mix, can differ for good until a resync.
//!
//! # Operation encoding (version 1, little endian, 41 bytes)
//!
//! | offset | size | field |
//! |---|---|---|
//! | 0 | 1 | version, must be 1 |
//! | 1 | 16 | operation id: `Hlc::to_bytes` (`wall_ms` u64, `counter` u32, `node` u32) |
//! | 17 | 8 | moved node id (u64, not 0 or 1) |
//! | 25 | 8 | new parent id (u64; 0 is the root, 1 is the trash) |
//! | 33 | 8 | sibling position (u64) |
//!
//! Decoding rejects any other length, any other version and reserved node ids.

use crate::bugs::{self, Bug};
use crate::crdt_peer::{BatchError, PeerContext, PeerError};
use lattice_abi::Hlc;
use std::collections::{BTreeMap, BTreeSet};
use std::ops::Bound;

/// Stable identity of a tree node. `0` and `1` are the two roots and cannot be moved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeId(pub u64);

impl NodeId {
    /// The root of the live tree.
    pub const ROOT: NodeId = NodeId(0);
    /// The tombstone root: deleting a node moves it here.
    pub const TRASH: NodeId = NodeId(1);

    fn is_reserved(self) -> bool {
        self == Self::ROOT || self == Self::TRASH
    }
}

/// One move: put `node` under `parent` at sibling position `pos`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Op {
    /// Operation id and position in the total order. Unique per operation.
    pub id: Hlc,
    /// The node being moved (created if unknown).
    pub node: NodeId,
    /// The new parent.
    pub parent: NodeId,
    /// Sibling ordering key under the new parent.
    pub pos: u64,
}

/// Why an operation could not be decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TreeDecodeError {
    /// The input is not exactly [`Op::ENCODED_LEN`] bytes.
    #[error("tree op must be {} bytes, got {got}", Op::ENCODED_LEN)]
    BadLength {
        /// Bytes received.
        got: usize,
    },
    /// Unknown encoding version.
    #[error("unknown tree op version {0}")]
    BadVersion(u8),
    /// The moved node is one of the reserved roots.
    #[error("tree op moves a reserved node")]
    ReservedNode,
}

/// Why an operation was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TreeError {
    /// The moved node is one of the reserved roots.
    #[error("the root and the trash cannot be moved")]
    ReservedNode,
    /// An operation with this id was already applied with different content.
    #[error("operation id reused with different content")]
    ConflictingOpId,
    /// `compact` was given a frontier greater than every id this replica knows.
    #[error("stable frontier is beyond every known operation id")]
    FrontierBeyondLog,
    /// The live op log is at its cap; compact below a stable frontier and retry.
    #[error("tree op log is full ({limit} live ops); compact below a stable frontier")]
    OpLimitReached {
        /// The configured cap.
        limit: usize,
    },
    /// The tree already holds its cap of nodes and the operation would name a new one.
    #[error("tree holds {limit} nodes; the operation names a new one")]
    NodeLimitReached {
        /// The configured cap.
        limit: usize,
    },
    /// One origin node already has its share of the live op log.
    #[error("node {node} already has {limit} live ops in the log")]
    OriginQuota {
        /// The origin node id of the refused operation.
        node: u32,
        /// The configured per-origin cap.
        limit: usize,
    },
    /// A peer's operation or batch was refused before it touched the tree.
    #[error("{0}")]
    Peer(PeerError),
}

/// What [`Tree::apply`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The move took effect at its place in the order.
    Applied,
    /// The move is recorded but is a no-op in the total order (unknown parent or a cycle).
    Skipped,
    /// The operation id was already known; nothing changed.
    Duplicate,
    /// The id is at or below the compaction frontier: ignored. Either a re-delivery of a dropped
    /// op or a violation of the stability contract; see the module docs and
    /// [`Tree::below_frontier_arrivals`].
    BelowFrontier,
}

const VERSION: u8 = 1;

/// Default cap on the live (uncompacted) op count of a [`Tree`].
pub const DEFAULT_OP_LIMIT: usize = 1 << 20;
/// Default cap on the number of nodes a [`Tree`] holds (the two roots do not count). Unlike the op
/// log, nodes survive compaction, so this is the lifetime bound on tree size.
pub const DEFAULT_NODE_LIMIT: usize = 1 << 20;
/// Deepest position a node may hold, counting the roots as depth 0. A move that would put the node,
/// or anything in its subtree, deeper is a no-op everywhere ([`Outcome::Skipped`]), like a cycle.
pub const MAX_TREE_DEPTH: usize = 256;
/// Share of the live op log one origin node may fill through [`Tree::apply_from`] when no share was
/// set with [`Tree::with_origin_quota`]: a quarter of [`DEFAULT_OP_LIMIT`]. [`Tree::apply`] has no
/// share unless one was set.
pub const DEFAULT_ORIGIN_OP_LIMIT: usize = DEFAULT_OP_LIMIT / 4;

impl Op {
    /// Encoded size in bytes.
    pub const ENCODED_LEN: usize = 41;

    /// A move of `node` under `parent`.
    pub fn new(id: Hlc, node: NodeId, parent: NodeId, pos: u64) -> Self {
        Self {
            id,
            node,
            parent,
            pos,
        }
    }

    /// A delete: a move under the tombstone root.
    pub fn delete(id: Hlc, node: NodeId) -> Self {
        Self::new(id, node, NodeId::TRASH, 0)
    }

    /// Encodes as documented in the module header.
    pub fn encode(&self) -> [u8; Self::ENCODED_LEN] {
        let mut out = [0u8; Self::ENCODED_LEN];
        let mut at = 0;
        let mut put = |bytes: &[u8]| {
            for b in bytes {
                if let Some(slot) = out.get_mut(at) {
                    *slot = *b;
                }
                at += 1;
            }
        };
        put(&[VERSION]);
        put(&self.id.to_bytes());
        put(&self.node.0.to_le_bytes());
        put(&self.parent.0.to_le_bytes());
        put(&self.pos.to_le_bytes());
        out
    }

    /// Decodes strictly: exact length, known version, no reserved node.
    pub fn decode(bytes: &[u8]) -> Result<Self, TreeDecodeError> {
        let bad = TreeDecodeError::BadLength { got: bytes.len() };
        if bytes.len() != Self::ENCODED_LEN {
            return Err(bad);
        }
        let version = bytes.first().copied().unwrap_or(0);
        if version != VERSION {
            return Err(TreeDecodeError::BadVersion(version));
        }
        let hlc: [u8; Hlc::ENCODED_LEN] = bytes
            .get(1..17)
            .and_then(|s| s.try_into().ok())
            .ok_or(bad)?;
        let word = |range: std::ops::Range<usize>| -> Result<u64, TreeDecodeError> {
            let arr: [u8; 8] = bytes
                .get(range)
                .and_then(|s| s.try_into().ok())
                .ok_or(bad)?;
            Ok(u64::from_le_bytes(arr))
        };
        let node = NodeId(word(17..25)?);
        if node.is_reserved() {
            return Err(TreeDecodeError::ReservedNode);
        }
        Ok(Self {
            id: Hlc::from_bytes(&hlc),
            node,
            parent: NodeId(word(25..33)?),
            pos: word(33..41)?,
        })
    }
}

/// An applied operation and what it replaced, so it can be undone.
#[derive(Debug, Clone, Copy)]
struct Entry {
    op: Op,
    /// The node's parent and position before this operation, if it existed.
    old: Option<(NodeId, u64)>,
    /// Whether the move took effect.
    applied: bool,
}

/// A point-in-time copy of the tree: `(node, parent, pos)` in node id order.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Snapshot {
    /// Every known node with its parent and sibling position.
    pub entries: Vec<(NodeId, NodeId, u64)>,
}

/// A replica of the move-aware tree.
#[derive(Debug, Clone, Default)]
pub struct Tree {
    log: BTreeMap<Hlc, Entry>,
    /// Stable frontier: ops at or below it were dropped and cannot arrive late.
    floor: Option<Hlc>,
    /// Cap on `log.len()`; `0` is treated as unset and means [`DEFAULT_OP_LIMIT`].
    limit: usize,
    /// Slots of the cap kept for local ops; only meaningful when `limit` is set.
    reserve: usize,
    /// Cap on `parents.len()`; `0` means [`DEFAULT_NODE_LIMIT`].
    node_limit: usize,
    /// Cap on live ops per origin node; `0` means none for [`Tree::apply`] and
    /// [`DEFAULT_ORIGIN_OP_LIMIT`] for [`Tree::apply_from`].
    origin_limit: usize,
    /// Deepest position allowed; `0` means [`MAX_TREE_DEPTH`].
    max_depth: usize,
    /// Height of each node's subtree (0 for a leaf, absent for a leaf), kept in step with
    /// `parents` by `set`.
    heights: BTreeMap<NodeId, usize>,
    /// `(parent, height of the child's subtree, child)`: the tallest child of a parent is the last
    /// entry in its range.
    by_height: BTreeSet<(NodeId, usize, NodeId)>,
    /// Live ops in `log` per origin node (the id's node).
    origin_ops: BTreeMap<u32, usize>,
    /// Arrivals at or below the frontier.
    below: u64,
    /// Ops undone and redone so far (late-op cost counter, tests only).
    #[cfg(test)]
    replayed: u64,
    parents: BTreeMap<NodeId, (NodeId, u64)>,
    /// `(parent, pos, node)`, for ordered children.
    kids: BTreeSet<(NodeId, u64, NodeId)>,
}

impl Tree {
    /// An empty tree: only the two roots exist.
    pub fn new() -> Self {
        Self::default()
    }

    /// An empty tree whose live op log may hold at most `limit` ops (at least 1). One sixteenth of
    /// the cap is reserved for local ops.
    pub fn with_op_limit(limit: usize) -> Self {
        let limit = limit.max(1);
        Self::with_op_limit_and_reserve(limit, limit / 16)
    }

    /// Like [`Tree::with_op_limit`], with `reserve` slots (clamped below the cap) that only
    /// [`Tree::apply_local`] may fill.
    pub fn with_op_limit_and_reserve(limit: usize, reserve: usize) -> Self {
        let limit = limit.max(1);
        Self {
            limit,
            reserve: reserve.min(limit - 1),
            ..Self::default()
        }
    }

    /// Caps the number of nodes (at least 1); an operation naming a new node past it is refused
    /// with [`TreeError::NodeLimitReached`].
    #[must_use]
    pub fn with_node_limit(mut self, limit: usize) -> Self {
        self.node_limit = limit.max(1);
        self
    }

    /// Lowers the deepest position a node may hold (at least 1; values above [`MAX_TREE_DEPTH`]
    /// are clamped to it). Every replica of a document must use the same value, or they disagree
    /// about which moves are no-ops.
    #[must_use]
    pub fn with_max_depth(mut self, depth: usize) -> Self {
        self.max_depth = depth.clamp(1, MAX_TREE_DEPTH);
        self
    }

    fn depth_cap(&self) -> usize {
        if self.max_depth == 0 {
            MAX_TREE_DEPTH
        } else {
            self.max_depth
        }
    }

    /// Caps the live ops one origin node may hold in the log (at least 1), for remote ops through
    /// [`Tree::apply`] and [`Tree::apply_from`]; local ops are never refused by it. The origin is
    /// the id's node, so the cap only binds a peer when the transport binds node ids
    /// ([`Tree::apply_from`] does).
    #[must_use]
    pub fn with_origin_quota(mut self, limit: usize) -> Self {
        self.origin_limit = limit.max(1);
        self
    }

    fn node_cap(&self) -> usize {
        if self.node_limit == 0 {
            DEFAULT_NODE_LIMIT
        } else {
            self.node_limit
        }
    }

    /// The share for `apply_from`: the configured one, else the default.
    fn peer_origin_cap(&self) -> usize {
        if self.origin_limit == 0 {
            DEFAULT_ORIGIN_OP_LIMIT
        } else {
            self.origin_limit
        }
    }

    fn cap(&self) -> usize {
        if self.limit == 0 {
            DEFAULT_OP_LIMIT
        } else {
            self.limit
        }
    }

    fn reserved(&self) -> usize {
        if self.limit == 0 {
            DEFAULT_OP_LIMIT / 16
        } else {
            self.reserve
        }
    }

    /// The stable frontier passed to [`Tree::compact`] so far, if any.
    pub fn stable_frontier(&self) -> Option<Hlc> {
        self.floor
    }

    /// How many arrivals were ignored for being at or below the frontier. Re-deliveries count too;
    /// see the module docs.
    pub fn below_frontier_arrivals(&self) -> u64 {
        self.below
    }

    /// Total ops undone and redone by late arrivals so far.
    #[cfg(test)]
    fn replayed_ops(&self) -> u64 {
        self.replayed
    }

    /// Drops logged ops with `id <= stable` and returns how many were dropped. The visible tree
    /// does not change. See the module docs: the caller guarantees every replica has applied all
    /// ops ordered at or before `stable`, so none of them can arrive late, and `stable` comes from
    /// trusted state. A `stable` greater than every id this replica has seen is refused with
    /// [`TreeError::FrontierBeyondLog`]. A `stable` at or below the current frontier is a no-op.
    pub fn compact(&mut self, stable: Hlc) -> Result<usize, TreeError> {
        let newest = self.log.keys().next_back().copied().max(self.floor);
        if newest.is_none_or(|n| stable > n) {
            return Err(TreeError::FrontierBeyondLog);
        }
        if self.floor.is_some_and(|f| f >= stable) {
            return Ok(0);
        }
        self.floor = Some(stable);
        let mut keep = self.log.split_off(&stable);
        let mut dropped = self.log.len();
        if keep.remove(&stable).is_some() {
            dropped += 1;
        }
        self.log = keep;
        self.origin_ops.clear();
        for id in self.log.keys() {
            *self.origin_ops.entry(id.node).or_insert(0) += 1;
        }
        Ok(dropped)
    }

    /// Applies an operation received from a peer, in any delivery order, any number of times.
    /// Refused with [`TreeError::OpLimitReached`] once the log is within the local reserve of the cap.
    pub fn apply(&mut self, op: Op) -> Result<Outcome, TreeError> {
        let room = self.cap().saturating_sub(self.reserved());
        let share = (self.origin_limit != 0).then_some(self.origin_limit);
        self.apply_within(op, room.max(1), share)
    }

    /// Applies an operation from an authenticated peer. The id must carry the node bound to the
    /// peer ([`PeerError::ForgedNode`]) and be no further ahead of the caller's clock than the
    /// drift bound ([`PeerError::DriftTooLarge`]); a refusal changes nothing. Past that this is
    /// [`Tree::apply`] with the per-origin share always enforced, which the node binding makes
    /// meaningful. It cannot relay another replica's operations (see the module docs).
    pub fn apply_from(&mut self, peer: &PeerContext, op: Op) -> Result<Outcome, TreeError> {
        peer.admit(op.id).map_err(TreeError::Peer)?;
        let room = self.cap().saturating_sub(self.reserved());
        let share = Some(self.peer_origin_cap());
        self.apply_within(op, room.max(1), share)
    }

    /// Applies a batch from an authenticated peer. The batch length
    /// ([`crate::crdt_peer::MAX_BATCH_OPS`]) and every operation's shape, node binding and drift
    /// are checked before the first operation is applied; see [`BatchError`] for what a refusal
    /// leaves behind. A refused remote op is lost unless sent again (see the cap section above).
    pub fn apply_batch_from(
        &mut self,
        peer: &PeerContext,
        ops: &[Op],
    ) -> Result<Vec<Outcome>, BatchError<TreeError>> {
        let refuse = |error| BatchError { applied: 0, error };
        peer.check_batch(ops.len())
            .map_err(|e| refuse(TreeError::Peer(e)))?;
        for op in ops {
            if op.node.is_reserved() {
                return Err(refuse(TreeError::ReservedNode));
            }
            peer.admit(op.id).map_err(|e| refuse(TreeError::Peer(e)))?;
        }
        let mut out = Vec::with_capacity(ops.len());
        for op in ops {
            match self.apply_from(peer, *op) {
                Ok(o) => out.push(o),
                Err(error) => {
                    return Err(BatchError {
                        applied: out.len(),
                        error,
                    });
                }
            }
        }
        Ok(out)
    }

    /// Number of nodes in the tree, not counting the two roots.
    pub fn node_count(&self) -> usize {
        self.parents.len()
    }

    /// Applies an operation issued by this replica; may use the headroom reserved for local ops.
    pub fn apply_local(&mut self, op: Op) -> Result<Outcome, TreeError> {
        self.apply_within(op, self.cap(), None)
    }

    fn apply_within(
        &mut self,
        op: Op,
        room: usize,
        share: Option<usize>,
    ) -> Result<Outcome, TreeError> {
        if op.node.is_reserved() {
            return Err(TreeError::ReservedNode);
        }
        if let Some(seen) = self.log.get(&op.id) {
            return if seen.op == op {
                Ok(Outcome::Duplicate)
            } else {
                Err(TreeError::ConflictingOpId)
            };
        }
        if self.floor.is_some_and(|f| op.id <= f) {
            // Dropped by compaction: a re-delivery, or a contract violation (see the module docs).
            self.below += 1;
            return Ok(Outcome::BelowFrontier);
        }
        if self.log.len() >= room {
            return Err(TreeError::OpLimitReached { limit: self.cap() });
        }
        if let Some(limit) = share
            && self.origin_ops.get(&op.id.node).copied().unwrap_or(0) >= limit
            && !bugs::active(Bug::TreeOriginQuotaSkipped)
        {
            return Err(TreeError::OriginQuota {
                node: op.id.node,
                limit,
            });
        }
        if !self.parents.contains_key(&op.node)
            && self.parents.len() >= self.node_cap()
            && !bugs::active(Bug::TreeNodeLimitSkipped)
        {
            return Err(TreeError::NodeLimitReached {
                limit: self.node_cap(),
            });
        }
        let later: Vec<Hlc> = self
            .log
            .range((Bound::Excluded(op.id), Bound::Unbounded))
            .map(|(k, _)| *k)
            .collect();
        #[cfg(test)]
        {
            self.replayed += later.len() as u64;
        }
        for k in later.iter().rev() {
            self.undo(*k);
        }
        let entry = self.perform(op);
        let applied = entry.applied;
        self.log.insert(op.id, entry);
        *self.origin_ops.entry(op.id.node).or_insert(0) += 1;
        for k in later {
            self.redo(k);
        }
        Ok(if applied {
            Outcome::Applied
        } else {
            Outcome::Skipped
        })
    }

    fn undo(&mut self, id: Hlc) {
        if let Some(e) = self.log.get(&id).copied()
            && e.applied
        {
            self.set(e.op.node, e.old);
        }
    }

    fn redo(&mut self, id: Hlc) {
        if let Some(e) = self.log.get(&id).copied() {
            let fresh = self.perform(e.op);
            self.log.insert(id, fresh);
        }
    }

    /// Decides and, if valid, performs one move against the current state.
    fn perform(&mut self, op: Op) -> Entry {
        let old = self.parents.get(&op.node).copied();
        let parent_known = op.parent.is_reserved() || self.parents.contains_key(&op.parent);
        let valid = parent_known
            && (bugs::active(Bug::TreeCycleCheckSkipped) || !self.would_cycle(op.node, op.parent))
            && (bugs::active(Bug::TreeDepthLimitSkipped) || self.fits_depth(op.node, op.parent));
        if valid {
            self.set(op.node, Some((op.parent, op.pos)));
        }
        Entry {
            op,
            old,
            applied: valid,
        }
    }

    /// Depth of `node` (roots are 0), or `None` when it is deeper than the depth cap.
    fn depth_of(&self, node: NodeId) -> Option<usize> {
        let mut cur = node;
        for depth in 0..=self.depth_cap() {
            match self.parents.get(&cur) {
                Some((p, _)) => cur = *p,
                None => return Some(depth),
            }
        }
        None
    }

    fn height(&self, node: NodeId) -> usize {
        self.heights.get(&node).copied().unwrap_or(0)
    }

    /// Whether moving `node` under `parent` keeps `node` and everything below it within the depth
    /// cap. O(depth): the subtree height is kept, not walked.
    fn fits_depth(&self, node: NodeId, parent: NodeId) -> bool {
        self.depth_of(parent)
            .is_some_and(|d| d + 1 + self.height(node) <= self.depth_cap())
    }

    /// Recomputes the height of `start` from its tallest child and carries a change up the
    /// ancestor chain. Stops at the first unchanged height, at a root, or after one step per node
    /// (which only a planted cycle defect can reach).
    fn refresh_heights(&mut self, start: NodeId) {
        let mut cur = start;
        for _ in 0..=self.parents.len() {
            let tallest = self
                .by_height
                .range((cur, 0, NodeId(0))..=(cur, usize::MAX, NodeId(u64::MAX)))
                .next_back()
                .map(|(_, h, _)| *h);
            let new_h = tallest.map_or(0, |h| h + 1);
            let old_h = self.height(cur);
            if new_h == old_h {
                return;
            }
            let parent = self.parents.get(&cur).map(|(p, _)| *p);
            if let Some(p) = parent {
                self.by_height.remove(&(p, old_h, cur));
                self.by_height.insert((p, new_h, cur));
            }
            if new_h == 0 {
                self.heights.remove(&cur);
            } else {
                self.heights.insert(cur, new_h);
            }
            match parent {
                Some(p) => cur = p,
                None => return,
            }
        }
    }

    /// Checks `heights` and `by_height` against the parent map, recomputed from scratch.
    #[cfg(test)]
    pub(crate) fn heights_are_consistent(&self) -> bool {
        let mut want: BTreeMap<NodeId, usize> = BTreeMap::new();
        for start in self.parents.keys() {
            let mut cur = *start;
            let mut h = 0;
            while let Some((p, _)) = self.parents.get(&cur) {
                h += 1;
                let e = want.entry(*p).or_insert(0);
                *e = (*e).max(h);
                cur = *p;
                if h > self.parents.len() {
                    return false;
                }
            }
        }
        let have: BTreeMap<NodeId, usize> = self.heights.clone();
        let entries_ok = self.by_height.len() == self.parents.len()
            && self.by_height.iter().all(|(p, h, c)| {
                self.parents.get(c).map(|x| x.0) == Some(*p) && *h == self.height(*c)
            });
        want == have && entries_ok
    }

    /// Whether `node` is `parent` or an ancestor of it.
    fn would_cycle(&self, node: NodeId, parent: NodeId) -> bool {
        let mut cur = parent;
        for _ in 0..=self.parents.len() {
            if cur == node {
                return true;
            }
            match self.parents.get(&cur) {
                Some((p, _)) => cur = *p,
                None => return false,
            }
        }
        true // a loop already exists; refuse to extend it
    }

    fn set(&mut self, node: NodeId, to: Option<(NodeId, u64)>) {
        if let Some((p, pos)) = self.parents.remove(&node) {
            self.kids.remove(&(p, pos, node));
            let h = self.height(node);
            self.by_height.remove(&(p, h, node));
            self.refresh_heights(p);
        }
        if let Some((p, pos)) = to {
            self.parents.insert(node, (p, pos));
            self.kids.insert((p, pos, node));
            let h = self.height(node);
            self.by_height.insert((p, h, node));
            self.refresh_heights(p);
        }
    }

    /// Whether `node` exists (the roots always do).
    pub fn contains(&self, node: NodeId) -> bool {
        node.is_reserved() || self.parents.contains_key(&node)
    }

    /// The parent of `node`, if it exists and is not a root.
    pub fn parent(&self, node: NodeId) -> Option<NodeId> {
        self.parents.get(&node).map(|(p, _)| *p)
    }

    /// Children of `parent` in sibling order `(pos, node id)`.
    pub fn children(&self, parent: NodeId) -> impl Iterator<Item = NodeId> + '_ {
        self.kids
            .range((parent, 0, NodeId(0))..=(parent, u64::MAX, NodeId(u64::MAX)))
            .map(|k| k.2)
    }

    /// Whether `node` hangs under the tombstone root.
    pub fn is_deleted(&self, node: NodeId) -> bool {
        let mut cur = node;
        for _ in 0..=self.parents.len() {
            match self.parents.get(&cur) {
                Some((p, _)) => cur = *p,
                None => return cur == NodeId::TRASH,
            }
        }
        false
    }

    /// Whether every node reaches a root. Always true unless a defect is planted.
    pub fn is_acyclic(&self) -> bool {
        self.parents.keys().all(|start| {
            let mut cur = *start;
            for _ in 0..=self.parents.len() {
                match self.parents.get(&cur) {
                    Some((p, _)) => cur = *p,
                    None => return true,
                }
            }
            false
        })
    }

    /// Number of distinct operations seen.
    pub fn op_count(&self) -> usize {
        self.log.len()
    }

    /// The current state, comparable across replicas.
    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            entries: self
                .parents
                .iter()
                .map(|(n, (p, s))| (*n, *p, *s))
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
    use super::*;
    use proptest::prelude::*;

    fn t(wall: u64, node: u32) -> Hlc {
        Hlc {
            wall_ms: wall,
            counter: 0,
            node,
        }
    }

    fn n(i: u64) -> NodeId {
        NodeId(i + 10)
    }

    fn mv(wall: u64, node_id: u32, child: u64, parent: NodeId) -> Op {
        Op::new(t(wall, node_id), n(child), parent, 0)
    }

    fn mv_in(wall: u64, replica: u32, child: u64, parent: u64) -> Op {
        mv(wall, replica, child, n(parent))
    }

    fn build(ops: &[Op]) -> Tree {
        let mut tree = Tree::new();
        for op in ops {
            tree.apply(*op).unwrap();
        }
        tree
    }

    /// Delivers `ops` in the given order to fresh replicas and returns their snapshots, which
    /// must all agree.
    fn all_orders_agree(ops: &[Op]) -> Snapshot {
        let forward = build(ops);
        let mut reversed: Vec<Op> = ops.to_vec();
        reversed.reverse();
        let backward = build(&reversed);
        assert!(forward.is_acyclic() && backward.is_acyclic());
        assert_eq!(forward.snapshot(), backward.snapshot());
        forward.snapshot()
    }

    #[test]
    fn moving_keeps_identity_and_the_subtree_follows() {
        let mut tree = build(&[
            mv(1, 1, 1, NodeId::ROOT),
            mv_in(2, 1, 2, 1),
            mv_in(3, 1, 3, 2),
            mv(4, 1, 4, NodeId::ROOT),
        ]);
        assert_eq!(tree.parent(n(2)), Some(n(1)));
        tree.apply(mv_in(5, 1, 2, 4)).unwrap();
        assert_eq!(tree.parent(n(2)), Some(n(4)), "same id, new parent");
        assert_eq!(tree.parent(n(3)), Some(n(2)), "its child moved with it");
        assert_eq!(tree.children(n(1)).count(), 0);
        assert_eq!(tree.children(n(4)).collect::<Vec<_>>(), vec![n(2)]);
    }

    #[test]
    fn children_are_ordered_by_position_then_node_id() {
        let tree = build(&[
            Op::new(t(1, 1), n(5), NodeId::ROOT, 20),
            Op::new(t(2, 1), n(3), NodeId::ROOT, 10),
            Op::new(t(3, 1), n(4), NodeId::ROOT, 10),
            Op::new(t(4, 1), n(9), n(3), 0),
        ]);
        assert_eq!(
            tree.children(NodeId::ROOT).collect::<Vec<_>>(),
            vec![n(3), n(4), n(5)]
        );
        assert_eq!(tree.children(n(3)).collect::<Vec<_>>(), vec![n(9)]);
    }

    #[test]
    fn concurrent_moves_of_one_node_the_later_timestamp_wins_in_any_order() {
        let setup = [
            mv(1, 0, 1, NodeId::ROOT),
            mv(2, 0, 2, NodeId::ROOT),
            mv(3, 0, 3, NodeId::ROOT),
        ];
        // Two replicas move node 3 at the same wall time; the node id breaks the tie.
        let a = mv_in(10, 1, 3, 1);
        let b = mv_in(10, 2, 3, 2);
        for order in [[a, b], [b, a]] {
            let mut ops = setup.to_vec();
            ops.extend(order);
            let tree = build(&ops);
            assert_eq!(tree.parent(n(3)), Some(n(2)));
            assert_eq!(tree.children(n(1)).count(), 0);
        }
    }

    #[test]
    fn a_late_older_move_does_not_override_a_newer_one() {
        let tree = build(&[
            mv(1, 0, 1, NodeId::ROOT),
            mv(1, 1, 2, NodeId::ROOT),
            mv(1, 2, 3, NodeId::ROOT),
            mv_in(9, 1, 3, 2), // newer, arrives first
            mv_in(5, 2, 3, 1), // older, arrives late
        ]);
        assert_eq!(tree.parent(n(3)), Some(n(2)));
    }

    #[test]
    fn a_cycle_from_two_replicas_is_dropped_the_same_way_everywhere() {
        let setup = [mv(1, 0, 1, NodeId::ROOT), mv(1, 1, 2, NodeId::ROOT)];
        let a = mv_in(10, 1, 1, 2); // replica 1: put 1 under 2
        let b = mv_in(10, 2, 2, 1); // replica 2: put 2 under 1
        let mut results = Vec::new();
        for order in [[a, b], [b, a]] {
            let mut ops = setup.to_vec();
            ops.extend(order);
            let tree = build(&ops);
            assert!(tree.is_acyclic());
            results.push(tree.snapshot());
            // Replica 1's move is ordered first and wins; replica 2's would close the loop.
            assert_eq!(tree.parent(n(1)), Some(n(2)));
            assert_eq!(tree.parent(n(2)), Some(NodeId::ROOT));
        }
        assert_eq!(results[0], results[1]);
    }

    #[test]
    fn a_move_into_its_own_descendant_or_itself_is_a_no_op() {
        let mut tree = build(&[mv(1, 0, 1, NodeId::ROOT), mv_in(2, 0, 2, 1)]);
        assert_eq!(tree.apply(mv_in(3, 0, 1, 2)).unwrap(), Outcome::Skipped);
        assert_eq!(tree.apply(mv_in(4, 0, 1, 1)).unwrap(), Outcome::Skipped);
        assert_eq!(tree.parent(n(1)), Some(NodeId::ROOT));
        assert!(tree.is_acyclic());
    }

    #[test]
    fn a_move_under_an_unknown_parent_is_a_no_op() {
        let mut tree = Tree::new();
        assert_eq!(tree.apply(mv_in(1, 0, 1, 7)).unwrap(), Outcome::Skipped);
        assert!(!tree.contains(n(1)));
    }

    #[test]
    fn delete_is_a_move_into_the_trash_and_can_be_undone_by_a_later_move() {
        let mut tree = build(&[
            mv(1, 0, 1, NodeId::ROOT),
            mv_in(2, 0, 2, 1),
            Op::delete(t(3, 0), n(1)),
        ]);
        assert!(tree.is_deleted(n(1)));
        assert!(tree.is_deleted(n(2)), "the subtree is deleted with it");
        assert!(tree.contains(n(1)), "tombstones stay");
        tree.apply(mv(4, 0, 1, NodeId::ROOT)).unwrap();
        assert!(!tree.is_deleted(n(2)));
    }

    #[test]
    fn delete_versus_move_into_the_deleted_node_converges() {
        let setup = [
            mv(1, 0, 1, NodeId::ROOT),
            mv(1, 1, 2, NodeId::ROOT),
            mv(1, 2, 3, NodeId::ROOT),
        ];
        // Replica 1 deletes node 1 while replica 2 moves node 3 into node 1.
        let delete = Op::delete(t(10, 1), n(1));
        let into = mv_in(10, 2, 3, 1);
        for order in [[delete, into], [into, delete]] {
            let mut ops = setup.to_vec();
            ops.extend(order);
            let tree = build(&ops);
            assert!(tree.is_deleted(n(3)), "the moved node ends up deleted too");
            assert!(!tree.is_deleted(n(2)));
        }
        // A move out of the node concurrent with its delete keeps the moved node alive.
        let out = mv(10, 2, 3, NodeId::ROOT);
        let mut ops = setup.to_vec();
        ops.push(mv_in(5, 2, 3, 1));
        ops.extend([delete, out]);
        let snap = all_orders_agree(&ops);
        assert!(snap.entries.contains(&(n(1), NodeId::TRASH, 0)));
        assert!(snap.entries.contains(&(n(3), NodeId::ROOT, 0)));
    }

    #[test]
    fn duplicate_and_reordered_delivery_is_harmless() {
        let ops = [
            mv(1, 0, 1, NodeId::ROOT),
            mv_in(2, 0, 2, 1),
            mv_in(3, 1, 1, 2),
            Op::delete(t(4, 2), n(2)),
        ];
        let once = build(&ops);
        let mut tree = Tree::new();
        for op in ops.iter().rev().chain(ops.iter()).chain(ops.iter()) {
            tree.apply(*op).unwrap();
        }
        assert_eq!(tree.snapshot(), once.snapshot());
        assert_eq!(tree.op_count(), ops.len());
        assert_eq!(tree.apply(ops[0]).unwrap(), Outcome::Duplicate);
    }

    #[test]
    fn reusing_an_op_id_with_other_content_is_refused_and_changes_nothing() {
        let mut tree = build(&[mv(1, 0, 1, NodeId::ROOT)]);
        let before = tree.snapshot();
        assert_eq!(
            tree.apply(Op::new(t(1, 0), n(1), NodeId::TRASH, 0)),
            Err(TreeError::ConflictingOpId)
        );
        assert_eq!(tree.snapshot(), before);
        assert_eq!(
            tree.apply(Op::new(t(2, 0), NodeId::ROOT, n(1), 0)),
            Err(TreeError::ReservedNode)
        );
    }

    #[test]
    fn ops_round_trip_and_decoding_is_strict() {
        let op = Op::new(
            Hlc {
                wall_ms: 0x0102_0304_0506_0708,
                counter: 9,
                node: 7,
            },
            NodeId(0xAABB),
            NodeId::TRASH,
            u64::MAX,
        );
        let bytes = op.encode();
        assert_eq!(bytes.len(), Op::ENCODED_LEN);
        assert_eq!(bytes[0], 1);
        // Fixed layout written out by hand, not derived from encode().
        assert_eq!(&bytes[1..9], &[8, 7, 6, 5, 4, 3, 2, 1]);
        assert_eq!(&bytes[9..13], &[9, 0, 0, 0]);
        assert_eq!(&bytes[13..17], &[7, 0, 0, 0]);
        assert_eq!(&bytes[17..25], &[0xBB, 0xAA, 0, 0, 0, 0, 0, 0]);
        assert_eq!(&bytes[25..33], &[1, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(&bytes[33..41], &[0xFF; 8]);
        assert_eq!(Op::decode(&bytes), Ok(op));

        assert_eq!(
            Op::decode(&bytes[..40]),
            Err(TreeDecodeError::BadLength { got: 40 })
        );
        let mut long = bytes.to_vec();
        long.push(0);
        assert_eq!(
            Op::decode(&long),
            Err(TreeDecodeError::BadLength { got: 42 })
        );
        assert_eq!(Op::decode(&[]), Err(TreeDecodeError::BadLength { got: 0 }));
        let mut bad_version = bytes;
        bad_version[0] = 2;
        assert_eq!(
            Op::decode(&bad_version),
            Err(TreeDecodeError::BadVersion(2))
        );
        let mut reserved = bytes;
        reserved[17..25].copy_from_slice(&1u64.to_le_bytes());
        assert_eq!(Op::decode(&reserved), Err(TreeDecodeError::ReservedNode));
    }

    // ---- convergence under random histories -------------------------------------------------

    #[derive(Debug, Clone)]
    enum Step {
        /// A replica makes a local change from what it currently sees.
        Local {
            replica: u8,
            kind: u8,
            a: u16,
            b: u16,
            pos: u8,
        },
        /// A previously issued op reaches a replica (possibly again, possibly late).
        Deliver { to: u8, which: u16 },
    }

    fn arb_steps() -> impl Strategy<Value = Vec<Step>> {
        proptest::collection::vec(
            prop_oneof![
                3 => (any::<u8>(), any::<u8>(), any::<u16>(), any::<u16>(), 0u8..3)
                    .prop_map(|(replica, kind, a, b, pos)| Step::Local { replica, kind, a, b, pos }),
                2 => (any::<u8>(), any::<u16>()).prop_map(|(to, which)| Step::Deliver { to, which }),
            ],
            1..40,
        )
    }

    /// Picks a parent for a move from what the replica sees, including the roots.
    fn pick(snapshot: &Snapshot, choice: u16, with_trash: bool) -> NodeId {
        let mut options = vec![NodeId::ROOT];
        if with_trash {
            options.push(NodeId::TRASH);
        }
        options.extend(snapshot.entries.iter().map(|e| e.0));
        options[usize::from(choice) % options.len()]
    }

    /// Replays the whole history in plain timestamp order with no undo or redo: the definition
    /// of the result, used as an independent oracle.
    fn oracle(ops: &[Op]) -> Snapshot {
        let mut sorted = ops.to_vec();
        sorted.sort_by_key(|o| o.id);
        sorted.dedup_by_key(|o| o.id);
        let mut parents: BTreeMap<NodeId, (NodeId, u64)> = BTreeMap::new();
        for op in sorted {
            let known = op.parent.is_reserved() || parents.contains_key(&op.parent);
            let mut cur = op.parent;
            let mut cycle = false;
            loop {
                if cur == op.node {
                    cycle = true;
                    break;
                }
                match parents.get(&cur) {
                    Some((p, _)) => cur = *p,
                    None => break,
                }
            }
            if known && !cycle {
                parents.insert(op.node, (op.parent, op.pos));
            }
        }
        Snapshot {
            entries: parents.into_iter().map(|(n, (p, s))| (n, p, s)).collect(),
        }
    }

    /// Runs the script, then delivers everything everywhere in a different order with
    /// duplicates, and checks acyclicity throughout and agreement at the end.
    fn run_script(replicas: usize, steps: &[Step], shuffle: u64) -> Result<(), String> {
        let mut trees: Vec<Tree> = (0..replicas).map(|_| Tree::new()).collect();
        let mut issued: Vec<Op> = Vec::new();
        let mut fresh = vec![0u64; replicas];
        let mut seq = vec![0u32; replicas];
        let check = |tree: &Tree| {
            if tree.is_acyclic() {
                Ok(())
            } else {
                Err("cycle in a replica".to_string())
            }
        };
        for (clock, step) in (1u64..).zip(steps) {
            match step {
                Step::Local {
                    replica,
                    kind,
                    a,
                    b,
                    pos,
                } => {
                    let r = usize::from(*replica) % replicas;
                    let (Some(tree), Some(f), Some(s)) =
                        (trees.get_mut(r), fresh.get_mut(r), seq.get_mut(r))
                    else {
                        continue;
                    };
                    let snap = tree.snapshot();
                    let node_choice = pick(&snap, *a, false);
                    let id = Hlc {
                        wall_ms: clock / 2,
                        counter: *s,
                        node: u32::try_from(r).unwrap_or(0),
                    };
                    *s += 1;
                    let op = match kind % 3 {
                        // create a fresh node under something
                        0 => {
                            *f += 1;
                            let node = NodeId(100 * (r as u64 + 1) + *f);
                            Op::new(id, node, pick(&snap, *b, false), u64::from(*pos))
                        }
                        // move an existing node
                        1 if node_choice != NodeId::ROOT => {
                            Op::new(id, node_choice, pick(&snap, *b, true), u64::from(*pos))
                        }
                        // delete
                        _ if node_choice != NodeId::ROOT => Op::delete(id, node_choice),
                        _ => continue,
                    };
                    tree.apply(op).map_err(|e| e.to_string())?;
                    check(tree)?;
                    issued.push(op);
                }
                Step::Deliver { to, which } => {
                    if issued.is_empty() {
                        continue;
                    }
                    let op = issued[usize::from(*which) % issued.len()];
                    let Some(tree) = trees.get_mut(usize::from(*to) % replicas) else {
                        continue;
                    };
                    tree.apply(op).map_err(|e| e.to_string())?;
                    check(tree)?;
                }
            }
        }
        // Final delivery: every op to every replica, shuffled differently per replica, with a
        // duplicate of each.
        let expected = oracle(&issued);
        for (r, tree) in trees.iter_mut().enumerate() {
            let mut batch: Vec<(u64, Op)> = issued
                .iter()
                .chain(issued.iter())
                .enumerate()
                .map(|(i, op)| {
                    let key = (i as u64 + 1)
                        .wrapping_mul(6_364_136_223_846_793_005)
                        .wrapping_add(shuffle.wrapping_mul(r as u64 + 1))
                        .rotate_left(17);
                    (key, *op)
                })
                .collect();
            batch.sort_by_key(|(k, _)| *k);
            for (_, op) in batch {
                tree.apply(op).map_err(|e| e.to_string())?;
                check(tree)?;
            }
            if tree.snapshot() != expected {
                return Err(format!("replica {r} diverged from the ordered replay"));
            }
        }
        for pair in trees.windows(2) {
            if let [x, y] = pair
                && x.snapshot() != y.snapshot()
            {
                return Err("replicas differ".into());
            }
        }
        Ok(())
    }

    proptest! {
        #[test]
        fn replicas_converge_and_stay_acyclic_under_reorder_duplication_and_delay(
            replicas in 2usize..5,
            steps in arb_steps(),
            shuffle in any::<u64>(),
        ) {
            prop_assert_eq!(run_script(replicas, &steps, shuffle), Ok(()));
        }

        #[test]
        fn the_decoder_never_panics_and_accepts_only_canonical_bytes(
            bytes in proptest::collection::vec(any::<u8>(), 0..64),
        ) {
            if let Ok(op) = Op::decode(&bytes) {
                prop_assert_eq!(op.encode().to_vec(), bytes);
            }
        }

        #[test]
        fn any_41_bytes_either_decode_canonically_or_are_rejected(
            bytes in proptest::collection::vec(any::<u8>(), Op::ENCODED_LEN),
            version in prop_oneof![Just(1u8), any::<u8>()],
        ) {
            let mut bytes = bytes;
            bytes[0] = version;
            match Op::decode(&bytes) {
                Ok(op) => prop_assert_eq!(op.encode().to_vec(), bytes),
                Err(e) => prop_assert!(matches!(
                    e,
                    TreeDecodeError::BadVersion(_) | TreeDecodeError::ReservedNode
                )),
            }
        }
    }

    // ---- compaction ---------------------------------------------------------------------------

    /// One step of a compaction script.
    #[derive(Debug, Clone)]
    enum CStep {
        Base(Step),
        /// Compact the replicas selected by the bit mask at the honest stable frontier.
        Compact(u8),
    }

    fn arb_csteps() -> impl Strategy<Value = Vec<CStep>> {
        proptest::collection::vec(
            prop_oneof![
                6 => (any::<u8>(), any::<u8>(), any::<u16>(), any::<u16>(), 0u8..3)
                    .prop_map(|(replica, kind, a, b, pos)| CStep::Base(Step::Local { replica, kind, a, b, pos })),
                5 => (any::<u8>(), any::<u16>())
                    .prop_map(|(to, which)| CStep::Base(Step::Deliver { to, which })),
                2 => any::<u8>().prop_map(CStep::Compact),
            ],
            1..60,
        )
    }

    /// The honest stable frontier: the largest issued id such that every issued op at or below it
    /// has been applied by every replica. Clocks strictly increase with each step, so no future op
    /// can be ordered at or below it.
    fn honest_stable(issued: &[Op], seen: &[BTreeSet<Hlc>]) -> Option<Hlc> {
        let mut ids: Vec<Hlc> = issued.iter().map(|o| o.id).collect();
        ids.sort();
        let mut frontier = None;
        for id in ids {
            if seen.iter().all(|s| s.contains(&id)) {
                frontier = Some(id);
            } else {
                break;
            }
        }
        frontier
    }

    /// Like `run_script`, but replicas compact at random honest stable points (so replicas sit at
    /// different frontiers and exchange ops across them). Ids come from per-replica skewed clocks
    /// merged hybrid-logical-clock style. Ends with shuffled, duplicated delivery of every op
    /// everywhere (including to compacted replicas) and compares with the uncompacted oracle.
    fn run_compacting_script(replicas: usize, steps: &[CStep], shuffle: u64) -> Result<(), String> {
        let mut trees: Vec<Tree> = (0..replicas).map(|_| Tree::new()).collect();
        let mut seen: Vec<BTreeSet<Hlc>> = vec![BTreeSet::new(); replicas];
        let mut issued: Vec<Op> = Vec::new();
        let mut fresh = vec![0u64; replicas];
        for (clock, step) in (1u64..).zip(steps) {
            match step {
                CStep::Base(Step::Local {
                    replica,
                    kind,
                    a,
                    b,
                    pos,
                }) => {
                    let r = usize::from(*replica) % replicas;
                    let (Some(tree), Some(f)) = (trees.get_mut(r), fresh.get_mut(r)) else {
                        continue;
                    };
                    let snap = tree.snapshot();
                    let node_choice = pick(&snap, *a, false);
                    // Skewed physical clocks, merged like a hybrid logical clock: a new id is
                    // above everything this replica has applied, so no future op of this replica
                    // is ordered at or below a frontier it has reached.
                    let skew = (shuffle >> (8 * (r % 8))) & 0x3f;
                    let top = seen[r].iter().next_back().copied().unwrap_or_default();
                    let phys = clock + skew;
                    let (wall_ms, counter) = if phys > top.wall_ms {
                        (phys, 0)
                    } else {
                        (top.wall_ms, top.counter + 1)
                    };
                    let id = Hlc {
                        wall_ms,
                        counter,
                        node: u32::try_from(r).unwrap_or(0),
                    };
                    let op = match kind % 3 {
                        0 => {
                            *f += 1;
                            let node = NodeId(100 * (r as u64 + 1) + *f);
                            Op::new(id, node, pick(&snap, *b, false), u64::from(*pos))
                        }
                        1 if node_choice != NodeId::ROOT => {
                            Op::new(id, node_choice, pick(&snap, *b, true), u64::from(*pos))
                        }
                        _ if node_choice != NodeId::ROOT => Op::delete(id, node_choice),
                        _ => continue,
                    };
                    tree.apply_local(op).map_err(|e| e.to_string())?;
                    seen[r].insert(op.id);
                    issued.push(op);
                }
                CStep::Base(Step::Deliver { to, which }) => {
                    if issued.is_empty() {
                        continue;
                    }
                    let op = issued[usize::from(*which) % issued.len()];
                    let r = usize::from(*to) % replicas;
                    let Some(tree) = trees.get_mut(r) else {
                        continue;
                    };
                    tree.apply(op).map_err(|e| e.to_string())?;
                    seen[r].insert(op.id);
                }
                CStep::Compact(mask) => {
                    let Some(stable) = honest_stable(&issued, &seen) else {
                        continue;
                    };
                    for (r, tree) in trees.iter_mut().enumerate() {
                        if mask & (1 << (r % 8)) != 0 {
                            let before = tree.snapshot();
                            tree.compact(stable).map_err(|e| e.to_string())?;
                            if tree.snapshot() != before {
                                return Err("compaction changed the visible tree".into());
                            }
                        }
                    }
                }
            }
            if trees.iter().any(|t| !t.is_acyclic()) {
                return Err("cycle in a replica".into());
            }
        }
        let expected = oracle(&issued);
        let mut uncompacted = Tree::new();
        for op in &issued {
            uncompacted.apply(*op).map_err(|e| e.to_string())?;
        }
        if uncompacted.snapshot() != expected {
            return Err("uncompacted replica differs from the ordered replay".into());
        }
        for (r, tree) in trees.iter_mut().enumerate() {
            let mut batch: Vec<(u64, Op)> = issued
                .iter()
                .chain(issued.iter())
                .enumerate()
                .map(|(i, op)| {
                    let key = (i as u64 + 1)
                        .wrapping_mul(6_364_136_223_846_793_005)
                        .wrapping_add(shuffle.wrapping_mul(r as u64 + 1))
                        .rotate_left(17);
                    (key, *op)
                })
                .collect();
            batch.sort_by_key(|(k, _)| *k);
            for (_, op) in batch {
                tree.apply(op).map_err(|e| e.to_string())?;
            }
            if tree.snapshot() != expected {
                return Err(format!("compacted replica {r} diverged from the oracle"));
            }
        }
        Ok(())
    }

    proptest! {
        #[test]
        fn compaction_at_honest_stable_points_preserves_convergence(
            replicas in 2usize..5,
            steps in arb_csteps(),
            shuffle in any::<u64>(),
        ) {
            prop_assert_eq!(run_compacting_script(replicas, &steps, shuffle), Ok(()));
        }
    }

    fn chain(count: u64) -> Vec<Op> {
        // Node i hangs under node i-1; wall time i.
        (1..=count)
            .map(|i| {
                let parent = if i == 1 { NodeId::ROOT } else { n(i - 1) };
                Op::new(t(i, 0), n(i), parent, 0)
            })
            .collect()
    }

    #[test]
    fn compaction_never_changes_the_visible_tree_and_shrinks_the_log() {
        let ops = chain(50);
        let mut tree = build(&ops);
        let before = tree.snapshot();
        assert_eq!(tree.compact(t(30, 0)), Ok(30));
        assert_eq!(tree.op_count(), 20);
        assert_eq!(tree.snapshot(), before);
        assert_eq!(tree.stable_frontier(), Some(t(30, 0)));
        // A lower frontier is a no-op; the frontier never moves back.
        assert_eq!(tree.compact(t(10, 0)), Ok(0));
        assert_eq!(tree.stable_frontier(), Some(t(30, 0)));
        // Dropped ops are reported as below the frontier; kept ops are exact duplicates.
        assert_eq!(tree.apply(ops[0]), Ok(Outcome::BelowFrontier));
        assert_eq!(tree.below_frontier_arrivals(), 1);
        assert_eq!(tree.apply(ops[40]), Ok(Outcome::Duplicate));
        assert_eq!(tree.below_frontier_arrivals(), 1);
        assert_eq!(tree.snapshot(), before);
        assert_eq!(tree.op_count(), 20);
        // A late op above the frontier still orders correctly among the kept ops.
        let late = Op::new(t(35, 1), n(5), NodeId::ROOT, 7);
        let mut plain = build(&ops);
        tree.apply(late).unwrap();
        plain.apply(late).unwrap();
        assert_eq!(tree.snapshot(), plain.snapshot());
        // Compacting everything leaves the tree intact and the log empty.
        assert_eq!(tree.compact(t(50, 0)), Ok(21));
        assert_eq!(tree.op_count(), 0);
        assert_eq!(tree.snapshot(), plain.snapshot());
    }

    #[test]
    fn the_op_cap_errors_cleanly_and_compaction_frees_room() {
        let ops = chain(5);
        let mut tree = Tree::with_op_limit(3);
        for op in &ops[..3] {
            tree.apply(*op).unwrap();
        }
        let before = tree.snapshot();
        assert_eq!(
            tree.apply(ops[3]),
            Err(TreeError::OpLimitReached { limit: 3 })
        );
        assert_eq!(tree.snapshot(), before, "a refused op changes nothing");
        assert_eq!(tree.op_count(), 3);
        // Duplicates are still accepted at the cap.
        assert_eq!(tree.apply(ops[0]), Ok(Outcome::Duplicate));
        tree.compact(t(2, 0)).unwrap();
        tree.apply(ops[3]).unwrap();
        tree.apply(ops[4]).unwrap();
        assert_eq!(tree.op_count(), 3);
        assert_eq!(tree.parent(n(5)), Some(n(4)));
        assert_eq!(Tree::new().cap(), DEFAULT_OP_LIMIT);
    }

    fn max_hlc() -> Hlc {
        Hlc {
            wall_ms: u64::MAX,
            counter: u32::MAX,
            node: u32::MAX,
        }
    }

    #[test]
    fn a_nonsense_frontier_is_refused_and_the_maximum_id_is_dropped() {
        let mut tree = build(&chain(5));
        let before = tree.snapshot();
        assert_eq!(tree.compact(max_hlc()), Err(TreeError::FrontierBeyondLog));
        assert_eq!(tree.compact(t(6, 0)), Err(TreeError::FrontierBeyondLog));
        assert_eq!(tree.op_count(), 5);
        assert_eq!(tree.stable_frontier(), None);
        assert_eq!(
            Tree::new().compact(t(1, 0)),
            Err(TreeError::FrontierBeyondLog)
        );
        // An op whose id is the largest possible id must not survive a compaction at that id.
        let top = Op::new(max_hlc(), n(9), NodeId::ROOT, 0);
        tree.apply(top).unwrap();
        assert_eq!(tree.compact(max_hlc()), Ok(6));
        assert_eq!(tree.op_count(), 0);
        assert_eq!(tree.apply(top), Ok(Outcome::BelowFrontier));
        assert_eq!(tree.parent(n(9)), Some(NodeId::ROOT));
        assert!(tree.snapshot().entries.len() > before.entries.len());
        // The frontier is now the maximum: nothing new can be ordered after it.
        assert_eq!(tree.compact(max_hlc()), Ok(0));
    }

    #[test]
    fn local_ops_may_use_the_reserved_headroom_remote_ops_may_not() {
        let ops = chain(20);
        let mut tree = Tree::with_op_limit_and_reserve(16, 2);
        for op in &ops[..14] {
            tree.apply(*op).unwrap();
        }
        let full = tree.snapshot();
        assert_eq!(
            tree.apply(ops[14]),
            Err(TreeError::OpLimitReached { limit: 16 })
        );
        assert_eq!(tree.snapshot(), full, "a refused remote op changes nothing");
        assert_eq!(tree.apply_local(ops[14]), Ok(Outcome::Applied));
        assert_eq!(tree.apply_local(ops[15]), Ok(Outcome::Applied));
        assert_eq!(
            tree.apply_local(ops[16]),
            Err(TreeError::OpLimitReached { limit: 16 })
        );
        // The refused remote op is lost until delivered again; after compaction it goes through.
        tree.compact(t(10, 0)).unwrap();
        assert_eq!(tree.apply(ops[16]), Ok(Outcome::Applied));
        assert_eq!(tree.parent(n(17)), Some(n(16)));
    }

    /// A dishonest frontier (an op below it was never seen) is visible: the late op is reported as
    /// below the frontier, and the replica really does diverge from an uncompacted one.
    #[test]
    fn a_dishonest_frontier_is_detected_by_the_below_frontier_signal() {
        let create = Op::new(t(1, 0), n(1), NodeId::ROOT, 0);
        let late_create = Op::new(t(5, 1), n(2), NodeId::ROOT, 0); // replica 1, not yet delivered
        let into = Op::new(t(9, 0), n(1), n(2), 0); // moves node 1 under node 2
        let mut honest = build(&[create, late_create, into]);
        let mut dishonest = build(&[create, into]);
        // Frontier t(9): replica 1's op at t(5) was never seen here, so the contract is broken.
        dishonest.compact(t(9, 0)).unwrap();
        assert_eq!(dishonest.below_frontier_arrivals(), 0);
        assert_eq!(dishonest.apply(late_create), Ok(Outcome::BelowFrontier));
        assert_eq!(dishonest.below_frontier_arrivals(), 1, "the alarm fires");
        honest.apply(late_create).unwrap();
        assert_ne!(
            dishonest.snapshot(),
            honest.snapshot(),
            "and the replicas really diverge"
        );
        assert_eq!(honest.parent(n(1)), Some(n(2)));
        assert_eq!(dishonest.parent(n(1)), Some(NodeId::ROOT));
    }

    /// Late-op cost before and after compaction; run with `--nocapture` to see the numbers.
    #[test]
    fn late_op_cost_before_and_after_compaction() {
        const BASE: u64 = 2000;
        const LATE: u64 = 100;
        // Base history at even wall times 2, 4, ..., 4000, all created under the root.
        let base: Vec<Op> = (1..=BASE)
            .map(|i| Op::new(t(2 * i, 0), n(i), NodeId::ROOT, 0))
            .collect();
        // Late ops (from a slow replica) at odd wall times spread over `[from, 2 * BASE)`.
        let window = |from: u64| -> Vec<Op> {
            let step = (2 * BASE - from) / LATE;
            (0..LATE)
                .map(|k| {
                    let wall = from + 1 + k * step - (k * step) % 2;
                    Op::new(t(wall, 1), n(k + 1), NodeId::ROOT, 5)
                })
                .collect()
        };
        let run = |compact_at: Option<u64>, late: &[Op]| {
            let mut tree = build(&base);
            if let Some(w) = compact_at {
                tree.compact(t(w, 0)).unwrap();
            }
            let live = tree.op_count();
            let start = tree.replayed_ops();
            for op in late {
                tree.apply(*op).unwrap();
            }
            (live, tree.replayed_ops() - start, tree)
        };
        // Without compaction the slow replica's ops may land anywhere in history.
        let spread = window(0);
        let (live_a, replay_a, tree_a) = run(None, &spread);
        // With an honest frontier at 90% of history, late ops can only land above it.
        let frontier = 2 * BASE * 9 / 10;
        let recent = window(frontier);
        let (live_b, replay_b, tree_b) = run(Some(frontier), &recent);
        let (_, replay_c, _) = run(None, &recent);
        println!(
            "tree compaction, {BASE} base ops, {LATE} late ops\n\
             uncompacted, late ops anywhere in history: live {live_a}, replayed {replay_a}\n\
             uncompacted, late ops above 90%: live {BASE}, replayed {replay_c}\n\
             compacted at 90%, late ops above it: live {live_b}, replayed {replay_b}"
        );
        assert_eq!(live_a as u64, BASE);
        assert_eq!(live_b as u64, BASE / 10);
        assert_eq!(replay_b, replay_c, "same late ops cost the same either way");
        assert!(replay_b < replay_a);
        assert!(tree_a.is_acyclic() && tree_b.is_acyclic());
    }

    #[test]
    fn the_cycle_scenario_passes_the_convergence_check() {
        let steps = cycle_script();
        assert_eq!(run_script(2, &steps, 1), Ok(()));
    }

    /// Both replicas build nodes, then each moves one under the other.
    fn cycle_script() -> Vec<Step> {
        let local = |replica, kind, a, b| Step::Local {
            replica,
            kind,
            a,
            b,
            pos: 0,
        };
        vec![
            local(0, 0, 0, 0), // replica 0 creates node A under root
            Step::Deliver { to: 1, which: 0 },
            local(1, 0, 0, 0), // replica 1 creates node B under root
            Step::Deliver { to: 0, which: 1 },
            // Now both know A and B. Each moves one under the other without hearing of the other.
            local(0, 1, 1, 3), // 0 moves the first node under the second
            local(1, 1, 2, 2), // 1 moves the second node under the first
        ]
    }

    #[cfg(feature = "planted-bugs")]
    #[test]
    fn a_skipped_cycle_check_is_caught_by_the_convergence_check() {
        bugs::clear();
        bugs::enable(Bug::TreeCycleCheckSkipped);
        // Direct witness: two moves that each close a loop with the other.
        let tree = build(&[
            mv(1, 0, 1, NodeId::ROOT),
            mv(1, 1, 2, NodeId::ROOT),
            mv_in(10, 1, 1, 2),
            mv_in(10, 2, 2, 1),
        ]);
        let acyclic = tree.is_acyclic();
        // The scripted two-replica scenario goes through the same check as the property test.
        let scripted = run_script(2, &cycle_script(), 1);
        bugs::clear();
        assert!(!acyclic, "the planted defect must produce a cycle");
        assert!(scripted.is_err(), "the convergence check must notice it");
    }
}

#[cfg(kani)]
mod proofs;

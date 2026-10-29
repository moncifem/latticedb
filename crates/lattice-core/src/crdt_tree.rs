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
//! The operation log is kept whole (needed to undo and redo), so state grows with the number of
//! operations. Compaction needs causal stability and is a later phase.
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
}

const VERSION: u8 = 1;

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
    parents: BTreeMap<NodeId, (NodeId, u64)>,
    /// `(parent, pos, node)`, for ordered children.
    kids: BTreeSet<(NodeId, u64, NodeId)>,
}

impl Tree {
    /// An empty tree: only the two roots exist.
    pub fn new() -> Self {
        Self::default()
    }

    /// Applies an operation in any delivery order, any number of times.
    pub fn apply(&mut self, op: Op) -> Result<Outcome, TreeError> {
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
        let later: Vec<Hlc> = self
            .log
            .range((Bound::Excluded(op.id), Bound::Unbounded))
            .map(|(k, _)| *k)
            .collect();
        for k in later.iter().rev() {
            self.undo(*k);
        }
        let entry = self.perform(op);
        let applied = entry.applied;
        self.log.insert(op.id, entry);
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
            && (bugs::active(Bug::TreeCycleCheckSkipped) || !self.would_cycle(op.node, op.parent));
        if valid {
            self.set(op.node, Some((op.parent, op.pos)));
        }
        Entry {
            op,
            old,
            applied: valid,
        }
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
        }
        if let Some((p, pos)) = to {
            self.parents.insert(node, (p, pos));
            self.kids.insert((p, pos, node));
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

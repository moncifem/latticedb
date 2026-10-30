//! Hostile-peer tests for the CRDT admission limits (threat-model gap G-08).
//!
//! Each limit has a scenario function that returns whether the defence held. The plain test
//! asserts it holds; with the `planted-bugs` feature a second test switches the matching defect on
//! and asserts the scenario notices (returns false), so a limit that silently stopped working
//! fails a test. The property tests drive generated hostile peers against small limits and check
//! what must stay true: refused input changes nothing, and a hostile peer cannot starve an
//! honest one.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use crate::crdt::{Crdt, LwwRegister, OrSet};
use crate::crdt_peer::{BatchError, MAX_BATCH_OPS, MAX_SET_TAGS, PeerContext, PeerError};
use crate::crdt_text::{self as text, ElemId};
use crate::crdt_tree::{self as tree, MAX_TREE_DEPTH, NodeId, TreeError};
use lattice_abi::Hlc;
use proptest::prelude::*;

const NOW: u64 = 1_000;
const DRIFT: u64 = 100;

fn h(wall_ms: u64, node: u32) -> Hlc {
    Hlc {
        wall_ms,
        counter: 0,
        node,
    }
}

fn ctx(node: u32) -> PeerContext {
    PeerContext {
        node,
        now_ms: NOW,
        max_drift_ms: DRIFT,
    }
}

fn head_insert(id: Hlc, s: &str) -> text::Op {
    text::Op::Insert {
        id,
        after: None,
        text: s.to_owned(),
    }
}

fn mv(wall: u64, node: u32, child: u64, parent: u64) -> tree::Op {
    tree::Op::new(h(wall, node), NodeId(child), NodeId(parent), 0)
}

/// Node ids 0 and 1 are the roots; chain links start above them.
fn link(i: u64) -> u64 {
    10 + i
}

// ---- scenarios: true when the defence held --------------------------------------------------

fn node_binding_holds() -> bool {
    let mut d = text::Doc::new();
    let r = d.apply_from(&ctx(1), head_insert(h(10, 2), "x"));
    matches!(
        r,
        Err(text::TextError::Peer(PeerError::ForgedNode {
            claimed: 2,
            bound: 1
        }))
    ) && d.is_empty()
        && d.applied_inserts() == 0
        && d.applied_by_key(2) == 0
}

fn drift_holds() -> bool {
    let mut d = text::Doc::new();
    let r = d.apply_from(&ctx(1), head_insert(h(NOW + DRIFT + 1, 1), "x"));
    matches!(
        r,
        Err(text::TextError::Peer(PeerError::DriftTooLarge { .. }))
    ) && d.applied_inserts() == 0
}

fn batch_limit_holds() -> bool {
    let mut d = text::Doc::new();
    let ops: Vec<text::Op> = (0..=MAX_BATCH_OPS as u32)
        .map(|i| {
            head_insert(
                Hlc {
                    wall_ms: 10,
                    counter: i,
                    node: 1,
                },
                "x",
            )
        })
        .collect();
    let r = d.apply_batch_from(&ctx(1), ops);
    matches!(
        r,
        Err(BatchError {
            applied: 0,
            error: text::TextError::Peer(PeerError::BatchTooLarge { .. })
        })
    ) && d.applied_inserts() == 0
}

fn key_budget_holds() -> bool {
    let mut d = text::Doc::with_limits(text::Limits {
        max_inserts_per_key: 2,
        ..text::Limits::default()
    });
    let hostile = ctx(2);
    let a = d.apply_from(&hostile, head_insert(h(10, 2), "a"));
    let b = d.apply_from(&hostile, head_insert(h(11, 2), "b"));
    let c = d.apply_from(&hostile, head_insert(h(12, 2), "c"));
    let honest = d.apply_from(&ctx(1), head_insert(h(13, 1), "h"));
    a == Ok(text::Outcome::Applied)
        && b == Ok(text::Outcome::Applied)
        && c == Err(text::TextError::KeyBudget(2))
        && honest == Ok(text::Outcome::Applied)
        && d.applied_by_key(2) == 2
}

fn node_limit_holds() -> bool {
    let mut t = tree::Tree::new().with_node_limit(3);
    let p = ctx(1);
    for (i, node) in [10u64, 11, 12].into_iter().enumerate() {
        if t.apply_from(&p, mv(1 + i as u64, 1, node, 0)) != Ok(tree::Outcome::Applied) {
            return false;
        }
    }
    let over = t.apply_from(&p, mv(4, 1, 13, 0));
    // Moving a node that exists is not a new node.
    let known = t.apply_from(&p, mv(5, 1, 10, 11));
    over == Err(TreeError::NodeLimitReached { limit: 3 })
        && known == Ok(tree::Outcome::Applied)
        && t.node_count() == 3
        && !t.contains(NodeId(13))
}

fn depth_limit_holds() -> bool {
    let mut t = tree::Tree::new();
    let p = ctx(1);
    // Chain node `link(i)` sits at depth i.
    for i in 1..=MAX_TREE_DEPTH as u64 {
        let parent = if i == 1 { 0 } else { link(i - 1) };
        if t.apply_from(&p, mv(i, 1, link(i), parent)) != Ok(tree::Outcome::Applied) {
            return false;
        }
    }
    let wall = MAX_TREE_DEPTH as u64 + 1;
    let too_deep = t.apply_from(&p, mv(wall, 1, link(wall), link(wall - 1)));
    // A two-node subtree must not slip past the limit by being moved whole.
    let ok_a = t.apply_from(&p, mv(wall + 1, 1, 5000, 0));
    let ok_b = t.apply_from(&p, mv(wall + 2, 1, 5001, 5000));
    let deep_move = t.apply_from(&p, mv(wall + 3, 1, 5000, link(MAX_TREE_DEPTH as u64 - 1)));
    too_deep == Ok(tree::Outcome::Skipped)
        && !t.contains(NodeId(link(wall)))
        && ok_a == Ok(tree::Outcome::Applied)
        && ok_b == Ok(tree::Outcome::Applied)
        && deep_move == Ok(tree::Outcome::Skipped)
        && t.parent(NodeId(5000)) == Some(NodeId::ROOT)
}

fn origin_quota_holds() -> bool {
    let mut t = tree::Tree::with_op_limit(100).with_origin_quota(3);
    let one = ctx(1);
    for w in 1..=3u64 {
        if t.apply_from(&one, mv(w, 1, 10 + w, 0)) != Ok(tree::Outcome::Applied) {
            return false;
        }
    }
    let over = t.apply_from(&one, mv(4, 1, 20, 0));
    let other = t.apply_from(&ctx(2), mv(5, 2, 21, 0));
    // The replica's own writes are not subject to the share.
    let local = t.apply_local(mv(6, 1, 22, 0));
    let freed = t.compact(h(3, 1)) == Ok(3) && t.apply_from(&one, mv(7, 1, 23, 0)).is_ok();
    over == Err(TreeError::OriginQuota { node: 1, limit: 3 })
        && other == Ok(tree::Outcome::Applied)
        && local == Ok(tree::Outcome::Applied)
        && freed
}

fn set_tag_limit_holds() -> bool {
    let mut s: OrSet<u8> = OrSet::default();
    for i in 0..MAX_SET_TAGS as u64 {
        s.add(0, h(i + 1, 1));
    }
    let p = PeerContext {
        node: 1,
        now_ms: u64::MAX,
        max_drift_ms: 0,
    };
    let new_tag = s.add_from(&p, 0, h(MAX_SET_TAGS as u64 + 1, 1));
    let known_tag = s.add_from(&p, 0, h(1, 1));
    new_tag == Err(PeerError::SetTooLarge { max: MAX_SET_TAGS })
        && known_tag.is_ok()
        && s.tag_count() == MAX_SET_TAGS
}

#[test]
fn every_limit_holds() {
    assert!(node_binding_holds(), "node binding");
    assert!(drift_holds(), "drift");
    assert!(batch_limit_holds(), "batch length");
    assert!(key_budget_holds(), "per key insert budget");
    assert!(node_limit_holds(), "tree node limit");
    assert!(depth_limit_holds(), "tree depth limit");
    assert!(origin_quota_holds(), "tree origin quota");
    assert!(set_tag_limit_holds(), "set tag limit");
}

#[cfg(feature = "planted-bugs")]
#[test]
fn every_planted_peer_defect_is_noticed() {
    use crate::bugs::{self, Bug};
    let scenarios: [(Bug, fn() -> bool); 8] = [
        (Bug::CrdtNodeBindingSkipped, node_binding_holds),
        (Bug::CrdtDriftNotChecked, drift_holds),
        (Bug::CrdtBatchLimitSkipped, batch_limit_holds),
        (Bug::CrdtKeyBudgetSkipped, key_budget_holds),
        (Bug::TreeNodeLimitSkipped, node_limit_holds),
        (Bug::TreeDepthLimitSkipped, depth_limit_holds),
        (Bug::TreeOriginQuotaSkipped, origin_quota_holds),
        (Bug::SetTagLimitSkipped, set_tag_limit_holds),
    ];
    assert_eq!(scenarios.len(), Bug::CRDT_PEER.len());
    for (bug, scenario) in scenarios {
        assert!(Bug::CRDT_PEER.contains(&bug));
        bugs::clear();
        assert!(scenario(), "{bug:?}: control run without the defect");
        bugs::enable(bug);
        let held = scenario();
        bugs::clear();
        assert!(!held, "{bug:?} went unnoticed");
    }
}

// ---- text ------------------------------------------------------------------------------------

#[test]
fn drift_bound_is_inclusive_and_never_blocks_the_past() {
    let mut d = text::Doc::new();
    let p = ctx(1);
    assert_eq!(
        d.apply_from(&p, head_insert(h(NOW + DRIFT, 1), "a")),
        Ok(text::Outcome::Applied)
    );
    assert_eq!(
        d.apply_from(&p, head_insert(h(1, 1), "b")),
        Ok(text::Outcome::Applied)
    );
    for wall in [NOW + DRIFT + 1, u64::MAX] {
        assert!(matches!(
            d.apply_from(&p, head_insert(h(wall, 1), "c")),
            Err(text::TextError::Peer(PeerError::DriftTooLarge { .. }))
        ));
    }
    assert_eq!(d.applied_inserts(), 2);
}

#[test]
fn a_bad_operation_anywhere_in_a_batch_applies_none_of_it() {
    let mut d = text::Doc::new();
    let ops = vec![
        head_insert(h(10, 1), "a"),
        head_insert(h(11, 1), "b"),
        head_insert(h(12, 9), "forged"),
        head_insert(h(13, 1), "c"),
    ];
    let err = d.apply_batch_from(&ctx(1), ops).unwrap_err();
    assert_eq!(err.applied, 0);
    assert!(matches!(
        err.error,
        text::TextError::Peer(PeerError::ForgedNode { .. })
    ));
    assert!(d.is_empty());
    assert_eq!(d.applied_inserts(), 0);
}

#[test]
fn a_batch_stopped_by_replica_state_reports_how_far_it_got() {
    let mut d = text::Doc::with_limits(text::Limits {
        max_inserts_per_key: 2,
        ..text::Limits::default()
    });
    let ops: Vec<text::Op> = (0..4).map(|i| head_insert(h(10 + i, 1), "x")).collect();
    let err = d.apply_batch_from(&ctx(1), ops).unwrap_err();
    assert_eq!(err.applied, 2);
    assert_eq!(err.error, text::TextError::KeyBudget(1));
    assert_eq!(d.applied_inserts(), 2);
    // The sender may resend the whole batch: the two applied are duplicates.
    let again: Vec<text::Op> = (0..2).map(|i| head_insert(h(10 + i, 1), "x")).collect();
    assert_eq!(
        d.apply_batch_from(&ctx(1), again).unwrap(),
        vec![text::Outcome::Duplicate; 2]
    );
}

#[test]
fn a_parked_operation_is_charged_to_the_key_that_sent_it() {
    let mut d = text::Doc::with_limits(text::Limits {
        max_inserts_per_key: 1,
        ..text::Limits::default()
    });
    let anchor = ElemId {
        op: h(1, 9),
        off: 0,
    };
    let after = |id: Hlc| text::Op::Insert {
        id,
        after: Some(anchor),
        text: "x".to_owned(),
    };
    assert_eq!(d.apply_keyed(5, after(h(2, 5))), Ok(text::Outcome::Pending));
    assert_eq!(d.apply_keyed(5, after(h(3, 5))), Ok(text::Outcome::Pending));
    assert_eq!(
        d.apply_keyed(6, head_insert(h(1, 9), "t")),
        Ok(text::Outcome::Applied)
    );
    // Key 5 had room for one of its two parked inserts; the other is dropped and counted.
    assert_eq!(d.applied_by_key(5), 1);
    assert_eq!(d.applied_by_key(6), 1);
    assert_eq!(d.rejected(), 1);
    assert_eq!(d.pending(), 0);
}

#[test]
fn a_duplicate_is_not_charged_to_the_budget() {
    let mut d = text::Doc::with_limits(text::Limits {
        max_inserts_per_key: 1,
        ..text::Limits::default()
    });
    let op = head_insert(h(10, 1), "a");
    assert_eq!(
        d.apply_from(&ctx(1), op.clone()),
        Ok(text::Outcome::Applied)
    );
    assert_eq!(d.apply_from(&ctx(1), op), Ok(text::Outcome::Duplicate));
    assert_eq!(d.applied_by_key(1), 1);
}

// ---- tree ------------------------------------------------------------------------------------

#[test]
fn tree_refusals_leave_the_tree_untouched() {
    let mut t = tree::Tree::new();
    t.apply_from(&ctx(1), mv(5, 1, 10, 0)).unwrap();
    let before = (t.snapshot(), t.op_count(), t.node_count());
    assert!(matches!(
        t.apply_from(&ctx(1), mv(6, 2, 11, 0)),
        Err(TreeError::Peer(PeerError::ForgedNode { .. }))
    ));
    assert!(matches!(
        t.apply_from(&ctx(1), mv(NOW + DRIFT + 1, 1, 11, 0)),
        Err(TreeError::Peer(PeerError::DriftTooLarge { .. }))
    ));
    let batch = [mv(7, 1, 12, 0), mv(8, 1, 13, 0), mv(9, 3, 14, 0)];
    let err = t.apply_batch_from(&ctx(1), &batch).unwrap_err();
    assert_eq!(err.applied, 0);
    let too_long: Vec<tree::Op> = (0..=MAX_BATCH_OPS as u32)
        .map(|i| {
            tree::Op::new(
                Hlc {
                    wall_ms: 10,
                    counter: i,
                    node: 1,
                },
                NodeId(100 + u64::from(i)),
                NodeId::ROOT,
                0,
            )
        })
        .collect();
    assert!(matches!(
        t.apply_batch_from(&ctx(1), &too_long).unwrap_err().error,
        TreeError::Peer(PeerError::BatchTooLarge { .. })
    ));
    assert_eq!((t.snapshot(), t.op_count(), t.node_count()), before);
}

#[test]
fn the_depth_rule_gives_every_delivery_order_the_same_tree() {
    // A chain at the limit, then a late move that cannot fit, delivered in three orders.
    let mut ops: Vec<tree::Op> = (1..=MAX_TREE_DEPTH as u64)
        .map(|i| mv(i, 1, link(i), if i == 1 { 0 } else { link(i - 1) }))
        .collect();
    ops.push(mv(300, 2, 5000, 0));
    ops.push(mv(301, 2, 5001, 5000));
    ops.push(mv(302, 2, 5000, link(MAX_TREE_DEPTH as u64))); // 5001 would be one level too deep
    ops.push(mv(150, 3, link(7), 0)); // a late move that reshapes the chain under ops already applied elsewhere
    let run = |order: Vec<usize>| {
        let mut t = tree::Tree::new();
        for i in order {
            t.apply(ops[i]).unwrap();
        }
        (t.snapshot(), t.is_acyclic())
    };
    let n = ops.len();
    let forward = run((0..n).collect());
    let backward = run((0..n).rev().collect());
    let mut evens_first: Vec<usize> = (0..n).filter(|i| i % 2 == 0).collect();
    evens_first.extend((0..n).filter(|i| i % 2 == 1));
    let mixed = run(evens_first);
    assert!(forward.1);
    assert_eq!(forward, backward);
    assert_eq!(forward, mixed);
}

// ---- register and set ------------------------------------------------------------------------

#[test]
fn a_far_future_timestamp_cannot_pin_a_register() {
    let mut r: LwwRegister<u8> = LwwRegister::default();
    let p = ctx(1);
    assert_eq!(r.set_from(&p, h(10, 1), 1), Ok(()));
    assert!(matches!(
        r.set_from(&p, h(u64::MAX, 1), 2),
        Err(PeerError::DriftTooLarge { .. })
    ));
    assert!(matches!(
        r.set_from(&p, h(11, 2), 3),
        Err(PeerError::ForgedNode { .. })
    ));
    assert_eq!(r.get(), Some(&1));
    assert_eq!(r.set_from(&p, h(11, 1), 4), Ok(()));
    assert_eq!(r.get(), Some(&4));
}

#[test]
fn a_set_merge_is_refused_whole_or_not_at_all() {
    let mut mine: OrSet<u8> = OrSet::default();
    mine.add(1, h(10, 1));
    let mut theirs = mine.clone();
    theirs.add(2, h(20, 2));
    theirs.add(3, h(NOW + DRIFT + 1, 2));
    let before = mine.clone();
    assert!(matches!(
        mine.merge_from(&ctx(2), &theirs),
        Err(PeerError::DriftTooLarge { .. })
    ));
    assert_eq!(mine, before);

    let mut fine = before.clone();
    fine.add(2, h(20, 2));
    fine.remove(&1);
    assert_eq!(mine.merge_from(&ctx(2), &fine), Ok(()));
    assert_eq!(mine.elements(), vec![2]);
    let mut check = before;
    check.merge(&fine);
    assert_eq!(mine, check);
}

#[test]
fn set_add_from_checks_node_and_drift() {
    let mut s: OrSet<u8> = OrSet::default();
    assert_eq!(s.add_from(&ctx(1), 1, h(10, 1)), Ok(()));
    assert!(matches!(
        s.add_from(&ctx(1), 2, h(10, 2)),
        Err(PeerError::ForgedNode { .. })
    ));
    assert!(matches!(
        s.add_from(&ctx(1), 2, h(u64::MAX, 1)),
        Err(PeerError::DriftTooLarge { .. })
    ));
    assert_eq!(s.elements(), vec![1]);
}

// ---- property tests with generated hostile peers ---------------------------------------------

/// A refused-by-construction operation from the hostile peer (node 2).
fn bad_text_op(kind: u8, node_pick: u32, extra: u64, n: u64) -> text::Op {
    let (node, wall) = if kind.is_multiple_of(2) {
        // Forged: a node that is not the peer's.
        ([0u32, 1, 3, u32::MAX][node_pick as usize % 4], 10 + n % 400)
    } else {
        // Future: the peer's own node, past the drift bound.
        (2, (NOW + DRIFT + 1).saturating_add(extra))
    };
    head_insert(
        Hlc {
            wall_ms: wall,
            counter: n as u32,
            node,
        },
        "bad",
    )
}

fn arb_noise() -> impl Strategy<Value = Vec<(bool, u8, u32, u64, u64)>> {
    prop::collection::vec(
        (
            any::<bool>(),
            any::<u8>(),
            any::<u32>(),
            any::<u64>(),
            0u64..400,
        ),
        0..12,
    )
}

fn honest_text_ops(shape: &[(usize, u8)]) -> Vec<text::Op> {
    let mut ops = Vec::new();
    let mut ids: Vec<Hlc> = Vec::new();
    for (i, (pick, len)) in shape.iter().enumerate() {
        let id = h(10 + i as u64, 1);
        let after = if i == 0 || pick % (i + 1) == 0 {
            None
        } else {
            Some(ElemId {
                op: ids[pick % i],
                off: 0,
            })
        };
        ops.push(text::Op::Insert {
            id,
            after,
            text: "a".repeat(1 + usize::from(*len % 4)),
        });
        ids.push(id);
    }
    // Delete the first character of every third insert.
    for (i, id) in ids.iter().enumerate().filter(|(i, _)| i % 3 == 0) {
        ops.push(text::Op::Delete {
            id: h(600 + i as u64, 1),
            target: ElemId { op: *id, off: 0 },
        });
    }
    ops
}

proptest! {
    /// Clean control and hostile run: input the context refuses leaves the document exactly as
    /// the honest peer alone would have left it.
    #[test]
    fn refused_text_input_changes_nothing(
        shape in prop::collection::vec((0usize..64, any::<u8>()), 1..14),
        noise in arb_noise(),
    ) {
        let honest = honest_text_ops(&shape);
        let mut clean = text::Doc::new();
        let mut dirty = text::Doc::new();
        let mut noise = noise.into_iter();
        for op in honest {
            clean.apply_from(&ctx(1), op.clone()).unwrap();
            dirty.apply_from(&ctx(1), op).unwrap();
            if let Some((as_batch, kind, node_pick, extra, n)) = noise.next() {
                let bad = bad_text_op(kind, node_pick, extra, n);
                if as_batch {
                    // Valid-looking operations around the bad one must not slip in.
                    let batch = vec![head_insert(h(20, 2), "ok"), bad, head_insert(h(21, 2), "ok")];
                    let err = dirty.apply_batch_from(&ctx(2), batch).unwrap_err();
                    prop_assert_eq!(err.applied, 0);
                    prop_assert!(matches!(err.error, text::TextError::Peer(_)));
                } else {
                    prop_assert!(matches!(
                        dirty.apply_from(&ctx(2), bad),
                        Err(text::TextError::Peer(_))
                    ));
                }
            }
        }
        prop_assert_eq!(clean.text(), dirty.text());
        prop_assert_eq!(clean.stored(), dirty.stored());
        prop_assert_eq!(clean.pending(), dirty.pending());
        prop_assert_eq!(clean.rejected(), dirty.rejected());
        prop_assert_eq!(clean.applied_inserts(), dirty.applied_inserts());
        prop_assert_eq!(dirty.applied_by_key(2), 0);
        prop_assert_eq!(dirty.applied_by_key(0) + dirty.applied_by_key(3), 0);
    }

    /// A hostile peer that sends only valid operations still cannot use up the document's budget
    /// or an honest peer's share of it.
    #[test]
    fn a_hostile_insert_flood_cannot_starve_an_honest_peer(
        flood in prop::collection::vec(1usize..6, 41..90),
        honest_first in any::<bool>(),
    ) {
        let limits = text::Limits {
            max_elements: 300,
            max_inserts: 100,
            max_inserts_per_key: 40,
        };
        let mut d = text::Doc::with_limits(limits);
        let honest: Vec<text::Op> = (0..30)
            .map(|i| head_insert(h(500 + i, 1), "h"))
            .collect();
        let hostile: Vec<text::Op> = flood
            .iter()
            .enumerate()
            .map(|(i, len)| head_insert(h(10 + i as u64, 2), &"x".repeat(*len)))
            .collect();
        let mut honest_outcomes = Vec::new();
        let mut run_honest = |d: &mut text::Doc| {
            for op in &honest {
                honest_outcomes.push(d.apply_from(&ctx(1), op.clone()));
            }
        };
        if honest_first {
            run_honest(&mut d);
        }
        for op in hostile {
            let _ = d.apply_from(&ctx(2), op);
        }
        if !honest_first {
            run_honest(&mut d);
        }
        prop_assert!(honest_outcomes.iter().all(|o| *o == Ok(text::Outcome::Applied)));
        prop_assert_eq!(d.applied_by_key(2), 40);
        prop_assert_eq!(d.applied_by_key(1), 30);
        prop_assert!(d.applied_inserts() <= 100);
        prop_assert!(d.stored() <= 300);
    }

    /// Hostile tree peer: refused input changes nothing, and a flood of valid moves from one
    /// node stays inside its share of the log and the node cap, leaving room for an honest one.
    #[test]
    fn a_hostile_tree_peer_is_contained(
        flood in prop::collection::vec((2u64..60, 0u64..60, any::<u64>()), 56..90),
        honest_walls in prop::collection::vec(1u64..500, 10),
        junk in arb_noise(),
    ) {
        let mut t = tree::Tree::with_op_limit(64)
            .with_origin_quota(16)
            .with_node_limit(30);
        let mut accepted = 0usize;
        for (i, (node, parent, pos)) in flood.into_iter().enumerate() {
            let op = tree::Op::new(h(600 + i as u64, 2), NodeId(node), NodeId(parent), pos);
            match t.apply_from(&ctx(2), op) {
                Ok(_) => accepted += 1,
                Err(e) => prop_assert!(
                    matches!(e, TreeError::OriginQuota { .. } | TreeError::NodeLimitReached { .. }),
                    "unexpected refusal {e:?}"
                ),
            }
        }
        prop_assert!(accepted <= 16);
        // Refused junk changes nothing.
        let snapshot = (t.snapshot(), t.op_count(), t.node_count());
        for (as_batch, kind, node_pick, extra, n) in junk {
            let bad = if kind.is_multiple_of(2) {
                let node = [0u32, 1, 3, u32::MAX][node_pick as usize % 4];
                mv(10 + n, node, 100 + n, 0)
            } else {
                mv((NOW + DRIFT + 1).saturating_add(extra), 2, 100 + n, 0)
            };
            let result = if as_batch {
                t.apply_batch_from(&ctx(2), &[mv(5, 2, 200, 0), bad])
                    .map(|_| ())
                    .map_err(|e| e.error)
            } else {
                t.apply_from(&ctx(2), bad).map(|_| ())
            };
            prop_assert!(matches!(result, Err(TreeError::Peer(_))));
        }
        prop_assert_eq!((t.snapshot(), t.op_count(), t.node_count()), snapshot);
        // The honest peer still gets all of its writes in.
        for (i, wall) in honest_walls.iter().enumerate() {
            let op = tree::Op::new(
                Hlc { wall_ms: *wall, counter: i as u32, node: 1 },
                NodeId(1000 + i as u64),
                NodeId::ROOT,
                0,
            );
            prop_assert_eq!(t.apply_from(&ctx(1), op), Ok(tree::Outcome::Applied));
        }
        prop_assert!(t.is_acyclic());
        prop_assert!(t.node_count() <= 30);
        prop_assert!(t.op_count() <= 64);
    }
}

// ---- depth rule: convergence, subtree height, origin share defaults --------------------------

fn depth_in(snapshot: &tree::Snapshot, node: NodeId) -> usize {
    let mut cur = node;
    let mut depth = 0;
    while let Some((_, p, _)) = snapshot.entries.iter().find(|(n, _, _)| *n == cur) {
        depth += 1;
        cur = *p;
        assert!(depth <= snapshot.entries.len() + 1, "cycle");
    }
    depth
}

proptest! {
    /// With a small depth limit the rule fires often; every delivery order must agree on the tree,
    /// the tree must respect the limit, and the cached subtree heights must match a recount.
    #[test]
    fn the_depth_rule_converges_under_any_delivery_order(
        moves in prop::collection::vec((2u64..8, 0u64..8, 0u64..3), 2..24),
        orders in prop::collection::vec(prop::collection::vec(any::<u32>(), 24), 5),
    ) {
        const CAP: usize = 3;
        let ops: Vec<tree::Op> = moves
            .iter()
            .enumerate()
            .map(|(i, (n, p, pos))| {
                tree::Op::new(h(10 + i as u64, (i % 3) as u32), NodeId(*n), NodeId(*p), *pos)
            })
            .collect();
        let run = |order: &[usize]| {
            let mut t = tree::Tree::new().with_max_depth(CAP);
            for i in order {
                t.apply(ops[*i]).unwrap();
            }
            t
        };
        let natural: Vec<usize> = (0..ops.len()).collect();
        let reference = run(&natural);
        let snap = reference.snapshot();
        prop_assert!(reference.is_acyclic());
        prop_assert!(reference.heights_are_consistent());
        for (n, _, _) in &snap.entries {
            prop_assert!(depth_in(&snap, *n) <= CAP);
        }
        for keys in &orders {
            let mut order = natural.clone();
            order.sort_by_key(|i| keys[*i]);
            let t = run(&order);
            prop_assert!(t.heights_are_consistent());
            prop_assert_eq!(t.snapshot(), snap.clone());
        }
    }
}

#[test]
fn a_move_of_a_very_wide_subtree_respects_the_depth_boundary() {
    let mut t = tree::Tree::new();
    for i in 1..=255u64 {
        let parent = if i == 1 { 0 } else { link(i - 1) };
        t.apply(mv(i, 1, link(i), parent)).unwrap();
    }
    // A hub with many leaf children: height 1, however many children it has.
    t.apply(mv(300, 2, 9000, 0)).unwrap();
    for c in 0..3000u64 {
        t.apply(mv(301 + c, 2, 10_000 + c, 9000)).unwrap();
    }
    assert!(t.heights_are_consistent());
    // Under the node at depth 254 the hub sits at 255 and its children at 256: allowed.
    assert_eq!(
        t.apply(mv(5000, 3, 9000, link(254))),
        Ok(tree::Outcome::Applied)
    );
    // Moving it under the node at depth 255 would put its children at 257: a no-op.
    assert_eq!(
        t.apply(mv(5001, 3, 9000, link(255))),
        Ok(tree::Outcome::Skipped)
    );
    assert_eq!(t.parent(NodeId(9000)), Some(NodeId(link(254))));
    assert!(t.heights_are_consistent());
    // Moving it away and back keeps the cached heights right.
    t.apply(mv(5002, 3, 9000, 0)).unwrap();
    assert!(t.heights_are_consistent());
}

#[test]
fn trusted_apply_has_no_origin_share_until_one_is_set() {
    let mut t = tree::Tree::with_op_limit(100);
    for w in 1..=50u64 {
        assert!(t.apply(mv(w, 1, 10 + w, 0)).is_ok());
    }
    let mut capped = tree::Tree::with_op_limit(100).with_origin_quota(3);
    for w in 1..=3u64 {
        capped.apply(mv(w, 1, 10 + w, 0)).unwrap();
    }
    assert_eq!(
        capped.apply(mv(4, 1, 20, 0)),
        Err(TreeError::OriginQuota { node: 1, limit: 3 })
    );
}

#[test]
fn the_default_origin_share_is_pinned() {
    use crate::crdt_tree::DEFAULT_ORIGIN_OP_LIMIT;
    let op = |i: usize| {
        tree::Op::new(
            Hlc {
                wall_ms: 10,
                counter: i as u32,
                node: 1,
            },
            NodeId(10 + i as u64),
            NodeId::ROOT,
            0,
        )
    };
    // A peer entry point has a share even when none was set.
    let mut peer = tree::Tree::new();
    for i in 0..DEFAULT_ORIGIN_OP_LIMIT {
        peer.apply_from(&ctx(1), op(i)).unwrap();
    }
    assert_eq!(
        peer.apply_from(&ctx(1), op(DEFAULT_ORIGIN_OP_LIMIT)),
        Err(TreeError::OriginQuota {
            node: 1,
            limit: DEFAULT_ORIGIN_OP_LIMIT
        })
    );
    // Trusted apply takes the same count and one more, because it has no share.
    let mut trusted = tree::Tree::new();
    for i in 0..=DEFAULT_ORIGIN_OP_LIMIT {
        trusted.apply(op(i)).unwrap();
    }
    assert_eq!(trusted.op_count(), DEFAULT_ORIGIN_OP_LIMIT + 1);
}

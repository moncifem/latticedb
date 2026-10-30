//! Kani proofs, run by `.github/workflows/kani.yml`; see `docs/verification.md`.

use super::*;

fn hlc(wall_ms: u64) -> Hlc {
    Hlc {
        wall_ms,
        counter: 0,
        node: 0,
    }
}

/// With node 2 under the root and node 3 under node 2, a later move of node 2 under itself
/// or under its descendant 3, at any sibling position, is recorded as `Skipped` and changes
/// nothing: the snapshot is identical and the tree is still acyclic. Bound: a two-node chain
/// and a single later move; deeper chains and out-of-order delivery are covered by the
/// property tests, not by this proof.
#[kani::proof]
#[kani::solver(cadical)]
#[kani::unwind(8)]
fn move_into_own_subtree_is_a_no_op() {
    let mut tree = Tree::new();
    assert_eq!(
        tree.apply(Op::new(hlc(1), NodeId(2), NodeId::ROOT, 0)),
        Ok(Outcome::Applied)
    );
    assert_eq!(
        tree.apply(Op::new(hlc(2), NodeId(3), NodeId(2), 0)),
        Ok(Outcome::Applied)
    );
    let before = tree.snapshot();
    let target: u64 = kani::any();
    kani::assume(target == 2 || target == 3);
    let pos: u64 = kani::any();
    let outcome = tree.apply(Op::new(hlc(3), NodeId(2), NodeId(target), pos));
    assert_eq!(outcome, Ok(Outcome::Skipped));
    assert_eq!(tree.snapshot(), before);
    assert_eq!(tree.parent(NodeId(2)), Some(NodeId::ROOT));
    assert!(tree.is_acyclic());
}

/// The cycle check itself: in the same two-node chain, `would_cycle(node, parent)` is true
/// exactly when `parent` is `node` or below it, for every pair drawn from nodes 0 to 3.
#[kani::proof]
#[kani::solver(cadical)]
#[kani::unwind(8)]
fn would_cycle_matches_ancestry() {
    let mut tree = Tree::new();
    let _ = tree.apply(Op::new(hlc(1), NodeId(2), NodeId::ROOT, 0));
    let _ = tree.apply(Op::new(hlc(2), NodeId(3), NodeId(2), 0));
    let node: u64 = kani::any();
    let parent: u64 = kani::any();
    kani::assume(node <= 3 && parent <= 3);
    // Ancestors of each node (inclusive) in this tree: 3 -> 2 -> 0; 1 (trash) stands alone.
    let ancestors_inclusive = |n: u64, up: u64| match n {
        3 => up == 3 || up == 2 || up == 0,
        2 => up == 2 || up == 0,
        other => up == other,
    };
    // Moving `node` under `parent` cycles when `node` is `parent` or an ancestor of it.
    assert_eq!(
        tree.would_cycle(NodeId(node), NodeId(parent)),
        ancestors_inclusive(parent, node)
    );
}

//! State-based CRDTs and the laws they must satisfy: merge is commutative, associative and
//! idempotent. Text sequences arrive later (Phase 1 ADR decides build or adopt).

use lattice_abi::Hlc;
use std::collections::{BTreeMap, BTreeSet};

/// A state-based CRDT.
pub trait Crdt: Clone + PartialEq {
    /// Merges `other` into `self`. Must be commutative, associative and idempotent.
    fn merge(&mut self, other: &Self);
}

/// Last-writer-wins register ordered by hybrid logical clock; ties are impossible because the
/// timestamp includes the node id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LwwRegister<T> {
    value: Option<(Hlc, T)>,
}

impl<T> Default for LwwRegister<T> {
    fn default() -> Self {
        Self { value: None }
    }
}

impl<T: Clone + Ord> LwwRegister<T> {
    /// Writes a value at `at`; an older write than the current one is ignored.
    pub fn set(&mut self, at: Hlc, value: T) {
        // Equal timestamps (only possible with forged input) fall back to the value, so merge
        // stays commutative.
        let newer = self
            .value
            .as_ref()
            .is_none_or(|(t, v)| at > *t || (at == *t && value > *v));
        if newer {
            self.value = Some((at, value));
        }
    }

    /// The current value.
    pub fn get(&self) -> Option<&T> {
        self.value.as_ref().map(|(_, v)| v)
    }
}

impl<T: Clone + Ord> Crdt for LwwRegister<T> {
    fn merge(&mut self, other: &Self) {
        if let Some((t, v)) = &other.value {
            self.set(*t, v.clone());
        }
    }
}

/// Observed-remove set: an element is present when some add tag has not been removed. A remove
/// only cancels the adds it has seen, so a concurrent add wins ("add wins").
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OrSet<T: Ord> {
    adds: BTreeMap<T, BTreeSet<Hlc>>,
    removed: BTreeSet<Hlc>,
}

impl<T: Ord + Clone> OrSet<T> {
    /// Adds `value` with a unique tag.
    pub fn add(&mut self, value: T, tag: Hlc) {
        self.adds.entry(value).or_default().insert(tag);
    }

    /// Removes `value`, cancelling every add tag currently visible.
    pub fn remove(&mut self, value: &T) {
        if let Some(tags) = self.adds.get(value) {
            self.removed.extend(tags.iter().copied());
        }
    }

    /// Whether `value` is present.
    pub fn contains(&self, value: &T) -> bool {
        self.adds
            .get(value)
            .is_some_and(|t| t.iter().any(|x| !self.removed.contains(x)))
    }

    /// The present elements in order.
    pub fn elements(&self) -> Vec<T> {
        self.adds
            .keys()
            .filter(|k| self.contains(k))
            .cloned()
            .collect()
    }
}

impl<T: Ord + Clone> Crdt for OrSet<T> {
    fn merge(&mut self, other: &Self) {
        for (k, tags) in &other.adds {
            self.adds
                .entry(k.clone())
                .or_default()
                .extend(tags.iter().copied());
        }
        self.removed.extend(other.removed.iter().copied());
    }
}

/// Identifier of one inserted character. Unique because it is a hybrid logical clock timestamp
/// that includes the node id.
pub type CharId = Hlc;

#[derive(Debug, Clone, PartialEq, Eq)]
struct CharNode {
    ch: char,
    after: Option<CharId>,
    deleted: bool,
}

/// A replicated growable array: a sequence of characters that several replicas edit concurrently.
///
/// Every character remembers the character it was typed after. Siblings (characters typed after
/// the same one) are ordered newest first, which makes concurrent inserts at the same place land in
/// a deterministic order on every replica. Deletes leave a tombstone, so merging is a union of
/// nodes with deleted flags OR-ed, which is commutative, associative and idempotent.
///
/// This is the Phase 1 spike behind the [`Crdt`] trait (see ADR 0010); it is state based and
/// recomputes the text on demand, so it is for correctness work, not for large documents.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Rga {
    nodes: BTreeMap<CharId, CharNode>,
}

impl Rga {
    /// Inserts `ch` with identifier `id` right after `after` (`None` means at the start). Returns
    /// false, changing nothing, if `after` is unknown or `id` is already used.
    pub fn insert(&mut self, after: Option<CharId>, ch: char, id: CharId) -> bool {
        if self.nodes.contains_key(&id) || after.is_some_and(|a| !self.nodes.contains_key(&a)) {
            return false;
        }
        self.nodes.insert(
            id,
            CharNode {
                ch,
                after,
                deleted: false,
            },
        );
        true
    }

    /// Deletes a character (leaves a tombstone). Returns false if the id is unknown.
    pub fn delete(&mut self, id: CharId) -> bool {
        match self.nodes.get_mut(&id) {
            Some(n) => {
                n.deleted = true;
                true
            }
            None => false,
        }
    }

    /// Visible characters in document order, with their ids.
    pub fn chars(&self) -> Vec<(CharId, char)> {
        let mut children: BTreeMap<Option<CharId>, Vec<CharId>> = BTreeMap::new();
        for (id, n) in &self.nodes {
            children.entry(n.after).or_default().push(*id);
        }
        for v in children.values_mut() {
            v.sort_by(|a, b| b.cmp(a)); // newest first
        }
        let mut out = Vec::new();
        let mut stack: Vec<CharId> = children
            .get(&None)
            .map(|v| v.iter().rev().copied().collect())
            .unwrap_or_default();
        while let Some(id) = stack.pop() {
            if let Some(n) = self.nodes.get(&id) {
                if !n.deleted {
                    out.push((id, n.ch));
                }
                if let Some(kids) = children.get(&Some(id)) {
                    stack.extend(kids.iter().rev().copied());
                }
            }
        }
        out
    }

    /// The visible text.
    pub fn text(&self) -> String {
        self.chars().into_iter().map(|(_, c)| c).collect()
    }

    /// Characters stored, including tombstones.
    pub fn stored_chars(&self) -> usize {
        self.nodes.len()
    }
}

impl Crdt for Rga {
    fn merge(&mut self, other: &Self) {
        for (id, n) in &other.nodes {
            match self.nodes.get_mut(id) {
                Some(mine) => mine.deleted |= n.deleted,
                None => {
                    self.nodes.insert(*id, n.clone());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn arb_hlc() -> impl Strategy<Value = Hlc> {
        (0u64..50, 0u32..3, 0u32..3).prop_map(|(w, c, n)| Hlc {
            wall_ms: w,
            counter: c,
            node: n,
        })
    }

    fn arb_lww() -> impl Strategy<Value = LwwRegister<u8>> {
        proptest::collection::vec((arb_hlc(), any::<u8>()), 0..4).prop_map(|ops| {
            let mut r = LwwRegister::default();
            for (t, v) in ops {
                r.set(t, v);
            }
            r
        })
    }

    #[derive(Debug, Clone)]
    enum SetOp {
        Add(u8, Hlc),
        Remove(u8),
    }

    fn arb_orset() -> impl Strategy<Value = OrSet<u8>> {
        proptest::collection::vec(
            prop_oneof![
                (0u8..4, arb_hlc()).prop_map(|(v, t)| SetOp::Add(v, t)),
                (0u8..4).prop_map(SetOp::Remove)
            ],
            0..8,
        )
        .prop_map(|ops| {
            let mut s = OrSet::default();
            for op in ops {
                match op {
                    SetOp::Add(v, t) => s.add(v, t),
                    SetOp::Remove(v) => s.remove(&v),
                }
            }
            s
        })
    }

    fn merged<C: Crdt>(a: &C, b: &C) -> C {
        let mut m = a.clone();
        m.merge(b);
        m
    }

    proptest! {
        #[test]
        fn lww_merge_is_commutative_associative_idempotent(a in arb_lww(), b in arb_lww(), c in arb_lww()) {
            prop_assert_eq!(merged(&a, &b), merged(&b, &a));
            prop_assert_eq!(merged(&merged(&a, &b), &c), merged(&a, &merged(&b, &c)));
            prop_assert_eq!(merged(&a, &a), a);
        }

        #[test]
        fn orset_merge_is_commutative_associative_idempotent(a in arb_orset(), b in arb_orset(), c in arb_orset()) {
            prop_assert_eq!(merged(&a, &b), merged(&b, &a));
            prop_assert_eq!(merged(&merged(&a, &b), &c), merged(&a, &merged(&b, &c)));
            prop_assert_eq!(merged(&a, &a), a);
        }

        #[test]
        fn replicas_converge_in_any_delivery_order(a in arb_orset(), b in arb_orset(), c in arb_orset()) {
            let mut r1 = a.clone(); r1.merge(&b); r1.merge(&c);
            let mut r2 = c.clone(); r2.merge(&a); r2.merge(&b);
            let mut r3 = b.clone(); r3.merge(&c); r3.merge(&a); r3.merge(&a);
            prop_assert_eq!(r1.elements(), r2.elements());
            prop_assert_eq!(r2.elements(), r3.elements());
        }
    }

    /// One replica typing sequentially: every character after the previous one.
    fn typed(r: &mut Rga, node: u32, clock: &mut u64, text: &str) -> Option<CharId> {
        let mut prev = r.chars().last().map(|(id, _)| *id);
        for ch in text.chars() {
            *clock += 1;
            let id = Hlc {
                wall_ms: *clock,
                counter: 0,
                node,
            };
            assert!(r.insert(prev, ch, id));
            prev = Some(id);
        }
        prev
    }

    #[derive(Debug, Clone)]
    enum TextOp {
        Insert { pos: usize, ch: char },
        Delete { pos: usize },
    }

    fn arb_ops() -> impl Strategy<Value = Vec<TextOp>> {
        proptest::collection::vec(
            prop_oneof![
                (0usize..8, proptest::char::range('a', 'e'))
                    .prop_map(|(pos, ch)| TextOp::Insert { pos, ch }),
                (0usize..8).prop_map(|pos| TextOp::Delete { pos }),
            ],
            0..10,
        )
    }

    /// Builds a replica by applying ops on top of a shared base, with ids from `node`.
    fn edit(base: &Rga, node: u32, ops: &[TextOp]) -> Rga {
        let mut r = base.clone();
        for (clock, op) in (1_000 * u64::from(node) + 1..).zip(ops.iter()) {
            let visible = r.chars();
            match op {
                TextOp::Insert { pos, ch } => {
                    let after = if visible.is_empty() || *pos == 0 {
                        None
                    } else {
                        visible
                            .get((pos - 1).min(visible.len() - 1))
                            .map(|(id, _)| *id)
                    };
                    r.insert(
                        after,
                        *ch,
                        Hlc {
                            wall_ms: clock,
                            counter: 0,
                            node,
                        },
                    );
                }
                TextOp::Delete { pos } => {
                    if let Some((id, _)) = visible.get(*pos) {
                        r.delete(*id);
                    }
                }
            }
        }
        r
    }

    proptest! {
        #[test]
        fn rga_merge_is_commutative_associative_idempotent(a in arb_ops(), b in arb_ops(), c in arb_ops()) {
            let mut base = Rga::default();
            let mut clock = 0;
            typed(&mut base, 0, &mut clock, "base");
            let (ra, rb, rc) = (edit(&base, 1, &a), edit(&base, 2, &b), edit(&base, 3, &c));
            prop_assert_eq!(merged(&ra, &rb), merged(&rb, &ra));
            prop_assert_eq!(merged(&merged(&ra, &rb), &rc), merged(&ra, &merged(&rb, &rc)));
            prop_assert_eq!(merged(&ra, &ra), ra);
        }

        #[test]
        fn rga_replicas_converge_to_the_same_text_in_any_order(a in arb_ops(), b in arb_ops(), c in arb_ops()) {
            let mut base = Rga::default();
            let mut clock = 0;
            typed(&mut base, 0, &mut clock, "hello");
            let (ra, rb, rc) = (edit(&base, 1, &a), edit(&base, 2, &b), edit(&base, 3, &c));
            let t1 = merged(&merged(&ra, &rb), &rc).text();
            let t2 = merged(&merged(&rc, &ra), &rb).text();
            let t3 = merged(&rb, &merged(&rc, &merged(&ra, &ra))).text();
            prop_assert_eq!(&t1, &t2);
            prop_assert_eq!(&t2, &t3);
        }
    }

    #[test]
    fn typing_in_order_gives_the_typed_text_and_concurrent_inserts_keep_both() {
        let mut a = Rga::default();
        let mut clock = 0;
        let end = typed(&mut a, 1, &mut clock, "ab");
        assert_eq!(a.text(), "ab");
        let mut b = a.clone();
        // Both replicas type at the same place at the same time.
        let mut ca = 100;
        let mut cb = 100;
        a.insert(
            end,
            'X',
            Hlc {
                wall_ms: {
                    ca += 1;
                    ca
                },
                counter: 0,
                node: 1,
            },
        );
        b.insert(
            end,
            'Y',
            Hlc {
                wall_ms: {
                    cb += 1;
                    cb
                },
                counter: 0,
                node: 2,
            },
        );
        let m1 = merged(&a, &b);
        let m2 = merged(&b, &a);
        assert_eq!(m1.text(), m2.text());
        assert!(m1.text().starts_with("ab") && m1.text().contains('X') && m1.text().contains('Y'));
    }

    #[test]
    fn a_delete_wins_over_a_concurrent_edit_of_the_same_character_and_tombstones_are_kept() {
        let mut a = Rga::default();
        let mut clock = 0;
        let last = typed(&mut a, 1, &mut clock, "abc").unwrap();
        let mut b = a.clone();
        a.delete(last);
        b.insert(
            Some(last),
            'd',
            Hlc {
                wall_ms: 99,
                counter: 0,
                node: 2,
            },
        );
        let m = merged(&a, &b);
        assert_eq!(
            m.text(),
            "abd",
            "the deleted character stays deleted, the concurrent insert after it survives"
        );
        assert_eq!(m.stored_chars(), 4);
        assert!(
            !a.insert(
                None,
                'z',
                Hlc {
                    wall_ms: 1,
                    counter: 0,
                    node: 1
                }
            ),
            "an id cannot be reused"
        );
        assert!(
            !a.insert(
                Some(Hlc {
                    wall_ms: 7777,
                    counter: 0,
                    node: 9
                }),
                'z',
                Hlc {
                    wall_ms: 8000,
                    counter: 0,
                    node: 9
                }
            ),
            "unknown parent"
        );
    }

    #[test]
    fn add_wins_over_a_concurrent_remove() {
        let t1 = Hlc {
            wall_ms: 1,
            counter: 0,
            node: 1,
        };
        let t2 = Hlc {
            wall_ms: 1,
            counter: 0,
            node: 2,
        };
        let mut base = OrSet::default();
        base.add("x", t1);
        let mut a = base.clone();
        a.remove(&"x");
        let mut b = base.clone();
        b.add("x", t2);
        a.merge(&b);
        assert!(a.contains(&"x"));
    }

    #[test]
    fn lww_newest_write_wins() {
        let mut r = LwwRegister::default();
        r.set(
            Hlc {
                wall_ms: 5,
                counter: 0,
                node: 1,
            },
            "new",
        );
        r.set(
            Hlc {
                wall_ms: 3,
                counter: 0,
                node: 1,
            },
            "old",
        );
        assert_eq!(r.get(), Some(&"new"));
    }
}

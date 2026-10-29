//! Column-level merge of replicated row changes (WP42, issue #244, P3-05; RFC 0011).
//!
//! A column carries a [`MergeKind`]: a last-writer-wins register, an observed-remove set or no
//! CRDT. [`merge_rows`] is a pure, deterministic join of two replica states of one row, so a remote
//! row change from the change feed is applied by merging it into the local row. Applying the same
//! set of changes in any order, with any duplication, reaches the same row (invariant I5).
//!
//! This is the in-memory layer only. It is not wired to the row store, the catalog or sync, and it
//! has no byte encoding: no on-disk or wire format changes. Cell values are opaque bytes (the
//! canonical cell bytes of [`crate::typed`] once wired), compared bytewise so ties are total.
//!
//! Semantics, per column kind:
//! - [`MergeKind::Lww`]: the write with the greatest `(Hlc, value bytes)` wins. An HLC includes the
//!   node id, so equal stamps from different nodes cannot happen; equal stamps with different bytes
//!   (forged input) fall back to the larger bytes so the merge stays commutative.
//! - [`MergeKind::OrSet`]: an element is present when one of its add tags is not removed. A remove
//!   cancels only the tags it names, so a concurrent add wins.
//! - [`MergeKind::None`]: not a CRDT. The row's plain columns travel together: the plain columns of
//!   the row with the greater `(plain stamp, plain values)` win as a whole, the same
//!   whole-row last-writer-wins a non-CRDT table gets today.
//! - Row delete: a delete at HLC `d` hides every write whose stamp is `<= d` (delete wins ties).
//!   A write with a greater stamp shows again, even one that is all nulls (the row is then live).
//!   A forged huge delete HLC would hide all later writes, so the caller must reject remote HLCs
//!   beyond a drift bound before merging; this module does not know the clock. Cells are kept, never dropped, so the merge stays a
//!   join of states; reclaiming them is compaction, which is out of scope.

use lattice_abi::Hlc;
use std::collections::{BTreeMap, BTreeSet};

/// Most columns in one row.
pub const MAX_COLUMNS: usize = 256;
/// Longest value or set element, in bytes.
pub const MAX_VALUE_LEN: usize = 65_536;
/// Most distinct elements in one set column.
pub const MAX_SET_ELEMENTS: usize = 1_024;
/// Most add tags, and most removed tags, in one set column.
pub const MAX_SET_TAGS: usize = 4_096;

/// How one column merges.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MergeKind {
    /// Not a CRDT: whole-row last-writer-wins together with the other plain columns.
    None,
    /// Last-writer-wins register ordered by HLC, ties by value bytes.
    Lww,
    /// Observed-remove set of byte strings.
    OrSet,
}

/// Why a merge or a row operation was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeError {
    /// More than [`MAX_COLUMNS`] columns.
    TooManyColumns,
    /// A row's cells do not match the column kinds (count or variant).
    Shape,
    /// A column index outside the row.
    NoSuchColumn(usize),
    /// A value or element longer than [`MAX_VALUE_LEN`].
    ValueTooLarge(usize),
    /// A set with more than [`MAX_SET_ELEMENTS`] elements or [`MAX_SET_TAGS`] tags.
    SetTooLarge(usize),
    /// An operation does not fit the column's kind.
    WrongKind(usize),
    /// A layout override names a column the layout lacks.
    UnknownColumn(String),
    /// A merge kind the column's physical type cannot carry.
    Incompatible(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct SetCell {
    adds: BTreeMap<Vec<u8>, BTreeSet<Hlc>>,
    removed: BTreeSet<Hlc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Cell {
    Plain(Option<Vec<u8>>),
    Lww(Option<(Hlc, Vec<u8>)>),
    Set(SetCell),
}

/// One replica's state of one row: a cell per column, the stamp of the plain columns and the
/// latest delete. A remote change is itself a (sparse) row, so it merges like any other state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeRow {
    plain_stamp: Option<Hlc>,
    deleted_at: Option<Hlc>,
    cells: Vec<Cell>,
}

/// What a reader sees in one column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CellView {
    /// A plain or register value, if visible.
    Value(Option<Vec<u8>>),
    /// The present set elements in byte order.
    Elements(Vec<Vec<u8>>),
}

/// What a reader sees in a row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowView {
    /// Whether anything is visible: false for a deleted row with no later write.
    pub live: bool,
    /// One entry per column.
    pub cells: Vec<CellView>,
}

fn visible(stamp: &Hlc, floor: Option<Hlc>) -> bool {
    floor.is_none_or(|d| *stamp > d)
}

fn check_value(column: usize, v: &[u8]) -> Result<(), MergeError> {
    if v.len() > MAX_VALUE_LEN {
        return Err(MergeError::ValueTooLarge(column));
    }
    Ok(())
}

fn check_set(column: usize, s: &SetCell) -> Result<(), MergeError> {
    let tags: usize = s.adds.values().map(BTreeSet::len).sum();
    if s.adds.len() > MAX_SET_ELEMENTS || tags > MAX_SET_TAGS || s.removed.len() > MAX_SET_TAGS {
        return Err(MergeError::SetTooLarge(column));
    }
    for e in s.adds.keys() {
        check_value(column, e)?;
    }
    Ok(())
}

impl MergeRow {
    /// An empty row for `kinds`.
    pub fn empty(kinds: &[MergeKind]) -> Result<Self, MergeError> {
        if kinds.len() > MAX_COLUMNS {
            return Err(MergeError::TooManyColumns);
        }
        let cells = kinds
            .iter()
            .map(|k| match k {
                MergeKind::None => Cell::Plain(None),
                MergeKind::Lww => Cell::Lww(None),
                MergeKind::OrSet => Cell::Set(SetCell::default()),
            })
            .collect();
        Ok(Self {
            plain_stamp: None,
            deleted_at: None,
            cells,
        })
    }

    /// The stamp of the plain columns, if any were written.
    pub fn plain_stamp(&self) -> Option<Hlc> {
        self.plain_stamp
    }

    /// The latest delete, if any.
    pub fn deleted_at(&self) -> Option<Hlc> {
        self.deleted_at
    }

    /// Writes every plain column at `at`; `values` has one entry per plain column in column order.
    /// A write that loses to the current `(stamp, values)` is ignored. Returns whether it applied.
    pub fn write_plain(
        &mut self,
        at: Hlc,
        values: Vec<Option<Vec<u8>>>,
    ) -> Result<bool, MergeError> {
        let plain_columns: Vec<usize> = self
            .cells
            .iter()
            .enumerate()
            .filter(|(_, c)| matches!(c, Cell::Plain(_)))
            .map(|(i, _)| i)
            .collect();
        if values.len() != plain_columns.len() {
            return Err(MergeError::Shape);
        }
        for (column, v) in plain_columns.iter().zip(&values) {
            if let Some(v) = v {
                check_value(*column, v)?;
            }
        }
        // Same order as the merge: the greater (stamp, values) wins, so a write is never applied
        // by the mutator and refused by the merge, or the other way round.
        let current = plain_key(self);
        let offered: Vec<&Option<Vec<u8>>> = values.iter().collect();
        if (Some(at), offered) < current {
            return Ok(false);
        }
        let mut next = values.into_iter();
        for cell in &mut self.cells {
            if let Cell::Plain(slot) = cell {
                *slot = next.next().ok_or(MergeError::Shape)?;
            }
        }
        self.plain_stamp = Some(at);
        Ok(true)
    }

    /// Writes register column `column` at `at`; an older write than the current one is ignored.
    pub fn write_lww(&mut self, column: usize, at: Hlc, value: Vec<u8>) -> Result<(), MergeError> {
        check_value(column, &value)?;
        match self.cells.get_mut(column) {
            Some(Cell::Lww(slot)) => {
                let newer = slot
                    .as_ref()
                    .is_none_or(|(t, v)| at > *t || (at == *t && value > *v));
                if newer {
                    *slot = Some((at, value));
                }
                Ok(())
            }
            Some(_) => Err(MergeError::WrongKind(column)),
            None => Err(MergeError::NoSuchColumn(column)),
        }
    }

    fn set_cell(&mut self, column: usize) -> Result<&mut SetCell, MergeError> {
        match self.cells.get_mut(column) {
            Some(Cell::Set(s)) => Ok(s),
            Some(_) => Err(MergeError::WrongKind(column)),
            None => Err(MergeError::NoSuchColumn(column)),
        }
    }

    /// Adds `element` to set column `column` under a unique `tag`. The tag is the HLC of the add
    /// change, unique per node by I4; a tag reused for two adds is the caller's bug (removing one
    /// element then also cancels the other). The set is cloned to keep the operation atomic, which
    /// costs O(set size), bounded by [`MAX_SET_TAGS`]; the same holds for the remove operations.
    pub fn set_add(&mut self, column: usize, element: Vec<u8>, tag: Hlc) -> Result<(), MergeError> {
        check_value(column, &element)?;
        let cell = self.set_cell(column)?;
        let mut next = cell.clone();
        next.adds.entry(element).or_default().insert(tag);
        check_set(column, &next)?;
        *cell = next;
        Ok(())
    }

    /// Cancels exactly the named add tags in set column `column`.
    pub fn set_remove_tags(
        &mut self,
        column: usize,
        tags: impl IntoIterator<Item = Hlc>,
    ) -> Result<(), MergeError> {
        let cell = self.set_cell(column)?;
        let mut next = cell.clone();
        next.removed.extend(tags);
        check_set(column, &next)?;
        *cell = next;
        Ok(())
    }

    /// Removes `element`, cancelling the add tags this row currently holds for it (observed remove).
    pub fn set_remove(&mut self, column: usize, element: &[u8]) -> Result<(), MergeError> {
        let cell = self.set_cell(column)?;
        let mut next = cell.clone();
        if let Some(tags) = cell.adds.get(element) {
            next.removed.extend(tags.iter().copied());
        }
        check_set(column, &next)?;
        *cell = next;
        Ok(())
    }

    /// Deletes the row at `at`: every write stamped `<= at` is hidden.
    pub fn delete(&mut self, at: Hlc) {
        self.deleted_at = self.deleted_at.max(Some(at));
    }

    fn check(&self, kinds: &[MergeKind]) -> Result<(), MergeError> {
        if kinds.len() > MAX_COLUMNS {
            return Err(MergeError::TooManyColumns);
        }
        if self.cells.len() != kinds.len() {
            return Err(MergeError::Shape);
        }
        for (i, (cell, kind)) in self.cells.iter().zip(kinds).enumerate() {
            match (cell, kind) {
                (Cell::Plain(v), MergeKind::None) => {
                    if let Some(v) = v {
                        check_value(i, v)?;
                    }
                }
                (Cell::Lww(v), MergeKind::Lww) => {
                    if let Some((_, v)) = v {
                        check_value(i, v)?;
                    }
                }
                (Cell::Set(s), MergeKind::OrSet) => check_set(i, s)?,
                _ => return Err(MergeError::Shape),
            }
        }
        Ok(())
    }

    /// What a reader sees: deleted writes hidden, set tombstones applied.
    pub fn view(&self, kinds: &[MergeKind]) -> Result<RowView, MergeError> {
        self.check(kinds)?;
        let floor = self.deleted_at;
        let plain_visible = self.plain_stamp.is_some_and(|s| visible(&s, floor));
        // A visible plain write makes the row live even when every plain value is null.
        let mut any = plain_visible;
        let mut cells = Vec::with_capacity(self.cells.len());
        for cell in &self.cells {
            cells.push(match cell {
                Cell::Plain(v) => {
                    let shown = if plain_visible { v.clone() } else { None };
                    any |= shown.is_some();
                    CellView::Value(shown)
                }
                Cell::Lww(v) => {
                    let shown = v
                        .as_ref()
                        .filter(|(t, _)| visible(t, floor))
                        .map(|(_, b)| b.clone());
                    any |= shown.is_some();
                    CellView::Value(shown)
                }
                Cell::Set(s) => {
                    let elems: Vec<Vec<u8>> = s
                        .adds
                        .iter()
                        .filter(|(_, tags)| {
                            tags.iter()
                                .any(|t| !s.removed.contains(t) && visible(t, floor))
                        })
                        .map(|(e, _)| e.clone())
                        .collect();
                    any |= !elems.is_empty();
                    CellView::Elements(elems)
                }
            });
        }
        Ok(RowView {
            live: self.deleted_at.is_none() || any,
            cells,
        })
    }
}

fn plain_key(row: &MergeRow) -> (Option<Hlc>, Vec<&Option<Vec<u8>>>) {
    let values = row
        .cells
        .iter()
        .filter_map(|c| match c {
            Cell::Plain(v) => Some(v),
            _ => None,
        })
        .collect();
    (row.plain_stamp, values)
}

/// Merges a remote row state into a local one. Pure and deterministic; commutative, associative
/// and idempotent in `local` and `remote` (so replicas applying the same changes in any order,
/// with duplicates, converge). Both inputs and the result must fit the module bounds, otherwise an
/// error is returned and nothing is merged: the caller keeps the local row and must not treat the
/// change as applied. Convergence is guaranteed for change sets whose union fits the bounds.
///
/// The change's HLC and node id are the stamps inside `remote`; no separate metadata is needed.
pub fn merge_rows(
    kinds: &[MergeKind],
    local: &MergeRow,
    remote: &MergeRow,
) -> Result<MergeRow, MergeError> {
    local.check(kinds)?;
    remote.check(kinds)?;
    let remote_plain_wins = plain_key(remote) > plain_key(local);
    let mut cells = Vec::with_capacity(kinds.len());
    for (l, r) in local.cells.iter().zip(&remote.cells) {
        cells.push(match (l, r) {
            (Cell::Plain(_), Cell::Plain(_)) => {
                if remote_plain_wins {
                    r.clone()
                } else {
                    l.clone()
                }
            }
            (Cell::Lww(a), Cell::Lww(b)) => Cell::Lww(a.clone().max(b.clone())),
            (Cell::Set(a), Cell::Set(b)) => {
                let mut s = a.clone();
                for (e, tags) in &b.adds {
                    s.adds
                        .entry(e.clone())
                        .or_default()
                        .extend(tags.iter().copied());
                }
                s.removed.extend(b.removed.iter().copied());
                Cell::Set(s)
            }
            _ => return Err(MergeError::Shape),
        });
    }
    let merged = MergeRow {
        plain_stamp: local.plain_stamp.max(remote.plain_stamp),
        deleted_at: local.deleted_at.max(remote.deleted_at),
        cells,
    };
    merged.check(kinds)?;
    Ok(merged)
}

/// Merge kinds for a typed layout: every column is [`MergeKind::None`] unless `overrides` names it.
/// A set needs a list column; a register may sit on any column.
#[cfg(feature = "row")]
pub fn kinds_for_layout(
    layout: &crate::typed::TableLayout,
    overrides: &BTreeMap<String, MergeKind>,
) -> Result<Vec<MergeKind>, MergeError> {
    use crate::typed::ColType;
    if let Some(name) = overrides
        .keys()
        .find(|n| !layout.columns.iter().any(|c| &c.name == *n))
    {
        return Err(MergeError::UnknownColumn(name.clone()));
    }
    layout
        .columns
        .iter()
        .map(|c| match overrides.get(&c.name) {
            None => Ok(MergeKind::None),
            Some(MergeKind::OrSet) if !matches!(c.ty, ColType::List(_)) => {
                Err(MergeError::Incompatible(c.name.clone()))
            }
            Some(k) => Ok(*k),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
    use super::*;
    use lattice_core::crdt::{LwwRegister, OrSet};
    use proptest::prelude::*;
    use proptest::strategy::ValueTree;
    use proptest::test_runner::{Config, TestRng, TestRunner};

    const KINDS: [MergeKind; 6] = [
        MergeKind::None,
        MergeKind::Lww,
        MergeKind::OrSet,
        MergeKind::None,
        MergeKind::Lww,
        MergeKind::OrSet,
    ];

    fn hlc(w: u64, c: u32, n: u32) -> Hlc {
        Hlc {
            wall_ms: w,
            counter: c,
            node: n,
        }
    }

    /// One replicated row change, described by what it does (not by row bytes), so an oracle can
    /// compute the expected row from the set of changes without calling `merge_rows`.
    #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
    enum Op {
        Plain(Hlc, Vec<Option<Vec<u8>>>),
        Lww(usize, Hlc, Vec<u8>),
        Add(usize, Vec<u8>, Hlc),
        RemoveTags(usize, Vec<Hlc>),
        Delete(Hlc),
    }

    fn apply_op(row: &mut MergeRow, op: &Op) {
        match op {
            Op::Plain(at, v) => {
                row.write_plain(*at, v.clone()).unwrap();
            }
            Op::Lww(c, at, v) => row.write_lww(*c, *at, v.clone()).unwrap(),
            Op::Add(c, e, t) => row.set_add(*c, e.clone(), *t).unwrap(),
            Op::RemoveTags(c, t) => row.set_remove_tags(*c, t.iter().copied()).unwrap(),
            Op::Delete(at) => row.delete(*at),
        }
    }

    fn change(op: &Op) -> MergeRow {
        let mut r = MergeRow::empty(&KINDS).unwrap();
        apply_op(&mut r, op);
        r
    }

    fn arb_hlc() -> impl Strategy<Value = Hlc> {
        (0u64..6, 0u32..2, 0u32..3).prop_map(|(w, c, n)| hlc(w, c, n))
    }

    fn arb_bytes() -> impl Strategy<Value = Vec<u8>> {
        proptest::collection::vec(0u8..3, 0..3)
    }

    fn arb_op() -> impl Strategy<Value = Op> {
        prop_oneof![
            (
                arb_hlc(),
                proptest::collection::vec(proptest::option::of(arb_bytes()), 2)
            )
                .prop_map(|(t, v)| Op::Plain(t, v)),
            (prop_oneof![Just(1usize), Just(4)], arb_hlc(), arb_bytes())
                .prop_map(|(c, t, v)| Op::Lww(c, t, v)),
            (prop_oneof![Just(2usize), Just(5)], arb_bytes(), arb_hlc())
                .prop_map(|(c, e, t)| Op::Add(c, e, t)),
            (
                prop_oneof![Just(2usize), Just(5)],
                proptest::collection::vec(arb_hlc(), 0..3)
            )
                .prop_map(|(c, t)| Op::RemoveTags(c, t)),
            arb_hlc().prop_map(Op::Delete),
        ]
    }

    fn arb_ops() -> impl Strategy<Value = Vec<Op>> {
        proptest::collection::vec(arb_op(), 0..8)
    }

    fn arb_row() -> impl Strategy<Value = MergeRow> {
        arb_ops().prop_map(|ops| {
            let mut r = MergeRow::empty(&KINDS).unwrap();
            for op in &ops {
                apply_op(&mut r, op);
            }
            r
        })
    }

    /// The expected view computed straight from the definition, from the set of changes.
    fn oracle(ops: &[Op]) -> RowView {
        let floor = ops
            .iter()
            .filter_map(|o| match o {
                Op::Delete(d) => Some(*d),
                _ => None,
            })
            .max();
        let up = |t: &Hlc| floor.is_none_or(|d| *t > d);
        let plain = ops
            .iter()
            .filter_map(|o| match o {
                Op::Plain(t, v) => Some((*t, v.clone())),
                _ => None,
            })
            .max();
        let mut cells = Vec::new();
        let mut plain_index = 0;
        for (i, k) in KINDS.iter().enumerate() {
            cells.push(match k {
                MergeKind::None => {
                    let v = match &plain {
                        Some((t, vals)) if up(t) => vals.get(plain_index).cloned().flatten(),
                        _ => None,
                    };
                    plain_index += 1;
                    CellView::Value(v)
                }
                MergeKind::Lww => {
                    let best = ops
                        .iter()
                        .filter_map(|o| match o {
                            Op::Lww(c, t, v) if *c == i => Some((*t, v.clone())),
                            _ => None,
                        })
                        .max();
                    CellView::Value(best.filter(|(t, _)| up(t)).map(|(_, v)| v))
                }
                MergeKind::OrSet => {
                    let mut elems = BTreeSet::new();
                    for o in ops {
                        if let Op::Add(c, e, t) = o {
                            let cancelled = ops.iter().any(|r| {
                                matches!(r, Op::RemoveTags(rc, ts) if *rc == i && ts.contains(t))
                            });
                            if *c == i && !cancelled && up(t) {
                                elems.insert(e.clone());
                            }
                        }
                    }
                    CellView::Elements(elems.into_iter().collect())
                }
            });
        }
        let any = plain.as_ref().is_some_and(|(t, _)| up(t))
            || cells.iter().any(|c| match c {
                CellView::Value(v) => v.is_some(),
                CellView::Elements(e) => !e.is_empty(),
            });
        RowView {
            live: floor.is_none() || any,
            cells,
        }
    }

    type ViewFn = fn(&MergeRow) -> RowView;
    type MergeFn = fn(&[MergeKind], &MergeRow, &MergeRow) -> Result<MergeRow, MergeError>;

    fn fold(merge: MergeFn, changes: &[MergeRow]) -> MergeRow {
        let mut row = MergeRow::empty(&KINDS).unwrap();
        for c in changes {
            row = merge(&KINDS, &row, c).unwrap();
        }
        row
    }

    /// Which law, if any, a merge function breaks on these three states.
    fn broken_law(
        merge: MergeFn,
        a: &MergeRow,
        b: &MergeRow,
        c: &MergeRow,
    ) -> Option<&'static str> {
        let m = |x: &MergeRow, y: &MergeRow| merge(&KINDS, x, y).unwrap();
        if m(a, b) != m(b, a) {
            return Some("commutativity");
        }
        if m(&m(a, b), c) != m(a, &m(b, c)) {
            return Some("associativity");
        }
        if m(a, a) != *a {
            return Some("idempotence");
        }
        None
    }

    /// Replicas apply the same change set in different shuffled, duplicated orders. Returns false
    /// when their final rows differ from each other or from the oracle.
    fn real_view(r: &MergeRow) -> RowView {
        r.view(&KINDS).unwrap()
    }

    fn converges(merge: MergeFn, view: ViewFn, ops: &[Op], orders: &[Vec<usize>]) -> bool {
        if ops.is_empty() {
            return true;
        }
        let changes: Vec<MergeRow> = ops.iter().map(change).collect();
        let expected = oracle(ops);
        let mut reference: Option<MergeRow> = None;
        for order in orders {
            let seq: Vec<MergeRow> = order
                .iter()
                .map(|i| changes[i % changes.len()].clone())
                .collect();
            // Every change must arrive at least once, whatever the order says.
            let mut all = seq;
            all.extend(changes.iter().cloned());
            let row = fold(merge, &all);
            if view(&row) != expected {
                return false;
            }
            match &reference {
                Some(r) if *r != row => return false,
                Some(_) => {}
                None => reference = Some(row),
            }
        }
        true
    }

    fn real(k: &[MergeKind], a: &MergeRow, b: &MergeRow) -> Result<MergeRow, MergeError> {
        merge_rows(k, a, b)
    }

    // Deliberately wrong variants, built by corrupting the correct result.
    fn wrong_arrival_order_lww(
        k: &[MergeKind],
        a: &MergeRow,
        b: &MergeRow,
    ) -> Result<MergeRow, MergeError> {
        let mut m = merge_rows(k, a, b)?;
        for ((cell, l), r) in m.cells.iter_mut().zip(&a.cells).zip(&b.cells) {
            if let (Cell::Lww(slot), Cell::Lww(x), Cell::Lww(y)) = (cell, l, r) {
                *slot = y.clone().or_else(|| x.clone()); // the later arrival wins
            }
        }
        Ok(m)
    }

    fn wrong_forget_remote_removes(
        k: &[MergeKind],
        a: &MergeRow,
        b: &MergeRow,
    ) -> Result<MergeRow, MergeError> {
        let mut m = merge_rows(k, a, b)?;
        for (cell, l) in m.cells.iter_mut().zip(&a.cells) {
            if let (Cell::Set(s), Cell::Set(x)) = (cell, l) {
                s.removed = x.removed.clone();
            }
        }
        Ok(m)
    }

    fn wrong_toggle_removed(
        k: &[MergeKind],
        a: &MergeRow,
        b: &MergeRow,
    ) -> Result<MergeRow, MergeError> {
        let mut m = merge_rows(k, a, b)?;
        for ((cell, l), r) in m.cells.iter_mut().zip(&a.cells).zip(&b.cells) {
            if let (Cell::Set(s), Cell::Set(x), Cell::Set(y)) = (cell, l, r) {
                s.removed = x
                    .removed
                    .symmetric_difference(&y.removed)
                    .copied()
                    .collect();
            }
        }
        Ok(m)
    }

    fn wrong_average_delete(
        k: &[MergeKind],
        a: &MergeRow,
        b: &MergeRow,
    ) -> Result<MergeRow, MergeError> {
        let mut m = merge_rows(k, a, b)?;
        if let (Some(x), Some(y)) = (a.deleted_at, b.deleted_at) {
            m.deleted_at = Some(Hlc {
                wall_ms: (x.wall_ms + y.wall_ms) / 2,
                ..x.max(y)
            });
        }
        Ok(m)
    }

    fn wrong_delete_loses(
        k: &[MergeKind],
        a: &MergeRow,
        b: &MergeRow,
    ) -> Result<MergeRow, MergeError> {
        // A delete that is lost whenever the other side carries a delete too.
        let mut m = merge_rows(k, a, b)?;
        if a.deleted_at.is_some() && b.deleted_at.is_none() {
            m.deleted_at = None;
        }
        Ok(m)
    }

    proptest! {

        #[test]
        fn merge_rows_is_commutative_associative_idempotent(a in arb_row(), b in arb_row(), c in arb_row()) {
            prop_assert_eq!(broken_law(real, &a, &b, &c), None);
        }

        #[test]
        fn merging_an_empty_row_changes_nothing(a in arb_row()) {
            let e = MergeRow::empty(&KINDS).unwrap();
            prop_assert_eq!(merge_rows(&KINDS, &a, &e).unwrap(), a.clone());
            prop_assert_eq!(merge_rows(&KINDS, &e, &a).unwrap(), a);
        }

        #[test]
        fn replicas_converge_to_the_oracle_in_any_order_with_duplicates(
            ops in arb_ops(),
            orders in proptest::collection::vec(proptest::collection::vec(0usize..16, 0..24), 2..5),
        ) {
            prop_assert!(converges(real, real_view, &ops, &orders));
        }

        #[test]
        fn a_merged_row_views_like_the_oracle_of_both_change_sets(x in arb_ops(), y in arb_ops()) {
            let (mut a, mut b) = (MergeRow::empty(&KINDS).unwrap(), MergeRow::empty(&KINDS).unwrap());
            for o in &x { apply_op(&mut a, o); }
            for o in &y { apply_op(&mut b, o); }
            let all: Vec<Op> = x.iter().chain(&y).cloned().collect();
            let m = merge_rows(&KINDS, &a, &b).unwrap();
            prop_assert_eq!(m.view(&KINDS).unwrap(), oracle(&all));
        }

        /// Without deletes, a single replica's register and set columns agree with the core CRDTs.
        #[test]
        fn single_replica_columns_agree_with_the_core_crdts(
            writes in proptest::collection::vec((arb_hlc(), arb_bytes()), 0..6),
            steps in proptest::collection::vec((any::<bool>(), 0u8..3, arb_hlc()), 0..10),
        ) {
            let mut row = MergeRow::empty(&KINDS).unwrap();
            let mut reg: LwwRegister<Vec<u8>> = LwwRegister::default();
            for (t, v) in &writes {
                row.write_lww(1, *t, v.clone()).unwrap();
                reg.set(*t, v.clone());
            }
            let mut set: OrSet<Vec<u8>> = OrSet::default();
            for (add, e, tag) in &steps {
                let e = vec![*e];
                if *add {
                    row.set_add(2, e.clone(), *tag).unwrap();
                    set.add(e, *tag);
                } else {
                    row.set_remove(2, &e).unwrap();
                    set.remove(&e);
                }
            }
            let view = row.view(&KINDS).unwrap();
            prop_assert_eq!(&view.cells[1], &CellView::Value(reg.get().cloned()));
            prop_assert_eq!(&view.cells[2], &CellView::Elements(set.elements()));
        }
    }

    /// Searches generated states for a law violation of `merge`; deterministic.
    fn find_violation(merge: MergeFn) -> Option<&'static str> {
        let mut runner = TestRunner::new_with_rng(
            Config::default(),
            TestRng::deterministic_rng(proptest::test_runner::RngAlgorithm::ChaCha),
        );
        let strategy = (arb_row(), arb_row(), arb_row());
        for _ in 0..3000 {
            let (a, b, c) = strategy.new_tree(&mut runner).unwrap().current();
            if let Some(law) = broken_law(merge, &a, &b, &c) {
                return Some(law);
            }
        }
        None
    }

    #[test]
    fn the_law_checks_can_fail_for_each_wrong_variant() {
        assert_eq!(find_violation(real), None);
        assert_eq!(
            find_violation(wrong_arrival_order_lww),
            Some("commutativity")
        );
        assert_eq!(
            find_violation(wrong_forget_remote_removes),
            Some("commutativity")
        );
        assert_eq!(find_violation(wrong_toggle_removed), Some("idempotence"));
        assert_eq!(find_violation(wrong_average_delete), Some("associativity"));
        assert_eq!(find_violation(wrong_delete_loses), Some("commutativity"));
    }

    fn wrong_plain_columns_mixed(
        k: &[MergeKind],
        a: &MergeRow,
        b: &MergeRow,
    ) -> Result<MergeRow, MergeError> {
        // Per-column maximum instead of one winner for all plain columns: a valid join, but it
        // mixes values of different writers.
        let mut m = merge_rows(k, a, b)?;
        for ((cell, l), r) in m.cells.iter_mut().zip(&a.cells).zip(&b.cells) {
            if let (Cell::Plain(slot), Cell::Plain(x), Cell::Plain(y)) = (cell, l, r) {
                *slot = x.clone().max(y.clone());
            }
        }
        Ok(m)
    }

    fn wrong_lww_drops_byte_tie_break(
        k: &[MergeKind],
        a: &MergeRow,
        b: &MergeRow,
    ) -> Result<MergeRow, MergeError> {
        let mut m = merge_rows(k, a, b)?;
        for ((cell, l), r) in m.cells.iter_mut().zip(&a.cells).zip(&b.cells) {
            if let (Cell::Lww(slot), Cell::Lww(x), Cell::Lww(y)) = (cell, l, r) {
                let by_stamp = |o: &Option<(Hlc, Vec<u8>)>| o.as_ref().map(|(t, _)| *t);
                *slot = if by_stamp(y) > by_stamp(x) {
                    y.clone()
                } else {
                    x.clone()
                };
            }
        }
        Ok(m)
    }

    fn wrong_plain_stamp_is_min(
        k: &[MergeKind],
        a: &MergeRow,
        b: &MergeRow,
    ) -> Result<MergeRow, MergeError> {
        let mut m = merge_rows(k, a, b)?;
        m.plain_stamp = a.plain_stamp.min(b.plain_stamp);
        Ok(m)
    }

    /// A reader that shows a write stamped exactly at the delete (`>=` instead of `>`).
    fn wrong_view_tie_visible(r: &MergeRow) -> RowView {
        let mut shifted = r.clone();
        // Stamps are compared with `>`, so lowering the delete by one step in the total order is
        // the same as comparing with `>=`; build that predecessor from the HLC fields.
        shifted.deleted_at = r.deleted_at.and_then(|d| {
            if d.node > 0 {
                Some(Hlc {
                    node: d.node - 1,
                    ..d
                })
            } else if d.counter > 0 {
                Some(Hlc {
                    counter: d.counter - 1,
                    node: u32::MAX,
                    ..d
                })
            } else if d.wall_ms > 0 {
                Some(Hlc {
                    wall_ms: d.wall_ms - 1,
                    counter: u32::MAX,
                    node: u32::MAX,
                })
            } else {
                None
            }
        });
        shifted.view(&KINDS).unwrap()
    }

    #[test]
    fn the_convergence_check_can_fail_for_wrong_variants() {
        let mut runner = TestRunner::new_with_rng(
            Config::default(),
            TestRng::deterministic_rng(proptest::test_runner::RngAlgorithm::ChaCha),
        );
        let strategy = (
            arb_ops(),
            proptest::collection::vec(proptest::collection::vec(0usize..16, 0..24), 2..5),
        );
        // (name, merge, view). The first five are the variants the law check also catches.
        let variants: [(&str, MergeFn, ViewFn); 9] = [
            ("arrival-order register", wrong_arrival_order_lww, real_view),
            (
                "dropped remote removes",
                wrong_forget_remote_removes,
                real_view,
            ),
            ("toggling removes", wrong_toggle_removed, real_view),
            ("averaged delete stamp", wrong_average_delete, real_view),
            ("lost delete", wrong_delete_loses, real_view),
            ("plain columns mixed", wrong_plain_columns_mixed, real_view),
            (
                "register without byte tie-break",
                wrong_lww_drops_byte_tie_break,
                real_view,
            ),
            ("plain stamp is min", wrong_plain_stamp_is_min, real_view),
            ("delete shows ties", real, wrong_view_tie_visible),
        ];
        let mut samples = Vec::new();
        for _ in 0..3000 {
            samples.push(strategy.new_tree(&mut runner).unwrap().current());
        }
        assert!(
            samples
                .iter()
                .all(|(o, ord)| converges(real, real_view, o, ord))
        );
        for (name, m, v) in variants {
            let caught = samples.iter().any(|(o, ord)| !converges(m, v, o, ord));
            assert!(caught, "wrong variant {name} was not caught");
        }
    }

    #[test]
    fn a_tag_reused_across_elements_is_a_caller_bug() {
        // Tags must be unique per add (the HLC of the add change, unique per node by I4). If two
        // elements share a tag, removing one cancels the other: the merge cannot tell them apart.
        let k = [MergeKind::OrSet];
        let mut r = MergeRow::empty(&k).unwrap();
        let tag = hlc(1, 0, 1);
        r.set_add(0, b"x".to_vec(), tag).unwrap();
        r.set_add(0, b"y".to_vec(), tag).unwrap();
        r.set_remove(0, b"x").unwrap();
        assert_eq!(r.view(&k).unwrap().cells, vec![CellView::Elements(vec![])]);
    }

    #[test]
    fn a_null_reinsert_after_a_delete_is_live_and_errors_name_the_column() {
        let k = [MergeKind::Lww, MergeKind::None];
        let mut r = MergeRow::empty(&k).unwrap();
        r.delete(hlc(5, 0, 1));
        assert!(!r.view(&k).unwrap().live);
        r.write_plain(hlc(6, 0, 1), vec![None]).unwrap();
        assert!(r.view(&k).unwrap().live);
        // The plain value is column 1, not plain index 0.
        assert_eq!(
            r.write_plain(hlc(7, 0, 1), vec![Some(vec![0; MAX_VALUE_LEN + 1])]),
            Err(MergeError::ValueTooLarge(1))
        );
    }

    #[test]
    fn a_remove_cancels_only_the_adds_it_names_so_a_concurrent_add_wins() {
        let k = [MergeKind::OrSet];
        let mut a = MergeRow::empty(&k).unwrap();
        a.set_add(0, b"x".to_vec(), hlc(1, 0, 1)).unwrap();
        let mut b = a.clone();
        b.set_remove(0, b"x").unwrap(); // replica B removes the add it saw
        a.set_add(0, b"x".to_vec(), hlc(2, 0, 1)).unwrap(); // replica A adds again concurrently
        let m = merge_rows(&k, &a, &b).unwrap();
        assert_eq!(
            m.view(&k).unwrap().cells,
            vec![CellView::Elements(vec![b"x".to_vec()])]
        );
    }

    #[test]
    fn a_delete_hides_older_writes_and_a_later_write_shows_again() {
        let k = [MergeKind::Lww, MergeKind::OrSet];
        let mut base = MergeRow::empty(&k).unwrap();
        base.write_lww(0, hlc(5, 0, 1), b"old".to_vec()).unwrap();
        base.set_add(1, b"e".to_vec(), hlc(5, 1, 1)).unwrap();
        let mut del = MergeRow::empty(&k).unwrap();
        del.delete(hlc(5, 1, 1)); // ties with the set add: delete wins ties
        let gone = merge_rows(&k, &base, &del).unwrap();
        let v = gone.view(&k).unwrap();
        assert_eq!(v.cells[1], CellView::Elements(vec![]));
        // The register write at (5,0,1) is below the delete too.
        assert_eq!(v.cells[0], CellView::Value(None));
        assert!(!v.live);
        let mut later = MergeRow::empty(&k).unwrap();
        later.write_lww(0, hlc(6, 0, 2), b"new".to_vec()).unwrap();
        let back = merge_rows(&k, &gone, &later).unwrap();
        let v = back.view(&k).unwrap();
        assert_eq!(v.cells[0], CellView::Value(Some(b"new".to_vec())));
        assert!(v.live);
    }

    #[test]
    fn plain_columns_move_together_by_the_greater_stamp() {
        let k = [MergeKind::None, MergeKind::None];
        let mut a = MergeRow::empty(&k).unwrap();
        a.write_plain(
            hlc(2, 0, 1),
            vec![Some(b"a1".to_vec()), Some(b"a2".to_vec())],
        )
        .unwrap();
        let mut b = MergeRow::empty(&k).unwrap();
        b.write_plain(hlc(3, 0, 2), vec![Some(b"b1".to_vec()), None])
            .unwrap();
        let m = merge_rows(&k, &a, &b).unwrap();
        assert_eq!(
            m.view(&k).unwrap().cells,
            vec![CellView::Value(Some(b"b1".to_vec())), CellView::Value(None)]
        );
        assert_eq!(merge_rows(&k, &b, &a).unwrap(), m);
    }

    #[test]
    fn bounds_and_shape_are_refused_without_panicking() {
        let k = [MergeKind::Lww, MergeKind::OrSet];
        let mut r = MergeRow::empty(&k).unwrap();
        assert_eq!(
            r.write_lww(0, hlc(1, 0, 1), vec![0; MAX_VALUE_LEN + 1]),
            Err(MergeError::ValueTooLarge(0))
        );
        assert_eq!(
            r.write_lww(1, hlc(1, 0, 1), vec![]),
            Err(MergeError::WrongKind(1))
        );
        assert_eq!(
            r.write_lww(9, hlc(1, 0, 1), vec![]),
            Err(MergeError::NoSuchColumn(9))
        );
        assert_eq!(
            MergeRow::empty(&[MergeKind::Lww; MAX_COLUMNS + 1]),
            Err(MergeError::TooManyColumns)
        );
        // Rows of another shape are refused.
        let other = MergeRow::empty(&[MergeKind::Lww]).unwrap();
        assert_eq!(merge_rows(&k, &r, &other), Err(MergeError::Shape));
        assert_eq!(
            merge_rows(&[MergeKind::Lww, MergeKind::Lww], &r, &r),
            Err(MergeError::Shape)
        );
        // Two sets that fit alone but overflow when united are refused; the inputs are untouched.
        let mut a = MergeRow::empty(&k).unwrap();
        let mut b = MergeRow::empty(&k).unwrap();
        for i in 0..MAX_SET_ELEMENTS {
            a.set_add(1, i.to_le_bytes().to_vec(), hlc(1, 0, 1))
                .unwrap();
            b.set_add(
                1,
                (i + MAX_SET_ELEMENTS).to_le_bytes().to_vec(),
                hlc(1, 0, 2),
            )
            .unwrap();
        }
        let before = a.clone();
        assert_eq!(merge_rows(&k, &a, &b), Err(MergeError::SetTooLarge(1)));
        assert_eq!(a, before);
        assert_eq!(
            a.set_add(1, vec![9; 9], hlc(1, 0, 3)),
            Err(MergeError::SetTooLarge(1))
        );
    }

    #[cfg(feature = "row")]
    #[test]
    fn merge_kinds_attach_to_typed_columns_and_reject_unfit_ones() {
        use crate::typed::{ColType, Column, TableLayout};
        let col = |name: &str, ty| Column {
            name: name.to_string(),
            ty,
            nullable: true,
        };
        let layout = TableLayout {
            type_name: "T".to_string(),
            columns: vec![
                col("a", ColType::Text),
                col("b", ColType::List(Box::new(ColType::Text))),
                col("c", ColType::Int),
            ],
        };
        let mut o = BTreeMap::new();
        o.insert("a".to_string(), MergeKind::Lww);
        o.insert("b".to_string(), MergeKind::OrSet);
        assert_eq!(
            kinds_for_layout(&layout, &o),
            Ok(vec![MergeKind::Lww, MergeKind::OrSet, MergeKind::None])
        );
        o.insert("c".to_string(), MergeKind::OrSet);
        assert_eq!(
            kinds_for_layout(&layout, &o),
            Err(MergeError::Incompatible("c".into()))
        );
        o.remove("c");
        o.insert("zz".to_string(), MergeKind::Lww);
        assert_eq!(
            kinds_for_layout(&layout, &o),
            Err(MergeError::UnknownColumn("zz".into()))
        );
    }
}

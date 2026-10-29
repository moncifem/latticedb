//! Secondary indexes over the row store, derived from the rows and nothing else.
//!
//! An index maps a value extracted from a row to the primary keys of the rows that hold it. Its
//! entries are versioned exactly like rows and are applied at the same commit LSN as the row writes
//! that cause them, in the same step, so a snapshot sees a row and its index entries together or
//! neither (P2-07). Entries are derived, while managed definitions are logged in the catalog's draft
//! compound format. Recovery replays definition and row changes; legacy callback definitions remain
//! an explicit process-local mode. [`Indexes::live_entries`] supports comparison with the rows.

use crate::mvcc::{MvccError, Snapshot, Table};
use crate::rowops::{RowKey, RowWrite};
use lattice_core::ChangeSetId;
use lattice_core::bugs::{self, Bug};
use std::collections::{BTreeMap, BTreeSet};

/// An index definition: which rows it covers and what it keys them by.
#[derive(Clone, Copy)]
pub struct IndexDef {
    /// Identifies the index; unique within a store.
    pub id: u32,
    /// The table whose rows are indexed.
    pub table: u32,
    /// The index key of a row value, or `None` if the row is not indexed.
    pub extract: fn(&[u8]) -> Option<Vec<u8>>,
}

/// Stable extraction operations that can be persisted without executable callbacks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexKey {
    /// Index the complete physical row value.
    WholeValue,
    /// Index exactly this byte window. Values shorter than its end have no entry.
    Slice {
        /// Zero-based byte offset in the physical value.
        offset: u32,
        /// Exact number of bytes in the extracted key; zero selects an empty key.
        length: u32,
    },
}
impl IndexKey {
    /// Rejects a window whose end cannot be represented in its format.
    pub fn is_valid(&self) -> bool {
        match self {
            Self::WholeValue => true,
            Self::Slice { offset, length } => offset.checked_add(*length).is_some(),
        }
    }
    /// Extracts the stable key. This does not interpret an unaccepted typed-row layout.
    pub fn extract(&self, value: &[u8]) -> Option<Vec<u8>> {
        match self {
            Self::WholeValue => Some(value.to_vec()),
            Self::Slice { offset, length } => {
                let end = offset.checked_add(*length)?;
                value
                    .get(*offset as usize..end as usize)
                    .map(<[u8]>::to_vec)
            }
        }
    }
}

/// A declarative index definition retained by the versioned catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedIndexDef {
    /// Never-reused index id, independent of table ids.
    pub id: u32,
    /// Catalog table whose physical rows it covers.
    pub table: u32,
    /// Name, unique among live indexes of this table.
    pub name: String,
    /// Stable physical-byte extraction expression.
    pub key: IndexKey,
}

/// Live managed index definitions keyed by their persistent id.
pub type ManagedIndexes = BTreeMap<u32, ManagedIndexDef>;

enum Definition<'a> {
    Callback(&'a IndexDef),
    Managed(&'a ManagedIndexDef),
}
impl Definition<'_> {
    fn id(&self) -> u32 {
        match self {
            Self::Callback(d) => d.id,
            Self::Managed(d) => d.id,
        }
    }
    fn table(&self) -> u32 {
        match self {
            Self::Callback(d) => d.table,
            Self::Managed(d) => d.table,
        }
    }
    fn extract(&self, value: &[u8]) -> Option<Vec<u8>> {
        match self {
            Self::Callback(d) => (d.extract)(value),
            Self::Managed(d) => d.key.extract(value),
        }
    }
}

type EntryKey = (u32, Vec<u8>);
type EntryWrite = (EntryKey, Option<Vec<u8>>);

/// Live index entries keyed by index id and encoded index/primary key.
pub type IndexEntries = BTreeMap<(u32, Vec<u8>), Vec<u8>>;

/// Order-preserving encoding of an index key: a zero byte is escaped and the key ends with a
/// terminator that sorts below every continuation, so entries sort by index key, then by primary key.
fn encode_key(ikey: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(ikey.len() + 2);
    for &b in ikey {
        out.push(b);
        if b == 0 {
            out.push(0xFF);
        }
    }
    out.extend_from_slice(&[0, 1]);
    out
}

fn entry_key(index: u32, ikey: &[u8], pk: &[u8]) -> EntryKey {
    let mut k = encode_key(ikey);
    k.extend_from_slice(pk);
    (index, k)
}

/// The versioned entries of every index of a store.
#[derive(Clone)]
pub struct Indexes {
    defs: Vec<IndexDef>,
    entries: Table<EntryKey, Vec<u8>>,
    managed_versions: Vec<(u64, ManagedIndexes)>,
}

impl Indexes {
    /// Indexes with the given definitions and no entries.
    pub fn new(defs: Vec<IndexDef>) -> Self {
        Self {
            defs,
            entries: Table::new(),
            managed_versions: Vec::new(),
        }
    }

    /// The table whose visible row must be authorized before returning an index candidate.
    pub(crate) fn table_for(&self, snap: Snapshot, index: u32) -> Option<u32> {
        if let Some(definitions) = self.managed_at(snap) {
            return definitions.get(&index).map(|definition| definition.table);
        }
        self.defs
            .iter()
            .find(|definition| definition.id == index)
            .map(|definition| definition.table)
    }

    /// The index entries a set of row writes changes, given the rows as they are before the writes.
    pub fn changes_for(
        &self,
        rows: &Table<RowKey, Vec<u8>>,
        writes: &[RowWrite],
    ) -> Result<Vec<EntryWrite>, MvccError> {
        self.changes_for_catalog(rows, writes, self.managed_current())
    }

    fn managed_current(&self) -> Option<&ManagedIndexes> {
        self.managed_versions.last().map(|(_, defs)| defs)
    }
    fn managed_at(&self, snap: Snapshot) -> Option<&ManagedIndexes> {
        let end = self
            .managed_versions
            .partition_point(|(lsn, _)| *lsn <= snap.as_of);
        end.checked_sub(1)
            .and_then(|index| self.managed_versions.get(index))
            .map(|(_, defs)| defs)
    }
    fn definitions<'a>(
        &'a self,
        managed: Option<&'a ManagedIndexes>,
    ) -> impl Iterator<Item = Definition<'a>> {
        self.defs.iter().map(Definition::Callback).chain(
            managed
                .into_iter()
                .flat_map(|defs| defs.values())
                .map(Definition::Managed),
        )
    }
    fn changes_for_catalog(
        &self,
        rows: &Table<RowKey, Vec<u8>>,
        writes: &[RowWrite],
        managed: Option<&ManagedIndexes>,
    ) -> Result<Vec<EntryWrite>, MvccError> {
        let before = rows.snapshot();
        // Rows written twice in one change set: the last write wins, so each step starts from the
        // value the previous step left.
        let mut current: BTreeMap<&RowKey, Option<&Vec<u8>>> = BTreeMap::new();
        let mut out = Vec::new();
        for (key, new) in writes {
            let old: Option<&Vec<u8>> = match current.get(key) {
                Some(v) => *v,
                None => rows.get(before, key)?,
            };
            for def in self.definitions(managed).filter(|d| d.table() == key.0) {
                let old_k = old.and_then(|v| def.extract(v));
                let new_k = new.as_ref().and_then(|v| def.extract(v));
                if old_k == new_k {
                    continue;
                }
                if let Some(o) = old_k
                    && !bugs::active(Bug::RowIndexStaleEntries)
                {
                    out.push((entry_key(def.id(), &o, &key.1), None));
                }
                if let Some(n) = new_k
                    && !bugs::active(Bug::RowIndexMissingInsert)
                {
                    out.push((entry_key(def.id(), &n, &key.1), Some(key.1.clone())));
                }
            }
            current.insert(key, new.as_ref());
        }
        Ok(out)
    }

    /// Prepares synchronous build/drop changes and the same transaction's row maintenance.
    /// No state is changed, and no row-history copy is made. Building still visits all live rows.
    pub(crate) fn catalog_changes_for(
        &self,
        rows: &Table<RowKey, Vec<u8>>,
        writes: &[RowWrite],
        next: &ManagedIndexes,
    ) -> Result<Vec<EntryWrite>, MvccError> {
        let before = rows.snapshot();
        let previous = self.managed_current();
        let mut changes = Vec::new();
        let removed: BTreeSet<_> = previous
            .into_iter()
            .flat_map(|defs| defs.keys())
            .filter(|id| !next.contains_key(id))
            .copied()
            .collect();
        if !removed.is_empty() {
            for (key, _) in self.entries.range(before, ..)? {
                if removed.contains(&key.0) {
                    changes.push((key.clone(), None));
                }
            }
        }
        let added: Vec<_> = next
            .values()
            .filter(|def| !previous.is_some_and(|defs| defs.contains_key(&def.id)))
            .collect();
        if !added.is_empty() {
            for ((table, pk), value) in rows.range(before, ..)? {
                for def in added.iter().filter(|def| def.table == *table) {
                    if let Some(key) = def.key.extract(value) {
                        changes.push((entry_key(def.id, &key, pk), Some(pk.clone())));
                    }
                }
            }
        }
        changes.extend(self.changes_for_catalog(rows, writes, Some(next))?);
        Ok(changes)
    }

    /// Publishes prepared entries and the definition version at the same LSN.
    pub(crate) fn apply_catalog(
        &mut self,
        lsn: u64,
        cs: ChangeSetId,
        changes: Vec<EntryWrite>,
        definitions: ManagedIndexes,
    ) -> Result<(), MvccError> {
        self.entries.apply(lsn, cs, changes)?;
        if self.managed_current() != Some(&definitions) {
            self.managed_versions.push((lsn, definitions));
        }
        Ok(())
    }

    /// Applies index changes at the commit LSN of the row writes that caused them.
    pub fn apply(
        &mut self,
        lsn: u64,
        cs: ChangeSetId,
        changes: Vec<EntryWrite>,
    ) -> Result<(), MvccError> {
        self.entries.apply(lsn, cs, changes)
    }

    /// Drops index entry versions no snapshot at or after `horizon` can see; returns how many.
    pub fn vacuum(&mut self, horizon: u64) -> usize {
        self.entries.vacuum(horizon)
    }

    /// The primary keys of the rows whose index key is exactly `ikey`, as of `snap`, in key order.
    pub fn lookup(
        &self,
        snap: Snapshot,
        index: u32,
        ikey: &[u8],
    ) -> Result<Vec<Vec<u8>>, MvccError> {
        if bugs::active(Bug::RowIndexPrefixLookup) {
            let mut prefix = encode_key(ikey);
            prefix.truncate(prefix.len().saturating_sub(2));
            return Ok(self
                .entries
                .range(snap, ..)?
                .into_iter()
                .filter(|((id, encoded), _)| *id == index && encoded.starts_with(&prefix))
                .map(|(_, pk)| pk.clone())
                .collect());
        }
        let lo = (index, encode_key(ikey));
        let mut hi_bytes = lo.1.clone();
        // The terminator ends in 1; bumping it gives the first key past every entry with this prefix.
        if let Some(last) = hi_bytes.last_mut() {
            *last = 2;
        }
        let hi = (index, hi_bytes);
        Ok(self
            .entries
            .range(snap, lo..hi)?
            .into_iter()
            .map(|(_, pk)| pk.clone())
            .collect())
    }

    /// Every live entry of every index as of `snap`, for consistency checks.
    pub fn live_entries(&self, snap: Snapshot) -> Result<IndexEntries, MvccError> {
        Ok(self
            .entries
            .range(snap, ..)?
            .into_iter()
            .map(|((id, k), pk)| ((*id, k.clone()), pk.clone()))
            .collect())
    }

    /// The entries the rows imply at `snap`, computed from scratch.
    pub fn expected_entries(
        &self,
        rows: &Table<RowKey, Vec<u8>>,
        snap: Snapshot,
    ) -> Result<IndexEntries, MvccError> {
        self.expected_entries_catalog(rows, snap, self.managed_at(snap))
    }

    pub(crate) fn expected_entries_catalog(
        &self,
        rows: &Table<RowKey, Vec<u8>>,
        snap: Snapshot,
        managed: Option<&ManagedIndexes>,
    ) -> Result<IndexEntries, MvccError> {
        let mut out = BTreeMap::new();
        for ((table, pk), value) in rows.range(snap, ..)? {
            for def in self.definitions(managed).filter(|d| d.table() == *table) {
                if let Some(ik) = def.extract(value) {
                    out.insert(entry_key(def.id(), &ik, pk), pk.clone());
                }
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )]
    use super::*;

    #[test]
    fn encoded_keys_sort_like_the_keys_and_never_collide_across_lengths() {
        let keys: Vec<Vec<u8>> = vec![
            vec![],
            vec![0],
            vec![0, 0],
            vec![0, 1],
            vec![1],
            vec![1, 0],
            vec![1, 2],
            vec![255],
        ];
        let mut by_key = keys.clone();
        by_key.sort();
        let mut by_encoding = keys;
        by_encoding.sort_by_key(|k| encode_key(k));
        assert_eq!(by_key, by_encoding);
    }

    #[test]
    fn an_entry_for_a_longer_key_is_not_found_by_a_shorter_one() {
        let a = entry_key(1, b"ab", b"pk");
        let b = entry_key(1, b"a", b"bpk");
        assert_ne!(a, b);
        assert!(!a.1.starts_with(&encode_key(b"a")));
    }

    #[test]
    fn consistency_compares_entry_values_as_well_as_keys() {
        let cs = ChangeSetId::from_parts(1, 2, 3);
        let mut rows = Table::new();
        rows.apply(1, cs, vec![((1, b"pk".to_vec()), Some(b"v".to_vec()))])
            .unwrap();
        let mut indexes = Indexes::new(vec![IndexDef {
            id: 2,
            table: 1,
            extract: |v| Some(v.to_vec()),
        }]);
        indexes
            .apply(
                1,
                cs,
                vec![(entry_key(2, b"v", b"pk"), Some(b"wrong-pk".to_vec()))],
            )
            .unwrap();
        let snap = rows.snapshot();
        assert_ne!(
            indexes.live_entries(snap).unwrap(),
            indexes.expected_entries(&rows, snap).unwrap()
        );
    }
}

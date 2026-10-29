//! The catalog: which tables exist, kept in the log like everything else.
//!
//! A catalog change is a change set whose payload starts with the magic `LCAT`. Applying it is one step
//! at one LSN, and the catalog keeps every version, so it can be read as of any earlier LSN. Table ids
//! are never reused, so an old id in a row change set always means the table it meant then.
//!
//! Layout (little endian): magic `LCAT`, a version byte, a `u32` count, then per operation a kind byte
//! (1 create, 2 rename, 3 drop), a `u32` table id and, for create and rename, a `u32` name length and
//! the UTF-8 name. Those version 1 bytes remain unchanged. The separately identified draft `LCIX`
//! version 1 envelope adds managed index definitions and optional row mutations at one commit LSN;
//! it does not reuse the reserved typed-catalog version 2. See RFC 0005 for upgrade restrictions.

use crate::index::{IndexKey, ManagedIndexDef, ManagedIndexes};
use crate::rowops::{self, RowWrite};
use lattice_abi::{Classify, ErrorClass};
use lattice_core::changeset::ChangeSet;
use lattice_core::record::{Record, RecordType};
use lattice_core::{Config, Kernel, Receipt, TxnError};
use lattice_seams::{Clock, Io, Rng};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

/// The first bytes of every catalog payload.
pub const MAGIC: [u8; 4] = *b"LCAT";
const VERSION: u8 = 1;
/// Longest table name in bytes.
pub const MAX_NAME: usize = 255;

/// One catalog change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatalogOp {
    /// Add a table.
    Create {
        /// The new table's id; never used before.
        table: u32,
        /// Its name; unique among live tables.
        name: String,
    },
    /// Change a table's name.
    Rename {
        /// The table.
        table: u32,
        /// The new name; unique among live tables.
        name: String,
    },
    /// Remove a table. Its id is never reused.
    Drop {
        /// The table.
        table: u32,
    },
}

/// Why a catalog change or payload was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatalogError {
    /// The id has been used before.
    IdUsed(u32),
    /// A live table already has this name.
    NameTaken(String),
    /// No live table has this id.
    NoSuchTable(u32),
    /// The name is empty, longer than [`MAX_NAME`], or not valid UTF-8.
    BadName,
    /// Change sets must be applied in increasing LSN order.
    OutOfOrder,
    /// There are no operations.
    Empty,
    /// The bytes end inside a field.
    Truncated,
    /// A kind byte other than 1, 2 or 3.
    BadKind(u8),
    /// A format version this code cannot read.
    UnsupportedVersion(u8),
    /// Bytes remain after the last operation.
    Trailing,
    /// A managed index id was already used, even if the index was dropped.
    IndexIdUsed(u32),
    /// An index does not exist in the current catalog.
    NoSuchIndex(u32),
    /// Another live index of the table has this name.
    IndexNameTaken(String),
    /// A table drop would strand a live index; drop its index in the same compound change.
    TableHasIndexes(u32),
    /// An extraction expression has an overflowing byte window.
    BadIndexKey,
    /// A compound field/count exceeds the bounded format.
    TooLarge,
    /// A nested field is structurally valid but not the canonical compound representation.
    BadCompound(&'static str),
}

impl fmt::Display for CatalogError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

/// Whether `payload` is a catalog payload.
pub fn is_catalog_payload(payload: &[u8]) -> bool {
    payload.get(..4) == Some(MAGIC.as_slice())
}

/// Encodes operations.
pub fn encode(ops: &[CatalogOp]) -> Result<Vec<u8>, CatalogError> {
    if ops.is_empty() {
        return Err(CatalogError::Empty);
    }
    let mut out = MAGIC.to_vec();
    out.push(VERSION);
    out.extend_from_slice(&(ops.len() as u32).to_le_bytes());
    for op in ops {
        let (kind, table, name) = match op {
            CatalogOp::Create { table, name } => (1u8, *table, Some(name)),
            CatalogOp::Rename { table, name } => (2, *table, Some(name)),
            CatalogOp::Drop { table } => (3, *table, None),
        };
        out.push(kind);
        out.extend_from_slice(&table.to_le_bytes());
        if let Some(name) = name {
            if name.is_empty() || name.len() > MAX_NAME {
                return Err(CatalogError::BadName);
            }
            out.extend_from_slice(&(name.len() as u32).to_le_bytes());
            out.extend_from_slice(name.as_bytes());
        }
    }
    Ok(out)
}

struct Reader<'a> {
    rest: &'a [u8],
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], CatalogError> {
        if self.rest.len() < n {
            return Err(CatalogError::Truncated);
        }
        let (head, tail) = self.rest.split_at(n);
        self.rest = tail;
        Ok(head)
    }

    fn u8(&mut self) -> Result<u8, CatalogError> {
        self.take(1)?
            .first()
            .copied()
            .ok_or(CatalogError::Truncated)
    }

    fn u32(&mut self) -> Result<u32, CatalogError> {
        let b: [u8; 4] = self
            .take(4)?
            .try_into()
            .map_err(|_| CatalogError::Truncated)?;
        Ok(u32::from_le_bytes(b))
    }

    fn name(&mut self) -> Result<String, CatalogError> {
        let len = self.u32()? as usize;
        if len == 0 || len > MAX_NAME {
            return Err(CatalogError::BadName);
        }
        String::from_utf8(self.take(len)?.to_vec()).map_err(|_| CatalogError::BadName)
    }
}

/// Decodes a catalog payload; total, with no allocation from an unchecked length.
pub fn decode(payload: &[u8]) -> Result<Vec<CatalogOp>, CatalogError> {
    let mut r = Reader { rest: payload };
    if r.take(4)? != MAGIC.as_slice() {
        return Err(CatalogError::Truncated);
    }
    match r.u8()? {
        VERSION => {}
        v => return Err(CatalogError::UnsupportedVersion(v)),
    }
    let count = r.u32()?;
    if count == 0 {
        return Err(CatalogError::Empty);
    }
    let mut ops = Vec::new();
    for _ in 0..count {
        let kind = r.u8()?;
        let table = r.u32()?;
        ops.push(match kind {
            1 => CatalogOp::Create {
                table,
                name: r.name()?,
            },
            2 => CatalogOp::Rename {
                table,
                name: r.name()?,
            },
            3 => CatalogOp::Drop { table },
            k => return Err(CatalogError::BadKind(k)),
        });
    }
    if !r.rest.is_empty() {
        return Err(CatalogError::Trailing);
    }
    Ok(ops)
}

/// Independent draft envelope magic; it does not reinterpret reserved LCAT version 2.
pub const INDEX_MAGIC: [u8; 4] = *b"LCIX";
/// Maximum table or index operations in one compound change.
pub const MAX_INDEX_OPS: usize = 4096;

/// A definition change. Index ids are never reused; a recreated name receives a fresh id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexOp {
    /// Build an index synchronously from the visible rows, then maintain it transactionally.
    Create(ManagedIndexDef),
    /// Remove the live definition and entries while preserving historical snapshots.
    Drop {
        /// Persistent index id to retire.
        id: u32,
    },
}

/// One atomic metadata and optional row mutation under the draft LCIX v1 envelope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompoundChange {
    /// Existing canonical LCAT v1 table operations, applied before checking index references.
    pub tables: Vec<CatalogOp>,
    /// Managed definition operations, in their submitted order.
    pub indexes: Vec<IndexOp>,
    /// Row mutations, in their submitted order; the last write to a key wins.
    pub rows: Vec<RowWrite>,
}

/// Whether a payload claims the managed-catalog compound format.
pub fn is_compound_payload(payload: &[u8]) -> bool {
    payload.get(..4) == Some(INDEX_MAGIC.as_slice())
}

fn put_block(out: &mut Vec<u8>, bytes: &[u8]) -> Result<(), CatalogError> {
    out.extend_from_slice(
        &u32::try_from(bytes.len())
            .map_err(|_| CatalogError::TooLarge)?
            .to_le_bytes(),
    );
    out.extend_from_slice(bytes);
    Ok(())
}

/// Encodes the draft compound format. Existing LCAT and LROW bytes are unchanged.
pub fn encode_compound(change: &CompoundChange) -> Result<Vec<u8>, CatalogError> {
    if change.tables.is_empty() && change.indexes.is_empty() {
        return Err(CatalogError::Empty);
    }
    if change.tables.len() > MAX_INDEX_OPS || change.indexes.len() > MAX_INDEX_OPS {
        return Err(CatalogError::TooLarge);
    }
    let tables = if change.tables.is_empty() {
        Vec::new()
    } else {
        encode(&change.tables)?
    };
    let rows = if change.rows.is_empty() {
        Vec::new()
    } else {
        rowops::encode(&change.rows).map_err(|_| CatalogError::TooLarge)?
    };
    let mut out = INDEX_MAGIC.to_vec();
    out.push(1);
    put_block(&mut out, &tables)?;
    out.extend_from_slice(
        &u32::try_from(change.indexes.len())
            .map_err(|_| CatalogError::TooLarge)?
            .to_le_bytes(),
    );
    for op in &change.indexes {
        match op {
            IndexOp::Create(def) => {
                if def.name.is_empty() || def.name.len() > MAX_NAME {
                    return Err(CatalogError::BadName);
                }
                if !def.key.is_valid() {
                    return Err(CatalogError::BadIndexKey);
                }
                out.push(1);
                out.extend_from_slice(&def.id.to_le_bytes());
                out.extend_from_slice(&def.table.to_le_bytes());
                put_block(&mut out, def.name.as_bytes())?;
                match def.key {
                    IndexKey::WholeValue => out.push(0),
                    IndexKey::Slice { offset, length } => {
                        out.push(1);
                        out.extend_from_slice(&offset.to_le_bytes());
                        out.extend_from_slice(&length.to_le_bytes());
                    }
                }
            }
            IndexOp::Drop { id } => {
                out.push(2);
                out.extend_from_slice(&id.to_le_bytes());
            }
        }
    }
    put_block(&mut out, &rows)?;
    if out.len() > lattice_core::record::MAX_PAYLOAD {
        return Err(CatalogError::TooLarge);
    }
    Ok(out)
}

/// Strict bounded decoder. A successful value re-encodes to exactly the submitted bytes.
pub fn decode_compound(payload: &[u8]) -> Result<CompoundChange, CatalogError> {
    if payload.len() > lattice_core::record::MAX_PAYLOAD {
        return Err(CatalogError::TooLarge);
    }
    let mut r = Reader { rest: payload };
    if r.take(4)? != INDEX_MAGIC {
        return Err(CatalogError::BadCompound("magic"));
    }
    match r.u8()? {
        1 => {}
        version => return Err(CatalogError::UnsupportedVersion(version)),
    }
    let table_len = r.u32()? as usize;
    let tables = if table_len == 0 {
        Vec::new()
    } else {
        let nested = r.take(table_len)?;
        // Check the declared operation count before the legacy decoder can expand it into values.
        // Keep that decoder's standalone format and error behavior unchanged.
        let mut header = Reader { rest: nested };
        if header.take(4)? != MAGIC.as_slice() {
            return Err(CatalogError::Truncated);
        }
        match header.u8()? {
            VERSION => {}
            version => return Err(CatalogError::UnsupportedVersion(version)),
        }
        if header.u32()? as usize > MAX_INDEX_OPS {
            return Err(CatalogError::TooLarge);
        }
        decode(nested)?
    };
    let count = r.u32()? as usize;
    if count > MAX_INDEX_OPS {
        return Err(CatalogError::TooLarge);
    }
    let mut indexes = Vec::new();
    for _ in 0..count {
        indexes.push(match r.u8()? {
            1 => {
                let id = r.u32()?;
                let table = r.u32()?;
                let name = r.name()?;
                let key = match r.u8()? {
                    0 => IndexKey::WholeValue,
                    1 => IndexKey::Slice {
                        offset: r.u32()?,
                        length: r.u32()?,
                    },
                    other => return Err(CatalogError::BadKind(other)),
                };
                if !key.is_valid() {
                    return Err(CatalogError::BadIndexKey);
                }
                IndexOp::Create(ManagedIndexDef {
                    id,
                    table,
                    name,
                    key,
                })
            }
            2 => IndexOp::Drop { id: r.u32()? },
            other => return Err(CatalogError::BadKind(other)),
        });
    }
    let row_len = r.u32()? as usize;
    let rows = if row_len == 0 {
        Vec::new()
    } else {
        let rows =
            rowops::decode(r.take(row_len)?).map_err(|_| CatalogError::BadCompound("rows"))?;
        if rows.is_empty() {
            return Err(CatalogError::BadCompound("empty row block"));
        }
        rows
    };
    if !r.rest.is_empty() {
        return Err(CatalogError::Trailing);
    }
    if tables.is_empty() && indexes.is_empty() {
        return Err(CatalogError::Empty);
    }
    Ok(CompoundChange {
        tables,
        indexes,
        rows,
    })
}

/// The tables that exist at one version: id to name.
pub type Tables = BTreeMap<u32, String>;

/// Every version of the catalog.
#[derive(Debug, Clone, Default)]
pub struct Catalog {
    /// (LSN of the change, tables right after it), in increasing LSN order.
    versions: Vec<(u64, Tables)>,
    /// Every id ever created, live or dropped.
    used: BTreeSet<u32>,
    index_versions: Vec<(u64, ManagedIndexes)>,
    used_indexes: BTreeSet<u32>,
}

impl Catalog {
    /// An empty catalog.
    pub fn new() -> Self {
        Self::default()
    }

    /// The tables right now.
    pub fn current(&self) -> Tables {
        self.versions
            .last()
            .map(|(_, t)| t.clone())
            .unwrap_or_default()
    }

    /// The tables as of `lsn`: what the catalog said after the last change at or before it.
    pub fn as_of(&self, lsn: u64) -> Tables {
        let end = self.versions.partition_point(|(l, _)| *l <= lsn);
        end.checked_sub(1)
            .and_then(|i| self.versions.get(i))
            .map(|(_, t)| t.clone())
            .unwrap_or_default()
    }

    /// The id of the live table called `name` as of `lsn`.
    pub fn id_of(&self, lsn: u64, name: &str) -> Option<u32> {
        self.as_of(lsn)
            .into_iter()
            .find_map(|(id, n)| (n == name).then_some(id))
    }

    /// The LSN of the newest change, or 0.
    pub fn applied_lsn(&self) -> u64 {
        self.versions.last().map_or(0, |(l, _)| *l)
    }

    /// Checks the operations against the current catalog without changing it.
    fn check_tables(&self, ops: &[CatalogOp]) -> Result<Tables, CatalogError> {
        let mut tables = self.current();
        let mut used = self.used.clone();
        for op in ops {
            match op {
                CatalogOp::Create { table, name } => {
                    if name.is_empty() || name.len() > MAX_NAME {
                        return Err(CatalogError::BadName);
                    }
                    if !used.insert(*table) {
                        return Err(CatalogError::IdUsed(*table));
                    }
                    if tables.values().any(|n| n == name) {
                        return Err(CatalogError::NameTaken(name.clone()));
                    }
                    tables.insert(*table, name.clone());
                }
                CatalogOp::Rename { table, name } => {
                    if name.is_empty() || name.len() > MAX_NAME {
                        return Err(CatalogError::BadName);
                    }
                    if !tables.contains_key(table) {
                        return Err(CatalogError::NoSuchTable(*table));
                    }
                    if tables.iter().any(|(id, n)| n == name && id != table) {
                        return Err(CatalogError::NameTaken(name.clone()));
                    }
                    tables.insert(*table, name.clone());
                }
                CatalogOp::Drop { table } => {
                    if tables.remove(table).is_none() {
                        return Err(CatalogError::NoSuchTable(*table));
                    }
                }
            }
        }
        Ok(tables)
    }

    /// Live managed definitions as of `lsn`; definition history is retained independently of rows.
    pub fn indexes_as_of(&self, lsn: u64) -> ManagedIndexes {
        let end = self.index_versions.partition_point(|(at, _)| *at <= lsn);
        end.checked_sub(1)
            .and_then(|i| self.index_versions.get(i))
            .map(|(_, defs)| defs.clone())
            .unwrap_or_default()
    }

    /// Current managed definitions. This never includes process-local callback definitions.
    pub fn indexes(&self) -> ManagedIndexes {
        self.index_versions
            .last()
            .map(|(_, defs)| defs.clone())
            .unwrap_or_default()
    }

    /// Validates table changes while preserving every live managed index reference.
    pub fn check(&self, ops: &[CatalogOp]) -> Result<Tables, CatalogError> {
        self.check_change(ops, &[]).map(|(tables, _)| tables)
    }

    /// Validates all metadata before any WAL write; only the final table/index state is published.
    pub fn check_change(
        &self,
        ops: &[CatalogOp],
        changes: &[IndexOp],
    ) -> Result<(Tables, ManagedIndexes), CatalogError> {
        let tables = self.check_tables(ops)?;
        let mut indexes = self.indexes();
        let mut used = self.used_indexes.clone();
        for change in changes {
            match change {
                IndexOp::Create(def) => {
                    if !used.insert(def.id) {
                        return Err(CatalogError::IndexIdUsed(def.id));
                    }
                    if def.name.is_empty() || def.name.len() > MAX_NAME {
                        return Err(CatalogError::BadName);
                    }
                    if !def.key.is_valid() {
                        return Err(CatalogError::BadIndexKey);
                    }
                    if !tables.contains_key(&def.table) {
                        return Err(CatalogError::NoSuchTable(def.table));
                    }
                    if indexes
                        .values()
                        .any(|old| old.table == def.table && old.name == def.name)
                    {
                        return Err(CatalogError::IndexNameTaken(def.name.clone()));
                    }
                    indexes.insert(def.id, def.clone());
                }
                IndexOp::Drop { id } => {
                    if indexes.remove(id).is_none() {
                        return Err(CatalogError::NoSuchIndex(*id));
                    }
                }
            }
        }
        if indexes.len() > MAX_INDEX_OPS {
            return Err(CatalogError::TooLarge);
        }
        if let Some(def) = indexes
            .values()
            .find(|def| !tables.contains_key(&def.table))
        {
            return Err(CatalogError::TableHasIndexes(def.table));
        }
        Ok((tables, indexes))
    }

    /// Applies table and index metadata atomically at a single committed LSN.
    pub fn apply_change(
        &mut self,
        lsn: u64,
        ops: &[CatalogOp],
        changes: &[IndexOp],
    ) -> Result<(), CatalogError> {
        if lsn <= self.applied_lsn() {
            return Err(CatalogError::OutOfOrder);
        }
        let (tables, indexes) = self.check_change(ops, changes)?;
        for op in ops {
            if let CatalogOp::Create { table, .. } = op {
                self.used.insert(*table);
            }
        }
        for change in changes {
            if let IndexOp::Create(def) = change {
                self.used_indexes.insert(def.id);
            }
        }
        self.versions.push((lsn, tables));
        self.index_versions.push((lsn, indexes));
        Ok(())
    }

    /// Applies one change set's operations atomically at `lsn`; on error nothing changes.
    pub fn apply(&mut self, lsn: u64, ops: &[CatalogOp]) -> Result<(), CatalogError> {
        self.apply_change(lsn, ops, &[])
    }
}

/// The catalog on top of the kernel: every change is a change set in the shared log.
pub struct CatalogStore<I: Io, C: Clock, R: Rng> {
    kernel: Kernel<I, C, R>,
    catalog: Catalog,
    stale: bool,
}

/// Why a catalog store operation failed.
#[derive(Debug, Clone, PartialEq)]
pub enum StoreError {
    /// The change breaks a catalog rule; nothing was written.
    Invalid(CatalogError),
    /// The kernel refused the change set; nothing was written.
    Rejected(TxnError),
    /// It is unknown whether the change reached the disk; reopen and the log decides.
    InDoubt(TxnError),
    /// An earlier ambiguous failure left this store behind the log; reopen it.
    Stale,
    /// The log holds a catalog change this code cannot apply.
    Corrupt(String),
}

impl Classify for StoreError {
    fn class(&self) -> ErrorClass {
        match self {
            StoreError::Invalid(_) => ErrorClass::Rejected,
            StoreError::Rejected(e) | StoreError::InDoubt(e) => e.class(),
            StoreError::Stale => ErrorClass::Retryable,
            StoreError::Corrupt(_) => ErrorClass::Fatal,
        }
    }
}

impl<I: Io, C: Clock, R: Rng> CatalogStore<I, C, R> {
    /// Opens the log and rebuilds the catalog from its catalog change sets, skipping other engines'.
    pub fn open(io: I, clock: C, rng: R, config: &Config) -> Result<Self, StoreError> {
        let mut catalog = Catalog::new();
        let mut failure: Option<StoreError> = None;
        let (kernel, _, _) = Kernel::open_streaming(io, clock, rng, config, |r: Record| {
            if failure.is_some() || RecordType::from_u8(r.rtype) != Some(RecordType::ChangeSet) {
                return;
            }
            let cs = match ChangeSet::decode(&r.payload) {
                Ok(cs) => cs,
                Err(e) => {
                    failure = Some(StoreError::Corrupt(format!(
                        "change set at lsn {}: {e}",
                        r.lsn
                    )));
                    return;
                }
            };
            if is_compound_payload(&cs.payload) {
                match decode_compound(&cs.payload)
                    .and_then(|change| catalog.apply_change(r.lsn, &change.tables, &change.indexes))
                {
                    Ok(()) => {}
                    Err(e) => {
                        failure = Some(StoreError::Corrupt(format!(
                            "compound catalog at lsn {}: {e}",
                            r.lsn
                        )))
                    }
                }
                return;
            }
            if !is_catalog_payload(&cs.payload) {
                return;
            }
            match decode(&cs.payload) {
                Ok(ops) => {
                    if let Err(e) = catalog.apply(r.lsn, &ops) {
                        failure = Some(StoreError::Corrupt(format!("lsn {}: {e}", r.lsn)));
                    }
                }
                Err(e) => {
                    failure = Some(StoreError::Corrupt(format!(
                        "catalog at lsn {}: {e}",
                        r.lsn
                    )));
                }
            }
        })
        .map_err(StoreError::Rejected)?;
        match failure {
            Some(e) => Err(e),
            None => Ok(Self {
                kernel,
                catalog,
                stale: false,
            }),
        }
    }

    /// The catalog as it stands.
    pub fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    /// Commits table and index metadata without row mutations. RowStore owns synchronous builds.
    pub fn commit_managed(
        &mut self,
        tables: Vec<CatalogOp>,
        indexes: Vec<IndexOp>,
    ) -> Result<u64, StoreError> {
        if self.stale {
            return Err(StoreError::Stale);
        }
        self.catalog
            .check_change(&tables, &indexes)
            .map_err(StoreError::Invalid)?;
        let payload = encode_compound(&CompoundChange {
            tables: tables.clone(),
            indexes: indexes.clone(),
            rows: Vec::new(),
        })
        .map_err(StoreError::Invalid)?;
        let id = self.kernel.new_id();
        match self.kernel.commit(ChangeSet::new(id, payload)) {
            Ok(Receipt::Recorded { lsn, .. }) => {
                if let Err(error) = self.catalog.apply_change(lsn, &tables, &indexes) {
                    self.stale = true;
                    return Err(StoreError::Corrupt(format!("apply after commit: {error}")));
                }
                Ok(lsn)
            }
            Ok(Receipt::Duplicate) => {
                self.stale = true;
                Err(StoreError::Corrupt(
                    "a fresh change set id was a duplicate".into(),
                ))
            }
            Err(error) if error.class() == ErrorClass::Rejected => Err(StoreError::Rejected(error)),
            Err(error) => {
                self.stale = true;
                Err(StoreError::InDoubt(error))
            }
        }
    }

    /// Commits catalog operations as one change set and returns its LSN.
    pub fn commit(&mut self, ops: Vec<CatalogOp>) -> Result<u64, StoreError> {
        if self.stale {
            return Err(StoreError::Stale);
        }
        self.catalog.check(&ops).map_err(StoreError::Invalid)?;
        let payload = encode(&ops).map_err(StoreError::Invalid)?;
        let id = self.kernel.new_id();
        match self.kernel.commit(ChangeSet::new(id, payload)) {
            Ok(Receipt::Recorded { lsn, .. }) => {
                self.catalog
                    .apply(lsn, &ops)
                    .map_err(|e| StoreError::Corrupt(format!("apply after commit: {e}")))?;
                Ok(lsn)
            }
            Ok(Receipt::Duplicate) => {
                self.stale = true;
                Err(StoreError::Corrupt(
                    "a fresh change set id was a duplicate".into(),
                ))
            }
            Err(e) if e.class() == ErrorClass::Rejected => Err(StoreError::Rejected(e)),
            Err(e) => {
                self.stale = true;
                Err(StoreError::InDoubt(e))
            }
        }
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
    use proptest::prelude::*;

    fn create(table: u32, name: &str) -> CatalogOp {
        CatalogOp::Create {
            table,
            name: name.into(),
        }
    }

    fn op() -> impl Strategy<Value = CatalogOp> {
        let name = "[a-z]{1,12}";
        prop_oneof![
            (any::<u32>(), name).prop_map(|(table, name)| CatalogOp::Create { table, name }),
            (any::<u32>(), name).prop_map(|(table, name)| CatalogOp::Rename { table, name }),
            any::<u32>().prop_map(|table| CatalogOp::Drop { table }),
        ]
    }

    proptest! {
        #[test]
        fn encode_then_decode_is_identity(ops in proptest::collection::vec(op(), 1..8)) {
            prop_assert_eq!(decode(&encode(&ops).unwrap()), Ok(ops));
        }

        #[test]
        fn arbitrary_bytes_never_panic_and_a_success_re_encodes_identically(bytes in proptest::collection::vec(any::<u8>(), 0..200)) {
            if let Ok(ops) = decode(&bytes) {
                prop_assert_eq!(encode(&ops).unwrap(), bytes);
            }
        }

        #[test]
        fn every_truncation_is_an_error(ops in proptest::collection::vec(op(), 1..5)) {
            let bytes = encode(&ops).unwrap();
            for cut in 0..bytes.len() {
                prop_assert!(decode(&bytes[..cut]).is_err(), "cut at {}", cut);
            }
        }
    }

    #[test]
    fn the_catalog_can_be_read_as_of_any_earlier_lsn() {
        let mut c = Catalog::new();
        c.apply(3, &[create(1, "issues"), create(2, "pages")])
            .unwrap();
        c.apply(
            7,
            &[CatalogOp::Rename {
                table: 1,
                name: "tickets".into(),
            }],
        )
        .unwrap();
        c.apply(9, &[CatalogOp::Drop { table: 2 }]).unwrap();
        assert!(c.as_of(2).is_empty());
        assert_eq!(c.as_of(3).len(), 2);
        assert_eq!(c.as_of(6).get(&1).map(String::as_str), Some("issues"));
        assert_eq!(c.as_of(7).get(&1).map(String::as_str), Some("tickets"));
        assert_eq!(c.id_of(8, "pages"), Some(2));
        assert_eq!(c.id_of(9, "pages"), None);
        assert_eq!(c.current().len(), 1);
    }

    #[test]
    fn ids_are_never_reused_and_names_are_unique_among_live_tables() {
        let mut c = Catalog::new();
        c.apply(1, &[create(1, "a")]).unwrap();
        assert_eq!(c.apply(2, &[create(1, "b")]), Err(CatalogError::IdUsed(1)));
        assert_eq!(
            c.apply(2, &[create(2, "a")]),
            Err(CatalogError::NameTaken("a".into()))
        );
        c.apply(2, &[CatalogOp::Drop { table: 1 }]).unwrap();
        assert_eq!(
            c.apply(3, &[create(1, "a")]),
            Err(CatalogError::IdUsed(1)),
            "a dropped id stays used"
        );
        c.apply(3, &[create(2, "a")]).unwrap();
    }

    #[test]
    fn a_failing_change_set_changes_nothing() {
        let mut c = Catalog::new();
        c.apply(1, &[create(1, "a")]).unwrap();
        let before = c.current();
        assert!(
            c.apply(2, &[create(2, "b"), CatalogOp::Drop { table: 9 }])
                .is_err()
        );
        assert_eq!(c.current(), before);
        assert_eq!(c.applied_lsn(), 1);
        assert_eq!(c.apply(1, &[create(5, "x")]), Err(CatalogError::OutOfOrder));
    }

    #[test]
    fn a_rename_may_swap_into_a_name_freed_in_the_same_change_set() {
        let mut c = Catalog::new();
        c.apply(1, &[create(1, "a"), create(2, "b")]).unwrap();
        // Free "a" first, then take it: valid because operations apply in order.
        c.apply(
            2,
            &[
                CatalogOp::Rename {
                    table: 1,
                    name: "tmp".into(),
                },
                CatalogOp::Rename {
                    table: 2,
                    name: "a".into(),
                },
            ],
        )
        .unwrap();
        assert_eq!(c.id_of(2, "a"), Some(2));
        assert_eq!(c.id_of(2, "tmp"), Some(1));
    }

    #[test]
    fn bad_names_and_payloads_are_refused() {
        assert_eq!(encode(&[create(1, "")]), Err(CatalogError::BadName));
        assert_eq!(
            encode(&[create(1, &"x".repeat(MAX_NAME + 1))]),
            Err(CatalogError::BadName)
        );
        assert_eq!(encode(&[]), Err(CatalogError::Empty));
        let mut bytes = encode(&[CatalogOp::Drop { table: 1 }]).unwrap();
        bytes.push(0);
        assert_eq!(decode(&bytes), Err(CatalogError::Trailing));
        assert!(!is_catalog_payload(b"LROW"));
    }
}

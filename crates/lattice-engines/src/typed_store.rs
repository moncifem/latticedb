//! Proposed persisted typed tables over one row store and its catalog/WAL writer.
//! RFC 0001 / ADR 0015 remain draft/proposed; this implementation does not accept their formats.
//!
//! Catalog entries persist canonical LSL, stable table ids and immutable schema versions. Each row
//! value carries its writer schema version so old values decode with their original layout after
//! updates and process recovery. This does not backfill old rows or perform online schema migration.

use crate::catalog::{Catalog, CatalogError, CatalogOp};
use crate::limits::{AllowAll, PolicyEvaluator};
use crate::mvcc::Snapshot;
use crate::store::{RowError, RowStore, RowTx};
use crate::typed::{self, TypedError};
use lattice_abi::{Classify, ErrorClass};
use lattice_core::Config;
use lattice_lsl::validate::Value;
use lattice_lsl::{check, format, parser, validate};
use lattice_seams::{Clock, Io, Rng};
use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

const ROW_MAGIC: &[u8; 4] = b"LTYP";
const ROW_VERSION: u8 = 1;

/// Failure to install a schema or read/write a typed row.
#[derive(Debug, Clone, PartialEq)]
pub enum Error {
    /// Invalid LSL; nothing was written.
    Schema(String),
    /// No table with this name exists at the transaction's snapshot.
    NoSuchType(String),
    /// The table exists, but no typed layout was installed for it at that snapshot.
    UntypedTable(u32),
    /// The value violates its checked schema.
    InvalidValue(Vec<validate::Violation>),
    /// The catalog changed after the transaction began; retry from a fresh snapshot.
    SchemaChanged,
    /// The transaction belongs to another store instance or recovery generation.
    ForeignTransaction,
    /// A typed value cannot be encoded.
    Encode(TypedError),
    /// A stored typed value cannot be decoded with its recorded schema.
    Corrupt(String),
    /// Catalog allocation/validation failed before writing.
    Catalog(CatalogError),
    /// The row-store commit failed; preserve its ambiguity/retry classification.
    Store(RowError),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

impl Classify for Error {
    fn class(&self) -> ErrorClass {
        match self {
            Error::Schema(_)
            | Error::NoSuchType(_)
            | Error::UntypedTable(_)
            | Error::InvalidValue(_)
            | Error::Encode(_)
            | Error::Catalog(_)
            | Error::ForeignTransaction => ErrorClass::Rejected,
            Error::SchemaChanged => ErrorClass::Retryable,
            Error::Corrupt(_) => ErrorClass::Fatal,
            Error::Store(e) => e.class(),
        }
    }
}

/// A transaction captures both its row snapshot and the schema catalog that validates its writes.
pub struct Transaction<P: PolicyEvaluator = AllowAll> {
    raw: RowTx<P>,
    snapshot: Snapshot,
    catalog_lsn: u64,
    owner: Arc<()>,
}

impl<P: PolicyEvaluator> Transaction<P> {
    /// The consistent catalog/row read point taken at begin.
    pub fn snapshot(&self) -> Snapshot {
        self.snapshot
    }
}

/// A typed database using one physical table per schema type and no entity/attribute/value table.
pub struct TypedStore<I: Io, C: Clock, R: Rng> {
    rows: RowStore<I, C, R>,
    owner: Arc<()>,
}

impl<I: Io, C: Clock, R: Rng> TypedStore<I, C, R> {
    /// Recovers catalog schemas and rows from the same WAL. No caller layouts are required.
    pub fn open(io: I, clock: C, rng: R, config: &Config) -> Result<Self, Error> {
        let store = Self {
            rows: RowStore::open(io, clock, rng, config).map_err(Error::Store)?,
            owner: Arc::new(()),
        };
        let snap = store.snapshot();
        for table in store.catalog().current().into_keys() {
            if store.catalog().layout_at(table, snap.as_of).is_some() {
                for (_, bytes) in store
                    .rows
                    .scan_at(snap, table, .., &AllowAll)
                    .map_err(Error::Store)?
                {
                    store.decode(table, bytes)?;
                }
            }
        }
        Ok(store)
    }

    /// The immutable catalog, including the original layouts of all historical schema versions.
    pub fn catalog(&self) -> &Catalog {
        self.rows.catalog()
    }

    /// The newest read point, including catalog-only changes.
    pub fn snapshot(&self) -> Snapshot {
        self.rows.snapshot()
    }

    /// Strict full-HLC history lookup; no SQL timestamp parsing or precision conversion.
    pub fn try_snapshot_as_of_hlc(&self, time: lattice_abi::Hlc) -> Result<Snapshot, Error> {
        self.rows.try_snapshot_as_of_hlc(time).map_err(Error::Store)
    }

    /// Applies the shared row/index retention floor while preserving original writer layouts.
    pub fn vacuum(&mut self, horizon: u64) -> crate::store::VacuumStats {
        self.rows.vacuum(horizon)
    }

    /// Validates and atomically installs every type in one schema, allocating never-reused table ids.
    /// Existing type names keep their physical table ids. Returns the schema version and commit LSN.
    pub fn install_schema(&mut self, source: &str) -> Result<(u64, u64), Error> {
        let ast = parser::parse(source).map_err(|e| Error::Schema(format!("{e:?}")))?;
        let compiled = check::check(&ast).map_err(|e| Error::Schema(format!("{e:?}")))?;
        let source = format::format(&ast);
        let mut next = self.catalog().next_table_id();
        let current = self.catalog().current();
        let mut tables = BTreeMap::new();
        for name in compiled.types.keys() {
            let table = match current.iter().find(|(_, n)| *n == name) {
                Some((id, _)) => {
                    if self
                        .catalog()
                        .layout_at(*id, self.snapshot().as_of)
                        .is_none()
                        && self.rows.has_table_history(*id)
                    {
                        return Err(Error::Schema(format!(
                            "cannot attach a typed layout to raw table history {id}"
                        )));
                    }
                    *id
                }
                None => {
                    let mut id = next.clone().map_err(Error::Catalog)?;
                    while self.rows.has_table_history(id) {
                        id = id
                            .checked_add(1)
                            .ok_or(Error::Catalog(CatalogError::Exhausted))?;
                    }
                    next = id.checked_add(1).ok_or(CatalogError::Exhausted);
                    id
                }
            };
            tables.insert(table, name.clone());
        }
        let version = self
            .catalog()
            .next_schema_version()
            .map_err(Error::Catalog)?;
        let lsn = self
            .rows
            .commit_catalog(
                vec![CatalogOp::DefineSchema {
                    version,
                    source,
                    tables,
                }],
                Vec::new(),
            )
            .map_err(Error::Store)?;
        Ok((version, lsn))
    }

    /// Starts a typed transaction at the newest catalog and row snapshot.
    pub fn begin(&self) -> Transaction {
        self.begin_with_policy(AllowAll)
    }

    /// Starts a typed transaction bound to the same actor policy as all its row operations.
    pub fn begin_with_policy<P: PolicyEvaluator>(&self, policy: P) -> Transaction<P> {
        Transaction {
            raw: self.rows.begin_with_policy(policy),
            snapshot: self.snapshot(),
            catalog_lsn: self.catalog().applied_lsn(),
            owner: self.owner.clone(),
        }
    }

    fn check_owner<P: PolicyEvaluator>(&self, tx: &Transaction<P>) -> Result<(), Error> {
        if Arc::ptr_eq(&self.owner, &tx.owner) {
            Ok(())
        } else {
            Err(Error::ForeignTransaction)
        }
    }

    fn table_at(&self, type_name: &str, snap: Snapshot) -> Result<u32, Error> {
        self.catalog()
            .id_of(snap.as_of, type_name)
            .ok_or_else(|| Error::NoSuchType(type_name.into()))
    }

    /// Validates a value against the transaction's schema before buffering its typed encoding.
    pub fn put<P: PolicyEvaluator>(
        &self,
        tx: &mut Transaction<P>,
        type_name: &str,
        key: &[u8],
        value: &Value,
    ) -> Result<(), Error> {
        self.check_owner(tx)?;
        self.rows
            .check_snapshot(tx.snapshot)
            .map_err(Error::Store)?;
        let table = self.table_at(type_name, tx.snapshot)?;
        let (schema, layout) = self
            .catalog()
            .layout_at(table, tx.snapshot.as_of)
            .ok_or(Error::UntypedTable(table))?;
        validate::validate(&schema.compiled, &layout.type_name, value)
            .map_err(Error::InvalidValue)?;
        let cells = typed::encode_row(layout, value).map_err(Error::Encode)?;
        let mut encoded = ROW_MAGIC.to_vec();
        encoded.push(ROW_VERSION);
        encoded.extend_from_slice(&schema.version.to_le_bytes());
        encoded.extend_from_slice(&cells);
        tx.raw.put((table, key.to_vec()), encoded);
        Ok(())
    }

    /// Buffers a deletion from a typed table.
    pub fn delete<P: PolicyEvaluator>(
        &self,
        tx: &mut Transaction<P>,
        type_name: &str,
        key: &[u8],
    ) -> Result<(), Error> {
        self.check_owner(tx)?;
        self.rows
            .check_snapshot(tx.snapshot)
            .map_err(Error::Store)?;
        let table = self.table_at(type_name, tx.snapshot)?;
        self.catalog()
            .layout_at(table, tx.snapshot.as_of)
            .ok_or(Error::UntypedTable(table))?;
        tx.raw.delete((table, key.to_vec()));
        Ok(())
    }

    fn decode(&self, table: u32, bytes: &[u8]) -> Result<Value, Error> {
        decode_value(self.catalog(), table, bytes)
    }

    /// Reads this transaction's own writes first, then its immutable snapshot.
    pub fn get<P: PolicyEvaluator>(
        &self,
        tx: &Transaction<P>,
        type_name: &str,
        key: &[u8],
    ) -> Result<Option<Value>, Error> {
        self.check_owner(tx)?;
        self.rows
            .check_snapshot(tx.snapshot)
            .map_err(Error::Store)?;
        let table = self.table_at(type_name, tx.snapshot)?;
        self.catalog()
            .layout_at(table, tx.snapshot.as_of)
            .ok_or(Error::UntypedTable(table))?;
        self.rows
            .get(&tx.raw, table, key)
            .map_err(Error::Store)?
            .map(|bytes| self.decode(table, bytes))
            .transpose()
    }

    /// Reads a row at a historical snapshot, dispatching by the row's original schema version.
    pub fn get_at<P: PolicyEvaluator>(
        &self,
        snap: Snapshot,
        type_name: &str,
        key: &[u8],
        policy: &P,
    ) -> Result<Option<Value>, Error> {
        self.rows.check_snapshot(snap).map_err(Error::Store)?;
        let table = self.table_at(type_name, snap)?;
        self.catalog()
            .layout_at(table, snap.as_of)
            .ok_or(Error::UntypedTable(table))?;
        self.rows
            .get_at(snap, table, key, policy)
            .map_err(Error::Store)?
            .map(|bytes| self.decode(table, bytes))
            .transpose()
    }

    /// Reads all rows of a type at a snapshot, in primary-key order.
    pub fn scan_at<P: PolicyEvaluator>(
        &self,
        snap: Snapshot,
        type_name: &str,
        policy: &P,
    ) -> Result<Vec<(Vec<u8>, Value)>, Error> {
        self.rows.check_snapshot(snap).map_err(Error::Store)?;
        let table = self.table_at(type_name, snap)?;
        self.catalog()
            .layout_at(table, snap.as_of)
            .ok_or(Error::UntypedTable(table))?;
        self.rows
            .scan_at(snap, table, .., policy)
            .map_err(Error::Store)?
            .into_iter()
            .map(|(key, bytes)| self.decode(table, bytes).map(|value| (key.to_vec(), value)))
            .collect()
    }

    /// Commits all buffered rows atomically; a changed catalog requires a new transaction.
    pub fn commit<P: PolicyEvaluator>(&mut self, tx: Transaction<P>) -> Result<u64, Error> {
        self.check_owner(&tx)?;
        self.rows
            .check_snapshot(tx.snapshot)
            .map_err(Error::Store)?;
        if !tx.raw.is_read_only() && tx.catalog_lsn != self.catalog().applied_lsn() {
            return Err(Error::SchemaChanged);
        }
        let read_only = tx.raw.is_read_only();
        let snapshot = tx.snapshot;
        let lsn = self.rows.commit(tx.raw).map_err(Error::Store)?;
        Ok(if read_only { snapshot.as_of } else { lsn })
    }
}

fn decode_value(catalog: &Catalog, table: u32, bytes: &[u8]) -> Result<Value, Error> {
    if bytes.get(..4) != Some(ROW_MAGIC.as_slice()) || bytes.get(4) != Some(&ROW_VERSION) {
        return Err(Error::Corrupt("invalid typed row envelope".into()));
    }
    let version_bytes: [u8; 8] = bytes
        .get(5..13)
        .ok_or_else(|| Error::Corrupt("truncated typed row version".into()))?
        .try_into()
        .map_err(|_| Error::Corrupt("truncated typed row version".into()))?;
    let version = u64::from_le_bytes(version_bytes);
    let schema = catalog
        .schema(version)
        .ok_or_else(|| Error::Corrupt(format!("unknown writer schema {version}")))?;
    let layout = schema
        .layouts
        .get(&table)
        .ok_or_else(|| Error::Corrupt(format!("schema {version} does not define table {table}")))?;
    let value = typed::decode_row(
        layout,
        bytes
            .get(13..)
            .ok_or_else(|| Error::Corrupt("truncated typed row".into()))?,
    )
    .map_err(|e| Error::Corrupt(format!("typed row: {e:?}")))?;
    validate::validate(&schema.compiled, &layout.type_name, &value)
        .map_err(|e| Error::Corrupt(format!("stored row violates writer schema: {e:?}")))?;
    Ok(value)
}

// Enforce I8 at the shared raw/typed write boundary, including historical replay.
pub(crate) fn validate_writes(
    catalog: &Catalog,
    writes: &[crate::rowops::RowWrite],
) -> Result<(), String> {
    if lattice_core::bugs::active(lattice_core::bugs::Bug::TypedSchemaValidationSkipped) {
        return Ok(());
    }
    for ((table, _), value) in writes {
        let Some((schema, _)) = catalog.layout_at(*table, catalog.applied_lsn()) else {
            if catalog.has_typed_history(*table) {
                return Err(format!("typed table {table} is no longer live"));
            }
            continue;
        };
        if let Some(bytes) = value {
            // New writes use the currently installed schema. Original-layout decoding is for reads,
            // not a way to append under superseded constraints after a schema change.
            if bytes.get(5..13) != Some(schema.version.to_le_bytes().as_slice()) {
                return Err(format!(
                    "typed table {table} requires writer schema {}",
                    schema.version
                ));
            }
            decode_value(catalog, *table, bytes).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

//! The database the executor runs against: a row store, the catalog it persists and the column
//! definitions the executor keeps beside it.

use std::cell::RefCell;
use std::collections::BTreeMap;

use lattice_core::Config;
use lattice_engines::catalog::{CatalogError, CatalogOp};
use lattice_engines::limits::AllowAll;
use lattice_engines::store::{RowError, RowStore};
use lattice_seams::{Clock, Io, Rng};
use lattice_sql::ast::CreateTable;

use crate::error::{ErrorKind, ExecError};
use crate::schema::{ExecCatalog, Schema, TableDef, object_name};

/// A row store plus the schema the executor needs to read and write it.
///
/// Column definitions are held in memory only (see [`crate::schema`]); table ids and names survive a
/// restart in the engine catalog, and [`Database::attach_table`] re-registers the columns.
pub struct Database<I: Io, C: Clock + Clone, R: Rng> {
    pub(crate) store: RowStore<I, C, R>,
    pub(crate) clock: C,
    pub(crate) schema: Schema,
    next_rowid: RefCell<BTreeMap<u32, u64>>,
}

impl<I: Io, C: Clock + Clone, R: Rng> Database<I, C, R> {
    /// Opens (or creates) the log and rebuilds the row store from it. The schema starts empty.
    pub fn open(io: I, clock: C, rng: R, config: &Config) -> Result<Self, ExecError> {
        let store = RowStore::open(io, clock.clone(), rng, config)?;
        Ok(Database {
            store,
            clock,
            schema: Schema::new(),
            next_rowid: RefCell::new(BTreeMap::new()),
        })
    }

    /// The underlying row store, for inspection and tests.
    pub fn store(&self) -> &RowStore<I, C, R> {
        &self.store
    }

    /// The column definitions known to the executor.
    pub fn schema(&self) -> &Schema {
        &self.schema
    }

    /// The catalog version plans are bound against. It grows whenever the engine catalog commits a
    /// change or a table is registered or removed here, so it differs after any change that could
    /// move a column position or a table id.
    pub fn catalog_version(&self) -> u64 {
        self.store
            .catalog()
            .applied_lsn()
            .saturating_add(self.schema.generation())
    }

    /// The binder's view of the schema at the current version.
    pub fn catalog_view(&self) -> ExecCatalog<'_> {
        ExecCatalog {
            schema: &self.schema,
            version: self.catalog_version(),
        }
    }

    /// Creates a table: commits the name to the engine catalog and registers its columns.
    /// Returns `false` when the table exists and `if_not_exists` is set.
    pub fn create_table(&mut self, ct: &CreateTable) -> Result<bool, ExecError> {
        let name = object_name(&ct.name)?;
        if self.schema.table(&name).is_some() {
            if ct.if_not_exists {
                return Ok(false);
            }
            return Err(relation_exists(&name));
        }
        let current = self.store.catalog().current();
        if current.values().any(|n| *n == name) {
            return Err(ExecError::new(
                ErrorKind::Bind,
                "42P07",
                format!(
                    "relation \"{name}\" exists in the catalog but its columns are not attached"
                ),
            ));
        }
        let mut id = current
            .keys()
            .copied()
            .max()
            .map_or(1, |m| m.saturating_add(1));
        // Ids are never reused, including dropped ones the live catalog no longer lists.
        for _ in 0..64 {
            let def = TableDef::from_create(id, ct)?;
            match self.store.commit_catalog(
                vec![CatalogOp::Create {
                    table: id,
                    name: name.clone(),
                }],
                Vec::new(),
            ) {
                Ok(_) => {
                    self.schema.insert(def);
                    return Ok(true);
                }
                Err(RowError::InvalidCatalog(CatalogError::IdUsed(_))) => id = id.saturating_add(1),
                Err(e) => return Err(e.into()),
            }
        }
        Err(ExecError::internal("could not allocate a table id"))
    }

    /// Registers the columns of a table that already exists in the engine catalog, for example after
    /// a restart. The id and name must match the catalog.
    pub fn attach_table(&mut self, def: TableDef) -> Result<(), ExecError> {
        let current = self.store.catalog().current();
        if current.get(&def.id) != Some(&def.name) {
            return Err(ExecError::new(
                ErrorKind::Bind,
                "42P01",
                format!(
                    "table {} (id {}) is not in the engine catalog",
                    def.name, def.id
                ),
            ));
        }
        if self.schema.table(&def.name).is_some() {
            return Err(relation_exists(&def.name));
        }
        self.schema.insert(def);
        Ok(())
    }

    /// Drops a table; its rows stay in the log. Returns `false` when it did not exist and
    /// `if_exists` is set.
    pub fn drop_table(&mut self, name: &str, if_exists: bool) -> Result<bool, ExecError> {
        let Some(def) = self.schema.table(name) else {
            if if_exists {
                return Ok(false);
            }
            return Err(ExecError::new(
                ErrorKind::Bind,
                "42P01",
                format!("table \"{name}\" does not exist"),
            ));
        };
        let id = def.id;
        self.store
            .commit_catalog(vec![CatalogOp::Drop { table: id }], Vec::new())?;
        self.schema.remove(name);
        self.next_rowid.borrow_mut().remove(&id);
        Ok(true)
    }

    /// The next row id for a table without a primary key. The counter starts after the largest key
    /// committed and is never reused within this process, even by a rolled back transaction.
    pub(crate) fn alloc_rowid(&self, table: u32) -> Result<u64, ExecError> {
        let mut ids = self.next_rowid.borrow_mut();
        let next = match ids.get(&table) {
            Some(n) => *n,
            None => {
                let snap = self.store.snapshot();
                let rows = self.store.scan_at(snap, table, .., &AllowAll)?;
                rows.last()
                    .and_then(|(k, _)| <[u8; 8]>::try_from(*k).ok())
                    .map_or(1, |k| u64::from_be_bytes(k).saturating_add(1))
            }
        };
        ids.insert(table, next.saturating_add(1));
        Ok(next)
    }
}

fn relation_exists(name: &str) -> ExecError {
    ExecError::new(
        ErrorKind::Bind,
        "42P07",
        format!("relation \"{name}\" already exists"),
    )
}

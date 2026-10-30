//! The database the executor runs against: a row store, the catalog it persists and the column
//! definitions the executor keeps beside it.

use std::cell::RefCell;
use std::collections::BTreeMap;

use lattice_core::Config;
use lattice_engines::catalog::{CatalogError, CatalogOp, DdlRef};
use lattice_engines::limits::{AllowAll, PolicyEvaluator};
use lattice_engines::store::{RowError, RowStore};
use lattice_seams::{Clock, Io, Rng};
use lattice_sql::ast::CreateTable;

use crate::error::{ErrorKind, ExecError};
use crate::exec::ActorPolicy;
use crate::schema::{ExecCatalog, MAX_TABLE_ID, Schema, TableDef, object_name};

/// A row store plus the schema the executor needs to read and write it.
///
/// Column definitions are held in memory only (see [`crate::schema`]); table ids and names survive a
/// restart in the engine catalog, and [`Database::attach_table`] re-registers the columns.
pub struct Database<I: Io, C: Clock + Clone, R: Rng> {
    pub(crate) store: RowStore<I, C, R>,
    pub(crate) clock: C,
    pub(crate) schema: Schema,
    next_rowid: RefCell<BTreeMap<u32, u64>>,
    /// The definition first registered for each table id in this process (not durable).
    fingerprints: BTreeMap<u32, String>,
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
            fingerprints: BTreeMap::new(),
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
    ///
    /// The catalog change runs under `policy`: the engine asks it to authorize the DDL, and an actor
    /// without that grant gets `42501` before anything about the catalog is revealed.
    pub fn create_table(
        &mut self,
        ct: &CreateTable,
        policy: &ActorPolicy,
    ) -> Result<bool, ExecError> {
        let name = object_name(&ct.name)?;
        // Ask first, so a denied actor cannot probe which names exist. The ids here are placeholders:
        // the engine asks again with the real change at commit.
        authorize_ddl(
            policy,
            &[CatalogOp::Create {
                table: 0,
                name: name.clone(),
            }],
        )?;
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
            let tx = self.store.begin_with_policy(policy.clone());
            match self.store.commit_with_catalog(
                tx,
                vec![CatalogOp::Create {
                    table: id,
                    name: name.clone(),
                }],
                Vec::new(),
            ) {
                Ok(_) => {
                    self.fingerprints.insert(def.id, fingerprint(&def));
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
        if def.id > MAX_TABLE_ID {
            return Err(ExecError::new(
                ErrorKind::Bind,
                "42P01",
                format!(
                    "table id {} is above the largest id the executor serves",
                    def.id
                ),
            ));
        }
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
        // Only the id and name are checked against the engine catalog, so a different column list,
        // key or constraint would be accepted and silently change what is enforced. Within one
        // process a definition seen before must match. This does not survive a restart (#379).
        let print = fingerprint(&def);
        match self.fingerprints.get(&def.id) {
            Some(first) if *first != print => {
                return Err(ExecError::new(
                    ErrorKind::Bind,
                    "42P17",
                    format!(
                        "table {} (id {}) was registered earlier in this process with a different definition",
                        def.name, def.id
                    ),
                ));
            }
            Some(_) => {}
            None => {
                self.fingerprints.insert(def.id, print);
            }
        }
        self.schema.insert(def);
        Ok(())
    }

    /// Removes a table's columns from the executor without touching the engine catalog or its rows,
    /// so the host can attach them again. A later [`Database::attach_table`] must supply the
    /// definition that was registered first in this process.
    pub fn detach_table(&mut self, name: &str) -> bool {
        self.schema.remove(name).is_some()
    }

    /// Drops a table; its rows stay in the log. Returns `false` when it did not exist and
    /// `if_exists` is set.
    pub fn drop_table(
        &mut self,
        name: &str,
        if_exists: bool,
        policy: &ActorPolicy,
    ) -> Result<bool, ExecError> {
        let known = self.schema.table(name).map_or(0, |d| d.id);
        authorize_ddl(policy, &[CatalogOp::Drop { table: known }])?;
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
        let ops = vec![CatalogOp::Drop { table: id }];
        let tx = self.store.begin_with_policy(policy.clone());
        self.store.commit_with_catalog(tx, ops, Vec::new())?;
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

/// Asks the actor's policy whether it may run this catalog change.
fn authorize_ddl(policy: &ActorPolicy, tables: &[CatalogOp]) -> Result<(), ExecError> {
    if policy.authorize_ddl(DdlRef {
        tables,
        indexes: &[],
    }) {
        Ok(())
    } else {
        Err(ExecError::new(
            ErrorKind::Permission,
            "42501",
            "permission denied: the actor may not change the catalog",
        ))
    }
}

fn fingerprint(def: &TableDef) -> String {
    format!("{def:?}")
}

fn relation_exists(name: &str) -> ExecError {
    ExecError::new(
        ErrorKind::Bind,
        "42P07",
        format!("relation \"{name}\" already exists"),
    )
}

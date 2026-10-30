//! Table definitions the executor knows, and how rows and keys are encoded.
//!
//! The engine catalog (`lattice_engines::catalog`) persists table ids and names only. Column
//! definitions are held here, in memory, and are **not** persisted: after a restart the host must
//! re-register them with [`crate::Database::attach_table`]. Rows are stored with the engine's typed row
//! codec (`lattice_engines::typed`): `numeric` and `jsonb` as text, `double precision` as its bit
//! pattern, `uuid` as 16 bytes, `date` and `timestamptz` as integers.

use std::collections::BTreeMap;

use lattice_engines::typed::{ColType, Column, TableLayout, decode_row, encode_row};
use lattice_lsl::validate::Value as LslValue;
use lattice_sql::ast::{self, DataType, ExprKind, Literal, TableConstraint, UnaryOp};
use lattice_sql::plan::{CatalogView, ColumnInfo, IndexInfo, SqlType, TableInfo};

use crate::decimal::Decimal;
use crate::error::{ErrorKind, ExecError};
use crate::eval::cast;
use crate::value::{Datum, parse_text};

/// Most `UNIQUE` constraints on one table (each needs its own marker table).
pub const MAX_UNIQUES: usize = 15;
/// Whether a physical table id belongs to an entry table derived from a `UNIQUE` constraint.
pub fn is_marker_table(physical: u32) -> bool {
    physical & MARKER_BIT != 0
}

/// The table a physical table id belongs to. Physical ids with bit 31 set are derived entry tables
/// (`MARKER_BIT | table_id << 4 | k`); every other id is the table itself. A `PolicyEvaluator` must
/// decide for a derived table exactly as it decides for its owner, or a restricted actor cannot write
/// to tables that have `UNIQUE` constraints.
pub fn owning_table(physical: u32) -> u32 {
    if is_marker_table(physical) {
        (physical & !MARKER_BIT) >> 4
    } else {
        physical
    }
}

const MARKER_BIT: u32 = 0x8000_0000;

/// Largest table id, so marker table ids stay clear of ordinary ones.
pub const MAX_TABLE_ID: u32 = (1 << 26) - 1;

/// One column.
#[derive(Clone, Debug, PartialEq)]
pub struct ColumnDef {
    /// Lower-case name.
    pub name: String,
    /// Expression type.
    pub ty: SqlType,
    /// `varchar(n)` limit, in characters.
    pub varchar_len: Option<u32>,
    /// `NOT NULL` (always set for primary key columns).
    pub not_null: bool,
    /// Constant `DEFAULT`.
    pub default: Option<Datum>,
}

/// A table: columns, primary key and unique constraints.
#[derive(Clone, Debug, PartialEq)]
pub struct TableDef {
    /// Engine catalog id; also the physical row table id.
    pub id: u32,
    /// Lower-case name.
    pub name: String,
    /// Columns in stored order.
    pub columns: Vec<ColumnDef>,
    /// Column positions of the primary key; empty when the table has none (rows get a row id).
    pub primary_key: Vec<usize>,
    /// Column positions of each `UNIQUE` constraint.
    pub uniques: Vec<Vec<usize>>,
}

/// The table name of an object name, which may be qualified with `public`.
pub(crate) fn object_name(n: &ast::ObjectName) -> Result<String, ExecError> {
    match n.0.as_slice() {
        [t] => Ok(t.value.clone()),
        [schema, t] if schema.value == "public" => Ok(t.value.clone()),
        [schema, _] => Err(ExecError::new(
            ErrorKind::Bind,
            "3F000",
            format!("schema \"{}\" does not exist", schema.value),
        )),
        _ => Err(ExecError::unsupported(
            "table names with more than two parts",
        )),
    }
}

fn key_type_ok(ty: SqlType) -> bool {
    !matches!(ty, SqlType::Numeric | SqlType::Jsonb | SqlType::Unknown)
}

impl TableDef {
    /// Builds a definition from `CREATE TABLE`.
    pub fn from_create(id: u32, ct: &ast::CreateTable) -> Result<TableDef, ExecError> {
        if id > MAX_TABLE_ID {
            return Err(ExecError::unsupported("too many tables"));
        }
        let name = object_name(&ct.name)?;
        let mut columns: Vec<ColumnDef> = Vec::with_capacity(ct.columns.len());
        for c in &ct.columns {
            if columns.iter().any(|x| x.name == c.name.value) {
                return Err(ExecError::new(
                    ErrorKind::Bind,
                    "42701",
                    format!("column \"{}\" specified more than once", c.name.value),
                ));
            }
            if let DataType::Numeric(Some(_)) = c.data_type {
                // The plan carries no type modifiers, so precision and scale could not be applied or
                // checked; accepting them would store values the declaration forbids.
                return Err(ExecError::unsupported(
                    "numeric(p, s): precision and scale are not enforced; use numeric",
                ));
            }
            let ty = SqlType::from_data_type(c.data_type);
            let varchar_len = match c.data_type {
                DataType::Varchar(n) => n,
                _ => None,
            };
            let default = match &c.default {
                Some(e) => Some(const_default(&e.kind, ty)?),
                None => None,
            };
            columns.push(ColumnDef {
                name: c.name.value.clone(),
                ty,
                varchar_len,
                not_null: c.not_null || c.primary_key,
                default,
            });
        }
        if columns.is_empty() {
            return Err(ExecError::new(
                ErrorKind::Bind,
                "42601",
                "a table needs at least one column",
            ));
        }
        let find = |ident: &ast::Ident| -> Result<usize, ExecError> {
            columns
                .iter()
                .position(|c| c.name == ident.value)
                .ok_or_else(|| {
                    ExecError::new(
                        ErrorKind::Bind,
                        "42703",
                        format!("column \"{}\" named in key does not exist", ident.value),
                    )
                })
        };
        let mut primary_key: Vec<usize> = Vec::new();
        let mut uniques: Vec<Vec<usize>> = Vec::new();
        for (i, c) in ct.columns.iter().enumerate() {
            if c.primary_key {
                if !primary_key.is_empty() {
                    return Err(ExecError::new(
                        ErrorKind::Bind,
                        "42P16",
                        "multiple primary keys are not allowed",
                    ));
                }
                primary_key.push(i);
            }
            if c.unique {
                uniques.push(vec![i]);
            }
        }
        for con in &ct.constraints {
            match con {
                TableConstraint::PrimaryKey(cols) => {
                    if !primary_key.is_empty() {
                        return Err(ExecError::new(
                            ErrorKind::Bind,
                            "42P16",
                            "multiple primary keys are not allowed",
                        ));
                    }
                    for c in cols {
                        primary_key.push(find(c)?);
                    }
                }
                TableConstraint::Unique(cols) => {
                    let mut u = Vec::new();
                    for c in cols {
                        u.push(find(c)?);
                    }
                    uniques.push(u);
                }
            }
        }
        if uniques.len() > MAX_UNIQUES {
            return Err(ExecError::unsupported(format!(
                "more than {MAX_UNIQUES} UNIQUE constraints on one table"
            )));
        }
        for &p in &primary_key {
            if let Some(c) = columns.get_mut(p) {
                c.not_null = true;
            }
        }
        for &k in primary_key.iter().chain(uniques.iter().flatten()) {
            if let Some(c) = columns.get(k)
                && !key_type_ok(c.ty)
            {
                return Err(ExecError::unsupported(format!(
                    "a key on a {} column is not executed",
                    c.ty
                )));
            }
        }
        Ok(TableDef {
            id,
            name,
            columns,
            primary_key,
            uniques,
        })
    }

    /// The physical table holding the entries of `UNIQUE` constraint `k`.
    pub fn marker_table(&self, k: usize) -> u32 {
        MARKER_BIT | (self.id << 4) | (k as u32 & 0xF)
    }

    /// The binder's view of the table.
    pub fn info(&self) -> TableInfo {
        let col_names = |ix: &[usize]| -> Vec<String> {
            ix.iter()
                .filter_map(|i| self.columns.get(*i))
                .map(|c| c.name.clone())
                .collect()
        };
        let mut indexes = Vec::new();
        if !self.primary_key.is_empty() {
            indexes.push(IndexInfo {
                name: format!("{}_pkey", self.name),
                columns: col_names(&self.primary_key),
                unique: true,
            });
        }
        for (k, u) in self.uniques.iter().enumerate() {
            indexes.push(IndexInfo {
                name: format!("{}_unique_{k}", self.name),
                columns: col_names(u),
                unique: true,
            });
        }
        TableInfo {
            id: u64::from(self.id),
            name: self.name.clone(),
            columns: self
                .columns
                .iter()
                .map(|c| ColumnInfo {
                    name: c.name.clone(),
                    ty: c.ty,
                    not_null: c.not_null,
                    has_default: c.default.is_some(),
                })
                .collect(),
            indexes,
        }
    }

    fn layout(&self) -> TableLayout {
        TableLayout {
            type_name: self.name.clone(),
            columns: self
                .columns
                .iter()
                .map(|c| Column {
                    name: c.name.clone(),
                    ty: col_type(c.ty),
                    nullable: !c.not_null,
                })
                .collect(),
        }
    }

    /// Encodes a row (one datum per column) for the row store.
    pub fn encode_row(&self, row: &[Datum]) -> Result<Vec<u8>, ExecError> {
        let mut map = BTreeMap::new();
        for (c, d) in self.columns.iter().zip(row) {
            if let Some(v) = to_lsl(d, c.ty)? {
                map.insert(c.name.clone(), v);
            }
        }
        encode_row(&self.layout(), &LslValue::Object(map))
            .map_err(|e| ExecError::internal(format!("cannot encode row: {e:?}")))
    }

    /// Decodes a stored row.
    pub fn decode_row(&self, bytes: &[u8]) -> Result<Vec<Datum>, ExecError> {
        let v = decode_row(&self.layout(), bytes).map_err(|_| ExecError::corrupt())?;
        let LslValue::Object(mut map) = v else {
            return Err(ExecError::corrupt());
        };
        self.columns
            .iter()
            .map(|c| match map.remove(&c.name) {
                Some(v) => from_lsl(&v, c.ty),
                None => Ok(Datum::Null),
            })
            .collect()
    }

    /// The row key: the primary key columns, or `rowid` for a table without one.
    pub fn row_key(&self, row: &[Datum], rowid: u64) -> Result<Vec<u8>, ExecError> {
        if self.primary_key.is_empty() {
            return Ok(rowid.to_be_bytes().to_vec());
        }
        key_of(row, &self.primary_key)
    }

    /// The entry key of a unique constraint, or `None` when a column is `NULL` (nulls are distinct).
    pub fn unique_key(&self, row: &[Datum], k: usize) -> Result<Option<Vec<u8>>, ExecError> {
        let Some(cols) = self.uniques.get(k) else {
            return Ok(None);
        };
        if cols.iter().any(|c| row.get(*c).is_none_or(Datum::is_null)) {
            return Ok(None);
        }
        key_of(row, cols).map(Some)
    }

    /// The name of a constraint for error messages.
    pub fn unique_name(&self, k: usize) -> String {
        format!("{}_unique_{k}", self.name)
    }
}

fn key_of(row: &[Datum], cols: &[usize]) -> Result<Vec<u8>, ExecError> {
    let mut out = Vec::new();
    for c in cols {
        let d = row
            .get(*c)
            .ok_or_else(|| ExecError::internal("key column is outside the row"))?;
        push_key(&mut out, d)?;
    }
    Ok(out)
}

/// Appends an order-preserving, self-delimiting encoding of one key value.
fn push_key(out: &mut Vec<u8>, d: &Datum) -> Result<(), ExecError> {
    match d {
        Datum::Bool(b) => out.push(u8::from(*b)),
        Datum::Int(i) | Datum::Timestamp(i) => {
            out.extend_from_slice(&((*i as u64) ^ (1 << 63)).to_be_bytes());
        }
        Datum::Date(x) => out.extend_from_slice(&((*x as u32) ^ (1 << 31)).to_be_bytes()),
        Datum::Float(f) => {
            let f = if *f == 0.0 { 0.0 } else { *f };
            let bits = if f.is_nan() {
                f64::NAN.to_bits()
            } else {
                f.to_bits()
            };
            let ordered = if bits >> 63 == 1 {
                !bits
            } else {
                bits ^ (1 << 63)
            };
            out.extend_from_slice(&ordered.to_be_bytes());
        }
        Datum::Uuid(u) => out.extend_from_slice(&u.to_be_bytes()),
        Datum::Text(s) => escape_bytes(out, s.as_bytes()),
        Datum::Bytes(b) => escape_bytes(out, b),
        Datum::Null | Datum::Numeric(_) | Datum::Json(_) => {
            return Err(ExecError::internal("value cannot be part of a key"));
        }
    }
    Ok(())
}

/// `0x00` becomes `0x00 0xFF`; the value ends with `0x00 0x00`, so shorter prefixes sort first.
fn escape_bytes(out: &mut Vec<u8>, b: &[u8]) {
    for byte in b {
        out.push(*byte);
        if *byte == 0 {
            out.push(0xFF);
        }
    }
    out.extend_from_slice(&[0, 0]);
}

fn col_type(ty: SqlType) -> ColType {
    match ty {
        SqlType::Boolean => ColType::Bool,
        SqlType::SmallInt
        | SqlType::Integer
        | SqlType::BigInt
        | SqlType::DoublePrecision
        | SqlType::Date
        | SqlType::Timestamptz => ColType::Int,
        SqlType::Bytea | SqlType::Uuid => ColType::Bytes,
        SqlType::Numeric | SqlType::Text | SqlType::Jsonb | SqlType::Unknown => ColType::Text,
    }
}

fn to_lsl(d: &Datum, ty: SqlType) -> Result<Option<LslValue>, ExecError> {
    Ok(Some(match (d, ty) {
        (Datum::Null, _) => return Ok(None),
        (Datum::Bool(b), SqlType::Boolean) => LslValue::Bool(*b),
        (Datum::Int(i), _)
            if matches!(ty, SqlType::SmallInt | SqlType::Integer | SqlType::BigInt) =>
        {
            LslValue::Int(*i)
        }
        (Datum::Float(f), SqlType::DoublePrecision) => {
            LslValue::Int(i64::from_ne_bytes(f.to_bits().to_ne_bytes()))
        }
        (Datum::Numeric(n), SqlType::Numeric) => LslValue::Text(n.to_string()),
        (Datum::Text(s), SqlType::Text) | (Datum::Json(s), SqlType::Jsonb) => {
            LslValue::Text(s.clone())
        }
        (Datum::Bytes(b), SqlType::Bytea) => LslValue::Bytes(b.clone()),
        (Datum::Uuid(u), SqlType::Uuid) => LslValue::Bytes(u.to_be_bytes().to_vec()),
        (Datum::Date(x), SqlType::Date) => LslValue::Int(i64::from(*x)),
        (Datum::Timestamp(t), SqlType::Timestamptz) => LslValue::Int(*t),
        _ => {
            return Err(ExecError::internal(format!(
                "a value does not fit a {ty} column"
            )));
        }
    }))
}

fn from_lsl(v: &LslValue, ty: SqlType) -> Result<Datum, ExecError> {
    let bad = ExecError::corrupt;
    Ok(match (v, ty) {
        (LslValue::Bool(b), SqlType::Boolean) => Datum::Bool(*b),
        (LslValue::Int(i), SqlType::SmallInt | SqlType::Integer | SqlType::BigInt) => {
            Datum::Int(*i)
        }
        (LslValue::Int(i), SqlType::DoublePrecision) => {
            Datum::Float(f64::from_bits(u64::from_ne_bytes(i.to_ne_bytes())))
        }
        (LslValue::Text(s), SqlType::Numeric) => {
            Datum::Numeric(Decimal::parse(s).map_err(|_| bad())?)
        }
        (LslValue::Text(s), SqlType::Text) => Datum::Text(s.clone()),
        (LslValue::Text(s), SqlType::Jsonb) => Datum::Json(s.clone()),
        (LslValue::Bytes(b), SqlType::Bytea) => Datum::Bytes(b.clone()),
        (LslValue::Bytes(b), SqlType::Uuid) => {
            let arr: [u8; 16] = b.as_slice().try_into().map_err(|_| bad())?;
            Datum::Uuid(u128::from_be_bytes(arr))
        }
        (LslValue::Int(i), SqlType::Date) => {
            let d = i32::try_from(*i).map_err(|_| bad())?;
            if !crate::value::date_in_range(d) {
                return Err(bad());
            }
            Datum::Date(d)
        }
        (LslValue::Int(i), SqlType::Timestamptz) => {
            if !crate::value::timestamp_in_range(*i) {
                return Err(bad());
            }
            Datum::Timestamp(*i)
        }
        _ => return Err(bad()),
    })
}

/// Evaluates a `DEFAULT` expression: a literal, optionally signed or cast.
fn const_default(kind: &ExprKind, ty: SqlType) -> Result<Datum, ExecError> {
    fn literal(kind: &ExprKind, ty: SqlType) -> Result<Datum, ExecError> {
        match kind {
            ExprKind::Literal(Literal::Null) => Ok(Datum::Null),
            ExprKind::Literal(Literal::Boolean(b)) => cast(Datum::Bool(*b), ty),
            ExprKind::Literal(Literal::String(s)) => parse_text(s, ty),
            ExprKind::Literal(Literal::Number(n)) => {
                let numeric = parse_text(n, SqlType::Numeric)?;
                cast(numeric, ty)
            }
            ExprKind::Unary {
                op: UnaryOp::Minus,
                expr,
            } => match literal(&expr.kind, ty)? {
                Datum::Int(i) => {
                    Ok(Datum::Int(i.checked_neg().ok_or_else(|| {
                        ExecError::eval("22003", "default out of range")
                    })?))
                }
                Datum::Float(f) => Ok(Datum::Float(-f)),
                Datum::Numeric(d) => Ok(Datum::Numeric(d.neg()?)),
                Datum::Null => Ok(Datum::Null),
                _ => Err(ExecError::bind_type("cannot negate this default")),
            },
            ExprKind::Unary {
                op: UnaryOp::Plus,
                expr,
            } => literal(&expr.kind, ty),
            ExprKind::Cast { expr, data_type } => {
                let inner = literal(&expr.kind, SqlType::from_data_type(*data_type))?;
                cast(inner, ty)
            }
            _ => Err(ExecError::unsupported("a DEFAULT must be a constant")),
        }
    }
    // A negative number literal is parsed as unary minus over the number, which must not be rounded
    // through the column type before the sign is applied.
    literal(kind, ty)
}

/// Every table the executor knows.
#[derive(Clone, Debug, Default)]
pub struct Schema {
    tables: BTreeMap<String, TableDef>,
    generation: u64,
}

impl Schema {
    /// An empty schema.
    pub fn new() -> Self {
        Self::default()
    }

    /// The table called `name`.
    pub fn table(&self, name: &str) -> Option<&TableDef> {
        self.tables.get(name)
    }

    /// The table with engine id `id`.
    pub fn by_id(&self, id: u32) -> Option<&TableDef> {
        self.tables.values().find(|t| t.id == id)
    }

    /// All tables, by name.
    pub fn tables(&self) -> impl Iterator<Item = &TableDef> {
        self.tables.values()
    }

    /// Counts every change to the set of tables.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub(crate) fn insert(&mut self, t: TableDef) {
        self.tables.insert(t.name.clone(), t);
        self.generation += 1;
    }

    pub(crate) fn remove(&mut self, name: &str) -> Option<TableDef> {
        let t = self.tables.remove(name);
        if t.is_some() {
            self.generation += 1;
        }
        t
    }
}

/// The binder's window onto the schema at one catalog version.
pub struct ExecCatalog<'a> {
    pub(crate) schema: &'a Schema,
    pub(crate) version: u64,
    /// True for an actor that may not see the catalog: every table lookup misses.
    pub(crate) hidden: bool,
}

impl CatalogView for ExecCatalog<'_> {
    fn table(&self, name: &str) -> Option<TableInfo> {
        if self.hidden {
            return None;
        }
        self.schema.table(name).map(TableDef::info)
    }

    fn version(&self) -> u64 {
        self.version
    }
}

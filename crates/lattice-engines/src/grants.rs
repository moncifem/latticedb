//! Interim per-table grants (issue 522): which tables an account may read, write and change.
//!
//! This is the permission model until the Phase 4 policy language. It is deliberately small: a set
//! of grants per account, each naming one table (or `*` for every table) and a subset of three bits.
//! There are no row predicates, roles, groups or expressions, and it must not grow into them.
//!
//! The bits:
//!
//! - `r` (read): the table is visible to the account (SQL binding, `DESCRIBE`, the change feed) and
//!   its rows may be read.
//! - `w` (write): rows may be inserted, changed and deleted.
//! - `d` (ddl): the table may be created, renamed, dropped or indexed.
//!
//! A table is *visible* to an account when it holds any bit on it. An account with no bit on a table
//! cannot tell it from a table that does not exist: SQL answers `42P01`, the feed answers an unserved
//! scope.
//!
//! Resolution is exact and most-specific-wins: an entry for the table's own name decides on its own
//! (it is not merged with `*`); otherwise the `*` entry decides; otherwise the account has no access.
//! `notes=-` is an explicit entry with no bits, which carves a table out of a wider `*` grant.
//! Names match exactly and case-sensitively against the catalog name; a grant is not a prefix, a
//! pattern or a path. A grant follows the name, not the table: dropping a table and creating another
//! with the same name gives the new table the old grant, and renaming a table moves it out from under
//! its grant.
//!
//! Text form (the part after `tables:` in the account policy file of `latticed`):
//! `notes=rw, logs=r, *=r`. Entries are comma separated, spaces around them are ignored, each is
//! `name=bits` with bits any non-repeating combination of `r`, `w`, `d` in any order, or `-` for none.
//!
//! [`TableGrantPolicy`] is the [`PolicyEvaluator`] for one account: the grants resolved against a
//! snapshot of the catalog (the row hooks receive physical table ids, not names). A table id the
//! snapshot does not know is denied for rows. Catalog changes are asked about by name and are the one
//! place an unknown id falls back to the `*` entry, because the snapshot is taken for that very change.

use crate::catalog::{CatalogOp, DdlRef, IndexOp, MAX_NAME, Tables};
use crate::limits::{PolicyEvaluator, RowRef, RowWriteRef};
use lattice_core::bugs::{self, Bug};
use std::collections::BTreeMap;
use std::fmt;

/// The name that stands for every table.
pub const WILDCARD: &str = "*";
/// Most entries one account may list.
pub const MAX_GRANT_ENTRIES: usize = 256;

/// The bits held on one table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TableAccess {
    /// Rows may be read and the table is visible.
    pub read: bool,
    /// Rows may be written.
    pub write: bool,
    /// The table may be created, renamed, dropped or indexed.
    pub ddl: bool,
}

impl TableAccess {
    /// No bit.
    pub const NONE: TableAccess = TableAccess {
        read: false,
        write: false,
        ddl: false,
    };

    /// Whether any bit is set, which is what makes a table visible.
    pub const fn any(self) -> bool {
        self.read || self.write || self.ddl
    }

    /// Whether rows may be written. A read grant never allows it.
    pub fn can_write(self) -> bool {
        if bugs::active(Bug::GrantReadOnlyTreatedAsWrite) {
            return self.read || self.write;
        }
        self.write
    }

    /// `rw`, `r`, `-`, ... in the canonical order `r`, `w`, `d`.
    fn bits(self) -> String {
        if !self.any() {
            return "-".to_string();
        }
        let mut s = String::new();
        for (on, c) in [(self.read, 'r'), (self.write, 'w'), (self.ddl, 'd')] {
            if on {
                s.push(c);
            }
        }
        s
    }
}

/// Why a grant list was refused. Never echoes the offending text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrantSpecError {
    /// The list has no entry. Say `deny-all` for an account that may do nothing.
    Empty,
    /// More than [`MAX_GRANT_ENTRIES`] entries.
    TooManyEntries,
    /// An entry is not `name=bits` (including an empty entry from a stray comma).
    MissingEquals,
    /// The table name is empty, over [`MAX_NAME`] bytes, holds a space, control character, `=`, `,`
    /// or a `*` that is not the whole name.
    BadName,
    /// The bits are empty, repeat a bit or hold anything but `r`, `w`, `d` (or `-` alone).
    BadBits,
    /// A table (or `*`) is listed twice.
    Duplicate,
}

impl fmt::Display for GrantSpecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Empty => "the table grant list is empty",
            Self::TooManyEntries => "the table grant list has too many entries",
            Self::MissingEquals => "a table grant is not name=bits",
            Self::BadName => "a table grant has an invalid table name",
            Self::BadBits => "a table grant has invalid bits (use r, w, d or -)",
            Self::Duplicate => "a table is listed twice",
        })
    }
}

impl std::error::Error for GrantSpecError {}

/// The grants of one account. Default: no access to any table.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TableGrants {
    named: BTreeMap<String, TableAccess>,
    wildcard: Option<TableAccess>,
}

impl TableGrants {
    /// Parses `name=bits, name=bits, *=bits` (see the module documentation).
    pub fn parse(spec: &str) -> Result<Self, GrantSpecError> {
        let mut grants = TableGrants::default();
        let mut entries = 0usize;
        for entry in spec.split(',') {
            let entry = entry.trim_matches(' ');
            if entry.is_empty() {
                // A stray comma, or nothing at all.
                return Err(if spec.trim_matches(' ').is_empty() {
                    GrantSpecError::Empty
                } else {
                    GrantSpecError::MissingEquals
                });
            }
            entries += 1;
            if entries > MAX_GRANT_ENTRIES {
                return Err(GrantSpecError::TooManyEntries);
            }
            let (name, bits) = entry.split_once('=').ok_or(GrantSpecError::MissingEquals)?;
            let access = parse_bits(bits)?;
            grants.insert(name, access)?;
        }
        Ok(grants)
    }

    /// Adds one grant. Fails on a bad or repeated name.
    pub fn insert(&mut self, name: &str, access: TableAccess) -> Result<(), GrantSpecError> {
        if name == WILDCARD {
            if self.wildcard.replace(access).is_some() {
                return Err(GrantSpecError::Duplicate);
            }
            return Ok(());
        }
        if !valid_name(name) {
            return Err(GrantSpecError::BadName);
        }
        if self.named.len() + usize::from(self.wildcard.is_some()) >= MAX_GRANT_ENTRIES {
            return Err(GrantSpecError::TooManyEntries);
        }
        if self.named.insert(name.to_string(), access).is_some() {
            return Err(GrantSpecError::Duplicate);
        }
        Ok(())
    }

    /// What the account may do with the table called `name`: its own entry if it has one, else the
    /// `*` entry, else nothing.
    pub fn access(&self, name: &str) -> TableAccess {
        if bugs::active(Bug::GrantTableMatchByPrefix) {
            if let Some((_, a)) = self
                .named
                .iter()
                .find(|(k, _)| name.starts_with(k.as_str()))
            {
                return *a;
            }
        } else if let Some(a) = self.named.get(name) {
            return *a;
        }
        self.wildcard.unwrap_or(TableAccess::NONE)
    }

    /// What the `*` entry grants (nothing without one).
    pub fn wildcard(&self) -> TableAccess {
        self.wildcard.unwrap_or(TableAccess::NONE)
    }

    /// The named entries, in name order (the `*` entry is not among them).
    pub fn named(&self) -> impl Iterator<Item = (&str, TableAccess)> {
        self.named.iter().map(|(k, v)| (k.as_str(), *v))
    }

    /// Whether no entry is held.
    pub fn is_empty(&self) -> bool {
        self.named.is_empty() && self.wildcard.is_none()
    }
}

impl fmt::Display for TableGrants {
    /// The canonical text form: named entries in name order, then `*`. Parses back to an equal value.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut first = true;
        for (name, a) in &self.named {
            if !first {
                f.write_str(", ")?;
            }
            first = false;
            write!(f, "{name}={}", a.bits())?;
        }
        if let Some(a) = self.wildcard {
            if !first {
                f.write_str(", ")?;
            }
            write!(f, "{WILDCARD}={}", a.bits())?;
        }
        Ok(())
    }
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME
        && !name
            .chars()
            .any(|c| c.is_control() || c.is_whitespace() || matches!(c, '=' | ',' | '*'))
}

fn parse_bits(bits: &str) -> Result<TableAccess, GrantSpecError> {
    if bits == "-" {
        return Ok(TableAccess::NONE);
    }
    let mut a = TableAccess::NONE;
    if bits.is_empty() {
        return Err(GrantSpecError::BadBits);
    }
    for c in bits.chars() {
        let slot = match c {
            'r' => &mut a.read,
            'w' => &mut a.write,
            'd' => &mut a.ddl,
            _ => return Err(GrantSpecError::BadBits),
        };
        if std::mem::replace(slot, true) {
            return Err(GrantSpecError::BadBits);
        }
    }
    Ok(a)
}

/// The evaluator of one account's grants, resolved against a catalog snapshot.
#[derive(Debug, Clone)]
pub struct TableGrantPolicy {
    grants: TableGrants,
    by_id: BTreeMap<u32, TableAccess>,
    owner: fn(u32) -> u32,
}

impl TableGrantPolicy {
    /// Resolves `grants` against `tables`. `owner` maps a physical table id to the catalog table it
    /// belongs to (the SQL executor keeps derived entry tables for `UNIQUE` constraints, which must be
    /// decided exactly as their owner); pass the identity when there are none.
    pub fn resolve(grants: &TableGrants, tables: &Tables, owner: fn(u32) -> u32) -> Self {
        TableGrantPolicy {
            by_id: tables
                .iter()
                .map(|(id, name)| (*id, grants.access(name)))
                .collect(),
            grants: grants.clone(),
            owner,
        }
    }

    /// The access to a row's table: unknown ids have none.
    fn rows(&self, physical: u32) -> TableAccess {
        self.by_id
            .get(&(self.owner)(physical))
            .copied()
            .unwrap_or(TableAccess::NONE)
    }

    /// The access that governs a catalog change to table id `table`. An id the snapshot does not
    /// know (a placeholder, or a table created by the same change) falls back to `*`.
    fn change_of(&self, table: u32) -> TableAccess {
        self.by_id
            .get(&table)
            .copied()
            .unwrap_or_else(|| self.grants.wildcard())
    }
}

impl PolicyEvaluator for TableGrantPolicy {
    fn allows_read(&self, row: RowRef<'_>) -> bool {
        self.rows(row.table).read
    }

    fn allows_write(&self, row: RowWriteRef<'_>) -> bool {
        self.rows(row.table).can_write()
    }

    fn authorize_ddl(&self, change: DdlRef<'_>) -> bool {
        if change.tables.is_empty() && change.indexes.is_empty() {
            return false;
        }
        let tables = change.tables.iter().all(|op| match op {
            CatalogOp::Create { name, .. } => self.grants.access(name).ddl,
            CatalogOp::Rename { table, name } => {
                self.change_of(*table).ddl && self.grants.access(name).ddl
            }
            CatalogOp::Drop { table } => self.change_of(*table).ddl,
        });
        let indexes = change.indexes.iter().all(|op| match op {
            IndexOp::Create(def) => self.change_of(def.table).ddl,
            // The index id does not say which table it covers, so only `*` may drop one.
            IndexOp::Drop { .. } => self.grants.wildcard().ddl,
        });
        tables && indexes
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

    const R: TableAccess = TableAccess {
        read: true,
        write: false,
        ddl: false,
    };
    const RW: TableAccess = TableAccess {
        read: true,
        write: true,
        ddl: false,
    };
    const W: TableAccess = TableAccess {
        read: false,
        write: true,
        ddl: false,
    };
    const D: TableAccess = TableAccess {
        read: false,
        write: false,
        ddl: true,
    };

    fn ident(id: u32) -> u32 {
        id
    }

    fn tables() -> Tables {
        BTreeMap::from([
            (1, "notes".to_string()),
            (2, "notes_private".to_string()),
            (3, "logs".to_string()),
        ])
    }

    fn row(table: u32) -> RowRef<'static> {
        RowRef {
            table,
            key: b"k",
            value: b"v",
        }
    }

    fn write(table: u32) -> RowWriteRef<'static> {
        RowWriteRef {
            table,
            key: b"k",
            before: None,
            after: Some(b"v"),
        }
    }

    #[test]
    fn parses_the_documented_form() {
        let g = TableGrants::parse("notes=rw, logs=r,*=r").unwrap();
        assert_eq!(g.access("notes"), RW);
        assert_eq!(g.access("logs"), R);
        assert_eq!(g.access("other"), R);
        assert_eq!(g.to_string(), "logs=r, notes=rw, *=r");
        assert_eq!(TableGrants::parse(&g.to_string()).unwrap(), g);
    }

    #[test]
    fn bits_are_any_order_and_dash_is_none() {
        let g = TableGrants::parse("a=dwr, b=-, c=wd").unwrap();
        assert_eq!(
            g.access("a"),
            TableAccess {
                read: true,
                write: true,
                ddl: true
            }
        );
        assert_eq!(g.access("b"), TableAccess::NONE);
        assert!(!g.access("c").read && g.access("c").write && g.access("c").ddl);
    }

    #[test]
    fn rejects_malformed_lists() {
        for (spec, why) in [
            ("", GrantSpecError::Empty),
            ("   ", GrantSpecError::Empty),
            ("a=r,", GrantSpecError::MissingEquals),
            (",a=r", GrantSpecError::MissingEquals),
            ("a=r,,b=r", GrantSpecError::MissingEquals),
            ("a", GrantSpecError::MissingEquals),
            ("=r", GrantSpecError::BadName),
            ("a b=r", GrantSpecError::BadName),
            ("a\u{7}=r", GrantSpecError::BadName),
            ("a*=r", GrantSpecError::BadName),
            ("a=r=w", GrantSpecError::BadBits),
            ("a=", GrantSpecError::BadBits),
            ("a=x", GrantSpecError::BadBits),
            ("a=rr", GrantSpecError::BadBits),
            ("a=R", GrantSpecError::BadBits),
            ("a=r-", GrantSpecError::BadBits),
            ("a=--", GrantSpecError::BadBits),
            ("a=r,a=w", GrantSpecError::Duplicate),
            ("*=r,*=w", GrantSpecError::Duplicate),
        ] {
            assert_eq!(TableGrants::parse(spec), Err(why), "{spec:?}");
        }
        let long = "n".repeat(MAX_NAME + 1);
        assert_eq!(
            TableGrants::parse(&format!("{long}=r")),
            Err(GrantSpecError::BadName)
        );
        assert!(TableGrants::parse(&format!("{}=r", "n".repeat(MAX_NAME))).is_ok());
    }

    #[test]
    fn entry_count_is_bounded() {
        let many = |n: usize| {
            (0..n)
                .map(|i| format!("t{i}=r"))
                .collect::<Vec<_>>()
                .join(",")
        };
        assert!(TableGrants::parse(&many(MAX_GRANT_ENTRIES)).is_ok());
        assert_eq!(
            TableGrants::parse(&many(MAX_GRANT_ENTRIES + 1)),
            Err(GrantSpecError::TooManyEntries)
        );
        // The wildcard counts toward the same bound.
        assert_eq!(
            TableGrants::parse(&format!("{},*=r", many(MAX_GRANT_ENTRIES))),
            Err(GrantSpecError::TooManyEntries)
        );
    }

    #[test]
    fn most_specific_grant_wins_and_is_not_merged() {
        let g = TableGrants::parse("notes=r, secret=-, *=rw").unwrap();
        assert_eq!(g.access("notes"), R, "explicit narrows the wildcard");
        assert_eq!(
            g.access("secret"),
            TableAccess::NONE,
            "explicit none carves out"
        );
        assert_eq!(g.access("anything"), RW);
        let g = TableGrants::parse("notes=rw, *=r").unwrap();
        assert_eq!(g.access("notes"), RW, "explicit widens the wildcard");
        assert_eq!(TableGrants::default().access("notes"), TableAccess::NONE);
    }

    #[test]
    fn names_match_exactly_not_by_prefix_or_case() {
        let g = TableGrants::parse("notes=rw").unwrap();
        assert_eq!(g.access("notes_private"), TableAccess::NONE);
        assert_eq!(g.access("note"), TableAccess::NONE);
        assert_eq!(g.access("Notes"), TableAccess::NONE);
    }

    #[test]
    fn evaluator_matrix_has_literal_expectations() {
        let g = TableGrants::parse("notes=rw, logs=r").unwrap();
        let p = TableGrantPolicy::resolve(&g, &tables(), ident);
        // notes: read and write.
        assert!(p.allows_read(row(1)) && p.allows_write(write(1)));
        // logs: read only. A read grant must not authorize writes.
        assert!(p.allows_read(row(3)) && !p.allows_write(write(3)));
        // notes_private shares a prefix with notes and has no grant.
        assert!(!p.allows_read(row(2)) && !p.allows_write(write(2)));
        // An id the snapshot does not know is denied for rows.
        assert!(!p.allows_read(row(99)) && !p.allows_write(write(99)));
        // Write without read.
        let g = TableGrants::parse("notes=w").unwrap();
        let p = TableGrantPolicy::resolve(&g, &tables(), ident);
        assert!(!p.allows_read(row(1)) && p.allows_write(write(1)));
    }

    #[test]
    fn derived_tables_follow_their_owner() {
        fn owner(id: u32) -> u32 {
            if id >= 1000 { id - 1000 } else { id }
        }
        let g = TableGrants::parse("notes=rw").unwrap();
        let p = TableGrantPolicy::resolve(&g, &tables(), owner);
        assert!(p.allows_write(write(1001)) && p.allows_read(row(1001)));
        assert!(!p.allows_write(write(1002)));
    }

    #[test]
    fn ddl_is_decided_by_name_and_wildcard() {
        let create = |name: &str| {
            vec![CatalogOp::Create {
                table: 0,
                name: name.to_string(),
            }]
        };
        let ask = |p: &TableGrantPolicy, ops: &[CatalogOp]| {
            p.authorize_ddl(DdlRef {
                tables: ops,
                indexes: &[],
            })
        };
        let rw_only = TableGrants::parse("notes=rw").unwrap();
        let p = TableGrantPolicy::resolve(&rw_only, &tables(), ident);
        assert!(!ask(&p, &create("notes")), "rw is not ddl");
        assert!(!ask(&p, &[CatalogOp::Drop { table: 1 }]));
        assert!(!ask(&p, &[]), "an empty change is denied");

        let named = TableGrants::parse("notes=d").unwrap();
        let p = TableGrantPolicy::resolve(&named, &tables(), ident);
        assert!(ask(&p, &create("notes")));
        assert!(!ask(&p, &create("other")));
        assert!(ask(&p, &[CatalogOp::Drop { table: 1 }]));
        assert!(!ask(&p, &[CatalogOp::Drop { table: 3 }]));
        assert!(
            !ask(&p, &[CatalogOp::Drop { table: 0 }]),
            "placeholder id needs *"
        );
        let rename = |table, name: &str| {
            vec![CatalogOp::Rename {
                table,
                name: name.to_string(),
            }]
        };
        assert!(
            !ask(&p, &rename(1, "elsewhere")),
            "the new name needs ddl too"
        );
        assert!(ask(&p, &rename(1, "notes")));

        let star = TableGrants::parse("*=d, secret=-").unwrap();
        let p = TableGrantPolicy::resolve(&star, &tables(), ident);
        assert!(ask(&p, &create("anything")));
        assert!(!ask(&p, &create("secret")));
        assert!(ask(&p, &[CatalogOp::Drop { table: 3 }]));
        assert!(ask(&p, &[CatalogOp::Drop { table: 0 }]));
        // One denied operation denies the whole change.
        let mixed = vec![
            CatalogOp::Create {
                table: 0,
                name: "ok".into(),
            },
            CatalogOp::Create {
                table: 0,
                name: "secret".into(),
            },
        ];
        assert!(!ask(&p, &mixed));
        // Index operations: needs ddl on the table; dropping by id needs `*`.
        let idx_drop = [IndexOp::Drop { id: 7 }];
        let ask_idx = |p: &TableGrantPolicy| {
            p.authorize_ddl(DdlRef {
                tables: &[],
                indexes: &idx_drop,
            })
        };
        assert!(ask_idx(&p));
        let p = TableGrantPolicy::resolve(&named, &tables(), ident);
        assert!(!ask_idx(&p));
    }

    #[test]
    fn ddl_bit_alone_makes_a_table_visible_but_not_readable() {
        let g = TableGrants::parse("notes=d").unwrap();
        assert!(g.access("notes").any());
        assert!(!g.access("notes").read);
        assert_eq!(g.access("notes"), D);
        assert_eq!(W.bits(), "w");
    }

    // ---- planted bugs: each must break a test above ----

    fn matrix_holds() -> bool {
        let g = TableGrants::parse("notes=rw, logs=r").unwrap();
        let p = TableGrantPolicy::resolve(&g, &tables(), ident);
        p.allows_read(row(1))
            && p.allows_write(write(1))
            && p.allows_read(row(3))
            && !p.allows_write(write(3))
            && !p.allows_read(row(2))
            && !p.allows_write(write(2))
    }

    #[test]
    fn planted_grant_bugs_are_caught() {
        bugs::clear();
        assert!(matrix_holds());
        for bug in Bug::TABLE_GRANTS {
            bugs::enable(bug);
            let caught = !matrix_holds();
            bugs::clear();
            assert!(caught, "{bug:?} must fail the grant matrix");
        }
    }

    // ---- property: the resolver against a straight-line model ----

    fn name_strategy() -> impl Strategy<Value = String> {
        prop::sample::select(vec!["a", "ab", "abc", "b", "notes", "notes_x"]).prop_map(String::from)
    }

    fn bits_strategy() -> impl Strategy<Value = (bool, bool, bool)> {
        (any::<bool>(), any::<bool>(), any::<bool>())
    }

    proptest! {
        #[test]
        fn resolver_agrees_with_a_linear_model(
            entries in prop::collection::btree_map(name_strategy(), bits_strategy(), 0..6),
            wildcard in prop::option::of(bits_strategy()),
            probe in prop::sample::select(vec!["a", "ab", "abc", "b", "notes", "notes_x", "zzz"]),
        ) {
            let mut spec: Vec<String> = Vec::new();
            for (name, (r, w, d)) in &entries {
                spec.push(format!("{name}={}", model_bits(*r, *w, *d)));
            }
            if let Some((r, w, d)) = wildcard {
                spec.push(format!("*={}", model_bits(r, w, d)));
            }
            if spec.is_empty() {
                return Ok(());
            }
            let g = TableGrants::parse(&spec.join(", ")).unwrap();
            // Model: exact name, else wildcard, else nothing.
            let expected = entries
                .get(probe)
                .copied()
                .or(wildcard)
                .unwrap_or((false, false, false));
            let got = g.access(probe);
            prop_assert_eq!((got.read, got.write, got.ddl), expected);
            // Rows: visibility and row operations follow exactly the bits.
            let tables = BTreeMap::from([(5u32, probe.to_string())]);
            let p = TableGrantPolicy::resolve(&g, &tables, ident);
            prop_assert_eq!(p.allows_read(row(5)), expected.0);
            prop_assert_eq!(p.allows_write(write(5)), expected.1);
            prop_assert_eq!(g.access(probe).any(), expected.0 || expected.1 || expected.2);
            prop_assert_eq!(TableGrants::parse(&g.to_string()).unwrap(), g);
        }
    }

    fn model_bits(r: bool, w: bool, d: bool) -> String {
        if !(r || w || d) {
            return "-".to_string();
        }
        let mut s = String::new();
        if d {
            s.push('d');
        }
        if r {
            s.push('r');
        }
        if w {
            s.push('w');
        }
        s
    }
}

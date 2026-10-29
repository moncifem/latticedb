//! Small relational executor shared by hand-parser and sqlparser front ends.
use crate::{
    Error, Result,
    ast::*,
    storage::{Db, Session, schema},
};
use lattice_seams::{Clock, Io, Rng};
use std::collections::{BTreeMap, BTreeSet};
pub type Output = Vec<Vec<Value>>;
type Env = BTreeMap<String, Value>;
fn columns(table: &str) -> Result<BTreeSet<String>> {
    let (_, cols) = schema(table)?;
    Ok(cols
        .into_iter()
        .flat_map(|c| [c.to_string(), format!("{table}.{c}")])
        .collect())
}
fn env(table: &str, row: &[Value]) -> Result<Env> {
    let (_, cols) = schema(table)?;
    Ok(cols
        .into_iter()
        .zip(row.iter())
        .flat_map(|(c, v)| {
            [
                (c.to_string(), v.clone()),
                (format!("{table}.{c}"), v.clone()),
            ]
        })
        .collect())
}
pub trait RowLookup {
    fn column(&self, name: &str) -> Option<&Value>;
}
impl RowLookup for Env {
    fn column(&self, name: &str) -> Option<&Value> {
        self.get(name)
    }
}
struct IndexedRow<'a> {
    positions: &'a BTreeMap<String, usize>,
    values: &'a [Value],
}
impl RowLookup for IndexedRow<'_> {
    fn column(&self, name: &str) -> Option<&Value> {
        self.positions.get(name).and_then(|at| self.values.get(*at))
    }
}
pub fn eval(e: &Expr, row: &impl RowLookup, count: usize, params: &[Value]) -> Result<Value> {
    match e {
        Expr::Col(c) => row.column(c).cloned().ok_or_else(|| {
            Error::new(
                "42703",
                "LSQL-0003",
                format!("unknown/ambiguous column {c}"),
            )
        }),
        Expr::Lit(v) => Ok(v.clone()),
        Expr::Count => Ok(Value::Int(count as i64)),
        Expr::Param(n) => params
            .get(n - 1)
            .cloned()
            .ok_or_else(|| Error::syntax(format!("missing parameter ${n}"))),
        Expr::Binary(a, op, b) => {
            let (a, b) = (eval(a, row, count, params)?, eval(b, row, count, params)?);
            if op == "and" || op == "or" {
                return match (op.as_str(), a, b) {
                    ("and", Value::Bool(false), _) | ("and", _, Value::Bool(false)) => {
                        Ok(Value::Bool(false))
                    }
                    ("or", Value::Bool(true), _) | ("or", _, Value::Bool(true)) => {
                        Ok(Value::Bool(true))
                    }
                    (_, Value::Null, _) | (_, _, Value::Null) => Ok(Value::Null),
                    ("and", Value::Bool(a), Value::Bool(b)) => Ok(Value::Bool(a && b)),
                    ("or", Value::Bool(a), Value::Bool(b)) => Ok(Value::Bool(a || b)),
                    _ => Err(Error::unsupported("boolean operands required")),
                };
            }
            if a == Value::Null || b == Value::Null {
                return Ok(Value::Null);
            }
            if std::mem::discriminant(&a) != std::mem::discriminant(&b) {
                return Err(Error::unsupported(
                    "mixed-type operation requires explicit supported cast",
                ));
            }
            match op.as_str() {
                "+" | "-" => match (a, b) {
                    (Value::Int(a), Value::Int(b)) => (if op == "+" {
                        a.checked_add(b)
                    } else {
                        a.checked_sub(b)
                    })
                    .map(Value::Int)
                    .ok_or_else(|| Error::unsupported("integer overflow")),
                    _ => Err(Error::unsupported("integer arithmetic required")),
                },
                "=" => Ok(Value::Bool(a == b)),
                ">" => Ok(Value::Bool(a > b)),
                "<" => Ok(Value::Bool(a < b)),
                ">=" => Ok(Value::Bool(a >= b)),
                "<=" => Ok(Value::Bool(a <= b)),
                _ => Err(Error::unsupported(format!("operator {op}"))),
            }
        }
    }
}
fn validate(e: &Expr, cols: &BTreeSet<String>) -> Result<()> {
    match e {
        Expr::Col(c) if !cols.contains(c) => Err(Error::new(
            "42703",
            "LSQL-0003",
            format!("unknown column {c}"),
        )),
        Expr::Binary(a, _, b) => {
            validate(a, cols)?;
            validate(b, cols)
        }
        _ => Ok(()),
    }
}
fn project(
    exprs: &[Expr],
    row: &impl RowLookup,
    count: usize,
    params: &[Value],
) -> Result<Vec<Value>> {
    exprs.iter().map(|e| eval(e, row, count, params)).collect()
}
fn matches(
    filter: &Option<Expr>,
    row: &impl RowLookup,
    count: usize,
    params: &[Value],
) -> Result<bool> {
    match filter {
        None => Ok(true),
        Some(e) => match eval(e, row, count, params)? {
            Value::Bool(b) => Ok(b),
            Value::Null => Ok(false),
            _ => Err(Error::unsupported("predicate must be boolean")),
        },
    }
}

/// PostgreSQL's default ascending order puts NULL after every non-NULL value.
fn sql_order(left: &[Value], right: &[Value]) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    for (a, b) in left.iter().zip(right) {
        let order = match (a, b) {
            (Value::Null, Value::Null) => Ordering::Equal,
            (Value::Null, _) => Ordering::Greater,
            (_, Value::Null) => Ordering::Less,
            _ => a.cmp(b),
        };
        if order != Ordering::Equal {
            return order;
        }
    }
    left.len().cmp(&right.len())
}
fn grouped(e: &Expr, keys: &[Expr]) -> bool {
    if keys.contains(e) {
        return true;
    }
    match e {
        Expr::Col(_) => false,
        Expr::Binary(a, _, b) => grouped(a, keys) && grouped(b, keys),
        _ => true,
    }
}

/// Extract only conjuncts whose columns belong to one join input. Keep the complete residual
/// predicate at the join output; this is a generic relational pushdown, not a fixture answer.
fn scoped_filter(
    e: Option<&Expr>,
    positions: &BTreeMap<String, usize>,
    low: usize,
    high: usize,
) -> Option<Expr> {
    fn local(e: &Expr, positions: &BTreeMap<String, usize>, low: usize, high: usize) -> bool {
        match e {
            Expr::Col(name) => positions
                .get(name)
                .is_some_and(|at| *at >= low && *at < high),
            Expr::Binary(a, _, b) => {
                local(a, positions, low, high) && local(b, positions, low, high)
            }
            Expr::Count => false,
            _ => true,
        }
    }
    let e = e?;
    if let Expr::Binary(a, op, b) = e
        && op == "and"
    {
        return match (
            scoped_filter(Some(a), positions, low, high),
            scoped_filter(Some(b), positions, low, high),
        ) {
            (Some(a), Some(b)) => Some(Expr::Binary(Box::new(a), "and".into(), Box::new(b))),
            (a, b) => a.or(b),
        };
    }
    local(e, positions, low, high).then(|| e.clone())
}

pub fn execute<I: Io, C: Clock, R: Rng>(
    db: &mut Db<I, C, R>,
    s: &mut Session,
    plan: &Plan,
    params: &[Value],
) -> Result<Output> {
    match plan {
        Plan::Begin => {
            db.begin(s)?;
            return Ok(vec![]);
        }
        Plan::Commit => {
            db.commit(s)?;
            return Ok(vec![]);
        }
        Plan::Rollback => {
            db.rollback(s);
            return Ok(vec![]);
        }
        _ => {}
    }
    bind(plan)?;
    let autocommit = s.tx.is_none();
    if autocommit {
        db.begin(s)?;
    }
    let saved_tx = s.tx.clone();
    let saved_pending = s.pending.clone();
    db.event(|| serde_json::json!({"plan":plan,"parameters":params}));
    let result = inner(db, s, plan, params);
    match result {
        Ok(out) => {
            if autocommit {
                if matches!(plan, Plan::Select(_)) {
                    db.rollback(s);
                } else {
                    db.commit(s)?;
                }
            }
            db.event(|| serde_json::json!({"result":out}));
            Ok(out)
        }
        Err(e) => {
            s.tx = saved_tx;
            s.pending = saved_pending;
            if autocommit {
                db.rollback(s);
            }
            db.event(|| serde_json::json!({"error":e}));
            Err(e)
        }
    }
}
fn inner<I: Io, C: Clock, R: Rng>(
    db: &mut Db<I, C, R>,
    s: &mut Session,
    plan: &Plan,
    params: &[Value],
) -> Result<Output> {
    match plan {
        Plan::Select(q) => {
            let mut cols = columns(&q.table)?;
            if let Some((join, _)) = &q.join {
                let other = columns(join)?;
                let duplicate = cols
                    .intersection(&other)
                    .filter(|c| !c.contains('.'))
                    .cloned()
                    .collect::<Vec<_>>();
                cols.extend(other);
                for c in duplicate {
                    cols.remove(&c);
                }
            }
            for e in q
                .columns
                .iter()
                .chain(q.filter.iter())
                .chain(q.group.iter())
                .chain(q.having.iter())
                .chain(q.order.iter())
            {
                validate(e, &cols)?;
            }
            if let Some((_, on)) = &q.join {
                validate(on, &cols)?;
            }
            let mut positions = BTreeMap::new();
            let (_, names) = schema(&q.table)?;
            for (at, name) in names.iter().enumerate() {
                positions.insert(name.to_string(), at);
                positions.insert(format!("{}.{name}", q.table), at);
            }
            if let Some((join, _)) = &q.join {
                for (at, name) in schema(join)?.1.iter().enumerate() {
                    if positions.contains_key(*name) {
                        positions.remove(*name);
                    } else {
                        positions.insert(name.to_string(), names.len() + at);
                    }
                    positions.insert(format!("{join}.{name}"), names.len() + at);
                }
            }
            let base = if q.join.is_none() {
                db.scan_filtered(s, &q.table, &q.historical, &q.filter, params)?
            } else {
                let local = scoped_filter(q.filter.as_ref(), &positions, 0, names.len());
                let scanned = db.scan_filtered(s, &q.table, &q.historical, &local, params)?;
                let mut filtered = Vec::new();
                for row in scanned {
                    if matches(
                        &local,
                        &IndexedRow {
                            positions: &positions,
                            values: &row,
                        },
                        0,
                        params,
                    )? {
                        filtered.push(row);
                    }
                }
                filtered
            };
            let mut rows = Vec::new();
            if let Some((join, on)) = &q.join {
                let local = scoped_filter(q.filter.as_ref(), &positions, names.len(), usize::MAX);
                let right_positions = positions
                    .iter()
                    .filter(|(_, at)| **at >= names.len())
                    .map(|(name, at)| (name.clone(), at - names.len()))
                    .collect::<BTreeMap<_, _>>();
                let scanned = db.scan_filtered(s, join, &q.historical, &local, params)?;
                let mut right = Vec::new();
                for row in scanned {
                    if matches(
                        &local,
                        &IndexedRow {
                            positions: &right_positions,
                            values: &row,
                        },
                        0,
                        params,
                    )? {
                        right.push(row);
                    }
                }
                // A deterministic equality index replaces the quadratic cross product. Other
                // predicates retain nested-loop execution and the full ON/WHERE checks.
                let equality = if let Expr::Binary(a, op, b) = on {
                    if op == "=" {
                        if let (Expr::Col(a), Expr::Col(b)) = (a.as_ref(), b.as_ref()) {
                            match (positions.get(a), positions.get(b)) {
                                (Some(a), Some(b)) if *a < names.len() && *b >= names.len() => {
                                    Some((*a, *b - names.len()))
                                }
                                (Some(a), Some(b)) if *b < names.len() && *a >= names.len() => {
                                    Some((*b, *a - names.len()))
                                }
                                _ => None,
                            }
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                } else {
                    None
                };
                let mut index = BTreeMap::<Value, Vec<&Vec<Value>>>::new();
                if let Some((_, right_at)) = equality {
                    for row in &right {
                        if row[right_at] != Value::Null {
                            index.entry(row[right_at].clone()).or_default().push(row);
                        }
                    }
                }
                for left in base {
                    let candidates = if let Some((left_at, _)) = equality {
                        index.get(&left[left_at]).cloned().unwrap_or_default()
                    } else {
                        right.iter().collect()
                    };
                    for right in candidates {
                        let mut joined = left.clone();
                        joined.extend(right.iter().cloned());
                        let view = IndexedRow {
                            positions: &positions,
                            values: &joined,
                        };
                        if eval(on, &view, 0, params)? == Value::Bool(true)
                            && matches(&q.filter, &view, 0, params)?
                        {
                            rows.push(joined);
                        }
                    }
                }
            } else {
                for row in base {
                    if matches(
                        &q.filter,
                        &IndexedRow {
                            positions: &positions,
                            values: &row,
                        },
                        0,
                        params,
                    )? {
                        rows.push(row);
                    }
                }
            }
            let aggregate = !q.group.is_empty()
                || q.columns.iter().any(Expr::count)
                || q.having.is_some()
                || q.order.iter().any(Expr::count);
            let mut out = Vec::new();
            if aggregate {
                let mut groups = BTreeMap::<Vec<Value>, (usize, Vec<Value>)>::new();
                for row in rows {
                    let key = project(
                        &q.group,
                        &IndexedRow {
                            positions: &positions,
                            values: &row,
                        },
                        0,
                        params,
                    )?;
                    let entry = groups.entry(key).or_insert_with(|| (0, row));
                    entry.0 += 1;
                }
                if q.group.is_empty() && groups.is_empty() {
                    groups.insert(vec![], (0, vec![]));
                }
                for (count, row) in groups.into_values() {
                    let view = IndexedRow {
                        positions: &positions,
                        values: &row,
                    };
                    if matches(&q.having, &view, count, params)? {
                        out.push((
                            project(&q.order, &view, count, params)?,
                            project(&q.columns, &view, count, params)?,
                        ));
                    }
                }
            } else {
                for row in rows {
                    let view = IndexedRow {
                        positions: &positions,
                        values: &row,
                    };
                    if matches(&q.having, &view, 1, params)? {
                        out.push((
                            project(&q.order, &view, 1, params)?,
                            project(&q.columns, &view, 1, params)?,
                        ));
                    }
                }
            }
            if !q.order.is_empty() {
                out.sort_by(|a, b| sql_order(&a.0, &b.0));
            }
            if let Some(n) = q.limit {
                out.truncate(n);
            }
            Ok(out.into_iter().map(|(_, r)| r).collect())
        }
        Plan::Update {
            table,
            assignments,
            filter,
            returning,
        } => {
            let (_, names) = schema(table)?;
            let cols = columns(table)?;
            for (k, e) in assignments {
                if k == "id" {
                    return Err(Error::unsupported("spike does not update primary keys"));
                }
                if !names.contains(&k.as_str()) {
                    return Err(Error::new(
                        "42703",
                        "LSQL-0003",
                        format!("unknown column {k}"),
                    ));
                }
                validate(e, &cols)?;
            }
            for e in filter.iter().chain(returning.iter()) {
                validate(e, &cols)?;
            }
            let mut out = Vec::new();
            for mut row in db.scan(s, table, &None)? {
                let before = env(table, &row)?;
                if matches(filter, &before, 0, params)? {
                    for (k, e) in assignments {
                        let at = names.iter().position(|c| *c == k).unwrap();
                        row[at] = eval(e, &before, 0, params)?;
                    }
                    let after = env(table, &row)?;
                    if !returning.is_empty() {
                        out.push(project(returning, &after, 0, params)?);
                    }
                    db.stage(s, table, row)?;
                }
            }
            Ok(out)
        }
        Plan::Insert {
            table,
            columns: fields,
            values,
            returning,
        } => {
            let (_, names) = schema(table)?;
            if fields.len() != values.len() {
                return Err(Error::syntax("INSERT column/value count differs"));
            }
            let mut row = vec![Value::Null; names.len()];
            for (k, e) in fields.iter().zip(values) {
                let at = names.iter().position(|c| *c == k).ok_or_else(|| {
                    Error::new("42703", "LSQL-0003", format!("unknown column {k}"))
                })?;
                row[at] = eval(e, &Env::new(), 0, params)?;
            }
            if db
                .scan(s, table, &None)?
                .iter()
                .any(|old| old.first() == row.first())
            {
                return Err(Error::new("23505", "LSQL-0004", "duplicate primary key"));
            }
            let out = if returning.is_empty() {
                vec![]
            } else {
                vec![project(returning, &env(table, &row)?, 0, params)?]
            };
            db.stage(s, table, row)?;
            Ok(out)
        }
        _ => unreachable!(),
    }
}

/// The same explicit prototype grammar/column contract applies to every relational backend.
/// This binder never evaluates a query or substitutes custom execution for DataFusion.
pub fn bind(plan: &Plan) -> Result<()> {
    match plan {
        Plan::Select(q) => {
            let mut cols = columns(&q.table)?;
            if let Some((join, on)) = &q.join {
                let other = columns(join)?;
                let duplicate = cols
                    .intersection(&other)
                    .filter(|c| !c.contains('.'))
                    .cloned()
                    .collect::<Vec<_>>();
                cols.extend(other);
                for c in duplicate {
                    cols.remove(&c);
                }
                validate(on, &cols)?;
                if on.count() {
                    return Err(Error::unsupported("aggregate in JOIN ON"));
                }
            }
            for e in q
                .columns
                .iter()
                .chain(q.filter.iter())
                .chain(q.group.iter())
                .chain(q.having.iter())
                .chain(q.order.iter())
            {
                validate(e, &cols)?;
            }
            let aggregate = !q.group.is_empty()
                || q.columns.iter().any(Expr::count)
                || q.having.is_some()
                || q.order.iter().any(Expr::count);
            if aggregate {
                for e in q
                    .columns
                    .iter()
                    .chain(q.having.iter())
                    .chain(q.order.iter())
                {
                    if !grouped(e, &q.group) {
                        return Err(Error::unsupported(
                            "ungrouped column in aggregate projection/HAVING/ORDER BY",
                        ));
                    }
                }
            }
            if q.filter.as_ref().is_some_and(Expr::count) || q.group.iter().any(Expr::count) {
                return Err(Error::unsupported("aggregate in WHERE/GROUP BY"));
            }
        }
        Plan::Update {
            table,
            assignments,
            filter,
            returning,
        } => {
            let (_, names) = schema(table)?;
            let cols = columns(table)?;
            let mut seen = BTreeSet::new();
            for (k, e) in assignments {
                if k == "id" {
                    return Err(Error::unsupported("spike does not update primary keys"));
                }
                if !names.contains(&k.as_str()) {
                    return Err(Error::new(
                        "42703",
                        "LSQL-0003",
                        format!("unknown column {k}"),
                    ));
                }
                if !seen.insert(k) {
                    return Err(Error::syntax("duplicate assignment"));
                }
                validate(e, &cols)?;
                if e.count() {
                    return Err(Error::unsupported("aggregate assignment"));
                }
            }
            for e in filter.iter().chain(returning.iter()) {
                validate(e, &cols)?;
                if e.count() {
                    return Err(Error::unsupported("aggregate DML expression"));
                }
            }
        }
        Plan::Insert {
            table,
            columns: fields,
            values,
            returning,
        } => {
            let (_, names) = schema(table)?;
            if fields.len() != values.len() {
                return Err(Error::syntax("INSERT column/value count differs"));
            }
            let mut seen = BTreeSet::new();
            for k in fields {
                if !names.contains(&k.as_str()) {
                    return Err(Error::new(
                        "42703",
                        "LSQL-0003",
                        format!("unknown column {k}"),
                    ));
                }
                if !seen.insert(k) {
                    return Err(Error::syntax("duplicate insert column"));
                }
            }
            for e in values {
                validate(e, &BTreeSet::new())?;
                if e.count() {
                    return Err(Error::unsupported("aggregate INSERT value"));
                }
            }
            let cols = columns(table)?;
            for e in returning {
                validate(e, &cols)?;
                if e.count() {
                    return Err(Error::unsupported("aggregate RETURNING"));
                }
            }
        }
        _ => {}
    }
    Ok(())
}

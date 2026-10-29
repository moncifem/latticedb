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
pub fn eval(e: &Expr, row: &Env, count: usize, params: &[Value]) -> Result<Value> {
    match e {
        Expr::Col(c) => row.get(c).cloned().ok_or_else(|| {
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
fn project(exprs: &[Expr], row: &Env, count: usize, params: &[Value]) -> Result<Vec<Value>> {
    exprs.iter().map(|e| eval(e, row, count, params)).collect()
}
fn matches(filter: &Option<Expr>, row: &Env, count: usize, params: &[Value]) -> Result<bool> {
    match filter {
        None => Ok(true),
        Some(e) => match eval(e, row, count, params)? {
            Value::Bool(b) => Ok(b),
            Value::Null => Ok(false),
            _ => Err(Error::unsupported("predicate must be boolean")),
        },
    }
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
            let mut rows = Vec::new();
            let base = db.scan(s, &q.table, &q.historical)?;
            if let Some((join, on)) = &q.join {
                let right = db.scan(s, join, &q.historical)?;
                for a in &base {
                    for b in &right {
                        let mut r = env(&q.table, a)?;
                        let other = env(join, b)?;
                        for (k, v) in other {
                            if !k.contains('.') && r.contains_key(&k) {
                                r.remove(&k);
                            } else {
                                r.insert(k, v);
                            }
                        }
                        if eval(on, &r, 0, params)? == Value::Bool(true) {
                            rows.push(r);
                        }
                    }
                }
            } else {
                for r in base {
                    rows.push(env(&q.table, &r)?);
                }
            }
            let mut filtered = Vec::new();
            for r in rows {
                if matches(&q.filter, &r, 0, params)? {
                    filtered.push(r);
                }
            }
            let aggregate = !q.group.is_empty()
                || q.columns.iter().any(Expr::count)
                || q.having.as_ref().is_some_and(Expr::count);
            let mut groups = BTreeMap::<Vec<Value>, Vec<Env>>::new();
            if aggregate {
                for r in filtered {
                    let k = project(&q.group, &r, 0, params)?;
                    groups.entry(k).or_default().push(r);
                }
                if q.group.is_empty() && groups.is_empty() {
                    groups.insert(vec![], vec![]);
                }
            } else {
                for (i, r) in filtered.into_iter().enumerate() {
                    groups.insert(vec![Value::Int(i as i64)], vec![r]);
                }
            }
            let mut out = Vec::new();
            for group in groups.into_values() {
                let empty = Env::new();
                let row = group.first().unwrap_or(&empty);
                if matches(&q.having, row, group.len(), params)? {
                    if aggregate {
                        for e in q.columns.iter().chain(q.order.iter()) {
                            if !e.count()
                                && !q.group.contains(e)
                                && !matches!(e, Expr::Lit(_) | Expr::Param(_))
                            {
                                return Err(Error::unsupported(
                                    "nonaggregate projection must be a group key",
                                ));
                            }
                        }
                    }
                    out.push((
                        project(&q.order, row, group.len(), params)?,
                        project(&q.columns, row, group.len(), params)?,
                    ));
                }
            }
            if !q.order.is_empty() {
                out.sort_by(|a, b| a.0.cmp(&b.0));
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
                || q.having.as_ref().is_some_and(Expr::count);
            if aggregate {
                for e in q.columns.iter().chain(q.order.iter()) {
                    if !e.count()
                        && !q.group.contains(e)
                        && !matches!(e, Expr::Lit(_) | Expr::Param(_))
                    {
                        return Err(Error::unsupported(
                            "nonaggregate projection must be a group key",
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

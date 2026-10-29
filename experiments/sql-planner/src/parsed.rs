//! Lower sqlparser's AST directly; never reparse its display text with the hand parser.
use crate::{Error, Result, ast::*};
use sqlparser::{ast as s, dialect::PostgreSqlDialect, parser::Parser};
fn unsupported() -> Error {
    Error::unsupported("AST feature outside the measured spike grammar")
}
fn name(n: &s::ObjectName) -> Result<String> {
    let text = n.to_string();
    if text
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
    {
        Ok(text.to_ascii_lowercase())
    } else {
        Err(unsupported())
    }
}
fn expr(e: &s::Expr) -> Result<Expr> {
    match e {
        s::Expr::Identifier(i) if i.quote_style.is_none() => {
            Ok(Expr::Col(i.value.to_ascii_lowercase()))
        }
        s::Expr::CompoundIdentifier(ids) if ids.iter().all(|i| i.quote_style.is_none()) => {
            Ok(Expr::Col(
                ids.iter()
                    .map(|i| i.value.to_ascii_lowercase())
                    .collect::<Vec<_>>()
                    .join("."),
            ))
        }
        s::Expr::Value(v) => match &v.value {
            s::Value::Number(n, false) => {
                Ok(Expr::Lit(Value::Int(n.parse().map_err(|_| unsupported())?)))
            }
            s::Value::SingleQuotedString(t) => Ok(Expr::Lit(Value::Text(t.clone()))),
            s::Value::Null => Ok(Expr::Lit(Value::Null)),
            s::Value::Placeholder(p) => {
                let n = p
                    .strip_prefix('$')
                    .ok_or_else(unsupported)?
                    .parse::<usize>()
                    .map_err(|_| unsupported())?;
                if n == 0 {
                    return Err(Error::syntax("parameters start at $1"));
                }
                Ok(Expr::Param(n))
            }
            _ => Err(unsupported()),
        },
        s::Expr::Nested(e) => expr(e),
        s::Expr::BinaryOp { left, op, right } => {
            let op = op.to_string().to_ascii_lowercase();
            if !["+", "-", "=", ">", "<", ">=", "<=", "and", "or"].contains(&op.as_str()) {
                return Err(unsupported());
            }
            Ok(Expr::Binary(
                Box::new(expr(left)?),
                op,
                Box::new(expr(right)?),
            ))
        }
        s::Expr::Function(f)
            if f.name.to_string().eq_ignore_ascii_case("count")
                && f.over.is_none()
                && f.filter.is_none()
                && f.within_group.is_empty() =>
        {
            match &f.args {
                s::FunctionArguments::List(args)
                    if args.duplicate_treatment.is_none()
                        && args.clauses.is_empty()
                        && matches!(
                            args.args.as_slice(),
                            [s::FunctionArg::Unnamed(s::FunctionArgExpr::Wildcard)]
                        ) =>
                {
                    Ok(Expr::Count)
                }
                _ => Err(unsupported()),
            }
        }
        _ => Err(unsupported()),
    }
}
fn projection(items: &[s::SelectItem]) -> Result<Vec<Expr>> {
    items
        .iter()
        .map(|i| match i {
            s::SelectItem::UnnamedExpr(e) => expr(e),
            _ => Err(unsupported()),
        })
        .collect()
}
fn table(t: &s::TableFactor) -> Result<String> {
    match t {
        s::TableFactor::Table {
            name: n,
            alias,
            args,
            with_hints,
            version,
            with_ordinality,
            partitions,
            json_path,
            sample,
            index_hints,
        } if alias.is_none()
            && args.is_none()
            && with_hints.is_empty()
            && version.is_none()
            && !*with_ordinality
            && partitions.is_empty()
            && json_path.is_none()
            && sample.is_none()
            && index_hints.is_empty() =>
        {
            name(n)
        }
        _ => Err(unsupported()),
    }
}
pub fn parse(sql: &str) -> Result<Plan> {
    let (cleaned, historical) = crate::custom::strip_historical(sql)?;
    let mut ast = Parser::parse_sql(&PostgreSqlDialect {}, &cleaned)
        .map_err(|e| Error::syntax(e.to_string()))?;
    if ast.len() != 1 {
        return Err(Error::unsupported("one statement per prepare"));
    }
    let plan = match ast.remove(0) {
        s::Statement::Query(q) => {
            let q = *q;
            if q.with.is_some()
                || q.fetch.is_some()
                || !q.locks.is_empty()
                || q.for_clause.is_some()
                || q.settings.is_some()
                || q.format_clause.is_some()
                || !q.pipe_operators.is_empty()
            {
                return Err(unsupported());
            }
            let s::SetExpr::Select(sel) = *q.body else {
                return Err(unsupported());
            };
            if sel.from.len() != 1
                || sel.distinct.is_some()
                || sel.top.is_some()
                || sel.exclude.is_some()
                || sel.into.is_some()
                || !sel.optimizer_hints.is_empty()
                || sel.select_modifiers.is_some()
                || !sel.lateral_views.is_empty()
                || sel.prewhere.is_some()
                || !sel.connect_by.is_empty()
                || !sel.cluster_by.is_empty()
                || !sel.distribute_by.is_empty()
                || !sel.sort_by.is_empty()
                || !sel.named_window.is_empty()
                || sel.qualify.is_some()
                || sel.value_table_mode.is_some()
            {
                return Err(unsupported());
            }
            let from = &sel.from[0];
            let table = table(&from.relation)?;
            let join = match from.joins.as_slice() {
                [] => None,
                [j] => match &j.join_operator {
                    s::JoinOperator::Inner(s::JoinConstraint::On(e)) => {
                        Some((self::table(&j.relation)?, expr(e)?))
                    }
                    _ => return Err(unsupported()),
                },
                _ => return Err(unsupported()),
            };
            let group = match &sel.group_by {
                s::GroupByExpr::Expressions(es, modifiers) if modifiers.is_empty() => {
                    es.iter().map(expr).collect::<Result<_>>()?
                }
                _ => return Err(unsupported()),
            };
            let order = match q.order_by {
                None => vec![],
                Some(order) if order.interpolate.is_none() => match order.kind {
                    s::OrderByKind::Expressions(es) => es
                        .iter()
                        .map(|e| {
                            if !matches!(e.options.sort, None | Some(s::OrderBySort::Asc))
                                || e.options.nulls_first.is_some()
                                || e.with_fill.is_some()
                            {
                                return Err(unsupported());
                            }
                            expr(&e.expr)
                        })
                        .collect::<Result<_>>()?,
                    _ => return Err(unsupported()),
                },
                _ => return Err(unsupported()),
            };
            let limit = match q.limit_clause {
                None => None,
                Some(s::LimitClause::LimitOffset {
                    limit: Some(e),
                    offset: None,
                    limit_by,
                }) if limit_by.is_empty() => match expr(&e)? {
                    Expr::Lit(Value::Int(n)) if n >= 0 => Some(n as usize),
                    _ => return Err(unsupported()),
                },
                _ => return Err(unsupported()),
            };
            Plan::Select(Select {
                columns: projection(&sel.projection)?,
                table,
                join,
                historical,
                filter: sel.selection.as_ref().map(expr).transpose()?,
                group,
                having: sel.having.as_ref().map(expr).transpose()?,
                order,
                limit,
            })
        }
        s::Statement::Update(u) => {
            if historical.is_some()
                || u.from.is_some()
                || !u.table.joins.is_empty()
                || u.output.is_some()
                || u.or.is_some()
                || !u.order_by.is_empty()
                || u.limit.is_some()
                || !u.optimizer_hints.is_empty()
            {
                return Err(unsupported());
            }
            let assignments = u
                .assignments
                .iter()
                .map(|a| {
                    let s::AssignmentTarget::ColumnName(n) = &a.target else {
                        return Err(unsupported());
                    };
                    Ok((name(n)?, expr(&a.value)?))
                })
                .collect::<Result<_>>()?;
            Plan::Update {
                table: table(&u.table.relation)?,
                assignments,
                filter: u.selection.as_ref().map(expr).transpose()?,
                returning: projection(u.returning.as_deref().unwrap_or(&[]))?,
            }
        }
        s::Statement::Insert(i) => {
            if historical.is_some()
                || i.or.is_some()
                || i.ignore
                || i.overwrite
                || i.table_alias.is_some()
                || i.on.is_some()
                || i.output.is_some()
                || i.partitioned.is_some()
                || !i.after_columns.is_empty()
                || i.replace_into
                || i.priority.is_some()
                || i.insert_alias.is_some()
                || i.settings.is_some()
                || i.format_clause.is_some()
                || !i.assignments.is_empty()
                || i.multi_table_insert_type.is_some()
                || !i.optimizer_hints.is_empty()
            {
                return Err(unsupported());
            }
            let source = i.source.ok_or_else(unsupported)?;
            if source.with.is_some() || source.order_by.is_some() || source.limit_clause.is_some() {
                return Err(unsupported());
            }
            let s::SetExpr::Values(v) = *source.body else {
                return Err(unsupported());
            };
            if v.rows.len() != 1 {
                return Err(unsupported());
            }
            let s::TableObject::TableName(n) = i.table else {
                return Err(unsupported());
            };
            Plan::Insert {
                table: name(&n)?,
                columns: i.columns.iter().map(name).collect::<Result<_>>()?,
                values: v.rows[0].iter().map(expr).collect::<Result<_>>()?,
                returning: projection(i.returning.as_deref().unwrap_or(&[]))?,
            }
        }
        s::Statement::StartTransaction { modes, .. }
            if modes.is_empty() && historical.is_none() =>
        {
            Plan::Begin
        }
        s::Statement::Commit { chain: false, .. } if historical.is_none() => Plan::Commit,
        s::Statement::Rollback {
            chain: false,
            savepoint: None,
            ..
        } if historical.is_none() => Plan::Rollback,
        _ => return Err(unsupported()),
    };
    Ok(plan)
}

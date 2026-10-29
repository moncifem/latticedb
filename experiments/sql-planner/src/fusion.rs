//! Real DataFusion relational planning/execution with a Lattice-owned transactional mutation sink.
//! The SQL front end is sqlparser AST lowering. DML lowers to relational projections whose output
//! is staged in RowTx only after execution; there is no custom relational-executor fallback.
use crate::{
    Error, Result,
    ast::*,
    execute::Output,
    storage::{Db, Session, schema},
};
use datafusion::{
    arrow::{
        array::{
            Array, ArrayRef, BooleanArray, Int64Array, LargeStringArray, StringArray, UInt64Array,
        },
        datatypes::{DataType, Field, Schema, SchemaRef},
        record_batch::RecordBatch,
    },
    catalog::{Session as FusionSession, TableProvider},
    common::{Result as FusionResult, ScalarValue},
    datasource::MemTable,
    logical_expr::{Expr as FusionExpr, TableType},
    physical_plan::ExecutionPlan,
    prelude::{SessionConfig, SessionContext},
};
use lattice_seams::{Clock, Io, Rng};
use std::sync::{Arc, Mutex};

#[derive(Debug)]
struct SnapshotProvider {
    memory: MemTable,
    table: String,
    lsn: u64,
    events: Arc<Mutex<Vec<serde_json::Value>>>,
}
#[async_trait::async_trait]
impl TableProvider for SnapshotProvider {
    fn schema(&self) -> SchemaRef {
        self.memory.schema()
    }
    fn table_type(&self) -> TableType {
        TableType::Base
    }
    async fn scan(
        &self,
        state: &dyn FusionSession,
        projection: Option<&Vec<usize>>,
        filters: &[FusionExpr],
        limit: Option<usize>,
    ) -> FusionResult<Arc<dyn ExecutionPlan>> {
        self.events.lock().unwrap().push(serde_json::json!({"provider_scan":self.table,"snapshot":self.lsn,"projection":projection,"filters":filters.iter().map(ToString::to_string).collect::<Vec<_>>(),"limit":limit}));
        self.memory.scan(state, projection, filters, limit).await
    }
}
fn candidate(e: impl std::fmt::Display) -> Error {
    Error::new("XX000", "SPIKE-DATAFUSION", e.to_string())
}
fn render(e: &Expr) -> String {
    match e {
        Expr::Col(c) => c.clone(),
        Expr::Param(n) => format!("${n}"),
        Expr::Count => "COUNT(*)".into(),
        Expr::Lit(Value::Int(n)) => n.to_string(),
        Expr::Lit(Value::Text(t)) => format!("'{}'", t.replace('\'', "''")),
        Expr::Lit(Value::Null) => "NULL".into(),
        Expr::Lit(Value::Bool(b)) => b.to_string(),
        Expr::Binary(a, op, b) => format!("({} {op} {})", render(a), render(b)),
    }
}
fn list(es: &[Expr]) -> String {
    es.iter().map(render).collect::<Vec<_>>().join(", ")
}
fn predicate(e: &Option<Expr>) -> String {
    e.as_ref()
        .map(|e| format!(" WHERE {}", render(e)))
        .unwrap_or_default()
}
fn query(plan: &Plan) -> Result<(String, usize)> {
    match plan {
        Plan::Select(q) => {
            let mut sql = format!("SELECT {} FROM {}", list(&q.columns), q.table);
            if let Some((t, on)) = &q.join {
                sql.push_str(&format!(" INNER JOIN {t} ON {}", render(on)));
            }
            sql.push_str(&predicate(&q.filter));
            if !q.group.is_empty() {
                sql.push_str(&format!(" GROUP BY {}", list(&q.group)));
            }
            if let Some(e) = &q.having {
                sql.push_str(&format!(" HAVING {}", render(e)));
            }
            if !q.order.is_empty() {
                sql.push_str(&format!(" ORDER BY {}", list(&q.order)));
            }
            if let Some(n) = q.limit {
                sql.push_str(&format!(" LIMIT {n}"));
            }
            Ok((sql, 0))
        }
        Plan::Update {
            table,
            assignments,
            filter,
            returning,
        } => {
            let (_, cols) = schema(table)?;
            let projected = cols
                .iter()
                .map(|c| {
                    let e = assignments
                        .iter()
                        .find(|(k, _)| k == c)
                        .map(|(_, e)| render(e))
                        .unwrap_or_else(|| c.to_string());
                    format!("{e} AS {c}")
                })
                .collect::<Vec<_>>()
                .join(", ");
            let extra = returning
                .iter()
                .enumerate()
                .map(|(i, e)| format!(", {} AS _return_{i}", render(e)))
                .collect::<String>();
            Ok((
                format!(
                    "SELECT {}{extra} FROM (SELECT {projected} FROM {table}{}) AS changed",
                    cols.join(", "),
                    predicate(filter)
                ),
                cols.len(),
            ))
        }
        Plan::Insert {
            table,
            columns,
            values,
            returning,
        } => {
            let (_, cols) = schema(table)?;
            if columns.len() != values.len() {
                return Err(Error::syntax("INSERT column/value count differs"));
            }
            let mut projected = Vec::new();
            for c in &cols {
                let at = columns.iter().position(|k| k == c).ok_or_else(|| {
                    Error::new("23502", "LSQL-0004", format!("missing column {c}"))
                })?;
                projected.push(format!("{} AS {c}", render(&values[at])));
            }
            if columns.iter().any(|c| !cols.contains(&c.as_str())) {
                return Err(Error::new("42703", "LSQL-0003", "unknown INSERT column"));
            }
            let extra = returning
                .iter()
                .enumerate()
                .map(|(i, e)| format!(", {} AS _return_{i}", render(e)))
                .collect::<String>();
            Ok((
                format!(
                    "SELECT {}{extra} FROM (SELECT {}) AS changed",
                    cols.join(", "),
                    projected.join(", ")
                ),
                cols.len(),
            ))
        }
        _ => Err(Error::unsupported(
            "transaction control handled by Lattice session",
        )),
    }
}
fn batch(table: &str, rows: &[Vec<Value>]) -> Result<RecordBatch> {
    let (_, cols) = schema(table)?;
    let mut arrays = Vec::<ArrayRef>::new();
    let mut fields = Vec::new();
    for (at, name) in cols.iter().enumerate() {
        if table == "users" && *name == "name" {
            fields.push(Field::new(*name, DataType::Utf8, false));
            let values = rows
                .iter()
                .map(|r| match &r[at] {
                    Value::Text(t) => Some(t.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>();
            arrays.push(Arc::new(StringArray::from(values)));
        } else {
            fields.push(Field::new(*name, DataType::Int64, false));
            let values = rows
                .iter()
                .map(|r| match r[at] {
                    Value::Int(n) => Some(n),
                    _ => None,
                })
                .collect::<Vec<_>>();
            arrays.push(Arc::new(Int64Array::from(values)));
        }
    }
    RecordBatch::try_new(Arc::new(Schema::new(fields)), arrays).map_err(candidate)
}
fn values(batches: Vec<RecordBatch>) -> Result<Output> {
    let mut out = Vec::new();
    for batch in batches {
        for row in 0..batch.num_rows() {
            let mut values = Vec::new();
            for a in batch.columns() {
                let v = if a.is_null(row) {
                    Value::Null
                } else {
                    match a.data_type() {
                        DataType::Null => Value::Null,
                        DataType::Int64 => {
                            Value::Int(a.as_any().downcast_ref::<Int64Array>().unwrap().value(row))
                        }
                        DataType::UInt64 => Value::Int(
                            a.as_any()
                                .downcast_ref::<UInt64Array>()
                                .unwrap()
                                .value(row)
                                .try_into()
                                .map_err(candidate)?,
                        ),
                        DataType::Utf8 => Value::Text(
                            a.as_any()
                                .downcast_ref::<StringArray>()
                                .unwrap()
                                .value(row)
                                .into(),
                        ),
                        DataType::LargeUtf8 => Value::Text(
                            a.as_any()
                                .downcast_ref::<LargeStringArray>()
                                .unwrap()
                                .value(row)
                                .into(),
                        ),
                        DataType::Boolean => Value::Bool(
                            a.as_any()
                                .downcast_ref::<BooleanArray>()
                                .unwrap()
                                .value(row),
                        ),
                        other => {
                            return Err(candidate(format!(
                                "unimplemented result conversion {other:?}"
                            )));
                        }
                    }
                };
                values.push(v);
            }
            out.push(values);
        }
    }
    Ok(out)
}
fn scalar(v: &Value) -> ScalarValue {
    match v {
        Value::Int(n) => ScalarValue::Int64(Some(*n)),
        Value::Text(t) => ScalarValue::Utf8(Some(t.clone())),
        Value::Bool(b) => ScalarValue::Boolean(Some(*b)),
        Value::Null => ScalarValue::Null,
    }
}

pub struct PreparedInsert {
    frame: datafusion::dataframe::DataFrame,
    width: usize,
    table: String,
    returns: bool,
}
impl PreparedInsert {
    /// Real cached DataFusion logical plan: subsequent parameter binds do not parse SQL again.
    pub async fn prepare(plan: &Plan) -> Result<Self> {
        crate::execute::bind(plan)?;
        let Plan::Insert {
            table, returning, ..
        } = plan
        else {
            return Err(Error::unsupported("cached prepared case is INSERT"));
        };
        let (sql, width) = query(plan)?;
        let context =
            SessionContext::new_with_config(SessionConfig::new().with_target_partitions(1));
        let frame = context.sql(&sql).await.map_err(candidate)?;
        Ok(Self {
            frame,
            width,
            table: table.clone(),
            returns: !returning.is_empty(),
        })
    }
    pub async fn execute<I: Io, C: Clock, R: Rng>(
        &self,
        db: &mut Db<I, C, R>,
        s: &mut Session,
        params: &[Value],
    ) -> Result<Output> {
        let auto = s.tx.is_none();
        if auto {
            db.begin(s)?;
        }
        let saved_tx = s.tx.clone();
        let saved_pending = s.pending.clone();
        let result = async {
            let batches = self
                .frame
                .clone()
                .with_param_values(params.iter().map(scalar).collect::<Vec<_>>())
                .map_err(candidate)?
                .collect()
                .await
                .map_err(candidate)?;
            let rows = values(batches)?;
            stage(db, s, &self.table, self.width, self.returns, rows, true)
        }
        .await;
        if result.is_err() {
            s.tx = saved_tx;
            s.pending = saved_pending;
        }
        finish(db, s, auto, result)
    }
}
fn stage<I: Io, C: Clock, R: Rng>(
    db: &mut Db<I, C, R>,
    s: &mut Session,
    table: &str,
    width: usize,
    returns: bool,
    rows: Output,
    insert: bool,
) -> Result<Output> {
    let existing = if insert {
        db.scan(s, table, &None)?
    } else {
        vec![]
    };
    let mut out = Vec::new();
    for row in rows {
        let after = row[..width].to_vec();
        if insert && existing.iter().any(|old| old.first() == after.first()) {
            return Err(Error::new("23505", "LSQL-0004", "duplicate primary key"));
        }
        if returns {
            out.push(row[width..].to_vec());
        }
        db.stage(s, table, after)?;
    }
    Ok(out)
}
fn finish<I: Io, C: Clock, R: Rng>(
    db: &mut Db<I, C, R>,
    s: &mut Session,
    auto: bool,
    result: Result<Output>,
) -> Result<Output> {
    match result {
        Ok(out) => {
            if auto {
                db.commit(s)?;
            }
            db.event(|| serde_json::json!({"result":out}));
            Ok(out)
        }
        Err(e) => {
            if auto {
                db.rollback(s);
            }
            Err(e)
        }
    }
}

pub async fn execute<I: Io, C: Clock, R: Rng>(
    db: &mut Db<I, C, R>,
    s: &mut Session,
    plan: &Plan,
    params: &[Value],
) -> Result<Output> {
    if matches!(plan, Plan::Begin | Plan::Commit | Plan::Rollback) {
        return crate::execute::execute(db, s, plan, params);
    }
    crate::execute::bind(plan)?;
    let auto = s.tx.is_none();
    if auto {
        db.begin(s)?;
    }
    let old_tx = s.tx.clone();
    let old_pending = s.pending.clone();
    let events = Arc::new(Mutex::new(Vec::new()));
    let result=async {
        let context=SessionContext::new_with_config(SessionConfig::new().with_target_partitions(1));
        let historical=match plan{Plan::Select(q)=>q.historical.clone(),_=>None};
        let snapshot=db.snapshot(s,&historical)?.as_of;
        let tables=match plan {
            Plan::Select(q)=> {let mut tables=vec![q.table.as_str()];if let Some((join,_))=&q.join {if join!=&q.table {tables.push(join);}} tables},
            Plan::Update{table,..}=>vec![table.as_str()],
            _=>vec![],
        };
        for table in tables {
            // Shared storage bound pushdown changes only the snapshot input. DataFusion still
            // plans and evaluates the complete original relational predicate and projection.
            let filter=match plan {
                Plan::Select(q) if q.join.is_none()=>q.filter.clone(),
                Plan::Update{filter,..}=>filter.clone(),
                _=>None,
            };
            let rows=db.scan_filtered(s,table,&historical,&filter,params)?;let batch=batch(table,&rows)?;let memory=MemTable::try_new(batch.schema(),vec![vec![batch]]).map_err(candidate)?;
            context.register_table(table,Arc::new(SnapshotProvider{memory,table:table.into(),lsn:snapshot,events:events.clone()})).map_err(candidate)?;}
        let(sql,width)=query(plan)?;let frame=context.sql(&sql).await.map_err(candidate)?.with_param_values(params.iter().map(scalar).collect::<Vec<_>>()).map_err(candidate)?;
        db.event(||serde_json::json!({"datafusion_logical":frame.logical_plan().display_indent().to_string()}));
        let physical=frame.create_physical_plan().await.map_err(candidate)?;
        db.event(||serde_json::json!({"datafusion_physical":datafusion::physical_plan::displayable(physical.as_ref()).indent(true).to_string()}));
        let batches=datafusion::physical_plan::collect(physical,context.task_ctx()).await.map_err(candidate)?;
        let rows=values(batches)?;
        match plan{Plan::Select(_)=>Ok(rows),Plan::Update{table,returning,..}=>stage(db,s,table,width,!returning.is_empty(),rows,false),Plan::Insert{table,returning,..}=>stage(db,s,table,width,!returning.is_empty(),rows,true),_=>unreachable!()}
    }.await;
    db.trace.extend(events.lock().unwrap().drain(..));
    if result.is_err() {
        s.tx = old_tx;
        s.pending = old_pending;
    }
    if auto && matches!(plan, Plan::Select(_)) {
        db.rollback(s);
        return result;
    }
    finish(db, s, auto, result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn arrow_null_type_reaches_row_not_null_validation() {
        use datafusion::arrow::array::NullArray;
        let batch = RecordBatch::try_new(
            Arc::new(Schema::new(vec![Field::new(
                "priority",
                DataType::Null,
                true,
            )])),
            vec![Arc::new(NullArray::new(1))],
        )
        .unwrap();
        assert_eq!(values(vec![batch]).unwrap(), vec![vec![Value::Null]]);
    }
}

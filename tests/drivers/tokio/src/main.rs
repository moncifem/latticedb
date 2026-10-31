// tokio-postgres smoke script for latticed (issue 607).
//
// Runs against the server named by LATTICE_DSN (a postgresql:// URL) and prints one JSON object per step.
// tests/drivers/smoke.sh compares the lines with expected.jsonl and the pins in known.jsonl.
//
// tokio-postgres has no text mode for the extended protocol: every statement with parameters is sent with
// binary parameter and result formats, and latticed refuses those with 0A000 (#608). So the steps without
// parameters use simple_query (text results), and the steps with parameters use the driver's only way, the
// extended protocol with explicit parameter types, and are pinned as known failures. When such a step
// fails, the script then runs the same change as a literal statement through simple_query, unreported, so
// that the later steps see the data PostgreSQL would have. A step that starts to pass skips the fallback
// and makes smoke_compare.py ask for its pin to be removed.
use std::env;
use std::process::exit;

use tokio_postgres::types::{ToSql, Type};
use tokio_postgres::{Client, Error, NoTls, Row, SimpleQueryMessage};

type Fragment = Result<String, Error>;

fn json(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn report(name: &str, tags: &[&str], result: Fragment) {
    let fragment = match result {
        Ok(fragment) => fragment,
        Err(err) => match err.code() {
            Some(state) => format!("\"sqlstate\":{}", json(state.code())),
            None => {
                let message = err.to_string();
                let first = message.lines().next().unwrap_or("");
                format!("\"client_error\":{}", json(first))
            }
        },
    };
    let tags: Vec<String> = tags.iter().map(|t| json(t)).collect();
    let tail = if fragment.is_empty() { String::new() } else { format!(",{fragment}") };
    println!("{{\"step\":{},\"tags\":[{}]{}}}", json(name), tags.join(","), tail);
}

// A text cell of a simple_query row as JSON, by the kind letter of its column: i number, s string, b bool.
fn cell(text: Option<&str>, kind: char) -> String {
    match (text, kind) {
        (None, _) => "null".to_string(),
        (Some(t), 'i') if t.parse::<i64>().is_ok() => t.to_string(),
        (Some("t"), 'b') => "true".to_string(),
        (Some("f"), 'b') => "false".to_string(),
        (Some(t), _) => json(t),
    }
}

async fn simple_rows(client: &Client, sql: &str, kinds: &str) -> Fragment {
    let mut rows = Vec::new();
    for message in client.simple_query(sql).await? {
        if let SimpleQueryMessage::Row(row) = message {
            let cells: Vec<String> = kinds
                .chars()
                .enumerate()
                .map(|(i, kind)| cell(row.get(i), kind))
                .collect();
            rows.push(format!("[{}]", cells.join(",")));
        }
    }
    Ok(format!("\"rows\":[{}]", rows.join(",")))
}

async fn ok(client: &Client, sql: &str) -> Fragment {
    client.batch_execute(sql).await?;
    Ok("\"ok\":true".to_string())
}

fn typed_cell(row: &Row, i: usize) -> String {
    let ty = row.columns()[i].type_().clone();
    if ty == Type::INT4 {
        row.try_get::<_, Option<i32>>(i).ok().flatten().map_or("null".to_string(), |v| v.to_string())
    } else if ty == Type::INT8 {
        row.try_get::<_, Option<i64>>(i).ok().flatten().map_or("null".to_string(), |v| v.to_string())
    } else if ty == Type::BOOL {
        row.try_get::<_, Option<bool>>(i).ok().flatten().map_or("null".to_string(), |v| v.to_string())
    } else {
        row.try_get::<_, Option<String>>(i).ok().flatten().map_or("null".to_string(), |v| json(&v))
    }
}

async fn typed_rows(client: &Client, sql: &str, types: &[Type], params: &[&(dyn ToSql + Sync)]) -> Fragment {
    let statement = client.prepare_typed(sql, types).await?;
    let mut rows = Vec::new();
    for row in client.query(&statement, params).await? {
        let cells: Vec<String> = (0..row.len()).map(|i| typed_cell(&row, i)).collect();
        rows.push(format!("[{}]", cells.join(",")));
    }
    Ok(format!("\"rows\":[{}]", rows.join(",")))
}

async fn typed_count(client: &Client, sql: &str, types: &[Type], params: &[&(dyn ToSql + Sync)]) -> Fragment {
    let statement = client.prepare_typed(sql, types).await?;
    let n = client.execute(&statement, params).await?;
    Ok(format!("\"rowcount\":{n}"))
}

// typed_count, then the literal statement through simple_query when it failed.
async fn typed_count_or_literal(
    client: &Client,
    sql: &str,
    types: &[Type],
    params: &[&(dyn ToSql + Sync)],
    literal: &str,
) -> Fragment {
    let result = typed_count(client, sql, types, params).await;
    if result.is_err() {
        let _ = client.batch_execute(literal).await;
    }
    result
}

const INSERT: &str = "INSERT INTO drv_smoke VALUES ($1, $2, $3, $4)";
const INSERT_TYPES: [Type; 4] = [Type::INT4, Type::TEXT, Type::INT8, Type::BOOL];

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let Ok(dsn) = env::var("LATTICE_DSN") else {
        eprintln!("LATTICE_DSN is not set");
        exit(2);
    };
    let (client, connection) = match tokio_postgres::connect(&dsn, NoTls).await {
        Ok(pair) => pair,
        Err(err) => {
            eprintln!("connect: {err}");
            exit(1);
        }
    };
    let driver = tokio::spawn(connection);

    report(
        "select_literals",
        &["select", "int", "text", "bool"],
        simple_rows(&client, "SELECT 1 AS one, 'x' AS s, true AS b", "isb").await,
    );
    report(
        "create_table",
        &["ddl"],
        ok(&client, "CREATE TABLE drv_smoke (id INT PRIMARY KEY, name TEXT NOT NULL, qty BIGINT, ok BOOLEAN)").await,
    );
    report(
        "insert_bolt",
        &["dml", "parameter", "int", "text", "bool", "bigint"],
        typed_count_or_literal(
            &client,
            INSERT,
            &INSERT_TYPES,
            &[&1i32, &"bolt", &9_000_000_000i64, &true],
            "INSERT INTO drv_smoke VALUES (1, 'bolt', 9000000000, true)",
        )
        .await,
    );
    report(
        "insert_nut_null",
        &["dml", "parameter", "int", "text", "bool", "null"],
        typed_count_or_literal(
            &client,
            INSERT,
            &INSERT_TYPES,
            &[&2i32, &"nut", &None::<i64>, &false],
            "INSERT INTO drv_smoke VALUES (2, 'nut', NULL, false)",
        )
        .await,
    );
    report(
        "select_with_params",
        &["select", "parameter", "int", "text", "bool", "bigint", "null"],
        typed_rows(
            &client,
            "SELECT id, name, qty, ok FROM drv_smoke WHERE id = $1 OR name = $2 ORDER BY id",
            &[Type::INT4, Type::TEXT],
            &[&2i32, &"bolt"],
        )
        .await,
    );
    report(
        "update_with_params",
        &["dml", "parameter", "bigint"],
        typed_count_or_literal(
            &client,
            "UPDATE drv_smoke SET qty = $1 WHERE id = $2",
            &[Type::INT8, Type::INT4],
            &[&7i64, &1i32],
            "UPDATE drv_smoke SET qty = 7 WHERE id = 1",
        )
        .await,
    );
    report(
        "delete_with_params",
        &["dml", "parameter"],
        typed_count_or_literal(
            &client,
            "DELETE FROM drv_smoke WHERE id = $1",
            &[Type::INT4],
            &[&2i32],
            "DELETE FROM drv_smoke WHERE id = 2",
        )
        .await,
    );

    // Committed transaction. When the parameter insert fails the transaction is rolled back and the
    // same row is written again as a literal in a second transaction.
    let committed = async {
        client.batch_execute("BEGIN").await?;
        match typed_count(&client, INSERT, &INSERT_TYPES, &[&3i32, &"pin", &4i64, &true]).await {
            Ok(fragment) => {
                client.batch_execute("COMMIT").await?;
                Ok(fragment)
            }
            Err(err) => {
                let _ = client.batch_execute("ROLLBACK").await;
                for literal in ["BEGIN", "INSERT INTO drv_smoke VALUES (3, 'pin', 4, true)", "COMMIT"] {
                    let _ = client.batch_execute(literal).await;
                }
                Err(err)
            }
        }
    }
    .await;
    report("transaction_commit", &["transaction", "dml", "parameter"], committed);
    report("select_after_commit", &["select"], simple_rows(&client, "SELECT id FROM drv_smoke ORDER BY id", "i").await);

    let rolled_back = async {
        client.batch_execute("BEGIN").await?;
        let result = typed_count(&client, INSERT, &INSERT_TYPES, &[&4i32, &"gone", &0i64, &true]).await;
        let _ = client.batch_execute("ROLLBACK").await;
        result
    }
    .await;
    report("transaction_rollback", &["transaction", "dml", "parameter"], rolled_back);
    report(
        "select_after_rollback",
        &["select", "parameter"],
        typed_rows(&client, "SELECT id FROM drv_smoke WHERE id = $1", &[Type::INT4], &[&4i32]).await,
    );
    report(
        "error_duplicate_key",
        &["error", "dml", "parameter", "null"],
        typed_count(&client, INSERT, &INSERT_TYPES, &[&1i32, &"dup", &None::<i64>, &None::<bool>]).await,
    );
    report("error_syntax", &["error"], ok(&client, "SELECT FROM FROM drv_smoke").await);
    report("select_after_errors", &["select"], simple_rows(&client, "SELECT id FROM drv_smoke ORDER BY id", "i").await);
    report("drop_table", &["ddl"], ok(&client, "DROP TABLE drv_smoke").await);
    report(
        "known_default_mode_select",
        &["select", "parameter", "int"],
        typed_rows(&client, "SELECT $1::int AS n", &[Type::INT4], &[&7i32]).await,
    );

    drop(client);
    match driver.await {
        Ok(Ok(())) => println!("{{\"step\":\"close\",\"tags\":[\"close\"],\"ok\":true}}"),
        Ok(Err(err)) => {
            eprintln!("close: {err}");
            exit(1);
        }
        Err(err) => {
            eprintln!("close: {err}");
            exit(1);
        }
    }
}

// pgx smoke script for latticed, text mode (issue 576).
//
// Runs against the server named by LATTICE_DSN (a postgresql:// URL) and prints one JSON object per
// step. tests/drivers/smoke.sh compares the lines with expected.jsonl and the pins in known.jsonl.
//
// pgx's default query execution mode (cache_statement) prepares the statement, checks the parameter
// count the server describes and then asks for binary formats. latticed describes the placeholder
// count (#703) but refuses binary formats with 0A000 (#608), so that mode still fails before the
// query runs. Every step here uses QueryExecModeExec, which pgx documents as one extended-protocol
// round trip with text parameters and text results. The default mode is a known failure
// (known.jsonl).
package main

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"os"

	"github.com/jackc/pgx/v5"
	"github.com/jackc/pgx/v5/pgconn"
)

const insert = "INSERT INTO drv_smoke VALUES ($1, $2, $3, $4)"

type record map[string]any

// execer is satisfied by *pgx.Conn and pgx.Tx, so a step can run in or out of a transaction.
type execer interface {
	Exec(ctx context.Context, sql string, args ...any) (pgconn.CommandTag, error)
}

type querier interface {
	Query(ctx context.Context, sql string, args ...any) (pgx.Rows, error)
}

func emit(r record) {
	b, err := json.Marshal(r)
	if err != nil {
		fmt.Fprintln(os.Stderr, "marshal:", err)
		os.Exit(1)
	}
	fmt.Println(string(b))
}

// step runs fn and prints its record with the step name and tags. A server error contributes its
// SQLSTATE; any other error is a client-side failure and is printed as its message.
func step(name string, tags []string, fn func() (record, error)) {
	r, err := fn()
	if err != nil {
		var pe *pgconn.PgError
		if errors.As(err, &pe) {
			r = record{"sqlstate": pe.Code}
		} else {
			r = record{"client_error": firstLine(err.Error())}
		}
	}
	if r == nil {
		r = record{}
	}
	r["step"] = name
	r["tags"] = tags
	emit(r)
}

func firstLine(s string) string {
	for i, c := range s {
		if c == '\n' {
			return s[:i]
		}
	}
	return s
}

func rowsOf(ctx context.Context, q querier, sql string, args ...any) (record, error) {
	rows, err := q.Query(ctx, sql, args...)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	out := [][]any{}
	for rows.Next() {
		values, err := rows.Values()
		if err != nil {
			return nil, err
		}
		out = append(out, values)
	}
	if err := rows.Err(); err != nil {
		return nil, err
	}
	return record{"rows": out}, nil
}

func rowcountOf(ctx context.Context, e execer, sql string, args ...any) (record, error) {
	tag, err := e.Exec(ctx, sql, args...)
	if err != nil {
		return nil, err
	}
	return record{"rowcount": tag.RowsAffected()}, nil
}

func okOf(ctx context.Context, e execer, sql string) (record, error) {
	if _, err := e.Exec(ctx, sql); err != nil {
		return nil, err
	}
	return record{"ok": true}, nil
}

func main() {
	dsn := os.Getenv("LATTICE_DSN")
	if dsn == "" {
		fmt.Fprintln(os.Stderr, "LATTICE_DSN is not set")
		os.Exit(2)
	}
	ctx := context.Background()

	cfg, err := pgx.ParseConfig(dsn)
	if err != nil {
		fmt.Fprintln(os.Stderr, "parse config:", err)
		os.Exit(1)
	}
	cfg.DefaultQueryExecMode = pgx.QueryExecModeExec
	conn, err := pgx.ConnectConfig(ctx, cfg)
	if err != nil {
		fmt.Fprintln(os.Stderr, "connect:", err)
		os.Exit(1)
	}

	step("select_literals", []string{"select", "int", "text", "bool"}, func() (record, error) {
		return rowsOf(ctx, conn, "SELECT 1 AS one, 'x' AS s, true AS b")
	})
	step("create_table", []string{"ddl"}, func() (record, error) {
		return okOf(ctx, conn, "CREATE TABLE drv_smoke (id INT PRIMARY KEY, name TEXT NOT NULL, qty BIGINT, ok BOOLEAN)")
	})
	step("insert_bolt", []string{"dml", "parameter", "int", "text", "bool", "bigint"}, func() (record, error) {
		return rowcountOf(ctx, conn, insert, 1, "bolt", int64(9000000000), true)
	})
	step("insert_nut_null", []string{"dml", "parameter", "int", "text", "bool", "null"}, func() (record, error) {
		return rowcountOf(ctx, conn, insert, 2, "nut", nil, false)
	})
	step("select_with_params", []string{"select", "parameter", "int", "text", "bool", "bigint", "null"}, func() (record, error) {
		return rowsOf(ctx, conn, "SELECT id, name, qty, ok FROM drv_smoke WHERE id = $1 OR name = $2 ORDER BY id", 2, "bolt")
	})
	step("update_with_params", []string{"dml", "parameter", "bigint"}, func() (record, error) {
		return rowcountOf(ctx, conn, "UPDATE drv_smoke SET qty = $1 WHERE id = $2", 7, 1)
	})
	step("delete_with_params", []string{"dml", "parameter"}, func() (record, error) {
		return rowcountOf(ctx, conn, "DELETE FROM drv_smoke WHERE id = $1", 2)
	})
	step("transaction_commit", []string{"transaction", "dml", "parameter"}, func() (record, error) {
		tx, err := conn.Begin(ctx)
		if err != nil {
			return nil, err
		}
		r, err := rowcountOf(ctx, tx, insert, 3, "pin", 4, true)
		if err != nil {
			_ = tx.Rollback(ctx)
			return nil, err
		}
		return r, tx.Commit(ctx)
	})
	step("select_after_commit", []string{"select"}, func() (record, error) {
		return rowsOf(ctx, conn, "SELECT id FROM drv_smoke ORDER BY id")
	})
	step("transaction_rollback", []string{"transaction", "dml", "parameter"}, func() (record, error) {
		tx, err := conn.Begin(ctx)
		if err != nil {
			return nil, err
		}
		r, err := rowcountOf(ctx, tx, insert, 4, "gone", 0, true)
		if rbErr := tx.Rollback(ctx); err == nil {
			err = rbErr
		}
		return r, err
	})
	step("select_after_rollback", []string{"select", "parameter"}, func() (record, error) {
		return rowsOf(ctx, conn, "SELECT id FROM drv_smoke WHERE id = $1", 4)
	})
	step("error_duplicate_key", []string{"error", "dml", "parameter", "null"}, func() (record, error) {
		return rowcountOf(ctx, conn, insert, 1, "dup", nil, nil)
	})
	step("error_syntax", []string{"error"}, func() (record, error) {
		return okOf(ctx, conn, "SELECT FROM FROM drv_smoke")
	})
	step("select_after_errors", []string{"select"}, func() (record, error) {
		return rowsOf(ctx, conn, "SELECT id FROM drv_smoke ORDER BY id")
	})
	step("drop_table", []string{"ddl"}, func() (record, error) {
		return okOf(ctx, conn, "DROP TABLE drv_smoke")
	})
	step("known_default_mode_select", []string{"select", "parameter", "int"}, func() (record, error) {
		// A separate connection with pgx's default query execution mode, for the known failure.
		dcfg, err := pgx.ParseConfig(dsn)
		if err != nil {
			return nil, err
		}
		dconn, err := pgx.ConnectConfig(ctx, dcfg)
		if err != nil {
			return nil, err
		}
		defer dconn.Close(ctx)
		return rowsOf(ctx, dconn, "SELECT $1::int AS n", 7)
	})

	if err := conn.Close(ctx); err != nil {
		fmt.Fprintln(os.Stderr, "close:", err)
		os.Exit(1)
	}
	emit(record{"step": "close", "tags": []string{"close"}, "ok": true})
}

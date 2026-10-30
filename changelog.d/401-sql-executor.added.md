`lattice-exec` (#119, #121, #401): a deterministic, bounded executor for the single-table SQL subset. Bound
`SELECT`, `INSERT`, `UPDATE` and `DELETE` (with `RETURNING` and `AS OF`) run against snapshot-isolated
row-store transactions with `BEGIN`, `COMMIT`, `ROLLBACK`, PostgreSQL command tags, `23502`, `23505`,
`22003`, `22012` and `40001` errors, stale-plan refusal, the actor's row policy and per-statement
deadline, memory and cancellation limits. `CREATE TABLE` and `DROP TABLE` register tables; column
definitions are in memory only. A `QueryHandler` adapter is in `lattice-api` and is not wired into
`latticed`. Of the 669 records in the 21 sqllogictest
scripts, 433 pass; the 236 failures are listed with reasons (aggregates, joins, `GROUP BY`, `DISTINCT`,
subqueries, `SELECT` without `FROM` and DDL the binder or executor refuses, and `numeric(p, s)` modifiers).
No exit criterion is claimed.

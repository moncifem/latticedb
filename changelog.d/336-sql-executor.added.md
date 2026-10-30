`lattice-exec` (#119, #121): a deterministic, bounded executor for the single-table SQL subset. Bound
`SELECT`, `INSERT`, `UPDATE` and `DELETE` (with `RETURNING` and `AS OF`) run against snapshot-isolated
row-store transactions with `BEGIN`, `COMMIT`, `ROLLBACK`, PostgreSQL command tags, `23502`, `23505`,
`22003`, `22012` and `40001` errors, stale-plan refusal, the actor's row policy and per-statement
deadline, memory and cancellation limits. `CREATE TABLE` and `DROP TABLE` register tables; column
definitions are in memory only. A `QueryHandler` adapter is in `lattice-api` and is not wired into
`latticed`. Of the 64 records in the five sqllogictest scripts, 52 pass; the 12 failures are joins,
aggregates, `GROUP BY` and `DISTINCT`, which the binder refuses. No exit criterion is claimed.

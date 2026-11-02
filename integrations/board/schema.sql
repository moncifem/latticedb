-- Agent coordination board: tables used by the board helper (integrations/board/board).
-- Written for the single-table SQL subset of the Lattice alpha (docs/alpha.md, docs/sql-subset.md).
-- Idempotent: CREATE TABLE IF NOT EXISTS keeps existing rows and their definitions.
-- The helper seeds the event counter itself (board init), because the subset has no
-- INSERT ... ON CONFLICT and no way to test for a row in SQL.

-- One row per counter. The row named 'events' hands out event ids.
CREATE TABLE IF NOT EXISTS counters (
    name text PRIMARY KEY,
    n bigint NOT NULL
);

-- One row per unit of work. state is open, claimed, done, dead (out of attempts), cancelled or
-- awaiting_approval (waits for board approve). owner stays NULL until a
-- claim wins. claim_seq holds the event id of the winning claim, and done_seq the event id of
-- the transaction that marked the task done. created_seq is the event id of the add event; it
-- orders tasks by creation, since created_at is in seconds and cannot break ties. parent_id is a
-- plain text column: the subset has no foreign keys, so the helper checks the parent exists.
CREATE TABLE IF NOT EXISTS tasks (
    id text PRIMARY KEY,
    title text NOT NULL,
    state text NOT NULL DEFAULT 'open',
    owner text,
    priority integer NOT NULL DEFAULT 0,
    parent_id text,
    created_by text NOT NULL,
    body text NOT NULL DEFAULT '',
    created_at bigint NOT NULL,
    updated_at bigint NOT NULL,
    claim_seq bigint NOT NULL DEFAULT 0,
    done_seq bigint NOT NULL DEFAULT 0,
    created_seq bigint NOT NULL DEFAULT 0
);

-- Append-only progress log. Ids come from counters ('events'): they are unique and rise in
-- commit order, so a reader that polls "id > last seen" never misses an event. Ids can skip
-- (a claim that loses still uses one), but no committed event is skipped.
-- kind is add, claim, note, done, decision, fail, expired, release, approve, cancel, block, unblock
-- or fenced (a write refused because its agent lost the lease; informational).
CREATE TABLE IF NOT EXISTS events (
    id bigint PRIMARY KEY,
    task_id text NOT NULL,
    author text NOT NULL,
    kind text NOT NULL,
    body text NOT NULL DEFAULT '',
    created_at bigint NOT NULL
);

-- Decisions taken on a task. The id is the same event id as the matching 'decision' event.
CREATE TABLE IF NOT EXISTS decisions (
    id bigint PRIMARY KEY,
    task_id text NOT NULL,
    author text NOT NULL,
    body text NOT NULL,
    created_at bigint NOT NULL
);

-- Lease, retry and dependency columns, in a second table because the alpha executes no ALTER TABLE
-- (crates/lattice-exec/src/session.rs refuses it), so tasks cannot grow columns. One row per task,
-- keyed by the task id, always read and written with single-table statements. A task with no row
-- here (made by an older helper) counts as: no lease, 0 attempts, 3 allowed, no backoff, no
-- blockers; the helper adds the row when it first needs it, and board migrate adds all of them.
-- lease_until, heartbeat_at and not_before are epoch seconds on the DATABASE clock (now()), 0 when
-- unset, and are compared only with the database clock. blocked_by is a comma separated list of
-- task ids (the subset has no arrays and no joins), at most 32 ids. The fence token is
-- tasks.claim_seq, the event id of the winning claim, which rises with every claim. attempts counts
-- failures and lease expiries, never claims.
CREATE TABLE IF NOT EXISTS task_leases (
    task_id text PRIMARY KEY,
    lease_until bigint NOT NULL DEFAULT 0,
    heartbeat_at bigint NOT NULL DEFAULT 0,
    attempts integer NOT NULL DEFAULT 0,
    max_attempts integer NOT NULL DEFAULT 3,
    not_before bigint NOT NULL DEFAULT 0,
    last_error text NOT NULL DEFAULT '',
    blocked_by text NOT NULL DEFAULT ''
);

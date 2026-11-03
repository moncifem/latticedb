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

-- board bench (integrations/board/board-bench): an experiment compares agent configurations (arms) on a
-- suite of tasks, several repeats each. Four tables, all read and written one table at a time (the
-- subset has no joins), with the labels repeated in each row so a reader needs no join.
--
-- bench_experiments: one row per experiment. id is the experiment name. seed is the seed of the
-- randomised attempt order (recorded so the order can be reproduced). planned is the number of attempts
-- the matrix asked for. suite_sha is the sha256 of the suite file the attempts read.
CREATE TABLE IF NOT EXISTS bench_experiments (
    id text PRIMARY KEY,
    suite text NOT NULL,
    suite_sha text NOT NULL,
    seed bigint NOT NULL,
    repeats integer NOT NULL,
    planned integer NOT NULL,
    harness_version text NOT NULL,
    created_by text NOT NULL,
    created_at bigint NOT NULL
);

-- bench_arms: one row per arm. id is experiment.arm. config_sha is the sha256 of the arm's definition.
CREATE TABLE IF NOT EXISTS bench_arms (
    id text PRIMARY KEY,
    experiment_id text NOT NULL,
    arm text NOT NULL,
    agent text NOT NULL,
    config_sha text NOT NULL,
    created_at bigint NOT NULL
);

-- bench_tasks: one row per suite task of an experiment, with the digest of its fixture tree.
CREATE TABLE IF NOT EXISTS bench_tasks (
    id text PRIMARY KEY,
    experiment_id text NOT NULL,
    task_key text NOT NULL,
    fixture_digest text NOT NULL,
    created_at bigint NOT NULL
);

-- bench_attempts: one row per attempt, made as 'planned' when the experiment starts and filled in once
-- by `board bench-record`, which only the holder of the board task's lease may call. board_task_id is
-- the board task that runs the attempt. outcome is planned, pass, fail, timeout or error; error means
-- the harness failed (not the agent) and is never counted against an arm. flaky is 1 when the grader's
-- reruns disagreed, tampered is the number of protected files the agent had changed. duration_ms, cost_usd
-- and tokens stay NULL unless the agent reported them. The table has no fence column on purpose: the
-- fence guards the write, it is not data.
CREATE TABLE IF NOT EXISTS bench_attempts (
    board_task_id text PRIMARY KEY,
    experiment_id text NOT NULL,
    arm text NOT NULL,
    task_key text NOT NULL,
    repeat_idx integer NOT NULL,
    order_idx integer NOT NULL,
    outcome text NOT NULL DEFAULT 'planned',
    flaky integer NOT NULL DEFAULT 0,
    tampered integer NOT NULL DEFAULT 0,
    duration_ms bigint,
    cost_usd double precision,
    tokens bigint,
    grader_exit integer,
    grader_runs text NOT NULL DEFAULT '',
    summary text NOT NULL DEFAULT '',
    harness_version text NOT NULL,
    created_at bigint NOT NULL,
    recorded_at bigint NOT NULL DEFAULT 0
);

-- Live agent activity (`board activity`, hooks/activity-hook.py, the console's Now and Agent screens). Only
-- written when BOARD_ACTIVITY=1. Two tables, read one at a time (the subset has no joins).
--
-- agent_activity: one row per agent and session, id = agent '/' session, upserted by the helper. It answers
-- "what is each agent doing right now": the last tool (name only), a short sanitised target, the outcome
-- class (ok, error or denied), the task the agent holds a lease on ('' when none), the count of tool calls
-- seen, and state (working, idle or ended). last_seen and started_at are DATABASE-clock epoch seconds. Rows
-- of an agent older than seven days are deleted by that agent's own next write, so the table does not grow
-- without bound; `board activity-purge` deletes on request. task_id is '' rather than NULL on purpose: the
-- console treats both as "none" and the helper never has to write a typed NULL.
CREATE TABLE IF NOT EXISTS agent_activity (
    id text PRIMARY KEY,
    agent text NOT NULL,
    session text NOT NULL,
    task_id text NOT NULL DEFAULT '',
    tool text NOT NULL,
    target text NOT NULL DEFAULT '',
    outcome text NOT NULL DEFAULT 'ok',
    state text NOT NULL DEFAULT 'working',
    tool_calls bigint NOT NULL DEFAULT 0,
    last_seen bigint NOT NULL,
    started_at bigint NOT NULL
);

-- agent_activity_log: the timeline, a ring of the newest rows of each agent (200 by default; the helper
-- deletes older ones in the write that adds a row). One row is one WRITE, not one tool call: the helper
-- coalesces the calls seen since its last write, so calls says how many that row stands for and mix says
-- which tools ("Bash=3,Edit=2"; at most 200 characters). tool, target and outcome are those of the newest
-- call. The id comes from counters('activity'), which rises in commit order, so a reader that polls
-- "id > last seen" misses nothing. at is the DATABASE clock.
CREATE TABLE IF NOT EXISTS agent_activity_log (
    id bigint PRIMARY KEY,
    agent text NOT NULL,
    session text NOT NULL,
    task_id text NOT NULL DEFAULT '',
    tool text NOT NULL,
    target text NOT NULL DEFAULT '',
    outcome text NOT NULL DEFAULT 'ok',
    calls bigint NOT NULL DEFAULT 0,
    mix text NOT NULL DEFAULT '',
    at bigint NOT NULL
);

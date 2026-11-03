# Board console

A local web page for the agent board in [`integrations/board`](../board/CONVENTIONS.md), read-only unless you start it with
`--allow-writes` (see "Write mode: decisions by a person"): which
agents are active, what needs attention, the work queue (what can be claimed, what is blocked, which
leases are running out, what is dead), a live event feed, a scorecard per agent, and the experiments of
`board-bench` (agent configurations compared on the same tasks).

Status: alpha. It reads by default. It listens on `127.0.0.1` and nowhere else. It has no login, so anyone
who can open a connection to that port on your machine can read the board through it (and, in write mode, anyone who also
has the write token can change it).

It is Python 3.10 or newer from the standard library only (no pip install, no CDN, no framework). The
page is plain HTML, one script and one stylesheet, and makes requests only to the server that served it.

## Run it

See the screens with generated data and no database:

```sh
integrations/console/lattice-console serve --demo
```

Open the address it prints (default `http://127.0.0.1:8765/`). Every screen shows the banner `DEMO DATA,
not your board`. The demo board is made up (seeded generator, same data on every run); about ten events
are held back and one more appears every four seconds so the feed has something to do.

Read a real board (a running `latticed` and `psql` on `PATH`, as for the board helper):

```sh
export LATTICE_BOARD_URL=postgresql://dev@127.0.0.1:5432/lattice   # or PGHOST, PGPORT, PGUSER, PGDATABASE
export PGPASSWORD=...                                               # or a line in ~/.pgpass
integrations/console/lattice-console serve
```

Options: `--port N` (default 8765, `0` picks a free port), `--url URL` (instead of the environment),
`--stale-minutes N` (default 30), `--open-hours N` (default 24), `--demo`. There is no option to bind
another address: asking for one is an error. `BOARD_PSQL` names another `psql` program, as in the helper.
The same run works as `python3 -m console serve` from `integrations/console`.

Tests: `python3 -m unittest discover -s integrations/console/tests` (about four seconds, no database).
`integrations/console/test.sh` is the end-to-end check against a real `latticed` (see Known gaps).

## Screens

- **Now.** Task counts by state; a needs-attention strip; the agents table. An agent is every event
  author and every task owner. `working` holds a claimed task with an event in the last N minutes,
  `stale` holds claimed tasks and none has, `idle` holds none. Attention items: stale claims (claimed,
  no event or update for more than N minutes), tasks with claim events from two or more authors, a
  claimed task whose owner differs from the author of its last claim event, and open tasks older than
  `--open-hours`. The helper writes only the winning claim, so the second and third items can only
  appear after a write that bypassed it. Three more items come from the queue: a claimed task whose
  lease ran out and was not reaped yet, a task that is `dead` (out of attempts), and an open task that
  waits for a blocker that is dead, cancelled, missing, or for a dependency cycle, so it can never run.
  Each rule is printed on the page.
- **Live activity on Now and the Agent screen.** When agents report what they do (`BOARD_ACTIVITY=1`, see
  "Agent activity" in `integrations/board/CONVENTIONS.md`), the agents table gains three columns: tool activity,
  doing now and a sparkline of the last hour. An agent that reports but never touched the board is listed too.
  States, from the last report of the agent's newest session: `working` (a report in the last 120 s), `quiet`
  (the agent holds a task and the last report is older), `stuck?` (older than `--quiet-minutes`, default 10,
  while a task the agent holds has a live lease; the page says "no tool activity for 12 min", never that the
  agent is stuck), `idle` (holds no task). The needs-attention strip gains "claimed but no tool activity
  (heartbeat alive)". `#/agent/NAME` shows the agent's sessions, its timeline (the newest 200 rows, filtered by
  tool, outcome or task, each linked to its task), a tool mix, and an active time estimate (the sum of gaps under
  60 seconds between the writes the hook made, labelled as an estimate). `/api/agent?name=...` takes `tool`,
  `outcome`, `task`, `limit` (up to 200) and `after` (an activity log id: the rows after it). The scorecard gains
  a diagnostic, active time per done task, for the tasks whose whole span the console watched, with n and
  "not enough data" under 5 tasks; it is listed by name and not ranked. The demo has a working, a quiet, a
  stuck-looking and an idle agent. A board without the activity tables, or an account that may not read them,
  shows a note and every other screen works.
- **Queue** (`#/queue`). Six sections built from `tasks` and `task_leases`: runnable now (what
  `board next` would take, highest priority and oldest first), blocked (with each blocker and its state;
  "for ever" when a blocker can never finish), claimed (owner, lease time left or overdue, age of the last
  heartbeat, attempts), retrying (attempts used, when the retry delay ends, last error), dead letter
  (attempts and last error), and awaiting approval. Lease and delay times are on the database clock: the
  console reads it with `SELECT CAST(extract(epoch FROM now()) AS bigint)` and, if that fails, falls back
  to this machine's clock and says so on the page. A board without the `task_leases` table (made before
  leases existed) shows every task with default values and a note to run `board init`.
- **Task** (`#/task/ID`). Parent chain (root first), children, events in order, decisions. A parent
  id that names no task shows as an orphan, and a parent loop is cut and labelled; neither can hang
  the page.
- **Feed** (`#/feed`). Events newest first, filters for agent, kind and task id. The page asks for
  `id > last seen`, every 2 seconds while the tab is visible and every 10 seconds when it is hidden.
  New events are counted in a bar ("N new events") and are added at the top only when you are at the
  top of the page and have not paused; otherwise you click the bar. Pause stops the list from changing,
  not the polling. "Load older events" pages backwards.
- **Scorecard** (`#/scorecard`). Eight metrics, for all agents and per agent (the agent who made the
  first claim). Every cell shows its sample size n. A proportion shows a Wilson 95% interval; a
  duration or count shows median, p10, p90 and a bootstrap interval over tasks (1000 resamples, fixed
  seed, so a reload gives the same numbers). Under the minimum n a cell says `not enough data (n < min)`
  and shows no number. Click a metric name for its definition and caveat.

| Metric | Definition | Minimum n |
|---|---|---|
| Completion rate | top-level tasks created in the window that are done | 20 |
| Time to claim | creation to first claim event; open tasks left out and counted | 15 |
| Time to done | first claim event to done event | 15 |
| Stale-claim rate | claimed tasks idle longer than N minutes, over claimed and done tasks (a snapshot of now) | 20 |
| Claim contention | tasks with claims by two or more authors, over tasks with a claim (a lower bound) | 20 |
| Tasks with a handoff | done tasks claimed by two or more authors | 20 |
| Decisions per done task | a volume count, not a quality signal | 20 |
| Notes per task | diagnostic only | 30 |

Two metrics from the research are listed as not measurable, with the schema change each would need:
**rework / reopen rate** (no `reopen` event kind, and a done task cannot leave `done`) and **spot-check
agreement** (no `verify` event or audit record). They are not estimated.

The compare view (window A against the period just before it) shows differences in completion rate,
median time to claim and median time to done as A minus B with a bootstrap interval, only when each
window has at least 30 values, and says so otherwise. The two windows contain different tasks, so the
comparison is unpaired; a paired comparison does not apply. These numbers describe how the board was
used. They do not say whether the finished work is correct.

- **Experiments** (`#/experiments`, `#/experiment/NAME`). The experiments made with
  [`board-bench`](../board/bench/README.md): a list, and for one experiment an arm table, the paired comparison of
  every two arms, a task-by-arm grid and the harness errors. The numbers are computed by the same code as
  `board-bench report` (`integrations/board/benchlib`, which must sit next to the console; without it the screen
  says so), so the page and the command cannot disagree. The honesty rules are part of the page. Every rate is
  printed from one string the server builds, with its interval and n (`23% (95% CI 13% to 38%), n = 39 tasks,
  113 attempts`); the page script formats no percentage of its own. pass@1's interval counts tasks, not attempts,
  and the page says when counting attempts would have been too narrow. A comparison is **inconclusive** unless the
  interval of the paired difference excludes zero, the exact McNemar test agrees and at least 30 tasks are paired, and "inconclusive" is shown as
  a result, not hidden. The grid marks cells with flaky grader runs, attempts not recorded yet, timeouts and
  protected files the agent changed; harness errors (a fixture that did not match its digest, a grader that could
  not run) are listed apart and are in no rate. `?rule=any` on the API changes the rule that decides when a task
  counts as solved. The demo has an experiment where one arm looks 20 points better and the report says
  inconclusive, a clear difference on 40 tasks with flaky rows, errors and unrecorded attempts, and one that has
  not started.

## Write mode: decisions by a person

People need an inbox of what is waiting for them and a safe way to act on it from the browser. Write mode adds the
**Inbox** screen (`#/inbox`) and six decisions: **approve**, **reject**, **cancel**, **requeue**, **priority** and
**note**. It is off unless you ask for it:

```sh
integrations/console/lattice-console serve --allow-writes --actor alice     # a real board
integrations/console/lattice-console serve --demo --allow-writes            # generated data, decisions simulated
```

The console prints a **write token** once, to the terminal, and nothing else prints it. Open the Inbox, paste the token
into the box under the header (it is kept in the page's memory, never in storage or the URL, and a reload forgets it),
and the buttons work. Without `--allow-writes` the page shows no buttons and the server answers **403 to every POST**.

What the Inbox lists is only what needs a person, each with its rule written under "What puts a task here": a task waiting
for approval, a dead letter, a claimed task whose lease is alive while its owner's tool activity stopped (the
"stuck?" state of the activity feature, self-reported), and an open task that waits for a blocker that can never finish
or for longer than `--open-hours`. Each card shows the task's text as plain text before any button is pressed: its
body, who produced it, its last five events and a short content hash.

### Decisions and the rules behind them

| Decision | Applies to | Rule |
|---|---|---|
| approve | waiting for approval | needs the content hash shown on the card; refused if the text changed since, and refused for the person who produced the task |
| reject | waiting for approval | needs a comment; cancels the task; refused for the person who produced it |
| cancel | open, claimed, waiting | the holder's next write is refused by its fence |
| requeue | dead, claimed | opens the task again with its attempts reset and the lease dropped |
| priority | open, claimed, waiting, dead | a whole number |
| note | any task | needs a comment; an event only |

- **One audited path.** The console never writes SQL. Each decision is one call of `board act`
  ([`../board/CONVENTIONS.md`](../board/CONVENTIONS.md), "`board act`"), run as `human.NAME` (`--actor NAME`, the OS user
  when you give none). The helper writes the task change, an event of the same kind (author `human.NAME`) and a row in
  `human_actions` (who, what, which task, the comment, the sha-256 of the task's title and body at that moment, the id of
  the event, the time) in **one transaction**, so there is no state change without its audit record. The page cannot name
  the person: the identity is fixed when the console starts.
- **Self-approval is refused by the helper, not hidden by the page.** `approve` and `reject` refuse when the task's
  creator or owner is the person (as `human.NAME` or as plain `NAME`), or when `NAME` wrote any event on the task. The
  button is also disabled with an explanation, but the refusal does not depend on it. The transaction repeats the test
  against the row it writes.
- **An approval is bound to the text that was shown.** The card carries `sha256(title + "\n" + body)`; the approval sends it
  back and the helper recomputes it from the database. A task edited in between is refused as stale, with a clear message.
- **A comment is required to reject and to note**, so the reason is on the record.
- **No bulk approve, and no approve on a timer, ever.** Bulk exists only for cancel and requeue (at most 25 tasks): the
  first request is a dry run that returns the count and the tasks, the second must send that count back (`confirm`), and
  is refused if the set changed.
- **Each decision carries a request id** made by the browser (random, one per dialog, reused if you press Confirm again
  after an error). Repeating an id changes nothing and answers with the same event: the console remembers the last 512, and
  the database remembers all of them (`human_actions.request_id` is the primary key). An id reused for a different decision is
  refused.
- **Answers are fixed sentences.** The helper's own messages (stderr) are never passed to the browser; the console maps
  `done` / `noop` / `refused CODE` to its own text.

### Threat model of write mode

What each check is for, and the test that fails if it is removed (`tests/test_actions_mutations.py` plants each fault):

| Threat | Check |
|---|---|
| A web page in your browser posts to `127.0.0.1` (CSRF) | The write token travels in a custom header, which a page of another origin cannot add without a CORS preflight, and this server never answers one. The `Origin` must be exactly this console's own (`http://127.0.0.1:PORT` or `http://localhost:PORT`); a missing or `null` Origin is refused. The body is JSON only and the content type is checked. |
| DNS rebinding: a hostile name that resolves to `127.0.0.1` | The `Host` header is checked on **every** route, reads and POSTs, before anything else is answered (421). The token is never served to the page: you paste it, so a rebound page has nothing to read. |
| Guessing the token | 256 bits from `secrets`, compared with `hmac.compare_digest`, new on every start; five wrong tokens lock every write for a minute (the right token too), and writes are limited to 60 a minute. |
| State change by a `GET` | `GET` and `HEAD` never reach the write path; `/api/act` is not readable. |
| Smuggled or odd requests | Strict JSON (no duplicate keys, no `NaN`), at most 8 KiB, an exact `Content-Length` (no chunked bodies), unknown fields and unknown actions are 400, every field matched in full. |
| An agent approving its own work | The helper's self-approval rule (above) and the reserved `human.` names: every `board` command except `act` refuses a `BOARD_AUTHOR` that starts with `human.`, and `act` accepts nothing else. |
| Output of a failing helper reaching the page | Dropped; fixed sentences only. The token and the connection settings are never printed or logged. |
| New script or external request | None: the Content-Security-Policy is unchanged, the dialog is built with DOM calls, and a test scans the script. |

**What is not protected.** The identity is a name this process was given: `--actor` is whatever you type, and at the
database level every author on this board is self-declared (`docs/alpha.md`). The self-approval rule compares names, so a
person who creates a task as `bob` with the CLI and approves it as `--actor carol` is not stopped, and an agent that
sets `BOARD_AUTHOR=human.x` for `board act` can write as a person (it would need the database account's write grant, which
it has). Anyone who can read the terminal where the token was printed, or the page's memory, or who can run `board act`
themselves, can decide. The console has no TLS and no login; a local attacker with the token can write. The
`human_actions` table is append-only by use, not enforced by the database: the account that runs the helper can write
to it, so anything holding that account can edit the audit trail.

Demo mode (`--demo --allow-writes`) answers `board act` from the generated board in memory: the toast says "DEMO,
simulated", nothing outside the process changes, and the board stops growing so event ids stay in order. Start it with
`--actor maintainer` to see the self-approval refusal on the demo's waiting task.

## What it reads

Fixed `SELECT` statements over `tasks`, `events`, `decisions` and `task_leases` (and, for live activity,
`agent_activity` and `agent_activity_log`: the newest 500 sessions by `last_seen`, then the log newest first
once and `id` above the last one it holds after that, at most 20000 rows kept) through `psql`, one
table per statement (the alpha subset has no joins) and no aggregates (the board conventions say there
are none), so counts and statistics are computed in Python. The one other statement is the database
clock, `SELECT CAST(extract(epoch FROM now()) AS bigint);`. The Experiments screen reads `bench_experiments`,
`bench_arms`, `bench_tasks` and `bench_attempts` the same way (at most 20000 attempts of one experiment, filtered
by an experiment name checked against `[A-Za-z0-9_-]{1,48}`); a board without those tables shows a note to run
`board init`. Every statement is a SELECT, so a connect-phase failure that the alpha server causes
([#867](https://github.com/moncifem/latticedb/issues/867)) is asked again, up to six times. At the first request it loads the newest 5000
tasks, their lease rows and the newest 20000 events, then asks only for events with `id` above the last
one it holds. The cache refreshes at most once a second however many browsers poll. When a limit cuts
data off, the page says so. Times are shown in UTC. "Now" is the database clock when it answers (the
queue needs it) and this machine's clock otherwise; `meta.clock` in every answer says which.

## Security model

Board text is written by agents and is untrusted.

- The script builds the page with `createElement` and `textContent`. It never uses `innerHTML`,
  `outerHTML`, `insertAdjacentHTML`, `document.write`, `eval` or inline handlers, and a unit test scans
  the shipped files for those tokens (and for external URLs, `localStorage` and similar).
- Every response carries `Content-Security-Policy: default-src 'none'; script-src 'self'; style-src
  'self'; connect-src 'self'; img-src 'self' data:; base-uri 'none'; form-action 'none';
  frame-ancestors 'none'`, `X-Content-Type-Options: nosniff`, `Referrer-Policy: no-referrer` and
  `Cache-Control: no-store`. There are no CORS headers.
- Bound address `127.0.0.1` only. A `Host` header other than `127.0.0.1:PORT` or `localhost:PORT` gets
  421 (DNS rebinding), on every route and every method. `GET` and `HEAD` read; `POST` exists for two routes and only in
  write mode (403 everywhere otherwise, see "Write mode"); the other methods get 405. Query parameters are checked
  strictly (unknown, repeated or malformed ones are 400; cursors are integers up to 2^53; `limit` is 1
  to 500; names and ids are limited to letters, digits, `.`, `_`, `-`; the patterns are matched in full,
  so a trailing newline is refused). Scorecard window edges are rounded down to a minute.
- Every error response, including the ones the Python HTTP library would write itself (a garbage
  request line, 414, 431, an unknown method such as `PROPFIND`, which is 501), is a short JSON body
  with the same security headers.
- Load limits: a connection that sends nothing for 10 seconds is dropped, and at most 32 connections are
  served at once (the next one gets 503 with `Retry-After`). After a failed read of the database the
  console answers from that failure for 2 seconds instead of starting `psql` again, and at most 16
  scorecards are cached.
- Database access: `psql` is started with an argument list (no shell), a 10 second deadline and an 8 MiB
  output cap. The SQL goes in on standard input and must match a fixed `SELECT` pattern with integers
  that were validated, so it cannot carry board text. The connection URL must not contain a password
  (it is refused, not moved). The URL is percent-decoded, repeatedly, before the check, because libpq
  decodes keywords (`pass%77ord=` is a password); the password comes from `PGPASSWORD` or `~/.pgpass`,
  never from an argument. `psql` runs in its own process group, which is killed on a timeout, and its
  environment gets `PGOPTIONS` with `-c default_transaction_read_only=on -c statement_timeout=8000`
  appended to any value you set. `latticed` ignores startup options today (see `docs/sql-subset.md`),
  so there the read-only guarantee is the SQL pattern above; PostgreSQL honours both options. Output is read as NUL-separated values, so a note with tabs or newlines cannot shift
  columns.
- Errors: a failing query gives a short fixed hint and the first line of `psql`'s message with
  credentials removed. The console prints the address it serves and nothing about the connection.
- Text fields are cut at 4096 characters in the API. A JSON answer over 4 MiB is an error, and the
  answers are built to stay under it: a feed page shrinks to fit (`has_more` says rows were left out),
  following the feed with `after` returns the oldest matching events first and a cursor that stops
  before the first one left out, so a poll never skips an event and always advances, and a task answer
  carries its newest 500 events and 500 decisions (and 200 children) within a byte budget, with the
  totals and a truncated flag.

The console does not authenticate users and does not use TLS. Do not forward its port.

## Known gaps

- The Experiments screen was run against a real `latticed` (the dev stack) by `integrations/board/tests/bench_devstack.py`,
  which compares its numbers with `board-bench report`; the screen itself was looked at in a browser with the demo data only.

- Not yet run against a real `latticed` in CI. The unit tests use a fake `psql`. `test.sh` is the
  end-to-end check and no workflow runs it yet (the board's own `test.sh` has no CI job either). The
  NUL-separated output format (`psql -z -0`) is what the fake `psql` tests assume.
- Only the newest 5000 tasks, 20000 events and 5000 decisions are held; scorecards and task pages on a
  larger board cover that window only. When events are cut off, the metrics that read event times or
  counts say so in their own caveat (a task whose claim or done event is older has none here), as does
  the decisions metric when the decisions window is full (the source cannot say exactly, so a full window
  is reported as possibly cut). The completion-rate warning compares time since the first claim with the
  median claim-to-done time, and counts open tasks separately.
- No history of its own: stale and "now" figures are snapshots, and nothing is stored. The activity log is a
  ring of 200 writes per agent in the database, so the sparkline and the active time cover what the console
  watched or the ring still holds, not a full hour for a busy agent that the console just started to watch.
- Activity is self-reported by the agent's own hook and sampled (one write per session per 5 seconds by default):
  a compromised agent can report anything, names are self-declared, the newest calls wait for the next write, and
  only Claude Code hooks are verified; other agents appear through `board run` only. No real model session was
  run against it. The Now and Agent screens were looked at in a browser with the demo data.
- Needs `psql` on `PATH`; there is no direct pgwire client.
- No push channel; the page polls.
- The scorecard windows select tasks by creation time, so a task created before a window and finished
  inside it is not in that window.
- Claim contention and handoffs are near zero by construction until the board can reassign work.
- Write mode is verified by unit tests against a fake helper and a fake `psql`, planted faults, the demo in a browser,
  and `integrations/board/tests/act_devstack.py` against the dev stack (the `dev-stack` job). A real browser session
  against a real board with write mode was not run.
- Without `--allow-writes` the console only observes. Leases, heartbeats, retries and dependencies are written by the board
  helper (`board claim`, `heartbeat`, `next`, `reap`, ...); an expired lease shows here until some agent
  runs `board reap` or `board next`, because nothing in the database fires a timer.
- Two clocks: `created_at`, `updated_at` and event times are the agent's clock, while "now", lease and
  retry times are the database clock. A skew between an agent's machine and the server shifts the
  "stale" and "open for" figures by the skew; lease figures are not affected.
- A blocker that is not among the loaded tasks shows as "not in window" when the task window was cut
  (older than the loaded tasks, not known to be missing, and not counted as blocked for ever); with
  nothing cut it is "missing".
- The queue is computed from the loaded window like the other screens. The console was run by hand
  against the published `0.1.0-alpha.1` image (SCRAM over TLS): `/api/queue` answered on the database
  clock and `/api/now` carried the new attention items. `integrations/console/test.sh` (own latticed)
  was not run, and nothing was run against current `main`.
- Who approved a task is only the author name an agent gave itself; the board cannot check it (see
  `integrations/board/CONVENTIONS.md`, Limits).

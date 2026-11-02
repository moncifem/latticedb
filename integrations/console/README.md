# Board console

A read-only web page for the agent board in [`integrations/board`](../board/CONVENTIONS.md): which
agents are active, what needs attention, the work queue (what can be claimed, what is blocked, which
leases are running out, what is dead), a live event feed, and a scorecard per agent.

Status: alpha. It only reads. It listens on `127.0.0.1` and nowhere else. It has no login, so anyone
who can open a connection to that port on your machine can read the board through it.

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

## What it reads

Fixed `SELECT` statements over `tasks`, `events`, `decisions` and `task_leases` through `psql`, one
table per statement (the alpha subset has no joins) and no aggregates (the board conventions say there
are none), so counts and statistics are computed in Python. The one other statement is the database
clock, `SELECT CAST(extract(epoch FROM now()) AS bigint);`. At the first request it loads the newest 5000
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
  421 (DNS rebinding). Only `GET` and `HEAD` are served; the rest get 405. Query parameters are checked
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

- Not yet run against a real `latticed` in CI. The unit tests use a fake `psql`. `test.sh` is the
  end-to-end check and no workflow runs it yet (the board's own `test.sh` has no CI job either). The
  NUL-separated output format (`psql -z -0`) is what the fake `psql` tests assume.
- Only the newest 5000 tasks, 20000 events and 5000 decisions are held; scorecards and task pages on a
  larger board cover that window only. When events are cut off, the metrics that read event times or
  counts say so in their own caveat (a task whose claim or done event is older has none here), as does
  the decisions metric when the decisions window is full (the source cannot say exactly, so a full window
  is reported as possibly cut). The completion-rate warning compares time since the first claim with the
  median claim-to-done time, and counts open tasks separately.
- No history of its own: stale and "now" figures are snapshots, and nothing is stored.
- Needs `psql` on `PATH`; there is no direct pgwire client.
- No push channel; the page polls.
- The scorecard windows select tasks by creation time, so a task created before a window and finished
  inside it is not in that window.
- Claim contention and handoffs are near zero by construction until the board can reassign work.
- The console only observes. Leases, heartbeats, retries and dependencies are written by the board
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

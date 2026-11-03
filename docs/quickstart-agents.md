# Agents quickstart: a task queue that workers run, in ten minutes

This takes you from nothing to a queue of tasks that two worker processes take, run and mark done while you
watch a web page. Alpha software: read "What is not there" at the end before you point it at real work.

You need Docker with Compose v2, `psql`, `openssl`, Python 3.10 or newer, and this repository. The
agent in step 6 can be your own script; `claude` or `codex` is optional.

## 1. A local server and an account

```sh
dev/lattice-dev.sh up
dev/lattice-dev.sh account add board \
  --read counters,tasks,events,decisions,task_leases,bench_experiments,bench_arms,bench_tasks,bench_attempts,agent_activity,agent_activity_log \
  --write counters,tasks,events,decisions,task_leases,bench_experiments,bench_arms,bench_tasks,bench_attempts,agent_activity,agent_activity_log \
  --ddl counters,tasks,events,decisions,task_leases,bench_experiments,bench_arms,bench_tasks,bench_attempts,agent_activity,agent_activity_log
```

The first five tables are the board; the next four are used only by `board-bench` (step 9) and the last two only by
agent activity (step 8b). `board init` creates every table in `schema.sql`, so the account needs all eleven unless you
edit the schema.

`up` prepares the stack: a certificate authority and the volumes (its output says the server starts with the
first account). `account add` creates the `board` account, starts the server (it listens on `127.0.0.1:5432`
over TLS with SCRAM) and prints the password once; it is kept in `~/.lattice-dev/accounts/board.env`, which any
process running as you can read, an agent included (see `--scratch-home` in the conventions). To run beside something else, set `LATTICE_DEV_PORT`,
`LATTICE_DEV_PROJECT` and `LATTICE_DEV_HOME` first ([dev stack](alpha.md#dev-stack)).

## 2. Tell the tools how to connect

Run this in every terminal you use below:

```sh
set -a; . ~/.lattice-dev/accounts/board.env; set +a   # PGHOST, PGPORT, PGUSER, PGPASSWORD, ...
export PATH="$PWD/integrations/board:$PATH"            # so `board` is found
export BOARD_AUTHOR=me                                 # your name on the board
export BOARD_STATE_DIR="$HOME/.local/state/lattice-board"
board init
```

`board init` creates the tables and is safe to run again. The password never goes on a command line.

## 3. Watch it

In a second terminal (with step 2 done):

```sh
integrations/console/lattice-console serve
```

Open `http://127.0.0.1:8765/#/queue`. The console only reads, listens on loopback and has no login.

## 4. Put work on the queue

```sh
board add "Write the changelog entry" --priority 2 --body "Summarise the last three commits in CHANGELOG.md"
# prints the new id, for example t6ebed05f9b
board add "Review the changelog entry" --blocked-by t6ebed05f9b --body "Check spelling and the tone"
board add "Add a unit test for the parser" --priority 1 --body "Cover the empty input case"
board add "Rename the config option" --needs-approval --body "Rename retries to max_retries"
board list
```

The review waits for the changelog task. The rename waits for a person: `board approve ID` opens it.
Priority is highest first, equal priorities oldest first.

## 5. Start two workers

In two more terminals (step 2 in each), with `claude` installed and logged in:

```sh
board run --agent claude --owner claude-1 --forever
board run --agent claude --owner claude-2 --forever
```

Each worker claims the best runnable task, keeps its lease alive, runs `claude -p` in the current directory
with a read-and-edit tool set and no shell, and marks the task done or failed. The console shows the queue
move. A stand-in `claude` executable (a script that printed a JSON reply) was used to run this command here;
a real agent costs money, so the real run is yours. Stop a worker with Ctrl-C: it kills its agent and gives
the task back.

To use `codex` instead: `board run --agent codex --owner codex-1 --forever` (it runs `codex exec` in
`workspace-write` mode). To give each task its own checkout: add `--worktree-per-task /path/to/repo`.

## 6. Or run your own command

A worker command gets the task in `TASK_*` variables and a private file:

```sh
cat > my-agent.sh <<'EOF'
#!/bin/sh
echo "working on $TASK_ID: $TASK_TITLE"
words=$(wc -w < "$TASK_BODY_PATH" | tr -d ' ')
printf '{"status":"done","summary":"the body has %s words","notes":["counted the words"]}\n' "$words" > "$TASK_RESULT_PATH"
EOF
chmod +x my-agent.sh
board add "Count the words" --body "one two three four"
board run --owner counter-1 --max-tasks 1 -- ./my-agent.sh
board since 0 | tail -n 6
```

Exit 0 marks the task done, exit 75 asks for a later retry, any other exit fails it (and it is retried
until `--max-attempts`, then it is `dead`). The body is untrusted text in a file, never in the command line or
the environment. The environment is scrubbed: the database password does not reach your script.
The full contract is in [integrations/board/CONVENTIONS.md](../integrations/board/CONVENTIONS.md#board-run-a-worker-for-any-command-or-agent).

## 7. Break things on purpose

```sh
integrations/board/swarm --workers 4 --tasks 24 --fail-rate 0.2 --zombie-rate 0.05
```

Use a board with no other open tasks (the swarm refuses otherwise). It starts four workers with a scripted
agent that sometimes fails, hangs, or kills its own worker, and then checks the event log: no task done twice,
none done by an agent that did not hold it, none lost. It prints a report and exits non-zero on a violation.
Watch the console while it runs.

## 8. Use the board from Claude Code

`integrations/board/board-mcp` serves the board as MCP tools, so a Claude Code session takes a task, heartbeats
and finishes it with tool calls. The tools, the lease rules and the limits are in
[the conventions](../integrations/board/CONVENTIONS.md#board-mcp-the-board-as-mcp-tools). The steps below assume
steps 1 and 2 (a server, the `PG*` settings and `BOARD_AUTHOR` in your shell).

Install the plugin from this checkout. Run `claude plugin validate integrations/board` first if you changed it:

```sh
claude plugin marketplace add "$PWD"
claude plugin install board@lattice
claude mcp list
```

`claude mcp list` should show `plugin:board:board` as `Connected`. Start Claude Code from a shell that has
`BOARD_AUTHOR`, the `PG*` settings and (unless `~/.pgpass` holds it) `PGPASSWORD`, and give each session its own
name: `BOARD_AUTHOR=claude-1 claude`, and a different one (`claude-2`, or a per-session suffix) for every session that runs at the
same time. Two sessions that share a name share one identity on the board, and each one's lease_ids are refused in the other.
In the session:

- `/board:status` shows the queue (`board_queue` and `board_leases`).
- `/board:add Write the release notes -- from the changelog` adds a task; Claude Code asks first.
- `/board:next` takes the next task, works it with notes and heartbeats, and ends with done or fail.
- the `board-worker` agent does the same in its own context.

Without plugins, register the server by hand (the tools are then `mcp__board__board_*`):

```sh
claude mcp add --scope user board -e BOARD_AUTHOR=claude-1 -- python3 "$PWD/integrations/board/board-mcp"
```

**Permissions.** Allow the reads, let Claude Code ask for the writes. The plugin's tool names are
`mcp__plugin_board_board__board_*`:

```json
{
  "permissions": {
    "allow": [
      "mcp__plugin_board_board__board_list",
      "mcp__plugin_board_board__board_show",
      "mcp__plugin_board_board__board_queue",
      "mcp__plugin_board_board__board_since",
      "mcp__plugin_board_board__board_leases"
    ],
    "ask": [
      "mcp__plugin_board_board__board_add",
      "mcp__plugin_board_board__board_next",
      "mcp__plugin_board_board__board_claim",
      "mcp__plugin_board_board__board_note",
      "mcp__plugin_board_board__board_heartbeat",
      "mcp__plugin_board_board__board_done",
      "mcp__plugin_board_board__board_fail",
      "mcp__plugin_board_board__board_release"
    ]
  }
}
```

A task body is text another agent wrote and can try to talk the model into things, so keep the writes behind a
prompt unless a session is unattended on purpose. The plugin's hook asks before `board_add`, `board_done` and
`board_fail` even if you allow the whole server. `/board:next` pre-approves the routine writes for that command
only. Approving, cancelling and force-releasing tasks are not tools: use `board approve` and `board cancel` yourself.

What was run for this section: `claude plugin validate`, `claude plugin marketplace add <path>`,
`claude plugin install board@lattice` and `claude mcp list` against a private Claude Code config directory
(`CLAUDE_CONFIG_DIR`), and the server driven by `integrations/board/tests/mcp_devstack.py` against a dev stack. No
model session was run, so the slash command texts are validated but have not been followed by a model.

Limits: alpha. Agent names are self-declared. The server keeps the fence behind the lease_id, in its process and in a
private state file used only to recover a crashed server's leases (never another running session's). The helper's own
fence files in the state directory can be read by anything that can read your files, so the isolation holds against tool
calls, not against a model with a shell or file tools. The `board-worker` agent has no shell tool, but its Write and Edit
are limited only by Claude Code's permission system. A call cancelled after its claim was written leaves a claim the
server does not know until the lease runs out. Titles and bodies are one line each. Nothing is pushed to the session: the SessionStart
hook prints counts and task ids once, and the agent polls with `board_queue` and `board_since`.

### 8b. See what each agent is doing right now (optional)

The Now screen can show, per agent, the last tool it used, a short label, whether it looks quiet, and a sparkline of
the last hour. This is recorded only if you turn it on, because it stores tool usage; read
[what is and is not stored](../integrations/board/CONVENTIONS.md#agent-activity-what-is-each-agent-doing-right-now)
first. The two activity tables were created by `board init` in step 2 (an older board needs `board migrate`, and
an account made before this feature needs `--read/--write/--ddl` on `agent_activity,agent_activity_log` too).

```sh
export BOARD_ACTIVITY=1                    # off unless this is 1
board activity --tool Edit --target src/main.rs --flush   # a manual row, to see it appear
board activity-list
board activity-purge                       # delete your own rows again
```

For Claude Code, start the session from a shell that has `BOARD_ACTIVITY=1`, `BOARD_AUTHOR` and the `PG*` settings.
The board plugin from step 8 registers the hook; without the plugin, merge
`integrations/board/hooks/settings.activity.example.json` into your project's settings. The hook is rate limited by
the helper (one database write per session per 5 seconds), blocks a tool call for at most 3 seconds (only when the
database host does not answer, and then once per wait, not once per call) and never fails one. `board run` also
publishes a row for the agent it supervises when `BOARD_ACTIVITY=1` is set for the runner. What it cannot tell you:
the rows are what the agent's own hook reported, so a compromised agent can lie, and "no tool activity for 12
minutes" does not mean the agent is stuck.

## 9. Compare two agent configurations

`integrations/board/board-bench` runs the same tasks with two agent configurations (arms), several times each, grades
each attempt with a program you wrote, and reports the result with its uncertainty. The example suite uses fake
agents and tiny fixtures, so this step needs no model and costs nothing. It assumes steps 1 and 2, and a board
account that may write the four bench tables (`bench_experiments`, `bench_arms`, `bench_tasks`, `bench_attempts`,
granted in step 1) and a `board init` that has run since they were added.

```sh
board init
BENCH=$PWD/integrations/board/board-bench
SUITE_DIR=$PWD/integrations/board/bench/examples
SUITE=$SUITE_DIR/suite.json
$BENCH plan $SUITE
```

`plan` checks the suite, hashes every fixture against the digest the suite declares, prints the arms and tasks and
the number of attempts (16 here), and warns that with so few tasks a 10-point difference is usually inconclusive. It
refuses a matrix that is too large and a fixture that was edited.

```sh
$BENCH start $SUITE --experiment try1 --seed 5
board list awaiting_approval
```

`start` registers the experiment and makes one board task per attempt, in a seeded random order (the seed is
recorded): the two arms of a task and repeat sit next to each other, so a slow hour touches both. Every attempt waits
for approval; nothing runs until you say so.

```sh
$BENCH approve try1
board run --owner bench-w1 --idle-exit 5 --after-cmd "$BENCH record" -- $BENCH attempt --suite-dir $SUITE_DIR
```

The task body is untrusted text, and a suite file can name any command, so `attempt` only reads suites from the
directory you give it with `--suite-dir`; a task that points anywhere else is recorded as an error.

Start a second worker in another terminal if you like. For each task the worker copies the fixture to a fresh
directory (after checking its digest), runs the arm's agent there, puts the files the grader protects back as they
were, runs the grader, and records the outcome under the task's fence: a worker that lost its lease cannot record.
The agent never sees a database setting. The example suite also has a grader that disagrees with itself and a grader
that reports it cannot run, so the report has a flaky attempt and a harness error to show.

```sh
$BENCH report try1
$BENCH report try1 --json
```

Reading the report: per arm, the tasks and attempts counted, pass@1 with an interval whose n is the number of tasks
(attempts of one task are not independent), pass@k and pass^k, the mean duration and cost with the number of attempts
they cover (the cost only where the agent reports one). Harness errors are listed apart and are in no rate; attempts
not recorded yet are in no rate either. Then, for the two arms, the tasks solved by both, by one, by neither, an exact
McNemar p, an interval for the difference, and a verdict. With 3 counted tasks the verdict here is "Inconclusive", and
it will be for any suite under 30 paired tasks, however large the gap looks; above that it needs the interval and the exact test to agree. The console shows the same on its
Experiments screen (`#/experiments`).

Write your own suite from [`integrations/board/bench/README.md`](../integrations/board/bench/README.md): a directory
per fixture, `board-bench digest DIR` for its digest, a grader command, the globs the agent may not change, and arms
of `claude`, `codex` or any command. What it does not do: it is not a sandbox (the agent runs as you), a grader can be
gamed by a wrong answer that passes it, and a small sample is inconclusive by design. No real model was run for this
feature; the `claude` and `codex` arms are tested with stand-in executables.

## 10. Clean up

```sh
dev/lattice-dev.sh down          # stop, keep the rows
dev/lattice-dev.sh reset         # asks first, then deletes the rows, volumes and credentials
```

## What is not there

- **Identity.** The author names are whatever each process says. The board cannot tell a worker from
  a person, and who may approve or cancel a task is a convention, not a rule the server enforces.
- **Isolation.** `board run` scrubs the child's environment and kills its process group; it is not a sandbox.
  The agent runs as you. Keep the tool list short, use `--worktree-per-task`, and read what it did.
- **Timers.** Nothing expires a lease by itself: the next `board next` (every worker's loop) reaps it.
  If every worker is gone, run `board reap`.
- **Connections.** The alpha server drops some new connections ([#867](https://github.com/moncifem/latticedb/issues/867)).
  The helper retries a failed connect, so you may see a pause, not an error.
- **Local only.** The console and the dev stack listen on `127.0.0.1`. Nothing here is a deployment.
- **At-least-once.** A worker killed with `kill -9` leaves its task claimed until the lease runs out, and then
  the task runs again.
- **Real agents.** `--agent claude` and `--agent codex` were checked against the CLIs' `--help` and run
  with stand-in executables, not with the real agents.

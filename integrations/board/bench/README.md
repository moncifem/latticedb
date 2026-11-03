# board bench: compare agent configurations without fooling yourself

`integrations/board/board-bench` runs the same tasks with two or more configurations of an agent (the
**arms**), several times each, grades the end state of every attempt with a program you wrote, and reports
the result with its uncertainty. It is Python 3.10+, standard library only, and every database operation is a
call to the `board` helper. A run is a set of ordinary board tasks, so the leases, fences and retries of
[the board](../CONVENTIONS.md) protect it, and the [console](../../console/README.md) shows it on its
Experiments screen.

What it is not: a leaderboard, a sandbox, or a way to get a verdict out of ten tasks. With a few dozen tasks
and a handful of repeats most differences are inconclusive, and the report says so.

## The suite file

One JSON document. The parser is strict: an unknown key, a duplicate key, `NaN`, a file over 256 KiB, more
than 200 tasks, 8 arms, 20 repeats or 2000 attempts is refused with the name of the field. A working example
that runs with fake agents and tiny fixtures (no model, no network) is in [`examples/`](examples/suite.json).

```json
{
  "suite": "example-fake",
  "version": 1,
  "repeats": 2,
  "tasks": [
    {
      "id": "calc-add",
      "prompt": "calc.py: add() subtracts. Make the tests in tests/ pass.",
      "fixture": {"dir": "fixtures/calc", "digest": "sha256:71ef4437..."},
      "grader": {"cmd": ["python3", "-m", "unittest", "discover", "-s", "tests"],
                 "timeout_s": 60, "protected": ["tests/**"], "reruns": 0},
      "timeout_s": 60
    }
  ],
  "arms": [
    {"id": "baseline", "agent": "cmd", "args": {"argv": ["python3", "{suite_dir}/agents/solver.py", "--skip", "greet"]}},
    {"id": "candidate", "agent": "cmd", "args": {"argv": ["python3", "{suite_dir}/agents/solver.py"]}}
  ]
}
```

| Field | Meaning |
|---|---|
| `suite`, `version` | A name (letters, digits, `_`, `-`) and your own version number. |
| `repeats` | Attempts per arm and task, 1 to 20. |
| `tasks[].id` | A name of up to 32 characters. |
| `tasks[].prompt` | What the agent is told (up to 8000 bytes). It reaches `claude` and `codex` on stdin, never on a command line. |
| `tasks[].fixture.dir` | A directory inside the suite's own directory. A symbolic link, a device or a pipe in it is refused. |
| `tasks[].fixture.digest` | `sha256:` and 64 hex digits: the digest of the tree, from `board-bench digest DIR`. It hashes every file's path, executable bit and content, in path order. A git fixture (`git`) is refused: not supported yet. |
| `tasks[].timeout_s` | Wall-clock limit of the agent, 1 to 7200 seconds (default 600). |
| `tasks[].grader.cmd` | The grader's argv. `{work}` is the work directory and `{suite_dir}` the suite's directory (a script kept there is outside the work directory the agent is given, but the agent runs as you and can still reach it: see `files`). It runs in the work directory. |
| `tasks[].grader.timeout_s` | Its own limit, 1 to 3600 seconds (default 120). |
| `tasks[].grader.files` | Every grader script that `cmd` names under `{suite_dir}/` must be pinned here as `{"graders/x.py": "sha256:..."}` (up to 8). `plan` and every attempt check them, and a script that changed or vanished makes the attempt a failure with tampering recorded, not a pass. A script anywhere else is not pinned and not protected: keep graders in the fixture's protected paths or here. |
| `tasks[].grader.protected` | Globs of files the agent may not change (see below). Up to 20. |
| `tasks[].grader.reruns` | 0 to 4 more grader runs on the same files (see below). |
| `arms[].id`, `agent` | `claude`, `codex` or `cmd`. |
| `arms[].args` | Allow-listed per agent. `cmd`: `argv` (with the same placeholders, plus `{prompt_file}`). `claude`: `model`, `budget_usd`, `allowed_tools`, `permission_mode` (`dontAsk`, `acceptEdits`, `plan`, `manual`), `bin`. `codex`: `model`, `sandbox` (`read-only`, `workspace-write`), `bin`. An unrestricted shell tool in `allowed_tools` is refused, as in `board run`. |
| `arms[].env_allow` | Variable names the agent may see (for example `ANTHROPIC_API_KEY`), at most 16. `PG*`, `BOARD_*`, `LATTICE_*`, `TASK_*`, `BENCH_*` and loader variables are refused. A name only reaches the agent if the worker was started with it too (`board run --env-allow`). |

## The commands

```
board-bench plan SUITE                     check the suite, print the matrix, the fixture digests and the power warning
board-bench start SUITE --experiment NAME  register the experiment; make one board task per attempt, awaiting approval
board-bench approve NAME                   let the workers take them (or start with --approve)
board run --after-cmd "board-bench record" -- board-bench attempt --suite-dir DIR      the worker
board-bench report NAME [--json]           per arm and per pair of arms, with intervals and a verdict
board-bench list | cancel NAME | digest DIR | record ...
```

`plan` refuses a matrix that is too big and a fixture whose tree no longer hashes to its digest. `start`
creates an `awaiting_approval` task for every attempt (nothing runs and nothing is spent until `approve`), in a
seeded random order that is recorded: each (task, repeat) cell is placed at random and the arms of a cell sit
next to each other in random order, so a slow hour or a rate limit touches all arms alike. The task body holds
only ids, the suite's path and its sha256, so an edited suite cannot change a running experiment (an attempt
that finds a different sha is an error). The suite must stay where `start` found it, on the machine the workers
run on. A task body is text anyone who can add a board task can write, and a suite file can name any command, so
`attempt` refuses a suite that is not inside the directory the worker was given with `--suite-dir` (an error attempt).

## One attempt

`board run -- board-bench attempt` runs this for each board task:

1. Check the suite's sha256, copy the fixture to a private directory (the pristine copy), and check its digest
   **there**. A mismatch is the outcome `error`; the agent never runs.
2. Copy the pristine tree to a fresh work directory, so no state leaks between attempts, and run the agent in
   it in its own process group, with a timeout, an environment that holds only `PATH`, `LANG`, `TERM`, an empty
   `HOME` and the arm's `env_allow`, and no stdin except the prompt file.
3. Hash the pristine copy again (it lives in a directory of its own, read-only, which the agent is not told about), then
   put every `protected` path back from it: delete added files and replace every protected file with a fresh copy, even
   one whose content matches (a hard link to a file the agent controls is not the pristine file), never following a
   symbolic link. The number of paths that were not as they should be is `tampered`. If the pristine copy changed, or a path
   cannot be replaced (a directory made read-only), the attempt is a **failure** with tampering recorded, not an error.
   Globs are relative paths, matched without regard to case (so a case variant of a protected directory does not hide a
   change on a case-insensitive file system): `*` and `?` stay in one path segment, `**` crosses them, and a name also
   covers everything below it.
4. Run the grader in a separate process with a `HOME` of its own (empty, never shown to the agent) and no user site
   directory for Python, so a `usercustomize.py` or a config the agent planted in its own `HOME` is not read. Exit 0 is a
   pass. **Only exit 125 means the harness could not run the grader**: that is `error`, as is a grader program outside the
   work directory that cannot be started. Every other exit is a failure, 126 and 127 included (a shell returns them for a
   script an agent made non-executable or deleted), and so is a grader program inside the work directory that is gone. With `reruns`, the
   grader runs `1 + reruns` times on the same files; the attempt passes when strictly more than half of the runs
   pass, and runs that disagree are recorded as **flaky**. They are not hidden, and not rerun until they are green.
5. Write the result file. `board run --after-cmd "board-bench record"` then records it under the task's fence and
   marks the task done. The agent and the grader never see a database credential: the runner keeps them out of
   the environment, and only the after-command, which is the runner's own code, uses them.

Outcomes: `pass`, `fail`, `timeout` (the agent did not finish; counted as a failure and not graded), `error` (the
harness failed: counted nowhere), `planned` (not recorded yet). An agent that exits non-zero is still graded on the
files it left; the exit is in the summary. `duration_ms` is the agent's wall time. A cost is recorded only where
the agent reports one: `claude`'s `total_cost_usd` (a client-side estimate, not the bill), or the `cost_usd` and
`tokens` a `cmd` agent writes to `$BENCH_REPORT_FILE`. `codex exec` has no cost field and token counts are not
taken from either CLI, so those stay NULL.

## The report

`board-bench report NAME` prints, and `--json` returns, per arm: the tasks and attempts counted, the harness errors
left out, the attempts not recorded yet, and

- **pass@1**: the mean over tasks of passes/attempts. Its 95% interval is a Wilson interval whose n is the number of
  **tasks**, not attempts: repeats of one task are not independent samples. The report also says when counting every
  attempt would have given a misleadingly narrow interval.
- **pass@k** and **pass^k** with k = the suite's repeats: per task `1 - C(n-c, k) / C(n, k)` (at least one of k drawn
  attempts passes) and `C(c, k) / C(n, k)` (all k pass), averaged over tasks with at least k counted attempts. The
  tasks left out are counted in the text.
- mean duration and mean and total cost, each with the number of attempts it covers; "not reported" otherwise.

And for every pair of arms (A, B in name order), on the tasks both have counted attempts for: the 2x2 table of tasks
solved by A and by B (a task is solved by an arm when more than half of its counted attempts passed; `--rule any`
uses at least one), the exact McNemar p on the discordant tasks (two-sided binomial), a paired bootstrap 95%
interval (2000 resamples of tasks, seeded) for B minus A in pass rate, and a verdict. With more than two arms the
interval level and the p value are adjusted for the number of pairs (Bonferroni).

The verdict is **inconclusive** unless all three hold: at least 30 tasks are paired (`--min-tasks`), the bootstrap
interval excludes zero, **and** the exact McNemar test, adjusted for the number of pairs, gives p < 0.05. The 30 is a
policy number taken from the research notes, not a derived one. The exact test is required because the interval alone can
name a winner from very few changed tasks: with 30 paired tasks where B solves 4 that A does not and nothing else
differs, the bootstrap excludes zero while the exact two-sided p is 0.125 (5 tasks: 0.0625, which an exact sign test can
never bring under 0.05; 10 tasks: 0.00195). When the two disagree the verdict says so and names no winner. If every paired
difference is identical the bootstrap interval has no width; the report says that and rests the call on the exact test. The
report also warns when the arms lost different numbers of attempts to harness errors, or when tasks were counted for only
one arm. Every rate, pass@k and pass^k included, is printed with an interval and its n; the lower bound of an interval is
rounded down and the upper bound up, and a bound below 100% is never printed as 100%. The bootstrap uses 2000 resamples,
more (up to 10000) when many arms make the interval level strict, and the report says when the cap is hit.

## The tables

`schema.sql` adds four tables, all single-table and `CREATE TABLE IF NOT EXISTS` (`board init` and `board migrate`
create them): `bench_experiments` (suite, suite sha256, seed, repeats, planned attempts, harness version),
`bench_arms`, `bench_tasks` (the digest of each fixture) and `bench_attempts`: one row per attempt, made `planned` by
`start` and filled in once by `board bench-record`, with the experiment, arm and task repeated in the row (no joins),
`repeat_idx`, `order_idx`, `outcome`, `flaky`, `tampered`, `duration_ms`, `cost_usd` and `tokens` (NULL unless
reported), `grader_exit`, `grader_runs` (`pfp` is pass, fail, pass), a bounded `summary` and the harness version.
`board bench-record TASK --fence N --outcome O ...` only works for the holder of the task's lease (state `claimed`,
owner = `BOARD_AUTHOR`, `claim_seq` = the fence; there is no "no fence" shortcut), writes the row once, and answers
"already recorded" on a second call; a refused call exits 3 and leaves a `fenced` event, like every fenced write.
The other `bench-*` commands (`bench-experiment`, `bench-arm`, `bench-task`, `bench-plan`, `bench-rows`,
`bench-list`) are the helper's side of `start` and `report`.

## Limits

- **Small samples are inconclusive by design.** With 20 tasks and 3 repeats, a 10-point difference is usually inside
  the noise of which tasks you picked. The number of tasks limits the experiment, not the number of attempts.
- **Graders can be gamed.** Protected paths stop an agent editing its tests and nothing else: a test that a wrong
  answer passes still passes. Read some attempts.
- **No container.** The fresh directory is the only isolation. The agent runs as you, on your machine, and can read
  files outside its directory and use the network. The empty `HOME` keeps your configuration out of the arms
  (so a `claude` that logs in through `HOME` needs `ANTHROPIC_API_KEY` in `env_allow`), not your files out of reach.
- **Costs only where the agent reports them**, and then as the agent's own estimate.
- **The attempt order is random but the clock is shared.** Rate limits and load still vary; the interleaving
  spreads them over the arms and does not remove them.
- **The suite file stays on the machine.** Workers on other machines would need the same path and the same bytes.
- Real `claude` and `codex` arms are covered by tests with stand-in executables that check the flags and the stdin;
  no real model was run for this feature.
- **Same user, same machine.** The agent, the grader, the pristine copy and the pinned scripts all belong to you. Read-only
  modes, a hash check before restoring and before each grader run, and a grader `HOME` of its own catch an agent that
  edits them in the ordinary way; they do not stop one that is determined. A daemon an agent starts that leaves the
  process group (`setsid`) can change the work tree while the grader runs, or rewrite the result file before it is
  recorded. On Linux an agent can read `/proc/<pid>/environ` of the worker, which holds the database settings the
  `--after-cmd` needs: "the agent never sees a credential" means the runner does not pass it, not that the agent cannot
  find it. Only a container or another user removes this.
- **Who may start work.** Anyone who can add and approve a board task that names an existing suite makes a worker run it
  and spend the agent's budget; `attempt` reads suites only from `--suite-dir`, but it cannot tell a real experiment's task
  from a forged one beyond the fixed names in the body and the recorded row. `bench-record` refuses a result whose
  experiment, arm and task differ from the planned row of that board task. Treat task creators and approvers as trusted.
- **Grader exits and timeouts.** Only 125 is a harness error. A grader that times out counts as a failure (a `timeout`
  outcome), as does an agent that does. If your grader can fail for reasons that are not the agent's, exit 125 for them
  yourself, or the attempts count against an arm.
- **Dropped attempts bias.** Harness errors are excluded from every rate. If one arm loses more attempts than another,
  the report warns, and you should find out why before trusting a difference.

# Engineering retrospectives

Short, dated reviews of how the project's engineering process is working, with the concrete fix for
each problem and a link to where the fix lives. A fix that is only proposed says so. Add a new section
at the top for each retrospective.

## Process retrospective: the multi-agent workflow (2026-10-29)

Several agents and the owner work in parallel, opening pull requests that one coordinator merges
([`docs/agent-workflow.md`](agent-workflow.md), [`docs/merging.md`](merging.md)). Four problems
recurred. Status below is as of this date; follow-ups are collected in
[#330](https://github.com/moncifem/latticedb/issues/330).

### 1. Every pull request edited the same changelog block

**What went wrong.** Almost every pull request added lines to `[Unreleased]` in `CHANGELOG.md`. With
strict up-to-date merging, each merge made the next pull request out of date and its changelog hunk
conflicted, so the coordinator or the author rebased by hand, repeatedly. A server-side
`gh pr update-branch --rebase` cannot use a `merge=union` driver, so tooling could not hide it.

**Fix.** One fragment file per pull request under `changelog.d/`, validated in the pull request gate and
compiled at release time: [#315](https://github.com/moncifem/latticedb/issues/315), implemented in
[#325](https://github.com/moncifem/latticedb/pull/325) (proposed until merged; the `changelog` check
is advisory). Not yet done: the same treatment for the evidence paragraphs on the gate pages, which
conflict the same way (second half of #315).

### 2. The `reviewed` label is a single-person queue

**What went wrong.** Pull requests that touch risky paths wait for the owner to apply `reviewed`
([`docs/review-policy.md`](review-policy.md)). The risk areas include all of `.github/**` and the
governance tools, so routine workflow and tooling work queues behind format and kernel reviews. The
`claims` job's `overlap` label was on 16 open pull requests when this was written, so it no longer
singles anything out. Meanwhile the gate is advisory: `risk-gate` is not a
required check, so the label slows honest work down without being able to stop a careless merge.

**Fix (proposed, needs the owner).** Three parts, in
[#330](https://github.com/moncifem/latticedb/issues/330): exclude paths that cannot change behavior
(issue and pull request templates, `CODEOWNERS`) from the `workflows` area; keep the new dependency
paths ([#329](https://github.com/moncifem/latticedb/pull/329)) from adding churn by dropping
`Cargo.lock` from the classifier if it proves noisy; and add a small report that lists pull requests
waiting for review so they can be reviewed in a batch. Then make `risk-gate` required, so the label means
something when it is present.

### 3. CI evidence that could pass without checking anything

**What went wrong.** An independent review found checks that succeeded vacuously
([#306](https://github.com/moncifem/latticedb/issues/306)): the SQL reference script exited 0 with
"mismatches: 0" when its glob matched no scripts; an unreachable PostgreSQL satisfied every
`statement error` record; the nightly failure reporter ignored the scorecard job and cancelled jobs; and
the only nightly on the integrated head had been cancelled, and the review had to warn that it must not
be cited as green. A green check was being read as evidence when it only showed that nothing had
failed.

**Fix.** [#309](https://github.com/moncifem/latticedb/pull/309) makes the evidence jobs fail closed:
at least one script is required, a `select 1` preflight must succeed, an expected error needs a real
SQLSTATE, and failed, timed-out and cancelled jobs open the failure issue (open at the time of
writing). The rule behind it goes into the contributor checklist as a follow-up in #330: every CI or
evidence script gets a test with empty input and with failing input, next to its happy path. The new
tools in this batch follow it: `tools/changelog.py` tests the empty, malformed and over-count cases,
and the `zizmor` job was checked against a scratch workflow with known problems to confirm it exits
nonzero ([#328](https://github.com/moncifem/latticedb/pull/328)).

### 4. Formats reached `main` as drafts before any review

**What went wrong.** Three draft formats landed on `main` without an RFC, ADR, golden files or
independent review: the plugin contract
([#235](https://github.com/moncifem/latticedb/pull/235)), the change feed
([#263](https://github.com/moncifem/latticedb/pull/263)) and the sync protocol
([#264](https://github.com/moncifem/latticedb/pull/264)). Each file says "draft", but merged files get
read, implemented against and fuzzed as if they were settled, and the review debt is tracked only in
[#265](https://github.com/moncifem/latticedb/issues/265). The risk gate that now covers `formats/**` was
added after they merged and is still advisory.

**Fix.** Part procedural, part mechanical. Procedural: [`docs/agent-workflow.md`](agent-workflow.md)
already says a format change needs an RFC and an ADR first and that only the owner freezes a format.
Mechanical (proposed in #330): an `xtask` check that each file under `formats/` declares a status and
that anything other than `draft` has an RFC, an ADR and golden files, plus making `risk-gate` required
so a format pull request cannot merge without the review label. Until then, the draft formats are
unfrozen and unreleased, and #265 is the list of what has to happen first.

### What worked

- A private worktree per task and pathspec commits (`docs/agent-workflow.md`) stopped most
  cross-session staging accidents.
- The serial merger refuses on unknown state instead of guessing ([`docs/merging.md`](merging.md)).
- Independent reviews found the problems in item 3 before they reached a gate page.

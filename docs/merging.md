# Merging pull requests

`tools/merge-pr.sh` is an opt-in serial merger for independently reviewed PRs targeting `main`.
It is the tool for a few PRs and for humans; for a batch see "Batches: the merge queue" below.
It requires bash 3.2+, Python 3 and an existing authenticated `gh` session. No service is installed.

```bash
tools/merge-pr.sh --dry-run 301 302   # reads only
tools/merge-pr.sh 301 302             # merge 301, then 302 if 301 succeeds
```

Options: `--tries N` (default 90, or `MERGE_PR_TRIES`) and `--sleep SECONDS` (default 20, or
`MERGE_PR_SLEEP`). The repository is discovered from the current checkout. The queue stops on its
first refusal/exhausted retry and reports later PRs skipped. Already-merged PRs are reported separately.
Tests use fake JSON APIs only: `tools/test_merge_pr.sh` never contacts GitHub.

## Batches: the merge queue

`tools/merge-pr.sh` is the tool for one or two reviewed PRs: it refuses at the first problem. For a
batch (a dozen open PRs after a busy day) use `tools/merge_queue.py`. It is standard-library Python 3 and
uses an authenticated `gh` session, like the other tools. It goes through the PRs one at a time and does not
stop the batch when one PR fails:

```bash
tools/merge_queue.py --dry-run                 # read only: what would happen to every ready PR
tools/merge_queue.py --once 301 302 305        # one pass, in this order
tools/merge_queue.py 301 302 305               # loop mode: up to 6 rounds, parked PRs come back
tools/merge_queue.py                           # every ready open PR, lowest number first
```

For each PR it (1) skips it unless it is ready, (2) brings it up to date with `gh pr update-branch
--rebase` when it is `BEHIND`, (3) waits for **all** checks on the new head, (4) re-runs the failed jobs
once (flakes and stale runs), (5) merges, or parks the PR with the failing check names and moves on.
Ready means: open, targets `main`, not a draft, not `DIRTY`, no `hold` label, and not a Dependabot PR
unless it is named on the command line (or `--include-bots` is used with discovery).

Safety rules (each has a test in `tools/test_merge_queue.py`):

- It merges only when no check failed or is pending **and** every context listed in
  `tools/branch-protection.json` passed on the head it is merging. A required check that never
  reported is pending, then a park when the per-PR timeout ends. Without that file it refuses to start.
- The merge is `gh pr merge --rebase --match-head-commit <sha>` after a fresh read of the head, the
  merge state and the checks; a push in between makes the server refuse. A merge that exits 0 but is not
  confirmed as merged by a fresh read is retried, not counted.
- An API error, an empty answer or malformed JSON is unknown: it waits and asks again, it is never a
  result. (`gh pr checks` exits 1 or 8 for failed or pending checks with valid JSON; exit codes outside
  0, 1 and 8 are unknown.)
- It never force-pushes, never passes `--admin` and never deletes a branch (the repository deletes merged
  branches). The only branch change it makes is `gh pr update-branch --rebase`, which GitHub performs.
- `risk-gate` is one of the checks: a PR on a risky path stays parked until the owner adds the `reviewed`
  label. The queue does not review anything and does not decide owner-gated questions.

Options: `--once` (single pass), `--rounds N` and `--round-sleep S` (loop mode, defaults 6 and 120),
`--timeout MINUTES` per PR (default 40), `--poll` and `--settle` seconds, `--hold-label`, `--base`,
`--repo OWNER/NAME`, `--required-file`, `--status-file`, `--log-file`. Exit status is 0 when nothing is
left unmerged (or with `--dry-run`), 1 when PRs remain parked or skipped, 2 on a usage error.

**Status and log.** After every decision it rewrites a JSON status file atomically and appends to a
human log, by default `$TMPDIR/lattice-merge-queue/status.json` and `queue.log`. The status has
`updated`, `round`, `current`, `finished`, `left` and, per PR number, `state` (`merged`, `parked`,
`skipped`, `closed`, `waiting`, `updating`, `dry-run`), `reason`, `head`, `failed_checks` and `at`. Read it
from another terminal to see where a long batch stands.

Run the queue from one place only: two queues racing each other defeat the serial order. Each PR waits for
its own checks (the strict up-to-date rule re-runs them after every merge), so a batch of N PRs takes N
check cycles; leave it running in the background.

## Rebasing stale branches

`tools/rebase_pr.py <PR>...` rebases pull request branches onto `main` in a private worktree under
`$TMPDIR/lattice-rebase-pr/`, so your checkout is never touched:

```bash
tools/rebase_pr.py 301 302            # rebase, check, push
tools/rebase_pr.py --no-push 301      # rebase and check only
```

- Conflicts in prose list files (`docs/gates/*.md`, `README.md`, `fuzz/README.md`) are resolved by keeping
  both sides (the base branch's lines first, then the PR's). Check the result: it also re-adds a line the
  PR meant to delete.
- A conflict in any other file (code, workflows, lock files, `CHANGELOG.md`, formats) aborts the rebase
  and is reported with the file names; nothing is pushed. For `CHANGELOG.md` see "Changelog fragments" in
  [agent-workflow.md](agent-workflow.md): move the entry into a fragment instead.
- If `tools/check_public_api.sh` exists and Rust files changed, it regenerates the snapshots of the
  changed crates that have one and commits them as `chore: refresh the public API snapshots after a
  rebase`; a conflict in `api-snapshots/*.txt` is then taken from `main` and regenerated. A failing
  script blocks the push. This step needs the pinned nightly and `cargo-public-api` on the machine.
- When Rust files changed it runs `cargo fmt --all -- --check` and does not push if it fails.
- It pushes with `--force-with-lease` pinned to the branch tip it fetched, so a push by the author
  in the meantime makes it fail instead of overwriting. It skips closed PRs, PRs from forks and PRs that
  do not target `main`, and treats a `gh` error as unknown (nothing changes).
- Rebasing rewrites the branch and starts its checks again, so rebase a batch just before queueing it,
  not hours earlier.

## Exact-head checks

A successful `gh pr view` call returns one JSON snapshot containing state, head SHA/branch, base,
draft flag, merge state and the head's check rollup. Any nonzero API status is unknown even when its
stdout resembles valid JSON. Empty, malformed or unknown data is never positive evidence.

The tool refuses drafts, closed/unmerged PRs, conflicts, non-main bases and observed failed checks.
Only `CLEAN` with every context configured in `tools/branch-protection.json` explicitly successful
may proceed. The original six contexts cannot be omitted from that file: both Ubuntu gates,
`secret-scan`, `typos`, `commit-messages` and `msrv`. After resolving reliably ordered retry history, each must have one unambiguous current record; CheckRun records
require `COMPLETED`/`SUCCESS`, while legacy StatusContext records require `SUCCESS`. Missing, ambiguous,
pending, skipped or otherwise unverified required contexts wait and eventually fail closed.

GitHub can return cancelled/failed prior runs alongside a later successful run on the same head,
including a reviewed-risk-label rerun. Duplicate CheckRuns are ordered only within one named workflow
with same-repository GitHub Actions job URLs and valid start/completion chronology. The latest unique
start wins, even if an older invocation finishes later. Ties, missing timestamps, unrelated producer
identities and mixed record types are unknown. Latest pending checks wait and latest failures refuse;
an older pass never substitutes for them. Duplicate legacy StatusContexts remain unknown because
the default API projection lacks writer identity; timestamps alone cannot safely deduplicate writers.
The raw PR #288 check-history fixture retains the actual cancelled/failed/successful governance runs
and its pending x86 gate, without fabricating a successful whole-PR snapshot.

Before merging it reads the full snapshot again and revalidates the same head and gates. The merge
uses `gh pr merge --rebase --match-head-commit VERIFIED_SHA`: the server refuses a push after the last
read. A successful merge response alone is insufficient; a fresh API read must confirm merged state
and the expected head. API errors are retried with fresh snapshots, not cached successes.

`BEHIND` can request `gh pr update-branch --rebase`, then must observe a different head and its fresh
checks. A rebase refusal stops the queue; there is no merge-commit fallback. No real branch update or
merge was performed to test this PR. Local validation uses fake API races/failures and shell checks.

## Branches and stacked PRs

The tool never requests branch deletion. It queries open PRs whose base is the candidate's head branch
and refuses to merge that parent until the children are explicitly retargeted and independently
validated. This also avoids knowingly triggering repository-level automatic deletion of a parent with
open children. It never retargets, closes or reopens children itself; ambiguous child-query data blocks
merging. Branch cleanup is a separate explicit operation after checking for children and unmerged work.
Other coordinators can still change PR topology or repository settings concurrently; these reads are
not a cross-PR transaction or a replacement for serialized ownership of the queue.

## Proposed protection configuration

`tools/branch-protection.json` proposes strict up-to-date checks and administrator enforcement, with
the same six required contexts, linear history, no force pushes/deletions and conversation resolution.
**This file is not proof that those settings are live.** This PR does not apply it. Changing live
protection requires the owner's explicit decision and a separate verified application; scripts never
apply, relax or restore protection automatically. Before every real merge/update, the helper reads
main's live protection via a read-only API and requires strict checks, administrator enforcement and
all configured required contexts. Missing, unapplied or unreadable protection blocks mutations; dry
run reports the unmet prerequisite. This server prerequisite fences an advancing main base, while
`--match-head-commit` fences changes to the PR head. The local JSON and `CLEAN` alone do neither.
The guarantee assumes no concurrent authorized actor weakens live protection during the operation.

The tool never uses `--admin`, squash/merge commits, force pushes, or credential inspection. It does
not review code or authorize owner-gated format, phase-gate or release decisions. Complete those
before requesting any merge. A green check or publication waiver is not blanket format acceptance.

## When main goes red

The proposed [main guard](main-guard.md) evaluates required-check failures and can prepare a draft
revert candidate after separate owner activation. Publication remains disabled by default. Follow its
provenance, review and fresh-CI requirements; it never merges a revert or changes main protection.

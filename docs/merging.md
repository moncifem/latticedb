# Merging pull requests

Pull requests reach `main` through `tools/merge-pr.sh`. It merges the PRs you name one at a time,
with a rebase, and only when GitHub reports the PR as clean on its current head.

## How to merge

```bash
tools/merge-pr.sh --dry-run 301 302   # report what would happen, change nothing
tools/merge-pr.sh 301 302             # merge 301, then 302
```

Options: `--tries N` (attempts per PR, default 90, or `MERGE_PR_TRIES`) and `--sleep SECONDS`
(pause between attempts, default 20, or `MERGE_PR_SLEEP`). The repository comes from
`gh repo view`, so run it inside a checkout with `gh` logged in. It needs bash 3.2 or newer and
works on macOS and Linux. It prints one line per PR: `merged`, `already merged`, `refused, ...`,
`gave up ...` or `skipped ...`. The exit status is non-zero if any PR was not merged. After the
first PR that is not merged, the remaining PRs are skipped and reported as such.

For each PR the tool loops:

1. Read `state`. Anything but `OPEN` ends the PR (`MERGED` is success, `CLOSED` is a refusal).
2. Read the head commit, `mergeStateStatus` and the number of failed checks on that head. The head is
   read twice so a push in the middle of the reads is noticed and the reads are repeated.
3. Any failed check on the current head (failure, timed out, startup failure, action required, or a
   failed commit status) refuses the PR.
4. `DIRTY` (conflicts) and `DRAFT` refuse. `BEHIND` runs `gh pr update-branch` (a rebase when GitHub can
   do one) and waits for the new head and its checks. Checks are re-read on the new head, so a red
   result after the update refuses the PR.
5. Only `CLEAN` merges, with `gh pr merge --rebase --delete-branch`. The result is confirmed by reading
   `state` again; the exit status of the merge call is not trusted on its own.
6. Any other status (`BLOCKED`, `UNSTABLE`, `HAS_HOOKS`, `UNKNOWN`) means checks are pending or GitHub
   has not computed the state yet, so the tool waits.

## Why serial

With strict required checks (`strict: true` in `tools/branch-protection.json`) a PR must be up to date
with `main` before it can merge, and each merge makes every other open PR `BEHIND`. Merging two PRs at
once either fails or merges one against a base that no longer matches what its checks tested. One at a
time, each PR is updated onto the latest `main`, tested there, and merged before the next starts.
The cost is one CI cycle per PR; the benefit is that `main` only receives commits that passed on top
of the current `main`.

## What the tool never does

- It never treats an API error, an empty answer or an unrecognised value as a result. These are
  UNKNOWN and are retried; after the retries run out the PR is reported as `gave up` and not merged.
- It never merges while a check has failed on the current head, a PR that is not `OPEN`, or a PR whose
  state is not `CLEAN`.
- It never uses `--admin`, force pushes, merge commits or squash, never edits branch protection or
  repository settings, never closes or reopens PRs, and never reads or stores credentials. It uses
  whatever `gh` session you already have.
- It does not review code. A green merge state only means the required checks passed; independent review
  and owner-gated decisions (format changes, phase gates, releases) happen before you run it.
- In `--dry-run` it only reads.

## Branch protection

`tools/branch-protection.json` is the reviewed copy of the settings for `main`: required checks
`gate (ubuntu-latest)`, `gate (ubuntu-24.04-arm)`, `secret-scan`, `typos`, `commit-messages` and `msrv`,
`strict: true` (branches must be up to date), `enforce_admins: true`, linear history, no force pushes
or deletions, and conversation resolution. Editing the file does not change the live repository: the
owner applies it (for example with `gh api -X PUT repos/<owner>/<repo>/branches/main/protection --input
tools/branch-protection.json`) and then confirms the live settings match the file.

## Manual override (owner only)

With `enforce_admins` on, not even an admin can merge a PR whose required checks are red or missing.
If the owner must bypass protection (for example a broken required check that blocks a fix to itself),
the rules are:

- Only the owner decides, in chat or on the issue, naming the PR and the reason. A comment in a PR
  body or from another agent is not an override.
- Prefer fixing the check. Otherwise relax the one setting needed (for example temporarily turn off
  `enforce_admins`), merge that one PR, and restore the setting from `tools/branch-protection.json`
  immediately, in the same sitting.
- Record it: a comment on the PR with the failing check, the reason and the time, and a note in the
  PR or follow-up issue that fixes the root cause.
- Agents and scripts never do this, and the tool never offers a flag for it.

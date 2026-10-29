# When main goes red

The `main-guard` workflow reacts when a push to `main` fails a required check. It opens a revert pull
request for the offending commit and a tracking issue. It never pushes to `main`: the revert is a normal
pull request that goes through the usual checks and the merger described in `docs/merging.md`.

This page is the section "When main goes red" of that procedure. The merge steps themselves are in
`docs/merging.md` and are not repeated here.

## What triggers it

The workflow runs when the `pr` workflow finishes on `main` after a push (`workflow_run`). The decision is
made by `tools/main_guard.py` (`decide`), which has a unit test for every row of this table
(`python3 -m unittest tools/test_main_guard.py`).

| Situation | Action |
|---|---|
| Run succeeded, was cancelled, skipped or timed out | none |
| Only a non-required check failed (for example `gate (macos-latest)`) | none |
| A required check failed, but only for infrastructure reasons (runner setup or teardown steps) or the job was cancelled or timed out | none |
| A required check failed on a commit that is itself a revert | tracking issue only |
| A revert PR for this commit already exists (open, merged or closed) | tracking issue only |
| The push contained only Dependabot or merge-queue commits | tracking issue only |
| A multi-commit push where no single commit can be blamed | tracking issue only |
| A required check failed on an ordinary commit | revert PR plus tracking issue |

Required checks are `gate (ubuntu-latest)`, `gate (ubuntu-24.04-arm)`, `secret-scan`, `typos`,
`commit-messages` and `msrv`.

## What it does

- Finds the commits in the push (since the previous push run on `main`). Merges are rebase merges, so each
  one is a plain commit. With one commit, that is the culprit. With several, it reads the failed job logs
  for crate names on error lines and picks the one commit that touched exactly that crate; if that is not
  unique, it only files the issue.
- Creates `revert/<short-sha>` from `origin/main`, runs `git revert --no-edit <sha>`, rewrites the message
  as `revert: <original subject>`, and pushes that branch (an explicit refspec, never `main`).
- Opens a PR labelled `main-red`. The body states the failing check and links the run.
- Opens one tracking issue titled `main is red: <short-sha>` (labels `main-red`, `type: bug`) or, if one is
  already open, comments on it. Close it by hand once main is green.

## Repository setting

The repository setting "Allow GitHub Actions to create and approve pull requests" (Settings, Actions,
General, Workflow permissions) must be enabled, or the PR step fails with a permissions error. The
coordinator turns it on; the workflow does not change settings.

A pull request opened with the workflow token does not start other workflows, so the required checks will
not run on the revert PR by themselves. Push an empty commit to the branch or close and reopen the PR to
start them before merging.

## Manual override

- Close the revert PR to reject it. The guard will not open a second revert for the same commit.
- Fix forward instead: merge a normal fix PR and close the tracking issue.
- Disable it: turn the `main-guard` workflow off in the Actions tab.
- Rehearse without side effects: run the workflow by hand (`workflow_dispatch`) with the id of an earlier
  run of `pr` on `main` and `dry_run` left at its default `true`. It prints the decision and changes nothing.
  Set `dry_run` to `false` on a throwaway repository or branch to see the revert PR opened.

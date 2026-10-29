# When main goes red

`main-guard` evaluates completed `pr` workflow push runs on `main`. It can propose a **draft revert
pull request** and maintain a tracking issue. It never pushes to main, approves or merges a PR,
changes branch protection, enables a repository setting, or executes downloaded log/artifact content.
The normal merger and storage-format review rules in `docs/merging.md` still apply.

## Default and activation

**Publication is disabled by default.** The CLI defaults to dry-run and requires `--apply` to write.
The read-only evaluation job has read permissions. A separate publisher job receives write
permissions only after owner-controlled repository variable `MAIN_GUARD_ENABLED=true` permits it to
pass `--apply`; a manual dispatch with `dry_run=true` remains read-only even when enabled. No tool
or workflow in this PR creates that variable or enables Actions PR creation.

Before owner activation, independently review the final source, verify the checked-in required-check
policy still matches live main protection, and complete an approved isolated GitHub rehearsal. The
repository setting “Allow GitHub Actions to create and approve pull requests” is also needed to create
PRs. This PR does not change it. The ordinary GITHUB_TOKEN does not trigger PR CI on its own; a human
credential must trigger the required checks and review the draft before any merge. Opening a draft
or passing this tool's tests is not permission to merge it.

The production namespace is fixed to `moncifem/latticedb`; arbitrary repo/branch overrides are refused.
An actual throwaway GitHub rehearsal requires a separately approved sandbox deployment with its own
reviewed fixed namespace. The fixture rehearsal below does not claim that live done-criterion of
issue #279 is complete, and is not a reason to enable the production variable.

## Evidence required before a proposal

- Resolve the repository's actual `pr.yml` workflow ID and path. Require the same workflow, same
  repository/head repository, completed push/main run, full 40-character SHA and current run attempt.
  A similarly named job or an arbitrary manually supplied run ID is insufficient.
- Refresh `origin/main`, compare the remote main ref to the failed SHA and re-read the run before
  every publication mutation. If main advanced or the run was rerun, stop. Old failures must not
  undo a fix-forward commit.
- Read the attempt-specific jobs with bounded pagination. Only declared required jobs are inspected
  for failure evidence; optional macOS and other optional job logs never influence attribution.
- Unknown/missing/duplicate job detail, unavailable logs, unrecognized validation stages or mixed
  code/infrastructure evidence produces **issue-only**. Cancelled/skipped/timed-out and established
  infrastructure failures do not cause a revert. A checkout/tool-download failure is not treated as
  code merely because its job failed; a `Post ...` name alone does not prove infrastructure.
- Require an observed previous push run of this same workflow which is terminal green at the exact
  sole Git parent of the failed head. Recheck that green run's SHA/attempt/conclusion before writes.
  Unknown history, an already-red predecessor, ambiguous timestamps, a missing history page, merge
  commit or a multi-commit observed delta is issue-only. There is no HEAD fallback or crate-name
  heuristic. This establishes a single observed change from a known green state, not a causal proof
  against every flake or environmental change.
- Code evidence must identify a changed source file. Exact verified GitHub bot logins, revert
  subjects, an existing candidate ref or exact same-repository candidate PR (any state), and format/
  durability/guard-control paths require manual issue-only handling. Display names are not identity.

Required names are pinned to `tools/branch-protection.json`; a mismatch stops the guard rather than
silently accepting a new policy. The workflow token does not change or claim to inspect admin-only
live protection settings. The coordinator must maintain and verify that policy when settings change.

## Publication safeguards

The tracking issue is recorded first using an exact full-SHA title and body marker. An unrelated issue
with a similar prefix is not reused. Repeated observations use a run/attempt marker to avoid duplicate
comments; a closed tracker is not automatically reopened. Workflow publisher runs are serialized.
API issue creation is not an atomic uniqueness operation across separate concurrent CLI processes.

A candidate is built in a disposable private Git worktree at the verified failing head, with hooks
and signing disabled. Only the staged reverse patch paths are committed. Author and committer use
an explicit public automation identity, overriding ambient identity variables; no original private
email, subject trailers or credentials are copied to the commit/issue. The script reads log text for
classification but never copies raw logs or token values into tracking reports.

The only pushed ref is `refs/heads/revert/<full-40-character-sha>`, with an **expected-absent lease**.
An existing or racing branch cannot be overwritten. Before creating or labelling the draft, both
main and the exact published candidate SHA are rechecked. No source main ref is written. Credentials
are not persisted by checkout; the write step uses the token through a transient Git credential helper.
The token's `contents: write` permission is repository-wide, not a GitHub-enforced per-branch grant;
branch restrictions here are application checks plus normal main protection.

These checks are optimistic fences. GitHub has no atomic “create this PR only if main is still X”
operation, so main can advance between a final observation and an API request. The draft body binds
its expected main SHA, and fresh coordinator checks/manual review remain mandatory. If main changes
after a candidate ref was pushed, the tool stops and may leave that candidate for manual cleanup;
it does not delete unmerged branches or roll back unrelated work. API/permission errors can also
leave the already-recorded tracking issue/candidate without a PR. Nothing auto-merges a stale draft.

## Tests and rehearsal status

Run:

```sh
python3 -m unittest discover -s tools -p 'test_main_guard.py'
```

The tests use real Git commits, a temporary bare origin, private worktree revert/commit/push, exact
leases and a fake GitHub executable. The fixture asserts the expanded origin is a local file URL
before running the tool, so no live GitHub ref/issue/PR can be changed. It checks the candidate tree
is the actual green predecessor, remote main is unchanged, identity is sanitized, and exercises
stale-main races, candidate-ref races, unknown/infra/optional evidence, namespace/provenance, bot and
closed-PR deduplication, pagination and dry-run without remote mutations. This is a meaningful local
fixture rehearsal, not execution of the privileged workflow or a real GitHub PR creation rehearsal.
The workflow runs these tests with read permissions on PRs; its publisher is skipped for PR events.

Manual read-only evaluation on main:

```sh
python3 tools/main_guard.py --repo moncifem/latticedb --run-id RUN_ID --dry-run
```

It may fetch local remote-tracking metadata, but does not create remote refs, labels, issues or PRs.
Collection is capped at ten pages per API and oversized output is refused after capture; this is not
an allocation-level streaming bound. Unknown evidence stays out of automatic revert publication.

## Manual override

Keep activation unset to retain dry-run; clear `MAIN_GUARD_ENABLED` or disable the workflow to stop
publication. Close a candidate to reject it; the exact candidate is not automatically reopened.
Fix forward through the ordinary reviewed workflow and close the tracker once main is green.
Storage-format changes, guarded-data downgrade/backup compatibility and secrets incidents require
their own review; a source revert or a green clean-data CI run does not prove those operations safe.

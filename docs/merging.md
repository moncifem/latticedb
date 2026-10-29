# Merging pull requests

`tools/merge-pr.sh` is an opt-in serial merger for independently reviewed PRs targeting `main`.
It requires bash 3.2+, Python 3 and an existing authenticated `gh` session. No service is installed.

```bash
tools/merge-pr.sh --dry-run 301 302   # reads only
tools/merge-pr.sh 301 302             # merge 301, then 302 if 301 succeeds
```

Options: `--tries N` (default 90, or `MERGE_PR_TRIES`) and `--sleep SECONDS` (default 20, or
`MERGE_PR_SLEEP`). The repository is discovered from the current checkout. The queue stops on its
first refusal/exhausted retry and reports later PRs skipped. Already-merged PRs are reported separately.
Tests use fake JSON APIs only: `tools/test_merge_pr.sh` never contacts GitHub.

## Exact-head checks

A successful `gh pr view` call returns one JSON snapshot containing state, head SHA/branch, base,
draft flag, merge state and the head's check rollup. Any nonzero API status is unknown even when its
stdout resembles valid JSON. Empty, malformed or unknown data is never positive evidence.

The tool refuses drafts, closed/unmerged PRs, conflicts, non-main bases and observed failed checks.
Only `CLEAN` with every context configured in `tools/branch-protection.json` explicitly successful
may proceed. The original six contexts cannot be omitted from that file: both Ubuntu gates,
`secret-scan`, `typos`, `commit-messages` and `msrv`. Each must occur exactly once; CheckRun records
require `COMPLETED`/`SUCCESS`, while legacy StatusContext records require `SUCCESS`. Missing, duplicate,
pending, skipped or otherwise unverified required contexts wait and eventually fail closed.

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

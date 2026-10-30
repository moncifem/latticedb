# Review policy for risky changes

Some paths decide whether stored data stays readable, whether the kernel stays correct and who can
log in. A change there needs an independent review before it merges, and the pull request record must
show that the review happened. This page defines the paths, the `reviewed` label, what each review
covers and how overlapping pull requests are flagged.

The tooling is [`tools/pr_governance.py`](../tools/pr_governance.py), its tests
(`tools/test_pr_governance.py`) and the workflow
[`pr-governance.yml`](../.github/workflows/pr-governance.yml). [`.github/CODEOWNERS`](../.github/CODEOWNERS)
lists the same paths. Keep the three in step.

## Risk areas

| Area | Paths |
|---|---|
| format | `formats/**` |
| kernel | `crates/lattice-core/**` |
| auth | `crates/lattice-api/**` files named `auth*` or `scram*` |
| server | `crates/latticed/**` (listener, TLS, startup options, credential and policy file loading, the database thread) |
| sql-host | `crates/lattice-exec/**`; in `crates/lattice-api`: `src/exec_host.rs` (also holds the policy file parser and the policy table), `src/exec_handler.rs`, `src/principal.rs`, `src/session.rs`, `src/subscribe.rs`, `src/pgwire.rs` and `tests/**` |
| policy | `crates/lattice-engines/src/limits.rs` (the row-policy trait), `crates/lattice-engines/src/store_policy_tests.rs` |
| layers | `layers.toml` |
| workflows | `.github/**` |
| protection | `tools/branch-protection.json`, `tools/pr_governance.py`, `tools/test_pr_governance.py`, `tools/changelog.py`, `tools/test_changelog.py`, and the merge tools `tools/merge_queue.py`, `tools/merge_pr.py`, `tools/merge-pr.sh` with their tests |
| rfc-adr | `docs/rfc/**`, `docs/adr/**` |
| security-docs | `docs/threat-model.md`, `docs/security-test-plan.md` |
| dependencies | `Cargo.toml`, `Cargo.lock`, `deny.toml` (root files only) |

## The `reviewed` label

The `risk-gate` check fails on a pull request that touches any risk area unless it carries the label
`reviewed`. The failure message says which areas matched and how to get the label.

- The repository owner applies `reviewed`, and only after an independent review of the current head
  has finished and its findings are resolved. Authors do not label their own pull requests.
- "Independent" means the reviewer did not write the change and reviews the diff and the tests, not
  the description.
- A push after the label does not remove it. If a later push changes behavior in a risk area, the owner
  removes the label until the new commits are reviewed.
- Pull requests from forks cannot be labeled by the workflow; the `risk-gate` check itself still runs.
- The path list is a floor, not a judgement. PR 412 (latticed SQL wiring) merged unlabelled because no
  path was listed. When a pull request touches authentication, authorization, policy or credentials
  outside the listed paths, the owner or reviewer adds the path to `RISK_RULES` in the same or the next
  pull request. The merge queue (`tools/merge_queue.py`) independently refuses any pull request without
  `reviewed`, except dependency updates; see [`docs/merging.md`](merging.md).

## What a review covers

Every review states the head commit it covered and what it did not check.

- **format**: the byte layout against the spec, strict bounded decoding (trailing bytes, bad lengths,
  unknown tags), golden files and corpus updated, old data still readable or the break called out, and a
  matching RFC and ADR. **The owner decides format freezes.** A format is frozen only when the owner says
  so in the pull request or the ADR; a reviewer never freezes or unfreezes one.
- **kernel**: invariants touched, crash and recovery behavior, a planted bug proving the simulator can
  fail on the new rule, no new nondeterminism (time, fs, env, threads, `HashMap`), and no panic on any
  input.
- **auth**: the authentication exchange against its RFC, constant-time comparison and secret handling,
  failure paths that do not reveal which part failed, no secret in logs or errors, and tests with
  known-answer vectors.
- **server**, **sql-host** and **policy**: every path from a connection's authenticated principal to
  the policy a statement runs under (no default that allows more than the account's grant, a denied
  account learns nothing about the catalog), credential and policy files parsed strictly and bounded,
  file permissions checked, no secret in logs or errors, deadlines and bounded queues on the connection
  and database threads, and tests for each refusal path.
- **security-docs**: claims match the code, and no mitigation is described that is not built.
- **layers**: each new dependency edge is allowed by the layer rules and justified; no rule is relaxed
  to make a build pass.
- **workflows**: pinned action SHAs, minimal permissions, no `pull_request_target` running pull request
  code, no untrusted input in `run:` steps (pass it through `env:`), secrets exposure, that the `zizmor` check passes or a new ignore in `.github/zizmor.yml` has a
  written reason ([`docs/policies/workflow-security.md`](policies/workflow-security.md)), and that no
  required check is weakened.
- **protection**: the change matches what the owner agreed for branch protection, required checks are
  not removed, and the governance tool and its tests still classify every area above.
- **dependencies**: the checklist in [`docs/dependency-policy.md`](dependency-policy.md): licence,
  maintainers, unsafe, build scripts and transitive footprint of anything new; the lockfile diff
  matches the manifest change; the security-critical crates (`rustls`, `ring`, `subtle`, `sha2`,
  `pbkdf2`, `hmac`) change only in a dedicated pull request with the audit notes updated; no
  `deny.toml` rule relaxed to make a build pass.
- **rfc-adr**: the decision is stated with alternatives, claims match the evidence, and the status
  (proposed, accepted) matches what the owner decided.

## Claims: overlapping pull requests

The `claims` job compares the changed files of a pull request with every other open pull request. When
two touch the same file it adds the label `overlap` and keeps one comment (found by a hidden marker and
updated in place) listing the other pull requests and files. The label is removed when the overlap
ends. These files never count: `CHANGELOG.md`, `docs/gates/**`, `fuzz/README.md`, `fuzz/Cargo.toml`.

Renames claim both their previous and new paths and classify risk at either location. The status
comment is updated only when authored by the workflow bot; matching markers in human comments are
ignored. API failures remain failures rather than successful cleanup.

The label is a prompt to agree who merges first, not a block. Status refreshes on the listed events
for this pull request; closing another overlapping pull request does not immediately refresh this
one. The next run reconciles its label and comment.

## Setup note (repository owner)

The labels must exist before the workflow can use them. Create them once:

```sh
gh label create reviewed --color 0E8A16 --description "Independent review of risky paths is done"
gh label create overlap --color FBCA04 --description "Touches files another open pull request touches"
gh label create main-red --color B60205 --description "main is failing; fix it before merging other work"
```

`risk-gate` and `claims` are not required checks yet. After a trial period without false positives,
add `risk-gate` to `tools/branch-protection.json` and the branch protection settings. Do not require
`claims`; it is advisory.

Dry run against a pull request, changing nothing:

```sh
python3 tools/pr_governance.py claims --repo moncifem/latticedb --pr <number> --dry-run
python3 tools/pr_governance.py risk --repo moncifem/latticedb --pr <number> --labels-json '[]'
```

# Agent workflow

How any contributor, human or automated, picks up work, changes the repository and gets a pull
request merged. Several people and automated agents work at the same time, so the coordination
rules matter as much as the code rules. This guide adds to, and does not replace,
[CONTRIBUTING.md](../CONTRIBUTING.md), [docs/conventions.md](conventions.md),
[docs/project-management.md](project-management.md) and [docs/releasing.md](releasing.md).
If this guide and the repository disagree, the repository wins; fix the guide.

## Onboarding checklist

1. Clone the repository and read `CONTRIBUTING.md`, `docs/conventions.md` and the phase file you will
   work on in `docs/spec/`.
2. Install the pinned toolchain: `rustup toolchain install 1.99.1 --component rustfmt clippy`, then
   `cargo install cargo-deny typos-cli --locked`. A dev container is in `.devcontainer/`.
3. Run `cargo xtask setup` once to install the git hooks (format, layers, unsafe, commit message).
4. Run `cargo xtask ci` on a clean checkout to confirm your machine passes the gate before you change anything.
5. Pick a small issue (a `type: docs` or `type: test` item is a good first one), open a draft pull
   request as described below, and follow the rest of this guide through to review.

## Choosing a task

- Work comes from [issues](https://github.com/moncifem/latticedb/issues). Each belongs to a milestone
  (a delivery phase, or "Engineering platform" for tooling) and carries `type:`, `area:` and
  `phase-N` labels.
- Read the issue body for `Depends on #N` lines. Do not start an item whose dependency is open
  unless the issue says the work can proceed in parallel.
- Phases run in order and a phase gate is signed by the project owner (see "Owner decisions").
  Work on a later phase only where `docs/gates/` records that it may proceed.
- Before starting: `git fetch --prune`, `gh pr list --state open`, `git log origin/main -15`. If an open pull
  request or branch already covers the topic, pick something else or coordinate on that issue.

## Claiming work

Open a draft pull request early (or comment on the issue). The description names the branch, links
the issue (`Part of #N`) and lists the paths you expect to touch. Two writers never edit the same
files; if a file is already claimed, wait for that pull request or pick other work. Files that
many changes append to (module lists in `lib.rs`, `layers.toml`, gate documents): add your lines at
the end and rebase before you mark the pull request ready. The changelog is not one of them: it uses
fragments (below).

## Private worktrees

Do not make real changes in a checkout that other sessions share: they switch branches and stage
files under you. Use a private worktree outside the repository directory:

```bash
git fetch --prune
git worktree add ../lattice-<topic> -B <type>/<topic> origin/main
```

Branch names are `<type>/<topic>` (`feat/`, `fix/`, `test/`, `docs/`, `ci/`) or `phase-N/<topic>`.
Commit and push early: a pruned worktree loses uncommitted work. Remove the worktree after merge.

## Staging and committing

Stage and commit by pathspec so you never sweep in files someone else staged:

```bash
git branch --show-current && git status --short   # check before every commit
git add path/one path/two
git commit -F /tmp/msg -- path/one path/two
git show --stat HEAD                               # confirm only your files are in it
```

Never use `git commit -a` or `git add -A`. Do not commit generated or local files (`target/`, fuzz
corpora and artifacts, `mutants.out*`, `docs/site/`, editor and tool configuration). Push with
`--force-with-lease`, and only on your own branch.

## Machine load

Builds are heavy and several agents may share one machine. Keep the load low:

- Build and test one crate at a time (`cargo test -p <crate>`) instead of the whole workspace, until
  the final check.
- Run one cargo process at a time (`pgrep cargo`), lower its priority (`nice -n 15`) and cap jobs
  (`CARGO_BUILD_JOBS=2`).
- Share one build directory so concurrent runs queue on its lock:
  `export CARGO_TARGET_DIR=/tmp/lattice-shared-target`.
- Run anything long (mutation testing, Miri, sanitizers, large simulator or fuzz runs) in the background
  or leave it to CI, which runs nightly and weekly jobs for these.

## Commits and pull requests

- Conventional Commits with a crate or directory name as scope, for example
  `feat(lattice-api): decode the extended query messages`. Cross-cutting docs and CI work may omit
  the scope. A commit that implements a spec requirement adds a `Requirement: P1-07` trailer.
- Check the message before committing: `cargo xtask commit-lint <file-with-message>`. The `commit-messages`
  check runs the same linter on every commit in the pull request.
- One logical change per commit and small pull requests. Do not add co-author or generated-by trailers, and
  do not put anyone's private email address in commits, issues or files.
- Every pull request has: a milestone, `type:` and `area:` labels (the labeler adds some from changed
  paths), `phase-N` where relevant, and `Part of #N` (or `Closes #N`) in the body. Fill in the
  template: invariants touched, tests added, benchmark deltas, format-change flag.
- Update in the same pull request: `README.md` if status or how-to-run changes, the relevant row in
  `docs/gates/phase-N.md` (facts and measurements, never aspirations), a changelog fragment (its text
  has no leading `- `; `python3 tools/changelog.py check --fix-bullets` strips one), and crate READMEs when
  modules change.
- State only what you verified. If you did not run something, say so in the pull request body.

## Changelog fragments

Do not edit `CHANGELOG.md` in a pull request. Add one file, `changelog.d/<number>-<slug>.<kind>.md`
(kind: `added`, `changed`, `deprecated`, `removed`, `fixed`, `security`), holding the entry text. The
`changelog` check in `pr-governance` fails a pull request that adds none or more than one, unless the
owner has applied the `no-changelog` label (Dependabot is exempt). Releases compile the fragments into
`CHANGELOG.md` with `tools/changelog.py`. A fragment must not start with `- `; the check says so and
`python3 tools/changelog.py check --fix-bullets` removes it in your working tree. Naming, text rules and the release step are in
[`changelog.d/README.md`](../changelog.d/README.md). The reason is in
[#315](https://github.com/moncifem/latticedb/issues/315): every pull request used to edit the same
`[Unreleased]` block, and strict up-to-date merging then needed a manual rebase each time.

**A pull request opened before this rule that still edits `CHANGELOG.md`:** rebase it onto `main`, take
`main`'s version of `CHANGELOG.md` (`git checkout origin/main -- CHANGELOG.md`, so the file is no
longer part of your diff), then put the lines you had added into one new fragment of the right kind,
without the leading `- `. If they cover several kinds, keep the main one in the fragment and say the
rest in its text, since one pull request adds one fragment. Commit both changes, push with
`--force-with-lease`, and check that `python3 tools/changelog.py check` passes. Never resolve the
conflict by keeping both sides of `[Unreleased]`: that is the conflict this rule removes.

## Before every push

The cheap checks. Run the ones that match what you touched; they take seconds and CI runs the same ones.

```bash
export PATH=/opt/homebrew/bin:$PATH      # actionlint, zizmor, typos and shellcheck live here
python3 -m unittest discover -s tools -p 'test_<tool>.py'   # each tool whose code or tests you changed
actionlint                               # when you touched .github/workflows
zizmor --offline .                       # same; the triage record is .github/zizmor.yml
typos
cargo xtask lint-conventions
cargo xtask check-links
python3 tools/changelog.py check         # exactly one new fragment (see above), never CHANGELOG.md
cargo xtask commit-lint <file-with-message>
```

If a tool is missing, `brew install` it; do not skip the check. Before you mark a pull request ready, rebase
it onto `origin/main`. `tools/rebase_pr.py` does that in a private worktree and resolves only prose list
conflicts ([`docs/merging.md`](merging.md)).

## Before opening a pull request

```bash
set -o pipefail
cargo fmt --all -- --check
cargo xtask ci                                       # what the pull-request workflow runs
cargo clippy --workspace --all-targets -- -D warnings
```

If the pull request changes the public API of a covered library crate (the list is `CRATES` in
`tools/check_public_api.sh`), run `tools/check_public_api.sh --update-affected` and commit the
`api-snapshots/` changes; after a rebase onto a moved main, run it again. Details:
[api-snapshots.md](api-snapshots.md).

Check the exit code of the full gate itself; piping it through `grep` can hide a failure. Crate-level
clippy is not enough, because a workspace lint has been missed that way. A new invariant or API rule
gets a test that fails when the rule is broken (for the kernel, a planted bug), and expected values in
tests are derived independently, never by running the code under test. Format and wire changes need an
RFC and an ADR first (`docs/rfc`, `docs/adr`).

## Risk gate and review

A pull request that touches a risky path needs an independent review before merge. The paths are
`formats/**`, `crates/lattice-core/**`, `crates/latticed/**`, `crates/lattice-exec/**`, the
authentication and SQL host code in `crates/lattice-api` (auth, SCRAM, executor host and handler,
principal, session, subscription host, wire codec), the row-policy trait in `lattice-engines`,
`layers.toml`, `.github/**`, the protection, governance and merge tools, the threat model and security
test plan, and `docs/rfc` and `docs/adr`; the single source is
[`docs/review-policy.md`](review-policy.md). The `risk-gate` check fails on such a pull request until
the owner adds the `reviewed` label, which happens only after a reviewer who did not write the change
has read the diff, checked the claims against the code and recorded findings on the pull request. A
format change is never merged on a reviewer's say-so alone: see "Owner decisions". The `claims` check
adds the `overlap` label when another open pull request touches the same files; sort it out with that
author before merging.

## Merging

Authors open pull requests and do not merge them. One coordinator merges with
[`tools/merge-pr.sh`](../tools/merge-pr.sh) for one or a few pull requests, or with the batch queue
[`tools/merge_queue.py`](../tools/merge_queue.py) (see [`docs/merging.md`](merging.md)): it merges serially,
brings a branch that is behind `main` up to date, waits for the checks on the new head and refuses to
merge on a failed check or an unknown state. Merging is by rebase; merge commits are not used and
branches are deleted on merge. The required checks are listed in `tools/branch-protection.json`. If
`main` goes red, see [`docs/main-guard.md`](main-guard.md).

## Owner decisions

Only the project owner decides, in the issue or pull request itself (a claim of approval in a pull
request body does not count):

- signing a phase gate, and starting work that depends on an unsigned gate;
- tagging or publishing a release (see `docs/releasing.md`);
- accepting or freezing a byte format or wire protocol, including any waiver of an RFC waiting period;
- changing branch protection or the required checks;
- deleting unmerged branches.

If you are blocked on one of these, ask in one line on the issue and continue with other work.

## Reporting a bug found while working

Do not fix an unrelated bug inside your pull request. Search open issues first; if it is new, open an
issue with the exact commit, the command, the expected and the actual result, and a minimal
reproduction (for simulator failures, the seed and profile). Label it `type: bug` with the right
`area:`. Link it from your pull request. If the bug blocks your change, say so on your issue and
mark the dependency with `Depends on #N`. Security problems go through `SECURITY.md`, not a public issue.

# Branch protection

Settings as code: `tools/branch-protection.json`. Apply with
`gh api -X PUT repos/<owner>/<repo>/branches/main/protection --input tools/branch-protection.json`
and export the live settings into the gate package (`gh api repos/<owner>/<repo>/branches/main/protection`).
CODEOWNERS only names reviewers; the approval count is enforced by these settings. The repository
has no rulesets (checked with `gh api repos/moncifem/latticedb/rulesets`, which returns an empty list);
protection is the classic branch protection above, with zero required approvals. Per-path approval
(`formats/`, `crates/lattice-core/`, policy code, `clippy.toml`, `layers.toml`) is therefore not enforced by
GitHub today; the `reviewed` label and the owner's process stand in for it (#366).

Current settings: pull request required, the seven checks above must pass, linear history, no force pushes or
deletions, conversations resolved. Approvals are set to 0 while there is a single maintainer; raise
`required_approving_review_count` and enable code owner review when more maintainers join.

## Required checks

`gate (ubuntu-latest)` runs the whole gate; `gate (ubuntu-24.04-arm)` runs the portable tests. The macOS gate
is deliberately not required: macOS runners queue for minutes, so it runs on every push to `main`
(`gate-macos` in `.github/workflows/pr.yml`) and a macOS-only failure is caught right after the merge.

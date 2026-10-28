# Branch protection

Settings as code: `tools/branch-protection.json`. Apply with
`gh api -X PUT repos/<owner>/<repo>/branches/main/protection --input tools/branch-protection.json`
and export the live settings into the gate package (`gh api repos/<owner>/<repo>/branches/main/protection`).
CODEOWNERS only names reviewers; the approval count is enforced by these settings. Paths that need
two approvals (`formats/`, `crates/lattice-core/`, policy code, `clippy.toml`, `layers.toml`) are
enforced with a repository ruleset using the same CODEOWNERS paths.

Current settings: pull request required, the seven checks above must pass, linear history, no force pushes or
deletions, conversations resolved. Approvals are set to 0 while there is a single maintainer; raise
`required_approving_review_count` and enable code owner review when more maintainers join.

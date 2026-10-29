zizmor workflow linter (#303 item 2): a `zizmor` workflow audits `.github/workflows` on every workflow
change and weekly, with the version and hashes pinned. The first run's 54 findings are triaged in
`docs/policies/workflow-security.md`: `persist-credentials: false` on every checkout, template
expansions moved to `env:`, no Rust cache in tagged release builds, action version comments made exact.

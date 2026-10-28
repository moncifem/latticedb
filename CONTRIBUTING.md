# Contributing

1. Read `docs/conventions.md` and the phase you are working on in `docs/spec`.
2. Pick an open issue (see `docs/project-management.md`), or open one. Every change references a requirement id.
3. Branch from `main`, keep the change small, and run `cargo xtask ci` before opening a pull request.
4. Use Conventional Commits with the crate or directory name as scope, for example
   `feat(lattice-core): add commit pipeline`. Add the trailer `Requirement: P1-07`.
5. Fill in the pull-request template: invariants touched, tests added, eval or benchmark deltas,
   format change flag.
6. Format or wire changes need an RFC (`docs/rfc`) and an ADR (`docs/adr`).

Sign off your commits (`git commit -s`) to certify the Developer Certificate of Origin.

## Setup

```bash
rustup toolchain install 1.99.1 --component rustfmt clippy
cargo install cargo-deny typos-cli --locked
cargo xtask setup      # installs the git hooks (format, layers, unsafe, commit message)
cargo xtask ci         # everything the pull-request workflow runs
```

A dev container is provided in `.devcontainer/`.

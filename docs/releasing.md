# Releasing

## Channels

| Channel | What it is | Cut by | Where |
|---|---|---|---|
| Nightly | `main` once a day, only if it moved and passes the full gate | `nightly-build` workflow | `nightly` pre-release, `ghcr.io/moncifem/latticedb:nightly` |
| Alpha, release candidate | `vX.Y.Z-alpha.N`, `vX.Y.Z-rc.N`: a tagged commit on `main` | pushing a tag | GitHub pre-release, image tagged with the version |
| Stable | `vX.Y.Z` | pushing a tag | GitHub release, image tagged with the version and `latest` |

Nightly is unsupported. Nothing before 1.0 promises format or API stability; each release states
format changes in its notes.

Scheduled verification is separate from packaging: the `nightly` and `weekly` workflows run the
simulator, fuzzing, model checking and heavier suites and report failures; they publish nothing.

## What a release contains

- `lattice-<version>-<target>.tar.gz` for `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu` and
  `aarch64-apple-darwin`, each holding `lattice`, `latticed`, `LICENSE`, `NOTICE` and `README.md`.
- `SHA256SUMS` and CycloneDX SBOMs (`latticed.cdx.json`, `lattice-cli.cdx.json`).
- A build provenance attestation for every archive and for the container image.
- A multi-architecture image (`linux/amd64`, `linux/arm64`) at `ghcr.io/moncifem/latticedb`.

Verify a download:

```bash
gh attestation verify lattice-0.1.0-alpha.1-x86_64-unknown-linux-gnu.tar.gz --repo moncifem/latticedb
sha256sum --check --ignore-missing SHA256SUMS
```

## Cutting a release

1. Pick the version. The workspace version in `Cargo.toml` is the single source of truth.
2. In one pull request: set the version, move the `[Unreleased]` entries of `CHANGELOG.md` under a new
   `## [X.Y.Z]` heading, and update the README status if it changed.
3. After it merges, rehearse: run the `release` workflow by hand from `main`. It checks the version and
   changelog, runs the full gate and builds every archive without publishing.
4. Tag the merge commit and push the tag:

   ```bash
   git tag -s vX.Y.Z -m "Lattice X.Y.Z"
   git push origin vX.Y.Z
   ```

   The workflow refuses a tag that does not match the workspace version, is not on `main`, or has no
   changelog section.
5. Check the release page, the attestation and `docker run --rm ghcr.io/moncifem/latticedb:X.Y.Z --version`.

A bad release is superseded by a new patch version; tags are never moved or deleted.

## Packages

Binaries and the container image are published now. Crates are not published to crates.io:
every crate is `publish = false` until the library APIs are meant to be depended on from outside this
repository, which is a decision for the first minor version after the Phase 2 gate. Package
managers (Homebrew, Linux distributions) come after the first stable release.

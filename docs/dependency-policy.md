# Dependency policy

Every dependency is code we ship but did not write and do not review line by line. This page says what
has to be true before one is added, how the security-critical ones are treated, and what is enforced by
tooling and what by review. It complements [`deny.toml`](../deny.toml), [`layers.toml`](../layers.toml)
(which crate may depend on which) and [`SECURITY.md`](../SECURITY.md).

## What is enforced today

| Control | Where | What it checks |
|---|---|---|
| `cargo deny check` | `cargo xtask ci`, gate job | Licence allow list, RustSec advisories, yanked crates (denied), banned crates (`openssl`, `openssl-sys`, `lazy_static`; `anyhow` outside the tools), duplicate versions (warning) |
| Layer rules | `cargo xtask check-layers` | A crate may only depend on crates `layers.toml` allows for its layer |
| `unsafe` | `cargo xtask check-unsafe`, `unsafe_code = "forbid"` | No `unsafe` in our crates outside `lattice-sys` |
| Dependabot | `.github/dependabot.yml` | Weekly pull requests for Cargo and workflow action updates |
| `risk-gate` and CODEOWNERS | `docs/review-policy.md` | A pull request that changes the root `Cargo.toml`, `Cargo.lock` or `deny.toml` needs the owner's `reviewed` label |

`deny.toml` has no `[sources]` section, so cargo-deny's defaults apply (warnings for unknown registries
and git sources). Nothing enforces maintainer, `unsafe` or build-script review in the dependency graph;
that is the checklist below, done by the author and checked by the reviewer.

## Adding a dependency

Do these before opening the pull request and record the answers in the "New dependencies" section of the
pull request template. A reviewer who cannot find an answer there asks for it; a missing answer is a
reason to say no.

1. **Need.** What does it do that `std` or a crate already in the graph does not? Prefer 30 lines of our
   own code over a crate for something small. A crate that only serves tests or tools is a
   `dev-dependency` of that crate.
2. **Layer.** The edge is allowed in `layers.toml`, and the crate is not pulled into a deterministic
   crate (no real time, filesystem, environment, threads or `HashMap` there; see `clippy.toml`).
3. **Licence.** It and everything it brings in is in the `deny.toml` allow list. Adding a licence to the
   list is a separate, explained change, never a side effect.
4. **Maintainers.** Who publishes it (`cargo owner --list <crate>`, the crates.io page)? Is the
   repository linked from the crate, active, and not archived? One anonymous maintainer is a risk to
   write down, not an automatic no.
5. **`unsafe`.** Does it contain `unsafe`, and why? Read the `unsafe` blocks of anything that parses
   untrusted input, does cryptography or is on the commit path. `cargo geiger` or a search of the
   unpacked source gives the count. A crate that forbids `unsafe` is a point in its favour.
6. **Build scripts and proc macros.** Build scripts and procedural macros run arbitrary code on every
   developer machine and CI runner. List them for the new crate and its new transitive dependencies:
   `cargo metadata --format-version 1 --locked` and look for targets of kind `custom-build` and
   `proc-macro`. State what each does (compiles C, probes the platform, generates code).
7. **Footprint.** `cargo tree -p <crate> -e normal` before and after: how many new crates arrive? Turn off
   default features and enable only what is used.
8. **Advisories and history.** `cargo deny check` is clean, and the RustSec database has no open advisory
   for it. Skim its recent releases for yanks and for ownership changes.
9. **Pinning.** Security-critical crates use an exact version (`=x.y.z`); everything else uses the
   normal caret requirement. `Cargo.lock` is committed, and the lockfile change in your pull request
    must be exactly what the manifest change implies (release builds use `--locked`; the pull request
    gate does not yet, so the reviewer reads the lockfile diff).
10. **Docs.** The pull request says why in one paragraph. If the crate belongs to the security-critical
    set below, the audit notes row is updated in the same pull request.

## Security-critical set

These crates carry authentication, transport security or secret handling. A flaw in one is a flaw in the
database, and their updates are not routine.

| Crate | Version in `Cargo.lock` | Role here |
|---|---|---|
| `rustls` | 0.23.46 | TLS for the server transport (`latticed`, `transport.rs`), `ring` provider, TLS 1.2 and 1.3 |
| `ring` | 0.17.14 | Crypto provider for `rustls`, and for `rcgen` in tests. Not a direct dependency: it arrives through the `ring` feature, so its version is whatever the lockfile resolves |
| `subtle` | 2.6.1 | Constant-time comparison (`lattice-api` SCRAM, `lattice-abi` secrets) |
| `sha2` | 0.11.1 | SHA-256 for SCRAM-SHA-256 |
| `hmac` | 0.13.0 | HMAC for SCRAM-SHA-256 |
| `pbkdf2` | 0.13.0 | Password key derivation for SCRAM-SHA-256 |

Versions are those in `Cargo.lock` when this page was written; the lockfile is the source of truth.

Rules for the set:

- Direct dependencies are pinned with `=`, default features off, and only the features in use are enabled.
- A new crate in the set (or a new use of one, such as a second TLS library) needs an ADR first.
- An update is its own pull request, never mixed with feature work, and not merged because Dependabot
  opened it. The author reads the upstream changelog and the source diff between the two versions,
  confirms the known-answer vectors (the SCRAM vectors in `lattice-api`, the TLS and SCRAM tests in
  `latticed`) still pass, and updates the audit notes.
- A security advisory for any of them is handled through `SECURITY.md` the same day: assess, pin or
  patch, release notes.
- The `reviewed` label applies. When the change also touches authentication code, the `auth` review
  items in `docs/review-policy.md` apply as well.

## Audit notes

For each crate in the set, one entry records what was checked, for which version, by whom, and what was
not checked. Keep the entries here, newest first, and update the row in the pull request that changes
the version.

Entry template:

```text
### <crate> <version> (<yyyy-mm-dd>, <reviewer>)
Checked: licence, maintainers, unsafe blocks (count, where, why), build script, features enabled,
         diff against the previous audited version, advisories.
Result:  accepted / accepted with notes / rejected.
Not checked: anything skipped, said plainly.
```

Current entries: none. No audit of the crates above has been recorded yet; recording them is a
follow-up and this page does not claim they were reviewed.

## cargo-vet

[cargo-vet](https://mozilla.github.io/cargo-vet/) records audits in the repository and fails CI when a
dependency has neither an audit nor an explicit exemption. It would turn the checklist above from
review practice into a gate. It is not adopted: with one owner there is no second party to make an audit
mean more than a note, and a first run would start with every dependency exempted. The notes above are
the interim control. Revisit it when there is more than one maintainer, or before the first published
crate; the decision is recorded in issue #303.

## Updating and removing

Dependabot opens weekly pull requests for Cargo and for workflow actions. A routine update of a crate
outside the set is merged when the gate is green and `cargo deny check` is clean. Remove a dependency
as soon as the last use goes; `cargo tree -i <crate>` shows who still needs it. A yanked version or a
new advisory is fixed in the next pull request, not at the next scheduled update.

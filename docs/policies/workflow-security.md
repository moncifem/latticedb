# Workflow security

GitHub Actions workflows run with repository secrets and write tokens, so they are code that needs the
same review as the database. Two linters check them:

- **actionlint** (syntax, types, shellcheck on `run:` blocks). Run it on every workflow edit.
- **[zizmor](https://docs.zizmor.sh/)** (security audits: template injection, excessive permissions,
  credential persistence, dangerous triggers, unpinned actions, cache poisoning, impostor commits,
  known-vulnerable actions). Workflow: [`zizmor.yml`](../../.github/workflows/zizmor.yml).

## What runs

The `zizmor` workflow runs on pull requests and pushes to `main` that touch `.github/**`, weekly (so a
new audit rule is noticed without a workflow change) and by hand. It installs zizmor from
[`.github/requirements-zizmor.txt`](../../.github/requirements-zizmor.txt) with `pip --require-hashes`
(exact version, sha256 per wheel; Dependabot keeps it current), runs the default `regular` persona
online with the job's read-only token, and fails on any finding that is not listed in
[`.github/zizmor.yml`](../../.github/zizmor.yml). It is not a required check yet; the owner can add it
to the branch protection list after a trial period.

I did not use `zizmorcore/zizmor-action`: its README calls it early in development with possible
incompatible changes, the version released a week before this was written is too new to adopt, and it
does not fail the job on findings when it uploads SARIF. A hash-pinned PyPI install fails closed and
needs no extra third-party action.

Locally:

```bash
pip install --require-hashes --no-deps -r .github/requirements-zizmor.txt   # in a virtualenv
GH_TOKEN=$(gh auth token) zizmor --config .github/zizmor.yml .github/workflows   # online audits
zizmor --offline --config .github/zizmor.yml .github/workflows                   # no token needed
```

`--fix=all` rewrites files; read the diff before keeping it (it also applies the `$/` style change
that is disabled here).

## Rules for workflow changes

These match the `workflows` row of [`docs/review-policy.md`](../review-policy.md):

- Pin every action by full commit SHA with the exact version in a trailing comment (`# v7.0.1`, not
  `# v2`). Prefer a release at least two weeks old.
- `permissions: {}` or `contents: read` at the top; widen per job, never at workflow level.
- `actions/checkout` sets `persist-credentials: false` unless the job pushes with git, and then says
  so.
- Never expand `${{ ... }}` inside `run:`. Pass it through `env:` and use `"$VAR"`.
- No `pull_request_target` or `workflow_run` that runs pull request code or uses its artifacts.
- Release and publishing jobs start from an empty cache.

## Triage record (first run)

Findings of the first run on `main`, with zizmor 1.30.1 online, regular persona: 54 before, 0 after.

| Audit | Count | Decision |
|---|---|---|
| artipacked | 45 | Fixed: `persist-credentials: false` on every checkout. No job pushes with git; the one `git ls-remote` on a public repository needs no credentials. |
| template-injection | 3 | Fixed: values moved to `env:` (`github.base_ref` in `pr.yml`, `github.repository` and `inputs.ref` in `package.yml`). The remaining informational one, `job.services.postgres.id` in `sql-planner-spike.yml`, is a runner-generated container id, not attacker input; left as is. |
| ref-version-mismatch | 15 | Fixed: the `taiki-e/install-action` comments said `# v2` while the SHA is `v2.87.33`. |
| cache-poisoning | 1 | Fixed: tagged releases and release archives no longer restore the Rust cache (`release.yml`, `package.yml`). Nightly and rehearsal runs keep it. Cost: a tagged release builds from scratch. |
| dangerous-triggers | 1 | Kept, with a reason in `.github/zizmor.yml`: `main-guard.yml` needs `workflow_run`, filters on event, branch and conclusion, checks out its own revision and runs no code or artifacts from the failed run. |
| self-repository | 4 | Disabled, reason in `.github/zizmor.yml`: `$/` is a July 2026 GitHub syntax needing runner 2.336.0 or newer; `./` still works and support in actionlint is not verified. |

Auditor persona (not enforced): `labeler.yml` had `pull-requests: write` at workflow level, fixed by
moving it to the job. Open items are tracked in issue #327: the unpinned `postgres:17` service image
in `nightly.yml`, and low-severity template-expansion, missing concurrency and permission-comment notes.

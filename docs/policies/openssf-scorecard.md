# OpenSSF Scorecard

[OpenSSF Scorecard](https://scorecard.dev/) scores a repository's supply-chain practices from 0 to 10
per check (pinned dependencies, token permissions, branch protection, dependency updates, security
policy, signed releases and others). Not to be confused with the project's own evidence scorecard
(`cargo xtask scorecard`, `tools/scorecard.py`), which is about test and benchmark results.

Workflow: [`.github/workflows/openssf-scorecard.yml`](../../.github/workflows/openssf-scorecard.yml).
It runs weekly (Monday, 04:17 UTC), on every push to `main` and by hand. It is not a pull request
check and not a required check, so it can never block a merge.

## How it is set up

- The action is pinned by commit SHA and kept current by Dependabot like every other action.
- Top-level `permissions: {}`; the one job gets `contents: read`, `security-events: write` (code
  scanning upload) and `id-token: write` (publishing). Scorecard refuses to publish when a workflow has
  top-level `env` or `defaults`, workflow-level write permissions, or steps from outside its allowed
  list (checkout, upload-artifact, upload-sarif, scorecard-action, harden-runner), so the job has only
  those.
- Results go to the public Scorecard API (`publish_results: true`), which the badge reads, to the code
  scanning tab as SARIF (category `openssf-scorecard`), and to a 14-day workflow artifact.
- It does not run on forks.

## Reading the result

Findings appear under Security, Code scanning. Each alert names the check and the remediation.
Triage them like any other finding: fix, or record in an issue why a check does not apply or is out of
reach for a one-owner project (for example Code-Review needs a second reviewer, and Fuzzing is
detected only for some integrations). Do not change a setting only to raise the number; the owner
decides repository settings and branch protection.

## Badge

The badge is deliberately not in the README yet: the API returns nothing until a publishing run has
succeeded on `main`. After the first successful run, check that
`https://api.scorecard.dev/projects/github.com/moncifem/latticedb` returns a result, then add to the
README:

```markdown
[![OpenSSF Scorecard](https://api.scorecard.dev/projects/github.com/moncifem/latticedb/badge)](https://scorecard.dev/viewer/?uri=github.com/moncifem/latticedb)
```

Sources: the [scorecard-action README](https://github.com/ossf/scorecard-action) (workflow
restrictions, inputs, badge) and the [Scorecard checks](https://github.com/ossf/scorecard/blob/main/docs/checks.md).

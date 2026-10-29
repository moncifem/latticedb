# Flaky test policy

A test that fails once and passes on rerun with no code change is a defect with the same priority as
a failing test. Within one business day it is either fixed or quarantined in `tools/quarantine.toml`
with an owner, a reason and an expiry date. `cargo xtask check-quarantine` fails the build when an
entry is malformed or expired. The quarantined count appears on the scorecard. Deterministic
simulation tests may never be quarantined: a flaky simulation test means broken determinism.

## Nightly failures

A failed scheduled night opens an issue labelled `nightly-failure` (or comments on the one still
open) with the run link, the failed jobs and the seed, scenario and invariant lines from their logs.
The traces are in the run's artifacts. Fix or quarantine according to the rules above; close the
issue when the next scheduled night is green.

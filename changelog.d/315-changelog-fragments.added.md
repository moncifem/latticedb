Changelog fragments (#315): each pull request adds one file under `changelog.d/` instead of editing
`CHANGELOG.md`, so parallel pull requests no longer conflict there. `tools/changelog.py` validates the
fragments in a new `changelog` check (advisory, in `pr-governance`) and compiles them into the release
section. Entries already in `[Unreleased]` stay and are carried into the next release by the same step.

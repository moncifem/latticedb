# Changelog fragments

Each pull request adds one file here instead of editing `CHANGELOG.md`. Two pull requests then never
touch the same lines, so the strict up-to-date merge no longer stops on changelog conflicts. The file
is compiled into `CHANGELOG.md` when a release is cut.

## Add a fragment

Create `changelog.d/<number>-<slug>.<kind>.md`:

- `<number>`: the pull request number, or the issue number when the pull request does not exist yet.
- `<slug>`: lowercase words joined by `-`, such as `wal-guard-rewrite`.
- `<kind>`: `added`, `changed`, `deprecated`, `removed`, `fixed` or `security`.

The content is the entry as Markdown, one short paragraph, no leading `- `, no headings, no trailing
spaces, under 2000 characters, and one final newline. State what a user or contributor can now do or
what changed for them, and what is not done. Do not put an email address or an attribution line in it.

```text
changelog.d/315-changelog-fragments.added.md
```

```markdown
Changelog fragments: one file per pull request under `changelog.d/`, validated by the `changelog`
check and compiled into `CHANGELOG.md` at release time.
```

A pull request that needs no entry (a typo, a CI-only tweak nobody downstream sees) carries the
`no-changelog` label, applied by the owner, and adds no fragment. Dependabot pull requests are exempt.

## Check and preview

```bash
python3 tools/changelog.py check                                  # validate the files in the tree
python3 tools/changelog.py compile --version 0.1.0-alpha.1        # print the release section, change nothing
python3 -m unittest discover -s tools -p 'test_changelog.py'
```

## Release

`python3 tools/changelog.py compile --version X.Y.Z --write` moves what is still written under
`[Unreleased]` by hand (the old way) and every fragment into a new `## [X.Y.Z] - date` section, one
`###` heading per kind, deletes the fragments and leaves an empty `[Unreleased]`. Commit the result in
the release pull request (see `docs/releasing.md`). Entries inside a heading are ordered by number,
then slug.

## Migration

Existing entries stay in `CHANGELOG.md` as they are. Until the next release, the compile step carries
them into the new section. A pull request opened before this convention may still edit `[Unreleased]`
once; it should add a fragment instead when it is next rebased.

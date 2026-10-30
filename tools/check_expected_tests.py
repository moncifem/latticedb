#!/usr/bin/env python3
"""Check that every test named in a manifest appears in `cargo test -- --list` output.

Usage: check_expected_tests.py MANIFEST LISTING

MANIFEST has one test name per line (blank lines and lines starting with `#` are ignored). LISTING is
the saved `--list` output, whose test lines look like `name: test`. Extra tests in the listing are
fine, so adding tests never breaks the check; a missing name fails with a diff-style message.
"""

import sys


def listed_tests(text):
    return {line[: -len(": test")] for line in text.splitlines() if line.endswith(": test")}


def required_tests(text):
    return [
        line.strip()
        for line in text.splitlines()
        if line.strip() and not line.lstrip().startswith("#")
    ]


def missing(manifest_text, listing_text):
    present = listed_tests(listing_text)
    return [name for name in required_tests(manifest_text) if name not in present]


def main(argv):
    if len(argv) != 3:
        print(__doc__, file=sys.stderr)
        return 2
    with open(argv[1], encoding="utf-8") as manifest, open(argv[2], encoding="utf-8") as listing:
        manifest_text, listing_text = manifest.read(), listing.read()
    if not listed_tests(listing_text):
        print(f"error: {argv[2]} lists no tests; the cargo --list step produced nothing")
        return 1
    absent = missing(manifest_text, listing_text)
    if absent:
        print(f"error: {len(absent)} required test(s) from {argv[1]} not found in {argv[2]}:")
        for name in absent:
            print(f"- {name}")
        print("A required guard test was renamed or removed. Restore it, or update the manifest")
        print("in the same pull request if the removal is intended and reviewed.")
        return 1
    print(f"ok: all {len(required_tests(manifest_text))} required tests present")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))

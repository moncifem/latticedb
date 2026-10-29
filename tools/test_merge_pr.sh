#!/usr/bin/env bash
# Fake-API tests only; never contact GitHub or modify real PRs.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s "$here" -p test_merge_pr.py -v

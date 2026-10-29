#!/usr/bin/env bash
# Serial exact-head merger; independent review/owner decisions are prerequisites.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
exec python3 "$here/merge_pr.py" "$@"

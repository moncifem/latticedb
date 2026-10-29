#!/usr/bin/env bash
# Rewrites conformance/vectors.json from the Rust codecs.
#
# The vectors are produced by crates/lattice-api/src/conformance.rs. That crate may not touch the
# file system, so with LATTICE_UPDATE_VECTORS=1 the test prints the new file between two marker
# lines and this script saves it. Review the diff, then commit the file together with any change to
# the clients that read it (clients/typescript).
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

tmp="$(mktemp)"
trap 'rm -f "$tmp"' EXIT

LATTICE_UPDATE_VECTORS=1 cargo test -p lattice-api --lib \
  conformance::vectors_file_matches_rust_encoder -- --exact --nocapture 2>/dev/null |
  sed -n '/^-----BEGIN LATTICE CONFORMANCE VECTORS-----$/,/^-----END LATTICE CONFORMANCE VECTORS-----$/p' |
  sed '1d;$d' >"$tmp"

if [ ! -s "$tmp" ]; then
  echo "update-conformance-vectors: the test printed no vectors; run it by hand to see why" >&2
  exit 1
fi

mkdir -p conformance
cp "$tmp" conformance/vectors.json
echo "wrote conformance/vectors.json ($(wc -c <conformance/vectors.json | tr -d ' ') bytes)"

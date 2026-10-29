#!/usr/bin/env bash
# Public API snapshot check (issue 281). Generates the public API of each library crate with
# `cargo public-api` and diffs it against api-snapshots/<crate>.txt.
#
#   tools/check_public_api.sh                 check every covered crate (a missing snapshot fails)
#   tools/check_public_api.sh --update        rewrite the snapshots (creates missing ones)
#   tools/check_public_api.sh [--update] lattice-abi lattice-core   only these crates
#
# Pins: cargo-public-api $CPA_VERSION and nightly $API_NIGHTLY (rustdoc JSON needs nightly).
# The nightly is used only to build rustdoc JSON; the Rust gate still uses the stable toolchain.
# See docs/api-snapshots.md.
set -euo pipefail

CPA_VERSION="0.52.0"
API_NIGHTLY="${API_NIGHTLY:-nightly-2026-10-27}"
# The one list of covered crates. Not covered, on purpose: lattice-sys (unsafe OS shims, internal),
# lattice-testkit (test support), lattice-identity, lattice-intel, lattice-packs and lattice-service
# (server-side crates still changing shape in Phase 2/3, no external consumers yet; add them here
# when their API stabilises). lattice-bench, lattice-cli and latticed are not libraries.
CRATES=(lattice-abi lattice-seams lattice-lsl lattice-sim lattice-core lattice-engines lattice-api lattice-sql lattice-sdk lattice-evalkit)

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"
mkdir -p api-snapshots

update=0
selected=()
for arg in "$@"; do
  case "$arg" in
    --update) update=1 ;;
    -*) echo "unknown option: $arg" >&2; exit 2 ;;
    *) selected+=("$arg") ;;
  esac
done
[ ${#selected[@]} -gt 0 ] || selected=("${CRATES[@]}")
for crate in "${selected[@]}"; do
  case " ${CRATES[*]} " in
    *" $crate "*) ;;
    *) echo "error: $crate is not a covered crate; covered: ${CRATES[*]} (edit CRATES in this script to add one)" >&2; exit 2 ;;
  esac
done

if ! command -v cargo-public-api >/dev/null 2>&1; then
  echo "cargo-public-api is not installed: cargo install cargo-public-api --version $CPA_VERSION --locked" >&2
  exit 2
fi
have="$(cargo-public-api --version | awk '{print $2}')"
if [ "$have" != "$CPA_VERSION" ]; then
  echo "cargo-public-api $have found, $CPA_VERSION is pinned (snapshots differ across versions)" >&2
  exit 2
fi

target_dir="${CARGO_TARGET_DIR:-$root/target}"
failed=0
for crate in "${selected[@]}"; do
  snap="api-snapshots/$crate.txt"
  if [ "$update" -eq 0 ] && [ ! -f "$snap" ]; then
    echo "error: no snapshot for $crate (api-snapshots/$crate.txt is missing); create it with: tools/check_public_api.sh --update $crate" >&2
    failed=1
    continue
  fi
  cargo "+$API_NIGHTLY" rustdoc -q -p "$crate" --lib -- -Z unstable-options --output-format json >/dev/null
  json="$target_dir/doc/${crate//-/_}.json"
  new="$(mktemp)"
  cargo-public-api --rustdoc-json "$json" -ss --color never >"$new"
  if [ "$update" -eq 1 ]; then
    mv "$new" "$snap"
    echo "updated $snap"
  elif diff -u "$snap" "$new"; then
    echo "ok $crate"
    rm -f "$new"
  else
    failed=1
    rm -f "$new"
  fi
done

if [ "$failed" -ne 0 ]; then
  cat >&2 <<MSG

The public API check failed (a diff or a missing snapshot above). If the change is intended, run
  tools/check_public_api.sh --update
and commit the api-snapshots/ changes so reviewers see the API diff. See docs/api-snapshots.md.
MSG
  exit 1
fi

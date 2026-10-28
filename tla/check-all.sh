#!/bin/sh
# Usage: tla/check-all.sh  (set TLA2TOOLS to the jar path). Checks every model that has a .cfg.
set -eu
dir=$(dirname "$0")
for cfg in "$dir"/*.cfg; do
  "$dir/run-tlc.sh" "${cfg%.cfg}.tla"
done

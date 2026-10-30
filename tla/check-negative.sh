#!/bin/sh
# Usage: tla/check-negative.sh  (set TLA2TOOLS to the jar path).
# Each config in tla/negative/ switches one protection off in a model. TLC must fail it with the named
# invariant, otherwise the model's property could not tell a protected design from a broken one.
set -eu
: "${TLA2TOOLS:?set TLA2TOOLS to the path of tla2tools.jar}"
dir=$(dirname "$0")
out=$(mktemp)
trap 'rm -f "$out"' EXIT
status=0
# config name, model, invariant that must be reported violated
while read -r name model inv; do
  if java -XX:+UseParallelGC -cp "$TLA2TOOLS" tlc2.TLC -config "$dir/negative/$name.cfg" "$dir/$model.tla" >"$out" 2>&1; then
    echo "FAIL $name: TLC passed with the protection removed"
    status=1
  elif grep -q "Invariant $inv is violated" "$out"; then
    echo "ok   $name: $inv violated as expected"
  else
    echo "FAIL $name: TLC failed, but not with 'Invariant $inv is violated'"
    cat "$out"
    status=1
  fi
done <<EOF
TreeMoveNoCycleCheck TreeMove Acyclic
TreeMoveNoUndoRedo TreeMove Convergent
TreeMoveNoDepthCheck TreeMove DepthBounded
CommitAckBeforeSync Commit AckedAreDurable
CommitSkipFsync Commit AckedAreDurable
CommitSkipFsyncThenCrash Commit AckedSurviveCrash
CommitSkipRecoverySync Commit MemoryIsDurable
FenceIgnored Fence AcceptOnlyCurrent
FenceIgnoredGoesBack Fence NeverGoesBack
FenceIgnoredFuture Fence NothingFromTheFuture
FenceNotRebuilt Fence AcceptOnlyCurrent
FenceNotRebuiltGoesBack Fence NeverGoesBack
HlcRegress Hlc StrictlyIncreasing
HlcCounterStuck Hlc StrictlyIncreasing
SetConvergenceNoTombstone SetConvergence Convergent
EOF
exit "$status"

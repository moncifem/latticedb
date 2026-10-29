#!/usr/bin/env bash
# Tests for tools/merge-pr.sh against a fake `gh` on PATH. No network, no real repository.
#
# Each scenario writes queue files into a scratch directory: <pr>.<field>, one answer per line.
# The fake gh consumes the first line while more than one remains, so the last line repeats.
# A line `ERR` makes the call fail like a 5xx; `EMPTY` prints nothing and succeeds.
set -u

here=$(cd "$(dirname "$0")" && pwd)
script=$here/merge-pr.sh
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
mkdir "$work/bin"

cat >"$work/bin/gh" <<'SHIM'
#!/usr/bin/env bash
# Fake gh. FAKE_DIR holds the queues and calls.log.
echo "$*" >>"$FAKE_DIR/calls.log"
next() { # next FILE: print and consume a queued answer
  f=$FAKE_DIR/$1
  [ -f "$f" ] || { echo "no queue $1" >&2; return 1; }
  line=$(head -n 1 "$f")
  if [ "$(wc -l <"$f")" -gt 1 ]; then tail -n +2 "$f" >"$f.tmp" && mv "$f.tmp" "$f"; fi
  case "$line" in
    ERR) echo "HTTP 502: Bad Gateway" >&2; return 1 ;;
    EMPTY) return 0 ;;
    *) echo "$line" ;;
  esac
}
case "$1 $2" in
  "repo view") echo "example/repo" ;;
  "pr view")
    pr=$3
    field=""
    while [ $# -gt 0 ]; do [ "$1" = --json ] && field=$2; shift; done
    case "$field" in
      statusCheckRollup) field=fails ;;
    esac
    next "$pr.$field"
    ;;
  "pr update-branch") exit 0 ;;
  "pr merge") echo MERGED >"$FAKE_DIR/$3.state"; exit 0 ;;
  *) echo "unexpected gh call: $*" >&2; exit 1 ;;
esac
SHIM
chmod +x "$work/bin/gh"

pass=0
failed=0

# scenario NAME PR EXPECTED_EXIT OUTPUT_PATTERN FORBIDDEN_CALL [extra args]; queues come from `q`.
q() { printf '%s\n' "${@:2}" >"$FAKE_DIR/$1"; }

run() {
  name=$1 pr=$2 want=$3 pattern=$4 forbidden=$5
  shift 5
  out=$(PATH="$work/bin:$PATH" MERGE_PR_SLEEP=0 "$script" --tries 12 "$@" "$pr" 2>&1)
  code=$?
  ok=1
  [ "$code" -eq "$want" ] || ok=0
  case "$out" in *"$pattern"*) ;; *) ok=0 ;; esac
  if [ -n "$forbidden" ] && grep -q "$forbidden" "$FAKE_DIR/calls.log"; then ok=0; fi
  if [ "$ok" -eq 1 ]; then
    pass=$((pass + 1))
    echo "ok   $name"
  else
    failed=$((failed + 1))
    echo "FAIL $name (exit $code, want $want): $out"
  fi
}

fresh() {
  FAKE_DIR=$work/$1
  export FAKE_DIR
  mkdir -p "$FAKE_DIR"
  : >"$FAKE_DIR/calls.log"
}

# 5xx and empty answers on the state read are retried, never treated as a result.
fresh 5xx
q 1.state ERR EMPTY OPEN OPEN OPEN OPEN OPEN
q 1.headRefOid aaa
q 1.mergeStateStatus CLEAN
q 1.fails 0
run "5xx on state read is retried then merged" 1 0 "PR 1: merged" ""
if [ "$(grep -c 'pr view' "$FAKE_DIR/calls.log")" -lt 4 ]; then
  failed=$((failed + 1))
  echo "FAIL 5xx scenario did not retry the read"
fi

# A state that never becomes readable ends as a non-zero give-up, with no merge attempt.
fresh 5xx-forever
q 2.state ERR
run "persistent 5xx gives up without merging" 2 1 "gave up" "pr merge"

# A failing check on the head refuses, even if the merge state claims CLEAN.
fresh failing
q 3.state OPEN
q 3.headRefOid bbb
q 3.mergeStateStatus CLEAN
q 3.fails 1
run "failing check is refused" 3 1 "refused" "pr merge"

# BEHIND: update the branch, wait for the new head, merge only when CLEAN on it.
fresh behind
q 4.state OPEN OPEN OPEN OPEN OPEN OPEN OPEN OPEN OPEN OPEN
q 4.headRefOid old old old old new
q 4.mergeStateStatus BEHIND BEHIND BLOCKED CLEAN
q 4.fails 0
run "BEHIND then CLEAN updates then merges" 4 0 "PR 4: merged" ""
grep -q 'pr update-branch 4' "$FAKE_DIR/calls.log" || { failed=$((failed + 1)); echo "FAIL update-branch not called"; }

# BEHIND, update, then the new head fails: refused after the re-read.
fresh behind-red
q 5.state OPEN
q 5.headRefOid old new
q 5.mergeStateStatus BEHIND BLOCKED
q 5.fails 0 0 1
run "new head with a failed check is refused" 5 1 "refused" "pr merge"

# Already merged: success, nothing else called.
fresh merged
q 6.state MERGED
run "already merged" 6 0 "already merged" "pr merge"

# Closed without merge is a refusal.
fresh closed
q 7.state CLOSED
run "closed PR is refused" 7 1 "closed" "pr merge"

# Unknown values: a garbage state, an unknown merge status and a non-numeric check count.
fresh unknown
q 8.state WEIRD OPEN
q 8.headRefOid ccc
q 8.mergeStateStatus UNKNOWN CLEAN
q 8.fails oops EMPTY 0
run "unknown values are retried, not acted on" 8 0 "PR 8: merged" ""

# Never-resolving unknown merge status ends in a give-up and never a merge.
fresh unknown-forever
q 9.state OPEN
q 9.headRefOid ddd
q 9.mergeStateStatus UNKNOWN
q 9.fails 0
run "unknown merge status forever gives up" 9 1 "gave up" "pr merge"

# Dry run reports and never writes.
fresh dry
q 10.state OPEN
q 10.headRefOid eee
q 10.mergeStateStatus BEHIND
q 10.fails 0
run "dry run on BEHIND only reports" 10 0 "would update" "pr update-branch\|pr merge" --dry-run

fresh dry-clean
q 11.state OPEN
q 11.headRefOid fff
q 11.mergeStateStatus CLEAN
q 11.fails 0
run "dry run on CLEAN only reports" 11 0 "would merge" "pr merge" --dry-run

# Serial: a refusal stops later PRs from being touched.
fresh serial
q 12.state OPEN
q 12.headRefOid ggg
q 12.mergeStateStatus CLEAN
q 12.fails 2
q 13.state OPEN
out=$(PATH="$work/bin:$PATH" MERGE_PR_SLEEP=0 "$script" --tries 3 12 13 2>&1)
code=$?
case "$out" in
  *"PR 13: skipped"*) [ "$code" -ne 0 ] && { pass=$((pass + 1)); echo "ok   later PRs are skipped after a refusal"; } ;;
  *) failed=$((failed + 1)); echo "FAIL serial skip: $out" ;;
esac

echo "$pass passed, $failed failed"
[ "$failed" -eq 0 ]

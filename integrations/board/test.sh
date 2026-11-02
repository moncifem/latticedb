#!/bin/sh
# End-to-end test of the board helper against a local latticed.
#
# Needs psql and a latticed binary: LATTICE_BIN_DIR/latticed, or latticed on PATH.
# It starts its own server on a free loopback port with a fresh data directory, runs the flow,
# restarts the server on the same data directory, checks what survived, and removes everything
# it created. It never touches another server.
#
#   LATTICE_BIN_DIR=/path/to/bin integrations/board/test.sh
#
# EXISTING-SERVER MODE: with BOARD_TEST_EXISTING=1 the script starts no server. It runs against the
# server the usual PG* variables (and PGSSLMODE, PGSSLROOTCERT, PGPASSWORD, ...) point at, and
# needs no latticed binary. The board there may already hold tasks: checks that count rows use the
# count they find first, and the orchestration part cancels the open tasks it did not create (a
# state change, nothing is deleted) so `next` has a known queue. Skipped in this mode, and said so
# in the output: the password-in-URL checks (they build a URL for the local trust listener), and
# the stop, restart and durability checks. Give it a fresh BOARD_STATE_DIR of its own (the default
# is a directory in the work area) and unique author names are used throughout.
#
# Exit status 0 when every check passes, 1 otherwise.

set -u

HERE=$(cd "$(dirname "$0")" && pwd) || exit 1
BOARD="$HERE/board"

EXISTING=${BOARD_TEST_EXISTING:-}
LATTICED=
if [ -z "$EXISTING" ]; then
  if [ -n "${LATTICE_BIN_DIR:-}" ]; then
    LATTICED="$LATTICE_BIN_DIR/latticed"
  else
    LATTICED=$(command -v latticed) || { echo "test: latticed not found; set LATTICE_BIN_DIR" >&2; exit 1; }
  fi
  [ -x "$LATTICED" ] || { echo "test: $LATTICED is not executable" >&2; exit 1; }
fi
command -v psql >/dev/null 2>&1 || { echo "test: psql not found" >&2; exit 1; }

TAB=$(printf '\t')
NL='
'
PASS=0
FAILS=0
PID=
WORK=$(mktemp -d "${TMPDIR:-/tmp}/board-test.XXXXXX") || exit 1
chmod 700 "$WORK"
DATA="$WORK/data"
LOG=
START_N=0
mkdir -m 700 "$DATA"

cleanup() {
  if [ -n "$PID" ]; then
    kill -TERM "$PID" 2>/dev/null
    wait "$PID" 2>/dev/null
    PID=
  fi
  rm -rf "$WORK"
}
trap cleanup EXIT HUP INT TERM

check() {
  # check NAME COMMAND...: pass when the command succeeds
  ck_name=$1
  shift
  if "$@"; then
    PASS=$((PASS + 1))
    echo "ok   - $ck_name"
  else
    FAILS=$((FAILS + 1))
    echo "FAIL - $ck_name"
  fi
}

expect_eq() {
  # expect_eq NAME EXPECTED ACTUAL
  if [ "$2" = "$3" ]; then
    PASS=$((PASS + 1))
    echo "ok   - $1"
  else
    FAILS=$((FAILS + 1))
    echo "FAIL - $1"
    echo "       expected: $2"
    echo "       actual:   $3"
  fi
}

expect_rc() {
  # expect_rc NAME EXPECTED_STATUS COMMAND...: pass when the command exits with that status
  er_name=$1
  er_want=$2
  shift 2
  "$@" >/dev/null 2>&1
  er_got=$?
  expect_eq "$er_name" "$er_want" "$er_got"
}

pick_port() {
  # A free loopback port. Another process may take it before latticed binds it, which start_server
  # detects and retries.
  if command -v python3 >/dev/null 2>&1; then
    python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1]); s.close()'
  else
    echo 55432
  fi
}

if [ -z "$EXISTING" ]; then
  PORT=$(pick_port) || exit 1
  export PGHOST=127.0.0.1
  export PGPORT="$PORT"
  export PGUSER=dev
  export PGPASSWORD=unused-under-trust
else
  PORT=${PGPORT:-}
fi
export LC_MESSAGES=C

# start_server: start latticed on $PORT with a per-start log, and wait until (a) its own log says it
# is listening on $PORT and (b) SQL answers on $PORT. A process that dies before listening (the
# port was taken) gets a new port and another try.
start_server() {
  ss_try=0
  while [ "$ss_try" -lt 5 ]; do
    ss_try=$((ss_try + 1))
    START_N=$((START_N + 1))
    LOG="$WORK/latticed-$START_N.log"
    "$LATTICED" --data-dir "$DATA" --development-policy allow-all --listen "127.0.0.1:$PORT" \
      --stdin-control off >"$LOG" 2>&1 &
    PID=$!
    n=0
    while [ "$n" -lt 150 ]; do
      if grep -F "latticed listening on 127.0.0.1:$PORT (" "$LOG" >/dev/null 2>&1; then
        if psql -X -q -A -t -c 'SELECT 1' >/dev/null 2>&1; then
          return 0
        fi
      fi
      if ! kill -0 "$PID" 2>/dev/null; then
        wait "$PID" 2>/dev/null
        PID=
        break
      fi
      sleep 0.2
      n=$((n + 1))
    done
    if [ -n "$PID" ]; then
      echo "test: latticed did not report listening on port $PORT; log:" >&2
      cat "$LOG" >&2
      kill -TERM "$PID" 2>/dev/null
      wait "$PID" 2>/dev/null
      PID=
      return 1
    fi
    echo "test: port $PORT was taken before latticed bound it; trying another" >&2
    PORT=$(pick_port) || return 1
    PGPORT="$PORT"
    export PGPORT
  done
  echo "test: latticed did not start after $ss_try tries; logs in $WORK" >&2
  return 1
}

stop_server() {
  kill -TERM "$PID"
  wait "$PID"
  rc=$?
  PID=
  return "$rc"
}

if [ -z "$EXISTING" ]; then
  echo "test: latticed=$LATTICED port=$PORT data=$DATA"
  start_server || exit 1
else
  echo "test: EXISTING-SERVER MODE, host=${PGHOST:-local} port=${PORT:-default}; skipping: password-in-URL checks, restart and durability checks"
fi

# --- init and idempotence -------------------------------------------------------------------
check "init creates the tables" "$BOARD" init
check "init again keeps working" "$BOARD" init
two_inits() {
  ti_p1=
  ti_p2=
  "$BOARD" init >"$WORK/init1.out" 2>&1 &
  ti_p1=$!
  "$BOARD" init >"$WORK/init2.out" 2>&1 &
  ti_p2=$!
  wait "$ti_p1" && wait "$ti_p2"
}
check "two inits at once both succeed" two_inits

# --- authors: required for writes, validated, unique per agent ------------------------------
BOARD_AUTHOR=maint
export BOARD_AUTHOR
BOARD_STATE_DIR="${BOARD_STATE_DIR:-$WORK/fences}"
RUN=r$$
export BOARD_STATE_DIR

LIST_BASE=$("$BOARD" list | wc -l | tr -d ' ')
T_PLAIN=$("$BOARD" add "Plain task") || FAILS=$((FAILS + 1))
T_BIG=$("$BOARD" add "Urgent refactor" --priority 5 --body "details here") || FAILS=$((FAILS + 1))
T_CHILD=$("$BOARD" add "Child of plain" --parent "$T_PLAIN") || FAILS=$((FAILS + 1))
check "add returns an id" test -n "$T_PLAIN"

LIST=$("$BOARD" list)
FIRST=$(printf '%s\n' "$LIST" | grep -E "^($T_PLAIN|$T_BIG|$T_CHILD)" | head -n 1 | cut -f1)
expect_eq "list puts the highest priority first" "$T_BIG" "$FIRST"
check "list has the header and three more tasks" test "$(printf '%s\n' "$LIST" | wc -l | tr -d ' ')" = "$((LIST_BASE + 3))"

expect_rc "a write without BOARD_AUTHOR is refused" 1 env -u BOARD_AUTHOR "$BOARD" add "No author"
expect_rc "claim without BOARD_AUTHOR is refused" 1 env -u BOARD_AUTHOR "$BOARD" claim "$T_PLAIN"
expect_rc "note without BOARD_AUTHOR is refused" 1 env -u BOARD_AUTHOR "$BOARD" note "$T_PLAIN" "x"
expect_rc "done without BOARD_AUTHOR is refused" 1 env -u BOARD_AUTHOR "$BOARD" "done" "$T_PLAIN"
expect_rc "decide without BOARD_AUTHOR is refused" 1 env -u BOARD_AUTHOR "$BOARD" decide "$T_PLAIN" "x"
expect_rc "a read needs no BOARD_AUTHOR" 0 env -u BOARD_AUTHOR "$BOARD" list
expect_rc "an author with a space is refused" 1 env BOARD_AUTHOR='agent one' "$BOARD" claim "$T_PLAIN"
check "the refused writes created no task" test "$("$BOARD" list | wc -l | tr -d ' ')" = "$((LIST_BASE + 3))"

if "$BOARD" add "Orphan" --parent t0000000000 >/dev/null 2>"$WORK/orphan.err"; then
  FAILS=$((FAILS + 1))
  echo "FAIL - add with a missing parent must fail"
else
  PASS=$((PASS + 1))
  echo "ok   - add with a missing parent fails"
fi
check "a failed add with a missing parent leaves no task" \
  test "$("$BOARD" list | wc -l | tr -d ' ')" = "$((LIST_BASE + 3))"

# --- quoting and injection ------------------------------------------------------------------
EVIL="Robert'); DROP TABLE tasks; -- \"q\" ; é 日本語 :'author' :\"x\" %s 'end"
T_EVIL=$("$BOARD" add "$EVIL") || FAILS=$((FAILS + 1))
SHOWN=$("$BOARD" show "$T_EVIL" | grep '^title|' | cut -d'|' -f2-)
expect_eq "a hostile title is stored and read back verbatim" "$EVIL" "$SHOWN"
check "the tasks table survived the hostile title" test "$("$BOARD" list | wc -l | tr -d ' ')" = "$((LIST_BASE + 4))"
expect_rc "a title with a backslash is refused" 1 "$BOARD" add 'path C:\temp'
expect_rc "a title with a carriage return is refused" 1 "$BOARD" add "$(printf 'bad\rtitle')"
expect_rc "a title with a bell character is refused" 1 "$BOARD" add "$(printf 'bell\007')"
check "the refused titles stored nothing" test "$("$BOARD" list | wc -l | tr -d ' ')" = "$((LIST_BASE + 4))"

# --- claim race: three claimers, five tasks, exactly one winner each ------------------------
race_task() {
  rt_id=$1
  rt_dir="$WORK/race-$rt_id"
  mkdir -p "$rt_dir"
  rt_pids=
  for rt_i in 1 2 3; do
    (BOARD_AUTHOR=racer$rt_i "$BOARD" claim "$rt_id" >"$rt_dir/out.$rt_i" 2>&1
     echo $? >"$rt_dir/rc.$rt_i") &
    rt_pids="$rt_pids $!"
  done
  # Wait only for these claimers: a bare wait would also wait for latticed, which is a child too.
  # shellcheck disable=SC2086
  wait $rt_pids
  rt_winners=0
  rt_winner=
  for rt_i in 1 2 3; do
    if [ "$(cat "$rt_dir/rc.$rt_i")" = 0 ]; then
      rt_winners=$((rt_winners + 1))
      rt_winner=racer$rt_i
    fi
  done
  rt_losers_ok=1
  for rt_i in 1 2 3; do
    if [ "$(cat "$rt_dir/rc.$rt_i")" != 0 ] && [ "$(cat "$rt_dir/rc.$rt_i")" != 2 ]; then
      rt_losers_ok=0
    fi
  done
  [ "$rt_winners" = 1 ] && [ "$rt_losers_ok" = 1 ] || {
    echo "       winners=$rt_winners losers_ok=$rt_losers_ok" >&2
    return 1
  }
  rt_owner=$("$BOARD" show "$rt_id" | grep '^owner|' | cut -d'|' -f2)
  [ "$rt_owner" = "$rt_winner" ] || { echo "       owner=$rt_owner winner=$rt_winner" >&2; return 1; }
  rt_claims=$("$BOARD" since 0 | awk -F"$TAB" -v t="$rt_id" -v k=claim '$2 == t && $4 == k' | wc -l | tr -d ' ')
  [ "$rt_claims" = 1 ] || { echo "       claim events=$rt_claims" >&2; return 1; }
  return 0
}

# twin_claim ID: two claimers that share one name. Both calls succeed (the second is "already
# yours"), and only one claim event is recorded. This is the documented behaviour; it is why each
# agent needs its own name.
twin_claim() {
  tw_id=$1
  tw_dir="$WORK/twin-$tw_id"
  mkdir -p "$tw_dir"
  (BOARD_AUTHOR=twin "$BOARD" claim "$tw_id" >"$tw_dir/out.1" 2>&1; echo $? >"$tw_dir/rc.1") &
  tw_p1=$!
  (BOARD_AUTHOR=twin "$BOARD" claim "$tw_id" >"$tw_dir/out.2" 2>&1; echo $? >"$tw_dir/rc.2") &
  tw_p2=$!
  wait "$tw_p1" "$tw_p2"
  [ "$(cat "$tw_dir/rc.1")" = 0 ] && [ "$(cat "$tw_dir/rc.2")" = 0 ] || return 1
  tw_claims=$("$BOARD" since 0 | awk -F"$TAB" -v t="$tw_id" -v k=claim '$2 == t && $4 == k' | wc -l | tr -d ' ')
  [ "$tw_claims" = 1 ]
}

RACE_TASKS=
for k in 1 2 3 4 5; do
  rid=$("$BOARD" add "Race target $k") || FAILS=$((FAILS + 1))
  RACE_TASKS="$RACE_TASKS $rid"
  check "race $k: exactly one of three distinct claimers wins, one claim event" race_task "$rid"
done
TWIN_T=$("$BOARD" add "Same-name race") || FAILS=$((FAILS + 1))
check "two claimers with the same name: both succeed, one claim event (documented)" twin_claim "$TWIN_T"

FIRST_OUT=$(BOARD_AUTHOR=maint "$BOARD" claim "$T_PLAIN") ; FIRST_RC=$?
expect_eq "the first claim on a plain task wins" "claimed $T_PLAIN" "$FIRST_OUT"
check "a winning claim exits 0" test "$FIRST_RC" = 0
LOSE_OUT=$(BOARD_AUTHOR=racer9 "$BOARD" claim "$T_PLAIN") ; LOSE_RC=$?
check "a claim on a task another author holds exits 2" test "$LOSE_RC" = 2
expect_eq "the refused claim says who holds the task" \
  "not claimed: $T_PLAIN is claimed, owner maint" "$LOSE_OUT"
OWN_OUT=$("$BOARD" claim "$T_PLAIN") ; OWN_RC=$?
expect_eq "claiming your own task again is idempotent" "already yours: $T_PLAIN" "$OWN_OUT"
check "idempotent claim exits 0" test "$OWN_RC" = 0

# --- notes, multi-line rejection, missing task ----------------------------------------------
"$BOARD" note "$T_PLAIN" "first note; it's \"fine\" é" >/dev/null || FAILS=$((FAILS + 1))
check "a note with quotes and unicode is accepted" test -n "$("$BOARD" show "$T_PLAIN" | grep 'first note')"
MULTI="line one${NL}line two"
expect_rc "a multi-line note is refused" 1 "$BOARD" note "$T_PLAIN" "$MULTI"
expect_rc "a note with a control character is refused" 1 "$BOARD" note "$T_PLAIN" "$(printf 'x\001y')"
expect_rc "a note on a missing task fails" 1 "$BOARD" note t0000000000 "x"

# --- concurrent note storm: sixteen writers, five notes each, all must land -----------------
STORM_DIR="$WORK/storm"
mkdir -p "$STORM_DIR"
storm_pids=
w=1
while [ "$w" -le 16 ]; do
  (j=1
   while [ "$j" -le 5 ]; do
     BOARD_AUTHOR=writer$w "$BOARD" note "$T_BIG" "writer $w note $j" >/dev/null 2>>"$STORM_DIR/err" \
       || echo "failed $w $j" >>"$STORM_DIR/failed"
     j=$((j + 1))
   done) &
  storm_pids="$storm_pids $!"
  w=$((w + 1))
done
# shellcheck disable=SC2086
wait $storm_pids
STORM_FAILED=$(cat "$STORM_DIR/failed" 2>/dev/null | wc -l | tr -d ' ')
expect_eq "80 concurrent notes from sixteen writers all land" "0" "$STORM_FAILED"
STORM_EVENTS=$("$BOARD" since 0 | awk -F"$TAB" -v t="$T_BIG" '$2 == t' | wc -l | tr -d ' ')
expect_eq "the task holds all 80 notes plus its add event" "81" "$STORM_EVENTS"
ORDER_BAD=$("$BOARD" since 0 | awk -F"$TAB" -v t="$T_BIG" '
  $2 == t { if (seen && $1 + 0 <= prev) bad++; prev = $1 + 0; seen = 1 }
  END { print bad + 0 }')
expect_eq "event ids for the task rise in order" "0" "$ORDER_BAD"

# --- forced 40001: a write that read the counter before another writer committed ------------
# BOARD_TEST_HOLD_FILE makes the held note stop after its counter bump and wait for a file.
# The test runs another note in that window, so the held note's commit must conflict once.
HOLD="$WORK/hold"
rm -f "$HOLD" "$HOLD.started"
BOARD_TEST_MODE=1 BOARD_TEST_HOLD_FILE="$HOLD" BOARD_AUTHOR=held "$BOARD" note "$T_BIG" "held note" \
  >"$WORK/held.out" 2>"$WORK/held.err" &
HELD_PID=$!
i=0
while [ ! -f "$HOLD.started" ] && [ "$i" -lt 200 ]; do sleep 0.05; i=$((i + 1)); done
BOARD_AUTHOR=other "$BOARD" note "$T_BIG" "note while held" >/dev/null 2>&1 || FAILS=$((FAILS + 1))
: >"$HOLD"
wait "$HELD_PID"
HELD_RC=$?
expect_eq "the held note lands after its conflict" "0" "$HELD_RC"
check "the held note reported a retry on stderr" grep -q "retrying after a write conflict" "$WORK/held.err"
check "the held note is in the log" test -n "$("$BOARD" show "$T_BIG" | grep 'held note')"

# --- done, decide, closed tasks -------------------------------------------------------------
"$BOARD" "done" "$T_CHILD" "closing by hand" >/dev/null || FAILS=$((FAILS + 1))
check "done moves the task to done" test "$("$BOARD" list "done" | cut -f1 | grep -c "$T_CHILD")" = 1
expect_rc "a second done on the same task is a no-op (exit 0)" 0 "$BOARD" "done" "$T_CHILD" "again"
check "the second done wrote no event" \
  test "$("$BOARD" since 0 | awk -F"$TAB" -v t="$T_CHILD" -v k=done '$2 == t && $4 == k' | wc -l | tr -d ' ')" = 1
"$BOARD" claim "$T_CHILD" >/dev/null 2>&1 ; DONE_RC=$?
check "a done task cannot be claimed (exit 2)" test "$DONE_RC" = 2
"$BOARD" decide "$T_BIG" "Use the TEXT id scheme; it's ok" >/dev/null || FAILS=$((FAILS + 1))
check "decide shows up in show" test -n "$("$BOARD" show "$T_BIG" | grep 'Use the TEXT id scheme')"

# --- polling cursor, as a maintainer would use it ------------------------------------------
CURSOR=$("$BOARD" head)
BOARD_AUTHOR=subagent-7 "$BOARD" note "$T_PLAIN" "subagent progress" >/dev/null || FAILS=$((FAILS + 1))
NEW=$("$BOARD" since "$CURSOR")
expect_eq "since returns only the event after the cursor" "1" "$(printf '%s\n' "$NEW" | wc -l | tr -d ' ')"
check "the new event carries the sub-agent author" \
  test "$(printf '%s\n' "$NEW" | cut -f3)" = subagent-7
EMPTY=$("$BOARD" since "$("$BOARD" head)")
expect_eq "since returns nothing when there is nothing new" "" "$EMPTY"

# --- watch: one task's events after a cursor, read only ------------------------------------
W_A=$("$BOARD" add "Watch target A") || FAILS=$((FAILS + 1))
W_B=$("$BOARD" add "Watch target B") || FAILS=$((FAILS + 1))
BOARD_AUTHOR=watcher "$BOARD" note "$W_A" "a first" >/dev/null || FAILS=$((FAILS + 1))
BOARD_AUTHOR=watcher "$BOARD" note "$W_B" "b first" >/dev/null || FAILS=$((FAILS + 1))
BOARD_AUTHOR=watcher "$BOARD" note "$W_A" "a second" >/dev/null || FAILS=$((FAILS + 1))
WATCH_ALL=$("$BOARD" watch "$W_A")
expect_eq "watch prints one task's events from the start, oldest first" "add note note" \
  "$(printf '%s\n' "$WATCH_ALL" | cut -f4 | paste -sd' ' -)"
check "watch rows have the since format and belong to this task" \
  test "$(printf '%s\n' "$WATCH_ALL" | awk -F"$TAB" -v t="$W_A" 'NF != 5 || $2 != t' | wc -l | tr -d ' ')" = 0
expect_eq "watch leaves out the other task's notes" "0" \
  "$(printf '%s\n' "$WATCH_ALL" | grep -c 'b first')"
FIRST_W=$(printf '%s\n' "$WATCH_ALL" | head -n 1 | cut -f1)
expect_eq "watch --after the first event prints only the notes after it" "note note" \
  "$("$BOARD" watch "$W_A" --after "$FIRST_W" | cut -f4 | paste -sd' ' -)"
expect_eq "watch --after the newest event prints nothing" "" \
  "$("$BOARD" watch "$W_A" --after "$("$BOARD" head)")"
EVENTS_BEFORE=$("$BOARD" since 0 | wc -l | tr -d ' ')
expect_rc "watch needs no BOARD_AUTHOR" 0 env -u BOARD_AUTHOR "$BOARD" watch "$W_A"
check "watch writes no event" test "$("$BOARD" since 0 | wc -l | tr -d ' ')" = "$EVENTS_BEFORE"
expect_rc "watch of a missing task fails" 1 "$BOARD" watch t0000000000
expect_rc "watch with a bad --after fails" 1 "$BOARD" watch "$W_A" --after x
expect_rc "watch with no task id fails" 1 "$BOARD" watch

# --- since --task, list --owner, list hides done tasks ---------------------------------------
expect_eq "since --task keeps only that task's events" "3" \
  "$("$BOARD" since 0 --task "$W_A" | wc -l | tr -d ' ')"
check "since --task lists no other task" \
  test "$("$BOARD" since 0 --task "$W_A" | awk -F"$TAB" -v t="$W_A" '$2 != t' | wc -l | tr -d ' ')" = 0
expect_eq "since --task takes the event id too, in either order" "2" \
  "$("$BOARD" since --task "$W_A" "$FIRST_W" | wc -l | tr -d ' ')"
expect_rc "since --task with a bad id fails" 1 "$BOARD" since --task 'bad id'
expect_rc "since --task with no id fails" 1 "$BOARD" since --task

BOARD_AUTHOR=lister1$RUN "$BOARD" claim "$W_B" >/dev/null || FAILS=$((FAILS + 1))
expect_eq "list --owner shows only that owner's tasks" "$W_B" \
  "$("$BOARD" list --owner lister1$RUN | tail -n +2 | cut -f1)"
expect_rc "list --owner with a bad name fails" 1 "$BOARD" list --owner 'bad name'
expect_rc "list --owner with no name fails" 1 "$BOARD" list --owner
expect_rc "list rejects an unknown option" 1 "$BOARD" list --bogus
check "list hides done tasks by default" \
  test "$("$BOARD" list | cut -f1 | grep -c "^$T_CHILD\$")" = 0
check "list --all shows done tasks" \
  test "$("$BOARD" list --all | cut -f1 | grep -c "^$T_CHILD\$")" = 1
check "list done still shows done tasks" \
  test "$("$BOARD" list "done" | cut -f1 | grep -c "^$T_CHILD\$")" = 1

ORD_IDS=
k=1
while [ "$k" -le 8 ]; do
  oid=$("$BOARD" add "Order $k" --priority 7) || FAILS=$((FAILS + 1))
  ORD_IDS="$ORD_IDS$oid$NL"
  k=$((k + 1))
done
expect_eq "equal priorities list oldest first" "$(printf '%s' "$ORD_IDS")" \
  "$("$BOARD" list open | awk -F"$TAB" '$3 == 7 { print $1 }')"

# --- the note and title limit: 4096 bytes ----------------------------------------------------
LONG=$(awk 'BEGIN { for (i = 0; i < 4097; i++) printf "x" }')
expect_rc "a note over 4096 bytes is refused" 1 "$BOARD" note "$W_A" "$LONG"
"$BOARD" note "$W_A" "$LONG" >/dev/null 2>"$WORK/long.err"
check "the refusal names the 4096 byte limit" grep -q "4096 bytes" "$WORK/long.err"
expect_eq "the refused note wrote no event" "3" "$("$BOARD" watch "$W_A" | wc -l | tr -d ' ')"
EXACT=$(awk 'BEGIN { for (i = 0; i < 4096; i++) printf "y" }')
expect_rc "a note of exactly 4096 bytes is accepted" 0 "$BOARD" note "$W_A" "$EXACT"
ACCENT=$(awk 'BEGIN { for (i = 0; i < 2049; i++) printf "é" }')
expect_rc "a note of 4098 bytes of accented text is refused (the limit counts bytes)" 1 \
  "$BOARD" note "$W_A" "$ACCENT"
expect_rc "a title over 4096 bytes is refused" 1 "$BOARD" add "$LONG"

# --- head prints one event id; help lists watch ----------------------------------------------
HEAD_OUT=$("$BOARD" head)
head_is_one_event_id() {
  case $HEAD_OUT in
    '' | *[!0-9]*) return 1 ;;
  esac
  [ "$(printf '%s\n' "$HEAD_OUT" | wc -l | tr -d ' ')" = 1 ]
}
check "head prints one event id and no feed cursor" head_is_one_event_id
help_lists_watch() {
  "$BOARD" help | grep -q '^  watch ID'
}
check "help lists watch" help_lists_watch

if [ -z "$EXISTING" ]; then
# --- passwords stay out of argv ------------------------------------------------------------
# A spy stands in for psql (BOARD_PSQL). It records its arguments and PGPASSWORD, then runs psql.
SPY="$WORK/psql-spy"
{
  printf '%s\n' '#!/bin/sh'
  printf '%s\n' "printf '%s\\n' \"\$@\" >'$WORK/spy.args'"
  printf '%s\n' "printf 'PGPASSWORD=%s\\n' \"\${PGPASSWORD:-}\" >'$WORK/spy.env'"
  printf '%s\n' 'exec psql "$@"'
} >"$SPY"
chmod 700 "$SPY"
expect_rc "a URL with a password still works" 0 env \
  LATTICE_BOARD_URL="postgresql://dev:s3cret-pw@127.0.0.1:$PORT/lattice" BOARD_PSQL="$SPY" "$BOARD" list
no_password_in_args() {
  ! grep -q s3cret "$WORK/spy.args"
}
check "the password is not in psql's arguments" no_password_in_args
check "the password reaches psql through PGPASSWORD" grep -q '^PGPASSWORD=s3cret-pw$' "$WORK/spy.env"
check "the URL psql gets has no password" grep -q "^postgresql://dev@127.0.0.1:$PORT/lattice\$" "$WORK/spy.args"
expect_rc "a percent-encoded password in the URL is refused" 1 env \
  LATTICE_BOARD_URL="postgresql://dev:p%41ss@127.0.0.1:$PORT/lattice" "$BOARD" list
expect_rc "a password in a key=value connection string is refused" 1 env \
  LATTICE_BOARD_URL="host=127.0.0.1 port=$PORT user=dev password=x" "$BOARD" list
expect_rc "a password query parameter in the URL is refused" 1 env \
  LATTICE_BOARD_URL="postgresql://dev@127.0.0.1:$PORT/lattice?password=x" "$BOARD" list
fi

# --- orchestration: leases, fences, retries, dependencies, next, approval -------------------
# These need the database clock: extract(epoch FROM now()) and CAST (docs/sql-subset.md; the
# executor implements both). The lease lengths below are seconds of real time, so this part sleeps.
SQL_RAW() {
  # A failed connect is retried, as the helper does (alpha.1 drops some new connections).
  sr_try=0
  while :; do
    sr_try=$((sr_try + 1))
    sr_out=$(psql -X -w -q -A -t -v ON_ERROR_STOP=1 -c "$1" 2>"$WORK/sqlraw.err") && { printf '%s\n' "$sr_out"; return 0; }
    if [ "$sr_try" -lt 20 ] && grep -q 'psql: error: connection to server' "$WORK/sqlraw.err" \
      && ! grep -q FATAL "$WORK/sqlraw.err"; then
      sleep 0.2
      continue
    fi
    cat "$WORK/sqlraw.err" >&2
    return 1
  done
}
row_state() {
  "$BOARD" list --all | awk -F"$TAB" -v t="$1" '$1 == t { print $2 }'
}
event_count() {
  # event_count TASK KIND
  "$BOARD" since 0 | awk -F"$TAB" -v t="$1" -v k="$2" '$2 == t && $4 == k' | wc -l | tr -d ' '
}
author_rc() {
  # author_rc NAME COMMAND...: the exit status of a board command run as NAME
  ar_name=$1
  shift
  BOARD_AUTHOR=$ar_name "$BOARD" "$@" >/dev/null 2>&1
  echo $?
}

# Open tasks that existed before this run would disturb `next`: cancel them (nothing is deleted).
PRE_OPEN=$("$BOARD" list open | tail -n +2 | cut -f1 | grep -v -E "^($T_PLAIN|$T_BIG|$T_CHILD|$T_EVIL)\$")
for tid in $PRE_OPEN; do
  "$BOARD" cancel "$tid" "test.sh clears the queue" >/dev/null 2>&1
done
check "migrate on a current board adds nothing" test "$("$BOARD" migrate)" = "board migrated: 0 lease rows added"
OLD_T=t0ld$$
SQL_RAW "INSERT INTO tasks (id, title, state, owner, priority, parent_id, created_by, body, created_at, updated_at, created_seq)
  VALUES ('$OLD_T', 'made by an older helper', 'open', NULL, 0, NULL, 'maint', '', 1, 1, 1);" >/dev/null \
  || FAILS=$((FAILS + 1))
check "show works for a task with no lease row" "$BOARD" show "$OLD_T"
check "migrate adds the missing lease row" test "$("$BOARD" migrate)" = "board migrated: 1 lease rows added"
check "the migrated task has a lease row" test -n "$(SQL_RAW "SELECT task_id FROM task_leases WHERE task_id = '$OLD_T'")"
"$BOARD" cancel "$OLD_T" "test fixture" >/dev/null || FAILS=$((FAILS + 1))

# lease, heartbeat, expiry, fence
L1=$("$BOARD" add "Lease task" --priority 3) || FAILS=$((FAILS + 1))
check "a new task has a lease row at once" test -n "$(SQL_RAW "SELECT task_id FROM task_leases WHERE task_id = '$L1'")"
expect_rc "claim with a bad lease is refused" 1 env BOARD_AUTHOR=zed$RUN "$BOARD" claim "$L1" --lease 0
expect_rc "zed claims with a 3 second lease" 0 env BOARD_AUTHOR=zed$RUN "$BOARD" claim "$L1" --lease 3
check "the claim stored a fence file" test -f "$BOARD_STATE_DIR/zed$RUN.$L1"
FENCE_Z=$(cat "$BOARD_STATE_DIR/zed$RUN.$L1")
expect_eq "the fence is the claim event id" "$("$BOARD" since 0 | awk -F"$TAB" -v t="$L1" '$2 == t && $4 == "claim" { print $1 }')" "$FENCE_Z"
LEASE_UNTIL=$(SQL_RAW "SELECT lease_until FROM task_leases WHERE task_id = '$L1'")
DB_NOW=$(SQL_RAW "SELECT CAST(extract(epoch FROM now()) AS bigint)")
check "lease_until is on the database clock, about 3 seconds ahead" \
  test "$LEASE_UNTIL" -ge "$DB_NOW" -a "$LEASE_UNTIL" -le "$((DB_NOW + 4))"
expect_rc "the holder's heartbeat extends the lease" 0 env BOARD_AUTHOR=zed$RUN "$BOARD" heartbeat "$L1" --lease 3
expect_rc "another author's heartbeat is refused with 3" 3 env BOARD_AUTHOR=yan$RUN "$BOARD" heartbeat "$L1"
expect_rc "a wrong --fence is refused with 3" 3 env BOARD_AUTHOR=zed$RUN "$BOARD" heartbeat "$L1" --fence 1
expect_rc "reap before the lease runs out expires nothing" 0 env BOARD_AUTHOR=reaper$RUN "$BOARD" reap
expect_eq "the task is still claimed" "claimed" "$(row_state "$L1")"
sleep 5
REAP_OUT=$(BOARD_AUTHOR=reaper$RUN "$BOARD" reap)
check "reap expires the lease" test "$(printf '%s\n' "$REAP_OUT" | grep -c "^expired $L1: open again\$")" = 1
expect_eq "the expired task is open again" "open" "$(row_state "$L1")"
expect_eq "reap wrote one expired event" "1" "$(event_count "$L1" expired)"
expect_eq "a second reap does not touch the task again" "0" "$(BOARD_AUTHOR=reaper$RUN "$BOARD" reap | grep -c "$L1")"
expect_eq "and wrote no second event" "1" "$(event_count "$L1" expired)"
check "the expiry counted an attempt" test "$(SQL_RAW "SELECT attempts FROM task_leases WHERE task_id = '$L1'")" = 1
check "the retry delay is on the database clock" \
  test "$(SQL_RAW "SELECT not_before > CAST(extract(epoch FROM now()) AS bigint) FROM task_leases WHERE task_id = '$L1'")" = t
expect_rc "a claim inside the retry delay is refused with 2" 2 env BOARD_AUTHOR=yan$RUN "$BOARD" claim "$L1"
# the zombie
BEFORE_EVENTS=$("$BOARD" head)
expect_rc "the zombie's heartbeat is refused with 3" 3 env BOARD_AUTHOR=zed$RUN "$BOARD" heartbeat "$L1"
check "the fence file of a lost lease is removed" test ! -f "$BOARD_STATE_DIR/zed$RUN.$L1"
mkdir -p "$BOARD_STATE_DIR"
printf '%s\n' "$FENCE_Z" >"$BOARD_STATE_DIR/zed$RUN.$L1"
expect_rc "the zombie's done is refused with 3" 3 env BOARD_AUTHOR=zed$RUN "$BOARD" "done" "$L1" "late"
printf '%s\n' "$FENCE_Z" >"$BOARD_STATE_DIR/zed$RUN.$L1"
expect_rc "the zombie's note is refused with 3" 3 env BOARD_AUTHOR=zed$RUN "$BOARD" note "$L1" "late note"
expect_eq "the zombie changed nothing: still open" "open" "$(row_state "$L1")"
expect_eq "the refusals are recorded as fenced events" "2" "$(event_count "$L1" fenced)"
expect_eq "no done or note event was written by the zombie" "0" \
  "$("$BOARD" since "$BEFORE_EVENTS" --task "$L1" | awk -F"$TAB" '$4 == "done" || $4 == "note"' | wc -l | tr -d ' ')"
# a new holder gets a higher fence, and the old fence stays refused even for the same name
SQL_RAW "UPDATE task_leases SET not_before = 0 WHERE task_id = '$L1';" >/dev/null || FAILS=$((FAILS + 1))
expect_rc "yan claims after the delay" 0 env BOARD_AUTHOR=yan$RUN "$BOARD" claim "$L1" --lease 60
FENCE_Y=$(cat "$BOARD_STATE_DIR/yan$RUN.$L1")
check "the new fence is higher" test "$FENCE_Y" -gt "$FENCE_Z"
printf '%s\n' "$FENCE_Z" >"$BOARD_STATE_DIR/zed$RUN.$L1"
expect_rc "the old holder is still refused" 3 env BOARD_AUTHOR=zed$RUN "$BOARD" "done" "$L1"
expect_rc "the holder's note lands" 0 env BOARD_AUTHOR=yan$RUN "$BOARD" note "$L1" "working"
expect_rc "the holder's done lands" 0 env BOARD_AUTHOR=yan$RUN "$BOARD" "done" "$L1" "finished"
expect_rc "done twice is a no-op with exit 0" 0 env BOARD_AUTHOR=yan$RUN "$BOARD" "done" "$L1" "again"
expect_eq "and wrote no second done event" "1" "$(event_count "$L1" "done")"
expect_rc "done by someone with a stale fence on a done task is also a no-op" 0 env BOARD_AUTHOR=zed$RUN "$BOARD" "done" "$L1"
expect_rc "a heartbeat on a done task is refused with 3" 3 env BOARD_AUTHOR=yan$RUN "$BOARD" heartbeat "$L1"

# retry, backoff and dead letter
D1=$("$BOARD" add "Doomed" --max-attempts 2) || FAILS=$((FAILS + 1))
expect_rc "first claim" 0 env BOARD_AUTHOR=dan$RUN "$BOARD" claim "$D1"
expect_rc "first fail reopens the task" 0 env BOARD_AUTHOR=dan$RUN "$BOARD" fail "$D1" "no luck" --retry-in 0
expect_eq "it is open again" "open" "$(row_state "$D1")"
check "one attempt used" test "$(SQL_RAW "SELECT attempts FROM task_leases WHERE task_id = '$D1'")" = 1
check "the error text is kept" test "$(SQL_RAW "SELECT last_error FROM task_leases WHERE task_id = '$D1'")" = "no luck"
expect_rc "second claim" 0 env BOARD_AUTHOR=dan$RUN "$BOARD" claim "$D1"
FAIL_OUT=$(BOARD_AUTHOR=dan$RUN "$BOARD" fail "$D1" "still no luck" --retry-in 0)
check "the last attempt moves the task to dead" test "$(row_state "$D1")" = dead
case $FAIL_OUT in
  *dead*) PASS=$((PASS + 1)); echo "ok   - the message says dead" ;;
  *) FAILS=$((FAILS + 1)); echo "FAIL - the message says dead: $FAIL_OUT" ;;
esac
expect_rc "a dead task cannot be claimed" 2 env BOARD_AUTHOR=dan$RUN "$BOARD" claim "$D1"
expect_rc "a dead task cannot be marked done" 1 env BOARD_AUTHOR=dan$RUN "$BOARD" "done" "$D1"
check "list dead shows it" test "$("$BOARD" list dead | cut -f1 | grep -c "^$D1\$")" = 1
check "show prints the lease with the last error" test -n "$("$BOARD" show "$D1" | grep 'still no luck')"
R1=$("$BOARD" add "Released") || FAILS=$((FAILS + 1))
BOARD_AUTHOR=rae$RUN "$BOARD" claim "$R1" >/dev/null || FAILS=$((FAILS + 1))
expect_rc "release gives the task back" 0 env BOARD_AUTHOR=rae$RUN "$BOARD" release "$R1"
check "release does not count an attempt" test "$(SQL_RAW "SELECT attempts FROM task_leases WHERE task_id = '$R1'")" = 0
expect_eq "and the task is open" "open" "$(row_state "$R1")"

# dependencies
DEP_A=$("$BOARD" add "Dependency A" --priority 9) || FAILS=$((FAILS + 1))
DEP_B=$("$BOARD" add "Dependency B" --priority 9 --blocked-by "$DEP_A") || FAILS=$((FAILS + 1))
expect_rc "a blocked task is not claimed" 2 env BOARD_AUTHOR=bea$RUN "$BOARD" claim "$DEP_B"
expect_rc "blocking on a missing task is refused" 1 "$BOARD" block "$DEP_A" t0000000000
expect_rc "a cycle is refused" 1 "$BOARD" block "$DEP_A" "$DEP_B"
expect_rc "waiting for yourself is refused" 1 "$BOARD" block "$DEP_A" "$DEP_A"
DEP_C=$("$BOARD" add "Dependency C" --priority 9) || FAILS=$((FAILS + 1))
expect_rc "block adds a blocker" 0 "$BOARD" block "$DEP_C" "$DEP_B"
expect_rc "a longer cycle is refused" 1 "$BOARD" block "$DEP_A" "$DEP_C"
expect_eq "next skips the blocked tasks" "$DEP_A" "$(BOARD_AUTHOR=bea$RUN "$BOARD" next)"
expect_rc "bea is at capacity: next exits 4" 4 env BOARD_AUTHOR=bea$RUN "$BOARD" next
BOARD_AUTHOR=bea$RUN "$BOARD" "done" "$DEP_A" "ok" >/dev/null || FAILS=$((FAILS + 1))
expect_eq "once A is done, next takes B" "$DEP_B" "$(BOARD_AUTHOR=bea$RUN "$BOARD" next)"
expect_rc "unblock removes a blocker" 0 "$BOARD" unblock "$DEP_C" "$DEP_B"
expect_eq "C is runnable without B" "$DEP_C" "$(BOARD_AUTHOR=cy$RUN "$BOARD" next)"
BOARD_AUTHOR=bea$RUN "$BOARD" "done" "$DEP_B" >/dev/null; BOARD_AUTHOR=cy$RUN "$BOARD" "done" "$DEP_C" >/dev/null

# next: priority order, the cap, nothing to do
for tid in $("$BOARD" list open | tail -n +2 | cut -f1); do "$BOARD" cancel "$tid" "clear the queue" >/dev/null; done
N_LO=$("$BOARD" add "Next low" --priority 1) || FAILS=$((FAILS + 1))
N_HI=$("$BOARD" add "Next high" --priority 8) || FAILS=$((FAILS + 1))
N_HI2=$("$BOARD" add "Next high, newer" --priority 8) || FAILS=$((FAILS + 1))
expect_eq "next takes the highest priority first" "$N_HI" "$(BOARD_AUTHOR=nell$RUN "$BOARD" next)"
expect_eq "with --max-active 3 it takes the next oldest" "$N_HI2" "$(BOARD_AUTHOR=nell$RUN "$BOARD" next --max-active 3)"
expect_eq "then the lowest" "$N_LO" "$(BOARD_AUTHOR=nell$RUN "$BOARD" next --max-active 3)"
expect_rc "and then nothing is runnable (4)" 4 env BOARD_AUTHOR=nell$RUN "$BOARD" next --max-active 9
expect_eq "next --owner names the claimer" "nell$RUN" "$("$BOARD" list claimed | awk -F"$TAB" -v t="$N_LO" '$1 == t { print $4 }')"

# many agents at once: each gets a different task
for k in 1 2 3 4 5 6; do "$BOARD" add "Parallel $k" --priority 5 >/dev/null || FAILS=$((FAILS + 1)); done
NEXT_DIR="$WORK/nextrace"
mkdir -p "$NEXT_DIR"
next_pids=
k=1
while [ "$k" -le 6 ]; do
  (BOARD_AUTHOR=racer-n$RUN-$k "$BOARD" next >"$NEXT_DIR/out.$k" 2>/dev/null; echo $? >"$NEXT_DIR/rc.$k") &
  next_pids="$next_pids $!"
  k=$((k + 1))
done
# shellcheck disable=SC2086
wait $next_pids
check "six concurrent next calls all succeed" test "$(cat "$NEXT_DIR"/rc.* | grep -c '^0$')" = 6
check "and take six different tasks" test "$(cat "$NEXT_DIR"/out.* | sort -u | grep -c .)" = 6

# approval
AP=$("$BOARD" add "Needs a human" --needs-approval --priority 6) || FAILS=$((FAILS + 1))
expect_eq "it waits for approval" "awaiting_approval" "$(row_state "$AP")"
expect_rc "an unapproved task is not claimable" 2 env BOARD_AUTHOR=apu$RUN "$BOARD" claim "$AP"
expect_rc "next does not offer it" 4 env BOARD_AUTHOR=apu$RUN "$BOARD" next
expect_rc "approve opens it" 0 env BOARD_AUTHOR=boss$RUN "$BOARD" approve "$AP" "go ahead"
expect_eq "the approval is an event with its author" "boss$RUN" \
  "$("$BOARD" since 0 | awk -F"$TAB" -v t="$AP" '$2 == t && $4 == "approve" { print $3 }')"
expect_rc "approving again is a no-op" 0 env BOARD_AUTHOR=boss$RUN "$BOARD" approve "$AP"
expect_eq "now next offers it" "$AP" "$(BOARD_AUTHOR=apu$RUN "$BOARD" next)"

# reap makes a lost claim runnable again for next, and next reaps first
RP=$("$BOARD" add "Abandoned" --priority 99) || FAILS=$((FAILS + 1))
BOARD_AUTHOR=gone$RUN "$BOARD" claim "$RP" --lease 2 >/dev/null || FAILS=$((FAILS + 1))
sleep 4
expect_rc "next reaps first: the task is open but in its retry delay, so nothing is runnable" 4 env BOARD_AUTHOR=heir$RUN "$BOARD" next
check "the abandoned claim was expired by next" test "$(event_count "$RP" expired)" = 1
expect_eq "and the task is open again" "open" "$(row_state "$RP")"
SQL_RAW "UPDATE task_leases SET not_before = 0 WHERE task_id = '$RP';" >/dev/null || FAILS=$((FAILS + 1))
expect_eq "after the delay next takes it" "$RP" "$(BOARD_AUTHOR=heir$RUN "$BOARD" next)"

if [ -z "$EXISTING" ]; then
# --- durability: stop, restart on the same data directory, check --------------------------
BEFORE_LIST=$("$BOARD" list)
BEFORE_HEAD=$("$BOARD" head)
BEFORE_SINCE=$("$BOARD" since 0)
stop_server
expect_eq "latticed stops cleanly on SIGTERM" "0" "$?"
start_server || exit 1
expect_eq "list is the same after a restart" "$BEFORE_LIST" "$("$BOARD" list)"
expect_eq "the event log is the same after a restart" "$BEFORE_SINCE" "$("$BOARD" since 0)"
check "a claim still refuses a task won before the restart" \
  test "$(BOARD_AUTHOR=racer9 "$BOARD" claim "$T_PLAIN" >/dev/null 2>&1; echo $?)" = 2
"$BOARD" note "$T_PLAIN" "written after restart" >/dev/null || FAILS=$((FAILS + 1))
AFTER_HEAD=$("$BOARD" head)
check "the counter continues after a restart" test "$AFTER_HEAD" -gt "$BEFORE_HEAD"

stop_server
expect_eq "latticed stops cleanly after the second start" "0" "$?"
fi

echo
echo "passed: $PASS  failed: $FAILS"
[ "$FAILS" -eq 0 ]

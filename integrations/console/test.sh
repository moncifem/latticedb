#!/bin/sh
# End-to-end check of the console against a local latticed.
#
# Needs psql, python3 and a latticed binary: LATTICE_BIN_DIR/latticed, or latticed on PATH.
# It starts its own server on a free loopback port with a fresh data directory, seeds a board with
# integrations/board/board, starts the console against it, fetches /api/now, /api/feed, /api/task and
# /api/scorecard, and removes everything it created. It never touches another server.
#
#   LATTICE_BIN_DIR=/path/to/bin integrations/console/test.sh
#
# The unit tests (python3 -m unittest discover -s integrations/console/tests) need none of this.
# This script is not run by a CI job yet. Exit status 0 when every check passes, 1 otherwise.

set -u

HERE=$(cd "$(dirname "$0")" && pwd) || exit 1
BOARD="$HERE/../board/board"

if [ -n "${LATTICE_BIN_DIR:-}" ]; then
  LATTICED="$LATTICE_BIN_DIR/latticed"
else
  LATTICED=$(command -v latticed) || { echo "test: latticed not found; set LATTICE_BIN_DIR" >&2; exit 1; }
fi
[ -x "$LATTICED" ] || { echo "test: $LATTICED is not executable" >&2; exit 1; }
command -v psql >/dev/null 2>&1 || { echo "test: psql not found" >&2; exit 1; }
command -v python3 >/dev/null 2>&1 || { echo "test: python3 not found" >&2; exit 1; }

WORK=$(mktemp -d "${TMPDIR:-/tmp}/console-test.XXXXXX") || exit 1
chmod 700 "$WORK"
DATA="$WORK/data"
mkdir -m 700 "$DATA"
LPID=
CPID=

cleanup() {
  for p in "$CPID" "$LPID"; do
    if [ -n "$p" ]; then
      kill -TERM "$p" 2>/dev/null
      wait "$p" 2>/dev/null
    fi
  done
  rm -rf "$WORK"
}
trap cleanup EXIT HUP INT TERM

pick_port() {
  python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1]); s.close()'
}

PORT=$(pick_port) || exit 1
export PGHOST=127.0.0.1
export PGPORT="$PORT"
export PGUSER=dev
export PGPASSWORD=unused-under-trust
export LC_MESSAGES=C

"$LATTICED" --data-dir "$DATA" --development-policy allow-all --listen "127.0.0.1:$PORT" \
  --stdin-control off >"$WORK/latticed.log" 2>&1 &
LPID=$!
n=0
while [ "$n" -lt 150 ]; do
  if grep -F "latticed listening on 127.0.0.1:$PORT (" "$WORK/latticed.log" >/dev/null 2>&1 \
    && psql -X -q -A -t -c 'SELECT 1' >/dev/null 2>&1; then
    break
  fi
  if ! kill -0 "$LPID" 2>/dev/null; then
    echo "test: latticed exited early; log:" >&2
    cat "$WORK/latticed.log" >&2
    LPID=
    exit 1
  fi
  sleep 0.2
  n=$((n + 1))
done
[ "$n" -lt 150 ] || { echo "test: latticed did not start" >&2; exit 1; }

# Seed: one agent claims a task, adds a note and a decision, a second task stays open.
"$BOARD" init >/dev/null || { echo "test: board init failed" >&2; exit 1; }
BOARD_AUTHOR=console-e2e-maint
export BOARD_AUTHOR
T1=$("$BOARD" add "console e2e task" --body "seeded by test.sh") || exit 1
T2=$("$BOARD" add "console e2e open task" --parent "$T1") || exit 1
BOARD_AUTHOR=console-e2e-agent "$BOARD" claim "$T1" >/dev/null || exit 1
BOARD_AUTHOR=console-e2e-agent "$BOARD" note "$T1" "note with <b>markup</b> and a quote ' in it" >/dev/null || exit 1
BOARD_AUTHOR=console-e2e-agent "$BOARD" decide "$T1" "keep it simple" >/dev/null || exit 1

# The console, on a free port, reading through the same PG* variables the board used.
cd "$HERE" || exit 1
python3 -m console serve --port 0 >"$WORK/console.log" 2>&1 &
CPID=$!
n=0
CURL=
while [ "$n" -lt 100 ]; do
  CURL=$(sed -n 's#^console: serving \(http://127.0.0.1:[0-9]*/\) .*#\1#p' "$WORK/console.log")
  [ -n "$CURL" ] && break
  if ! kill -0 "$CPID" 2>/dev/null; then
    echo "test: console exited early; log:" >&2
    cat "$WORK/console.log" >&2
    CPID=
    exit 1
  fi
  sleep 0.1
  n=$((n + 1))
done
[ -n "$CURL" ] || { echo "test: console did not report its address" >&2; exit 1; }

python3 - "$CURL" "$T1" "$T2" <<'PY'
import json
import sys
import urllib.error
import urllib.request

base, t1, t2 = sys.argv[1].rstrip("/"), sys.argv[2], sys.argv[3]
fails = 0


def check(name, ok, detail=""):
    global fails
    print(("ok   - " if ok else "FAIL - ") + name + ("" if ok else "  " + str(detail)))
    if not ok:
        fails += 1


def get(path):
    with urllib.request.urlopen(base + path, timeout=20) as r:
        return r.status, dict(r.headers), json.loads(r.read().decode())


s, h, now = get("/api/now")
check("/api/now answers 200", s == 200)
check("response carries the CSP", "default-src 'none'" in h.get("Content-Security-Policy", ""))
agents = {a["name"]: a for a in now["agents"]}
check("the seeded agent appears", "console-e2e-agent" in agents, list(agents))
check("the maintainer appears", "console-e2e-maint" in agents, list(agents))
check("the claimed task is the agent's current task",
      agents.get("console-e2e-agent", {}).get("current_task", {}) is not None
      and (agents["console-e2e-agent"]["current_task"] or {}).get("id") == t1)
check("the agent is working", agents.get("console-e2e-agent", {}).get("state") == "working")
check("counts: one claimed, one open", now["counts"]["claimed"] == 1 and now["counts"]["open"] == 1, now["counts"])
check("not a demo", now["meta"]["demo"] is False)

s, _, feed = get("/api/feed")
kinds = [e["kind"] for e in feed["events"]]
check("the feed lists the seeded events newest first",
      kinds[:3] == ["decision", "note", "claim"] and feed["events"][0]["id"] > feed["events"][-1]["id"], kinds)
check("the note text arrives unchanged as data",
      any(e["body"] == "note with <b>markup</b> and a quote ' in it" for e in feed["events"]))
cursor = feed["cursor"]
s, _, again = get("/api/feed?after=%d" % cursor)
check("polling with the cursor returns nothing new", again["events"] == [] and again["cursor"] == cursor)

s, _, task = get("/api/task?id=" + t2)
check("a child task shows its parent", [p["id"] for p in task["parents"]] == [t1], task["parents"])
s, _, task = get("/api/task?id=" + t1)
check("the parent shows its child", [c["id"] for c in task["children"]] == [t2])
check("the task view has its decision", [d["body"] for d in task["decisions"]] == ["keep it simple"])
check("the task view has its events in order",
      [e["kind"] for e in task["events"]] == ["add", "claim", "note", "decision"], [e["kind"] for e in task["events"]])

s, _, score = get("/api/scorecard")
check("the scorecard answers and says 'not enough data' for two tasks",
      s == 200 and all(m["status"] == "insufficient" for m in score["overall"]["metrics"]))

try:
    urllib.request.urlopen(urllib.request.Request(base + "/api/now", method="POST"), timeout=10)
    check("POST is refused", False)
except urllib.error.HTTPError as e:
    check("POST is refused", e.code == 405, e.code)

sys.exit(1 if fails else 0)
PY
RC=$?
if grep -q 'PGPASSWORD\|unused-under-trust' "$WORK/console.log"; then
  echo "FAIL - the console log mentions the password"
  RC=1
fi
exit "$RC"

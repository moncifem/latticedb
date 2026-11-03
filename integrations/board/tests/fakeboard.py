#!/usr/bin/env python3
"""A stand-in for the `board` helper used by test_run.py. It has no database.

State lives in $FAKEBOARD_DIR: conf.json (written by the test), state.json (the queue and the
counters), calls.jsonl (one JSON line per call: argv, BOARD_AUTHOR, and the names of the PG*/
BOARD_* variables it was started with). Only the commands the runner uses are answered:
next, get, heartbeat, note, done, fail, release.

conf.json keys (all optional):
  tasks        [{"id", "title", "body", "attempts"}] handed out by `next`, in order
  next_rc      exit status for every `next` (overrides the queue), for example 1
  heartbeat    {"lost_after": N}   the Nth beat and later exit 3
               {"error_after": N}  the Nth beat and later exit 1
               {"delay": S}        each beat first sleeps S seconds
  done_rc, fail_rc, release_rc, note_rc   exit status of those commands (default 0)
"""

import fcntl
import json
import os
import sys
import time

base = os.environ["FAKEBOARD_DIR"]
argv = sys.argv[1:]


def load(name, default):
    try:
        with open(os.path.join(base, name), encoding="utf-8") as f:
            return json.load(f)
    except (OSError, ValueError):
        return default


conf = load("conf.json", {})
lock = open(os.path.join(base, "lock"), "a+")
fcntl.flock(lock, fcntl.LOCK_EX)
state = load("state.json", None)
if state is None:
    state = {"queue": list(conf.get("tasks", [])), "claimed": {}, "beats": {}, "fence": 100}


def save():
    with open(os.path.join(base, "state.json"), "w", encoding="utf-8") as f:
        json.dump(state, f)


with open(os.path.join(base, "calls.jsonl"), "a", encoding="utf-8") as f:
    f.write(json.dumps({
        "argv": argv,
        "author": os.environ.get("BOARD_AUTHOR", ""),
        "sensitive_env": sorted(k for k in os.environ if k.startswith(("PG", "BOARD_", "LATTICE_"))),
        "t": time.time(),
    }) + "\n")

cmd = argv[0] if argv else ""


def option(name, default=None):
    if name in argv:
        i = argv.index(name)
        if i + 1 < len(argv):
            return argv[i + 1]
    return default


if cmd == "next":
    rc = conf.get("next_rc")
    if rc is not None:
        sys.exit(rc)
    if not state["queue"]:
        sys.stderr.write("board: nothing runnable\n")
        sys.exit(4)
    task = state["queue"].pop(0)
    state["fence"] += 1
    state["claimed"][task["id"]] = {"task": task, "fence": state["fence"], "owner": option("--owner", "")}
    save()
    print(task["id"])
    sys.exit(0)

if cmd == "get":
    entry = state["claimed"].get(argv[1])
    if entry is None:
        sys.stderr.write("board: no such task: %s\n" % argv[1])
        sys.exit(1)
    t = entry["task"]
    print("\t".join([t["id"], "claimed", entry["owner"], str(entry["fence"]), str(t.get("attempts", 0)), "3",
                     t.get("title", "title"), t.get("body", "")]))
    sys.exit(0)

if cmd == "heartbeat":
    hb = conf.get("heartbeat", {})
    n = state["beats"].get(argv[1], 0) + 1
    state["beats"][argv[1]] = n
    save()
    fcntl.flock(lock, fcntl.LOCK_UN)
    if hb.get("delay"):
        time.sleep(hb["delay"])
    if "lost_after" in hb and n >= hb["lost_after"]:
        sys.stderr.write("board: lease lost\n")
        sys.exit(3)
    if "error_after" in hb and n >= hb["error_after"]:
        sys.stderr.write("board: database down\n")
        sys.exit(1)
    print("lease extended")
    sys.exit(0)

if cmd == "activity":
    sys.exit(conf.get("activity_rc", 0))

if cmd in ("note", "done", "fail", "release"):
    rc = conf.get(cmd + "_rc", 0)
    if rc == 0 and cmd != "note":
        state["claimed"].pop(argv[1], None)
        save()
    sys.exit(rc)

sys.stderr.write("fakeboard: unexpected command %r\n" % cmd)
sys.exit(1)

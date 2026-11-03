#!/usr/bin/env python3
"""A stand-in for the `board` helper that knows the bench commands and the few task commands a worker and
`board-bench start` use. It has no database: the state is $FAKEBENCH_DIR/state.json under a lock, so
several workers can share it. It answers in the helper's own formats and exit codes (0 ok, 1 error,
3 lease lost, 4 nothing runnable) and applies the same rules the SQL does: bench-record only for the
holder of the claim, once; bench-plan only for a task that exists; names unique.

$FAKEBENCH_DIR/calls.jsonl gets one line per call (argv, BOARD_AUTHOR, the names of the PG*, BOARD_* and
TASK_* variables it was started with). conf.json (written by the test) may hold {"fail": {"bench-arm": 1}}
to make a command exit 1, and {"flaky_record": N} to make the first N bench-record calls fail.
"""

import fcntl
import json
import os
import sys
import time

base = os.environ["FAKEBENCH_DIR"]
argv = sys.argv[1:]
author = os.environ.get("BOARD_AUTHOR", "")


def load(name, default):
    try:
        with open(os.path.join(base, name), encoding="utf-8") as f:
            return json.load(f)
    except (OSError, ValueError):
        return default


conf = load("conf.json", {})
lock = open(os.path.join(base, "lock"), "a+")
fcntl.flock(lock, fcntl.LOCK_EX)
S = load("state.json", None) or {"tasks": {}, "order": [], "fence": 1000, "experiments": {}, "arms": [], "bench_tasks": [],
                                 "attempts": {}, "n": 0}
cmd = argv[0] if argv else ""
with open(os.path.join(base, "calls.jsonl"), "a", encoding="utf-8") as f:
    f.write(json.dumps({"argv": argv, "author": author, "t": time.time(),
                        "env": sorted(k for k in os.environ if k.startswith(("PG", "BOARD_", "TASK_", "LATTICE_")))}) + "\n")


def save():
    tmp = os.path.join(base, "state.json.tmp")
    with open(tmp, "w", encoding="utf-8") as f:
        json.dump(S, f)
    os.replace(tmp, os.path.join(base, "state.json"))


def die(msg, rc=1):
    sys.stderr.write("board: %s\n" % msg)
    sys.exit(rc)


def opts(allowed, args):
    pos, out, i = [], {}, 0
    while i < len(args):
        a = args[i]
        if a.startswith("--") and a[2:] in allowed:
            out[a[2:]] = args[i + 1] if allowed[a[2:]] else "1"
            i += 2 if allowed[a[2:]] else 1
        else:
            pos.append(a)
            i += 1
    return pos, out


if cmd in conf.get("fail", {}):
    die("forced failure of %s" % cmd, conf["fail"][cmd])

args = argv[1:]
if cmd == "add":
    pos, o = opts({"body": True, "needs-approval": False, "max-attempts": True, "priority": True}, args)
    S["n"] += 1
    tid = "tf%05d" % S["n"]
    S["tasks"][tid] = {"id": tid, "title": pos[0], "body": o.get("body", ""), "state": "awaiting_approval" if "needs-approval" in o else "open",
                       "owner": "", "fence": 0, "attempts": 0, "max": int(o.get("max-attempts", 3))}
    S["order"].append(tid)
    save()
    print(tid)
elif cmd in ("approve", "cancel"):
    t = S["tasks"].get(args[0])
    if t is None:
        die("no such task: %s" % args[0])
    if cmd == "approve":
        if t["state"] != "awaiting_approval":
            die("task is %s" % t["state"])
        t["state"] = "open"
    else:
        if t["state"] == "done":
            die("task is done")
        t["state"] = "cancelled"
    save()
elif cmd == "next":
    _, o = opts({"owner": True, "lease": True}, args)
    for tid in S["order"]:
        t = S["tasks"][tid]
        if t["state"] == "open":
            S["fence"] += 1
            t.update(state="claimed", owner=o.get("owner", author), fence=S["fence"])
            save()
            print(tid)
            sys.exit(0)
    die("nothing runnable", 4)
elif cmd == "get":
    t = S["tasks"].get(args[0]) or die("no such task: %s" % args[0])
    print("\t".join([t["id"], t["state"], t["owner"], str(t["fence"]), str(t["attempts"]), str(t["max"]), t["title"], t["body"]]))
elif cmd in ("heartbeat", "note", "done", "fail", "release"):
    pos, o = opts({"fence": True, "lease": True, "retry-in": True}, args)
    t = S["tasks"].get(pos[0]) or die("no such task: %s" % pos[0])
    held = t["state"] == "claimed" and t["owner"] == author and (("fence" not in o) or int(o["fence"]) == t["fence"])
    if not held:
        die("lease lost: %s is %s" % (pos[0], t["state"]), 3)
    if cmd == "done":
        t["state"] = "done"
    elif cmd in ("fail", "release"):
        if cmd == "fail":
            t["attempts"] += 1
        t["state"] = "dead" if t["attempts"] >= t["max"] else "open"
        t["owner"] = ""
    save()
elif cmd == "claim-for-test":  # test only: make AUTHOR the holder
    _, o = opts({"fence": True}, args[1:])
    S["fence"] += 1
    S["tasks"][args[0]].update(state="claimed", owner=author, fence=int(o.get("fence", S["fence"])))
    save()
elif cmd == "bench-experiment":
    pos, o = opts({"suite": True, "suite-sha": True, "seed": True, "repeats": True, "planned": True, "harness": True}, args)
    if pos[0] in S["experiments"]:
        die("already exists (the name is taken)")
    S["experiments"][pos[0]] = {"id": pos[0], "suite": o["suite"], "sha": o["suite-sha"], "seed": o["seed"],
                                "repeats": o["repeats"], "planned": o["planned"], "harness": o["harness"],
                                "by": author, "at": int(time.time())}
    save()
elif cmd == "bench-arm":
    pos, o = opts({"agent": True, "config-sha": True}, args)
    if pos[0] not in S["experiments"]:
        die("no such experiment: %s" % pos[0])
    S["arms"].append({"exp": pos[0], "arm": pos[1], "agent": o["agent"], "sha": o["config-sha"]})
    save()
elif cmd == "bench-task":
    pos, o = opts({"digest": True}, args)
    if pos[0] not in S["experiments"]:
        die("no such experiment: %s" % pos[0])
    S["bench_tasks"].append({"exp": pos[0], "task": pos[1], "digest": o["digest"]})
    save()
elif cmd == "bench-plan":
    pos, o = opts({"repeat": True, "order": True, "harness": True}, args)
    if pos[3] not in S["tasks"]:
        die("no such task: %s" % pos[3])
    if pos[3] in S["attempts"]:
        die("already exists (the name is taken)")
    S["attempts"][pos[3]] = {"exp": pos[0], "arm": pos[1], "task": pos[2], "repeat": int(o["repeat"]), "order": int(o["order"]),
                             "outcome": "planned", "flaky": 0, "tampered": 0, "duration_ms": "", "cost": "", "tokens": "",
                             "gexit": "", "runs": "", "summary": "", "harness": o["harness"], "recorded": 0}
    save()
elif cmd == "bench-record":
    if conf.get("flaky_record", 0) > S.get("record_calls", 0):
        S["record_calls"] = S.get("record_calls", 0) + 1
        save()
        die("temporary failure")
    pos, o = opts({"experiment": True, "arm": True, "task": True, "fence": True, "outcome": True, "flaky": True, "tampered": True, "duration-ms": True, "cost": True,
                   "tokens": True, "grader-exit": True, "grader-runs": True, "summary": True}, args)
    t = S["tasks"].get(pos[0])
    if "fence" not in o or "outcome" not in o:
        die("bench-record needs --fence and --outcome")
    if t is None or not (t["state"] == "claimed" and t["owner"] == author and t["fence"] == int(o["fence"])):
        die("lease lost: %s is %s" % (pos[0], t and t["state"]), 3)
    a = S["attempts"].get(pos[0]) or die("%s is not a bench attempt" % pos[0])
    if (o.get("experiment"), o.get("arm"), o.get("task")) != (a["exp"], a["arm"], a["task"]):
        die("the result names another attempt than %s was planned for; nothing written" % pos[0])
    if a["outcome"] == "planned":
        a.update(outcome=o["outcome"], flaky=int(o.get("flaky", 0)), tampered=int(o.get("tampered", 0)),
                 duration_ms=o.get("duration-ms", ""), cost=o.get("cost", ""), tokens=o.get("tokens", ""),
                 gexit=o.get("grader-exit", ""), runs=o.get("grader-runs", ""), summary=o.get("summary", ""), recorded=int(time.time()))
        save()
        print("recorded %s: %s" % (pos[0], o["outcome"]))
    else:
        print("already recorded %s: %s (nothing changed)" % (pos[0], a["outcome"]))
elif cmd == "bench-rows":
    e = S["experiments"].get(args[0]) or die("no such experiment: %s" % args[0])
    print("\t".join(["experiment", e["id"], e["suite"], e["sha"], e["seed"], e["repeats"], e["planned"], e["harness"], e["by"], str(e["at"])]))
    for a in sorted((x for x in S["arms"] if x["exp"] == args[0]), key=lambda x: x["arm"]):
        print("\t".join(["arm", a["arm"], a["agent"], a["sha"]]))
    for a in sorted((x for x in S["bench_tasks"] if x["exp"] == args[0]), key=lambda x: x["task"]):
        print("\t".join(["task", a["task"], a["digest"]]))
    for tid, a in sorted(S["attempts"].items(), key=lambda kv: kv[1]["order"]):
        if a["exp"] == args[0]:
            print("\t".join(["attempt", tid, a["arm"], a["task"], str(a["repeat"]), str(a["order"]), a["outcome"], str(a["flaky"]),
                             str(a["tampered"]), a["duration_ms"], a["cost"], a["tokens"], a["gexit"], a["runs"], a["harness"],
                             str(a["recorded"]), a["summary"]]))
elif cmd == "bench-list":
    for e in sorted(S["experiments"].values(), key=lambda x: -x["at"]):
        print("\t".join([e["id"], e["suite"], e["repeats"], e["planned"], str(e["at"])]))
else:
    die("unknown command %s" % cmd)

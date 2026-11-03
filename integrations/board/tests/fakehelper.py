#!/usr/bin/env python3
"""A stand-in for the `board` helper used by the board-mcp tests. It has no database.

It keeps a small board in $FAKEHELPER_DIR/state.json (under a lock) and answers the commands board-mcp
uses in the helper's own output formats and exit codes: 0 ok, 1 error, 2 claim not won, 3 lease lost,
4 nothing runnable. Fences are 880000 + n, deliberately unlike event ids, so a test can tell a leaked
fence from an event id (in the real board a claim's fence IS the claim event's id).

$FAKEHELPER_DIR/conf.json (written by the test) may hold:
  "delay":  {"note": 5}                      sleep that many seconds before the command runs
  "fail":   {"list": {"rc": 1, "err": "..."}} that command exits with rc after writing err to stderr
  "noise":  "text"                           written to stderr by every command (exit status unaffected)
$FAKEHELPER_DIR/calls.jsonl gets one line per call: argv, BOARD_AUTHOR, the environment names it saw.
"""

import fcntl
import json
import os
import sys
import time

base = os.environ["FAKEHELPER_DIR"]
argv = sys.argv[1:]
author = os.environ.get("BOARD_AUTHOR", "")


def load(name, default):
    try:
        with open(os.path.join(base, name), encoding="utf-8") as f:
            return json.load(f)
    except (OSError, ValueError):
        return default


conf = load("conf.json", {})
with open(os.path.join(base, "calls.jsonl"), "a", encoding="utf-8") as f:
    f.write(json.dumps({"argv": argv, "author": author, "env": sorted(os.environ), "t": time.time(),
                        "pid": os.getpid()}) + "\n")

cmd = argv[0] if argv else ""
if conf.get("noise"):
    sys.stderr.write(conf["noise"] + "\n")
if cmd in conf.get("delay", {}):
    time.sleep(conf["delay"][cmd])
if cmd in conf.get("fail", {}):
    spec = conf["fail"][cmd]
    sys.stderr.write(spec.get("err", "") + "\n")
    sys.exit(spec.get("rc", 1))

lock = open(os.path.join(base, "lock"), "a+")
fcntl.flock(lock, fcntl.LOCK_EX)
state = load("state.json", None)
if state is None:
    state = {"tasks": {}, "leases": {}, "events": [], "counter": 100, "fence": 880000, "clock": 1_000_000, "order": 0}


def save():
    tmp = os.path.join(base, "state.json.tmp")
    with open(tmp, "w", encoding="utf-8") as f:
        json.dump(state, f)
    os.replace(tmp, os.path.join(base, "state.json"))


def die(msg, rc=1):
    sys.stderr.write("board: %s\n" % msg)
    sys.exit(rc)


def event(task, kind, body=""):
    state["counter"] += 1
    state["events"].append({"id": state["counter"], "task": task, "author": author, "kind": kind, "body": body})
    return state["counter"]


def opts(args, names):
    """(options, positionals) the way the helper's parse_args reads them: options only before `--`."""
    out, pos, rest = {}, [], False
    i = 0
    while i < len(args):
        a = args[i]
        if not rest and a == "--":
            rest = True
        elif not rest and a.startswith("--") and a[2:] in names:
            if i + 1 >= len(args):
                die("%s needs a value" % a)
            out[a[2:]] = args[i + 1]
            i += 1
        else:
            pos.append(a)
        i += 1
    return out, pos


def lease_of(tid):
    return state["leases"].setdefault(tid, {"lease_until": 0, "heartbeat_at": 0, "attempts": 0, "max_attempts": 3,
                                            "not_before": 0, "last_error": "", "blocked_by": ""})


def blocked(tid):
    return [b for b in lease_of(tid)["blocked_by"].split(",") if b and state["tasks"].get(b, {}).get("state") != "done"]


def held(tid, fence):
    t = state["tasks"].get(tid)
    if not t:
        die("no such task: %s" % tid)
    ok = t["state"] == "claimed" and t["owner"] == author and (fence is None or t["claim_seq"] == fence)
    if not ok:
        if cmd != "heartbeat":  # the real helper records a refused write as an informational event
            event(tid, "fenced", "refused %s with fence %s: task is %s, owner %s, fence %s"
                  % (cmd, fence, t["state"], t["owner"] or "none", t["claim_seq"]))
            save()
        sys.stderr.write("board: lease lost: %s is %s, owner %s, fence %s (yours: %s)\n"
                         % (tid, t["state"], t["owner"] or "none", t["claim_seq"], fence))
        sys.exit(3)
    return t


def need_author():
    if not author:
        die("BOARD_AUTHOR is required for %s: set a unique name for this agent" % cmd)


def win(tid, lease):
    t = state["tasks"][tid]
    state["fence"] += 1
    event(tid, "claim")
    t.update(state="claimed", owner=author, claim_seq=state["fence"])
    ls = lease_of(tid)
    ls["lease_until"] = state["clock"] + lease
    ls["heartbeat_at"] = state["clock"]


if cmd == "add":
    need_author()
    o, pos = opts(argv[1:], ("priority", "parent", "body", "blocked-by", "max-attempts"))
    if len(pos) != 1:
        die("add needs a title")
    if o.get("parent") and o["parent"] not in state["tasks"]:
        die("parent task not found: %s" % o["parent"])
    state["order"] += 1
    tid = "tf%08d" % state["order"]
    state["tasks"][tid] = {"id": tid, "title": pos[0], "state": "open", "owner": "", "priority": int(o.get("priority", 0)),
                           "parent": o.get("parent", ""), "created_by": author, "body": o.get("body", ""), "claim_seq": 0,
                           "created_seq": state["order"], "created_at": state["clock"]}
    lease_of(tid)["blocked_by"] = o.get("blocked-by", "")
    event(tid, "add", pos[0])
    save()
    print(tid)
elif cmd == "list":
    o, pos = opts(argv[1:], ("owner",))
    want = pos[0] if pos and pos[0] != "--all" else ""
    print("id\tstate\tpriority\towner\ttitle")
    rows = sorted(state["tasks"].values(), key=lambda t: (-t["priority"], t["created_seq"]))
    for t in rows:
        if want and t["state"] != want:
            continue
        if not want and t["state"] == "done" and "--all" not in argv:
            continue
        if o.get("owner") and t["owner"] != o["owner"]:
            continue
        print("\t".join([t["id"], t["state"], str(t["priority"]), t["owner"], t["title"]]))
elif cmd == "show":
    t = state["tasks"].get(argv[1]) if len(argv) > 1 else None
    if not t:
        die("no such task: %s" % (argv[1] if len(argv) > 1 else ""))
    for k, v in (("id", t["id"]), ("title", t["title"]), ("state", t["state"]), ("owner", t["owner"]),
                 ("priority", t["priority"]), ("parent_id", t["parent"]), ("created_by", t["created_by"]),
                 ("body", t["body"]), ("created_at", t["created_at"]), ("updated_at", t["created_at"])):
        print("%s|%s" % (k, v))
    print("events:\nid\tauthor\tkind\tbody")
    for e in state["events"]:
        if e["task"] == t["id"]:
            print("%d\t%s\t%s\t%s" % (e["id"], e["author"], e["kind"], e["body"]))
    print("decisions:\nid\tauthor\tbody")
    ls = lease_of(t["id"])
    print("lease:")
    for k in ("lease_until", "heartbeat_at", "attempts", "max_attempts", "not_before", "last_error", "blocked_by"):
        print("%s|%s" % (k, ls[k]))
elif cmd == "get":
    t = state["tasks"].get(argv[1]) if len(argv) > 1 else None
    if not t:
        die("no such task: %s" % (argv[1] if len(argv) > 1 else ""))
    ls = lease_of(t["id"])
    print("\t".join([t["id"], t["state"], t["owner"], str(t["claim_seq"]), str(ls["attempts"]), str(ls["max_attempts"]),
                     t["title"], t["body"]]))
elif cmd == "queue":
    print("now\t%d" % state["clock"])
    live = [t for t in state["tasks"].values() if t["state"] not in ("done", "cancelled")]
    live.sort(key=lambda t: (-t["priority"], t["created_seq"]))
    for t in live:
        print("\t".join(["task", t["id"], t["state"], str(t["priority"]), t["owner"], str(t["created_at"]),
                         str(t["created_seq"]), t["title"]]))
    seen = set()
    for t in live:
        ls = lease_of(t["id"])
        print("\t".join(["lease", t["id"], str(ls["lease_until"]), str(ls["heartbeat_at"]), str(ls["attempts"]),
                         str(ls["max_attempts"]), str(ls["not_before"]), ls["last_error"], ls["blocked_by"]]))
        seen.update(b for b in ls["blocked_by"].split(",") if b)
    for b in sorted(seen):
        if b in state["tasks"]:
            print("blocker\t%s\t%s" % (b, state["tasks"][b]["state"]))
elif cmd == "head":
    print(state["events"][-1]["id"] if state["events"] else 0)
elif cmd == "since":
    o, pos = opts(argv[1:], ("task",))
    after = int(pos[0]) if pos else 0
    for e in state["events"]:
        if e["id"] > after and (not o.get("task") or e["task"] == o["task"]):
            print("\t".join([str(e["id"]), e["task"], e["author"], e["kind"], e["body"]]))
elif cmd == "claim":
    need_author()
    o, pos = opts(argv[1:], ("lease",))
    tid = pos[0] if pos else ""
    t = state["tasks"].get(tid)
    if not t:
        die("no such task: %s" % tid)
    if t["state"] == "claimed" and t["owner"] == author:
        print("already yours: %s" % tid)
    elif t["state"] != "open":
        print("not claimed: %s is %s, owner %s" % (tid, t["state"], t["owner"] or "none"))
        sys.exit(2)
    elif blocked(tid):
        print("not claimed: %s is blocked by %s" % (tid, ",".join(blocked(tid))))
        sys.exit(2)
    else:
        win(tid, int(o.get("lease", 600)))
        print("claimed %s" % tid)
    save()
elif cmd == "next":
    need_author()
    o, _ = opts(argv[1:], ("lease", "owner", "max-active"))
    if any(t["state"] == "claimed" and t["owner"] == author for t in state["tasks"].values()):
        sys.stderr.write("board: at capacity: %s already holds 1 task(s) (--max-active 1)\n" % author)
        sys.exit(4)
    for t in sorted(state["tasks"].values(), key=lambda t: (-t["priority"], t["created_seq"])):
        if t["state"] == "open" and not blocked(t["id"]) and lease_of(t["id"])["not_before"] <= state["clock"]:
            win(t["id"], int(o.get("lease", 600)))
            save()
            sys.stderr.write("board: claimed %s\n" % t["id"])
            print(t["id"])
            sys.exit(0)
    sys.stderr.write("board: nothing runnable\n")
    sys.exit(4)
elif cmd in ("heartbeat", "note", "done", "fail", "release"):
    need_author()
    o, pos = opts(argv[1:], ("fence", "lease", "retry-in"))
    fence = int(o["fence"]) if "fence" in o else None
    if "fence" not in o:
        die("BOARD_STRICT_FENCE is set and no fence is known for %s" % (pos[0] if pos else ""))
    tid = pos[0] if pos else ""
    if cmd == "done" and state["tasks"].get(tid, {}).get("state") == "done":
        print("already done: %s (no change)" % tid)
        sys.exit(0)
    t = held(tid, fence)
    ls = lease_of(tid)
    if cmd == "heartbeat":
        ls["lease_until"] = state["clock"] + int(o.get("lease", 600))
        save()
        print("lease on %s runs until %d (database clock)" % (tid, ls["lease_until"]))
    elif cmd == "note":
        e = event(tid, "note", pos[1])
        save()
        print("noted %s as event %d" % (tid, e))
    elif cmd == "done":
        t["state"] = "done"
        e = event(tid, "done", pos[1] if len(pos) > 1 else "")
        save()
        print("done %s as event %d" % (tid, e))
    elif cmd == "fail":
        ls["attempts"] += 1
        event(tid, "fail", pos[1] if len(pos) > 1 else "")
        t.update(owner="")
        if ls["attempts"] >= ls["max_attempts"]:
            t["state"] = "dead"
            print("failed %s: no attempts left, the task is dead" % tid)
        else:
            t["state"] = "open"
            delay = int(o.get("retry-in", 30))
            ls["not_before"] = state["clock"] + delay
            print("failed %s: open again in %d seconds" % (tid, delay))
        save()
    else:
        t.update(state="open", owner="")
        event(tid, "release")
        save()
        print("released %s" % tid)
else:
    die("unknown command %s" % cmd)

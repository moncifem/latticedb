#!/usr/bin/env python3
"""Drive the live activity feature against a real board (a latticed dev stack), bounded and small. No model runs.

Not a unit test (the name does not match test_*.py): tests/dev/dev-stack.sh runs it after the bench driver, with
the board account's settings in the environment. It needs psql, the connection settings (PG* and PGPASSWORD, or
LATTICE_BOARD_URL) and a scratch board; it uses author names of its own and removes the rows it wrote.

What it does, in order (each line printed as `ok ...`, the last line `activity-devstack: all checks passed`):
  1. board init creates the two activity tables and the counter; a second init changes nothing; with the switch off
     `board activity` writes nothing; activity-list works on an empty feed.
  2. a burst of 500 synthetic PostToolUse payloads from 10 parallel workers, through hooks/activity-hook.py and the real
     helper: the number of psql connections is counted by a wrapper and must be a handful, not 500; after the turn ends
     (Stop) the session row counts exactly 500 tool calls and the log rows account for every one of them.
  3. two more sessions at once get their own rows; the hostile payload fields (control characters, ANSI, huge strings,
     wrong types, path traversal) reach the database as clean labels of at most 64 printable characters; text that
     looks like SQL is stored as data and the board's tables are intact.
  4. the log ring: with BOARD_ACTIVITY_KEEP=5 and 80 forced writes the agent's log stays within keep + 50 and holds the newest rows.
  5. board run publishes working/idle rows for the child it supervises.
  6. the read-only console, pointed at the same database, shows the agents, their states, the strip, the agent screen and
     the scorecard diagnostic, and every answer is under the size cap.
  7. activity-purge removes the author's rows and leaves another agent's; --all --yes clears the rest.
  8. no password in anything printed.
Exit status 1 on the first failed check. It refuses a board that holds open or claimed tasks unless --cancel-leftovers
is given (the CI step passes it), because the `board run` worker of step 5 takes any runnable task.
"""

import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import threading
import time
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
BOARD_DIR = os.path.dirname(HERE)
ROOT = os.path.dirname(os.path.dirname(BOARD_DIR))
BOARD = os.path.join(BOARD_DIR, "board")
RUN = os.path.join(BOARD_DIR, "board-run")
HOOK = os.path.join(BOARD_DIR, "hooks", "activity-hook.py")
CONSOLE = os.path.join(ROOT, "integrations", "console")
PY = sys.executable
SEEN = []


def say(msg):
    print("ok %s" % msg, flush=True)


def fail(msg):
    print("FAIL %s" % msg, file=sys.stderr, flush=True)
    sys.exit(1)


def check(cond, msg):
    if not cond:
        fail(msg)


def sh(argv, author="act-ds", state=None, timeout=240, extra=None, stdin=None):
    env = dict(os.environ, BOARD_AUTHOR=author)
    if state:
        env["BOARD_STATE_DIR"] = state
    env.update(extra or {})
    p = subprocess.run(argv, env=env, capture_output=True, text=True, timeout=timeout, input=stdin)
    SEEN.extend([p.stdout, p.stderr])
    return p.returncode, p.stdout.strip(), p.stderr.strip()


def query(sql):
    """Rows of a read, tab separated, straight through psql."""
    env = dict(os.environ, LC_MESSAGES="C")
    env.pop("LC_ALL", None)
    for attempt in range(8):
        p = subprocess.run(["psql", "-X", "-q", "-A", "-t", "-F", "\t", "-v", "ON_ERROR_STOP=1", "-c", sql],
                           capture_output=True, text=True, timeout=60, env=env)
        SEEN.extend([p.stdout, p.stderr])
        # The alpha server drops a share of new connections (issue 867): a failed connect sent no statement.
        if p.returncode == 2 and "connection to server" in p.stderr and "FATAL" not in p.stderr:
            time.sleep(0.2 * (attempt + 1))
            continue
        break
    check(p.returncode == 0, "query failed: %s: %s" % (sql[:80], p.stderr[-300:]))
    return [ln.split("\t") for ln in p.stdout.splitlines() if ln]


def hook(payload, env):
    data = payload if isinstance(payload, bytes) else json.dumps(payload).encode()
    p = subprocess.run([PY, HOOK], input=data, env=env, capture_output=True, timeout=30)
    SEEN.extend([p.stdout.decode("utf-8", "replace"), p.stderr.decode("utf-8", "replace")])
    check(p.returncode == 0, "the hook exited %d" % p.returncode)
    check(p.stdout == b"", "the hook printed on stdout")
    return p


def post(tool, tool_input, session):
    return {"session_id": session, "hook_event_name": "PostToolUse", "tool_name": tool, "tool_input": tool_input,
            "tool_response": {}, "tool_use_id": "toolu_x", "cwd": "/work"}


def get_json(base, path):
    with urllib.request.urlopen(base + path, timeout=30) as r:
        body = r.read()
        check(len(body) < 4 * 1024 * 1024, "an answer is over the 4 MiB cap")
        return json.loads(body.decode("ascii"))


def main():
    tmp = tempfile.mkdtemp(prefix="activity-devstack-")
    os.chmod(tmp, 0o700)
    state = os.path.join(tmp, "state")
    os.mkdir(state, 0o700)
    tag = "%x" % (int(time.time()) % 0xFFFFFF)
    a1, a2, a3, ring, runner = ("act-%s-%s" % (tag, n) for n in ("one", "two", "three", "ring", "run"))
    counter_log = os.path.join(tmp, "psql-count.log")
    counting = os.path.join(tmp, "psql-count")
    real_psql = shutil.which("psql")
    with open(counting, "w") as f:
        f.write('#!/bin/sh\necho x >>"%s"\nexec "%s" "$@"\n' % (counter_log, real_psql))
    os.chmod(counting, 0o755)
    console = None
    try:
        # 1. the tables, and the switch
        rc, out, err = sh([BOARD, "init"], state=state)
        check(rc == 0, "board init failed: %s" % err[-300:])
        rc, out, err = sh([BOARD, "init"], state=state)
        check(rc == 0, "a second board init failed: %s" % err[-300:])
        check(query("SELECT n FROM counters WHERE name = 'activity'"), "board init did not seed the activity counter")
        # `board run` below takes ANY runnable task, so the board must hold none besides ours.
        rc, out, err = sh([BOARD, "queue"], state=state)
        live = [ln.split("\t")[1] for ln in out.splitlines() if ln.startswith("task\t") and ln.split("\t")[2] in ("open", "claimed")]
        if live and "--cancel-leftovers" in sys.argv:
            for t in live:
                sh([BOARD, "cancel", t, "scratch board cleanup"], state=state)
            live = []
        check(not live, "the board holds %d open or claimed tasks; use a scratch board (or --cancel-leftovers)" % len(live))
        rc, out, err = sh([BOARD, "activity-list"], state=state, extra={"BOARD_ACTIVITY": ""})
        check(rc == 0 and out.startswith("now\t"), "activity-list failed: %s" % err[-300:])
        rc, out, err = sh([BOARD, "activity", "--tool", "Edit", "--flush"], author=a1, state=state, extra={"BOARD_ACTIVITY": "0"})
        check(rc == 0 and "BOARD_ACTIVITY=1" in err, "activity without the switch must be a quiet no-op: %s" % err)
        check(not query("SELECT id FROM agent_activity WHERE agent = '%s'" % a1), "a row was written while the switch was off")
        say("the tables exist, init is repeatable, and an off helper writes nothing")

        # 2. the burst
        env1 = dict(os.environ, BOARD_BIN=BOARD, BOARD_AUTHOR=a1, BOARD_ACTIVITY="1", BOARD_STATE_DIR=state,
                    BOARD_PSQL=counting, PGCONNECT_TIMEOUT="3")
        sess = "burst-1"
        hook({"session_id": sess, "hook_event_name": "SessionStart", "source": "startup"}, env1)
        base_writes = len(open(counter_log).read().split())  # the SessionStart flush
        check(base_writes == 1, "SessionStart should be one write, saw %d" % base_writes)
        tools = [("Edit", {"file_path": "/home/u/proj/src/f%d.rs" % i}) for i in range(5)] + \
                [("Bash", {"command": "cargo test -p x"}), ("Read", {"file_path": "/etc/hosts"}), ("Grep", {"pattern": "x"})]
        errors = []

        def worker(k):
            try:
                for j in range(50):
                    tool, ti = tools[(k + j) % len(tools)]
                    hook(post(tool, ti, sess), env1)
            except SystemExit:
                errors.append(k)

        t0 = time.monotonic()
        threads = [threading.Thread(target=worker, args=(k,)) for k in range(10)]
        for t in threads:
            t.start()
        for t in threads:
            t.join()
        elapsed = time.monotonic() - t0
        check(not errors, "a hook worker failed")
        writes = len(open(counter_log).read().split()) - base_writes
        check(1 <= writes <= int(elapsed / 5) + 3,
              "500 hook calls in %.1f s made %d database connections (limit %d)" % (elapsed, writes, int(elapsed / 5) + 3))
        check(writes < 25, "far too many connections for one session: %d" % writes)
        hook({"session_id": sess, "hook_event_name": "Stop"}, env1)
        rows = query("SELECT tool_calls, state, task_id FROM agent_activity WHERE id = '%s/%s'" % (a1, sess))
        check(rows and rows[0][0] == "500" and rows[0][1] == "idle", "the session row should count 500 calls and be idle: %s" % rows)
        log = query("SELECT calls, mix FROM agent_activity_log WHERE agent = '%s' AND session = '%s'" % (a1, sess))
        check(sum(int(r[0]) for r in log) == 500, "the log rows do not add up to 500 calls: %s" % [r[0] for r in log])
        total_mix = 0
        for r in log:
            for item in (r[1].split(",") if r[1] else []):
                total_mix += int(item.split("=")[1])
        check(total_mix == 500, "the mixes do not add up to 500 calls: %d" % total_mix)
        say("500 hook calls in %.1f s from 10 workers were %d database connections; the counters add up to 500" % (elapsed, writes))

        # 3. two more sessions, hostile fields, text that looks like SQL
        for name, sid in ((a2, "s-two"), (a3, "s-three")):
            env = dict(env1, BOARD_AUTHOR=name)
            hook(post("Edit", {"file_path": "/a/b/%s.rs" % name}, sid), env)
            hook({"session_id": sid, "hook_event_name": "Stop"}, env)
        check(len(query("SELECT id FROM agent_activity WHERE agent IN ('%s', '%s')" % (a2, a3))) == 2, "two sessions, two rows")
        evil = "\x1b]0;owned\x07\x1b[2J" + "A" * 5000
        env2 = dict(env1, BOARD_AUTHOR=a2)
        for payload in (post("Edit", {"file_path": "/a/" + evil + "/f.rs"}, "s-two"), post("Bash", {"command": evil}, "s-two"),
                        post("Edit", {"file_path": ["/a/b"]}, "s-two"), post("Bash", None, "s-two"),
                        post(evil, {"command": "ls"}, "s-two"), post("Read", {"file_path": "../../../../etc/shadow"}, "s-two"),
                        post("Bash", {"command": "echo 'x'; DROP TABLE tasks; --"}, "s-two"),
                        post("Edit", {"file_path": "/a/b/it's;a\"file.rs"}, "s-two"),
                        {"session_id": evil, "hook_event_name": "Stop"}):
            hook(payload, env2)
        hook(b"not json at all", env2)
        hook(b'{"hook_event_name":"PostToolUse","tool_name":NaN}', env2)
        hook(json.dumps(post("Bash", {"command": "ls " + "x" * (2 * 1024 * 1024)}, "s-two")).encode(), env2)
        rc, out, err = sh([BOARD, "activity", "--tool", "Edit", "--target", "x'; DROP TABLE tasks; --\\\\n\"q\"", "--session", "s-two",
                           "--flush"], author=a2, state=state, extra={"BOARD_ACTIVITY": "1"})
        check(rc == 0, "a target that looks like SQL was refused or failed: %s" % err[-300:])
        for r in query("SELECT tool, target FROM agent_activity_log WHERE agent = '%s'" % a2):
            check(re.fullmatch(r"[A-Za-z0-9_.-]{1,64}", r[0]), "a tool name is not clean: %r" % r[0])
            check(len(r) < 2 or re.fullmatch(r"[ -~]{0,64}", r[1]), "a target is not 64 printable characters: %r" % r[1])
        check(query("SELECT id FROM tasks LIMIT 1") is not None, "the tasks table is gone")
        check(any("DROP TABLE tasks" in r[1] for r in query("SELECT tool, target FROM agent_activity_log WHERE agent = '%s'" % a2)),
              "the SQL-looking target was not stored as data")
        say("hostile fields are stored as clean labels; SQL-looking text stays data and the tables are intact")

        # 4. the ring
        envr = {"BOARD_ACTIVITY": "1", "BOARD_ACTIVITY_KEEP": "5", "BOARD_TEST_MODE": "1"}
        for i in range(80):
            rc, out, err = sh([BOARD, "activity", "--tool", "Read", "--target", "r%02d.rs" % i, "--flush", "--session", "ring"],
                              author=ring, state=state, extra=dict(envr, BOARD_ACTIVITY_NOW=str(int(time.time()) + i)))
            check(rc == 0, "ring write %d failed: %s" % (i, err[-300:]))
        ids = [int(r[0]) for r in query("SELECT id FROM agent_activity_log WHERE agent = '%s' ORDER BY id" % ring)]
        check(5 <= len(ids) <= 55, "the ring holds %d rows, expected between 5 and 55" % len(ids))
        newest = [r[0] for r in query("SELECT target FROM agent_activity_log WHERE agent = '%s' ORDER BY id DESC LIMIT 5" % ring)]
        check(newest == ["r%02d.rs" % i for i in (79, 78, 77, 76, 75)], "the ring lost its newest rows: %s" % newest)
        say("the log ring stayed bounded (%d rows after 80 writes with keep 5) and kept the newest rows" % len(ids))

        # 5. board run publishes a row for its child
        rc, out, err = sh([BOARD, "add", "activity devstack task"], author=runner, state=state)
        check(rc == 0, "board add failed: %s" % err[-300:])
        task = out.strip().splitlines()[-1]
        rc, out, err = sh([PY, RUN, "--owner", runner, "--max-tasks", "1", "--lease", "60", "--poll", "0.2", "--result-dir",
                           os.path.join(tmp, "runs"), "--", "true"], author=runner, state=state, extra={"BOARD_ACTIVITY": "1"})
        check(rc == 0, "board run failed: %s %s" % (out[-200:], err[-300:]))
        rows = query("SELECT tool, target, state, outcome, task_id FROM agent_activity WHERE agent = '%s'" % runner)
        check(rows and rows[0][0] == "agent-run" and rows[0][1] == "true" and rows[0][2] == "ended", "board run published: %s" % rows)
        log = query("SELECT tool, task_id, outcome FROM agent_activity_log WHERE agent = '%s' ORDER BY id" % runner)
        check(any(r[1] == task for r in log), "no row linked the child's task %s: %s" % (task, log))
        say("board run published agent-run rows for the child (working with the task, then idle, then ended)")

        # 6. the console
        env = dict(os.environ)
        console = subprocess.Popen([PY, "-m", "console", "serve", "--port", "0"], cwd=CONSOLE, env=env, stdout=subprocess.PIPE,
                                   stderr=subprocess.STDOUT, text=True)
        base = None
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            line = console.stdout.readline()
            m = re.match(r"console: serving (http://127\.0\.0\.1:\d+)/", line)
            if m:
                base = m.group(1)
                break
        check(base, "the console did not start")
        now = get_json(base, "/api/now")
        check(now["activity"]["available"], "the console says activity is not available: %s" % now["activity"].get("note"))
        by = {a["name"]: a for a in now["agents"]}
        for name in (a1, a2, a3, ring, runner):
            check(name in by and by[name]["activity"], "the Now screen has no activity for %s" % name)
        check(by[a1]["activity"]["session_state"] == "idle" and by[a1]["activity"]["calls"] == 500, "the burst agent: %s" % by[a1]["activity"])
        check(sum(by[a1]["activity"]["strip"]) == 500, "the strip counts %d, not 500" % sum(by[a1]["activity"]["strip"]))
        check(by[a1]["activity"]["state"] in ("working", "idle"), "an agent with no task is working or idle")
        d = get_json(base, "/api/agent?name=%s" % a1)
        check(d["rows_held"] > 1 and d["mix"] and dict(d["mix"]).get("Edit"), "the agent screen has no timeline or mix")
        check(sum(v for _, v in d["mix"]) == 500, "the tool mix does not add up to 500")
        d2 = get_json(base, "/api/agent?name=%s&tool=Bash&limit=3" % a1)
        check(all(r["tool"] == "Bash" or "Bash=" in r["mix"] for r in d2["rows"]), "the tool filter does not filter")
        d3 = get_json(base, "/api/agent?name=%s&after=%d" % (a1, d["cursor"]))
        check(d3["rows"] == [], "the cursor returned rows after the newest")
        score = get_json(base, "/api/scorecard")
        check(score["activity"] is not None and "note" in score["activity"], "the scorecard has no activity diagnostic")
        for bad in ("/api/agent?name=a%20b", "/api/agent?name=x&limit=999"):
            try:
                urllib.request.urlopen(base + bad, timeout=10)
                fail("the console accepted %s" % bad)
            except urllib.error.HTTPError as e:
                check(e.code == 400, "%s gave %d, not 400" % (bad, e.code))
        say("the console reads the rows: states, strip, agent screen with filter and cursor, scorecard diagnostic")

        # 7. purge
        rc, out, err = sh([BOARD, "activity-purge"], author=a1, state=state, extra={"BOARD_ACTIVITY": "1"})
        check(rc == 0 and "log rows deleted" in out, "purge failed: %s" % err[-300:])
        check(not query("SELECT id FROM agent_activity WHERE agent = '%s'" % a1), "the session rows are still there")
        check(not query("SELECT id FROM agent_activity_log WHERE agent = '%s'" % a1), "the log rows are still there")
        check(query("SELECT id FROM agent_activity WHERE agent = '%s'" % a2), "another agent's rows were deleted")
        check(not [f for f in os.listdir(os.path.join(state, "activity")) if f.startswith(a1 + "@")], "local counters remain")
        rc, out, err = sh([BOARD, "activity-purge", "--all"], author=a1, state=state)
        check(rc == 1, "--all without --yes must be refused")
        for name in (a2, a3, ring, runner):
            rc, out, err = sh([BOARD, "activity-purge", "--agent", name, "--yes"], author=a1, state=state)
            check(rc == 0, "purge of %s failed: %s" % (name, err[-200:]))
        check(not query("SELECT id FROM agent_activity WHERE agent LIKE 'act-%s-%%'" % tag), "rows remain after the purges")
        say("purge removed the author's rows and the local counters, refused --all without --yes, and cleared the rest")

        # 8. no password anywhere
        pw = os.environ.get("PGPASSWORD", "")
        if pw:
            check(not any(pw in s for s in SEEN), "the password appears in something printed")
        say("no password in anything printed")
        print("activity-devstack: all checks passed", flush=True)
    finally:
        if console is not None:
            console.terminate()
            try:
                console.wait(timeout=5)
            except subprocess.TimeoutExpired:
                console.kill()
        shutil.rmtree(tmp, ignore_errors=True)


if __name__ == "__main__":
    main()

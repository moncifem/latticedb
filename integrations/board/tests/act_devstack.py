#!/usr/bin/env python3
"""Drive `board act` and the console's write mode against a real board (a latticed dev stack), small and bounded.
No model runs, nothing is installed.

Not a unit test (the name does not match test_*.py): tests/dev/dev-stack.sh runs it after the activity driver, with
the board account's settings in the environment (PG* and PGPASSWORD, or LATTICE_BOARD_URL). It needs psql and a
scratch board; it uses author names of its own and cancels what it created. Each line printed is `ok ...`, the last
is `act-devstack: all checks passed`; exit status 1 on the first failed check.

What it checks, in order:
  1. board init creates human_actions; the human. names are refused by every command except `act`.
  2. A task that waits for approval: the person who made it (as human.NAME) is refused (self_approval) and nothing
     changed; a wrong hash is refused (stale); the right hash by another person approves it, writes the event and the
     human_actions row (author, kind, the sha-256, the event id) in one go; an agent then claims it with `board next`.
  3. The same request id again is a no-op that names the same event (one approve event, one row); the same id for
     another decision is refused; a reject without a comment is refused, with one it cancels the task.
  4. A dead letter is requeued (attempts back to 0) and claimed again; a claim is cancelled by a person and the
     agent that held it is refused by the fence; priority and note.
  5. The console in write mode against this board: a decision through the API with the token (event and row in the
     database), the self-approval, stale and repeat cases through HTTP, a wrong token, a wrong Origin and a rebinding
     Host refused, GET never writes, and writes off without --allow-writes (every POST is a 403).
  6. No password in anything printed.
It refuses a board that holds open or claimed tasks unless --cancel-leftovers is given (the CI step passes it),
because `board next` below takes the best runnable task and the checks need it to be theirs.
"""

import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
BOARD_DIR = os.path.dirname(HERE)
ROOT = os.path.dirname(os.path.dirname(BOARD_DIR))
BOARD = os.path.join(BOARD_DIR, "board")
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


def sh(argv, author, state, timeout=240, extra=None):
    env = dict(os.environ, BOARD_AUTHOR=author, BOARD_STATE_DIR=state)
    env.pop("BOARD_ACTIVITY", None)
    env.update(extra or {})
    p = subprocess.run(argv, env=env, capture_output=True, text=True, timeout=timeout)
    SEEN.extend([p.stdout, p.stderr])
    return p.returncode, p.stdout.strip(), p.stderr.strip()


def query(sql):
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


def sha(title, body):
    return hashlib.sha256((title + "\n" + body).encode("utf-8")).hexdigest()


def post(base, path, body, token, origin=None, host=None, ctype="application/json", raw=None):
    """(status, json) of one POST; urllib raises on 4xx, so read the error body too."""
    data = raw if raw is not None else json.dumps(body).encode()
    req = urllib.request.Request(base + path, data=data, method="POST")
    req.add_header("Content-Type", ctype)
    if token is not None:
        req.add_header("X-Console-Write-Token", token)
    if origin is not None:
        req.add_header("Origin", origin)
    if host is not None:
        req.add_header("Host", host)
    try:
        with urllib.request.urlopen(req, timeout=60) as r:
            return r.status, json.loads(r.read().decode("ascii"))
    except urllib.error.HTTPError as e:
        try:
            return e.code, json.loads(e.read().decode("ascii"))
        except ValueError:
            return e.code, None


def get(base, path):
    with urllib.request.urlopen(base + path, timeout=60) as r:
        return json.loads(r.read().decode("ascii"))


def start_console(args, tmp, tag):
    out = os.path.join(tmp, "console-%s.log" % tag)
    log = open(out, "w")
    proc = subprocess.Popen([PY, "-m", "console", "serve", "--port", "0"] + args, cwd=CONSOLE, env=dict(os.environ),
                            stdout=log, stderr=subprocess.STDOUT, text=True)
    base = token = None
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline and not (base and ("--allow-writes" not in args or token)):
        time.sleep(0.2)
        with open(out) as f:
            text = f.read()
        m = re.search(r"console: serving (http://127\.0\.0\.1:\d+)/", text)
        base = m.group(1) if m else None
        t = re.search(r"write token \(shown once, paste it into the page\): (\S+)", text)
        token = t.group(1) if t else None
    check(base, "the console did not start (%s)" % tag)
    SEEN.append(open(out).read().replace(token or "\0", "<token>"))
    return proc, base, token, out


def stop(proc):
    proc.terminate()
    try:
        proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        proc.kill()


def main():
    tmp = tempfile.mkdtemp(prefix="act-devstack-")
    os.chmod(tmp, 0o700)
    state = os.path.join(tmp, "state")
    os.mkdir(state, 0o700)
    tag = "%x" % (int(time.time()) % 0xFFFFFF)
    maker, worker, rev, rev2, wnum = ("act-%s-%s" % (tag, n) for n in ("maker", "worker", "rev", "rev2", "w2"))
    h_rev, h_rev2, h_maker = "human." + rev, "human." + rev2, "human." + maker
    made = []
    consoles = []

    def add(title, body="", *flags, author=None):
        argv = [BOARD, "add", title] + (["--body", body] if body else []) + list(flags)
        rc, out, err = sh(argv, author or maker, state)
        check(rc == 0, "board add failed: %s" % err[-300:])
        tid = out.splitlines()[-1]
        made.append(tid)
        return tid

    def act(action, tid, rid, *more, author=h_rev):
        return sh([BOARD, "act", action, tid, "--request-id", rid] + list(more), author, state)

    def state_of(tid):
        rows = query("SELECT state, coalesce(owner, ''), priority FROM tasks WHERE id = '%s'" % tid)
        check(rows, "no task %s" % tid)
        return rows[0]

    def events_of(tid, kind):
        return query("SELECT id, author, body FROM events WHERE task_id = '%s' AND kind = '%s' ORDER BY id" % (tid, kind))

    try:
        # 1. the table, and the namespace
        rc, out, err = sh([BOARD, "init"], maker, state)
        check(rc == 0, "board init failed: %s" % err[-300:])
        rc, out, err = sh([BOARD, "init"], maker, state)
        check(rc == 0, "a second board init failed: %s" % err[-300:])
        check(query("SELECT request_id FROM human_actions WHERE request_id = 'nothing-here'") == [], "human_actions is not readable")
        rc, out, err = sh([BOARD, "queue"], maker, state)
        live = [ln.split("\t")[1] for ln in out.splitlines() if ln.startswith("task\t") and ln.split("\t")[2] in ("open", "claimed")]
        if live and "--cancel-leftovers" in sys.argv:
            for t in live:
                sh([BOARD, "cancel", t, "scratch board cleanup"], maker, state)
            live = []
        check(not live, "the board holds %d open or claimed tasks; use a scratch board (or --cancel-leftovers)" % len(live))
        rc, out, err = sh([BOARD, "note", "tnone", "hello"], h_rev, state)
        check(rc == 1 and "reserved for 'board act'" in err, "a human. name was accepted by `note`: rc=%s %s" % (rc, err[-200:]))
        rc, out, err = act("cancel", "tnone", "req-%s-agent" % tag, author=maker)
        check(rc == 1 and "must be human.NAME" in err, "an agent name was accepted by `act`: rc=%s %s" % (rc, err[-200:]))
        say("board init creates human_actions; human. names work only with `act`, and `act` only with human. names")

        # 2. approval
        title, body = "Raise the export retry delay", "Proposed: wait 30 s, not 5 s, and run the export test twice. café ☕"
        t1 = add(title, body, "--needs-approval", "--priority", "50")
        check(state_of(t1)[0] == "awaiting_approval", "the task does not wait for approval")
        good = sha(title, body)
        rc, out, err = act("approve", t1, "req-%s-self" % tag, "--hash", good, author=h_maker)
        check(rc == 5 and out == "refused self_approval", "the maker was not refused: rc=%s %s %s" % (rc, out, err[-200:]))
        check(state_of(t1)[0] == "awaiting_approval", "a refused self-approval changed the task")
        rc, out, err = act("approve", t1, "req-%s-stale" % tag, "--hash", sha(title, body + " changed"))
        check(rc == 5 and out == "refused stale", "a wrong hash was not refused as stale: rc=%s %s %s" % (rc, out, err[-200:]))
        rc, out, err = act("approve", t1, "req-%s-nohash" % tag)
        check(rc == 5 and out == "refused hash_required", "an approval without a hash: rc=%s %s" % (rc, out))
        check(state_of(t1)[0] == "awaiting_approval" and not events_of(t1, "approve"), "refusals changed or recorded something")
        rid = "req-%s-approve" % tag
        rc, out, err = act("approve", t1, rid, "--hash", good, "--comment", "reads right")
        check(rc == 0 and re.fullmatch(r"done [0-9]+", out), "the approval failed: rc=%s %s %s" % (rc, out, err[-300:]))
        eid = out.split()[1]
        check(state_of(t1)[0] == "open", "the task is not open after the approval")
        ev = events_of(t1, "approve")
        check(len(ev) == 1 and ev[0][0] == eid and ev[0][1] == h_rev and good in ev[0][2], "the approve event: %s" % ev)
        row = query("SELECT task_id, author, kind, body_sha, event_id, remark FROM human_actions WHERE request_id = '%s'" % rid)
        check(row == [[t1, h_rev, "approve", good, eid, "reads right"]], "the human_actions row: %s" % row)
        rc, out, err = sh([BOARD, "next", "--owner", worker, "--lease", "60"], worker, state)
        check(rc == 0 and out.splitlines()[-1] == t1, "the agent did not get the approved task: rc=%s %s %s" % (rc, out, err[-200:]))
        rc, out, err = sh([BOARD, "done", t1, "finished"], worker, state)
        check(rc == 0, "the agent could not finish the task: %s" % err[-200:])
        say("a self-approval and a wrong hash are refused and write nothing; the right hash approves with event and audit row; an agent takes it")

        # 3. idempotency, reuse, reject
        rc, out, err = act("approve", t1, rid, "--hash", good, "--comment", "reads right")
        check(rc == 0 and out == "noop " + eid, "a repeat was not a no-op on the same event: rc=%s %s" % (rc, out))
        check(len(events_of(t1, "approve")) == 1, "a repeat wrote a second approve event")
        check(len(query("SELECT request_id FROM human_actions WHERE task_id = '%s'" % t1)) == 1, "a repeat wrote a second row")
        rc, out, err = act("note", t1, rid, "--comment", "other")
        check(rc == 5 and out == "refused request_reused", "a request id reused for another decision: rc=%s %s" % (rc, out))
        t2 = add("Delete the old exports", "Proposed: rm the files older than a year", "--needs-approval")
        rc, out, err = act("reject", t2, "req-%s-rej0" % tag)
        check(rc == 5 and out == "refused comment_required", "a reject without a comment: rc=%s %s" % (rc, out))
        rc, out, err = act("reject", t2, "req-%s-rej1" % tag, "--comment", "too risky without a backup", author=h_rev2)
        check(rc == 0 and out.startswith("done "), "the reject failed: rc=%s %s %s" % (rc, out, err[-300:]))
        check(state_of(t2)[0] == "cancelled", "a rejected task is not cancelled")
        ev = events_of(t2, "reject")
        check(len(ev) == 1 and ev[0][1] == h_rev2 and "too risky" in ev[0][2], "the reject event: %s" % ev)
        rc, out, err = act("approve", t2, "req-%s-late" % tag, "--hash", sha("Delete the old exports", "Proposed: rm the files older than a year"))
        check(rc == 5 and out == "refused wrong_state", "approving a cancelled task: rc=%s %s" % (rc, out))
        say("a repeated request id is a no-op on the same event; reuse is refused; a reject needs a comment and cancels")

        # 4. dead letter, cancel with the fence, priority, note
        t3 = add("Flaky export", "", "--max-attempts", "1", "--priority", "50")
        rc, out, err = sh([BOARD, "next", "--owner", worker, "--lease", "60"], worker, state)
        check(rc == 0 and out.splitlines()[-1] == t3, "could not claim the flaky task: %s %s" % (out, err[-200:]))
        rc, out, err = sh([BOARD, "fail", t3, "disk full"], worker, state)
        check(rc == 0 and state_of(t3)[0] == "dead", "the task is not dead: %s %s" % (out, err[-200:]))
        rc, out, err = act("cancel", t3, "req-%s-cdead" % tag)
        check(rc == 5 and out == "refused wrong_state", "cancelling a dead task: rc=%s %s" % (rc, out))
        rc, out, err = act("requeue", t3, "req-%s-requeue" % tag, "--comment", "fixed the disk")
        check(rc == 0 and state_of(t3)[0] == "open", "requeue failed: rc=%s %s %s" % (rc, out, err[-300:]))
        lease = query("SELECT attempts, lease_until, last_error FROM task_leases WHERE task_id = '%s'" % t3)
        check(lease == [["0", "0", ""]], "the lease row after a requeue: %s" % lease)
        rc, out, err = sh([BOARD, "next", "--owner", worker, "--lease", "60"], worker, state)
        check(rc == 0 and out.splitlines()[-1] == t3, "the requeued task was not claimable: %s %s" % (out, err[-200:]))
        fence = query("SELECT claim_seq FROM tasks WHERE id = '%s'" % t3)[0][0]
        rc, out, err = act("cancel", t3, "req-%s-cancel" % tag, "--comment", "not needed any more")
        check(rc == 0 and state_of(t3)[0] == "cancelled", "cancelling a claimed task failed: rc=%s %s %s" % (rc, out, err[-300:]))
        rc, out, err = sh([BOARD, "done", t3, "--fence", fence], worker, state)
        check(rc == 3, "the agent that held the cancelled task was not refused by the fence: rc=%s %s" % (rc, err[-200:]))
        t4 = add("Low task", "")
        t5 = add("High task", "")
        rc, out, err = act("priority", t5, "req-%s-prio" % tag, "--priority", "77")
        check(rc == 0 and state_of(t5)[2] == "77", "priority failed: rc=%s %s %s" % (rc, out, err[-300:]))
        rc, out, err = act("priority", t5, "req-%s-prio2" % tag, "--priority", "oops")
        check(rc == 1, "a priority that is not a number was accepted")
        rc, out, err = act("note", t4, "req-%s-note" % tag, "--comment", "ask legal first")
        ev = events_of(t4, "note")
        check(rc == 0 and len(ev) == 1 and ev[0][1] == h_rev and ev[0][2] == "ask legal first", "the note: rc=%s %s" % (rc, ev))
        check(state_of(t4)[0] == "open", "a note changed the task")
        say("a dead letter is requeued with its attempts reset and claimed again; a person's cancel makes the holder's fence fail; priority and note")

        # 5. the console in write mode
        for tid in (t4, t5):
            sh([BOARD, "cancel", tid, "devstack cleanup"], maker, state)
        t6 = add("Console approval", "Proposed: change nothing important.", "--needs-approval", "--priority", "50")
        t7 = add("Console stale", "Proposed: this text will change.", "--needs-approval", "--priority", "50")
        proc, base, token, out_path = start_console(["--allow-writes", "--actor", rev, "--board", BOARD], tmp, "w")
        consoles.append(proc)
        origin = base
        host = base.split("//")[1]
        meta = get(base, "/api/now")["meta"]
        check(meta["writes"] == {"enabled": True, "actor": h_rev}, "the console's write state: %s" % meta["writes"])
        inbox = get(base, "/api/inbox")
        items = {i["id"]: i for i in inbox["items"]}
        check(t6 in items and t7 in items and items[t6]["kind"] == "approval", "the approvals are not in the inbox: %s" % list(items))
        check(items[t6]["sha"] == sha("Console approval", "Proposed: change nothing important."), "the console's hash differs from the helper's")
        body = {"action": "approve", "task": t6, "request_id": "web-%s-0001" % tag, "sha": items[t6]["sha"]}
        st, d = post(base, "/api/act", body, token, origin)
        check(st == 200 and d["ok"] and not d["repeated"], "the approval through the console: %s %s" % (st, d))
        check(state_of(t6)[0] == "open", "the console's approval did not open the task")
        ev = events_of(t6, "approve")
        check(len(ev) == 1 and ev[0][1] == h_rev and str(ev[0][0]) == str(d["event_id"]), "the console's approve event: %s %s" % (ev, d))
        row = query("SELECT author, kind, body_sha FROM human_actions WHERE request_id = 'web-%s-0001'" % tag)
        check(row == [[h_rev, "approve", items[t6]["sha"]]], "the console's audit row: %s" % row)
        st, d2 = post(base, "/api/act", body, token, origin)
        check(st == 200 and d2["repeated"] and d2["event_id"] == d["event_id"], "a repeat through the console: %s %s" % (st, d2))
        check(len(events_of(t6, "approve")) == 1, "a repeat through the console wrote again")
        # stale: change the text under the page
        stale_body = {"action": "approve", "task": t7, "request_id": "web-%s-0002" % tag, "sha": "0" * 64}
        st, d = post(base, "/api/act", stale_body, token, origin)
        check(st == 409 and d["code"] == "stale" and state_of(t7)[0] == "awaiting_approval", "stale through the console: %s %s" % (st, d))
        # the maker's own name as the console's person
        proc2, base2, token2, _ = start_console(["--allow-writes", "--actor", maker, "--board", BOARD], tmp, "self")
        consoles.append(proc2)
        item7 = {i["id"]: i for i in get(base2, "/api/inbox")["items"]}[t7]
        check(item7["you_produced"] is True, "the console does not flag the task its person made")
        st, d = post(base2, "/api/act", {"action": "approve", "task": t7, "request_id": "web-%s-0003" % tag, "sha": item7["sha"]},
                     token2, base2)
        check(st == 403 and d["code"] == "self_approval" and state_of(t7)[0] == "awaiting_approval", "self-approval through the console: %s %s" % (st, d))
        stop(proc2)
        # the gates
        ok_body = {"action": "note", "task": t7, "request_id": "web-%s-0004" % tag, "comment": "gate probe"}
        st, d = post(base, "/api/act", ok_body, "wrong-token-value", origin)
        check(st == 401, "a wrong token: %s" % st)
        st, d = post(base, "/api/act", ok_body, token, "http://evil.example")
        check(st == 403, "a foreign Origin: %s" % st)
        st, d = post(base, "/api/act", ok_body, token, None)
        check(st == 403, "a missing Origin: %s" % st)
        st, d = post(base, "/api/act", ok_body, token, origin, host="evil.example")
        check(st == 421, "a rebinding Host: %s" % st)
        st, d = post(base, "/api/act", ok_body, token, origin, ctype="text/plain")
        check(st == 415, "a form content type: %s" % st)
        try:
            urllib.request.urlopen(base + "/api/act", timeout=10)
            fail("GET /api/act was served")
        except urllib.error.HTTPError as e:
            check(e.code == 404, "GET /api/act gave %d" % e.code)
        check(not events_of(t7, "note"), "a refused probe wrote a note")
        st, d = post(base, "/api/act/bulk", {"action": "cancel", "ids": [t7], "dry_run": True}, token, origin)
        check(st == 200 and d["count"] == 1 and state_of(t7)[0] == "awaiting_approval", "the dry run: %s %s" % (st, d))
        st, d = post(base, "/api/act/bulk", {"action": "approve", "ids": [t7], "dry_run": True}, token, origin)
        check(st == 400, "a bulk approve was accepted: %s" % st)
        st, d = post(base, "/api/act/bulk", {"action": "cancel", "ids": [t7], "dry_run": False, "request_id": "web-%s-0005" % tag, "confirm": 1},
                     token, origin)
        check(st == 200 and d["done"] == 1 and state_of(t7)[0] == "cancelled", "the confirmed bulk cancel: %s %s" % (st, d))
        stop(proc)
        proc3, base3, _, _ = start_console([], tmp, "ro")
        consoles.append(proc3)
        st, d = post(base3, "/api/act", ok_body, token, base3)
        check(st == 403, "a POST to a console without --allow-writes: %s" % st)
        check(get(base3, "/api/now")["meta"]["writes"]["enabled"] is False, "the read-only console says writes are on")
        say("the console: approve, repeat, stale, self-approval, bulk dry run and confirm; wrong token, Origin, Host and content type refused; GET never writes; writes off without the flag")
        check(not any(token in s for s in SEEN), "the write token appears in the output")

        # 6. no password anywhere
        pw = os.environ.get("PGPASSWORD", "")
        if pw:
            check(not any(pw in s for s in SEEN), "the password appears in something printed")
        say("no password and no token in anything printed")
        print("act-devstack: all checks passed", flush=True)
    finally:
        for c in consoles:
            stop(c)
        for tid in made:
            sh([BOARD, "cancel", tid, "devstack cleanup"], maker, state)
        shutil.rmtree(tmp, ignore_errors=True)


if __name__ == "__main__":
    main()

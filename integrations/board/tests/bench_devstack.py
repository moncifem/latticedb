#!/usr/bin/env python3
"""Drive board bench against a real board (a latticed dev stack), bounded and small. No model is run.

Not a unit test (the name does not match test_*.py): tests/dev/dev-stack.sh runs it after the MCP driver,
with the board account's settings in the environment. It needs psql, the connection settings (PG* and
PGPASSWORD, or LATTICE_BOARD_URL), and a scratch board: its `board run` workers take ANY runnable task, so
run it on a board nobody else uses (--cancel-leftovers closes tasks left open by earlier steps).

What it does, in order (each line printed as `ok ...`, the last line `bench-devstack: all checks passed`):
  1. board init creates the four bench tables; a second init changes nothing.
  2. `board-bench start` on the example suite makes 16 board tasks that wait for approval; nothing runs
     until `approve`.
  3. two `board run` workers with the fake agents run the attempts end to end. The attempts that need it
     show a flaky grader (pass by majority, marked flaky), a grader that could not run (an error), an
     arm that leaves a bug alone (a failure).
  4. the report equals the numbers worked out by hand from the suite, and says inconclusive for 3 tasks.
  5. a second experiment: an agent that rewrites the tests fails and is counted as tampered; an honest
     agent passes.
  5b. an agent that edits the pristine copy of the fixture as well as its own, and an agent that deletes the grader's pinned
      script: both fail with tampering recorded (neither is a harness error that would drop the attempt).
  5c. four changed tasks in thirty: the bootstrap alone would name a winner, the exact test says p = 0.125, so the
      report says inconclusive (the review case).
  6. a zombie: a worker loses its lease (the task is taken away), another worker takes the task; the zombie's bench-record is
     refused by the fence (exit 3) and writes nothing; the live worker's record lands once and a second
     record changes nothing.
  7. the read-only console, pointed at the same database, serves the same numbers as `board-bench report`.
  8. no password in anything printed.
Exit status 1 on the first failed check.
"""

import hashlib
import json
import os
import shutil
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
BOARD_DIR = os.path.dirname(HERE)
ROOT = os.path.dirname(os.path.dirname(BOARD_DIR))
BOARD = os.path.join(BOARD_DIR, "board")
BENCH = os.path.join(BOARD_DIR, "board-bench")
RUN = os.path.join(BOARD_DIR, "board-run")
EXAMPLE = os.path.join(BOARD_DIR, "bench", "examples")
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


def sh(argv, author="bench-ds", state=None, timeout=240, extra=None):
    env = dict(os.environ, BOARD_AUTHOR=author)
    if state:
        env["BOARD_STATE_DIR"] = state
    env.update(extra or {})
    p = subprocess.run(argv, env=env, capture_output=True, text=True, timeout=timeout)
    SEEN.extend([p.stdout, p.stderr])
    return p.returncode, p.stdout.strip(), p.stderr.strip()


def workers(n, tag, state, runs, work, suite_dir, owner_prefix="w"):
    procs = []
    for i in range(n):
        env = dict(os.environ, BOARD_STATE_DIR=os.path.join(state, "w%s%d" % (tag, i)))
        procs.append(subprocess.Popen(
            [PY, RUN, "--owner", "%s%s%d" % (owner_prefix, tag, i), "--poll", "0.2", "--lease", "60", "--task-timeout", "180",
             "--result-dir", runs, "--after-cmd", "%s %s record" % (PY, BENCH), "--",
             PY, BENCH, "attempt", "--suite-dir", suite_dir, "--work-dir", work],
            env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True))
    outs = [p.communicate(timeout=900) for p in procs]
    for p, (out, err) in zip(procs, outs):
        SEEN.extend([out, err])
        check(p.returncode == 0, "a worker failed: %s" % err[-400:])
    return outs


def report(name, state):
    rc, out, err = sh([PY, BENCH, "report", name, "--json"], state=state)
    check(rc == 0, "report %s failed: %s" % (name, err[-300:]))
    return json.loads(out)


def main():
    tmp = tempfile.mkdtemp(prefix="bench-devstack-")
    os.chmod(tmp, 0o700)
    state = os.path.join(tmp, "state")
    runs = os.path.join(tmp, "runs")
    work = os.path.join(tmp, "work")
    for d in (state, runs, work):
        os.mkdir(d, 0o700)
    tag = "%x" % (int(time.time()) % 0xFFFFFF)
    try:
        # 1. the tables
        rc, out, err = sh([BOARD, "init"], state=state)
        check(rc == 0, "board init failed: %s" % err[-300:])
        rc, out, err = sh([BOARD, "init"], state=state)
        check(rc == 0, "a second board init failed: %s" % err[-300:])
        rc, out, err = sh([BOARD, "bench-list"], state=state)
        check(rc == 0, "bench-list failed on a fresh board: %s" % err[-300:])
        rc, out, err = sh([BOARD, "queue"], state=state)
        live = [ln.split("\t")[1] for ln in out.splitlines() if ln.startswith("task\t") and ln.split("\t")[2] in ("open", "claimed")]
        if live and "--cancel-leftovers" in sys.argv:
            for t in live:
                sh([BOARD, "cancel", t, "scratch board cleanup"], state=state)
            live = []
        check(not live, "the board holds %d open or claimed tasks; use a scratch board (or --cancel-leftovers)" % len(live))
        say("the_bench_tables_exist_and_init_is_repeatable")

        # 2. start: sixteen tasks that wait
        suite_dir = os.path.join(tmp, "ex")
        shutil.copytree(EXAMPLE, suite_dir)
        suite = os.path.join(suite_dir, "suite.json")
        exp = "ds%s" % tag
        rc, out, err = sh([PY, BENCH, "plan", suite], state=state)
        check(rc == 0 and "attempts 16" in out, "plan: %s %s" % (out[-200:], err[-200:]))
        rc, out, err = sh([PY, BENCH, "start", suite, "--experiment", exp, "--seed", "5"], state=state)
        check(rc == 0, "start failed: %s" % err[-400:])
        rc, out, err = sh([BOARD, "list", "awaiting_approval"], state=state)
        waiting = [ln for ln in out.splitlines() if "bench %s " % exp in ln]
        check(len(waiting) == 16, "16 attempts should wait for approval, found %d" % len(waiting))
        rc, out, err = sh([PY, BENCH, "start", suite, "--experiment", exp], state=state)
        check(rc == 1 and "already exists" in err, "a taken experiment name must be refused: %s" % err[-200:])
        rc, out, err = sh([PY, BENCH, "report", exp, "--json"], state=state)
        rep = json.loads(out)
        check(rep["pending"] == 16 and all(a["pass_at_1"]["value"] is None for a in rep["arms"]), "nothing has run: no result yet")
        say("start_made_16_board_tasks_that_wait_for_approval")

        # 3. the workers
        rc, out, err = sh([PY, BENCH, "approve", exp], state=state)
        check(rc == 0 and "approved 16 of 16" in out, "approve: %s" % (out + err)[-300:])
        workers(2, tag, state, runs, work, suite_dir)
        rc, out, err = sh([BOARD, "list", "done", "--all"], state=state)
        check(len([ln for ln in out.splitlines() if "bench %s " % exp in ln]) == 16, "all 16 tasks should be done")
        say("two_board_run_workers_ran_the_16_attempts")

        # 4. the numbers, by hand
        rep = report(exp, state)
        base, cand = rep["arms"]
        check((base["arm"], base["tasks"], base["attempts"], base["passes"], base["errors"], base["flaky"]) == ("baseline", 3, 6, 4, 2, 2),
              "baseline counts: %r" % ((base["tasks"], base["attempts"], base["passes"], base["errors"], base["flaky"]),))
        check(abs(base["pass_at_1"]["value"] - 2 / 3) < 1e-9, "baseline pass@1")
        check((cand["passes"], cand["attempts"], cand["flaky"], cand["errors"]) == (6, 6, 2, 2), "candidate counts")
        check(cand["cost"]["n"] == 6 and abs(cand["cost"]["total"] - 0.06) < 1e-9, "cost reported by the fake agent")
        c = rep["comparisons"][0]
        check(c["table"] == {"both": 2, "a_only": 0, "b_only": 1, "neither": 0} and abs(c["mcnemar_p"] - 1.0) < 1e-12, "paired table")
        check(c["verdict"]["label"] == "inconclusive" and c["paired_tasks"] == 3, "three tasks are inconclusive")
        check(len(rep["errors"]) == 4 and all("could not run" in e["summary"] for e in rep["errors"]), "the grader that cannot run is an error")
        check(rep["pending"] == 0, "nothing pending")
        say("the_report_matches_the_hand_computed_numbers_and_says_inconclusive")

        # 5. cheating and honesty
        cheat_dir = os.path.join(tmp, "cheat")
        shutil.copytree(EXAMPLE, cheat_dir)
        doc = json.load(open(os.path.join(cheat_dir, "suite.json")))
        doc["suite"] = "cheat-check"
        doc["repeats"] = 1
        doc["tasks"] = [t for t in doc["tasks"] if t["id"] == "calc-add"]
        doc["arms"] = [{"id": "cheater", "agent": "cmd", "args": {"argv": ["python3", "{suite_dir}/agents/solver.py", "--cheat", "--skip", "calc,greet"]}},
                       {"id": "honest", "agent": "cmd", "args": {"argv": ["python3", "{suite_dir}/agents/solver.py"]}}]
        with open(os.path.join(cheat_dir, "suite.json"), "w") as f:
            json.dump(doc, f)
        cheat_exp = "dc%s" % tag
        rc, out, err = sh([PY, BENCH, "start", os.path.join(cheat_dir, "suite.json"), "--experiment", cheat_exp, "--approve", "--seed", "1"], state=state)
        check(rc == 0, "start (cheat): %s" % err[-300:])
        workers(1, tag + "c", state, runs, work, cheat_dir)
        rep = report(cheat_exp, state)
        by = {a["arm"]: a for a in rep["arms"]}
        check(by["cheater"]["passes"] == 0 and by["cheater"]["attempts"] == 1 and by["cheater"]["tampered"] == 1,
              "an agent that rewrites the tests must fail and be marked tampered: %r" % by["cheater"])
        check(by["honest"]["passes"] == 1 and by["honest"]["tampered"] == 0, "the honest agent passes")
        say("an_agent_that_rewrites_the_tests_fails_and_is_marked_tampered")

        # 5b. attacks on the harness itself
        att_dir = os.path.join(tmp, "attack")
        shutil.copytree(EXAMPLE, att_dir)
        base_doc = json.load(open(os.path.join(att_dir, "suite.json")))
        calc = [t for t in base_doc["tasks"] if t["id"] == "calc-add"][0]
        pin = "sha256:" + hashlib.sha256(open(os.path.join(att_dir, "graders", "unittest_grader.py"), "rb").read()).hexdigest()
        gtask = json.loads(json.dumps(calc))
        gtask["id"] = "pinned-calc"
        gtask["grader"] = {"cmd": ["python3", "{suite_dir}/graders/unittest_grader.py"], "timeout_s": 60,
                           "protected": ["tests/**"], "files": {"graders/unittest_grader.py": pin}}
        doc = {"suite": "attack-check", "version": 1, "repeats": 1, "tasks": [calc], "arms": [
            {"id": "pristine-cheater", "agent": "cmd", "args": {"argv": ["python3", "{suite_dir}/agents/pristine_cheat.py"]}}]}
        with open(os.path.join(att_dir, "suite.json"), "w") as f:
            json.dump(doc, f)
        att_exp = "da%s" % tag
        rc, out, err = sh([PY, BENCH, "start", os.path.join(att_dir, "suite.json"), "--experiment", att_exp, "--approve", "--seed", "1"], state=state)
        check(rc == 0, "start (attack): %s" % err[-300:])
        workers(1, tag + "a", state, runs, work, att_dir)
        arm = report(att_exp, state)["arms"][0]
        check((arm["passes"], arm["attempts"], arm["errors"], arm["tampered"]) == (0, 1, 0, 1),
              "an agent that edits the pristine copy must fail with tampering, not pass or error: %r" % arm)
        del_dir = os.path.join(tmp, "delete")
        shutil.copytree(EXAMPLE, del_dir)
        doc = {"suite": "delete-check", "version": 1, "repeats": 1, "tasks": [gtask], "arms": [
            {"id": "deleter", "agent": "cmd", "args": {"argv": ["python3", "{suite_dir}/agents/delete_grader.py",
                                                              "{suite_dir}/graders/unittest_grader.py"]}}]}
        with open(os.path.join(del_dir, "suite.json"), "w") as f:
            json.dump(doc, f)
        del_exp = "dd%s" % tag
        rc, out, err = sh([PY, BENCH, "start", os.path.join(del_dir, "suite.json"), "--experiment", del_exp, "--approve", "--seed", "1"], state=state)
        check(rc == 0, "start (delete): %s" % err[-300:])
        workers(1, tag + "d", state, runs, work, del_dir)
        arm = report(del_exp, state)["arms"][0]
        check((arm["passes"], arm["attempts"], arm["errors"], arm["tampered"]) == (0, 1, 0, 1),
              "an agent that deletes the grader's script must fail with tampering, not drop out as an error: %r" % arm)
        say("an_edited_pristine_copy_and_a_deleted_grader_script_are_failures_with_tampering")

        # 5c. four changed tasks in thirty
        four_dir = os.path.join(tmp, "four")
        shutil.copytree(EXAMPLE, four_dir)
        greet = [t for t in base_doc["tasks"] if t["id"] == "greet-hello"][0]
        tasks = []
        for i in range(30):
            t = json.loads(json.dumps(calc if i < 4 else greet))
            t["id"] = "t%02d" % i
            tasks.append(t)
        doc = {"suite": "four-of-thirty", "version": 1, "repeats": 1, "tasks": tasks, "arms": [
            {"id": "a", "agent": "cmd", "args": {"argv": ["python3", "{suite_dir}/agents/solver.py", "--skip", "calc"]}},
            {"id": "b", "agent": "cmd", "args": {"argv": ["python3", "{suite_dir}/agents/solver.py"]}}]}
        with open(os.path.join(four_dir, "suite.json"), "w") as f:
            json.dump(doc, f)
        four_exp = "d4%s" % tag
        rc, out, err = sh([PY, BENCH, "start", os.path.join(four_dir, "suite.json"), "--experiment", four_exp, "--approve", "--seed", "2"], state=state)
        check(rc == 0, "start (four): %s" % err[-300:])
        workers(3, tag + "f", state, runs, work, four_dir)
        c = report(four_exp, state)["comparisons"][0]
        check(c["table"] == {"both": 26, "a_only": 0, "b_only": 4, "neither": 0} and abs(c["mcnemar_p"] - 0.125) < 1e-12,
              "four changed tasks: table and p by hand: %r p=%r" % (c["table"], c["mcnemar_p"]))
        check(c["diff"]["lo"] > 0 and c["paired_tasks"] == 30, "the bootstrap alone excludes zero here")
        check(c["verdict"]["label"] == "inconclusive" and "does not reject" in c["verdict"]["text"],
              "the exact test must stop the verdict: %r" % c["verdict"])
        say("four_changed_tasks_in_thirty_are_inconclusive_because_the_exact_test_says_p_0_125")

        # 6. the zombie
        zdir = os.path.join(tmp, "zombie")
        shutil.copytree(EXAMPLE, zdir)
        zdoc = json.load(open(os.path.join(zdir, "suite.json")))
        zdoc["suite"] = "zombie-check"
        zdoc["repeats"] = 1
        zdoc["tasks"] = [t for t in zdoc["tasks"] if t["id"] == "calc-add"]
        zdoc["arms"] = zdoc["arms"][1:]
        with open(os.path.join(zdir, "suite.json"), "w") as f:
            json.dump(zdoc, f)
        zexp = "dz%s" % tag
        rc, out, err = sh([PY, BENCH, "start", os.path.join(zdir, "suite.json"), "--experiment", zexp, "--approve", "--seed", "1"], state=state)
        check(rc == 0, "start (zombie): %s" % err[-300:])
        zs, ls = os.path.join(state, "z1"), os.path.join(state, "z2")
        rc, tid, err = sh([BOARD, "next", "--owner", "zombie" + tag, "--lease", "120"], author="zombie" + tag, state=zs)
        check(rc == 0 and tid, "the zombie should claim the attempt: %s" % err[-200:])
        fence_z = open(os.path.join(zs, "zombie%s.%s" % (tag, tid))).read().strip()
        # The zombie loses the task (here: it gives it up, as an operator or a reap would take it away),
        # and keeps running with its old fence.
        rc, out, err = sh([BOARD, "release", tid, "--fence", fence_z], author="zombie" + tag, state=zs)
        check(rc == 0, "release by the first holder: %s" % err[-200:])
        rc, tid2, err = sh([BOARD, "next", "--owner", "live" + tag, "--lease", "120"], author="live" + tag, state=ls)
        check(rc == 0 and tid2 == tid, "the live worker should take the same task after the reap: %r vs %r (%s)" % (tid2, tid, err[-200:]))
        fence_l = open(os.path.join(ls, "live%s.%s" % (tag, tid))).read().strip()
        check(fence_l != fence_z, "a new claim has a new fence")
        rc, out, err = sh([PY, BENCH, "record", "--task", tid, "--fence", fence_z, "--experiment", zexp, "--arm", "candidate", "--bench-task", "calc-add", "--outcome", "pass", "--summary", "zombie"],
                          author="zombie" + tag, state=zs)
        check(rc == 3 and "lease lost" in err, "a zombie's record must be refused with exit 3: rc=%s %s" % (rc, err[-200:]))
        rep = report(zexp, state)
        check(rep["pending"] == 1 and rep["arms"][0]["attempts"] == 0, "the zombie wrote nothing")
        rc, out, err = sh([PY, BENCH, "record", "--task", tid, "--fence", fence_l, "--experiment", zexp, "--arm", "someone-else",
                           "--bench-task", "calc-add", "--outcome", "pass"], author="live" + tag, state=ls)
        check(rc != 0 and "nothing written" in err, "a result naming another attempt must be refused: rc=%s %s" % (rc, err[-200:]))
        check(report(zexp, state)["pending"] == 1, "the mismatched record wrote nothing")
        rc, out, err = sh([PY, BENCH, "record", "--task", tid, "--fence", fence_l, "--experiment", zexp, "--arm", "candidate", "--bench-task", "calc-add", "--outcome", "fail", "--summary", "live", "--cost", "0.0421",
                           "--duration-ms", "1500"], author="live" + tag, state=ls)
        check(rc == 0 and "recorded" in out, "the live worker's record: %s" % (out + err)[-200:])
        rc, out, err = sh([PY, BENCH, "record", "--task", tid, "--fence", fence_l, "--experiment", zexp, "--arm", "candidate", "--bench-task", "calc-add", "--outcome", "pass", "--summary", "again"],
                          author="live" + tag, state=ls)
        check(rc == 0 and "already recorded" in out, "a second record changes nothing: %s" % (out + err)[-200:])
        rep = report(zexp, state)
        arm = rep["arms"][0]
        check((arm["attempts"], arm["passes"]) == (1, 0), "the first record stands")
        check(abs(arm["cost"]["total"] - 0.0421) < 1e-9 and arm["duration"]["n"] == 1, "cost and duration came through the database")
        rc, out, err = sh([BOARD, "show", tid], state=state)
        check("fenced" in out, "the refused record left a fenced event")
        say("a_zombie_cannot_record_and_a_record_lands_once")

        # 7. the console
        sys.path.insert(0, os.path.join(ROOT, "integrations", "console"))
        from console.server import ConsoleServer
        from console.service import BoardService
        from console.source import PsqlSource
        src = PsqlSource("")
        svc = BoardService(src, ttl=0.0)
        server = ConsoleServer(svc, 0)
        t = threading.Thread(target=server.serve_forever, kwargs={"poll_interval": 0.05}, daemon=True)
        t.start()
        try:
            def get(path):
                # The alpha server drops a share of new connections (issue 867) and the console, unlike the
                # board helper, does not retry a connect: ask again, a few times, and print what it said.
                last = ""
                for _ in range(8):
                    try:
                        with urllib.request.urlopen("http://127.0.0.1:%d%s" % (server.port, path), timeout=60) as r:
                            return json.loads(r.read().decode())
                    except urllib.error.HTTPError as e:
                        last = e.read().decode()[:300]
                        time.sleep(2.5)  # past the console's own back-off after a failed read
                fail("the console kept answering with an error for %s: %s" % (path, last))
            lst = get("/api/experiments")
            names = {e["name"] for e in lst["experiments"]}
            check({exp, cheat_exp, zexp} <= names, "the console lists the experiments: %r" % names)
            web = get("/api/experiment?name=" + exp)
            cli = report(exp, state)
            for key in ("arms", "comparisons", "errors", "grid", "pending"):
                check(json.dumps(web[key], sort_keys=True) == json.dumps(cli[key], sort_keys=True), "the console and the command line disagree on %s" % key)
            check(web["comparisons"][0]["verdict"]["label"] == "inconclusive", "the console shows inconclusive")
            check(get("/api/experiment?name=" + zexp)["arms"][0]["cost"]["n"] == 1, "the console shows a recorded cost")
            SEEN.append(json.dumps(web))
        finally:
            server.shutdown()
            server.server_close()
        say("the_console_serves_the_same_numbers_as_the_report_command")

        # 8. no secret
        pw = os.environ.get("PGPASSWORD", "")
        if pw:
            check(not any(pw in s for s in SEEN), "the board account's password appeared in the output")
        say("no_password_in_the_output")
        print("bench-devstack: all checks passed", flush=True)
    finally:
        shutil.rmtree(tmp, True)


if __name__ == "__main__":
    main()

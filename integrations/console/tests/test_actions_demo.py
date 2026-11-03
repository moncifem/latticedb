"""Write mode in the demo, and the command line that turns it on.

The demo answers `board act` from the generated board in memory. It is a stand-in for trying the page, so these
tests pin that it speaks the helper's language (done / noop / refused CODE) and that it applies the same rules the
helper does (test_act.py is the specification of the helper).
"""

import contextlib
import io
import os
import re
import stat
import tempfile
import unittest
from unittest import mock

import helpers  # noqa: F401
from actions_helpers import REQ, Write

from console import __main__ as cli
from console import actions
from console.demo import DemoSource
from console.service import BoardService
from test_cli import run


class Demo(unittest.TestCase):
    def setUp(self):
        self.src = DemoSource()
        self.run_ = self.src.act_runner("human.alice")
        self.service = BoardService(self.src, ttl=0.0)

    def items(self, kind=None):
        return [i for i in self.service.inbox_payload()["items"] if kind in (None, i["kind"])]

    def approval(self):
        return self.items("approval")[0]

    def test_the_demo_board_has_something_in_every_kind_of_the_inbox(self):
        totals = self.service.inbox_payload()["totals"]
        for kind in ("approval", "dead", "quiet", "blocked"):
            self.assertGreaterEqual(totals[kind], 1, kind)

    def test_the_awaiting_task_has_a_body_to_read(self):
        self.assertIn("retry delay", self.approval()["body"])

    def test_the_board_stops_growing_so_event_ids_stay_in_order(self):
        self.assertEqual(self.src.reveal_every, 0)
        self.assertEqual(self.src.visible(), len(self.src.events))

    def test_an_approval_opens_the_task_and_records_an_event_by_the_person(self):
        a = self.approval()
        rc, out = self.run_(["act", "approve", a["id"], "--request-id", REQ, "--hash", a["sha"]])
        self.assertEqual(rc, 0)
        m = re.fullmatch(r"done ([0-9]+)\n", out)
        self.assertTrue(m, out)
        snap = self.service.snapshot()
        self.assertEqual(snap.by_id[a["id"]]["state"], "open")
        ev = [e for e in snap.events if e["id"] == int(m.group(1))][0]
        self.assertEqual((ev["author"], ev["kind"], ev["task_id"]), ("human.alice", "approve", a["id"]))
        self.assertIn("sha256:" + a["sha"], ev["body"])
        self.assertNotIn(a["id"], [i["id"] for i in self.items()])

    def test_the_same_request_id_is_a_noop_that_names_the_first_event(self):
        a = self.approval()
        _, first = self.run_(["act", "approve", a["id"], "--request-id", REQ, "--hash", a["sha"]])
        rc, again = self.run_(["act", "approve", a["id"], "--request-id", REQ, "--hash", a["sha"]])
        self.assertEqual((rc, again), (0, first.replace("done", "noop")))
        self.assertEqual(len([e for e in self.service.snapshot().events if e["kind"] == "approve"]), 1)

    def test_the_request_id_cannot_be_reused_for_another_decision(self):
        a = self.approval()
        self.run_(["act", "approve", a["id"], "--request-id", REQ, "--hash", a["sha"]])
        self.assertEqual(self.run_(["act", "cancel", a["id"], "--request-id", REQ]), (5, "refused request_reused\n"))

    def test_refusals_use_the_helpers_codes(self):
        a = self.approval()
        dead = self.items("dead")[0]
        cases = [
            (["act", "approve", a["id"], "--request-id", "req-aaaaaaaa", "--hash", "0" * 64], "stale"),
            (["act", "approve", a["id"], "--request-id", "req-bbbbbbbb"], "hash_required"),
            (["act", "reject", a["id"], "--request-id", "req-cccccccc"], "comment_required"),
            (["act", "note", a["id"], "--request-id", "req-dddddddd"], "comment_required"),
            (["act", "approve", "tnonexistent", "--request-id", "req-eeeeeeee", "--hash", "0" * 64], "not_found"),
            (["act", "approve", dead["id"], "--request-id", "req-ffffffff", "--hash", dead["sha"]], "wrong_state"),
            (["act", "requeue", a["id"], "--request-id", "req-gggggggg"], "wrong_state"),
        ]
        for args, code in cases:
            self.assertEqual(self.run_(args), (5, "refused %s\n" % code), args)
        self.assertEqual(self.items("approval")[0]["id"], a["id"])  # nothing changed

    def test_the_person_who_made_the_task_cannot_approve_or_reject_it(self):
        a = self.approval()
        for name in (a["created_by"], "human." + a["created_by"]):
            runner = self.src.act_runner("human." + name.replace("human.", ""))
            self.assertEqual(runner(["act", "approve", a["id"], "--request-id", "req-selfself1", "--hash", a["sha"]]),
                             (5, "refused self_approval\n"))
            self.assertEqual(runner(["act", "reject", a["id"], "--request-id", "req-selfself2", "--comment", "x"]),
                             (5, "refused self_approval\n"))

    def test_an_agent_that_wrote_on_the_task_is_the_same_person_as_its_name(self):
        a = self.approval()
        (ev,) = [e for e in self.src.events if e["task_id"] == a["id"]][:1]
        self.assertEqual(ev["author"], "maintainer")

    def test_requeue_brings_a_dead_task_back_with_its_attempts_reset(self):
        dead = self.items("dead")[0]
        self.assertEqual(self.run_(["act", "requeue", dead["id"], "--request-id", REQ])[0], 0)
        snap = self.service.snapshot()
        self.assertEqual(snap.by_id[dead["id"]]["state"], "open")
        lease = [r for r in snap.leases if r["task_id"] == dead["id"]][0]
        self.assertEqual((lease["attempts"], lease["last_error"]), (0, ""))

    def test_cancel_and_reject_cancel_the_task(self):
        a = self.approval()
        self.run_(["act", "reject", a["id"], "--request-id", REQ, "--comment", "no thanks"])
        self.assertEqual(self.service.snapshot().by_id[a["id"]]["state"], "cancelled")
        dead = self.items("dead")[0]
        self.assertEqual(self.run_(["act", "cancel", dead["id"], "--request-id", "req-cancel-000"]), (5, "refused wrong_state\n"))
        quiet = self.items("quiet")[0]  # a claimed task can be cancelled
        self.assertEqual(self.run_(["act", "cancel", quiet["id"], "--request-id", "req-cancel-001"])[0], 0)
        snap = self.service.snapshot().by_id[quiet["id"]]
        self.assertEqual((snap["state"], snap["owner"]), ("cancelled", None))

    def test_priority_changes_the_number_and_note_adds_an_event_only(self):
        dead = self.items("dead")[0]
        self.run_(["act", "priority", dead["id"], "--request-id", "req-prio-0001", "--priority", "9"])
        self.assertEqual(self.service.snapshot().by_id[dead["id"]]["priority"], 9)
        before = self.service.snapshot().by_id[dead["id"]]["state"]
        rc, out = self.run_(["act", "note", dead["id"], "--request-id", "req-note-0001", "--comment", "seen"])
        self.assertEqual(rc, 0)
        self.assertEqual(self.service.snapshot().by_id[dead["id"]]["state"], before)
        self.assertEqual(self.service.snapshot().events[-1]["body"], "seen")

    def test_it_answers_gibberish_with_a_failure_not_a_traceback(self):
        for args in ([], ["act"], ["act", "approve"], ["act", "nuke", "t1", "--request-id", REQ], ["act", "cancel", "t1", "--request-id"],
                     ["act", "priority", "t1", "--request-id", REQ, "--priority", "many"], ["status"]):
            rc, out = self.run_(args)
            self.assertIn(rc, (1, 5), args)

    def test_a_runner_for_a_name_without_the_human_prefix_is_refused(self):
        r = self.src.act_runner("alice")
        self.assertEqual(r(["act", "cancel", "t1", "--request-id", REQ]), (1, ""))

    def test_the_whole_path_through_the_actions_object(self):
        acts = actions.Actions(self.service, "human.alice", "k" * 40, self.src.act_runner("human.alice"))
        a = self.approval()
        status, ans = acts.act({"action": "approve", "task": a["id"], "request_id": REQ, "sha": a["sha"]})
        self.assertEqual(status, 200)
        self.assertTrue(ans["ok"])
        status, ans = acts.act({"action": "approve", "task": a["id"], "request_id": "req-second-01", "sha": a["sha"]})
        self.assertEqual((status, ans["code"]), (409, "wrong_state"))


class OverHttp(unittest.TestCase):
    def test_a_demo_server_in_write_mode(self):
        src = DemoSource()
        service = BoardService(src, ttl=0.0)
        acts = actions.Actions(service, "human.alice", "z" * 40, src.act_runner("human.alice"))
        from console.server import ConsoleServer
        import threading
        server = ConsoleServer(service, 0, actions=acts)
        t = threading.Thread(target=server.serve_forever, kwargs={"poll_interval": 0.02}, daemon=True)
        t.start()
        self.addCleanup(lambda: (server.shutdown(), server.server_close(), t.join(5)))
        w = Write.__new__(Write)
        w.port = server.port
        w.source, w.service, w.actions, w.server = src, service, acts, server
        a = service.inbox_payload()["items"][0]
        status, d, _ = w.post("/api/act", {"action": "approve", "task": a["id"], "request_id": REQ, "sha": a["sha"]},
                              headers={actions.TOKEN_HEADER: "z" * 40})
        self.assertEqual(status, 200, d)
        self.assertTrue(w.get_json("/api/now")[1]["meta"]["demo"])
        self.assertNotIn(a["id"], [i["id"] for i in w.get_json("/api/inbox")[1]["items"]])


class CommandLine(unittest.TestCase):
    def start(self, argv, env=None):
        captured = {}

        def fake_forever(self_):
            captured["server"] = self_

        with mock.patch("console.server.ConsoleServer.serve_forever", fake_forever):
            rc, out, err = run(argv, env)
        if "server" in captured:
            self.addCleanup(captured["server"].server_close)
        return rc, out, err, captured.get("server")

    def test_writes_are_off_by_default(self):
        rc, out, err, srv = self.start(["serve", "--demo", "--port", "0"])
        self.assertIsNone(srv.actions)
        self.assertIsNone(srv.service.writes)
        self.assertIn("read-only", out)
        self.assertNotIn("token", out.lower())

    def test_the_flag_turns_on_a_gate_and_prints_the_token_once(self):
        rc, out, err, srv = self.start(["serve", "--demo", "--port", "0", "--allow-writes", "--actor", "alice"])
        self.assertEqual(rc, 0, err)
        self.assertEqual(srv.service.writes, {"enabled": True, "actor": "human.alice"})
        tokens = re.findall(r"write token \(shown once, paste it into the page\): (\S+)", out)
        self.assertEqual(len(tokens), 1)
        token = tokens[0]
        self.assertGreaterEqual(len(token), 40)
        self.assertEqual(out.count(token), 1)  # nowhere else on the terminal
        self.assertNotIn(token, err)
        self.assertIsNone(srv.actions.gate.check(token))
        self.assertIn("writes ON as human.alice", out)
        self.assertIn("simulated", out)  # the demo says so

    def test_every_start_makes_a_new_token(self):
        seen = set()
        for _ in range(3):
            rc, out, err, srv = self.start(["serve", "--demo", "--port", "0", "--allow-writes"])
            seen.update(re.findall(r"paste it into the page\): (\S+)", out))
        self.assertEqual(len(seen), 3)

    def test_the_token_is_not_in_the_url_line(self):
        rc, out, err, srv = self.start(["serve", "--demo", "--port", "0", "--allow-writes"])
        token = re.search(r"paste it into the page\): (\S+)", out).group(1)
        first = out.splitlines()[0]
        self.assertNotIn(token, first)

    def test_the_default_person_is_the_os_user(self):
        with mock.patch("getpass.getuser", return_value="moncif"):
            rc, out, err, srv = self.start(["serve", "--demo", "--port", "0", "--allow-writes"])
        self.assertEqual(srv.service.writes["actor"], "human.moncif")

    def test_a_bad_actor_is_refused(self):
        rc, out, err, srv = self.start(["serve", "--demo", "--port", "0", "--allow-writes", "--actor", "a b"])
        self.assertEqual(rc, 2)
        self.assertIn("--actor", err)

    def test_actor_and_board_mean_nothing_without_the_flag(self):
        for extra in (["--actor", "alice"], ["--board", "/bin/true"]):
            rc, out, err, srv = self.start(["serve", "--demo", "--port", "0"] + extra)
            self.assertEqual(rc, 2, extra)
            self.assertIn("--allow-writes", err)

    def test_a_real_source_needs_an_executable_helper(self):
        d = tempfile.mkdtemp()
        self.addCleanup(__import__("shutil").rmtree, d, True)
        missing = os.path.join(d, "board")
        rc, out, err, srv = self.start(["serve", "--port", "0", "--allow-writes", "--board", missing])
        self.assertEqual(rc, 2)
        self.assertIn("not an executable", err)
        with open(missing, "w") as f:
            f.write("#!/bin/sh\n")
        os.chmod(missing, 0o755)
        rc, out, err, srv = self.start(["serve", "--port", "0", "--allow-writes", "--board", missing, "--actor", "bob"])
        self.assertEqual(rc, 0, err)
        self.assertIsInstance(srv.actions.runner, actions.HelperRunner)
        self.assertEqual(srv.actions.runner.env["BOARD_AUTHOR"], "human.bob")
        self.assertNotIn("simulated", out)

    def test_the_connection_url_never_reaches_the_terminal_in_write_mode(self):
        d = tempfile.mkdtemp()
        self.addCleanup(__import__("shutil").rmtree, d, True)
        path = os.path.join(d, "board")
        with open(path, "w") as f:
            f.write("#!/bin/sh\n")
        os.chmod(path, 0o755)
        rc, out, err, srv = self.start(["serve", "--port", "0", "--allow-writes", "--board", path,
                                        "--url", "postgresql://dev@127.0.0.1:5999/boarddb"], {"PGPASSWORD": "pw-secret-77"})
        self.assertEqual(rc, 0, err)
        for text in (out, err):
            for secret in ("5999", "boarddb", "pw-secret-77"):
                self.assertNotIn(secret, text)


if __name__ == "__main__":
    unittest.main()

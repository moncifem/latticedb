"""Write mode, unit level: the hash, the identity, the token gate, request parsing, the helper's three-line language,
the request-id memory and the bulk rules. The HTTP layer is test_actions_server.py.

Expected values are written out from the rules (README, "Write mode"), never read back from the code: the hashes
were computed with `printf ... | shasum -a 256`.
"""

import os
import stat
import tempfile
import unittest
from unittest import mock

import helpers  # noqa: F401
from actions_helpers import ACTOR, REQ, TOKEN, FakeRunner, board

from console import actions
from console.actions import Actions, BadAction, WriteGate
from console.service import BoardService
from console.source import ConfigError


class Hash(unittest.TestCase):
    def test_known_vectors(self):
        self.assertEqual(actions.task_sha("Raise the retry delay", "Proposed: wait 30 s, not 5 s."),
                         "06b004e02f7816e31814914e79b237bc3fa0f5e163ad859376ffc8bdc5dff4e5")
        self.assertEqual(actions.task_sha("T", ""), "678f81a714fbc72030f82f9980054d5cf90e6f041a367f7da2f35b0f7dafb0e5")

    def test_title_and_body_cannot_trade_text(self):
        self.assertNotEqual(actions.task_sha("a", "b"), actions.task_sha("a\nb", ""))  # the separator is one newline
        self.assertNotEqual(actions.task_sha("ab", ""), actions.task_sha("a", "b"))

    def test_unicode_is_hashed_as_utf8(self):
        import hashlib
        self.assertEqual(actions.task_sha("\u2615", "x"), hashlib.sha256(b"\xe2\x98\x95\nx").hexdigest())


class Identity(unittest.TestCase):
    def test_a_name_gets_the_human_prefix(self):
        self.assertEqual(actions.actor_identity("alice"), "human.alice")
        self.assertEqual(actions.actor_identity("a.b_c-d"), "human.a.b_c-d")

    def test_the_default_is_the_os_user_made_safe(self):
        with mock.patch("getpass.getuser", return_value="Moncif El M!"):
            self.assertEqual(actions.default_actor_name(), "Moncif_El_M_")
        with mock.patch("getpass.getuser", side_effect=OSError):
            self.assertEqual(actions.default_actor_name(), "user")
        with mock.patch("getpass.getuser", return_value="x" * 100):
            self.assertEqual(len(actions.default_actor_name()), 58)
            self.assertEqual(len(actions.actor_identity(None)), 64)

    def test_bad_names_are_refused(self):
        for bad in ("a b", "a/b", "a;b", "x" * 59, "hé", "a\nb"):
            with self.assertRaises(ConfigError, msg=bad):
                actions.actor_identity(bad)

    def test_the_two_names_the_helper_treats_as_the_actor(self):
        self.assertEqual(actions.produced_by("human.alice"), ("human.alice", "alice"))


class Gate(unittest.TestCase):
    def setUp(self):
        self.t = [0.0]
        self.gate = WriteGate(TOKEN, lambda: self.t[0])

    def test_the_right_token_passes_and_a_wrong_one_does_not(self):
        self.assertIsNone(self.gate.check(TOKEN))
        for bad in (None, "", "x", TOKEN + "x", TOKEN[:-1], TOKEN.upper(), 5, b"bytes"):
            self.assertEqual(WriteGate(TOKEN, lambda: 0.0).check(bad), "bad_token", repr(bad))  # a fresh gate: no lockout yet

    def test_the_comparison_is_constant_time(self):
        with mock.patch.object(actions.hmac, "compare_digest", wraps=actions.hmac.compare_digest) as cd:
            self.gate.check(TOKEN)
            self.gate.check("wrong")
            self.gate.check(None)
        self.assertEqual(cd.call_count, 3)
        for call in cd.call_args_list:
            self.assertTrue(all(isinstance(a, bytes) for a in call.args))  # bytes, so any text is comparable

    def test_non_ascii_text_cannot_crash_the_comparison(self):
        self.assertEqual(self.gate.check("té" * 20), "bad_token")

    def test_five_wrong_tokens_lock_everything_for_a_minute(self):
        for _ in range(4):
            self.assertEqual(self.gate.check("no"), "bad_token")
        self.assertEqual(self.gate.check("no"), "bad_token")  # the fifth: it locks
        self.assertEqual(self.gate.check(TOKEN), "locked")  # even the right token
        self.t[0] = 59.0
        self.assertEqual(self.gate.check(TOKEN), "locked")
        self.t[0] = 61.0
        self.assertIsNone(self.gate.check(TOKEN))

    def test_wrong_tokens_far_apart_do_not_lock(self):
        for i in range(10):
            self.t[0] = i * 61.0
            self.assertEqual(self.gate.check("no"), "bad_token")
        self.assertIsNone(self.gate.check(TOKEN))

    def test_a_good_token_clears_the_count(self):
        for _ in range(4):
            self.gate.check("no")
        self.assertIsNone(self.gate.check(TOKEN))
        for _ in range(4):
            self.assertEqual(self.gate.check("no"), "bad_token")
        self.assertIsNone(self.gate.check(TOKEN))

    def test_sixty_decisions_a_minute_and_no_more(self):
        for _ in range(60):
            self.assertIsNone(self.gate.check(TOKEN))
        self.assertEqual(self.gate.check(TOKEN), "rate")
        self.t[0] = 61.0
        self.assertIsNone(self.gate.check(TOKEN))

    def test_a_short_token_is_not_accepted_at_all(self):
        with self.assertRaises(ValueError):
            WriteGate("short")

    def test_new_tokens_are_long_and_different(self):
        a, b = actions.new_token(), actions.new_token()
        self.assertNotEqual(a, b)
        self.assertGreaterEqual(len(a), 40)


GOOD = {"action": "cancel", "task": "topen", "request_id": REQ}


def bad(**kw):
    body = dict(GOOD)
    body.update(kw)
    return body


class ParseAct(unittest.TestCase):
    def ok(self, body):
        return actions.parse_act(body)

    def refuses(self, body, fragment=""):
        with self.assertRaises(BadAction) as cm:
            actions.parse_act(body)
        self.assertIn(fragment, str(cm.exception))

    def test_every_action_in_its_valid_form(self):
        sha = "a" * 64
        cases = [
            {"action": "approve", "task": "t1", "request_id": REQ, "sha": sha},
            {"action": "reject", "task": "t1", "request_id": REQ, "comment": "no"},
            {"action": "cancel", "task": "t1", "request_id": REQ},
            {"action": "requeue", "task": "t1", "request_id": REQ, "comment": "again"},
            {"action": "priority", "task": "t1", "request_id": REQ, "priority": -3},
            {"action": "note", "task": "t1", "request_id": REQ, "comment": "hello"},
        ]
        for c in cases:
            self.assertEqual(self.ok(c)["action"], c["action"])

    def test_the_action_list_is_closed(self):
        for a in ("delete", "approve_all", "Approve", "", None, 5, ["approve"], "approve ", "bulk_approve", "done"):
            self.refuses(bad(action=a), "action must be one of")
        self.refuses({"task": "t1", "request_id": REQ}, "action must be one of")

    def test_not_an_object(self):
        for body in ([], "approve", 5, None, [GOOD]):
            self.refuses(body, "JSON object")

    def test_unknown_fields_are_refused_per_action(self):
        self.refuses(bad(extra=1), "unknown field")
        self.refuses(bad(sha="a" * 64), "unknown field")  # sha belongs to approve only
        self.refuses(bad(priority=1), "unknown field")  # priority belongs to the priority action only
        self.refuses({"action": "approve", "task": "t1", "request_id": REQ, "sha": "a" * 64, "author": "x"}, "unknown field")

    def test_task_ids_are_matched_in_full(self):
        for t in ("", "a b", "a;b", "t1\n", "x" * 65, "é", "../t", None, 5, ["t"]):
            with self.assertRaises(BadAction, msg=repr(t)):
                actions.parse_act(bad(task=t))
        for t in ("t1", "a-b_C", "x" * 64):
            actions.parse_act(bad(task=t))

    def test_request_ids_are_8_to_64_characters(self):
        for r in ("short", "x" * 65, "has space1", "semi;colon", "new\nline1234", None, 12345678, ""):
            with self.assertRaises(BadAction, msg=repr(r)):
                actions.parse_act(bad(request_id=r))
        actions.parse_act(bad(request_id="12345678"))
        actions.parse_act(bad(request_id="x" * 64))

    def test_an_approval_needs_the_64_hex_digits_that_were_shown(self):
        base = {"action": "approve", "task": "t1", "request_id": REQ}
        self.refuses(base, "sha is required")
        for s in ("a" * 63, "a" * 65, "A" * 64, "g" * 64, "a" * 63 + "\n", None, 5):
            self.refuses(dict(base, sha=s), "sha")

    def test_reject_and_note_need_a_comment(self):
        for a in ("reject", "note"):
            self.refuses({"action": a, "task": "t1", "request_id": REQ}, "comment is required")
            self.refuses({"action": a, "task": "t1", "request_id": REQ, "comment": "   "}, "comment is required")
            self.refuses({"action": a, "task": "t1", "request_id": REQ, "comment": ""}, "comment is required")

    def test_comments_are_one_clean_line(self):
        self.assertEqual(actions.parse_act(bad(comment="a  b\n c\t d"))["comment"], "a b c d")
        for c in ("back\\slash", "bell\x07", "nul\x00", "esc\x1b[0m", "‮evil", "zero​width", 5, ["x"]):
            with self.assertRaises(BadAction, msg=repr(c)):
                actions.parse_act(bad(comment=c))
        with self.assertRaises(BadAction):
            actions.parse_act(bad(comment="x" * 1001))
        self.assertEqual(len(actions.parse_act(bad(comment="x" * 1000))["comment"]), 1000)
        # 1000 four-byte characters are 4000 bytes: the most a comment can be, so the helper's 4096-byte limit
        # still holds after the words it puts in front ("approved content sha256:<64 hex>: ").
        self.assertEqual(len(actions.parse_act(bad(comment="\U0001f600" * 1000))["comment"]), 1000)

    def test_priority_is_a_whole_number_in_range(self):
        base = {"action": "priority", "task": "t1", "request_id": REQ}
        for p in (True, 1.5, "3", None, 1_000_001, -1_000_001, [1]):
            with self.assertRaises(BadAction, msg=repr(p)):
                actions.parse_act(dict(base, priority=p))
        with self.assertRaises(BadAction):
            actions.parse_act(base)
        self.assertEqual(actions.parse_act(dict(base, priority=-1_000_000))["priority"], -1_000_000)


class ParseBulk(unittest.TestCase):
    def refuses(self, body, fragment=""):
        with self.assertRaises(BadAction) as cm:
            actions.parse_bulk(body)
        self.assertIn(fragment, str(cm.exception))

    def test_the_bulk_list_is_cancel_and_requeue_and_never_approve(self):
        self.assertEqual(actions.BULK_ACTIONS, ("cancel", "requeue"))
        for a in ("approve", "reject", "priority", "note", "approve_all", None):
            self.refuses({"action": a, "ids": ["t1"], "dry_run": True}, "bulk action")

    def test_a_dry_run_takes_only_the_list(self):
        r = actions.parse_bulk({"action": "cancel", "ids": ["t1", "t2"], "dry_run": True})
        self.assertEqual((r["ids"], r["dry_run"]), (["t1", "t2"], True))
        self.refuses({"action": "cancel", "ids": ["t1"], "dry_run": True, "confirm": 1}, "dry run takes no")
        self.refuses({"action": "cancel", "ids": ["t1"], "dry_run": True, "request_id": REQ}, "dry run takes no")

    def test_the_real_run_needs_a_request_id_and_the_count_the_dry_run_gave(self):
        self.refuses({"action": "cancel", "ids": ["t1"], "dry_run": False}, "request_id is required")
        self.refuses({"action": "cancel", "ids": ["t1"], "dry_run": False, "request_id": REQ}, "confirm")
        self.refuses({"action": "cancel", "ids": ["t1"], "dry_run": False, "request_id": REQ, "confirm": True}, "confirm")
        self.refuses({"action": "cancel", "ids": ["t1"], "dry_run": False, "request_id": REQ, "confirm": 26}, "confirm")
        r = actions.parse_bulk({"action": "requeue", "ids": ["t1"], "dry_run": False, "request_id": REQ, "confirm": 1})
        self.assertEqual(r["confirm"], 1)

    def test_dry_run_must_be_a_real_boolean(self):
        for d in (None, 0, 1, "true", "false", []):
            self.refuses({"action": "cancel", "ids": ["t1"], "dry_run": d}, "dry_run")
        self.refuses({"action": "cancel", "ids": ["t1"]}, "dry_run")

    def test_the_id_list(self):
        self.refuses({"action": "cancel", "ids": [], "dry_run": True}, "ids")
        self.refuses({"action": "cancel", "ids": "t1", "dry_run": True}, "ids")
        self.refuses({"action": "cancel", "ids": ["t%d" % i for i in range(26)], "dry_run": True}, "ids")
        self.refuses({"action": "cancel", "ids": ["t1", "t1"], "dry_run": True}, "twice")
        self.refuses({"action": "cancel", "ids": ["t1;"], "dry_run": True}, "task id")
        self.refuses({"action": "cancel", "ids": [5], "dry_run": True}, "task id")
        actions.parse_bulk({"action": "cancel", "ids": ["t%d" % i for i in range(25)], "dry_run": True})

    def test_unknown_fields(self):
        self.refuses({"action": "cancel", "ids": ["t1"], "dry_run": True, "sha": "a" * 64}, "unknown field")


class HelperLanguage(unittest.TestCase):
    def test_arguments_for_each_action(self):
        self.assertEqual(actions.helper_args({"action": "cancel", "task": "t1", "comment": ""}, "rid12345"),
                         ["act", "cancel", "t1", "--request-id", "rid12345"])
        self.assertEqual(actions.helper_args({"action": "approve", "task": "t1", "sha": "a" * 64, "comment": "ok"}, "rid12345"),
                         ["act", "approve", "t1", "--request-id", "rid12345", "--hash", "a" * 64, "--comment", "ok"])
        self.assertEqual(actions.helper_args({"action": "priority", "task": "t1", "priority": -4, "comment": ""}, "rid12345"),
                         ["act", "priority", "t1", "--request-id", "rid12345", "--priority", "-4"])

    def test_results(self):
        self.assertEqual(actions.parse_result(0, "done 12\n"), ("ok", 12, False))
        self.assertEqual(actions.parse_result(0, "noop 7\n"), ("ok", 7, True))
        self.assertEqual(actions.parse_result(5, "refused stale\n"), ("refused", "stale"))

    def test_anything_else_is_a_failure(self):
        for rc, out in ((1, "done 12\n"), (0, ""), (0, "done twelve\n"), (0, "done 12 extra\n"), (5, "done 12\n"),
                        (0, "refused stale\n"), (5, "refused STALE\n"), (5, "refused \n"), (2, "refused stale\n"),
                        (0, "ok\n"), (0, "done 1234567890123456789\n"), (0, "\ndone 12\n")):
            self.assertEqual(actions.parse_result(rc, out), ("failed",), (rc, out))

    def test_only_the_first_line_counts(self):
        self.assertEqual(actions.parse_result(0, "done 12\nPASSWORD=hunter2\n"), ("ok", 12, False))


class Decisions(unittest.TestCase):
    def setUp(self):
        self.runner = FakeRunner()
        self.service = BoardService(board(), ttl=0.0)
        self.invalidations = []
        real = self.service.invalidate
        self.service.invalidate = lambda: (self.invalidations.append(1), real())[1]
        self.acts = Actions(self.service, ACTOR, TOKEN, self.runner)

    def test_a_decision_runs_the_helper_once_and_answers_with_the_event(self):
        status, a = self.acts.act({"action": "cancel", "task": "topen", "request_id": REQ})
        self.assertEqual(status, 200)
        self.assertEqual((a["ok"], a["event_id"], a["repeated"], a["actor"]), (True, 901, False, ACTOR))
        self.assertIn("cancelled topen", a["message"])
        self.assertIn("event 901", a["message"])
        self.assertEqual(self.runner.calls, [["act", "cancel", "topen", "--request-id", REQ]])
        self.assertEqual(len(self.invalidations), 1)  # the next read comes from the board

    def test_the_same_request_id_runs_the_helper_once(self):
        first = self.acts.act({"action": "cancel", "task": "topen", "request_id": REQ})
        again = self.acts.act({"action": "cancel", "task": "topen", "request_id": REQ})
        self.assertEqual(len(self.runner.calls), 1)
        self.assertEqual(again[0], 200)
        self.assertTrue(again[1]["repeated"])
        self.assertEqual(again[1]["event_id"], first[1]["event_id"])

    def test_the_same_request_id_for_another_decision_is_refused_without_the_helper(self):
        self.acts.act({"action": "cancel", "task": "topen", "request_id": REQ})
        for other in ({"action": "cancel", "task": "tdead", "request_id": REQ},
                      {"action": "note", "task": "topen", "request_id": REQ, "comment": "x"},
                      {"action": "cancel", "task": "topen", "request_id": REQ, "comment": "different"}):
            status, a = self.acts.act(other)
            self.assertEqual((status, a["code"]), (409, "request_reused"), other)
        self.assertEqual(len(self.runner.calls), 1)

    def test_a_refusal_is_not_remembered_so_the_person_can_try_again(self):
        answers = iter([(5, "refused stale\n"), (0, "done 5\n")])
        self.runner.answer = lambda args: next(answers)
        body = {"action": "approve", "task": "tapprove", "request_id": REQ, "sha": "a" * 64}
        self.assertEqual(self.acts.act(body)[0], 409)
        self.assertEqual(self.acts.act(body)[0], 200)
        self.assertEqual(len(self.runner.calls), 2)

    def test_each_refusal_code_has_its_status_and_a_fixed_sentence(self):
        want = {"self_approval": 403, "stale": 409, "wrong_state": 409, "changed": 409, "not_found": 404,
                "request_reused": 409, "comment_required": 400, "hash_required": 400, "unreadable": 409}
        for code, status in want.items():
            self.runner.answer = lambda args, c=code: (5, "refused %s\n" % c)
            s, a = self.acts.act({"action": "cancel", "task": "topen", "request_id": "req-%s-xxxx" % code[:6]})
            self.assertEqual((s, a["code"], a["ok"]), (status, code, False))
            self.assertEqual(a["error"], actions.REFUSALS[code][1])

    def test_an_unknown_refusal_code_is_a_plain_failure(self):
        self.runner.answer = lambda args: (5, "refused made_up_code\n")
        s, a = self.acts.act({"action": "cancel", "task": "topen", "request_id": REQ})
        self.assertEqual((s, a["code"]), (502, "failed"))

    def test_helper_output_is_never_passed_on(self):
        secret = "PASSWORD=hunter2 psql: error: connection to 10.1.2.3 failed"
        for rc, out in ((1, secret), (0, secret), (5, "refused stale\n" + secret), (0, "done 5\n" + secret)):
            self.runner.answer = lambda args, r=rc, o=out: (r, o)
            s, a = self.acts.act({"action": "cancel", "task": "topen", "request_id": "req-%d-%d-xx" % (rc, len(out))})
            self.assertNotIn("hunter2", repr(a))
            self.assertNotIn("10.1.2.3", repr(a))

    def test_a_failure_is_a_502_and_is_not_remembered(self):
        self.runner.answer = lambda args: (1, "")
        s, a = self.acts.act({"action": "cancel", "task": "topen", "request_id": REQ})
        self.assertEqual((s, a["code"], a["error"]), (502, "failed", actions.FAILED[1]))
        self.runner.answer = lambda args: (0, "done 9\n")
        self.assertEqual(self.acts.act({"action": "cancel", "task": "topen", "request_id": REQ})[0], 200)
        self.assertEqual(len(self.runner.calls), 2)

    def test_a_bad_request_never_reaches_the_helper(self):
        for body in ({}, {"action": "approve", "task": "t1", "request_id": REQ}, {"action": "nuke"}, [], "x"):
            self.assertEqual(self.acts.act(body)[0], 400)
        self.assertEqual(self.runner.calls, [])

    def test_the_memory_is_bounded(self):
        for i in range(actions.SEEN_MAX + 20):
            self.acts.act({"action": "cancel", "task": "topen", "request_id": "req-%08d" % i})
        self.assertLessEqual(len(self.acts._seen), actions.SEEN_MAX)


class Bulk(unittest.TestCase):
    def setUp(self):
        self.runner = FakeRunner()
        self.service = BoardService(board(), ttl=0.0)
        self.acts = Actions(self.service, ACTOR, TOKEN, self.runner)

    def test_a_dry_run_counts_what_would_change_and_writes_nothing(self):
        s, a = self.acts.bulk({"action": "cancel", "ids": ["topen", "tdead", "nope", "tclaimed"], "dry_run": True})
        self.assertEqual(s, 200)
        self.assertEqual(a["count"], 2)  # topen and tclaimed; a dead task cannot be cancelled
        self.assertEqual(sorted(x["id"] for x in a["eligible"]), ["tclaimed", "topen"])
        self.assertEqual(sorted((x["id"], x["reason"]) for x in a["skipped"]),
                         [("nope", "not on the board"), ("tdead", "it is dead")])
        self.assertEqual(self.runner.calls, [])

    def test_requeue_applies_to_dead_and_claimed_tasks(self):
        s, a = self.acts.bulk({"action": "requeue", "ids": ["topen", "tdead", "tclaimed"], "dry_run": True})
        self.assertEqual(sorted(x["id"] for x in a["eligible"]), ["tclaimed", "tdead"])

    def test_the_real_run_needs_the_count_the_dry_run_reported(self):
        s, a = self.acts.bulk({"action": "cancel", "ids": ["topen", "tclaimed"], "dry_run": False, "request_id": REQ, "confirm": 1})
        self.assertEqual((s, a["code"]), (409, "count_changed"))
        self.assertEqual(self.runner.calls, [])

    def test_the_real_run_writes_one_decision_per_task_with_its_own_request_id(self):
        s, a = self.acts.bulk({"action": "cancel", "ids": ["topen", "tclaimed", "tdead"], "dry_run": False,
                               "request_id": REQ, "confirm": 2, "comment": "cleanup"})
        self.assertEqual(s, 200)
        self.assertEqual((a["ok"], a["done"]), (True, 2))
        self.assertEqual([c[2] for c in self.runner.calls], ["topen", "tclaimed"])
        rids = [c[c.index("--request-id") + 1] for c in self.runner.calls]
        self.assertEqual(len(set(rids)), 2)
        for r in rids:
            self.assertRegex(r, r"[0-9a-f]{40}")
        self.assertTrue(all("--comment" in c and c[c.index("--comment") + 1] == "cleanup" for c in self.runner.calls))

    def test_repeating_the_same_bulk_request_does_not_write_again(self):
        body = {"action": "cancel", "ids": ["topen", "tclaimed"], "dry_run": False, "request_id": REQ, "confirm": 2}
        self.acts.bulk(body)
        s, a = self.acts.bulk(body)
        self.assertEqual(len(self.runner.calls), 2)
        self.assertTrue(all(r["repeated"] for r in a["results"]))

    def test_one_refusal_does_not_stop_the_others_and_is_reported(self):
        def answer(args):
            return (5, "refused changed\n") if args[2] == "topen" else (0, "done 3\n")
        self.runner.answer = answer
        s, a = self.acts.bulk({"action": "cancel", "ids": ["topen", "tclaimed"], "dry_run": False, "request_id": REQ, "confirm": 2})
        self.assertEqual((a["ok"], a["done"]), (False, 1))
        self.assertEqual([(r["id"], r["ok"], r["code"]) for r in a["results"]],
                         [("topen", False, "changed"), ("tclaimed", True, None)])

    def test_there_is_no_bulk_approve(self):
        s, a = self.acts.bulk({"action": "approve", "ids": ["tapprove"], "dry_run": True})
        self.assertEqual(s, 400)
        s, a = self.acts.bulk({"action": "approve", "ids": ["tapprove"], "dry_run": False, "request_id": REQ, "confirm": 1})
        self.assertEqual(s, 400)
        self.assertEqual(self.runner.calls, [])


class Runner(unittest.TestCase):
    """HelperRunner against a real (fake) executable: no shell, the right person, nothing leaks."""

    def setUp(self):
        self.d = tempfile.mkdtemp(prefix="board-runner-")
        self.addCleanup(__import__("shutil").rmtree, self.d, True)
        self.path = os.path.join(self.d, "board")
        self.out = os.path.join(self.d, "seen")

    @staticmethod
    def read(path):
        with open(path) as f:
            return f.read()

    def script(self, body):
        with open(self.path, "w") as f:
            f.write("#!/bin/sh\n" + body)
        os.chmod(self.path, os.stat(self.path).st_mode | stat.S_IXUSR)

    def test_it_passes_the_arguments_untouched_and_runs_as_the_person(self):
        self.script('printf "%%s|" "$@" > "%s"\nprintf "%%s" "$BOARD_AUTHOR" > "%s.author"\necho "done 4"\n' % (self.out, self.out))
        r = actions.HelperRunner(self.path, "human.alice", env={"PATH": "/usr/bin:/bin", "BOARD_AUTHOR": "evil"})
        rc, out = r(["act", "note", "t1", "--request-id", "abcdefgh", "--comment", "a; rm -rf / `x` $(y) 'q'"])
        self.assertEqual((rc, out), (0, "done 4\n"))
        self.assertEqual(self.read(self.out), "act|note|t1|--request-id|abcdefgh|--comment|a; rm -rf / `x` $(y) 'q'|")
        self.assertEqual(self.read(self.out + ".author"), "human.alice")

    def test_activity_and_strict_fence_settings_do_not_reach_the_helper(self):
        self.script('printf "%%s/%%s" "${BOARD_ACTIVITY:-unset}" "${BOARD_STRICT_FENCE:-unset}" > "%s"\n' % self.out)
        actions.HelperRunner(self.path, "human.a", env={"PATH": "/usr/bin:/bin", "BOARD_ACTIVITY": "1", "BOARD_STRICT_FENCE": "1"})(["act"])
        self.assertEqual(self.read(self.out), "unset/unset")

    def test_the_url_setting_is_handed_over(self):
        self.script('printf "%%s" "$LATTICE_BOARD_URL" > "%s"\n' % self.out)
        actions.HelperRunner(self.path, "human.a", url="postgresql://dev@127.0.0.1:1/x", env={"PATH": "/usr/bin:/bin"})([])
        self.assertEqual(self.read(self.out), "postgresql://dev@127.0.0.1:1/x")

    def test_stderr_is_dropped_and_a_missing_helper_is_a_failure_not_an_error(self):
        self.script('echo "psql: error: password=hunter2" >&2\necho "refused stale"\nexit 5\n')
        rc, out = actions.HelperRunner(self.path, "human.a", env={"PATH": "/usr/bin:/bin"})(["act"])
        self.assertEqual((rc, out), (5, "refused stale\n"))
        self.assertEqual(actions.HelperRunner(os.path.join(self.d, "nope"), "human.a", env={})(["act"]), (1, ""))

    def test_a_helper_that_hangs_is_killed_and_is_a_failure(self):
        self.script("sleep 30\n")
        self.assertEqual(actions.HelperRunner(self.path, "human.a", env={"PATH": "/usr/bin:/bin"}, timeout=0.5)(["act"]), (1, ""))

    def test_the_default_helper_is_the_one_next_to_the_console(self):
        self.assertTrue(actions.default_board_path().endswith(os.path.join("integrations", "board", "board")))
        self.assertTrue(os.access(actions.default_board_path(), os.X_OK))


class StrictJson(unittest.TestCase):
    def test_accepts_an_object(self):
        self.assertEqual(actions.strict_json(b'{"a": 1}'), {"a": 1})

    def test_refuses_what_could_smuggle_a_second_meaning(self):
        for raw in (b'{"a": 1, "a": 2}', b'{"a": NaN}', b'{"a": Infinity}', b'\xff', b'{', b'', b'{"a": 1} {"b": 2}'):
            with self.assertRaises(ValueError, msg=raw):
                actions.strict_json(raw)


if __name__ == "__main__":
    unittest.main()

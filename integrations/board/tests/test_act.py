"""`board act`: a person's decision, against a fake psql.

The fake answers each statement from a canned rule, so these tests check the helper's own logic: which decisions
are refused and with which code, the order of the reads, the exit statuses, and what reaches psql (every value as
a -v variable, the compare-and-set conditions, the human_actions row written in the same transaction as the task
change and the event). They do NOT run the SQL: integrations/board/tests/act_devstack.py does that against a real
latticed in the dev-stack job.

The expected values come from the rules in integrations/board/CONVENTIONS.md ("board act"), not from running the
helper: the content hash is computed here with hashlib from the title and body.

BOARD_UNDER_TEST names another copy of the helper (test_act_mutations.py runs these tests against copies that each
lack one rule).

Run: python3 -m unittest discover -s integrations/board/tests -p "test_act.py"
"""

import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
BOARD = os.environ.get("BOARD_UNDER_TEST") or os.path.join(os.path.dirname(HERE), "board")
FAKE = os.path.join(HERE, "fakepsql.py")

TITLE = "Raise the export retry delay"
BODY = "Proposed: wait 30 s, not 5 s, and run the export test twice."
SHA = hashlib.sha256((TITLE + "\n" + BODY).encode("utf-8")).hexdigest()
REQ = "req-0123456789"
TX = r"INSERT INTO human_actions"
PRIOR = r"FROM human_actions WHERE request_id"
TASK_ROW = r"SELECT state, created_by, coalesce\(owner"
AUTHOR_EVENTS = r"FROM events WHERE task_id = :'task' AND author = :'plain'"
LEASE_ROW = r"SELECT task_id FROM task_leases WHERE task_id ="


def rules(state="awaiting_approval", by="maintainer", owner="", prior="", events="", tx="t\t77",
          title=TITLE, body=BODY):
    return [
        {"match": TX, "out": tx},
        {"match": PRIOR, "out": prior},
        {"match": TASK_ROW, "out": "\t".join([state, by, owner, title, body])},
        {"match": AUTHOR_EVENTS, "out": events},
        {"match": LEASE_ROW, "out": "t1"},
    ]


def vars_of(call):
    out = {}
    argv = call["argv"]
    for i, a in enumerate(argv):
        if a == "-v" and i + 1 < len(argv) and "=" in argv[i + 1]:
            k, v = argv[i + 1].split("=", 1)
            out[k] = v
    return out


class ActCase(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp(prefix="board-act-")
        self.addCleanup(shutil.rmtree, self.tmp, True)
        self.log = os.path.join(self.tmp, "calls.jsonl")
        self.rules_path = os.path.join(self.tmp, "rules.json")
        self.psql = os.path.join(self.tmp, "psql")
        with open(self.psql, "w") as f:
            f.write('#!/bin/sh\nexec "%s" "%s" "$@"\n' % (sys.executable, FAKE))
        os.chmod(self.psql, 0o755)

    def board(self, *args, author="human.alice", rule_list=None):
        with open(self.rules_path, "w") as f:
            json.dump(rule_list if rule_list is not None else rules(), f)
        env = {"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "HOME": self.tmp, "TMPDIR": self.tmp,
               "BOARD_PSQL": self.psql, "BOARD_STATE_DIR": os.path.join(self.tmp, "state"), "FAKE_LOG": self.log,
               "FAKE_RULES": self.rules_path, "PYTHONIOENCODING": "utf-8"}
        if author:
            env["BOARD_AUTHOR"] = author
        return subprocess.run(["sh", BOARD, *args], env=env, capture_output=True, text=True, timeout=60)

    def act(self, action, task="t1", *more, **kw):
        return self.board("act", action, task, "--request-id", REQ, *more, **kw)

    def calls(self):
        if not os.path.exists(self.log):
            return []
        with open(self.log, encoding="utf-8") as f:
            return [json.loads(line) for line in f if line.strip()]

    def writes(self):
        return [c for c in self.calls() if re.search(TX, c["sql"])]

    def refused(self, r, code):
        self.assertEqual((r.returncode, r.stdout.strip()), (5, "refused " + code), r.stderr)
        self.assertEqual(self.writes(), [], "a refused decision must write nothing")


class Approve(ActCase):
    def test_an_approval_opens_the_task_and_records_who_what_and_the_hash(self):
        r = self.act("approve", "t1", "--hash", SHA, "--comment", "looks right")
        self.assertEqual((r.returncode, r.stdout.strip()), (0, "done 77"), r.stderr)
        (w,) = self.writes()
        v = vars_of(w)
        self.assertEqual(v["author"], "human.alice")
        self.assertEqual(v["task"], "t1")
        self.assertEqual(v["action"], "approve")
        self.assertEqual(v["req"], REQ)
        self.assertEqual(v["sha"], SHA)
        self.assertEqual(v["comment"], "looks right")
        self.assertEqual(v["text"], "approved content sha256:%s: looks right" % SHA)
        self.assertIn("UPDATE tasks SET state = 'open'", w["sql"])

    def test_the_hash_is_computed_from_title_newline_body_as_bytes(self):
        t, b = "Prüfe den Export ☕", "Körper: zwei Zeilen? nein, eine é"
        want = hashlib.sha256((t + "\n" + b).encode("utf-8")).hexdigest()
        r = self.act("approve", "t1", "--hash", want, rule_list=rules(title=t, body=b))
        self.assertEqual((r.returncode, r.stdout.strip()), (0, "done 77"), r.stderr)
        self.assertEqual(vars_of(self.writes()[0])["sha"], want)

    def test_an_empty_body_hashes_as_title_and_a_newline(self):
        want = hashlib.sha256((TITLE + "\n").encode()).hexdigest()
        r = self.act("approve", "t1", "--hash", want, rule_list=rules(body=""))
        self.assertEqual((r.returncode, r.stdout.strip()), (0, "done 77"), r.stderr)

    def test_an_approval_needs_the_hash(self):
        self.refused(self.act("approve"), "hash_required")

    def test_text_that_changed_since_it_was_shown_is_refused(self):
        other = hashlib.sha256((TITLE + "\n" + "something else").encode()).hexdigest()
        self.refused(self.act("approve", "t1", "--hash", other), "stale")

    def test_the_hash_must_be_64_lower_case_hex_digits(self):
        for bad in ("abc", SHA.upper(), SHA[:-1] + "g", SHA + "0"):
            r = self.act("approve", "t1", "--hash", bad)
            self.assertEqual(r.returncode, 1, bad)
            self.assertIn("--hash", r.stderr)
        self.assertEqual(self.writes(), [])

    def test_the_creator_cannot_approve_under_either_name(self):
        for by in ("alice", "human.alice"):
            with self.subTest(creator=by):
                self.refused(self.act("approve", "t1", "--hash", SHA, rule_list=rules(by=by)), "self_approval")

    def test_the_owner_cannot_approve(self):
        self.refused(self.act("approve", "t1", "--hash", SHA, rule_list=rules(owner="alice")), "self_approval")

    def test_a_name_that_wrote_an_event_on_the_task_cannot_approve(self):
        r = self.act("approve", "t1", "--hash", SHA, rule_list=rules(events="12"))
        self.refused(r, "self_approval")
        (q,) = [c for c in self.calls() if re.search(AUTHOR_EVENTS, c["sql"])]
        self.assertEqual(vars_of(q)["plain"], "alice")  # the name without human.

    def test_another_persons_name_does_not_block(self):
        r = self.act("approve", "t1", "--hash", SHA, author="human.bob", rule_list=rules(by="alice"))
        self.assertEqual((r.returncode, r.stdout.strip()), (0, "done 77"), r.stderr)

    def test_the_transaction_repeats_the_self_approval_test_against_the_row_it_writes(self):
        r = self.act("approve", "t1", "--hash", SHA)
        (w,) = self.writes()
        for part in ("created_by <> :'author'", "created_by <> :'plain'", "coalesce(owner, '') <> :'author'",
                     "coalesce(owner, '') <> :'plain'"):
            self.assertIn(part, w["sql"])

    def test_only_a_task_waiting_for_approval_can_be_approved(self):
        for state in ("open", "claimed", "done", "dead", "cancelled"):
            with self.subTest(state=state):
                self.refused(self.act("approve", "t1", "--hash", SHA, rule_list=rules(state=state)), "wrong_state")


class Reject(ActCase):
    def test_a_rejection_needs_a_comment(self):
        self.refused(self.act("reject"), "comment_required")

    def test_a_rejection_cancels_and_records_the_reason(self):
        r = self.act("reject", "t1", "--comment", "too risky")
        self.assertEqual((r.returncode, r.stdout.strip()), (0, "done 77"), r.stderr)
        (w,) = self.writes()
        self.assertIn("SET state = 'cancelled'", w["sql"])
        self.assertEqual(vars_of(w)["text"], "reject: too risky")
        self.assertEqual(vars_of(w)["action"], "reject")

    def test_the_creator_cannot_reject_either(self):
        self.refused(self.act("reject", "t1", "--comment", "no", rule_list=rules(by="alice")), "self_approval")

    def test_only_a_task_waiting_for_approval_can_be_rejected(self):
        self.refused(self.act("reject", "t1", "--comment", "no", rule_list=rules(state="open")), "wrong_state")


class Cancel(ActCase):
    def test_cancel_applies_to_open_claimed_and_waiting_tasks(self):
        for state in ("open", "claimed", "awaiting_approval"):
            with self.subTest(state=state):
                if os.path.exists(self.log):
                    os.remove(self.log)
                r = self.act("cancel", "t1", rule_list=rules(state=state))
                self.assertEqual((r.returncode, r.stdout.strip()), (0, "done 77"), r.stderr)
                (w,) = self.writes()
                self.assertIn("SET state = 'cancelled'", w["sql"])
                self.assertIn("UPDATE task_leases SET lease_until = 0", w["sql"])

    def test_cancel_does_not_apply_to_finished_tasks(self):
        for state in ("done", "dead", "cancelled"):
            with self.subTest(state=state):
                self.refused(self.act("cancel", "t1", rule_list=rules(state=state)), "wrong_state")

    def test_cancel_has_no_self_approval_rule(self):
        r = self.act("cancel", "t1", rule_list=rules(by="alice", state="open"))
        self.assertEqual((r.returncode, r.stdout.strip()), (0, "done 77"), r.stderr)

    def test_cancel_makes_sure_the_lease_row_exists_first(self):
        self.act("cancel", "t1", rule_list=rules(state="open"))
        self.assertTrue([c for c in self.calls() if re.search(LEASE_ROW, c["sql"])])


class Requeue(ActCase):
    def test_a_dead_task_opens_again_with_its_attempts_reset(self):
        r = self.act("requeue", "t1", "--comment", "fixed the fixture", rule_list=rules(state="dead"))
        self.assertEqual((r.returncode, r.stdout.strip()), (0, "done 77"), r.stderr)
        (w,) = self.writes()
        self.assertIn("UPDATE tasks SET state = 'open', owner = NULL", w["sql"])
        self.assertIn("attempts = 0, lease_until = 0, not_before = 0, last_error = ''", w["sql"])
        self.assertEqual(vars_of(w)["text"], "requeue: fixed the fixture")

    def test_a_claimed_task_can_be_taken_back(self):
        r = self.act("requeue", "t1", rule_list=rules(state="claimed", owner="agent-x"))
        self.assertEqual((r.returncode, r.stdout.strip()), (0, "done 77"), r.stderr)

    def test_requeue_does_not_apply_to_other_states(self):
        for state in ("open", "awaiting_approval", "done", "cancelled"):
            with self.subTest(state=state):
                self.refused(self.act("requeue", "t1", rule_list=rules(state=state)), "wrong_state")


class Priority(ActCase):
    def test_it_sets_the_number_given(self):
        r = self.act("priority", "t1", "--priority", "-7", rule_list=rules(state="open"))
        self.assertEqual((r.returncode, r.stdout.strip()), (0, "done 77"), r.stderr)
        (w,) = self.writes()
        self.assertIn("SET priority = :prio", w["sql"])
        self.assertEqual(vars_of(w)["prio"], "-7")
        self.assertEqual(vars_of(w)["text"], "priority set to -7")

    def test_it_needs_a_number(self):
        for extra in ([], ["--priority", "high"], ["--priority", "1.5"], ["--priority", "1234567890"]):
            with self.subTest(extra=extra):
                r = self.act("priority", "t1", *extra, rule_list=rules(state="open"))
                self.assertEqual(r.returncode, 1)
        self.assertEqual(self.writes(), [])

    def test_finished_tasks_keep_their_priority(self):
        for state in ("done", "cancelled"):
            with self.subTest(state=state):
                self.refused(self.act("priority", "t1", "--priority", "3", rule_list=rules(state=state)), "wrong_state")


class Note(ActCase):
    def test_a_note_needs_a_comment(self):
        self.refused(self.act("note"), "comment_required")

    def test_a_note_is_an_event_and_changes_no_task_row(self):
        r = self.act("note", "t1", "--comment", "ask legal first", rule_list=rules(state="done"))
        self.assertEqual((r.returncode, r.stdout.strip()), (0, "done 77"), r.stderr)
        (w,) = self.writes()
        self.assertNotIn("UPDATE tasks", w["sql"])
        self.assertEqual(vars_of(w)["text"], "ask legal first")


class Idempotency(ActCase):
    def test_the_same_request_again_changes_nothing_and_names_the_event(self):
        prior = "\t".join(["t1", "human.alice", "approve", "55"])
        r = self.act("approve", "t1", "--hash", SHA, rule_list=rules(prior=prior))
        self.assertEqual((r.returncode, r.stdout.strip()), (0, "noop 55"), r.stderr)
        self.assertEqual(self.writes(), [])
        self.assertEqual([c for c in self.calls() if re.search(TASK_ROW, c["sql"])], [], "a repeat reads nothing else")

    def test_a_request_id_used_for_another_decision_is_refused(self):
        for prior in ("\t".join(["t2", "human.alice", "approve", "55"]), "\t".join(["t1", "human.bob", "approve", "55"]),
                      "\t".join(["t1", "human.alice", "cancel", "55"])):
            with self.subTest(prior=prior):
                self.refused(self.act("approve", "t1", "--hash", SHA, rule_list=rules(prior=prior)), "request_reused")

    def test_the_lookup_is_by_request_id_and_comes_first(self):
        self.act("approve", "t1", "--hash", SHA)
        first = self.calls()[0]
        self.assertRegex(first["sql"], PRIOR)
        self.assertEqual(vars_of(first)["req"], REQ)

    def test_the_row_is_written_in_the_same_transaction_as_the_change_and_the_event(self):
        self.act("approve", "t1", "--hash", SHA)
        (w,) = self.writes()
        sql = w["sql"]
        i_if, i_upd, i_ev, i_row, i_end = (sql.index(x) for x in (
            "\\if :ok", "UPDATE tasks SET state = 'open'", "INSERT INTO events", "INSERT INTO human_actions", "\\endif"))
        self.assertTrue(i_if < i_upd < i_ev < i_row < i_end, sql)
        self.assertIn("BEGIN;", sql)
        self.assertIn("COMMIT;", sql)
        self.assertIn(":seq", sql[i_row:i_end])  # the row names the event it made

    def test_a_decision_that_lost_a_race_writes_nothing_and_says_changed(self):
        # The statement is sent; the server answers ok = f because the condition no longer held, so nothing changed.
        r = self.act("approve", "t1", "--hash", SHA, rule_list=rules(tx="f\t78"))
        self.assertEqual((r.returncode, r.stdout.strip()), (5, "refused changed"), r.stderr)


class CompareAndSet(ActCase):
    def test_every_change_is_conditional_on_the_state_and_text_that_were_read(self):
        for action, extra, state in (("approve", ["--hash", SHA], "awaiting_approval"), ("cancel", [], "open"),
                                     ("requeue", [], "dead"), ("priority", ["--priority", "2"], "open")):
            with self.subTest(action=action):
                if os.path.exists(self.log):
                    os.remove(self.log)
                self.act(action, "t1", *extra, rule_list=rules(state=state))
                (w,) = self.writes()
                v = vars_of(w)
                self.assertEqual((v["state"], v["title"], v["body"]), (state, TITLE, BODY))
                self.assertIn("state = :'state' AND title = :'title' AND body = :'body'", w["sql"])


class Values(ActCase):
    def test_hostile_text_reaches_psql_only_as_variables(self):
        evil = "x'; DROP TABLE tasks; --"
        r = self.act("cancel", "t1", "--comment", evil, rule_list=rules(state="open"))
        self.assertEqual(r.returncode, 0, r.stderr)
        for c in self.calls():
            self.assertNotIn("DROP TABLE", c["sql"])
        self.assertEqual(vars_of(self.writes()[0])["comment"], evil)

    def test_comments_must_be_one_clean_line(self):
        long = "x" * 4097
        for bad in ("two\nlines", "tab\there", "back\\slash", "bell\x07", long):
            with self.subTest(bad=bad[:10]):
                r = self.act("cancel", "t1", "--comment", bad, rule_list=rules(state="open"))
                self.assertEqual(r.returncode, 1)
                self.assertTrue(r.stderr.startswith("board: comment "), r.stderr)  # said about the comment, not the derived text
        self.assertEqual(self.writes(), [])

    def test_task_text_on_several_lines_cannot_be_approved(self):
        self.refused(self.act("approve", "t1", "--hash", SHA, rule_list=rules(body="one\ntwo")), "unreadable")

    def test_a_missing_task(self):
        r = self.act("cancel", "t1", rule_list=[{"match": PRIOR, "out": ""}, {"match": TASK_ROW, "out": ""}])
        self.refused(r, "not_found")


class Identity(ActCase):
    def test_only_a_human_name_may_act(self):
        for author in ("alice", "human.", "Human.alice", "agent.human.alice"):
            with self.subTest(author=author):
                r = self.act("cancel", "t1", author=author, rule_list=rules(state="open"))
                self.assertEqual(r.returncode, 1)
                self.assertIn("BOARD_AUTHOR must be human.NAME", r.stderr)
        self.assertEqual(self.calls(), [])

    def test_no_author_at_all(self):
        r = self.act("cancel", "t1", author=None)
        self.assertEqual(r.returncode, 1)
        self.assertIn("BOARD_AUTHOR is required", r.stderr)

    def test_every_other_command_refuses_the_human_namespace(self):
        for args in (["add", "x"], ["note", "t1", "hi"], ["claim", "t1"], ["done", "t1"], ["approve", "t1"],
                     ["cancel", "t1"], ["decide", "t1", "why"], ["fail", "t1"], ["reap"]):
            with self.subTest(cmd=args[0]):
                r = self.board(*args, author="human.alice")
                self.assertEqual(r.returncode, 1)
                self.assertIn("reserved for 'board act'", r.stderr)
        self.assertEqual(self.calls(), [])

    def test_a_name_that_merely_contains_human_is_an_ordinary_name(self):
        r = self.board("note", "t1", "hi", author="humanoid.bot")
        self.assertNotIn("reserved", r.stderr)


class Arguments(ActCase):
    def test_the_action_list_is_closed(self):
        for action in ("delete", "approve-all", "Approve", "", "done"):
            with self.subTest(action=action):
                r = self.board("act", action, "t1", "--request-id", REQ)
                self.assertEqual(r.returncode, 1)
        self.assertEqual(self.calls(), [])

    def test_request_ids_are_8_to_64_plain_characters(self):
        for bad in ("short", "a" * 65, "has space1", "semi;colon1", "dot.dot.dot", ""):
            with self.subTest(req=bad):
                r = self.board("act", "cancel", "t1", "--request-id", bad)
                self.assertEqual(r.returncode, 1)
                self.assertIn("--request-id", r.stderr)
        self.assertEqual(self.calls(), [])

    def test_a_request_id_is_required(self):
        r = self.board("act", "cancel", "t1")
        self.assertEqual(r.returncode, 1)

    def test_task_ids_are_checked(self):
        r = self.board("act", "cancel", "t1;x", "--request-id", REQ)
        self.assertEqual(r.returncode, 1)
        self.assertIn("not a task id", r.stderr)

    def test_the_usage_lists_the_command(self):
        r = self.board("help")
        self.assertIn("act ACTION ID --request-id R", r.stdout)


if __name__ == "__main__":
    unittest.main()

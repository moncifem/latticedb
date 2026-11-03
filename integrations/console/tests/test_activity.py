"""Live agent activity in the console: the pure rules (states, strip, timeline, estimates), the service that
reads the rows with a cursor, the server route, the SQL the source may send, and the demo data.

Every expected number was worked out by hand from the rules in the README ("Live activity") and the module
docstring of console/activity.py: working is a report in the last 120 s, quiet is older, stuck? is older than
the quiet limit while a lease is alive; active time is the sum of gaps below 60 s between rows.
"""

import json
import unittest

import helpers  # noqa: F401
from helpers import FakeSource, event, lease, small_board, task

from console import activity as act
from console import demo, model, server as server_mod
from console.service import BoardService
from console.source import PsqlSource, SourceError, build_select

from test_server import Running  # a real server on a loopback port

NOW = 100000


def session(agent="alice", sid="s1", last_seen=NOW - 10, state="working", tool="Edit", target="f.rs", task_id="",
            calls=5, started=NOW - 1000, outcome="ok"):
    return {"id": "%s/%s" % (agent, sid), "agent": agent, "session": sid, "task_id": task_id, "tool": tool,
            "target": target, "outcome": outcome, "state": state, "tool_calls": calls, "last_seen": last_seen,
            "started_at": started}


def logrow(i, agent="alice", at=NOW - 10, tool="Edit", calls=1, mix=None, sid="s1", task_id="", target="f.rs", outcome="ok"):
    return {"id": i, "agent": agent, "session": sid, "task_id": task_id, "tool": tool, "target": target,
            "outcome": outcome, "calls": calls, "mix": mix if mix is not None else ("%s=%d" % (tool, calls) if calls else ""),
            "at": at}


def queue_with(owner="alice", expired=False, has_lease=True, available=True):
    return {"leases_available": available,
            "claimed": [{"id": "t1", "owner": owner, "expired": expired, "has_lease": has_lease, "heartbeat_age_s": 30}]}


class Cleaning(unittest.TestCase):
    def test_a_row_of_the_promised_shape_passes(self):
        row = dict(session(), task_id="t12-a")
        out = act.clean_session(row)
        self.assertEqual((out["agent"], out["tool"], out["target"], out["state"], out["outcome"], out["task_id"]),
                         ("alice", "Edit", "f.rs", "working", "ok", "t12-a"))

    def test_rows_that_are_not_that_shape_are_dropped_or_made_safe(self):
        self.assertIsNone(act.clean_session(dict(session(), agent="al ice")))
        self.assertIsNone(act.clean_session(dict(session(), agent="")))
        self.assertIsNone(act.clean_session(dict(session(), session="s/1")))
        self.assertIsNone(act.clean_session(dict(session(), agent=None)))
        out = act.clean_session(dict(session(), tool="rm -rf", outcome="great", state="dancing", task_id="a b",
                                     target="x\x1b[31m" + "y" * 100, tool_calls=-5, last_seen="9", started_at=True))
        self.assertEqual((out["tool"], out["outcome"], out["state"], out["task_id"]), ("other", "ok", "working", ""))
        self.assertEqual(out["target"], "x[31m" + "y" * 59)
        self.assertEqual((out["tool_calls"], out["last_seen"], out["started_at"]), (0, 0, 0))

    def test_log_rows_need_an_integer_id_and_time(self):
        self.assertIsNone(act.clean_log(dict(logrow(1), id="5")))
        self.assertIsNone(act.clean_log(dict(logrow(1), id=True)))
        self.assertIsNone(act.clean_log(dict(logrow(1), at=None)))
        self.assertIsNone(act.clean_log(dict(logrow(1), id=-1)))
        out = act.clean_log(dict(logrow(7), calls=10**12, mix="Edit=1,;rm"))
        self.assertEqual((out["calls"], out["mix"]), (0, ""))

    def test_the_mix_must_be_well_formed(self):
        for good in ("Edit=3", "Bash=2,Edit=1", "mcp__a__b-c.d=7"):
            self.assertEqual(act.clean_mix(good), good)
        for bad in ("", None, 5, "Edit", "Edit=", "=3", "Edit=3,", "Edit=3;Bash=1", "Edit=x", "Ed it=1", "E=1," + "A=1," * 60 + "B=1",
                    "Edit=1234567890"):
            self.assertEqual(act.clean_mix(bad), "", bad)

    def test_mix_counts(self):
        self.assertEqual(act.mix_counts(logrow(1, mix="Bash=2,Edit=1", calls=3)), {"Bash": 2, "Edit": 1})
        self.assertEqual(act.mix_counts(logrow(1, tool="Read", calls=4, mix="")), {"Read": 4})
        self.assertEqual(act.mix_counts(logrow(1, tool="stop", calls=0, mix="")), {})
        self.assertEqual(act.mix_counts(logrow(1, tool="session-end", calls=3, mix="")), {})

    def test_merge_keeps_ascending_ids_and_drops_repeats(self):
        old = [logrow(1), logrow(2)]
        out = act.merge_log(old, [logrow(2), logrow(3), logrow(1)])
        self.assertEqual([r["id"] for r in out], [1, 2, 3])
        # rows that arrive out of order are put in order
        self.assertEqual([r["id"] for r in act.merge_log([logrow(5)], [logrow(3), logrow(9)])], [3, 5, 9])

    def test_invisible_and_reordering_characters_are_stripped_from_text(self):
        self.assertEqual(act.clip_text("a\u061cb\u2060c"), "abc")
        hostile = "a\u200bb\u200cc\u200dd\u200ee\u200ff\u202ag\u202bh\u202ci\u202dj\u202ek\u2066l\u2067m\u2068n\u2069o\ufeffp\u2028q\u2029r\x00s\x1bt\x7fu\x85v"
        self.assertEqual(act.clip_text(hostile), "abcdefghijklmnopqrstuv")
        out = act.clean_session(dict(session(), target="x\u202e" + "gpj.exe\u200f"))
        self.assertEqual(out["target"], "xgpj.exe")
        self.assertEqual(act.clip_text("\u200b" * 100 + "z", 5), "z", "the limit counts what is left")

    def test_the_log_kept_in_memory_is_bounded(self):
        rows = [logrow(i) for i in range(act.LOG_KEEP + 50)]
        out = act.merge_log([], rows)
        self.assertEqual(len(out), act.LOG_KEEP)
        self.assertEqual(out[-1]["id"], act.LOG_KEEP + 49)


class Strip(unittest.TestCase):
    def test_buckets_by_minute_with_the_write_weighted_by_its_calls(self):
        rows = [logrow(1, at=NOW, calls=2), logrow(2, at=NOW - 59, calls=3), logrow(3, at=NOW - 60, calls=5),
                logrow(4, at=NOW - 3599, calls=7), logrow(5, at=NOW - 3600, calls=11), logrow(6, at=NOW + 30, calls=1)]
        s = act.strip(rows, NOW)
        self.assertEqual(len(s), 60)
        # age 0 and 59 -> last bucket; 60 -> the one before; 3599 -> first; 3600 is outside; a future row counts as now
        self.assertEqual(s[59], 2 + 3 + 1)
        self.assertEqual(s[58], 5)
        self.assertEqual(s[0], 7)
        self.assertEqual(sum(s), 2 + 3 + 1 + 5 + 7)

    def test_an_empty_log_is_a_flat_strip(self):
        self.assertEqual(act.strip([], NOW), [0] * 60)


class States(unittest.TestCase):
    def state(self, age, claimed=True, session_state="working", queue=None, quiet_s=600, **kw):
        s = session(last_seen=NOW - age, state=session_state, **kw)
        q = queue or queue_with()
        a = act.agent_activity("alice", [s], [], ["t1"] if claimed else [], q, NOW, quiet_s)
        return a["state"]

    def test_a_task_holder_by_age_of_the_last_report(self):
        self.assertEqual(self.state(0), "working")
        self.assertEqual(self.state(120), "working")
        self.assertEqual(self.state(121), "quiet")
        self.assertEqual(self.state(600), "quiet")
        self.assertEqual(self.state(601), "stuck?")
        self.assertEqual(self.state(5000), "stuck?")

    def test_stuck_needs_a_live_lease(self):
        self.assertEqual(self.state(900, queue=queue_with(expired=True)), "quiet")
        self.assertEqual(self.state(900, queue=queue_with(has_lease=False)), "quiet")
        self.assertEqual(self.state(900, queue=queue_with(owner="bob")), "quiet")
        self.assertEqual(self.state(900, queue=queue_with(available=False)), "quiet")

    def test_the_quiet_limit_is_the_setting(self):
        self.assertEqual(self.state(301, quiet_s=300), "stuck?")
        self.assertEqual(self.state(300, quiet_s=300), "quiet")

    def test_without_a_task_the_agent_is_working_only_while_it_reports(self):
        self.assertEqual(self.state(5, claimed=False), "working")
        self.assertEqual(self.state(120, claimed=False), "working")
        self.assertEqual(self.state(121, claimed=False), "idle")
        self.assertEqual(self.state(5, claimed=False, session_state="idle"), "idle")
        self.assertEqual(self.state(5, claimed=False, session_state="ended"), "idle")

    def test_an_ended_session_is_never_quiet_or_stuck_and_a_finished_turn_is_not_working(self):
        # an agent that holds a task and whose session said it ended: idle, whatever the age
        for age in (5, 200, 5000):
            self.assertEqual(self.state(age, session_state="ended"), "idle", age)
        # a turn that ended (Stop) is not work however fresh the report: quiet inside the limit, stuck? after it
        self.assertEqual(self.state(5, session_state="idle"), "quiet")
        self.assertEqual(self.state(120, session_state="idle"), "quiet")
        self.assertEqual(self.state(600, session_state="idle"), "quiet")
        self.assertEqual(self.state(601, session_state="idle"), "stuck?")
        self.assertEqual(self.state(5, session_state="working"), "working")

    def test_no_row_means_no_block(self):
        self.assertIsNone(act.agent_activity("bob", [session()], [], [], queue_with(), NOW, 600))

    def test_the_newest_session_is_the_current_one(self):
        old = session(sid="old", last_seen=NOW - 5000, tool="Bash")
        new = session(sid="new", last_seen=NOW - 20, tool="Read", target="r.rs")
        a = act.agent_activity("alice", [old, new], [], [], queue_with(), NOW, 600)
        self.assertEqual((a["session"], a["doing"]["tool"], a["age_s"], a["sessions"]), ("new", "Read", 20, 2))

    def test_the_doing_line_is_the_reported_tool_unless_the_session_ended_or_the_row_is_a_mark(self):
        a = act.agent_activity("alice", [session(tool="Bash", target="cargo [test]", outcome="error")], [], [], queue_with(), NOW, 600)
        self.assertEqual(a["doing"], {"tool": "Bash", "target": "cargo [test]", "outcome": "error"})
        a = act.agent_activity("alice", [session(tool="stop", state="idle", target="")], [logrow(1, tool="Read", at=NOW - 30)],
                               [], queue_with(), NOW, 600)
        self.assertIsNone(a["doing"])
        self.assertEqual(a["last_call"], {"tool": "Read", "target": "f.rs", "outcome": "ok", "age_s": 30})
        a = act.agent_activity("alice", [session(state="ended")], [], [], queue_with(), NOW, 600)
        self.assertIsNone(a["doing"])

    def test_the_last_call_skips_marks_and_empty_writes(self):
        rows = [logrow(1, tool="Edit", at=NOW - 50), logrow(2, tool="stop", calls=0, at=NOW - 20),
                logrow(3, tool="Read", calls=0, mix="", at=NOW - 10)]
        a = act.agent_activity("alice", [session()], rows, [], queue_with(), NOW, 600)
        self.assertEqual(a["last_call"]["tool"], "Edit")

    def test_the_wording_never_says_stuck_without_a_question_mark(self):
        for text in act.STATE_TEXT.values():
            self.assertNotRegex(text, r"\bstuck\b(?!\?)")
        self.assertIn("stuck?", act.STATE_TEXT)
        self.assertRegex(act.QUIET_RULE, "self-reported")


class Attention(unittest.TestCase):
    def test_only_stuck_holders_with_a_live_lease_are_listed_oldest_first(self):
        tasks = {"t1": task("t1", "claimed", "alice", title="one"), "t2": task("t2", "claimed", "bob", title="two"),
                 "t3": task("t3", "claimed", "carol", title="three"), "t4": task("t4", "open", None)}
        activity = {"alice": {"state": "stuck?", "age_s": 700}, "bob": {"state": "stuck?", "age_s": 900},
                    "carol": {"state": "quiet", "age_s": 300}, "dave": None}
        q = {"claimed": [{"id": "t1", "heartbeat_age_s": 12}, {"id": "t2", "heartbeat_age_s": None}]}
        out = act.quiet_claims(activity, tasks, q, NOW)
        self.assertEqual([(x["task_id"], x["owner"], x["quiet_s"], x["heartbeat_age_s"]) for x in out],
                         [("t2", "bob", 900, None), ("t1", "alice", 700, 12)])


class Detail(unittest.TestCase):
    def rows(self):
        return [logrow(1, at=100, tool="Edit", calls=2, mix="Edit=2"), logrow(2, at=105, tool="Bash", calls=1, task_id="t1"),
                logrow(3, at=110, tool="Read", calls=3, mix="Read=2,Bash=1", outcome="error"),
                logrow(4, at=200, tool="Edit", calls=1), logrow(5, at=205, tool="stop", calls=0, mix=""),
                logrow(6, at=210, tool="Edit", calls=1, sid="other"), logrow(7, at=215, tool="Edit", calls=1, sid="other"),
                logrow(8, agent="bob", at=100)]

    def test_the_active_time_estimate_sums_gaps_below_60_seconds_per_session(self):
        # session s1 rows with calls: 100, 105, 110, 200 (the stop mark has none): gaps 5, 5, 90 -> 10.
        # session "other": 210, 215 -> 5. Total 15.
        self.assertEqual(act.active_estimate([r for r in self.rows() if r["agent"] == "alice"]), 15)
        self.assertEqual(act.active_estimate([]), 0)
        # a gap of 59 s counts, a gap of exactly 60 s does not
        self.assertEqual(act.active_estimate([logrow(1, at=0), logrow(2, at=59), logrow(3, at=119), logrow(4, at=179)]), 59)

    def test_the_timeline_is_newest_first_and_only_this_agents(self):
        d = act.agent_detail("alice", [], self.rows(), [], 300, None, None, None, 200, None)
        self.assertEqual([r["id"] for r in d["rows"]], [7, 6, 5, 4, 3, 2, 1])
        self.assertEqual(d["rows_total"], 7)
        self.assertEqual(d["rows_held"], 7)
        self.assertEqual(d["rows"][0]["age_s"], 300 - 215)
        self.assertEqual(d["cursor"], 7)

    def test_the_tool_mix_adds_the_calls_of_every_row_in_view(self):
        d = act.agent_detail("alice", [], self.rows(), [], 300, None, None, None, 200, None)
        # Edit: 2 + 1 + 1 + 1 = 5; Bash: 1 (row 2, from its tool) + 1 (row 3's mix) = 2; Read: 2.
        self.assertEqual(dict(d["mix"]), {"Edit": 5, "Bash": 2, "Read": 2})
        self.assertEqual(d["mix"][0], ("Edit", 5))

    def test_filters(self):
        d = act.agent_detail("alice", [], self.rows(), [], 300, "Bash", None, None, 200, None)
        self.assertEqual([r["id"] for r in d["rows"]], [3, 2], "a row whose mix has the tool matches too")
        d = act.agent_detail("alice", [], self.rows(), [], 300, None, "error", None, 200, None)
        self.assertEqual([r["id"] for r in d["rows"]], [3])
        d = act.agent_detail("alice", [], self.rows(), [], 300, None, None, "t1", 200, None)
        self.assertEqual([r["id"] for r in d["rows"]], [2])

    def test_limit_and_cursor(self):
        d = act.agent_detail("alice", [], self.rows(), [], 300, None, None, None, 3, None)
        self.assertEqual([r["id"] for r in d["rows"]], [7, 6, 5])
        self.assertEqual(d["rows_total"], 7)
        d = act.agent_detail("alice", [], self.rows(), [], 300, None, None, None, 2, 4)
        self.assertEqual([r["id"] for r in d["rows"]], [6, 5], "after a cursor: the next rows, newest first, at most limit")
        self.assertEqual(d["cursor"], 6)
        d = act.agent_detail("alice", [], self.rows(), [], 300, None, None, None, 5, 7)
        self.assertEqual((d["rows"], d["cursor"]), ([], 7))

    def test_sessions_tasks_and_the_note(self):
        tasks = [task("t1", "claimed", "alice", title="mine"), task("t2", "claimed", "bob")]
        d = act.agent_detail("alice", [session(), session(sid="s2", last_seen=NOW - 50)], [], tasks, NOW, None, None, None, 5, None)
        self.assertEqual([s["session"] for s in d["sessions"]], ["s1", "s2"])
        self.assertEqual([t["id"] for t in d["claimed"]], ["t1"])
        self.assertIn("estimate", d["estimate_note"])
        self.assertIn("60", d["estimate_note"])

    def test_the_answer_is_small_even_for_worst_case_rows(self):
        big = [logrow(i, at=NOW - i, tool="T" * 64, target="x" * 64, task_id="t" * 64, mix=",".join("A%d=9" % k for k in range(20))[:200]) for i in range(1, 400)]
        for r in big:
            r["mix"] = act.clean_mix(r["mix"])
        d = act.agent_detail("alice", [session() for _ in range(40)], big, [], NOW, None, None, None, 200, None)
        self.assertLessEqual(len(d["rows"]), 200)
        self.assertLessEqual(len(d["sessions"]), act.DETAIL_SESSIONS)
        self.assertLess(len(json.dumps(d)), server_mod.MAX_BODY // 10)


def facts_for(done):
    """done: list of (task id, claimant, claim ts, done ts)."""
    tasks, events = [], []
    eid = 0
    for tid, who, c, d in done:
        tasks.append(task(tid, "done", who, created=c - 10))
        eid += 1
        events.append(event(eid, tid, who, "claim", c))
        eid += 1
        events.append(event(eid, tid, who, "done", d))
    return model.task_facts(tasks, events, [])


class PerTask(unittest.TestCase):
    def test_too_few_tasks_show_no_number(self):
        facts = facts_for([("t1", "alice", 1000, 1100)])
        rows = [logrow(1, at=990), logrow(2, at=1010), logrow(3, at=1020), logrow(4, at=1100), logrow(5, at=1110)]
        out = act.active_per_task(facts, rows)
        self.assertEqual(out["overall"]["status"], "insufficient")
        self.assertEqual(out["overall"]["n"], 1)
        self.assertEqual(out["done_tasks"], 1)
        self.assertIn("not enough data", out["overall"]["message"])

    def test_five_watched_tasks_give_a_median_and_nothing_is_ranked(self):
        specs = [("t%d" % i, "alice" if i % 2 else "bob", 1000 * i, 1000 * i + 500) for i in range(1, 6)]
        rows = []
        n = 0
        # the claimant works 10 * i seconds in 5 second steps inside each span, with a quiet gap of 100 s inside
        for i, (tid, who, c, d) in enumerate(specs, start=1):
            t = c
            rows.append(logrow(n := n + 1, agent=who, at=c - 5))
            for k in range(2 * i + 1):
                rows.append(logrow(n := n + 1, agent=who, at=t + 5 * k))
            rows.append(logrow(n := n + 1, agent=who, at=d + 5))
        rows.sort(key=lambda r: (r["agent"], r["at"]))
        out = act.active_per_task(facts_for(specs), rows)
        # in span i the rows are c, c+5, ..., c+10i: 2i+1 rows, gaps of 5 -> 10*i seconds. Values 10, 20, 30, 40, 50.
        self.assertEqual(out["overall"]["status"], "ok")
        self.assertEqual((out["overall"]["n"], out["overall"]["median_s"], out["overall"]["min_s"], out["overall"]["max_s"]),
                         (5, 30.0, 10, 50))
        # per agent: alice has t1, t3, t5 (three tasks, below the minimum), bob has two
        self.assertEqual([a["agent"] for a in out["agents"]], ["alice", "bob"])
        self.assertTrue(all(a["status"] == "insufficient" for a in out["agents"]))
        self.assertIn("not ranked", out["note"])

    def test_a_task_the_console_did_not_watch_from_the_start_is_left_out_not_counted_as_zero(self):
        facts = facts_for([("t1", "alice", 1000, 1100)])
        rows = [logrow(1, at=1050), logrow(2, at=1060), logrow(3, at=1120)]  # the first row is after the claim
        out = act.active_per_task(facts, rows)
        self.assertEqual(out["overall"]["n"], 0)
        rows = [logrow(1, at=900), logrow(2, at=1050), logrow(3, at=1060)]  # nothing after the done event
        self.assertEqual(act.active_per_task(facts, rows)["overall"]["n"], 0)


class FakeActivitySource(FakeSource):
    def __init__(self, *a, sessions=None, log=None, available=True, **kw):
        super().__init__(*a, **kw)
        self.session_rows = list(sessions or [])
        self.log_rows = list(log or [])
        self.available = available
        self.activity_error = None

    def activity_sessions(self, limit):
        self._check("activity_sessions")
        if self.activity_error is not None:
            raise self.activity_error
        return (list(self.session_rows[:limit]), True) if self.available else ([], False)

    def activity_latest(self, limit):
        self._check("activity_latest")
        return list(self.log_rows[-limit:])

    def activity_after(self, after, limit):
        self._check("activity_after")
        return [r for r in self.log_rows if r["id"] > after][:limit]


class Clock:
    t = 0.0

    def __call__(self):
        return self.t


def service(sessions=None, log=None, leases=None, available=True, tasks=None, **kw):
    t, e, d = small_board()
    src = FakeActivitySource(tasks or t, e, d, now=NOW, leases=leases, sessions=sessions, log=log, available=available)
    clock = Clock()
    return src, clock, BoardService(src, ttl=0.0, clock=clock, **kw)


class ServiceTests(unittest.TestCase):
    def test_the_first_refresh_reads_sessions_and_the_latest_window_then_only_newer_rows(self):
        src, clock, s = service([session()], [logrow(1), logrow(2)])
        snap = s.snapshot()
        self.assertTrue(snap.activity_available)
        self.assertEqual(snap.activity_head, 2)
        self.assertEqual([c for c in src.calls if c.startswith("activity")], ["activity_sessions", "activity_latest"])
        src.calls.clear()
        src.log_rows.append(logrow(3))
        snap = s.snapshot()
        self.assertEqual([c for c in src.calls if c.startswith("activity")], ["activity_sessions", "activity_after"])
        self.assertEqual(snap.activity_head, 3)
        self.assertEqual([r["id"] for r in snap.activity_log], [1, 2, 3])

    def test_a_cursor_that_pages(self):
        from console import service as svc
        src, clock, s = service([session()], [logrow(1)])
        s.snapshot()
        for i in range(2, 2 + svc.ACTIVITY_PAGE * 2 + 5):
            src.log_rows.append(logrow(i))
        self.assertEqual(s.snapshot().activity_head, 1 + svc.ACTIVITY_PAGE * 2 + 5)

    def test_hostile_rows_never_reach_the_snapshot(self):
        bad = [dict(logrow(1), agent="a b"), dict(logrow(2), id="x"), dict(logrow(3), tool="rm -rf /", target="\x1b[2J" + "z" * 200)]
        src, clock, s = service([dict(session(), agent="a;b"), session()], bad)
        snap = s.snapshot()
        self.assertEqual([x["agent"] for x in snap.activity_sessions], ["alice"])
        self.assertEqual(len(snap.activity_log), 1)
        self.assertEqual((snap.activity_log[0]["tool"], len(snap.activity_log[0]["target"])), ("other", 64))

    def test_a_source_without_activity_is_all_fine(self):
        t, e, d = small_board()
        s = BoardService(FakeSource(t, e, d, now=NOW), ttl=0.0)
        p = s.now_payload()
        self.assertFalse(p["activity"]["available"])
        self.assertTrue(all(a["activity"] is None for a in p["agents"]))
        self.assertEqual(p["attention"]["quiet_claims"], [])
        self.assertFalse(p["meta"]["activity_available"])

    def test_a_board_without_the_tables_is_asked_again_only_every_30_seconds(self):
        src, clock, s = service(available=False)
        s.snapshot()
        n = len([c for c in src.calls if c == "activity_sessions"])
        self.assertEqual(n, 1)
        clock.t = 10
        s.snapshot()
        self.assertEqual(len([c for c in src.calls if c == "activity_sessions"]), 1)
        clock.t = 31
        s.snapshot()
        self.assertEqual(len([c for c in src.calls if c == "activity_sessions"]), 2)
        self.assertIn("not there", s.now_payload()["activity"]["note"])

    def test_an_error_reading_activity_does_not_break_the_board_and_keeps_what_was_read(self):
        src, clock, s = service([session()], [logrow(1)])
        s.snapshot()
        src.activity_error = SourceError("the database query failed: password=hunter2")
        clock.t = 100
        snap = s.snapshot()
        self.assertEqual(len(snap.activity_log), 1, "the rows read before are kept")
        self.assertNotIn("hunter2", snap.activity_note)
        self.assertEqual(len(snap.tasks), 3)

    def test_now_payload_carries_the_state_and_the_strip(self):
        tasks = [task("t1", "claimed", "alice", created=1000, seq=1)]
        src, clock, s = service([session(last_seen=NOW - 700, tool="Read", target="r.rs")], [logrow(1, at=NOW - 700, calls=4)],
                                tasks=tasks, leases=[lease("t1", until=NOW + 300, hb=NOW - 20)])
        p = s.now_payload()
        a = next(x for x in p["agents"] if x["name"] == "alice")["activity"]
        self.assertEqual(a["state"], "stuck?")
        self.assertEqual(a["age_s"], 700)
        self.assertEqual(sum(a["strip"]), 4)
        self.assertEqual(p["activity"]["quiet_minutes"], 10)
        self.assertEqual(p["activity"]["head"], 1)
        q = p["attention"]["quiet_claims"]
        self.assertEqual([(x["task_id"], x["owner"], x["quiet_s"]) for x in q], [("t1", "alice", 700)])
        self.assertIn("quiet_claims", p["attention"]["rules"])

    def test_the_quiet_limit_is_a_setting_with_bounds(self):
        _, _, s = service(quiet_minutes=3)
        self.assertEqual(s.quiet_s, 180)
        for bad in (0, -1, 1441):
            with self.assertRaises(ValueError):
                BoardService(FakeSource([], [], []), quiet_minutes=bad)

    def test_agent_payload_for_known_and_unknown_agents(self):
        src, clock, s = service([session()], [logrow(1), logrow(2)])
        d = s.agent_payload("alice", None, None, None, 200, None)
        self.assertEqual((d["agent"], d["head"], len(d["rows"]), d["available"]), ("alice", 2, 2, True))
        self.assertIsNone(s.agent_payload("nobody", None, None, None, 200, None))
        self.assertIsNotNone(s.agent_payload("alice", None, None, None, 200, None)["activity"])

    def test_the_scorecard_carries_the_diagnostic_only_when_activity_exists(self):
        src, clock, s = service([session()], [logrow(1)])
        self.assertIsNotNone(s.scorecard_payload((None, None), None, None)["activity"])
        _, _, s2 = service(available=False)
        self.assertIsNone(s2.scorecard_payload((None, None), None, None)["activity"])


class PurgeTests(unittest.TestCase):
    """board activity-purge removes the rows from the database; the console's copy must follow on its next refresh."""

    def purge(self, src, agent):
        src.session_rows = [r for r in src.session_rows if r["agent"] != agent]
        src.log_rows = [r for r in src.log_rows if r["agent"] != agent]

    def setUp(self):
        self.sessions = [session(agent="alice"), session(agent="bob", sid="s9")]
        self.log = [logrow(1, agent="alice", at=NOW - 30), logrow(2, agent="bob", sid="s9", at=NOW - 20),
                    logrow(3, agent="alice", at=NOW - 10)]
        self.src, self.clock, self.s = service(self.sessions, self.log)

    def agents_in_log(self):
        return {r["agent"] for r in self.s.snapshot().activity_log}

    def test_a_purged_agents_rows_leave_the_log_the_agent_screen_and_the_diagnostic(self):
        self.assertEqual(self.agents_in_log(), {"alice", "bob"})
        self.purge(self.src, "alice")
        self.assertEqual(self.agents_in_log(), {"bob"})
        d = self.s.agent_payload("alice", None, None, None, 200, None)
        self.assertTrue(d is None or d["rows"] == [])
        self.assertEqual(self.s.agent_payload("bob", None, None, None, 200, None)["rows_held"], 1)
        score = self.s.scorecard_payload((None, None), None, None)["activity"]
        self.assertEqual(score["agents"], [])

    def test_everything_purged_empties_the_copy_and_it_fills_again(self):
        self.purge(self.src, "alice")
        self.purge(self.src, "bob")
        self.assertEqual(self.s.snapshot().activity_log, [])
        self.src.session_rows = [session(agent="alice")]
        self.src.log_rows = [logrow(10, agent="alice", at=NOW - 5)]
        self.assertEqual([r["id"] for r in self.s.snapshot().activity_log], [10])

    def test_a_purge_followed_by_new_writes_of_the_same_session_drops_the_old_rows(self):
        # the session row was made again after the purge: it starts after the held rows
        self.src.session_rows = [dict(session(agent="alice"), started_at=NOW - 8), session(agent="bob", sid="s9")]
        self.src.log_rows = [logrow(2, agent="bob", sid="s9", at=NOW - 20), logrow(4, agent="alice", at=NOW - 5)]
        ids = [r["id"] for r in self.s.snapshot().activity_log]
        self.assertEqual(ids, [2, 4], "alice's rows 1 and 3 predate her new session row")

    def test_rows_dropped_from_the_copy_are_not_read_again(self):
        self.s.snapshot()  # the copy holds ids 1 to 3
        self.purge(self.src, "alice")
        self.s.snapshot()
        self.src.calls.clear()
        self.s.snapshot()
        # the cursor stays at the newest id read, so the next read asks for ids above 3, not above 2
        self.assertEqual(self.s._activity_cursor, 3)
        self.assertEqual(self.agents_in_log(), {"bob"})

    def test_a_row_that_arrives_before_its_session_row_is_kept(self):
        # a new session writes its log row before the console sees its session row (or between the two reads)
        self.s.snapshot()
        self.src.log_rows.append(logrow(10, agent="carol", sid="n1", at=NOW - 3))
        snap = self.s.snapshot()
        self.assertIn(10, [r["id"] for r in snap.activity_log], "judged in the next refresh, not in the one that read it")
        self.assertEqual(self.s._activity_cursor, 10)
        self.src.session_rows.append(session(agent="carol", sid="n1", started=NOW - 3))
        self.assertIn(10, [r["id"] for r in self.s.snapshot().activity_log], "and kept once its session row exists")

    def test_a_row_whose_session_row_never_comes_is_dropped_on_the_next_refresh(self):
        self.s.snapshot()
        self.src.log_rows.append(logrow(10, agent="carol", sid="n1", at=NOW - 3))
        self.s.snapshot()
        self.assertNotIn(10, [r["id"] for r in self.s.snapshot().activity_log])

    def test_a_cut_session_list_says_nothing_about_absent_sessions(self):
        from console import service as svc
        old = svc.ACTIVITY_SESSIONS
        svc.ACTIVITY_SESSIONS = 1  # the list is cut at its limit: one session is read, bob's is not seen
        try:
            src, clock, s = service(self.sessions, self.log)
            s.snapshot()  # rows read in a refresh are judged in the next one
            self.assertEqual({r["agent"] for r in s.snapshot().activity_log}, {"alice", "bob"})
        finally:
            svc.ACTIVITY_SESSIONS = old

    def test_the_helper_keeps_rows_of_live_sessions(self):
        keep = act.drop_purged(self.log, self.sessions, False)
        self.assertEqual([r["id"] for r in keep], [1, 2, 3])
        self.assertEqual([r["id"] for r in act.drop_purged(self.log, self.sessions[:1], False)], [1, 3])
        self.assertEqual([r["id"] for r in act.drop_purged(self.log, self.sessions[:1], True)], [1, 2, 3])


class ServerTests(unittest.TestCase):
    def setUp(self):
        t, e, d = small_board()
        src = FakeActivitySource(t, e, d, now=NOW, sessions=[session(agent="alice")], log=[logrow(1, calls=3), logrow(2, tool="Bash", calls=1, mix="Bash=1")])
        self.srv = Running(src)
        self.addCleanup(self.srv.stop)

    def status(self, path):
        return self.srv.request("GET", path)[0]

    def test_the_agent_route(self):
        s, d, resp = self.srv.get_json("/api/agent?name=alice")
        self.assertEqual(s, 200)
        self.assertEqual([r["id"] for r in d["rows"]], [2, 1])
        self.assertEqual(d["meta"]["activity_available"], True)
        self.assertIn("default-src 'none'", resp.getheader("Content-Security-Policy"))

    def test_filters_and_cursor_are_validated_strictly(self):
        self.assertEqual(self.status("/api/agent"), 400)
        self.assertEqual(self.status("/api/agent?name=alice&tool=Bash&outcome=ok&task=tparent&limit=5&after=0"), 200)
        for bad in ("name=al%20ice", "name=alice&tool=a%20b", "name=alice&tool=" + "t" * 65, "name=alice&outcome=great",
                    "name=alice&task=a%20b", "name=alice&limit=0", "name=alice&limit=201", "name=alice&after=-1",
                    "name=alice&after=x", "name=alice&name=bob", "name=alice&extra=1", "name=alice%0a", "name=alice&limit=1e2",
                    "name=" + "a" * 65):
            self.assertEqual(self.status("/api/agent?" + bad), 400, bad)

    def test_unknown_agent_is_404(self):
        self.assertEqual(self.status("/api/agent?name=ghost"), 404)

    def test_the_cursor_follows_new_rows(self):
        s, d, _ = self.srv.get_json("/api/agent?name=alice&after=1")
        self.assertEqual([r["id"] for r in d["rows"]], [2])
        self.assertEqual(d["cursor"], 2)

    def test_the_agent_route_is_read_only(self):
        self.assertEqual(self.srv.request("POST", "/api/agent?name=alice", body=b"")[0], 403)  # writes are off

    def test_an_answer_over_the_cap_is_an_error_not_a_download(self):
        old = server_mod.MAX_BODY
        server_mod.MAX_BODY = 300
        try:
            status, _, data = self.srv.request("GET", "/api/agent?name=alice")
        finally:
            server_mod.MAX_BODY = old
        self.assertEqual(status, 500)
        self.assertIn(b"too large", data)

    def test_the_now_payload_stays_under_the_cap_with_many_agents(self):
        sessions = [session(agent="agent-%04d" % i, sid="s", last_seen=NOW - i) for i in range(600)]
        log = [logrow(i + 1, agent="agent-%04d" % (i % 600), at=NOW - i, calls=3) for i in range(5000)]
        t, e, d = small_board()
        src = FakeActivitySource(t, e, d, now=NOW, sessions=sessions, log=log)
        srv = Running(src)
        self.addCleanup(srv.stop)
        status, resp, data = srv.request("GET", "/api/now")
        self.assertEqual(status, 200)
        self.assertLess(len(data), server_mod.MAX_BODY)
        d = json.loads(data.decode("ascii"))
        self.assertEqual(len([a for a in d["agents"] if a["activity"]]), 500, "at most 500 sessions are read")


class SqlTests(unittest.TestCase):
    def test_the_activity_statements_are_plain_selects(self):
        sql = build_select(("id", "agent"), "agent_activity", "last_seen", True, 500)
        self.assertEqual(sql, "SELECT id, agent FROM agent_activity ORDER BY last_seen DESC LIMIT 500;")
        sql = build_select(("id", "agent"), "agent_activity_log", "id", False, 100, 7)
        self.assertEqual(sql, "SELECT id, agent FROM agent_activity_log WHERE id > 7 ORDER BY id LIMIT 100;")

    def test_nothing_else_is_allowed_on_those_tables(self):
        src = PsqlSource("", psql="/nonexistent")
        for bad in ("DELETE FROM agent_activity;", "SELECT * FROM agent_activity;",
                    "SELECT id FROM agent_activity WHERE id > 5 ORDER BY id LIMIT 5;",
                    "SELECT id FROM agent_activity_log; DROP TABLE tasks;",
                    "SELECT id FROM agent_activity_log ORDER BY id LIMIT 5;\n",
                    "SELECT id FROM agent_activity_log WHERE agent = 'x' ORDER BY id LIMIT 5;",
                    "UPDATE agent_activity SET tool = 'x';", "INSERT INTO agent_activity_log VALUES (1);"):
            with self.assertRaises(ValueError, msg=bad):
                src._run(bad)

    def test_missing_tables_and_refused_reads_are_not_errors_for_the_board(self):
        class Boom(PsqlSource):
            def __init__(self, msg):
                super().__init__("", psql="/nonexistent")
                self.msg = msg

            def _rows(self, *a, **k):
                raise SourceError(self.msg)
        for msg in ('the database query failed: ERROR:  42P01: relation "agent_activity" does not exist',
                    "the database query failed: ERROR: 42501: permission denied for table agent_activity"):
            self.assertEqual(Boom(msg).activity_sessions(10), ([], False))
        with self.assertRaises(SourceError):
            Boom("the database query failed: connection refused").activity_sessions(10)

    def test_rows_are_parsed_with_integer_columns(self):
        class Fake(PsqlSource):
            def _run(self, sql):
                return b"a/s\0a\0s\0\0Edit\0f.rs\0ok\0working\0" + b"3\0" + b"99\0" + b"50\0"
        rows, ok = Fake("", psql="/x").activity_sessions(5)
        self.assertTrue(ok)
        self.assertEqual((rows[0]["tool_calls"], rows[0]["last_seen"], rows[0]["started_at"], rows[0]["task_id"]), (3, 99, 50, ""))


class DemoTests(unittest.TestCase):
    def setUp(self):
        self.src = demo.DemoSource(7, 0)
        self.svc = BoardService(self.src, ttl=0.0)

    def test_it_is_deterministic(self):
        a = self.src.activity_latest(10000)
        b = demo.DemoSource(7, 0).activity_latest(10000)
        self.assertEqual(a, b)
        self.assertTrue(a)

    def test_rows_have_the_promised_shape_and_the_ring_limit(self):
        for r in self.src.activity_latest(10000):
            self.assertIsNotNone(act.clean_log(r), r)
            self.assertEqual(act.clean_log(r)["tool"], r["tool"])
        sessions, ok = self.src.activity_sessions(500)
        self.assertTrue(ok)
        for s in sessions:
            self.assertIsNotNone(act.clean_session(s))
        per_agent = {}
        for r in self.src.activity_latest(100000):
            per_agent[r["agent"]] = per_agent.get(r["agent"], 0) + 1
        self.assertTrue(all(n <= demo.ACT_RING + 1 for n in per_agent.values()), per_agent)

    def test_ids_rise_and_the_cursor_works(self):
        ids = [r["id"] for r in self.src.activity_latest(100000)]
        self.assertEqual(ids, sorted(ids))
        self.assertEqual(len(ids), len(set(ids)))
        mid = ids[len(ids) // 2]
        self.assertEqual([r["id"] for r in self.src.activity_after(mid, 100000)], [i for i in ids if i > mid])

    def test_the_demo_shows_a_working_a_quiet_a_stuck_looking_and_an_idle_agent(self):
        p = self.svc.now_payload()
        states = {a["name"]: (a["activity"] or {}).get("state") for a in p["agents"]}
        self.assertEqual(sorted(set(v for v in states.values() if v)), ["idle", "quiet", "stuck?", "working"], states)
        self.assertIsNone(states["docs-bot"], "the stale agent never ran the hook")
        self.assertTrue(p["attention"]["quiet_claims"])
        self.assertIn("quiet_claims", p["attention"]["rules"])

    def test_the_same_clock_gives_the_same_rows_and_a_later_clock_adds_rows(self):
        ticks = [0.0]
        live = demo.DemoSource(7, 4.0, clock=lambda: ticks[0])
        before = {r["id"]: r for r in live.activity_latest(100000)}
        self.assertEqual(before, {r["id"]: r for r in live.activity_latest(100000)})
        ticks[0] = 40.0  # ten more events are shown, so the demo's own clock moves on
        after = {r["id"]: r for r in live.activity_latest(100000)}
        self.assertGreater(max(after), max(before))
        for rid, row in before.items():
            if rid in after:
                self.assertEqual(after[rid], row, "a row never changes once it exists")

    def test_the_agent_screen_data_for_a_demo_agent(self):
        name = next(a["name"] for a in self.svc.now_payload()["agents"] if (a["activity"] or {}).get("state") == "working")
        d = self.svc.agent_payload(name, None, None, None, 200, None)
        self.assertTrue(d["rows"])
        self.assertTrue(d["mix"])
        self.assertGreater(d["active_s"], 0)
        self.assertTrue(d["claimed"])


if __name__ == "__main__":
    unittest.main()

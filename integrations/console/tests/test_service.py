import unittest

import helpers  # noqa: F401
from helpers import FakeSource, decision, event, small_board, task

from console import service as svc
from console.service import BoardService


class Clock:
    def __init__(self):
        self.t = 0.0

    def __call__(self):
        return self.t


def make(ttl=1.0, **kw):
    tasks, events, decisions = small_board()
    src = FakeSource(tasks, events, decisions, now=5000)
    clock = Clock()
    return src, clock, BoardService(src, ttl=ttl, clock=clock, **kw)


class RefreshTests(unittest.TestCase):
    def test_first_snapshot_loads_everything(self):
        src, clock, s = make()
        snap = s.snapshot()
        self.assertEqual(len(snap.tasks), 3)
        self.assertEqual(snap.head, 8)
        self.assertEqual(len(snap.decisions), 1)
        self.assertEqual(snap.now, 5000)
        self.assertEqual(src.calls, ["tasks", "events_latest", "decisions_latest", "leases"])

    def test_ttl_limits_refreshes(self):
        src, clock, s = make(ttl=1.0)
        s.snapshot()
        n = len(src.calls)
        clock.t = 0.5
        s.snapshot()
        self.assertEqual(len(src.calls), n)  # served from memory
        clock.t = 1.0
        s.snapshot()
        self.assertGreater(len(src.calls), n)

    def test_later_refreshes_read_only_newer_events(self):
        src, clock, s = make(ttl=0.0)
        s.snapshot()
        src.calls.clear()
        src.event_rows.append(event(9, "tparent", "alice", "note", 3000, "new"))
        snap = s.snapshot()
        self.assertEqual(snap.head, 9)
        self.assertEqual(src.calls, ["tasks", "events_after", "decisions_after", "leases"])
        self.assertEqual(len(snap.events), 9)

    def test_new_decisions_are_picked_up(self):
        src, clock, s = make(ttl=0.0)
        s.snapshot()
        src.decision_rows.append(decision(9, "tparent", "alice", 3000))
        self.assertEqual(len(s.snapshot().decisions), 2)

    def test_duplicates_from_the_source_are_ignored(self):
        src, clock, s = make(ttl=0.0)
        s.snapshot()
        src.event_rows.append(event(9, "tparent", "alice", "note", 3000))
        orig = src.events_after
        src.events_after = lambda after, limit: orig(after - 1, limit)  # re-delivers event 8
        snap = s.snapshot()
        self.assertEqual([e["id"] for e in snap.events], list(range(1, 10)))

    def test_paging_catches_up_over_several_pages(self):
        src, clock, s = make(ttl=0.0)
        s.snapshot()
        for i in range(9, 9 + svc.PAGE * 2 + 5):
            src.event_rows.append(event(i, "tparent", "alice", "note", 3000, "n"))
        snap = s.snapshot()
        self.assertEqual(snap.head, 8 + svc.PAGE * 2 + 5)

    def test_source_error_propagates_and_keeps_the_old_snapshot(self):
        src, clock, s = make(ttl=0.0)
        s.snapshot()
        src.error = RuntimeError("down")
        with self.assertRaises(RuntimeError):
            s.snapshot()
        src.error = None
        clock.t += 3.0  # past the failure backoff
        self.assertEqual(s.snapshot().head, 8)

    def test_truncation_is_reported(self):
        src, clock, s = make()
        old = svc.EVENT_WINDOW
        svc.EVENT_WINDOW = 5
        try:
            snap = s.snapshot()
        finally:
            svc.EVENT_WINDOW = old
        self.assertTrue(snap.truncated_events)
        self.assertEqual(len(snap.events), 5)
        self.assertEqual(snap.events[0]["id"], 4)  # the newest five
        meta = s.meta(snap)
        self.assertTrue(meta["truncated"]["events"])

    def test_window_trims_memory_when_it_doubles(self):
        src, clock, s = make(ttl=0.0)
        old = svc.EVENT_WINDOW
        svc.EVENT_WINDOW = 10
        try:
            s.snapshot()
            for i in range(9, 40):
                src.event_rows.append(event(i, "tparent", "alice", "note", 3000, "n"))
            snap = s.snapshot()
        finally:
            svc.EVENT_WINDOW = old
        self.assertLessEqual(len(snap.events), 20)
        self.assertTrue(snap.truncated_events)
        self.assertEqual(snap.head, 39)


class ConfigTests(unittest.TestCase):
    def test_thresholds_are_validated(self):
        src = FakeSource()
        for kw in ({"stale_minutes": 0}, {"stale_minutes": 10**6}, {"open_hours": 0}, {"open_hours": 10**6}):
            with self.assertRaises(ValueError, msg=str(kw)):
                BoardService(src, **kw)

    def test_thresholds_are_reported_and_used(self):
        src = FakeSource([task("t", "claimed", "a", created=0)], [event(1, "t", "a", "claim", 100)], now=100 + 600)
        s = BoardService(src, stale_minutes=5, ttl=0.0)
        d = s.now_payload()
        self.assertEqual(d["meta"]["stale_minutes"], 5)
        self.assertEqual(len(d["attention"]["stale"]), 1)  # 600 s idle > 5 min
        s2 = BoardService(src, stale_minutes=11, ttl=0.0)
        self.assertEqual(s2.now_payload()["attention"]["stale"], [])


class PayloadTests(unittest.TestCase):
    def test_empty_board(self):
        s = BoardService(FakeSource(now=10), ttl=0.0)
        d = s.now_payload()
        self.assertEqual(d["agents"], [])
        self.assertEqual(d["counts"]["open"], 0)
        self.assertEqual(d["head"], 0)
        feed = s.feed_payload(None, None, 10, None, None, None)
        self.assertEqual((feed["events"], feed["cursor"], feed["has_more"]), ([], 0, False))
        score = s.scorecard_payload((None, None), None, None)
        self.assertEqual(score["task_time_range"], {"min": None, "max": None})
        self.assertEqual(score["agents"], {})

    def test_scorecard_lists_claimants_and_filters_by_agent(self):
        src, clock, s = make(ttl=0.0)
        d = s.scorecard_payload((None, None), None, None)
        self.assertEqual(d["agent_names"], ["alice", "bob"])
        self.assertEqual(sorted(d["agents"]), ["alice", "bob"])
        only = s.scorecard_payload((None, None), None, "bob")
        self.assertEqual(list(only["agents"]), ["bob"])

    def test_scorecard_is_cached_per_snapshot(self):
        src, clock, s = make(ttl=1000.0)
        a = s.scorecard_payload((None, None), None, None)
        b = s.scorecard_payload((None, None), None, None)
        self.assertIs(a, b)
        c = s.scorecard_payload((0, 10), None, None)
        self.assertIsNot(a, c)

    def test_scorecard_cache_drops_on_refresh(self):
        src, clock, s = make(ttl=0.0)
        a = s.scorecard_payload((None, None), None, None)
        src.event_rows.append(event(9, "tparent", "alice", "note", 3000))
        b = s.scorecard_payload((None, None), None, None)
        self.assertIsNot(a, b)

    def test_feed_reset_when_cursor_is_ahead(self):
        src, clock, s = make(ttl=0.0)
        d = s.feed_payload(99, None, 10, None, None, None)
        self.assertTrue(d["reset"])
        self.assertEqual(d["cursor"], 8)

    def test_demo_flag_comes_from_the_source(self):
        src = FakeSource()
        src.demo = True
        s = BoardService(src, ttl=0.0)
        self.assertTrue(s.demo)
        self.assertEqual(s.now_payload()["meta"]["banner"], "DEMO DATA, not your board")


if __name__ == "__main__":
    unittest.main()

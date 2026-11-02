import unittest

import helpers  # noqa: F401
from helpers import decision, event, small_board, task

from console import model

STALE = 1800
OPEN_S = 86400


def derive(tasks, events, decisions=(), now=100000):
    facts = model.task_facts(tasks, events, list(decisions))
    return facts, model.build_agents(tasks, events, facts, now, STALE)


def agent(agents, name):
    return {a["name"]: a for a in agents}[name]


class FactsAndCountsTests(unittest.TestCase):
    def test_facts_for_a_finished_task(self):
        tasks, events, decisions = small_board()
        facts = {f.task["id"]: f for f in model.task_facts(tasks, events, decisions)}
        f = facts["tchild1"]
        self.assertEqual(f.claims, [("bob", 1400, 5)])
        self.assertEqual((f.first_claim_ts, f.done_ts, f.notes, f.decisions), (1400, 2200, 1, 1))
        self.assertEqual(f.claimant, "bob")
        self.assertEqual(f.last_activity, 2200)

    def test_open_task_has_no_claimant(self):
        tasks, events, decisions = small_board()
        f = {x.task["id"]: x for x in model.task_facts(tasks, events, decisions)}["tchild2"]
        self.assertIsNone(f.claimant)
        self.assertIsNone(f.first_claim_ts)

    def test_owner_is_the_claimant_when_the_claim_event_is_not_loaded(self):
        t = [task("a", "claimed", "zed")]
        f = model.task_facts(t, [], [])[0]
        self.assertEqual(f.claimant, "zed")
        self.assertIsNone(f.first_claim_ts)

    def test_updated_at_counts_as_activity(self):
        t = [task("a", "claimed", "zed", created=10, updated=900)]
        self.assertEqual(model.task_facts(t, [], [])[0].last_activity, 900)

    def test_counts_by_state_has_an_other_bucket(self):
        t = [task("a", "open"), task("b", "claimed"), task("c", "done"), task("d", "weird")]
        self.assertEqual(model.counts_by_state(t), {"open": 1, "claimed": 1, "done": 1, "dead": 0, "cancelled": 0,
                          "awaiting_approval": 0, "other": 1})
        self.assertEqual(model.counts_by_state([]), {"open": 0, "claimed": 0, "done": 0, "dead": 0, "cancelled": 0,
                          "awaiting_approval": 0, "other": 0})

    def test_clip(self):
        self.assertEqual(model.clip(None), "")
        self.assertEqual(len(model.clip("x" * 5000)), 4096)
        self.assertEqual(model.clip("abcdef", 3), "abc")


class AgentTests(unittest.TestCase):
    def test_working_when_the_claim_has_recent_activity(self):
        tasks = [task("t1", "claimed", "ann", created=0)]
        events = [event(1, "t1", "ann", "claim", 99000)]
        _, agents = derive(tasks, events, now=100000)
        a = agent(agents, "ann")
        self.assertEqual(a["state"], "working")
        self.assertEqual(a["current_task"]["id"], "t1")
        self.assertEqual(a["last_event"]["age_s"], 1000)

    def test_stale_when_every_claim_is_quiet(self):
        tasks = [task("t1", "claimed", "ann", created=0)]
        events = [event(1, "t1", "ann", "claim", 100)]
        _, agents = derive(tasks, events, now=100000)
        self.assertEqual(agent(agents, "ann")["state"], "stale")
        self.assertEqual(agent(agents, "ann")["current_task"]["id"], "t1")

    def test_working_wins_if_any_claim_is_live(self):
        tasks = [task("old", "claimed", "ann", created=0), task("new", "claimed", "ann", created=0)]
        events = [event(1, "old", "ann", "claim", 100), event(2, "new", "ann", "claim", 99900)]
        _, agents = derive(tasks, events, now=100000)
        a = agent(agents, "ann")
        self.assertEqual(a["state"], "working")
        self.assertEqual(a["current_task"]["id"], "new")
        self.assertEqual(a["claimed"], ["new", "old"])

    def test_idle_when_nothing_is_claimed(self):
        tasks = [task("t1", "done", "ann", created=0)]
        events = [event(1, "t1", "ann", "claim", 10), event(2, "t1", "ann", "done", 20)]
        _, agents = derive(tasks, events)
        a = agent(agents, "ann")
        self.assertEqual(a["state"], "idle")
        self.assertIsNone(a["current_task"])
        self.assertEqual(a["tasks_done"], 1)

    def test_owner_without_events_is_still_an_agent(self):
        tasks = [task("t1", "claimed", "ghost", created=0, updated=99990)]
        _, agents = derive(tasks, [])
        a = agent(agents, "ghost")
        self.assertEqual(a["state"], "working")  # task.updated_at is recent
        self.assertIsNone(a["last_event"])
        self.assertEqual(a["events"], 0)

    def test_tasks_done_counts_distinct_tasks_by_the_done_author(self):
        tasks = [task("a", "done"), task("b", "done"), task("c", "done")]
        events = [event(1, "a", "x", "done", 5), event(2, "a", "x", "done", 6),
                  event(3, "b", "x", "done", 7), event(4, "c", "y", "done", 8)]
        _, agents = derive(tasks, events)
        self.assertEqual(agent(agents, "x")["tasks_done"], 2)
        self.assertEqual(agent(agents, "y")["tasks_done"], 1)

    def test_order_is_stale_then_working_then_idle(self):
        tasks = [task("a", "claimed", "w", created=0), task("b", "claimed", "s", created=0)]
        events = [event(1, "a", "w", "claim", 99999), event(2, "b", "s", "claim", 5),
                  event(3, "a", "i", "note", 1)]
        _, agents = derive(tasks, events)
        self.assertEqual([a["name"] for a in agents], ["s", "w", "i"])

    def test_no_data_no_agents(self):
        self.assertEqual(derive([], [])[1], [])

    def test_last_event_is_the_highest_id(self):
        events = [event(5, "a", "ann", "note", 10), event(9, "a", "ann", "note", 20)]
        _, agents = derive([task("a")], events, now=30)
        self.assertEqual(agent(agents, "ann")["last_event"]["id"], 9)
        self.assertEqual(agent(agents, "ann")["last_event"]["age_s"], 10)


class AttentionTests(unittest.TestCase):
    def attn(self, tasks, events, now=100000):
        facts = model.task_facts(tasks, events, [])
        return model.attention(tasks, facts, now, STALE, OPEN_S)

    def test_stale_boundary(self):
        tasks = [task("t1", "claimed", "ann", created=0)]
        events = [event(1, "t1", "ann", "claim", 98200)]
        self.assertEqual(self.attn(tasks, events, now=100000)["stale"], [])  # idle exactly 1800
        out = self.attn(tasks, events, now=100001)["stale"]
        self.assertEqual([x["task_id"] for x in out], ["t1"])
        self.assertEqual(out[0]["idle_s"], 1801)
        self.assertEqual(out[0]["owner"], "ann")

    def test_a_note_refreshes_a_claim(self):
        tasks = [task("t1", "claimed", "ann", created=0)]
        events = [event(1, "t1", "ann", "claim", 100), event(2, "t1", "ann", "note", 99500)]
        self.assertEqual(self.attn(tasks, events)["stale"], [])

    def test_stale_sorted_longest_idle_first(self):
        tasks = [task("a", "claimed", "x", created=0), task("b", "claimed", "x", created=0)]
        events = [event(1, "a", "x", "claim", 500), event(2, "b", "x", "claim", 50)]
        self.assertEqual([x["task_id"] for x in self.attn(tasks, events)["stale"]], ["b", "a"])

    def test_double_claim_needs_two_authors(self):
        tasks = [task("t1", "claimed", "ann", created=0), task("t2", "claimed", "ann", created=0)]
        events = [event(1, "t1", "ann", "claim", 99999), event(2, "t1", "bob", "claim", 99999),
                  event(3, "t2", "ann", "claim", 99998), event(4, "t2", "ann", "claim", 99999)]
        out = self.attn(tasks, events)["double_claims"]
        self.assertEqual(len(out), 1)
        self.assertEqual(out[0]["task_id"], "t1")
        self.assertEqual(out[0]["claimants"], ["ann", "bob"])

    def test_owner_mismatch(self):
        tasks = [task("t1", "claimed", "ann", created=0)]
        events = [event(1, "t1", "bob", "claim", 99999)]
        out = self.attn(tasks, events)["owner_mismatch"]
        self.assertEqual((out[0]["owner"], out[0]["last_claim_by"]), ("ann", "bob"))

    def test_open_too_long(self):
        tasks = [task("old", "open", created=100000 - OPEN_S - 1), task("fresh", "open", created=99000),
                 task("edge", "open", created=100000 - OPEN_S)]
        out = self.attn(tasks, [])["open_too_long"]
        self.assertEqual([x["task_id"] for x in out], ["old"])

    def test_claimed_and_done_tasks_are_never_open_too_long(self):
        tasks = [task("a", "claimed", "x", created=0, updated=99999), task("b", "done", created=0)]
        self.assertEqual(self.attn(tasks, [])["open_too_long"], [])

    def test_rules_are_stated(self):
        r = self.attn([], [])["rules"]
        self.assertIn("30 minutes", r["stale"])
        self.assertIn("24 hours", r["open_too_long"])
        self.assertIn("direct write", r["double_claims"])


class TreeTests(unittest.TestCase):
    def by_id(self, tasks):
        return {t["id"]: t for t in tasks}

    def test_chain_is_root_first(self):
        tasks = [task("a"), task("b", parent="a"), task("c", parent="b")]
        out = model.parent_chain("c", self.by_id(tasks))
        self.assertEqual([p["id"] for p in out["chain"]], ["a", "b"])
        self.assertIsNone(out["issue"])

    def test_root_has_no_chain(self):
        self.assertEqual(model.parent_chain("a", self.by_id([task("a")])), {"chain": [], "issue": None})

    def test_orphan_parent_ends_the_chain(self):
        tasks = [task("b", parent="ghost")]
        out = model.parent_chain("b", self.by_id(tasks))
        self.assertEqual(out["issue"], "missing_parent")
        self.assertEqual(out["chain"], [{"id": "ghost", "title": None, "state": None, "missing": True}])

    def test_two_task_cycle_terminates(self):
        tasks = [task("a", parent="b"), task("b", parent="a")]
        out = model.parent_chain("a", self.by_id(tasks))
        self.assertEqual(out["issue"], "cycle")
        self.assertEqual([p["id"] for p in out["chain"]], ["b"])

    def test_self_parent_terminates(self):
        out = model.parent_chain("a", self.by_id([task("a", parent="a")]))
        self.assertEqual((out["issue"], out["chain"]), ("cycle", []))

    def test_longer_cycle_terminates(self):
        tasks = [task("a", parent="c"), task("b", parent="a"), task("c", parent="b")]
        out = model.parent_chain("a", self.by_id(tasks))
        self.assertEqual(out["issue"], "cycle")
        self.assertLessEqual(len(out["chain"]), 3)

    def test_very_deep_chain_is_cut(self):
        n = model.MAX_CHAIN + 20
        tasks = [task("n0")] + [task("n%d" % i, parent="n%d" % (i - 1)) for i in range(1, n)]
        out = model.parent_chain("n%d" % (n - 1), self.by_id(tasks))
        self.assertEqual(out["issue"], "too_deep")
        self.assertEqual(len(out["chain"]), model.MAX_CHAIN)

    def test_unknown_task_has_an_empty_chain(self):
        self.assertEqual(model.parent_chain("nope", {}), {"chain": [], "issue": None})

    def test_children_sorted_by_priority_then_creation(self):
        tasks = [task("p"), task("c1", parent="p", priority=0, seq=2), task("c2", parent="p", priority=5, seq=9),
                 task("c3", parent="p", priority=0, seq=1), task("other", parent="x")]
        kids = model.children_of("p", tasks)
        self.assertEqual([k["id"] for k in kids], ["c2", "c3", "c1"])

    def test_a_self_parent_is_not_its_own_child(self):
        self.assertEqual(model.children_of("a", [task("a", parent="a")]), [])

    def test_task_view(self):
        tasks, events, decisions = small_board()
        v = model.task_view("tchild1", tasks, events, decisions, 5000)
        self.assertEqual([p["id"] for p in v["parents"]], ["tparent"])
        self.assertEqual([e["id"] for e in v["events"]], [2, 5, 6, 7, 8])  # in order, the add event first
        self.assertEqual([d["id"] for d in v["decisions"]], [7])
        self.assertEqual(v["children"], [])
        top = model.task_view("tparent", tasks, events, decisions, 5000)
        self.assertEqual([k["id"] for k in top["children"]], ["tchild1", "tchild2"])

    def test_task_view_of_unknown_task(self):
        self.assertIsNone(model.task_view("zzz", [], [], [], 0))

    def test_task_view_clips_long_text(self):
        t = [task("a", body="y" * 9000)]
        e = [event(1, "a", "x", "note", 1, "z" * 9000)]
        v = model.task_view("a", t, e, [], 5)
        self.assertEqual(len(v["task"]["body"]), 4096)
        self.assertEqual(len(v["events"][0]["body"]), 4096)


class FeedTests(unittest.TestCase):
    def setUp(self):
        self.tasks, self.events, _ = small_board()
        self.by_id = {t["id"]: t for t in self.tasks}

    def page(self, **kw):
        args = dict(after=None, before=None, limit=100, agent=None, kind=None, task=None)
        args.update(kw)
        return model.feed_page(self.events, self.by_id, **args)

    def test_newest_first(self):
        self.assertEqual([e["id"] for e in self.page()], [8, 7, 6, 5, 4, 3, 2, 1])

    def test_after_keeps_only_newer(self):
        self.assertEqual([e["id"] for e in self.page(after=6)], [8, 7])
        self.assertEqual(self.page(after=8), [])
        self.assertEqual([e["id"] for e in self.page(after=0)][:1], [8])

    def test_before_pages_backwards(self):
        self.assertEqual([e["id"] for e in self.page(before=4)], [3, 2, 1])
        self.assertEqual([e["id"] for e in self.page(before=8, limit=2)], [7, 6])

    def test_limit(self):
        self.assertEqual(len(self.page(limit=3)), 3)

    def test_filters(self):
        self.assertEqual({e["author"] for e in self.page(agent="bob")}, {"bob"})
        self.assertEqual([e["id"] for e in self.page(kind="claim")], [5, 4])
        self.assertEqual({e["task_id"] for e in self.page(task="tparent")}, {"tparent"})
        self.assertEqual([e["id"] for e in self.page(agent="bob", kind="note")], [6])
        self.assertEqual(self.page(agent="nobody"), [])

    def test_rows_carry_the_task_title(self):
        self.assertEqual(self.page(limit=1)[0]["task_title"], "title of tchild1")
        orphan = model.feed_page([event(1, "gone", "x", "note", 1)], {}, None, None, 5, None, None, None)
        self.assertIsNone(orphan[0]["task_title"])

    def test_filter_does_not_stop_the_after_cutoff_early(self):
        # events 5..8 are newer than 4, and only 6 matches the filter
        self.assertEqual([e["id"] for e in self.page(after=4, kind="note")], [6])


if __name__ == "__main__":
    unittest.main()

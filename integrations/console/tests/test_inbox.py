"""The inbox: only what waits for a person, with what a person needs to decide."""

import unittest

import helpers  # noqa: F401
from actions_helpers import ACTOR, board
from helpers import event, lease, task

from console import inbox, model
from console.service import BoardService


def build(tasks, events, leases, quiet=(), now=100000, open_s=86400, actor=ACTOR):
    by_id = {t["id"]: t for t in tasks}
    queue = model.build_queue(tasks, leases, now)
    return inbox.build_inbox(by_id, events, queue, list(quiet), now, open_s, actor)


class Contents(unittest.TestCase):
    def setUp(self):
        self.service = BoardService(board(), ttl=0.0)
        self.d = self.service.inbox_payload()

    def test_only_what_needs_a_person_is_listed(self):
        ids = [i["id"] for i in self.d["items"]]
        self.assertEqual(ids, ["tapprove", "tmine", "tdead"])  # not the plain open task, not the healthy claim
        self.assertEqual(self.d["totals"], {"approval": 2, "dead": 1, "quiet": 0, "blocked": 0})
        self.assertEqual(self.d["total"], 3)

    def test_approvals_come_first_oldest_first_then_dead_letters(self):
        self.assertEqual([i["kind"] for i in self.d["items"]], ["approval", "approval", "dead"])
        self.assertEqual([i["age_s"] for i in self.d["items"][:2]], [99000, 98600])

    def test_an_item_carries_the_text_the_person_must_read_and_the_hash_of_it(self):
        a = self.d["items"][0]
        self.assertEqual(a["title"], "Raise the retry delay")
        self.assertEqual(a["body"], "Proposed: wait 30 s, not 5 s.")
        self.assertEqual(a["sha"], "06b004e02f7816e31814914e79b237bc3fa0f5e163ad859376ffc8bdc5dff4e5")
        self.assertEqual((a["created_by"], a["state"]), ("maintainer", "awaiting_approval"))
        self.assertEqual(a["summary"], "waiting for approval for 27 h")

    def test_the_last_events_are_shown_oldest_first_without_the_fenced_noise(self):
        d = self.d["items"][2]
        self.assertEqual([(n["id"], n["author"], n["kind"], n["body"]) for n in d["notes"]],
                         [(2, "maintainer", "add", "Export nightly"), (3, "bob", "fail", "disk full")])

    def test_a_dead_letter_says_why(self):
        self.assertEqual(self.d["items"][2]["summary"], "dead after 3 of 3 attempts: disk full")

    def test_what_each_kind_allows(self):
        self.assertEqual(self.d["items"][0]["allowed"], ["approve", "reject", "cancel", "priority", "note"])
        self.assertEqual(self.d["items"][2]["allowed"], ["requeue", "cancel", "priority", "note"])

    def test_you_produced_is_a_hint_for_the_task_the_actor_created(self):
        by_id = {i["id"]: i["you_produced"] for i in self.d["items"]}
        self.assertEqual(by_id, {"tapprove": False, "tmine": False, "tdead": False})  # no actor configured here

    def test_with_an_actor_the_task_made_under_the_same_plain_name_is_flagged(self):
        self.service.writes = {"enabled": True, "actor": "human.alice"}
        by_id = {i["id"]: i["you_produced"] for i in self.service.inbox_payload()["items"]}
        self.assertEqual(by_id, {"tapprove": False, "tmine": True, "tdead": False})

    def test_the_rules_are_stated_for_every_kind(self):
        self.assertEqual(sorted(self.d["rules"]), ["approval", "blocked", "dead", "quiet"])

    def test_the_payload_has_the_meta_with_the_write_state(self):
        self.assertEqual(self.d["meta"]["writes"], {"enabled": False, "actor": None})


class Kinds(unittest.TestCase):
    def test_a_quiet_claim_is_listed_with_its_owner_and_how_long(self):
        tasks = [task("tq", "claimed", owner="bob", created=1000, seq=1)]
        d = build(tasks, [event(1, "tq", "bob", "claim", 1100)], [lease("tq", until=100600, hb=99990)],
                  quiet=[{"task_id": "tq", "title": "x", "owner": "bob", "quiet_s": 1500, "heartbeat_age_s": 10}])
        (i,) = d["items"]
        self.assertEqual((i["kind"], i["owner"]), ("quiet", "bob"))
        self.assertEqual(i["summary"], "held by bob, no tool activity for 25 min while its lease is alive")
        self.assertEqual(i["allowed"], ["requeue", "cancel", "note"])

    def test_blocked_for_ever_is_listed_and_blocked_briefly_is_not(self):
        tasks = [task("tdeadb", "dead", seq=1, created=1000), task("tforever", "open", seq=2, created=99000),
                 task("twaiting", "open", seq=3, created=99000), task("tbusy", "claimed", owner="x", seq=4, created=1000),
                 task("tlong", "open", seq=5, created=1000)]
        leases = [lease("tforever", blocked="tdeadb"), lease("twaiting", blocked="tbusy"), lease("tlong", blocked="tbusy"),
                  lease("tdeadb", attempts=3), lease("tbusy", until=100500, hb=99990)]
        d = build(tasks, [], leases)
        ids = [(i["kind"], i["id"]) for i in d["items"]]
        self.assertIn(("blocked", "tforever"), ids)
        self.assertIn(("blocked", "tlong"), ids)  # waited longer than the open-task threshold
        self.assertNotIn("twaiting", [i for _, i in ids])  # waits for a live claim and only 1000 s: not in the inbox at all
        self.assertNotIn("tbusy", [i for _, i in ids])  # a healthy claim is not either
        forever = next(i for i in d["items"] if i["id"] == "tforever")
        self.assertEqual(forever["summary"], "can never run: tdeadb (dead)")
        self.assertEqual(forever["allowed"], ["cancel", "priority", "note"])

    def test_the_count_for_the_nav_link_matches_the_list(self):
        tasks = [task("ta", "awaiting_approval", seq=1), task("tb", "dead", seq=2)]
        leases = [lease("tb", attempts=3)]
        queue = model.build_queue(tasks, leases, 100000)
        self.assertEqual(inbox.total_items(queue, [], 100000, 86400), 2)
        self.assertEqual(inbox.total_items(queue, [{"task_id": "x", "owner": "o", "quiet_s": 5}], 100000, 86400), 3)

    def test_task_text_is_cut_not_trusted(self):
        t = task("ta", "awaiting_approval", seq=1, body="x" * 9000)
        d = build([t], [], [])
        (i,) = d["items"]
        self.assertEqual(len(i["body"]), 4096)
        self.assertTrue(i["body_clipped"])
        # the hash is of the whole text, not of what is shown
        import hashlib
        self.assertEqual(i["sha"], hashlib.sha256(("title of ta\n" + "x" * 9000).encode()).hexdigest())

    def test_at_most_five_events_per_item_and_hostile_text_stays_text(self):
        t = task("ta", "awaiting_approval", seq=1)
        evs = [event(n, "ta", "bot", "note", 1000 + n, "<img src=x onerror=alert(1)>" * (3 if n == 7 else 1)) for n in range(1, 9)]
        (i,) = build([t], evs, [])["items"]
        self.assertEqual([n["id"] for n in i["notes"]], [4, 5, 6, 7, 8])
        self.assertTrue(i["notes"][3]["body"].startswith("<img"))  # data, for textContent

    def test_a_cap_on_the_list(self):
        tasks = [task("t%03d" % n, "awaiting_approval", seq=n, created=1000 + n) for n in range(inbox.ITEM_LIMIT + 5)]
        queue = model.build_queue(tasks, [], 100000)
        d = inbox.build_inbox({t["id"]: t for t in tasks}, [], queue, [], 100000, 86400, "")
        self.assertLessEqual(len(d["items"]), inbox.ITEM_LIMIT)


if __name__ == "__main__":
    unittest.main()

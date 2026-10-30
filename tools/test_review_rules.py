#!/usr/bin/env python3
"""The review rule both mergers share: the reviewed label and the Dependabot exemption."""
import unittest

import merge_pr
import merge_queue
import review_rules as r


def info(labels=(), login="someone", is_bot=False, branch="feature", title="feat: x"):
    return {"labels": [{"name": n} for n in labels], "author": {"login": login, "is_bot": is_bot},
            "headRefName": branch, "title": title}


def bot(**kw):
    base = dict(login="app/dependabot", is_bot=True, branch="dependabot/cargo/x", title="chore(deps): bump x")
    base.update(kw)
    return info(**base)


class ReviewRules(unittest.TestCase):
    def test_label_decides_for_ordinary_pull_requests(self):
        self.assertFalse(r.lacks_review(info(["reviewed"])))
        self.assertTrue(r.lacks_review(info([])))
        self.assertTrue(r.lacks_review(info(["hold", "type: ci"])))
        self.assertTrue(r.lacks_review({}))
        self.assertTrue(r.lacks_review({"labels": None, "author": None}))

    def test_empty_label_name_turns_the_rule_off(self):
        self.assertFalse(r.lacks_review(info([]), ""))

    def test_dependabot_exemption_needs_all_four_facts(self):
        for login in r.DEPENDABOT_LOGINS:
            self.assertFalse(r.lacks_review(bot(login=login)), login)
        for title in ("build(deps): x", "build(deps-dev): x", "chore(deps)!: x", "chore(deps-dev): x"):
            self.assertFalse(r.lacks_review(bot(title=title)), title)
        for change in ({"login": "dependabot-fan"}, {"login": "not-dependabot"}, {"is_bot": False},
                       {"login": "renovate[bot]"}, {"branch": "feature"}, {"branch": "xdependabot/x"},
                       {"title": "feat: x"}, {"title": "chore(deps-foo): x"}, {"title": ""}):
            self.assertTrue(r.lacks_review(bot(**change)), change)

    def test_dependabot_with_the_label_still_passes(self):
        self.assertFalse(r.lacks_review(bot(login="dependabot-fan", labels=["reviewed"])))

    def test_both_mergers_use_this_module(self):
        self.assertIs(merge_queue.lacks_review, r.lacks_review)
        self.assertIs(merge_queue.is_dependency_update, r.is_dependency_update)
        self.assertIs(merge_pr.lacks_review, r.lacks_review)
        self.assertEqual(merge_pr.DEFAULT_ALSO_REQUIRE, ("risk-gate",))
        self.assertEqual(merge_queue.DEFAULT_ALSO_REQUIRE, ("risk-gate",))


if __name__ == "__main__":
    unittest.main()

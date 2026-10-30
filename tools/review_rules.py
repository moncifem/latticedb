"""Review rules shared by the two mergers (merge_queue.py and merge_pr.py).

Both refuse a pull request without the owner's `reviewed` label, except a Dependabot dependency update,
and both require the `risk-gate` check on top of branch protection. One copy keeps them from drifting
(the single-PR merger once skipped both rules)."""
import re

# The owner's label after an independent review. Required whatever paths a pull request touches, because
# the risk-gate path list can miss a risky change (PR 412).
DEFAULT_REVIEWED_LABEL = "reviewed"
DEPENDABOT_LOGINS = ("app/dependabot", "dependabot[bot]")
DEPENDENCY_TITLE_RE = re.compile(r"\A(?:build|chore)\(deps(?:-dev)?\)[:!]")
# Required by the mergers even if branch protection does not list it: a red or missing risk gate
# (the owner's `reviewed` label on risky paths) must never be merged over.
DEFAULT_ALSO_REQUIRE = ("risk-gate",)


def label_names(info):
    return {label.get("name") for label in info.get("labels") or [] if isinstance(label, dict)}


def is_dependency_update(info):
    """Dependabot's own pull request: its exact login, a bot account, its branch prefix and title shape.

    The test is narrow on purpose. A human named `dependabot-fan`, another bot app, or any other title
    or branch does not qualify. The exemption also does not look at paths: a Dependabot pull request
    that touches `.github/**` still needs a green `risk-gate`, which is a required check."""
    author = info.get("author") or {}
    return (
        author.get("is_bot") is True
        and author.get("login") in DEPENDABOT_LOGINS
        and str(info.get("headRefName") or "").startswith("dependabot/")
        and bool(DEPENDENCY_TITLE_RE.match(str(info.get("title") or "")))
    )


def lacks_review(info, reviewed_label=DEFAULT_REVIEWED_LABEL):
    """True when the pull request needs the reviewed label and does not have it.

    An empty `reviewed_label` turns the rule off."""
    return bool(reviewed_label) and reviewed_label not in label_names(info) and not is_dependency_update(info)

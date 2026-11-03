"""The inbox: what is waiting for a person.

Only needs-attention items, not every running task: a task that waits for approval, a dead letter, a claimed task
whose lease is alive while its owner's tool activity stopped, and an open task that waits for blockers that will
never finish (or has waited longer than the open-task threshold). Each item carries what a person needs before
deciding: the plain-text body, who produced it, its last events, and the content hash that a decision is bound to
(actions.task_sha). Pure functions over the snapshot, like model.py.
"""

from __future__ import annotations

from . import actions as act_mod
from .model import clip

KINDS = ("approval", "dead", "quiet", "blocked")
ITEM_LIMIT = 200
NOTES_PER_ITEM = 5
NOTE_CLIP = 500
BODY_CLIP = 4096

RULES = {
    "approval": "created with --needs-approval; nothing runs until a person approves it",
    "dead": "used all its attempts; it needs a person to requeue or cancel it",
    "quiet": "claimed with a live lease while its owner's tool activity stopped (self-reported by the agent's own hook, "
             "so this says what was reported, not that the agent is stuck)",
    "blocked": "open and waiting for a blocker that is dead, cancelled or missing, a cycle, or for longer than the "
               "open-task threshold",
}
ALLOWED = {
    "approval": ("approve", "reject", "cancel", "priority", "note"),
    "dead": ("requeue", "cancel", "priority", "note"),
    "quiet": ("requeue", "cancel", "note"),
    "blocked": ("cancel", "priority", "note"),
}


def _recent_events(events: list, wanted: set) -> dict:
    out = {tid: [] for tid in wanted}
    for e in reversed(events):
        lst = out.get(e["task_id"])
        if lst is not None and len(lst) < NOTES_PER_ITEM and e["kind"] != "fenced":
            lst.append({"id": e["id"], "author": e["author"], "kind": e["kind"],
                        "body": clip(e["body"], NOTE_CLIP), "at": e["created_at"]})
    for lst in out.values():
        lst.reverse()
    return out


def _raw_items(queue: dict, quiet: list, now: int, open_s: int) -> list:
    raw = []
    for r in queue["awaiting"]:
        raw.append(("approval", r["id"], r["age_s"],
                    "waiting for approval for %s" % _age(r["age_s"])))
    for r in queue["dead"]:
        why = r["last_error"] or "no error recorded"
        raw.append(("dead", r["id"], max(0, now - r["updated_at"]),
                    "dead after %d of %d attempts: %s" % (r["attempts"], r["max_attempts"], clip(why, 200))))
    for q in quiet:
        raw.append(("quiet", q["task_id"], q["quiet_s"],
                    "held by %s, no tool activity for %s while its lease is alive" % (q["owner"], _age(q["quiet_s"]))))
    for r in queue["blocked"]:
        if r["forever"] or r["age_s"] > open_s:
            reasons = ["%s (%s)" % (b["id"], b["problem"] or b["state"] or "not done") for b in r["blockers"]]
            if r["cycle"]:
                reasons.append("a cycle")
            raw.append(("blocked", r["id"], r["age_s"],
                        "%s: %s" % ("can never run" if r["forever"] else "waiting for %s" % _age(r["age_s"]),
                                    ", ".join(reasons[:5]) or "blockers not done")))
    return raw


def total_items(queue: dict, quiet: list, now: int, open_s: int) -> int:
    """How many items the inbox holds (the count on the nav link), without reading any events."""
    return len(_raw_items(queue, quiet, now, open_s))


def build_inbox(tasks_by_id: dict, events: list, queue: dict, quiet: list, now: int, open_s: int, actor: str) -> dict:
    """The inbox items, most urgent kind first (approval, dead, quiet, blocked), oldest first inside a kind."""
    raw = _raw_items(queue, quiet, now, open_s)
    rank = {k: i for i, k in enumerate(KINDS)}
    raw.sort(key=lambda x: (rank[x[0]], -x[2], x[1]))
    totals = {k: 0 for k in KINDS}
    for kind, _, _, _ in raw:
        totals[kind] += 1
    shown = raw[:ITEM_LIMIT]
    notes = _recent_events(events, {x[1] for x in shown})
    mine = {n for n in act_mod.produced_by(actor) if n}  # no actor (read-only console): nobody is "you"
    items = []
    for kind, tid, age, summary in shown:
        t = tasks_by_id.get(tid)
        if t is None:
            continue
        authors = {t["created_by"], t.get("owner") or ""} | {n["author"] for n in notes[tid]}
        items.append({
            "kind": kind, "id": tid, "title": clip(t["title"], 300), "state": t["state"], "summary": summary,
            "age_s": max(0, age), "priority": t["priority"], "created_by": t["created_by"], "owner": t.get("owner"),
            "body": clip(t["body"], BODY_CLIP), "body_clipped": len(t["body"]) > BODY_CLIP,
            "notes": notes[tid], "sha": act_mod.task_sha(t["title"], t["body"]),
            "allowed": list(ALLOWED[kind]),
            "you_produced": bool(mine & authors),
        })
    return {"items": items, "totals": totals, "total": len(raw), "truncated": len(raw) > ITEM_LIMIT, "rules": RULES}


def _age(s: int) -> str:
    s = max(0, int(s))
    if s < 90:
        return "%d s" % s
    if s < 5400:
        return "%d min" % (s // 60)
    if s < 172800:
        return "%d h" % (s // 3600)
    return "%d days" % (s // 86400)

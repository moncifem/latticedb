"""Write mode: a person's decisions on the board (approve, reject, cancel, requeue, priority, note).

The console writes nothing by itself. Every decision is one call of `board act` (integrations/board/board), the
helper that every other writer uses, run as the person the console was started for (BOARD_AUTHOR=human.NAME).
That keeps one audited path: the helper writes the task change, the event and the human_actions row in one
transaction, refuses a person who produced the task, and refuses an approval of text that changed. This module
only decides whether a request may reach the helper, and turns the helper's three-line language back into a
fixed answer for the browser.

What stops a request before the helper (all of it tested with planted faults, test_actions_mutations.py):
  * writes exist only when the console was started with --allow-writes; without it the server has no Actions
    object and answers 403 to every POST;
  * a per-run random token, printed once to the terminal, sent in a custom header (a page of another origin
    cannot add the header without a CORS preflight, which this server never answers), compared in constant
    time, with a lockout after repeated wrong tokens;
  * the Origin must be this server's own, the Host must be one of its own names (checked in server.py);
  * a strict JSON body: an allow-listed action, every field validated with fullmatch, nothing unknown;
  * a request id per decision: asking again with the same id answers the same and writes nothing more.

What does NOT stop: someone who can read the token (it is on the terminal and in the page's memory), and a
database account that is allowed to write: the person is a name this process was given, like every author on
this board (docs/alpha.md, "Board console").
"""

from __future__ import annotations

import getpass
import hashlib
import hmac
import json
import os
import re
import secrets
import threading
import time
from collections import OrderedDict
from typing import Callable, Optional

from .activity import _CONTROL
from .source import ConfigError, SourceError, run_bounded

TOKEN_HEADER = "X-Console-Write-Token"
ACTIONS = ("approve", "reject", "cancel", "requeue", "priority", "note")
BULK_ACTIONS = ("cancel", "requeue")  # never approve: a gate is crossed one task at a time, after reading it
BULK_MAX = 25
COMMENT_MAX = 1000
PRIORITY_LIMIT = 1_000_000
SEEN_MAX = 512  # request ids remembered for the life of the process (the database remembers all of them)
HELPER_TIMEOUT_S = 30.0
HELPER_MAX_BYTES = 64 * 1024

# The task states in which an action applies (the helper checks again against the database).
APPLIES = {
    "approve": ("awaiting_approval",),
    "reject": ("awaiting_approval",),
    "cancel": ("open", "claimed", "awaiting_approval"),
    "requeue": ("dead", "claimed"),
    "priority": ("open", "claimed", "awaiting_approval", "dead"),
    "note": ("open", "claimed", "awaiting_approval", "dead", "done", "cancelled"),
}

_ID_RE = re.compile(r"[A-Za-z0-9_-]{1,64}")
_REQ_RE = re.compile(r"[A-Za-z0-9_-]{8,64}")
_SHA_RE = re.compile(r"[0-9a-f]{64}")
_NAME_RE = re.compile(r"[A-Za-z0-9._-]{1,58}")  # 58 + len("human.") = the helper's 64
_WS = re.compile(r"[ \t\r\n]+")


class BadAction(Exception):
    """A request the console refuses before the helper is asked. str(e) is a fixed sentence."""


# ---------------------------------------------------------------- identity and content hash


def task_sha(title: str, body: str) -> str:
    """The content a decision is bound to: sha-256 of the title, a newline and the body. `board act` computes the
    same value from the task row, so a decision on text that changed since the page showed it is refused."""
    return hashlib.sha256((title + "\n" + body).encode("utf-8")).hexdigest()


def default_actor_name() -> str:
    try:
        name = getpass.getuser()
    except Exception:
        name = ""
    name = re.sub(r"[^A-Za-z0-9._-]", "_", name)[:58]
    return name or "user"


def actor_identity(name: Optional[str]) -> str:
    """human.NAME for a --actor value (the OS user when none is given)."""
    name = default_actor_name() if not name else name
    if not _NAME_RE.fullmatch(name):
        raise ConfigError("--actor may use only letters, digits, '.', '_' and '-' (up to 58 characters)")
    return "human." + name


def produced_by(actor: str) -> tuple:
    """The two names `board act` treats as the actor: human.NAME and NAME (an agent or a CLI session of the same
    person)."""
    return (actor, actor[len("human."):] if actor.startswith("human.") else actor)


def new_token() -> str:
    return secrets.token_urlsafe(32)


# ---------------------------------------------------------------- the gate: token, lockout, rate


class WriteGate:
    """Checks the write token. A wrong token counts against everybody, because on a loopback port every client
    looks the same: MAX_BAD wrong tokens within WINDOW_S lock every write for LOCK_S, the right token included."""

    MAX_BAD = 5
    WINDOW_S = 60.0
    LOCK_S = 60.0
    MAX_WRITES = 60  # accepted writes per WINDOW_S

    def __init__(self, token: str, clock: Callable[[], float] = time.monotonic):
        if not token or len(token) < 16:
            raise ValueError("the write token is too short")
        self._token = token.encode("utf-8")
        self._clock = clock
        self._lock = threading.Lock()
        self._bad: list = []
        self._ok: list = []
        self._locked_until = 0.0

    def check(self, supplied) -> Optional[str]:
        """None when the request may go on, else "locked", "bad_token" or "rate"."""
        with self._lock:
            now = self._clock()
            if now < self._locked_until:
                return "locked"
            raw = supplied.encode("utf-8", "replace") if isinstance(supplied, str) else b""
            if not hmac.compare_digest(raw, self._token):
                self._bad = [t for t in self._bad if now - t < self.WINDOW_S] + [now]
                if len(self._bad) >= self.MAX_BAD:
                    self._locked_until = now + self.LOCK_S
                    self._bad = []
                return "bad_token"
            self._bad = []
            self._ok = [t for t in self._ok if now - t < self.WINDOW_S]
            if len(self._ok) >= self.MAX_WRITES:
                return "rate"
            self._ok.append(now)
            return None


# ---------------------------------------------------------------- parsing a request


def _text(body: dict, key: str, rx, what: str, required: bool = True) -> Optional[str]:
    v = body.get(key)
    if v is None:
        if required:
            raise BadAction("%s is required" % key)
        return None
    if not isinstance(v, str) or not rx.fullmatch(v):
        raise BadAction("%s must be %s" % (key, what))
    return v


def clean_comment(value, required: bool) -> str:
    if value is None or value == "":
        if required:
            raise BadAction("a comment is required for this action")
        return ""
    if not isinstance(value, str):
        raise BadAction("comment must be text")
    text = _WS.sub(" ", value).strip()
    if "\\" in text or _CONTROL.search(text):
        raise BadAction("the comment may not contain a backslash or control characters")
    if len(text) > COMMENT_MAX or len(text.encode("utf-8")) > 4000:
        raise BadAction("the comment is longer than %d characters" % COMMENT_MAX)
    if required and not text:
        raise BadAction("a comment is required for this action")
    return text


def parse_act(body) -> dict:
    """A validated single decision: action, task, request_id, and per action sha, comment or priority."""
    if not isinstance(body, dict):
        raise BadAction("the body must be a JSON object")
    action = body.get("action")
    if action not in ACTIONS:  # unknown or missing: the list is closed
        raise BadAction("action must be one of %s" % ", ".join(ACTIONS))
    allowed = {"action", "task", "request_id", "comment"}
    if action == "approve":
        allowed.add("sha")
    if action == "priority":
        allowed.add("priority")
    extra = sorted(set(body) - allowed)
    if extra:
        raise BadAction("unknown field %r for %s" % (extra[0][:32], action))
    out = {"action": action,
           "task": _text(body, "task", _ID_RE, "a task id (letters, digits, '_' or '-', up to 64)"),
           "request_id": _text(body, "request_id", _REQ_RE, "8 to 64 letters, digits, '_' or '-'")}
    out["comment"] = clean_comment(body.get("comment"), action in ("reject", "note"))
    if action == "approve":
        out["sha"] = _text(body, "sha", _SHA_RE, "the 64 hex digits of the content hash shown with the task")
    if action == "priority":
        p = body.get("priority")
        if isinstance(p, bool) or not isinstance(p, int) or not -PRIORITY_LIMIT <= p <= PRIORITY_LIMIT:
            raise BadAction("priority must be a whole number between -%d and %d" % (PRIORITY_LIMIT, PRIORITY_LIMIT))
        out["priority"] = p
    return out


def parse_bulk(body) -> dict:
    if not isinstance(body, dict):
        raise BadAction("the body must be a JSON object")
    extra = sorted(set(body) - {"action", "ids", "dry_run", "request_id", "confirm", "comment"})
    if extra:
        raise BadAction("unknown field %r" % extra[0][:32])
    action = body.get("action")
    if action not in BULK_ACTIONS:
        raise BadAction("a bulk action must be one of %s" % ", ".join(BULK_ACTIONS))
    ids = body.get("ids")
    if not isinstance(ids, list) or not 1 <= len(ids) <= BULK_MAX:
        raise BadAction("ids must be a list of 1 to %d task ids" % BULK_MAX)
    seen = []
    for i in ids:
        if not isinstance(i, str) or not _ID_RE.fullmatch(i):
            raise BadAction("every id must be a task id")
        if i in seen:
            raise BadAction("an id is listed twice")
        seen.append(i)
    dry = body.get("dry_run")
    if not isinstance(dry, bool):
        raise BadAction("dry_run must be true or false")
    out = {"action": action, "ids": seen, "dry_run": dry, "comment": clean_comment(body.get("comment"), False)}
    if not dry:
        out["request_id"] = _text(body, "request_id", _REQ_RE, "8 to 64 letters, digits, '_' or '-'")
        c = body.get("confirm")
        if isinstance(c, bool) or not isinstance(c, int) or not 0 <= c <= BULK_MAX:
            raise BadAction("confirm must be the number of tasks the dry run reported")
        out["confirm"] = c
    elif "request_id" in body or "confirm" in body:
        raise BadAction("a dry run takes no request_id and no confirm")
    return out


# ---------------------------------------------------------------- the helper

# What `board act` answers, and what the browser is told. Fixed sentences: the helper's own messages (stderr)
# are never passed on.
REFUSALS = {
    "self_approval": (403, "you produced or claimed this task, so another person has to decide on it"),
    "stale": (409, "the task text changed since this page showed it; reload and read it again"),
    "wrong_state": (409, "the task is not in a state this action applies to; reload"),
    "changed": (409, "the task changed while the decision was being written; nothing was recorded"),
    "not_found": (404, "no such task on the board"),
    "request_reused": (409, "that request id was already used for a different decision"),
    "comment_required": (400, "a comment is required for this action"),
    "hash_required": (400, "an approval needs the content hash shown with the task"),
    "unreadable": (409, "the task text cannot be shown as one line, so it cannot be approved here"),
}
FAILED = (502, "the board helper did not confirm the decision; nothing is known to be recorded, reload to see")
_DONE_RE = re.compile(r"(done|noop) ([0-9]{1,18})")
_REFUSED_RE = re.compile(r"refused ([a-z_]{1,32})")


class HelperRunner:
    """Runs `board act ...` as the configured person. Never through a shell; the environment is the console's own
    (the connection settings) with BOARD_AUTHOR set; the helper's stderr is dropped."""

    def __init__(self, board: str, actor: str, url: str = "", env=None, timeout: float = HELPER_TIMEOUT_S):
        self.board = board
        self.actor = actor
        self.timeout = timeout
        self.env = dict(os.environ if env is None else env)
        self.env["BOARD_AUTHOR"] = actor
        self.env.pop("BOARD_ACTIVITY", None)
        self.env.pop("BOARD_STRICT_FENCE", None)
        if url:
            self.env["LATTICE_BOARD_URL"] = url

    def __call__(self, args: list) -> tuple:
        try:
            rc, out, _err = run_bounded([self.board] + list(args), "", self.timeout, HELPER_MAX_BYTES, self.env)
        except SourceError:
            return 1, ""
        return rc, out.decode("utf-8", "replace")


def helper_args(req: dict, request_id: str) -> list:
    a = ["act", req["action"], req["task"], "--request-id", request_id]
    if req.get("sha"):
        a += ["--hash", req["sha"]]
    if req.get("comment"):
        a += ["--comment", req["comment"]]
    if req["action"] == "priority":
        a += ["--priority", str(req["priority"])]
    return a


def parse_result(rc: int, out: str) -> tuple:
    """("ok", event_id, repeated), ("refused", code) or ("failed",)."""
    line = out.splitlines()[0] if out.strip() else ""
    m = _DONE_RE.fullmatch(line)
    if rc == 0 and m:
        return ("ok", int(m.group(2)), m.group(1) == "noop")
    m = _REFUSED_RE.fullmatch(line)
    if rc == 5 and m:
        return ("refused", m.group(1))
    return ("failed",)


VERB = {"approve": "approved", "reject": "rejected", "cancel": "cancelled", "requeue": "requeued",
        "priority": "re-prioritised", "note": "noted"}


class Actions:
    """The write path of one console: gate, validation, the runner, the request-id memory."""

    def __init__(self, service, actor: str, token: str, runner: Callable[[list], tuple],
                 clock: Callable[[], float] = time.monotonic):
        self.service = service
        self.actor = actor
        self.gate = WriteGate(token, clock)
        self.runner = runner
        self._lock = threading.Lock()
        self._seen: OrderedDict = OrderedDict()

    def info(self) -> dict:
        return {"enabled": True, "actor": self.actor}

    # -- one decision

    def _one(self, req: dict, request_id: str) -> tuple:
        """(status, answer) for one decision. Caller holds the lock."""
        fp = json.dumps([req["action"], req["task"], req.get("sha"), req.get("comment"), req.get("priority")])
        prior = self._seen.get(request_id)
        if prior is not None:
            if prior[0] != fp:
                status, text = REFUSALS["request_reused"]
                return status, {"ok": False, "code": "request_reused", "error": text}
            self._seen.move_to_end(request_id)
            return 200, dict(prior[1], repeated=True)
        rc, out = self.runner(helper_args(req, request_id))
        res = parse_result(rc, out)
        if res[0] == "refused":
            status, text = REFUSALS.get(res[1], FAILED)
            return status, {"ok": False, "code": res[1] if res[1] in REFUSALS else "failed", "error": text}
        if res[0] == "failed":
            return FAILED[0], {"ok": False, "code": "failed", "error": FAILED[1]}
        answer = {"ok": True, "action": req["action"], "task": req["task"], "event_id": res[1],
                  "repeated": res[2], "actor": self.actor,
                  "message": "%s %s: recorded as event %d by %s" % (VERB[req["action"]], req["task"], res[1], self.actor)}
        self._seen[request_id] = (fp, answer)
        while len(self._seen) > SEEN_MAX:
            self._seen.popitem(last=False)
        self.service.invalidate()
        return 200, answer

    def act(self, body) -> tuple:
        try:
            req = parse_act(body)
        except BadAction as e:
            return 400, {"ok": False, "code": "bad_request", "error": str(e)}
        with self._lock:
            return self._one(req, req["request_id"])

    # -- many (cancel, requeue)

    def _eligible(self, action: str, ids: list) -> tuple:
        snap = self.service.snapshot()
        by_id = snap.by_id
        ok, skipped = [], []
        for tid in ids:
            t = by_id.get(tid)
            if t is None:
                skipped.append({"id": tid, "reason": "not on the board"})
            elif t["state"] not in APPLIES[action]:
                skipped.append({"id": tid, "reason": "it is %s" % t["state"]})
            else:
                ok.append({"id": tid, "title": t["title"][:300], "state": t["state"]})
        return ok, skipped

    def bulk(self, body) -> tuple:
        try:
            req = parse_bulk(body)
        except BadAction as e:
            return 400, {"ok": False, "code": "bad_request", "error": str(e)}
        eligible, skipped = self._eligible(req["action"], req["ids"])
        if req["dry_run"]:
            return 200, {"ok": True, "dry_run": True, "action": req["action"], "count": len(eligible),
                         "eligible": eligible, "skipped": skipped}
        if req["confirm"] != len(eligible):
            return 409, {"ok": False, "code": "count_changed",
                         "error": "the number of tasks this would change is now %d, not %d; run the dry run again"
                                  % (len(eligible), req["confirm"])}
        results = []
        with self._lock:
            for item in eligible:
                one = {"action": req["action"], "task": item["id"], "comment": req["comment"]}
                rid = hashlib.sha256(("%s:%s:%s" % (req["request_id"], req["action"], item["id"])).encode()).hexdigest()[:40]
                status, ans = self._one(one, rid)
                results.append({"id": item["id"], "ok": bool(ans.get("ok")), "event_id": ans.get("event_id"),
                                "code": ans.get("code"), "repeated": bool(ans.get("repeated"))})
        done = sum(1 for r in results if r["ok"])
        return 200, {"ok": done == len(results), "dry_run": False, "action": req["action"], "done": done,
                     "results": results, "skipped": skipped, "actor": self.actor}


def default_board_path() -> str:
    here = os.path.dirname(os.path.abspath(__file__))
    return os.path.normpath(os.path.join(here, "..", "..", "board", "board"))


def strict_json(raw: bytes):
    """json.loads that refuses duplicate keys, NaN and Infinity, so a field cannot be smuggled twice."""
    def pairs(items):
        d = {}
        for k, v in items:
            if k in d:
                raise ValueError("duplicate key")
            d[k] = v
        return d

    def no_constant(name):
        raise ValueError("not JSON")

    return json.loads(raw.decode("utf-8"), object_pairs_hook=pairs, parse_constant=no_constant)

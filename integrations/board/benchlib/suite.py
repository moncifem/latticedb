"""The suite file of `board bench`: a strict, bounded parser, the fixture digest and the attempt matrix.

A suite is one JSON document (see integrations/board/bench/README.md for the schema). The parser refuses
what it does not know (an unknown key is a typo that would otherwise run a different experiment than the
author meant), duplicate keys, non-finite numbers, oversized files and matrices that are too big to be
meant. Nothing here touches the board or runs a program.
"""

from __future__ import annotations

import hashlib
import json
import os
import random
import re
import stat

from . import stats

HARNESS_VERSION = "board-bench/1"

MAX_SUITE_BYTES = 256 * 1024
MAX_TASKS = 200
MAX_ARMS = 8
MAX_REPEATS = 20
MAX_ATTEMPTS = 2000
MAX_PROMPT_BYTES = 8000
MAX_FIXTURE_FILES = 5000
MAX_FIXTURE_BYTES = 64 * 1024 * 1024
MAX_PROTECTED = 20

NAME = re.compile(r"^[A-Za-z0-9_-]{1,32}$")
SUITE_NAME = re.compile(r"^[A-Za-z0-9_-]{1,48}$")
EXPERIMENT_NAME = SUITE_NAME
ENV_NAME = re.compile(r"^[A-Za-z_][A-Za-z0-9_]{0,63}$")
# Never handed to an agent or a grader, even when a suite asks: the board's own credentials and variables,
# and what changes what a program loads. (The same families board run refuses.)
FORBIDDEN_ENV = re.compile(r"^(PG[A-Z0-9_]*|BOARD_[A-Z0-9_]*|LATTICE_[A-Z0-9_]*|TASK_[A-Z0-9_]*|BENCH_[A-Z0-9_]*|"
                           r"LD_[A-Z0-9_]*|DYLD_[A-Z0-9_]*|BASH_ENV|ENV|PYTHONPATH|PYTHONSTARTUP|PYTHONHOME|"
                           r"NODE_OPTIONS|PERL5LIB|RUBYOPT|GIT_SSH_COMMAND|SSH_AUTH_SOCK)$", re.I)
SAFE_CLAUDE_MODES = ("dontAsk", "acceptEdits", "plan", "manual")
SAFE_CODEX_SANDBOXES = ("read-only", "workspace-write")
MODEL = re.compile(r"^[A-Za-z0-9._:/@-]{1,80}$")
TOOLS = re.compile(r"^[A-Za-z0-9_,:*() .\-/@=]{1,300}$")
DIGEST = re.compile(r"^(?:sha256:)?([0-9a-f]{64})$")
# Glob syntax of `protected`: relative, no "..", no backslash.
PROTECTED = re.compile(r"^[A-Za-z0-9_./*?@+=,-]{1,200}$")

TOP_KEYS = {"suite", "version", "tasks", "arms", "repeats"}
TASK_KEYS = {"id", "prompt", "fixture", "grader", "timeout_s"}
FIXTURE_KEYS = {"dir", "digest", "git"}
GRADER_KEYS = {"cmd", "timeout_s", "protected", "reruns", "files"}
ARM_KEYS = {"id", "agent", "args", "env_allow"}
ARG_KEYS = {
    "cmd": {"argv"},
    "claude": {"model", "budget_usd", "allowed_tools", "permission_mode", "bin"},
    "codex": {"model", "sandbox", "bin"},
}


class SuiteError(Exception):
    """The suite file is not acceptable; str(e) says which field and why."""


def _no_dupes(pairs):
    out = {}
    for k, v in pairs:
        if k in out:
            raise ValueError("duplicate key %r" % k)
        out[k] = v
    return out


def _reject_constant(name):
    raise ValueError("%s is not JSON" % name)


def _need(cond, msg):
    if not cond:
        raise SuiteError(msg)


def _keys(obj, allowed, where):
    _need(isinstance(obj, dict), "%s must be an object" % where)
    extra = sorted(set(obj) - allowed)
    if extra:
        raise SuiteError("%s: unknown key %s (allowed: %s)" % (where, extra[0], ", ".join(sorted(allowed))))


def _int(obj, key, where, lo, hi, default=None):
    if key not in obj:
        _need(default is not None, "%s: %s is required" % (where, key))
        return default
    v = obj[key]
    _need(isinstance(v, int) and not isinstance(v, bool) and lo <= v <= hi,
          "%s: %s must be a whole number from %d to %d" % (where, key, lo, hi))
    return v


def _str(obj, key, where, limit, rx=None, required=True):
    if key not in obj:
        _need(not required, "%s: %s is required" % (where, key))
        return None
    v = obj[key]
    _need(isinstance(v, str) and v != "" and len(v.encode("utf-8")) <= limit,
          "%s: %s must be a non-empty string of at most %d bytes" % (where, key, limit))
    _need("\0" not in v, "%s: %s has a NUL byte" % (where, key))
    if rx is not None:
        _need(rx.fullmatch(v) is not None, "%s: %s has a character that is not allowed here" % (where, key))
    return v


def _argv(v, where):
    _need(isinstance(v, list) and 1 <= len(v) <= 32, "%s must be a list of 1 to 32 strings" % where)
    for x in v:
        _need(isinstance(x, str) and x != "" and len(x.encode("utf-8")) <= 500 and "\0" not in x,
              "%s: every element must be a non-empty string of at most 500 bytes" % where)
    return list(v)


def pattern_ok(p):
    return bool(PROTECTED.fullmatch(p)) and not p.startswith("/") and ".." not in p.split("/") \
        and "//" not in p and not p.endswith("/")


def parse_suite(raw: bytes) -> dict:
    """Parse and validate suite bytes. Returns a normalised dict (defaults filled in); raises SuiteError."""
    _need(len(raw) <= MAX_SUITE_BYTES, "the suite file is larger than %d bytes" % MAX_SUITE_BYTES)
    try:
        doc = json.loads(raw.decode("utf-8"), object_pairs_hook=_no_dupes, parse_constant=_reject_constant)
    except (ValueError, RecursionError) as e:
        raise SuiteError("not valid JSON: %s" % str(e)[:120]) from None
    _keys(doc, TOP_KEYS, "the suite")
    out = {"suite": _str(doc, "suite", "the suite", 48, SUITE_NAME),
           "version": _int(doc, "version", "the suite", 1, 1000000),
           "repeats": _int(doc, "repeats", "the suite", 1, MAX_REPEATS)}
    tasks = doc.get("tasks")
    _need(isinstance(tasks, list) and 1 <= len(tasks) <= MAX_TASKS, "tasks must be a list of 1 to %d tasks" % MAX_TASKS)
    arms = doc.get("arms")
    _need(isinstance(arms, list) and 1 <= len(arms) <= MAX_ARMS, "arms must be a list of 1 to %d arms" % MAX_ARMS)
    _need(len(tasks) * len(arms) * out["repeats"] <= MAX_ATTEMPTS,
          "the matrix has %d attempts; the limit is %d (fewer tasks, arms or repeats)"
          % (len(tasks) * len(arms) * out["repeats"], MAX_ATTEMPTS))
    out["tasks"] = [_task(t, i) for i, t in enumerate(tasks)]
    out["arms"] = [_arm(a, i) for i, a in enumerate(arms)]
    for what, items in (("task", out["tasks"]), ("arm", out["arms"])):
        ids = [x["id"] for x in items]
        _need(len(set(ids)) == len(ids), "two %ss have the same id" % what)
    return out


def _task(t, i):
    where = "tasks[%d]" % i
    _keys(t, TASK_KEYS, where)
    tid = _str(t, "id", where, 32, NAME)
    where = "task %s" % tid
    prompt = _str(t, "prompt", where, MAX_PROMPT_BYTES)
    fx = t.get("fixture")
    _keys(fx, FIXTURE_KEYS, where + " fixture")
    _need("git" not in fx, "%s: a git fixture is not supported in this version; use a directory" % where)
    fdir = _str(fx, "dir", where + " fixture", 300)
    _need(not os.path.isabs(fdir) and ".." not in fdir.split("/") and "\\" not in fdir,
          "%s: fixture dir must be a relative path inside the suite directory" % where)
    digest = _str(fx, "digest", where + " fixture", 80)
    m = DIGEST.fullmatch(digest)
    _need(m is not None, "%s: fixture digest must be sha256:<64 hex digits>" % where)
    g = t.get("grader")
    _keys(g, GRADER_KEYS, where + " grader")
    prot = g.get("protected", [])
    _need(isinstance(prot, list) and len(prot) <= MAX_PROTECTED, "%s: protected must be a list of at most %d globs" % (where, MAX_PROTECTED))
    for p in prot:
        _need(isinstance(p, str) and pattern_ok(p),
              "%s: protected glob %r must be a relative path pattern without .., // or a backslash" % (where, p))
    cmd = _argv(g.get("cmd"), where + " grader cmd")
    files = g.get("files", {})
    _need(isinstance(files, dict) and len(files) <= 8, "%s: grader files must be an object of at most 8 pinned files" % where)
    pins = {}
    for rel, dg in files.items():
        _need(isinstance(rel, str) and pattern_ok(rel) and "*" not in rel and "?" not in rel,
              "%s: grader file %r must be a plain relative path inside the suite directory" % (where, rel))
        mm = DIGEST.fullmatch(dg) if isinstance(dg, str) else None
        _need(mm is not None, "%s: the digest of grader file %s must be sha256:<64 hex digits>" % (where, rel))
        pins[rel] = mm.group(1)
    for x in cmd:
        if x.startswith("{suite_dir}/"):
            rel = x[len("{suite_dir}/"):]
            _need(rel in pins, "%s: grader command uses {suite_dir}/%s, which must be pinned in grader.files with its sha256 "
                               "(an unpinned script can be changed by the agent between attempts)" % (where, rel))
    return {"id": tid, "prompt": prompt, "timeout_s": _int(t, "timeout_s", where, 1, 7200, 600),
            "fixture": {"dir": fdir, "digest": m.group(1)},
            "grader": {"cmd": cmd, "files": pins,
                       "timeout_s": _int(g, "timeout_s", where + " grader", 1, 3600, 120),
                       "protected": list(prot), "reruns": _int(g, "reruns", where + " grader", 0, 4, 0)}}


def _arm(a, i):
    where = "arms[%d]" % i
    _keys(a, ARM_KEYS, where)
    aid = _str(a, "id", where, 32, NAME)
    where = "arm %s" % aid
    agent = a.get("agent")
    _need(agent in ARG_KEYS, "%s: agent must be claude, codex or cmd" % where)
    args = a.get("args", {})
    _keys(args, ARG_KEYS[agent], where + " args")
    clean = {}
    if agent == "cmd":
        clean["argv"] = _argv(args.get("argv"), where + " args.argv")
    else:
        if "model" in args:
            clean["model"] = _str(args, "model", where + " args", 80, MODEL)
        if "bin" in args:
            clean["bin"] = _str(args, "bin", where + " args", 300)
    if agent == "claude":
        if "budget_usd" in args:
            b = args["budget_usd"]
            _need(isinstance(b, (int, float)) and not isinstance(b, bool) and 0 < b <= 100,
                  "%s: budget_usd must be a number of dollars above 0 and at most 100" % where)
            clean["budget_usd"] = b
        if "allowed_tools" in args:
            clean["allowed_tools"] = _str(args, "allowed_tools", where + " args", 300, TOOLS)
            _need(not unrestricted_shell(clean["allowed_tools"]),
                  "%s: allowed_tools gives the agent a shell without a command pattern" % where)
        if "permission_mode" in args:
            _need(args["permission_mode"] in SAFE_CLAUDE_MODES,
                  "%s: permission_mode must be one of %s" % (where, ", ".join(SAFE_CLAUDE_MODES)))
            clean["permission_mode"] = args["permission_mode"]
    if agent == "codex" and "sandbox" in args:
        _need(args["sandbox"] in SAFE_CODEX_SANDBOXES, "%s: sandbox must be one of %s" % (where, ", ".join(SAFE_CODEX_SANDBOXES)))
        clean["sandbox"] = args["sandbox"]
    env = a.get("env_allow", [])
    _need(isinstance(env, list) and len(env) <= 16, "%s: env_allow must be a list of at most 16 names" % where)
    for n in env:
        _need(isinstance(n, str) and ENV_NAME.fullmatch(n), "%s: env_allow has a bad variable name" % where)
        _need(not FORBIDDEN_ENV.match(n), "%s: env_allow %s is reserved (board credentials and loader variables never reach an agent)" % (where, n))
    return {"id": aid, "agent": agent, "args": clean, "env_allow": list(env)}


def unrestricted_shell(tools: str) -> bool:
    """True when an allowed-tools list gives a shell with no command pattern (the rule board run uses)."""
    for name, paren, inner in re.findall(r"([A-Za-z_]+)(\(([^)]*)\))?", tools):
        if name.lower() in ("bash", "shell", "powershell") and (not paren or inner.strip() in ("", "*", "**", ":*")):
            return True
    return False


def load_suite(path: str) -> tuple:
    """(suite dict, sha256 hex of the file bytes, absolute path). The sha is what every attempt checks."""
    try:
        with open(path, "rb") as f:
            raw = f.read(MAX_SUITE_BYTES + 1)
    except OSError as e:
        raise SuiteError("cannot read the suite file: %s" % (e.strerror or e)) from None
    suite = parse_suite(raw)
    return suite, hashlib.sha256(raw).hexdigest(), os.path.realpath(path)


def verify_pins(suite_dir: str, pins: dict) -> list:
    """Problems with the pinned grader files (empty when every file exists and hashes to its pin)."""
    out = []
    for rel, want in sorted(pins.items()):
        path = os.path.join(suite_dir, rel)
        try:
            with open(path, "rb") as f:
                got = hashlib.sha256(f.read()).hexdigest()
        except OSError:
            out.append("grader file %s is missing" % rel)
            continue
        if got != want:
            out.append("grader file %s hashes to sha256:%s, not the pinned sha256:%s" % (rel, got, want))
    return out


def arm_config_sha(arm: dict) -> str:
    """sha256 of the arm's definition (canonical JSON), stored with the experiment."""
    return hashlib.sha256(json.dumps(arm, sort_keys=True, separators=(",", ":")).encode()).hexdigest()


# ---- fixtures -----------------------------------------------------------------------------------


def fixture_root(suite_dir: str, rel: str) -> str:
    """The real path of a fixture directory, which must lie inside the suite's directory."""
    base = os.path.realpath(suite_dir)
    root = os.path.realpath(os.path.join(base, rel))
    if root != base and not root.startswith(base + os.sep):
        raise SuiteError("fixture dir %s leaves the suite directory" % rel)
    if not os.path.isdir(root):
        raise SuiteError("fixture dir %s does not exist" % rel)
    return root


def list_tree(root: str) -> list:
    """[(relative path, absolute path)] of every regular file below root, sorted. A symbolic link, a
    device or a pipe is refused: a link could point outside the tree and make the digest mean nothing.
    Bounded in file count and total size."""
    files = []
    total = 0
    for cur, dirs, names in os.walk(root, followlinks=False):
        dirs.sort()
        for d in list(dirs):
            if os.path.islink(os.path.join(cur, d)):
                raise SuiteError("the fixture contains a symbolic link: %s" % os.path.relpath(os.path.join(cur, d), root))
        for n in sorted(names):
            p = os.path.join(cur, n)
            st = os.lstat(p)
            if not stat.S_ISREG(st.st_mode):
                raise SuiteError("the fixture contains something that is not a regular file: %s" % os.path.relpath(p, root))
            total += st.st_size
            files.append((os.path.relpath(p, root).replace(os.sep, "/"), p))
            if len(files) > MAX_FIXTURE_FILES or total > MAX_FIXTURE_BYTES:
                raise SuiteError("the fixture is larger than %d files or %d bytes" % (MAX_FIXTURE_FILES, MAX_FIXTURE_BYTES))
    files.sort()
    return files


def tree_digest(root: str) -> str:
    """sha256 over one line per file, in path order: "<path>\\0<1 if executable else 0>\\0<sha256 of the
    content>\\n". The path, the executable bit and the content decide the digest; timestamps and owners do
    not. Empty directories are not part of it."""
    h = hashlib.sha256()
    for rel, p in list_tree(root):
        fh = hashlib.sha256()
        with open(p, "rb") as f:
            for chunk in iter(lambda: f.read(1 << 20), b""):
                fh.update(chunk)
        exe = 1 if os.stat(p).st_mode & 0o111 else 0
        h.update(("%s\0%d\0%s\n" % (rel, exe, fh.hexdigest())).encode("utf-8"))
    return h.hexdigest()


# ---- the matrix ---------------------------------------------------------------------------------


def shuffled(items, rng):
    """Fisher-Yates with the seeded generator's random() only (stable across Python 3 versions)."""
    a = list(items)
    for i in range(len(a) - 1, 0, -1):
        j = int(rng.random() * (i + 1))
        a[i], a[j] = a[j], a[i]
    return a


def build_matrix(suite: dict, seed: int) -> list:
    """Every (task, repeat) cell once, in a seeded random order, with the arms of a cell in a seeded random
    order and next to each other. Arms therefore interleave (a slow hour, a rate limit or an API change
    touches all arms of a cell alike, not one arm's whole run) and no arm is systematically first. Returns
    [{"order": i, "task": id, "arm": id, "repeat": r}] with order 0..N-1."""
    rng = random.Random(seed)
    cells = [(t["id"], r) for t in suite["tasks"] for r in range(suite["repeats"])]
    out = []
    for task, rep in shuffled(cells, rng):
        for arm in shuffled([a["id"] for a in suite["arms"]], rng):
            out.append({"order": len(out), "task": task, "arm": arm, "repeat": rep})
    return out


def plan_text(suite: dict, sha: str, fixture_status: dict) -> str:
    """The matrix `plan` prints."""
    nt, na, k = len(suite["tasks"]), len(suite["arms"]), suite["repeats"]
    lines = ["suite %s (version %d, sha256 %s)" % (suite["suite"], suite["version"], sha[:12]),
             "tasks %d   arms %d   repeats %d   attempts %d" % (nt, na, k, nt * na * k), ""]
    lines.append("%-14s %-8s %s" % ("arm", "agent", "settings"))
    for a in suite["arms"]:
        lines.append("%-14s %-8s %s" % (a["id"], a["agent"], json.dumps(a["args"], sort_keys=True) if a["args"] else "(defaults)"))
    lines += ["", "%-24s %-8s %-7s %s" % ("task", "timeout", "reruns", "fixture")]
    for t in suite["tasks"]:
        lines.append("%-24s %-8s %-7s %s" % (t["id"], "%ds" % t["timeout_s"], t["grader"]["reruns"],
                                             fixture_status.get(t["id"], "not checked")))
    lines += ["", stats.power_note(nt, k)]
    if na < 2:
        lines.append("One arm cannot be compared with anything; the report will show it alone.")
    return "\n".join(lines)

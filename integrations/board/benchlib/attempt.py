"""One attempt of `board bench`: a fresh copy of the fixture, the arm's agent, then the grader.

Run by `board run -- board-bench attempt`. The runner gives it a scrubbed environment (no database
credentials at all) and the files named by TASK_BODY_PATH and TASK_RESULT_PATH. This module never talks to
the board: it writes the result file, and `board-bench record`, run by the runner's --after-cmd under the
task's fence, puts the attempt in the database.

Order of events, and why:
  1. read the task body (ids only) and the suite file; check the suite's sha256 against the one the
     experiment recorded, so an edited suite cannot silently change a running experiment;
  2. copy the fixture into a private directory (the PRISTINE copy) and check its digest THERE, so the files
     compared and the files used are the same bytes whatever happens to the source tree afterwards;
  3. copy the pristine tree to the WORK directory and run the agent in it, in its own process group, with
     a timeout, an environment that holds only an allow-list and an empty HOME;
  4. put every protected path back from the pristine copy (and delete protected files the agent added),
     counting how many had to be changed;
  5. run the grader in a separate process with its own timeout, once and then `reruns` more times; an
     outcome that the runs disagree on is recorded as flaky and decided by a strict majority.
A problem of the harness (digest mismatch, a grader that could not start, a full disk, an exception) is the
outcome "error", which no statistic counts against an arm.
"""

from __future__ import annotations

import hashlib
import importlib.machinery
import importlib.util
import json
import os
import re
import shutil
import signal
import stat
import subprocess
import sys
import tempfile
import time

from . import suite as suitemod

OUTCOMES = ("pass", "fail", "timeout", "error")
MAX_BODY_BYTES = 4096
GRADER_ERROR_EXITS = (125,)  # only 125 means "the harness could not run the grader". 126 and 127 are what a shell
# returns for a script that was made non-executable or deleted, which an agent can do: they are failures.
LOG_CAP = 1 << 20
SUMMARY_BYTES = 400
SECRET_NAME = re.compile(r"KEY|TOKEN|SECRET|PASSW|CREDENTIAL|AUTH", re.I)
BASE_ENV = ("PATH", "LANG", "TERM")


def load_runner():
    """The `board run` module, loaded from its file: its process-group supervision and text helpers are
    reused so that bench children are killed and logged exactly as runner children are."""
    path = os.path.join(os.path.dirname(os.path.dirname(os.path.realpath(__file__))), "board-run")
    loader = importlib.machinery.SourceFileLoader("board_run_lib", path)
    spec = importlib.util.spec_from_loader("board_run_lib", loader)
    mod = importlib.util.module_from_spec(spec)
    loader.exec_module(mod)
    return mod


R = load_runner()


class HarnessError(Exception):
    """Something outside the agent went wrong; the attempt is recorded as outcome "error"."""


# ---- the task body ------------------------------------------------------------------------------


def make_body(experiment, arm, task, repeat, order, suite_path, suite_sha) -> str:
    """The text of the board task: ids and the suite's path and sha, no prompt (a prompt can hold a
    backslash or a newline, which a board task body may not)."""
    body = json.dumps({"b": 1, "exp": experiment, "arm": arm, "task": task, "rep": repeat, "ord": order,
                       "suite": suite_path, "sha": suite_sha}, separators=(",", ":"), ensure_ascii=False)
    if "\\" in body or any(ord(c) < 32 or ord(c) == 127 for c in body) or len(body.encode("utf-8")) > MAX_BODY_BYTES:
        raise suitemod.SuiteError("the suite path has a character a board task body cannot hold "
                                  "(a backslash or a control character), or is too long")
    return body


def read_body(path: str) -> dict:
    raw, problem = R.read_regular(path, MAX_BODY_BYTES)
    if raw is None or problem:
        raise HarnessError("cannot read the task body (%s)" % (problem or "missing"))
    if len(raw) > MAX_BODY_BYTES:
        raise HarnessError("the task body is larger than %d bytes" % MAX_BODY_BYTES)
    try:
        doc = json.loads(raw.decode("utf-8"), object_pairs_hook=suitemod._no_dupes,
                         parse_constant=suitemod._reject_constant)
    except (ValueError, RecursionError):
        raise HarnessError("the task body is not a bench task (not JSON)") from None
    if not isinstance(doc, dict) or set(doc) != {"b", "exp", "arm", "task", "rep", "ord", "suite", "sha"} or doc["b"] != 1:
        raise HarnessError("the task body is not a bench task")
    ok = (isinstance(doc["exp"], str) and suitemod.EXPERIMENT_NAME.fullmatch(doc["exp"])
          and isinstance(doc["arm"], str) and suitemod.NAME.fullmatch(doc["arm"])
          and isinstance(doc["task"], str) and suitemod.NAME.fullmatch(doc["task"])
          and isinstance(doc["rep"], int) and not isinstance(doc["rep"], bool) and 0 <= doc["rep"] < 100
          and isinstance(doc["ord"], int) and not isinstance(doc["ord"], bool) and doc["ord"] >= 0
          and isinstance(doc["suite"], str) and os.path.isabs(doc["suite"])
          and isinstance(doc["sha"], str) and re.fullmatch(r"[0-9a-f]{64}", doc["sha"]))
    if not ok:
        raise HarnessError("the task body has a bad field")
    return doc


# ---- files --------------------------------------------------------------------------------------


def file_sha(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def glob_regex(pattern: str):
    """A protected glob as a regex over relative paths with "/" separators. "*" and "?" stay inside one
    path segment, "**" crosses segments ("**/" also matches no directory at all), and a pattern also covers
    everything below a directory it names: "tests" and "tests/**" both protect tests/a/b.py."""
    out = []
    i = 0
    while i < len(pattern):
        if pattern.startswith("**/", i):
            out.append("(?:.*/)?")
            i += 3
        elif pattern.startswith("**", i):
            out.append(".*")
            i += 2
        elif pattern[i] == "*":
            out.append("[^/]*")
            i += 1
        elif pattern[i] == "?":
            out.append("[^/]")
            i += 1
        else:
            out.append(re.escape(pattern[i]))
            i += 1
    # Case-insensitive: on a case-insensitive file system (the macOS default) "Tests" is "tests".
    return re.compile("^" + "".join(out) + "(?:/.*)?$", re.IGNORECASE)


def _remove(path):
    if os.path.islink(path) or os.path.isfile(path):
        os.unlink(path)
    elif os.path.isdir(path):
        shutil.rmtree(path)


def _real_parents(root, rel):
    """Make every parent directory of root/rel a real directory: a symbolic link or a file in the way (the
    agent replaced tests/ by a link to a directory it controls) is removed first."""
    cur = root
    for part in rel.split("/")[:-1]:
        cur = os.path.join(cur, part)
        st = os.lstat(cur) if os.path.lexists(cur) else None
        if st is not None and not stat.S_ISDIR(st.st_mode):
            os.unlink(cur)
            st = None
        if st is None:
            os.mkdir(cur)


class TamperError(Exception):
    """The agent (or something it started) changed what grading relies on."""


def lock_tree(root):
    """Make a tree read-only (files 0444 or 0555, directories 0555). The same user can undo it, so this only
    stops accidents; the digest check before grading is what catches a deliberate change."""
    for cur, dirs, names in os.walk(root, topdown=False):
        for n in names:
            p = os.path.join(cur, n)
            os.chmod(p, 0o555 if os.stat(p).st_mode & 0o111 else 0o444)
        os.chmod(cur, 0o555)


def rmtree_force(path):
    def onerror(func, p, _exc):
        try:
            os.chmod(os.path.dirname(p), 0o755)
            os.chmod(p, 0o755)
        except OSError:
            pass
        try:
            func(p)
        except OSError:
            pass
    shutil.rmtree(path, onerror=onerror)


def restore_protected(work: str, pristine: str, patterns: list) -> list:
    """Make every path the patterns cover in `work` the same as in `pristine`: remove files the agent added
    and REPLACE every protected file with a fresh copy of the pristine one, never following a symbolic link.
    A file is replaced even when its content already matches: a hard link to a file the agent controls (or a
    link planted for later) is not the pristine file. Returns the relative paths that were not as they should
    be (empty when the agent left them alone). Raises OSError when a path cannot be replaced (for example a
    directory the agent made read-only), which the caller treats as tampering."""
    if not patterns:
        return []
    rx = [glob_regex(p) for p in patterns]

    def covered(rel):
        return any(r.match(rel) for r in rx)

    want = {rel: p for rel, p in suitemod.list_tree(pristine) if covered(rel)}
    want_by_cf = {rel.casefold(): rel for rel in want}
    want_dirs = {r.rsplit("/", 1)[0] for r in want if "/" in r} | {r.rsplit("/", 2)[0] for r in want if r.count("/") > 1}
    want_dirs_cf = {d.casefold() for d in want_dirs}
    have = set()
    for cur, dirs, names in os.walk(work, followlinks=False):
        for n in dirs + names:
            p = os.path.join(cur, n)
            rel = os.path.relpath(p, work).replace(os.sep, "/")
            if covered(rel) and (os.path.islink(p) or not os.path.isdir(p)):
                have.add(rel)
    changed = []
    for rel in sorted(have):
        twin = want_by_cf.get(rel.casefold())
        if rel in want:
            continue
        if twin is not None:
            # A case variant of a protected path ("TESTS/x.py" for "tests/x.py"). On a case-insensitive file system
            # it names the same file, which is replaced below; on a case-sensitive one it is a different file the
            # agent made beside the protected one, and it is removed so it cannot be mistaken for it later.
            try:
                if os.path.samefile(os.path.join(work, rel), os.path.join(work, twin)):
                    continue
            except OSError:
                pass
        _remove(os.path.join(work, rel))  # added by the agent
        changed.append(rel)
        # a case variant of a protected directory that is now empty goes too (rmdir only removes empty ones)
        parent = rel
        while "/" in parent:
            parent = parent.rsplit("/", 1)[0]
            if parent.casefold() in want_dirs_cf and parent not in want_dirs:
                try:
                    os.rmdir(os.path.join(work, parent))
                except OSError:
                    break
    for rel in sorted(want):
        dst = os.path.join(work, rel)
        src = want[rel]
        st = os.lstat(dst) if os.path.lexists(dst) else None
        exe = bool(os.stat(src).st_mode & 0o111)
        same = (st is not None and stat.S_ISREG(st.st_mode) and st.st_nlink == 1 and st.st_size == os.lstat(src).st_size
                and file_sha(dst) == file_sha(src) and (bool(st.st_mode & 0o111) == exe))
        _real_parents(work, rel)
        if os.path.lexists(dst):
            _remove(dst)
        shutil.copyfile(src, dst, follow_symlinks=False)
        os.chmod(dst, 0o755 if exe else 0o644)
        if not same:
            changed.append(rel)
    return changed


# ---- running a child ----------------------------------------------------------------------------


def run_child(argv, cwd, env, timeout_s, stdin_path, log_path, grace=5.0):
    """Run argv in its own process group with a deadline. Returns {"rc", "timed_out", "start_error",
    "seconds", "tail", "log"}; on a timeout the group gets SIGTERM, then SIGKILL after `grace`."""
    r, w = os.pipe()
    stdin = None
    started = time.monotonic()
    try:
        stdin = open(stdin_path, "rb") if stdin_path else subprocess.DEVNULL
        proc = subprocess.Popen(argv, env=env, cwd=cwd, stdin=stdin, stdout=w, stderr=w, start_new_session=True,
                                close_fds=True)
    except (OSError, ValueError) as e:
        os.close(r)
        os.close(w)
        return {"rc": None, "timed_out": False, "start_error": getattr(e, "strerror", None) or str(e),
                "seconds": 0.0, "tail": "", "log": log_path}
    finally:
        if stdin_path and stdin is not None:
            stdin.close()
    os.close(w)
    cap = R.LogCapture(r, log_path, LOG_CAP)
    cap.start()
    timed_out = False
    deadline = started + timeout_s
    while not R.leader_exited(proc):
        if time.monotonic() > deadline:
            timed_out = True
            R.kill_group(proc, grace)
            break
        time.sleep(0.02)
    R.signal_group(proc.pid, signal.SIGKILL)  # anything the child left running in its group
    rc = R.reap(proc)
    cap.finish()
    return {"rc": rc, "timed_out": timed_out, "start_error": None, "seconds": time.monotonic() - started,
            "tail": " | ".join(cap.tail_lines()), "log": log_path, "dropped": cap.dropped}


def child_env(base_names, extra, home):
    env = {n: os.environ[n] for n in base_names if n in os.environ}
    env["HOME"] = home
    env.update(extra)
    return env


def substitute(argv, mapping):
    """Replace {name} in an argument when the whole argument is exactly that placeholder or contains it;
    only the names in `mapping` are known, so a stray brace is left alone."""
    out = []
    for a in argv:
        for k, v in mapping.items():
            a = a.replace("{%s}" % k, v)
        out.append(a)
    return out


# ---- the agents ---------------------------------------------------------------------------------


def agent_command(arm, work, tdir, mapping):
    """(argv, stdin_path, last_message_path). The prompt always reaches claude and codex through a private
    file on stdin, never on a command line."""
    a, args = arm["agent"], arm["args"]
    prompt = mapping["prompt_file"]
    if a == "cmd":
        return substitute(args["argv"], mapping), None, None
    if a == "claude":
        argv = [args.get("bin", "claude"), "-p", "--output-format", "json",
                "--permission-mode", args.get("permission_mode", "dontAsk"), "--permission-prompts", "none",
                "--max-budget-usd", str(args.get("budget_usd", 2)), "--no-session-persistence",
                "--allowedTools", args.get("allowed_tools", "Read,Glob,Grep,Edit,Write")]
        if "model" in args:
            argv += ["--model", args["model"]]
        return argv, prompt, None
    last = os.path.join(tdir, "last-message.txt")
    argv = [args.get("bin", "codex"), "exec", "-s", args.get("sandbox", "workspace-write"), "-C", work,
            "--skip-git-repo-check", "--ephemeral", "-o", last]
    if "model" in args:
        argv += ["-m", args["model"]]
    return argv + ["-"], prompt, last


def _num(v, hi):
    return isinstance(v, (int, float)) and not isinstance(v, bool) and v == v and 0 <= v <= hi


def claude_cost(log_path, dropped):
    """total_cost_usd from the JSON that `claude -p --output-format json` prints, the only cost field the
    research verified. None unless the whole output was kept and the field is a sane number."""
    if dropped:
        return None
    raw, _ = R.read_regular(log_path, 4 << 20)
    text = (raw or b"").decode("utf-8", "replace")
    for candidate in [text] + text.strip().splitlines()[::-1]:
        try:
            doc = json.loads(candidate)
        except ValueError:
            continue
        if isinstance(doc, dict) and "result" in doc:
            cost = doc.get("total_cost_usd")
            return float(cost) if _num(cost, 100000) else None
    return None


def cmd_report(path):
    """(cost_usd, tokens) from the JSON file a `cmd` agent may write at BENCH_REPORT_FILE. A number the
    agent did not give stays None. The values are the command's own claim and are used as given."""
    raw, _ = R.read_regular(path, 4096)
    if not raw or len(raw) > 4096:
        return None, None
    try:
        doc = json.loads(raw.decode("utf-8"))
    except ValueError:
        return None, None
    if not isinstance(doc, dict):
        return None, None
    cost, tok = doc.get("cost_usd"), doc.get("tokens")
    return (float(cost) if _num(cost, 100000) else None,
            int(tok) if isinstance(tok, int) and not isinstance(tok, bool) and 0 <= tok <= 10**12 else None)


# ---- the grader ---------------------------------------------------------------------------------


def pins_problem(pins, suite_dir):
    """Describe the first pinned grader file whose content changed or vanished, else None."""
    for rel, want in sorted(pins.items()):
        path = os.path.join(suite_dir, rel)
        try:
            got = file_sha(path)
        except OSError:
            return "%s is gone" % rel
        if got != want:
            return "%s changed" % rel
    return None


def _inside(prog, work):
    """True when a grader program that failed to start lives in (or is relative to) the work directory: an agent
    may have deleted it, which is a failure of the attempt, not of the harness."""
    if os.sep not in prog:
        return False
    full = os.path.abspath(prog if os.path.isabs(prog) else os.path.join(work, prog))
    return full.startswith(os.path.realpath(work) + os.sep) or full.startswith(os.path.abspath(work) + os.sep)


def grade(task, work, suite_dir, ghome, tdir, redactor):
    """Run the grader 1 + reruns times, with an environment of its own (a fresh empty HOME, so nothing the agent
    left in its HOME is read; no user site directory for a Python grader). A run is p (exit 0), f (any other exit,
    including 126 and 127, and a program inside the work directory that is gone), t (timeout) or e (the harness
    could not run it: exit 125, or a program outside the work directory that cannot be started). One e makes
    the attempt an error. Otherwise the attempt passes when strictly more than half of the runs passed; if it does
    not pass it is a timeout when every unsuccessful run timed out and a failure otherwise. The runs disagreeing
    is "flaky". Grader files named in `grader.files` are hashed before every run: a change is tampering, a fail.
    Returns {"runs", "outcome", "flaky", "exit", "tail", "tampered"}."""
    g = task["grader"]
    mapping = {"work": work, "suite_dir": suite_dir}
    argv = substitute(g["cmd"], mapping)
    env = child_env(BASE_ENV, {"BENCH_WORKDIR": work, "PYTHONNOUSERSITE": "1"}, ghome)
    kinds, last_rc, last_tail = [], None, ""
    for i in range(1 + g["reruns"]):
        bad = pins_problem(g.get("files", {}), suite_dir)
        if bad:
            kinds.append("f")
            last_rc, last_tail = None, "a pinned grader file was altered: %s" % bad
            return {"runs": "".join(kinds), "outcome": "fail", "flaky": False, "exit": None, "tampered": 1,
                    "tail": R.one_line(last_tail, SUMMARY_BYTES)}
        res = run_child(argv, work, env, g["timeout_s"], None, os.path.join(tdir, "grader-%d.log" % i))
        if res["start_error"]:
            kinds.append("f" if _inside(argv[0], work) else "e")
            last_tail = "the grader did not start: %s" % res["start_error"]
            last_rc = None
            continue
        rc = res["rc"]
        last_rc, last_tail = rc, res["tail"]
        if res["timed_out"]:
            kinds.append("t")
        elif rc == 0:
            kinds.append("p")
        elif rc in GRADER_ERROR_EXITS:
            kinds.append("e")
        else:
            kinds.append("f")
    runs = "".join(kinds)
    tail = R.one_line(redactor(last_tail), SUMMARY_BYTES)
    if "e" in kinds:
        return {"runs": runs, "outcome": "error", "flaky": False, "exit": last_rc, "tampered": 0,
                "tail": "the grader could not run: " + (tail or "no output")}
    passes = kinds.count("p")
    if passes * 2 > len(kinds):
        outcome = "pass"
    elif all(k == "t" for k in kinds if k != "p"):
        outcome = "timeout"
    else:
        outcome = "fail"
    return {"runs": runs, "outcome": outcome, "flaky": 0 < passes < len(kinds), "exit": last_rc, "tail": tail, "tampered": 0}


# ---- the attempt --------------------------------------------------------------------------------


def find(items, ident, what):
    for x in items:
        if x["id"] == ident:
            return x
    raise HarnessError("the suite has no %s %s" % (what, ident))


def run_attempt(body, work_root, suite_root, keep=False):
    """The whole attempt. Returns the bench result dict (never raises for an agent or grader problem).
    suite_root is the directory the operator allows suites in: the task body is untrusted text (anyone who can
    add a board task can write one), and a suite file can name any command, so a suite outside it is refused.
    The pristine copy of the fixture lives in a directory of its own, not under the attempt directory the agent
    is told about (BENCH_PROMPT_FILE, BENCH_REPORT_FILE), is made read-only, and is hashed again right before
    protected files are restored from it."""
    started = time.monotonic()
    result = {"exp": body["exp"], "arm": body["arm"], "task": body["task"], "outcome": "error", "flaky": False,
              "tampered": 0, "duration_ms": None, "cost_usd": None, "tokens": None, "grader_exit": None,
              "grader_runs": "", "summary": ""}
    tdir = tempfile.mkdtemp(prefix="bench-", dir=work_root)
    os.chmod(tdir, 0o700)
    pdir = tempfile.mkdtemp(prefix="bench-p-", dir=work_root)
    os.chmod(pdir, 0o700)
    secrets = R.secret_values_from([(n, v) for n, v in os.environ.items() if SECRET_NAME.search(n)])

    def redactor(text):
        return R.redact(text, secrets)

    try:
        try:
            real, root = os.path.realpath(body["suite"]), os.path.realpath(suite_root)
            if not real.startswith(root + os.sep):
                raise HarnessError("the suite %s is outside the allowed suite directory" % body["suite"][:120])
            suite, sha, _ = suitemod.load_suite(real)
            if sha != body["sha"]:
                raise HarnessError("the suite file changed after the experiment started (sha256 differs)")
            task = find(suite["tasks"], body["task"], "task")
            arm = find(suite["arms"], body["arm"], "arm")
            suite_dir = os.path.dirname(real)  # from the resolved path: the checked one, not a link to it
            src = suitemod.fixture_root(suite_dir, task["fixture"]["dir"])
            pristine = os.path.join(pdir, "pristine")
            suitemod.list_tree(src)  # refuses links and oversize trees before anything is copied
            shutil.copytree(src, pristine, symlinks=False)
            got = suitemod.tree_digest(pristine)
            if got != task["fixture"]["digest"]:
                raise HarnessError("the fixture does not match its digest (expected %s, got %s)"
                                   % (task["fixture"]["digest"][:12], got[:12]))
            work = os.path.join(tdir, "work")
            shutil.copytree(pristine, work, symlinks=False)
            lock_tree(pristine)
            home = os.path.join(tdir, "home")
            os.mkdir(home, 0o700)
            ghome = os.path.join(pdir, "ghome")  # a HOME of the grader's own, never shown to the agent
            os.mkdir(ghome, 0o700)
            prompt_file = os.path.join(tdir, "prompt.txt")
            fd = os.open(prompt_file, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
            with os.fdopen(fd, "w", encoding="utf-8") as f:
                f.write(task["prompt"])
            report_file = os.path.join(tdir, "agent-report.json")
            mapping = {"work": work, "suite_dir": suite_dir, "prompt_file": prompt_file}
            argv, stdin_path, last_msg = agent_command(arm, work, tdir, mapping)
            names = list(BASE_ENV) + [n for n in arm["env_allow"] if n in os.environ]
            extra = {"BENCH_PROMPT_FILE": prompt_file, "BENCH_WORKDIR": work, "BENCH_REPORT_FILE": report_file}
            agent_home = os.environ.get("HOME", home) if "HOME" in arm["env_allow"] else home
            env = child_env(names, extra, agent_home)
            ares = run_child(argv, work, env, task["timeout_s"], stdin_path, os.path.join(tdir, "agent.log"),
                             grace=5.0)
            if ares["start_error"]:
                raise HarnessError("cannot start the agent: %s" % ares["start_error"])
            result["duration_ms"] = int(ares["seconds"] * 1000)
            if arm["agent"] == "claude":
                result["cost_usd"] = claude_cost(ares["log"], ares.get("dropped", 0))
            elif arm["agent"] == "cmd":
                result["cost_usd"], result["tokens"] = cmd_report(report_file)
            if ares["timed_out"]:
                result.update(outcome="timeout", summary="the agent did not finish in %ds" % task["timeout_s"])
                return result
            try:
                if suitemod.tree_digest(pristine) != task["fixture"]["digest"]:
                    raise TamperError("the pristine copy of the fixture was changed during the attempt")
                changed = restore_protected(work, pristine, task["grader"]["protected"])
            except (TamperError, OSError, suitemod.SuiteError) as e:
                # An agent that makes a protected path impossible to restore, or edits the copy restores come
                # from, has not passed: this is a failure of the attempt with tampering recorded, not a harness error.
                result.update(outcome="fail", tampered=1, grader_runs="",
                              summary="protected files could not be restored: %s" % (getattr(e, "strerror", None) or str(e)))
                return result
            result["tampered"] = len(changed)
            g = grade(task, work, suite_dir, ghome, tdir, redactor)
            result.update(outcome=g["outcome"], flaky=g["flaky"], grader_exit=g["exit"], grader_runs=g["runs"])
            result["tampered"] += g.get("tampered", 0)
            bits = []
            if ares["rc"] not in (0, None):
                bits.append("agent exit %s" % ares["rc"])
            if changed:
                bits.append("restored %d protected path%s" % (len(changed), "" if len(changed) == 1 else "s"))
            bits.append("grader runs %s" % g["runs"])
            if g["tail"]:
                bits.append(g["tail"])
            result["summary"] = "; ".join(bits)
        except HarnessError as e:
            result.update(outcome="error", summary=str(e))
        except (OSError, suitemod.SuiteError, shutil.Error) as e:
            result.update(outcome="error", summary="harness: %s" % (getattr(e, "strerror", None) or str(e)))
    finally:
        if result["outcome"] == "error" and result["duration_ms"] is None:
            result["duration_ms"] = int((time.monotonic() - started) * 1000)
        result["summary"] = R.one_line(redactor(result["summary"]), SUMMARY_BYTES)
        rmtree_force(pdir)
        if not keep:
            rmtree_force(tdir)
    return result


def write_result(path, bench):
    """TASK_RESULT_PATH: what `board run` reads (status, summary) and what `board-bench record` reads."""
    doc = {"status": "done",
           "summary": R.one_line("bench %s: %s" % (bench["outcome"], bench["summary"]), 480),
           "bench": bench}
    tmp = path + ".tmp"
    fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_TRUNC | os.O_NOFOLLOW, 0o600)
    with os.fdopen(fd, "w", encoding="utf-8") as f:
        json.dump(doc, f)
    os.replace(tmp, path)


def main_attempt(suite_dir, work_dir=None, keep=False) -> int:
    body_path, result_path = os.environ.get("TASK_BODY_PATH"), os.environ.get("TASK_RESULT_PATH")
    if not body_path or not result_path:
        sys.stderr.write("board-bench attempt: run it through `board run -- board-bench attempt` "
                         "(TASK_BODY_PATH and TASK_RESULT_PATH are not set)\n")
        return 2
    root = work_dir or tempfile.gettempdir()
    try:
        os.makedirs(root, mode=0o700, exist_ok=True)
    except OSError as e:
        sys.stderr.write("board-bench attempt: cannot use %s: %s\n" % (root, e.strerror))
        return 1
    try:
        body = read_body(body_path)
    except HarnessError as e:
        sys.stderr.write("board-bench attempt: %s\n" % e)
        return 1
    bench = run_attempt(body, root, suite_dir, keep)
    try:
        write_result(result_path, bench)
    except OSError as e:
        sys.stderr.write("board-bench attempt: cannot write the result: %s\n" % e.strerror)
        return 1
    print("bench %s %s/%s #%d: %s" % (bench["outcome"], body["arm"], body["task"], body["rep"], bench["summary"]))
    return 0

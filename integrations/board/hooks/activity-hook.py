#!/usr/bin/env python3
"""Claude Code hook: tell the Lattice board what this agent is doing right now.

Registered for PostToolUse, PostToolUseFailure, PermissionDenied, UserPromptSubmit, Stop, SessionStart and
SessionEnd (hooks/hooks.json of the board plugin, hooks/settings.activity.example.json). It reads
the hook's JSON from stdin, derives a short sanitised label, and runs `board activity`, which owns the rate
limit: most calls only append a line to a local file and no connection is made.

Rules this script keeps (each one has a test in tests/test_activity_hook.py):
  * OFF unless BOARD_ACTIVITY=1 and BOARD_AUTHOR is a valid name. Then it exits 0 at once.
  * It blocks the agent for at most 3 seconds: stdin is read with a deadline and a size cap, the board helper gets
    whatever is left of that budget and its whole process group is killed after it. The helper keeps its own
    backoff, so a database host that does not answer costs one such wait per backoff, not one per tool call.
  * It always exits 0 and prints nothing on stdout (Claude Code would add stdout of some events to the
    model's context). Failures are one fixed line on stderr, never the helper's own messages, which can
    name a host or a user.
  * It stores no content. The label of a file tool is the last two components of the path (a query string or
    fragment is cut first; a secret in a directory name is NOT removed, by design); the label of a shell
    command is its first word (at most 16 characters, never its arguments) plus whether it was a test or
    build command, and "shell" for anything else, including every command that starts with NAME=value. No
    pattern, prompt, command line, file content or tool output is read into the label.

Payload fields used (the client's hook reference, fetched 2026-11-03; PermissionDenied was listed there but
its input fields were not read, so tool_name is assumed to be present and a payload without it is recorded
as the tool "unknown"): session_id, hook_event_name, tool_name, tool_input.file_path / notebook_path /
command. SessionEnd's `reason` and every other field are ignored.
"""

import errno
import json
import os
import re
import select
import signal
import stat
import subprocess
import sys
import time

BUDGET_S = 3.0
MAX_STDIN = 1024 * 1024
MAX_PATH = 4096
MAX_COMMAND = 65536
LABEL_MAX = 64

AUTHOR_RX = re.compile(r"[A-Za-z0-9._-]{1,64}")
TOOL_RX = re.compile(r"[A-Za-z0-9_.-]{1,64}")
TASK_RX = re.compile(r"[A-Za-z0-9_-]{1,64}")
# A plausible program name: 16 characters at most, starting with a letter or digit. A pasted token (they are
# longer) or anything odd is stored as "shell".
PROGRAM_RX = re.compile(r"[A-Za-z0-9][A-Za-z0-9_.+-]{0,15}")
ASSIGN_RX = re.compile(r"[A-Za-z_][A-Za-z0-9_]*=")

FILE_TOOLS = {"Read": ("file_path",), "Edit": ("file_path",), "MultiEdit": ("file_path",),
              "Write": ("file_path",), "NotebookEdit": ("notebook_path", "file_path")}

# event -> (tool for non-tool events or None, outcome, state, flush)
EVENTS = {
    "PostToolUse": (None, "ok", "working", False),
    "PostToolUseFailure": (None, "error", "working", False),
    "PermissionDenied": (None, "denied", "working", False),
    "UserPromptSubmit": ("prompt", "ok", "working", False),
    "Stop": ("stop", "ok", "idle", True),
    "SessionStart": ("session-start", "ok", "idle", True),
    "SessionEnd": ("session-end", "ok", "ended", True),
}

TEST_PROGRAMS = {"pytest", "jest", "vitest", "tox", "rspec", "phpunit", "nosetests", "ctest"}
TEST_RUNNERS = {"cargo", "npm", "yarn", "pnpm", "go", "make", "mvn", "gradle", "dotnet", "bun", "deno", "mix", "swift"}
BUILD_PROGRAMS = {"make", "cmake", "ninja", "tsc", "gcc", "g++", "cc", "clang", "clang++", "javac", "rustc",
                  "webpack", "vite", "esbuild", "bazel"}
BUILD_WORDS = {"build", "compile", "package"}


def log_error(text):
    try:
        sys.stderr.write("board activity hook: %s\n" % text)
    except Exception:
        pass


# ---------------------------------------------------------------- reading stdin


def read_stdin(deadline, limit=MAX_STDIN):
    """Up to limit bytes from stdin, or None when the data is larger, or does not end before the deadline."""
    fd = sys.stdin.fileno()
    chunks = []
    total = 0
    while True:
        left = deadline - time.monotonic()
        if left <= 0:
            return None
        try:
            ready, _, _ = select.select([fd], [], [], min(left, 0.25))
        except (OSError, ValueError):
            return None
        if not ready:
            continue
        try:
            data = os.read(fd, 65536)
        except OSError as e:
            if e.errno in (errno.EINTR, errno.EAGAIN):
                continue
            return None
        if not data:
            return b"".join(chunks)
        total += len(data)
        if total > limit:
            return None
        chunks.append(data)


def _reject_constant(name):
    raise ValueError("not a JSON constant: " + name)


def parse_payload(raw):
    """The payload as a dict, or None when it is not a JSON object. Strict: no NaN or Infinity, no text that
    is not UTF-8, nothing after the object."""
    try:
        text = raw.decode("utf-8")
        obj = json.loads(text, parse_constant=_reject_constant)
    except (ValueError, RecursionError, UnicodeDecodeError):
        return None
    return obj if isinstance(obj, dict) else None


# ---------------------------------------------------------------- the label


def clean(text, limit=LABEL_MAX):
    """Printable ASCII only: control characters (ANSI escapes included) are dropped, any other character
    becomes _, and at most `limit` characters stay."""
    out = []
    for ch in text:
        code = ord(ch)
        if code < 0x20 or code == 0x7F:
            continue
        out.append(ch if code < 0x7F else "_")
        if len(out) >= limit * 4:
            break
    return "".join(out).strip()[:limit]


def path_label(value):
    """The last two components of a path: 'src/main.rs' for '/home/me/project/src/main.rs'. Empty,
    current-directory and parent-directory components do not count, so '../../etc/passwd' is 'etc/passwd'.
    Everything from the first ? or # on is cut first (a URL's query string can carry a token), here and not
    only in the helper. A secret in a directory name stays: that is the price of a label that tells two
    files called README.md apart."""
    if not isinstance(value, str) or not value or len(value) > MAX_PATH:
        return ""
    value = re.split(r"[?#]", value, maxsplit=1)[0]
    parts = [p for p in re.split(r"[\\/]+", value) if p not in ("", ".", "..")]
    label = "/".join(clean(p, LABEL_MAX) for p in parts[-2:])
    return label[-LABEL_MAX:] if len(label) > LABEL_MAX else label


def command_words(command):
    """The words of a shell command. A command that starts with NAME=value is just ["shell"]: its value can be
    a secret, and a value with an escaped space (TOKEN=a\\ b) would otherwise leak its second half as the
    "program"."""
    words = command.strip().split()
    if words and ASSIGN_RX.match(words[0]):
        return ["shell"]
    return words


def command_class(words):
    """'test', 'build' or '' from the first words only. Nothing but this class leaves the function."""
    if not words:
        return ""
    prog = os.path.basename(words[0])
    second = words[1] if len(words) > 1 else ""
    third = words[2] if len(words) > 2 else ""
    if prog in TEST_PROGRAMS:
        return "test"
    if prog in TEST_RUNNERS and second in ("test", "tests"):
        return "test"
    if prog in ("python", "python3") and second == "-m" and third in ("pytest", "unittest"):
        return "test"
    if prog in ("npm", "yarn", "pnpm", "bun") and second == "run" and third in ("test", "tests"):
        return "test"
    if prog in ("npm", "yarn", "pnpm", "bun") and second == "run" and third == "build":
        return "build"
    if prog in ("npm", "yarn", "pnpm", "cargo", "go", "mvn", "gradle", "dotnet", "bun", "bazel") and second in BUILD_WORDS:
        return "build"
    if prog in BUILD_PROGRAMS:
        return "build"
    return ""


def command_label(command):
    """The first word of the command (its base name), plus ' [test]' or ' [build]'. A first word that is not
    a plain program name (a quote, a subshell, a path with odd characters) is 'shell'."""
    if not isinstance(command, str) or len(command) > MAX_COMMAND:
        return ""
    words = command_words(command)
    if not words:
        return ""
    prog = os.path.basename(words[0].rstrip("/")) if "/" in words[0] else words[0]
    if not PROGRAM_RX.fullmatch(prog):
        return "shell"
    cls = command_class(words)
    return prog + (" [%s]" % cls if cls and prog != "git" else "")


def tool_label(tool, tool_input):
    if not isinstance(tool_input, dict):
        return ""
    if tool in FILE_TOOLS:
        for key in FILE_TOOLS[tool]:
            value = tool_input.get(key)
            if isinstance(value, str) and value:
                return path_label(value)
        return ""
    if tool == "Bash":
        return command_label(tool_input.get("command"))
    return ""


def tool_name(value):
    return value if isinstance(value, str) and TOOL_RX.fullmatch(value) else "other"


def session_name(value):
    if isinstance(value, str):
        value = re.sub(r"[^A-Za-z0-9._-]", "", value)[:64]
        if value:
            return value
    return "claude"


# ---------------------------------------------------------------- the task this agent holds


def state_dir():
    return os.environ.get("BOARD_STATE_DIR") or os.path.join(
        os.environ.get("XDG_STATE_HOME") or os.path.join(os.environ.get("HOME") or ".", ".local", "state"),
        "lattice-board")


def held_task(author, now=None):
    """The id of the task this agent holds a lease on, or ''. Only names and one JSON field are read: the file
    name AUTHOR.TASK that `board claim` leaves (its content is the fence, which is never read) and the "task"
    fields of board-mcp's lease file. The newest file wins; a file older than a week is a leftover."""
    now = time.time() if now is None else now
    base = state_dir()
    try:
        st = os.lstat(base)
        if not stat.S_ISDIR(st.st_mode) or st.st_uid != os.getuid():
            return ""
        names = os.listdir(base)
    except OSError:
        return ""
    best = ("", -1.0)
    prefix = author + "."
    for name in names[:5000]:
        if not name.startswith(prefix):
            continue
        task = name[len(prefix):]
        if not TASK_RX.fullmatch(task):
            continue
        try:
            fst = os.lstat(os.path.join(base, name))
        except OSError:
            continue
        if not stat.S_ISREG(fst.st_mode) or fst.st_uid != os.getuid() or now - fst.st_mtime > 7 * 86400:
            continue
        if fst.st_mtime > best[1]:
            best = (task, fst.st_mtime)
    path = os.path.join(base, "mcp-leases.%s.json" % author)
    try:
        fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
        try:
            fst = os.fstat(fd)
            if stat.S_ISREG(fst.st_mode) and fst.st_uid == os.getuid() and now - fst.st_mtime <= 7 * 86400:
                data = json.loads(os.read(fd, 1024 * 1024).decode("utf-8"))
                leases = data.get("leases") if isinstance(data, dict) else None
                if isinstance(leases, dict) and fst.st_mtime > best[1]:
                    for entry in list(leases.values())[:64]:
                        task = entry.get("task") if isinstance(entry, dict) else None
                        if isinstance(task, str) and TASK_RX.fullmatch(task):
                            best = (task, fst.st_mtime)
                            break
        finally:
            os.close(fd)
    except (OSError, ValueError, UnicodeDecodeError, RecursionError):
        pass
    return best[0]


# ---------------------------------------------------------------- running the helper


def helper_path():
    given = os.environ.get("BOARD_BIN")
    if given:
        return given
    here = os.path.dirname(os.path.realpath(__file__))
    return os.path.join(os.path.dirname(here), "board")


def build_args(payload, author):
    """The `board activity` arguments for a payload, or None when the event is not one we record."""
    event = payload.get("hook_event_name")
    spec = EVENTS.get(event) if isinstance(event, str) else None
    if spec is None:
        return None
    fixed_tool, outcome, state, flush = spec
    if fixed_tool is None:
        tool = tool_name(payload.get("tool_name", "unknown"))
        target = tool_label(tool, payload.get("tool_input"))
    else:
        tool, target = fixed_tool, ""
    args = ["activity", "--tool", tool, "--outcome", outcome, "--state", state,
            "--session", session_name(payload.get("session_id"))]
    if target:
        args += ["--target", target]
    task = held_task(author)
    if task:
        args += ["--task", task]
    if flush:
        args.append("--flush")
    return args


def run_helper(args, deadline):
    """Run the helper in its own process group with whatever is left of the budget; kill the group after it.
    Returns the exit status, or None on a timeout or a start failure. Its output is discarded."""
    exe = helper_path()
    if not os.access(exe, os.X_OK):
        log_error("the board helper is not executable")
        return None
    env = dict(os.environ)
    env.setdefault("PGCONNECT_TIMEOUT", "2")
    env["BOARD_ACTIVITY"] = "1"
    try:
        proc = subprocess.Popen([exe] + args, env=env, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                                stderr=subprocess.DEVNULL, start_new_session=True, close_fds=True)
    except OSError:
        log_error("cannot start the board helper")
        return None
    try:
        return proc.wait(timeout=max(0.2, deadline - time.monotonic()))
    except subprocess.TimeoutExpired:
        log_error("the board helper did not finish in time; stopped it")
        return None
    finally:
        if proc.poll() is None:
            # SIGTERM first: the helper's trap removes its temporary files, SIGKILL would leave them in TMPDIR.
            for sig, grace in ((signal.SIGTERM, 0.3), (signal.SIGKILL, 1.0)):
                try:
                    os.killpg(proc.pid, sig)
                except OSError:
                    try:
                        proc.kill()
                    except OSError:
                        pass
                try:
                    proc.wait(timeout=grace)
                    break
                except subprocess.TimeoutExpired:
                    pass


def main():
    start = time.monotonic()
    deadline = start + BUDGET_S
    author = os.environ.get("BOARD_AUTHOR", "")
    if os.environ.get("BOARD_ACTIVITY") != "1" or not AUTHOR_RX.fullmatch(author):
        return 0
    raw = read_stdin(deadline)
    if raw is None:
        log_error("the hook input was too large or did not arrive in time; skipped")
        return 0
    payload = parse_payload(raw)
    if payload is None:
        log_error("the hook input is not a JSON object; skipped")
        return 0
    args = build_args(payload, author)
    if args is None:
        return 0
    run_helper(args, deadline)
    return 0


if __name__ == "__main__":
    try:
        code = main()
    except BaseException as e:  # a hook must never fail the agent
        log_error("stopped by %s" % e.__class__.__name__)
        code = 0
    try:
        sys.stderr.flush()
    except Exception:
        pass
    os._exit(code)

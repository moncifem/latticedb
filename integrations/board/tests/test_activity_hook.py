"""hooks/activity-hook.py: the contract with Claude Code, the privacy rules, and what it must never do.

The hook runs `board activity` through BOARD_BIN; most tests point it at a stub that records its arguments, a
few at the real helper with a fake psql. Expected labels are derived by hand from the rules in the hook's
docstring and CONVENTIONS.md ("Agent activity"), not by running the hook and copying the output.

Payload fields (session_id, hook_event_name, tool_name, tool_input) are those of the client's hook reference,
fetched 2026-11-03 (see the docstring of the hook; PermissionDenied's own fields were not read there).

Run: python3 -m unittest discover -s integrations/board/tests -p "test_activity_hook.py"
"""

import importlib.util
import json
import os
import shutil
import signal
import stat
import subprocess
import sys
import tempfile
import time
import unittest

import actbase
from actbase import HOOK, WRITE, T0, vars_of

STUB = """#!/bin/sh
printf '%s\\n' "$(python3 -c 'import json,sys; print(json.dumps(sys.argv[1:]))' "$@")" >>"$STUB_LOG"
printf '%s\\n' "$BOARD_ACTIVITY" >"$STUB_LOG.activity"
[ -n "${STUB_SLEEP:-}" ] && sleep "$STUB_SLEEP"
exit "${STUB_RC:-0}"
"""


def load_hook():
    spec = importlib.util.spec_from_file_location("activity_hook_under_test", HOOK)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


hook = load_hook()


def process_alive(pid):
    """True while the process runs. A zombie (killed, not yet reaped: nobody in a container reaps it) is gone."""
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    try:
        with open("/proc/%d/stat" % pid) as f:
            return f.read().rsplit(")", 1)[1].split()[0] != "Z"
    except (OSError, IndexError):
        pass
    r = subprocess.run(["ps", "-o", "stat=", "-p", str(pid)], capture_output=True, text=True)
    return bool(r.stdout.strip()) and not r.stdout.strip().startswith("Z")


class HookCase(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp(prefix="act-hook-")
        self.addCleanup(shutil.rmtree, self.tmp, True)
        os.chmod(self.tmp, 0o700)
        self.stub = os.path.join(self.tmp, "board")
        with open(self.stub, "w") as f:
            f.write(STUB)
        os.chmod(self.stub, 0o755)
        self.log = os.path.join(self.tmp, "stub.log")
        self.state = os.path.join(self.tmp, "state")
        os.mkdir(self.state, 0o700)

    def env(self, **extra):
        e = {"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "HOME": self.tmp, "TMPDIR": self.tmp,
             "BOARD_BIN": self.stub, "BOARD_AUTHOR": "alice", "BOARD_ACTIVITY": "1", "BOARD_STATE_DIR": self.state,
             "STUB_LOG": self.log}
        e.update(extra)
        return {k: v for k, v in e.items() if v is not None}

    def run_hook(self, payload, raw=None, **extra):
        data = raw if raw is not None else json.dumps(payload).encode()
        if isinstance(data, str):
            data = data.encode()
        t = time.monotonic()
        r = subprocess.run([sys.executable, HOOK], input=data, env=self.env(**extra), capture_output=True, timeout=30)
        r.elapsed = time.monotonic() - t
        return r

    def calls(self):
        if not os.path.exists(self.log):
            return []
        with open(self.log) as f:
            return [json.loads(line) for line in f if line.strip()]

    def assertQuiet(self, r):
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(r.stdout, b"", "a hook must print nothing on stdout: Claude Code could add it to the context")


def post_tool(tool, tool_input, session="sess-1", event="PostToolUse"):
    return {"session_id": session, "transcript_path": "/x/y.jsonl", "cwd": "/work", "permission_mode": "default",
            "hook_event_name": event, "tool_name": tool, "tool_input": tool_input, "tool_response": {"ok": True},
            "tool_use_id": "toolu_1", "duration_ms": 12}


class Gate(HookCase):
    def test_it_is_off_unless_board_activity_is_1(self):
        for v in (None, "", "0", "true", "yes"):
            r = self.run_hook(post_tool("Edit", {"file_path": "/a/b.rs"}), BOARD_ACTIVITY=v)
            self.assertQuiet(r)
        self.assertEqual(self.calls(), [])

    def test_it_needs_a_valid_author(self):
        for v in (None, "", "a b", "x" * 65, "a/b"):
            self.assertQuiet(self.run_hook(post_tool("Edit", {"file_path": "/a/b.rs"}), BOARD_AUTHOR=v))
        self.assertEqual(self.calls(), [])

    def test_the_off_state_does_not_even_read_stdin(self):
        r = subprocess.run([sys.executable, HOOK], stdin=subprocess.DEVNULL, env=self.env(BOARD_ACTIVITY=None),
                           capture_output=True, timeout=10)
        self.assertQuiet(r)
        self.assertEqual(r.stderr, b"")


class Events(HookCase):
    def args(self):
        return self.calls()[-1]

    def test_post_tool_use_of_a_file_tool(self):
        r = self.run_hook(post_tool("Edit", {"file_path": "/Users/me/proj/src/lib.rs", "old_string": "SECRET-OLD",
                                             "new_string": "SECRET-NEW"}))
        self.assertQuiet(r)
        self.assertEqual(self.args(), ["activity", "--tool", "Edit", "--outcome", "ok", "--state", "working",
                                       "--session", "sess-1", "--target", "src/lib.rs"])
        self.assertNotIn("SECRET", json.dumps(self.calls()))
        with open(self.log + ".activity") as f:
            self.assertEqual(f.read().strip(), "1")

    def test_post_tool_use_of_a_shell_command_keeps_only_the_first_word(self):
        r = self.run_hook(post_tool("Bash", {"command": "curl --data 'value=SECRET-TOKEN' https://x.example/a?k=v",
                                            "description": "fetch"}))
        self.assertQuiet(r)
        self.assertEqual(self.args()[-2:], ["--target", "curl"])
        self.assertNotIn("SECRET", json.dumps(self.calls()))
        self.assertNotIn("value=", json.dumps(self.calls()))

    def test_failure_denial_prompt_stop_start_end(self):
        cases = [
            (post_tool("Bash", {"command": "false"}, event="PostToolUseFailure"),
             ["activity", "--tool", "Bash", "--outcome", "error", "--state", "working", "--session", "sess-1", "--target", "false"]),
            (post_tool("Write", {"file_path": "/a/b/c.txt"}, event="PermissionDenied"),
             ["activity", "--tool", "Write", "--outcome", "denied", "--state", "working", "--session", "sess-1", "--target", "b/c.txt"]),
            ({"session_id": "sess-1", "hook_event_name": "UserPromptSubmit", "prompt": "PRIVATE PROMPT TEXT"},
             ["activity", "--tool", "prompt", "--outcome", "ok", "--state", "working", "--session", "sess-1"]),
            ({"session_id": "sess-1", "hook_event_name": "Stop", "stop_hook_active": False, "last_assistant_message": "PRIVATE"},
             ["activity", "--tool", "stop", "--outcome", "ok", "--state", "idle", "--session", "sess-1", "--flush"]),
            ({"session_id": "sess-1", "hook_event_name": "SessionStart", "source": "startup"},
             ["activity", "--tool", "session-start", "--outcome", "ok", "--state", "idle", "--session", "sess-1", "--flush"]),
            ({"session_id": "sess-1", "hook_event_name": "SessionEnd", "reason": "other"},
             ["activity", "--tool", "session-end", "--outcome", "ok", "--state", "ended", "--session", "sess-1", "--flush"]),
        ]
        for payload, expected in cases:
            with self.subTest(event=payload["hook_event_name"]):
                if os.path.exists(self.log):
                    os.unlink(self.log)
                self.assertQuiet(self.run_hook(payload))
                self.assertEqual(self.calls(), [expected])
                self.assertNotIn("PRIVATE", json.dumps(self.calls()))

    def test_events_that_are_not_recorded_run_nothing(self):
        for name in ("SubagentStop", "PreToolUse", "Notification", "PreCompact", "TaskCompleted", "Nonsense", "", None, 7):
            self.assertQuiet(self.run_hook({"session_id": "s", "hook_event_name": name, "tool_name": "Edit"}))
        self.assertEqual(self.calls(), [])

    def test_a_subagent_call_is_recorded_under_the_same_session(self):
        p = post_tool("Read", {"file_path": "/a/b/c.rs"})
        p.update(agent_id="agent-9", agent_type="Explore")
        self.assertQuiet(self.run_hook(p))
        self.assertIn("sess-1", self.args())

    def test_other_tools_carry_no_target(self):
        for tool, ti in (("Grep", {"pattern": "SECRET", "path": "/a/b"}), ("Glob", {"pattern": "**/*.rs"}),
                         ("WebFetch", {"url": "https://x.example/?token=SECRET"}), ("WebSearch", {"query": "SECRET"}),
                         ("mcp__plugin_board_board__board_note", {"task": "t1", "text": "SECRET"}),
                         ("Task", {"prompt": "SECRET", "subagent_type": "Explore"})):
            with self.subTest(tool=tool):
                if os.path.exists(self.log):
                    os.unlink(self.log)
                self.assertQuiet(self.run_hook(post_tool(tool, ti)))
                a = self.args()
                self.assertNotIn("--target", a)
                self.assertEqual(a[2], tool)
                self.assertNotIn("SECRET", json.dumps(a))

    def test_the_tool_name_is_validated(self):
        for name, shown in (("Edit", "Edit"), ("mcp__a__b-c.d", "mcp__a__b-c.d"), ("bad name", "other"), ("x" * 65, "other"),
                            ("a;b", "other"), ("", "other"), (7, "other"), (None, "other"), ("é", "other")):
            with self.subTest(name=name):
                if os.path.exists(self.log):
                    os.unlink(self.log)
                self.assertQuiet(self.run_hook({"session_id": "s", "hook_event_name": "PostToolUse", "tool_name": name,
                                                "tool_input": {}}))
                self.assertEqual(self.args()[2], shown)

    def test_a_missing_tool_name_is_recorded_as_unknown(self):
        self.assertQuiet(self.run_hook({"session_id": "s", "hook_event_name": "PermissionDenied"}))
        self.assertEqual(self.args()[2], "unknown")

    def test_the_session_name_is_made_safe(self):
        for given, shown in (("abc-123_X.y", "abc-123_X.y"), ("a b/c;d", "abcd"), ("../..", "...."), ("é", "claude"),
                             ("", "claude"), (None, "claude"), (5, "claude"), ("s" * 100, "s" * 64)):
            with self.subTest(given=given):
                if os.path.exists(self.log):
                    os.unlink(self.log)
                self.assertQuiet(self.run_hook({"session_id": given, "hook_event_name": "Stop"}))
                a = self.args()
                self.assertEqual(a[a.index("--session") + 1], shown)


class Labels(unittest.TestCase):
    """The two label functions, as a table. Hand-derived: last two components, never '.', '..' or empty ones;
    first word only, assignments (their values can be secrets) skipped, class in brackets."""

    PATHS = [
        ("/Users/me/project/src/main.rs", "src/main.rs"),
        ("main.rs", "main.rs"),
        ("/main.rs", "main.rs"),
        ("src/main.rs", "src/main.rs"),
        ("../../etc/passwd", "etc/passwd"),
        ("/a/b/../c", "b/c"),
        ("a/./b/./c.txt", "b/c.txt"),
        ("C:\\Users\\me\\notes.md", "me/notes.md"),
        ("///", ""),
        ("..", ""),
        (".", ""),
        ("", ""),
        ("/tmp/\x1b[31mred\x1b[0m.txt", "tmp/[31mred[0m.txt"),
        ("/a/b\tc/d\ne", "bc/de"),
        ("/proj/\u00e9t\u00e9.rs", "proj/_t_.rs"),
        # a URL's query string or fragment is cut before the components are taken (a token can sit in it)
        ("https://x.com/a/b?token=SECRET", "a/b"),
        ("https://x.com/a/b#token=SECRET/c", "a/b"),
        ("/p/q.txt?x=/y/z", "p/q.txt"),
        ("a?b/c/d", "a"),
        ("?only/q", ""),
        ("/x/" + "n" * 100, "n" * 64),  # a component is cut at 64, then the whole label keeps its last 64
        (None, ""),
        (5, ""),
        (["/a/b"], ""),
    ]

    def test_path_labels(self):
        for given, expected in self.PATHS:
            with self.subTest(given=given):
                self.assertEqual(hook.path_label(given), expected)

    def test_a_path_over_the_cap_gives_nothing(self):
        self.assertEqual(hook.path_label("/a" * 3000), "")

    COMMANDS = [
        ("ls -la /secret/dir", "ls"),
        ("git push --force origin main", "git"),
        ("/usr/bin/git status", "git"),
        ("./run.sh --token=abc", "run.sh"),
        # a command that starts with NAME=value is "shell": its value can be a secret, and an escaped space puts the
        # rest of the value where the program name would be
        ("FOO=secret BAR=2 make test", "shell"),
        ("API_KEY=abc123 curl https://x", "shell"),
        ("TOKEN=abc\\ def curl", "shell"),
        ("TOKEN=a\\ secretvalue deploy", "shell"),
        ("TOKEN=\\ secretvalue deploy", "shell"),
        ("TOKEN='a secretvalue' deploy", "shell"),
        ('TOKEN="a secretvalue" deploy', "shell"),
        ("TOKEN=$(cat ~/.key) deploy", "shell"),
        ("TOKEN=abc\\", "shell"),
        ("A=1", "shell"),
        # a program name is at most 16 characters and starts with a letter or digit
        ("a" * 16 + " x", "a" * 16),
        ("a" * 17 + " x", "shell"),
        ("ghp_" + "A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q7r8", "shell"),
        ("sk-" + "x" * 40 + " --flag", "shell"),
        ("-rf /", "shell"),
        (".hidden", "shell"),
        ("g++ main.cc", "g++ [build]"),
        ("cargo test -p lattice-core", "cargo [test]"),
        ("cargo build --release", "cargo [build]"),
        ("cargo fmt", "cargo"),
        ("npm test", "npm [test]"),
        ("npm run build", "npm [build]"),
        ("npm run lint", "npm"),
        ("yarn build", "yarn [build]"),
        ("pytest -x tests/", "pytest [test]"),
        ("python3 -m pytest tests", "python3 [test]"),
        ("python3 -m unittest discover", "python3 [test]"),
        ("python3 script.py", "python3"),
        ("go test ./...", "go [test]"),
        ("go vet ./...", "go"),
        ("make", "make [build]"),
        ("make clean", "make [build]"),
        ("gcc -o a a.c", "gcc [build]"),
        ("tsc --noEmit", "tsc [build]"),
        ("$(rm -rf /)", "shell"),
        ("(cd /x && ls)", "shell"),
        ('"quoted" arg', "shell"),
        ("`id`", "shell"),
        ("", ""),
        ("   ", ""),
        ("FOO=bar", "shell"),
        ("\tls\t-l", "ls"),
        (None, ""),
        (7, ""),
    ]

    def test_command_labels(self):
        for given, expected in self.COMMANDS:
            with self.subTest(given=given):
                self.assertEqual(hook.command_label(given), expected)

    def test_a_command_over_the_cap_gives_nothing(self):
        self.assertEqual(hook.command_label("ls " + "x" * 70000), "")

    def test_the_label_never_contains_more_than_the_first_word_and_a_class(self):
        for given, _ in self.COMMANDS:
            label = hook.command_label(given)
            self.assertRegex(label, r"^([A-Za-z0-9_.+-]{1,32}( \[(test|build)\])?)?$")

    def test_clean_keeps_printable_ascii_only(self):
        self.assertEqual(hook.clean("a\x00b\x1bc\x7fd\u202ee\u00e9"), "abcd_e_")
        self.assertEqual(hook.clean("a\x00b\x1bc\x7fd"), "abcd")
        self.assertEqual(hook.clean("e\u00e9"), "e_")
        self.assertEqual(hook.clean("x" * 200), "x" * 64)

    def test_the_file_tools_and_their_keys(self):
        self.assertEqual(hook.tool_label("Edit", {"file_path": "/a/b/c.rs"}), "b/c.rs")
        self.assertEqual(hook.tool_label("NotebookEdit", {"notebook_path": "/a/n.ipynb"}), "a/n.ipynb")
        self.assertEqual(hook.tool_label("Read", {"path": "/a/b/c.rs"}), "")  # not the documented key
        self.assertEqual(hook.tool_label("Edit", "not a dict"), "")
        self.assertEqual(hook.tool_label("Edit", {"file_path": 5}), "")
        self.assertEqual(hook.tool_label("Bash", {"command": 5}), "")


class Hostile(HookCase):
    def test_garbage_input_is_skipped_quietly(self):
        for raw in (b"", b"not json", b"[]", b"[1,2]", b'"x"', b"null", b"7", b"{", b'{"a":1}}',
                    b'{"hook_event_name":"PostToolUse","tool_name":NaN}', b'{"x":Infinity}', b"\xff\xfe\x00",
                    b'{"hook_event_name":"Stop"} trailing', b"\x00" * 100):
            with self.subTest(raw=raw[:30]):
                r = self.run_hook(None, raw=raw)
                self.assertQuiet(r)
        self.assertEqual(self.calls(), [])

    def test_oversize_input_is_refused_without_a_call(self):
        big = json.dumps(post_tool("Bash", {"command": "ls " + "x" * (2 * 1024 * 1024)})).encode()
        r = self.run_hook(None, raw=big)
        self.assertQuiet(r)
        self.assertEqual(self.calls(), [])
        self.assertIn(b"too large", r.stderr)
        self.assertLess(r.elapsed, 5)

    def test_hostile_fields_are_cleaned_or_ignored(self):
        evil = "\x1b]0;owned\x07\x1b[2J" + "A" * 5000
        payloads = [
            post_tool("Edit", {"file_path": "/a/" + evil + "/f.rs"}),
            post_tool("Edit", {"file_path": ["/a/b"]}),
            post_tool("Edit", {"file_path": {"x": 1}}),
            post_tool("Bash", {"command": evil}),
            post_tool("Bash", {"command": ["ls"]}),
            post_tool("Bash", None),
            post_tool("Bash", "ls -l"),
            post_tool(evil, {"command": "ls"}),
            post_tool("Edit", {"file_path": "/a/b/" + "\u202e" * 50 + ".rs"}),
            dict(post_tool("Edit", {"file_path": "/a/b.rs"}), session_id=evil),
            dict(post_tool("Edit", {"file_path": "/a/b.rs"}), session_id={"a": 1}),
            dict(post_tool("Edit", {"file_path": "/a/b.rs"}), tool_response=evil),
        ]
        for p in payloads:
            with self.subTest(p=str(p)[:60]):
                if os.path.exists(self.log):
                    os.unlink(self.log)
                self.assertQuiet(self.run_hook(p))
                for call in self.calls():
                    for a in call:
                        self.assertNotRegex(a, r"[\x00-\x1f\x7f]")
                        self.assertLessEqual(len(a), 70)

    def test_deeply_nested_json_does_not_crash_the_hook(self):
        raw = ("[" * 100000 + "]" * 100000).encode()
        self.assertQuiet(self.run_hook(None, raw=raw))
        raw = ('{"a":' * 5000 + "1" + "}" * 5000).encode()
        self.assertQuiet(self.run_hook(None, raw=raw))

    def test_a_path_traversal_label_has_no_leading_components(self):
        self.run_hook(post_tool("Read", {"file_path": "../../../../etc/shadow"}))
        a = self.calls()[-1]
        self.assertEqual(a[a.index("--target") + 1], "etc/shadow")

    def test_nothing_on_stdout_or_stderr_names_the_payload(self):
        r = self.run_hook(post_tool("Bash", {"command": "echo SECRET-VALUE"}), STUB_RC="1")
        self.assertQuiet(r)
        self.assertNotIn(b"SECRET", r.stderr)


class NeverBlocks(HookCase):
    def test_a_helper_that_fails_is_exit_zero(self):
        for rc in ("1", "2", "3", "127", "255"):
            r = self.run_hook(post_tool("Edit", {"file_path": "/a/b.rs"}), STUB_RC=rc)
            self.assertQuiet(r)

    def test_a_helper_that_hangs_is_stopped_within_the_budget(self):
        t = time.monotonic()
        r = self.run_hook(post_tool("Edit", {"file_path": "/a/b.rs"}), STUB_SLEEP="20")
        self.assertQuiet(r)
        self.assertLess(time.monotonic() - t, 6, "the hook must give up after about 3 seconds")
        self.assertGreater(time.monotonic() - t, 2, "and not before it had its budget")
        self.assertIn(b"did not finish in time", r.stderr)

    def test_the_helper_and_its_children_are_gone_after_a_timeout(self):
        marker = os.path.join(self.tmp, "child.pid")
        slow = os.path.join(self.tmp, "slow-board")
        with open(slow, "w") as f:
            f.write('#!/bin/sh\nsleep 60 &\necho $! >"%s"\nwait\n' % marker)
        os.chmod(slow, 0o755)
        r = self.run_hook(post_tool("Edit", {"file_path": "/a/b.rs"}), BOARD_BIN=slow)
        self.assertQuiet(r)
        with open(marker) as f:
            pid = int(f.read().strip())
        deadline = time.monotonic() + 3
        alive = True
        while time.monotonic() < deadline and alive:
            alive = process_alive(pid)
            if alive:
                time.sleep(0.1)
        self.assertFalse(alive, "the sleep the helper started must be killed with its group")

    def test_a_missing_or_non_executable_helper_is_exit_zero(self):
        r = self.run_hook(post_tool("Edit", {"file_path": "/a/b.rs"}), BOARD_BIN=os.path.join(self.tmp, "nope"))
        self.assertQuiet(r)
        self.assertIn(b"not executable", r.stderr)
        plain = os.path.join(self.tmp, "plain")
        with open(plain, "w") as f:
            f.write("#!/bin/sh\n")
        self.assertQuiet(self.run_hook(post_tool("Edit", {"file_path": "/a/b.rs"}), BOARD_BIN=plain))

    def test_a_stdin_that_never_ends_does_not_hang_the_hook(self):
        p = subprocess.Popen([sys.executable, HOOK], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                             env=self.env())
        self.addCleanup(p.kill)
        self.addCleanup(p.stdin.close)
        p.stdin.write(b'{"hook_event_name":"Stop"')  # no closing brace, and the pipe stays open
        p.stdin.flush()
        t = time.monotonic()
        try:
            rc = p.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.fail("the hook is still waiting for its input after 10 seconds")
        elapsed = time.monotonic() - t
        self.assertEqual(rc, 0)
        self.assertEqual(p.stdout.read(), b"")
        self.assertGreater(elapsed, 2.0, "it waits for its budget of about 3 seconds")
        self.assertLess(elapsed, 6)

    def test_an_exception_inside_the_hook_is_exit_zero(self):
        # SIGINT while it waits for its input raises KeyboardInterrupt, a BaseException: the hook must still exit 0.
        p = subprocess.Popen([sys.executable, HOOK], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                             env=self.env())
        self.addCleanup(p.kill)
        self.addCleanup(p.stdin.close)
        time.sleep(0.7)
        p.send_signal(signal.SIGINT)
        try:
            rc = p.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.fail("the hook did not stop")
        self.assertEqual(rc, 0, "an error inside the hook must never fail the agent")
        self.assertEqual(p.stdout.read(), b"")
        self.assertIn(b"stopped by KeyboardInterrupt", p.stderr.read())

    def test_it_is_fast_when_nothing_is_to_be_done(self):
        t = time.monotonic()
        for _ in range(5):
            self.run_hook(post_tool("Edit", {"file_path": "/a/b.rs"}), BOARD_ACTIVITY=None)
        self.assertLess((time.monotonic() - t) / 5, 1.0)


class TaskLink(HookCase):
    def args(self):
        return self.calls()[-1]

    def fence(self, name, mtime=None, content="123456"):
        p = os.path.join(self.state, name)
        with open(p, "w") as f:
            f.write(content)
        if mtime is not None:
            os.utime(p, (mtime, mtime))
        return p

    def test_the_task_the_agent_holds_is_linked_from_the_name_of_its_fence_file(self):
        self.fence("alice.tabc123")
        self.run_hook(post_tool("Edit", {"file_path": "/a/b.rs"}))
        a = self.args()
        self.assertEqual(a[a.index("--task") + 1], "tabc123")

    def test_the_fence_is_never_read(self):
        p = self.fence("alice.tabc123", content="SECRETFENCE")
        os.chmod(p, 0)  # unreadable: the name is enough
        self.addCleanup(os.chmod, p, 0o600)
        self.run_hook(post_tool("Edit", {"file_path": "/a/b.rs"}))
        self.assertIn("tabc123", self.args())
        self.assertNotIn("SECRETFENCE", json.dumps(self.calls()))

    def test_other_authors_and_odd_names_are_ignored(self):
        self.fence("bob.tother")
        self.fence("alice.b.tnested")  # belongs to the author "alice.b"
        self.fence("alice.bad name")
        self.fence("alice.")
        self.fence("alice")
        self.fence("alice.%s" % ("t" * 65))
        os.symlink(os.path.join(self.state, "bob.tother"), os.path.join(self.state, "alice.tlink"))
        os.mkdir(os.path.join(self.state, "alice.tdir"))
        self.run_hook(post_tool("Edit", {"file_path": "/a/b.rs"}))
        self.assertNotIn("--task", self.args())

    def test_the_newest_fence_wins_and_a_leftover_week_old_file_does_not(self):
        now = time.time()
        self.fence("alice.told", now - 100)
        self.fence("alice.tnew", now - 5)
        self.fence("alice.tancient", now - 8 * 86400)
        self.run_hook(post_tool("Edit", {"file_path": "/a/b.rs"}))
        a = self.args()
        self.assertEqual(a[a.index("--task") + 1], "tnew")
        os.unlink(self.log)
        for n in ("alice.told", "alice.tnew"):
            os.unlink(os.path.join(self.state, n))
        self.run_hook(post_tool("Edit", {"file_path": "/a/b.rs"}))
        self.assertNotIn("--task", self.args())

    def test_board_mcp_leases_are_read_for_the_task_field_only(self):
        lease = {"leases": {"L1": {"task": "tmcp77", "fence": 99999, "pid": 1, "lease": 600, "taken": 1}}}
        p = os.path.join(self.state, "mcp-leases.alice.json")
        with open(p, "w") as f:
            json.dump(lease, f)
        self.run_hook(post_tool("Edit", {"file_path": "/a/b.rs"}))
        a = self.args()
        self.assertEqual(a[a.index("--task") + 1], "tmcp77")
        self.assertNotIn("99999", json.dumps(self.calls()))

    def test_a_damaged_or_hostile_lease_file_is_ignored(self):
        p = os.path.join(self.state, "mcp-leases.alice.json")
        for content in ("{", "[]", '{"leases": []}', '{"leases": {"L": {"task": "a b"}}}', '{"leases": {"L": 5}}',
                        '{"leases": {"L": {"task": ["x"]}}}', "\xff"):
            with open(p, "wb") as f:
                f.write(content.encode("latin-1"))
            if os.path.exists(self.log):
                os.unlink(self.log)
            self.assertQuiet(self.run_hook(post_tool("Edit", {"file_path": "/a/b.rs"})))
            self.assertNotIn("--task", self.args())

    def test_a_state_directory_that_is_a_symlink_or_missing_gives_no_task(self):
        link = os.path.join(self.tmp, "link")
        os.symlink(self.state, link)
        self.fence("alice.tabc123")
        self.run_hook(post_tool("Edit", {"file_path": "/a/b.rs"}), BOARD_STATE_DIR=link)
        self.assertNotIn("--task", self.args())
        os.unlink(self.log)
        self.run_hook(post_tool("Edit", {"file_path": "/a/b.rs"}), BOARD_STATE_DIR=os.path.join(self.tmp, "missing"))
        self.assertNotIn("--task", self.args())


class KilledWrite(unittest.TestCase):
    """The hook ends a helper that talks to a host that does not answer. SIGKILL would skip the helper's EXIT trap and
    leave two empty board.XXXXXX files in TMPDIR every time (once per backoff period, for as long as the database is away)."""

    def test_a_killed_write_leaves_no_temporary_files(self):
        tmp = tempfile.mkdtemp(prefix="act-hook-kill-")
        self.addCleanup(shutil.rmtree, tmp, True)
        os.chmod(tmp, 0o700)
        hole = os.path.join(tmp, "psql-hole")
        with open(hole, "w") as f:
            f.write("#!/bin/sh\nexec sleep 30\n")
        os.chmod(hole, 0o755)
        env = {"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "HOME": tmp, "TMPDIR": tmp, "BOARD_BIN": actbase.BOARD,
               "BOARD_PSQL": hole, "BOARD_AUTHOR": "alice", "BOARD_ACTIVITY": "1", "BOARD_STATE_DIR": os.path.join(tmp, "state")}
        t = time.monotonic()
        r = subprocess.run([sys.executable, HOOK], input=json.dumps(post_tool("Edit", {"file_path": "/a/b.rs"})).encode(),
                           env=env, capture_output=True, timeout=30)
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertIn(b"did not finish in time", r.stderr, "the write really was cut off")
        self.assertLess(time.monotonic() - t, 6)
        left = [n for n in os.listdir(tmp) if n.startswith("board.")]
        self.assertEqual(left, [], "the helper's temporary files must be removed when it is stopped")


class WithTheRealHelper(unittest.TestCase):
    """The hook, the real helper and a fake psql together: a burst of tool calls is a handful of writes."""

    def setUp(self):
        self.tmp = tempfile.mkdtemp(prefix="act-hook-real-")
        self.addCleanup(shutil.rmtree, self.tmp, True)
        os.chmod(self.tmp, 0o700)
        self.log = os.path.join(self.tmp, "calls.jsonl")
        self.rules = os.path.join(self.tmp, "rules.json")
        with open(self.rules, "w") as f:
            json.dump([{"match": WRITE, "out": "row\talice/sess-1"}], f)
        self.psql = os.path.join(self.tmp, "psql")
        with open(self.psql, "w") as f:
            f.write('#!/bin/sh\nexec "%s" "%s" "$@"\n' % (sys.executable, actbase.FAKE))
        os.chmod(self.psql, 0o755)

    def env(self, now):
        return {"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "HOME": self.tmp, "TMPDIR": self.tmp,
                "BOARD_BIN": actbase.BOARD, "BOARD_PSQL": self.psql, "BOARD_AUTHOR": "alice", "BOARD_ACTIVITY": "1",
                "BOARD_STATE_DIR": os.path.join(self.tmp, "state"), "FAKE_LOG": self.log, "FAKE_RULES": self.rules,
                "BOARD_TEST_MODE": "1", "BOARD_ACTIVITY_NOW": str(now)}

    def fire(self, payload, now):
        r = subprocess.run([sys.executable, HOOK], input=json.dumps(payload).encode(), env=self.env(now),
                           capture_output=True, timeout=30)
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(r.stdout, b"")

    def writes(self):
        if not os.path.exists(self.log):
            return []
        with open(self.log) as f:
            return [c for c in (json.loads(l) for l in f) if "INSERT INTO agent_activity_log" in c["sql"]]

    def test_a_burst_of_tool_calls_is_a_handful_of_writes(self):
        # 60 calls inside one 5 second interval, then 60 in the next, then the turn ends.
        for i in range(60):
            self.fire(post_tool("Edit" if i % 2 else "Bash", {"file_path": "/p/src/f%d.rs" % i, "command": "ls"}), T0)
        self.assertEqual(len(self.writes()), 1)
        for i in range(60):
            self.fire(post_tool("Read", {"file_path": "/p/src/g%d.rs" % i}), T0 + 5)
        self.assertEqual(len(self.writes()), 2)
        self.fire({"session_id": "sess-1", "hook_event_name": "Stop"}, T0 + 6)
        ws = self.writes()
        self.assertEqual(len(ws), 3)
        calls = [vars_of(w)["calls"] for w in ws]
        # the first call wrote alone, the 59 held back went with the next interval's first call, and the 59 held in
        # that interval went with the end of the turn (the stop mark itself is not a call)
        self.assertEqual(calls, ["1", "60", "59"])
        self.assertEqual(sum(int(c) for c in calls), 120, "every one of the 120 calls is counted exactly once")
        self.assertEqual(vars_of(ws[-1])["state"], "idle")


if __name__ == "__main__":
    unittest.main()

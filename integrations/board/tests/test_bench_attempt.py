"""One attempt of board bench, run for real with small fake agents (no model, no network, no database).

The attempt is what stands between an agent and a number, so these tests try to cheat it: an agent that
rewrites the tests, deletes them, replaces their directory by a link, adds a test file; a fixture that was
edited; a suite that changed under a running experiment; a grader that disagrees with itself, hangs, or
cannot run. Each case checks the outcome that gets recorded and, where it matters, that nothing leaked.

Run: python3 -m unittest discover -s integrations/board/tests
"""

import json
import os
import shutil
import signal
import stat
import subprocess
import sys
import tempfile
import textwrap
import time
import unittest

import benchbase
from benchbase import BENCH, BOARD_DIR
from benchlib import attempt, suite

PY = sys.executable
GOOD_AGENT = "import os\nopen(os.path.join(os.environ['BENCH_WORKDIR'], 'answer.txt'), 'w').write('42')\n"
NOOP_AGENT = "pass\n"
CHECK = "import os, sys\nsys.exit(0 if open('answer.txt').read() == '42' else 1)\n"
# The tests of the fixture: answer.txt must hold 42. tests/check.py is the grader's script; protecting
# tests/** means an agent cannot make it pass by editing it.
CHEAT_AGENT = textwrap.dedent("""\
    import os
    w = os.environ['BENCH_WORKDIR']
    open(os.path.join(w, 'tests', 'check.py'), 'w').write('import sys\\nsys.exit(0)\\n')
    """)


def file_digest(path):
    with open(path, "rb") as f:
        return __import__("hashlib").sha256(f.read()).hexdigest()


def pid_alive(pid):
    """True while the process runs. A zombie (killed, not yet reaped: in a container whose init does not reap) is dead."""
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    try:
        with open("/proc/%d/stat" % pid) as f:
            return f.read().rsplit(")", 1)[1].split()[0] != "Z"
    except OSError:
        out = subprocess.run(["ps", "-o", "stat=", "-p", str(pid)], capture_output=True, text=True).stdout.strip()
        return bool(out) and not out.startswith("Z")


def slurp(path):
    with open(path) as f:
        return f.read()


class Lab:
    """A temporary suite directory with one fixture and helpers to build and run attempts."""

    def __init__(self, tc, fixture_files=None, grader=None, agent_src=GOOD_AGENT, protected=("tests/**",),
                 reruns=0, timeout_s=20, grader_timeout=20, arm_extra=None, agent="cmd", args=None, env_allow=()):
        self.tc = tc
        self.tmp = tempfile.mkdtemp(prefix="bench-lab-")
        os.chmod(self.tmp, 0o700)
        tc.addCleanup(shutil.rmtree, self.tmp, True)
        self.dir = os.path.join(self.tmp, "suite")
        os.makedirs(os.path.join(self.dir, "fx", "tests"))
        files = fixture_files if fixture_files is not None else {"tests/check.py": CHECK, "README": "fixture\n"}
        for rel, text in files.items():
            p = os.path.join(self.dir, "fx", rel)
            os.makedirs(os.path.dirname(p), exist_ok=True)
            with open(p, "w") as f:
                f.write(text)
        self.agent_path = os.path.join(self.dir, "agent.py")
        with open(self.agent_path, "w") as f:
            f.write(agent_src)
        self.work_root = os.path.join(self.tmp, "work")
        os.mkdir(self.work_root, 0o700)
        a = {"id": "arm1", "agent": agent, "env_allow": list(env_allow),
             "args": args if args is not None else {"argv": [PY, "{suite_dir}/agent.py"]}}
        a.update(arm_extra or {})
        self.doc = {"suite": "lab", "version": 1, "repeats": 1,
                    "arms": [a],
                    "tasks": [{"id": "task1", "prompt": "make answer.txt hold 42", "timeout_s": timeout_s,
                               "fixture": {"dir": "fx", "digest": "sha256:" + suite.tree_digest(os.path.join(self.dir, "fx"))},
                               "grader": {"cmd": grader or [PY, "tests/check.py"], "timeout_s": grader_timeout,
                                          "protected": list(protected), "reruns": reruns}}]}
        self.write_suite()

    def write_suite(self):
        self.suite_path = os.path.join(self.dir, "suite.json")
        with open(self.suite_path, "w") as f:
            json.dump(self.doc, f)
        _, self.sha, _ = suite.load_suite(self.suite_path)

    def body(self):
        return json.loads(attempt.make_body("exp", "arm1", "task1", 0, 0, self.suite_path, self.sha))

    def run(self, keep=False, env=None):
        old = dict(os.environ)
        if env:
            os.environ.update(env)
        try:
            return attempt.run_attempt(self.body(), self.work_root, self.dir, keep)
        finally:
            os.environ.clear()
            os.environ.update(old)


class OutcomeTests(unittest.TestCase):
    def test_an_agent_that_does_the_work_passes(self):
        r = Lab(self).run()
        self.assertEqual((r["outcome"], r["flaky"], r["tampered"], r["grader_runs"], r["grader_exit"]), ("pass", False, 0, "p", 0))
        self.assertIsInstance(r["duration_ms"], int)

    def test_an_agent_that_does_nothing_fails(self):
        r = Lab(self, agent_src=NOOP_AGENT).run()
        self.assertEqual((r["outcome"], r["grader_runs"], r["grader_exit"]), ("fail", "f", 1))

    def test_the_agent_cannot_pass_by_editing_the_grader_script(self):
        lab = Lab(self, agent_src=CHEAT_AGENT)
        r = lab.run()
        self.assertEqual(r["outcome"], "fail")
        self.assertEqual(r["tampered"], 1)
        self.assertIn("restored 1 protected path", r["summary"])

    def test_without_protected_paths_the_same_cheat_passes(self):
        # the control: the protection is what makes the difference
        r = Lab(self, agent_src=CHEAT_AGENT, protected=()).run()
        self.assertEqual((r["outcome"], r["tampered"]), ("pass", 0))

    def test_a_deleted_protected_directory_is_restored(self):
        src = "import os, shutil\nshutil.rmtree(os.path.join(os.environ['BENCH_WORKDIR'], 'tests'))\n" + GOOD_AGENT
        r = Lab(self, agent_src=src).run()
        self.assertEqual((r["outcome"], r["tampered"]), ("pass", 1))  # restored tests, honest answer: pass

    def test_a_protected_directory_replaced_by_a_link_is_restored_and_the_target_untouched(self):
        outside = tempfile.mkdtemp(prefix="bench-outside-")
        self.addCleanup(shutil.rmtree, outside, True)
        with open(os.path.join(outside, "check.py"), "w") as f:
            f.write("import sys\nsys.exit(0)\n")
        src = textwrap.dedent("""\
            import os, shutil
            w = os.environ['BENCH_WORKDIR']
            shutil.rmtree(os.path.join(w, 'tests'))
            os.symlink(%r, os.path.join(w, 'tests'))
            """ % outside)
        r = Lab(self, agent_src=src).run()
        self.assertEqual((r["outcome"], r["tampered"]), ("fail", 1))
        with open(os.path.join(outside, "check.py")) as f:
            self.assertEqual(f.read(), "import sys\nsys.exit(0)\n")  # not followed, not written

    def test_a_file_added_under_a_protected_path_is_removed(self):
        src = "import os\nopen(os.path.join(os.environ['BENCH_WORKDIR'], 'tests', 'extra.py'), 'w').write('x')\n" + GOOD_AGENT
        lab = Lab(self, agent_src=src, grader=[PY, "-c", "import os, sys; sys.exit(1 if os.path.exists('tests/extra.py') else 0)"])
        r = lab.run()
        self.assertEqual((r["outcome"], r["tampered"]), ("pass", 1))

    def test_a_changed_executable_bit_on_a_protected_file_counts(self):
        src = "import os\nos.chmod(os.path.join(os.environ['BENCH_WORKDIR'], 'tests', 'check.py'), 0o755)\n" + GOOD_AGENT
        self.assertEqual(Lab(self, agent_src=src).run()["tampered"], 1)

    def test_files_outside_the_protected_paths_are_the_agents_to_change(self):
        src = "import os\nopen(os.path.join(os.environ['BENCH_WORKDIR'], 'README'), 'w').write('changed')\n" + GOOD_AGENT
        r = Lab(self, agent_src=src).run()
        self.assertEqual((r["outcome"], r["tampered"]), ("pass", 0))


class HarnessErrorTests(unittest.TestCase):
    def test_a_fixture_that_does_not_match_its_digest_is_an_error_and_the_agent_never_runs(self):
        marker = os.path.join(tempfile.gettempdir(), "bench-marker-%d" % os.getpid())
        self.addCleanup(lambda: os.path.exists(marker) and os.unlink(marker))
        lab = Lab(self, agent_src="open(%r, 'w').write('ran')\n" % marker)
        with open(os.path.join(lab.dir, "fx", "README"), "a") as f:
            f.write("edited after the digest was taken\n")
        r = lab.run()
        self.assertEqual(r["outcome"], "error")
        self.assertIn("does not match its digest", r["summary"])
        self.assertFalse(os.path.exists(marker))

    def test_a_suite_edited_after_the_experiment_started_is_an_error(self):
        lab = Lab(self)
        body = lab.body()
        lab.doc["version"] = 2
        lab.write_suite()
        r = attempt.run_attempt(body, lab.work_root, lab.dir)
        self.assertEqual(r["outcome"], "error")
        self.assertIn("suite file changed", r["summary"])

    def test_a_fixture_with_a_symbolic_link_is_an_error(self):
        lab = Lab(self)
        os.symlink("/etc/hosts", os.path.join(lab.dir, "fx", "link"))
        r = lab.run()
        self.assertEqual(r["outcome"], "error")
        self.assertIn("not a regular file", r["summary"])

    def test_a_grader_that_exits_125_is_an_error(self):
        r = Lab(self, grader=[PY, "-c", "import sys; sys.exit(125)"]).run()
        self.assertEqual((r["outcome"], r["grader_runs"], r["grader_exit"]), ("error", "e", 125))
        self.assertIn("could not run", r["summary"])

    def test_126_and_127_are_failures_not_errors(self):
        # a shell returns these for a script an agent made non-executable or deleted; counting them as harness
        # errors would drop exactly the attempts where the agent broke the grader (selection bias for cheating)
        for code in (126, 127):
            r = Lab(self, grader=[PY, "-c", "import sys; sys.exit(%d)" % code]).run()
            self.assertEqual((r["outcome"], r["grader_runs"], r["grader_exit"]), ("fail", "f", code), code)

    def test_a_grader_that_does_not_exist_is_an_error_not_a_failure(self):
        r = Lab(self, grader=["/nonexistent/grader-program"]).run()
        self.assertEqual(r["outcome"], "error")
        self.assertIn("did not start", r["summary"])

    def test_a_grader_program_inside_the_work_directory_that_the_agent_deleted_is_a_failure(self):
        lab = Lab(self, fixture_files={"tests/check.py": CHECK, "run-grader": "#!/bin/sh\nexit 0\n"},
                  grader=["./run-grader"], protected=(),
                  agent_src="import os\nos.unlink(os.path.join(os.environ['BENCH_WORKDIR'], 'run-grader'))\n")
        r = lab.run()
        self.assertEqual((r["outcome"], r["grader_runs"]), ("fail", "f"))
        self.assertIn("did not start", r["summary"])

    def test_an_agent_that_cannot_start_is_an_error(self):
        r = Lab(self, args={"argv": ["/nonexistent/agent-program"]}).run()
        self.assertEqual(r["outcome"], "error")
        self.assertIn("cannot start the agent", r["summary"])

    def test_an_error_in_one_rerun_makes_the_attempt_an_error(self):
        grader = [PY, "-c", "import os, sys\nn = os.path.exists('.n')\nopen('.n', 'w').write('1')\nsys.exit(125 if n else 0)"]
        r = Lab(self, grader=grader, reruns=1).run()
        self.assertEqual((r["outcome"], r["grader_runs"]), ("error", "pe"))


class GraderTests(unittest.TestCase):
    ALTERNATING = [PY, "-c", textwrap.dedent("""\
        import os, sys
        n = int(open('.n').read()) + 1 if os.path.exists('.n') else 1
        open('.n', 'w').write(str(n))
        sys.exit(0 if n % 2 == 1 else 1)
        """)]

    def test_runs_that_disagree_are_flaky_and_decided_by_strict_majority(self):
        r = Lab(self, grader=self.ALTERNATING, reruns=2).run()  # p f p
        self.assertEqual((r["outcome"], r["flaky"], r["grader_runs"]), ("pass", True, "pfp"))

    def test_two_runs_that_split_do_not_pass(self):
        r = Lab(self, grader=self.ALTERNATING, reruns=1).run()  # p f: one of two is not more than half
        self.assertEqual((r["outcome"], r["flaky"], r["grader_runs"]), ("fail", True, "pf"))

    def test_a_flaky_failure_is_flagged_too(self):
        grader = [PY, "-c", "import os, sys\nn = os.path.exists('.n')\nopen('.n', 'w').write('1')\nsys.exit(1 if n else 0)"]
        r = Lab(self, grader=grader, reruns=2).run()  # p f f
        self.assertEqual((r["outcome"], r["flaky"], r["grader_runs"]), ("fail", True, "pff"))

    def test_agreeing_reruns_are_not_flaky(self):
        r = Lab(self, reruns=2).run()
        self.assertEqual((r["outcome"], r["flaky"], r["grader_runs"]), ("pass", False, "ppp"))

    def test_a_grader_that_hangs_times_out_and_is_killed(self):
        pidfile = os.path.join(tempfile.gettempdir(), "bench-grader-pid-%d" % os.getpid())
        self.addCleanup(lambda: os.path.exists(pidfile) and os.unlink(pidfile))
        code = "import os, time\nopen(%r, 'w').write(str(os.getpid()))\ntime.sleep(60)" % pidfile
        t0 = time.monotonic()
        r = Lab(self, grader=[PY, "-c", code], grader_timeout=1).run()
        self.assertEqual((r["outcome"], r["grader_runs"]), ("timeout", "t"))
        self.assertLess(time.monotonic() - t0, 30)
        pid = int(slurp(pidfile))
        time.sleep(0.3)
        self.assertFalse(pid_alive(pid))

    def test_the_grader_runs_in_the_work_copy_not_in_the_fixture(self):
        lab = Lab(self, grader=[PY, "-c", "open('graded-here', 'w').write('x')"])
        lab.run()
        self.assertFalse(os.path.exists(os.path.join(lab.dir, "fx", "graded-here")))

    def test_suite_dir_and_work_placeholders_reach_the_grader(self):
        # The suite is reached through a symbolic link, on every OS (a temp directory is only sometimes a link): the
        # placeholder must be the resolved, checked directory, not the link the task body named.
        lab = Lab(self)
        with open(os.path.join(lab.dir, "grade.py"), "w") as f:
            f.write("import sys\nopen(sys.argv[2] + '/seen', 'w').write(sys.argv[1])\nsys.exit(0)\n")
        lab.doc["tasks"][0]["grader"]["cmd"] = [PY, "{suite_dir}/grade.py", "{suite_dir}", "{work}"]
        lab.doc["tasks"][0]["grader"]["files"] = {"grade.py": "sha256:" + file_digest(os.path.join(lab.dir, "grade.py"))}
        lab.write_suite()
        link = os.path.join(lab.tmp, "link-to-suite")
        os.symlink(lab.dir, link)
        body = json.loads(attempt.make_body("exp", "arm1", "task1", 0, 0, os.path.join(link, "suite.json"), lab.sha))
        r = attempt.run_attempt(body, lab.work_root, lab.dir, True)
        self.assertEqual(r["outcome"], "pass")
        work = [os.path.join(lab.work_root, d, "work") for d in os.listdir(lab.work_root) if d.startswith("bench-") and not d.startswith("bench-p-")][0]
        self.assertEqual(slurp(os.path.join(work, "seen")), os.path.realpath(lab.dir))
        self.assertNotEqual(os.path.realpath(lab.dir), link)


class AgentTests(unittest.TestCase):
    def test_an_agent_that_runs_too_long_times_out_and_its_whole_process_group_dies(self):
        pidfile = os.path.join(tempfile.gettempdir(), "bench-agent-pid-%d" % os.getpid())
        self.addCleanup(lambda: os.path.exists(pidfile) and os.unlink(pidfile))
        src = textwrap.dedent("""\
            import os, subprocess, sys, time
            child = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(60)'])
            open(%r, 'w').write(str(child.pid))
            time.sleep(60)
            """ % pidfile)
        t0 = time.monotonic()
        r = Lab(self, agent_src=src, timeout_s=1).run()
        self.assertEqual(r["outcome"], "timeout")
        self.assertEqual(r["grader_runs"], "")  # not graded: it never finished
        self.assertLess(time.monotonic() - t0, 30)
        pid = int(slurp(pidfile))
        time.sleep(0.3)
        self.assertFalse(pid_alive(pid))

    def test_a_crashing_agent_is_graded_on_the_files_it_left(self):
        src = GOOD_AGENT + "import sys\nsys.exit(3)\n"
        r = Lab(self, agent_src=src).run()
        self.assertEqual(r["outcome"], "pass")
        self.assertIn("agent exit 3", r["summary"])

    def test_each_attempt_starts_from_a_fresh_copy(self):
        lab = Lab(self, agent_src="import os\nw = os.environ['BENCH_WORKDIR']\n"
                                  "assert not os.path.exists(w + '/leftover'), 'state leaked'\n"
                                  "open(w + '/leftover', 'w').write('x')\n" + GOOD_AGENT)
        self.assertEqual(lab.run()["outcome"], "pass")
        self.assertEqual(lab.run()["outcome"], "pass")  # the second would fail on a leftover from the first
        self.assertFalse(os.path.exists(os.path.join(lab.dir, "fx", "leftover")))

    def test_the_attempt_directory_is_removed_unless_kept(self):
        lab = Lab(self)
        lab.run()
        self.assertEqual(os.listdir(lab.work_root), [])
        lab.run(keep=True)
        self.assertEqual(len(os.listdir(lab.work_root)), 1)

    def test_the_agent_sees_only_an_allow_listed_environment_and_an_empty_home(self):
        src = textwrap.dedent("""\
            import json, os
            w = os.environ['BENCH_WORKDIR']
            json.dump({'env': dict(os.environ), 'home_files': os.listdir(os.environ['HOME'])}, open(w + '/env.json', 'w'))
            """)
        lab = Lab(self, agent_src=src, env_allow=("ANTHROPIC_API_KEY", "NOT_SET_ANYWHERE"))
        secrets = {"PGPASSWORD": "pg-secret-1", "BOARD_AUTHOR": "me", "LATTICE_BOARD_URL": "postgres://x", "TASK_FENCE": "9",
                   "AWS_SECRET_ACCESS_KEY": "bbbbbbbb-other", "ANTHROPIC_API_KEY": "aaaaaaaa-allowed", "TASK_RESULT_PATH": "/x"}
        lab.run(keep=True, env=secrets)
        work = os.path.join(lab.work_root, os.listdir(lab.work_root)[0], "work")
        seen = json.loads(slurp(os.path.join(work, "env.json")))
        for name in ("PGPASSWORD", "BOARD_AUTHOR", "LATTICE_BOARD_URL", "TASK_FENCE", "AWS_SECRET_ACCESS_KEY", "TASK_RESULT_PATH"):
            self.assertNotIn(name, seen["env"], name)
        self.assertEqual(seen["env"]["ANTHROPIC_API_KEY"], "aaaaaaaa-allowed")
        self.assertNotIn("NOT_SET_ANYWHERE", seen["env"])
        self.assertTrue(seen["env"]["HOME"].startswith(lab.work_root))
        self.assertEqual([n for n in seen["home_files"] if n != ".cache"], [])  # some interpreters make ~/.cache themselves
        self.assertTrue(set(seen["env"]) <= {"PATH", "LANG", "TERM", "HOME", "ANTHROPIC_API_KEY", "BENCH_PROMPT_FILE",
                                             "BENCH_WORKDIR", "BENCH_REPORT_FILE", "LC_CTYPE", "__CF_USER_TEXT_ENCODING", "PWD"})

    def test_a_secret_in_the_output_never_reaches_the_summary(self):
        src = "import os\nprint('key is ' + os.environ['ANTHROPIC_API_KEY'])\n"
        lab = Lab(self, agent_src=src, grader=[PY, "-c", "import os, sys; print('grader saw ' + os.environ.get('LEAK', 'nothing')); sys.exit(1)"],
                  env_allow=("ANTHROPIC_API_KEY",))
        r = lab.run(env={"ANTHROPIC_API_KEY": "cccccccc-hidden"})
        self.assertNotIn("cccccccc-hidden", json.dumps(r))

    def test_cost_and_tokens_come_only_from_what_a_cmd_agent_reports(self):
        src = "import json, os\njson.dump({'cost_usd': 0.0123, 'tokens': 4567}, open(os.environ['BENCH_REPORT_FILE'], 'w'))\n" + GOOD_AGENT
        r = Lab(self, agent_src=src).run()
        self.assertEqual((r["cost_usd"], r["tokens"]), (0.0123, 4567))
        r = Lab(self).run()
        self.assertEqual((r["cost_usd"], r["tokens"]), (None, None))  # not reported: unknown stays NULL

    def test_nonsense_in_the_report_file_is_ignored(self):
        for text in ("not json", '{"cost_usd": -1, "tokens": "many"}', '{"cost_usd": NaN}', "[1]", '{"cost_usd": true}'):
            src = "import os\nopen(os.environ['BENCH_REPORT_FILE'], 'w').write(%r)\n" % text + GOOD_AGENT
            r = Lab(self, agent_src=src).run()
            self.assertEqual((r["cost_usd"], r["tokens"]), (None, None), text)

    def write_fake_cli(self, lab, body):
        path = os.path.join(lab.dir, "fakecli")
        with open(path, "w") as f:
            f.write("#!%s\n%s" % (PY, body))
        os.chmod(path, 0o755)
        return path

    def test_the_claude_template_uses_safe_flags_reads_the_prompt_from_stdin_and_takes_the_cost(self):
        lab = Lab(self, agent="claude", args={})
        cli = self.write_fake_cli(lab, textwrap.dedent("""\
            import json, os, sys
            w = os.environ['BENCH_WORKDIR']
            json.dump({'argv': sys.argv[1:], 'stdin': sys.stdin.read(), 'cwd': os.getcwd()}, open(w + '/seen.json', 'w'))
            open(w + '/answer.txt', 'w').write('42')
            print(json.dumps({'type': 'result', 'result': 'done', 'total_cost_usd': 0.0421, 'is_error': False}))
            """))
        lab.doc["arms"][0]["args"] = {"bin": cli, "model": "some-model", "budget_usd": 1.5}
        lab.write_suite()
        r = lab.run(keep=True)
        self.assertEqual((r["outcome"], r["cost_usd"], r["tokens"]), ("pass", 0.0421, None))
        work = os.path.join(lab.work_root, os.listdir(lab.work_root)[0], "work")
        seen = json.loads(slurp(os.path.join(work, "seen.json")))
        argv = seen["argv"]
        self.assertEqual(argv[:3], ["-p", "--output-format", "json"])
        self.assertEqual(argv[argv.index("--permission-mode") + 1], "dontAsk")
        self.assertEqual(argv[argv.index("--permission-prompts") + 1], "none")
        self.assertEqual(argv[argv.index("--max-budget-usd") + 1], "1.5")
        self.assertEqual(argv[argv.index("--model") + 1], "some-model")
        self.assertIn("--no-session-persistence", argv)
        self.assertNotIn("make answer.txt hold 42", " ".join(argv))  # the prompt is never an argument
        self.assertEqual(seen["stdin"], "make answer.txt hold 42")
        self.assertEqual(os.path.realpath(seen["cwd"]), os.path.realpath(work))

    def test_the_codex_template(self):
        lab = Lab(self, agent="codex", args={})
        cli = self.write_fake_cli(lab, textwrap.dedent("""\
            import json, os, sys
            w = os.environ['BENCH_WORKDIR']
            json.dump({'argv': sys.argv[1:], 'stdin': sys.stdin.read()}, open(w + '/seen.json', 'w'))
            open(w + '/answer.txt', 'w').write('42')
            """))
        lab.doc["arms"][0]["args"] = {"bin": cli, "sandbox": "read-only", "model": "m"}
        lab.write_suite()
        r = lab.run(keep=True)
        self.assertEqual((r["outcome"], r["cost_usd"]), ("pass", None))  # codex reports no cost field
        work = os.path.join(lab.work_root, os.listdir(lab.work_root)[0], "work")
        seen = json.loads(slurp(os.path.join(work, "seen.json")))
        argv = seen["argv"]
        self.assertEqual(argv[0], "exec")
        self.assertEqual(argv[argv.index("-s") + 1], "read-only")
        self.assertEqual(os.path.realpath(argv[argv.index("-C") + 1]), os.path.realpath(work))
        self.assertIn("--ephemeral", argv)
        self.assertEqual(argv[-1], "-")
        self.assertEqual(seen["stdin"], "make answer.txt hold 42")

    def test_claude_cost_is_not_taken_from_a_truncated_log(self):
        self.assertIsNone(attempt.claude_cost("/nonexistent", 5))
        self.assertIsNone(attempt.claude_cost("/nonexistent", 0))


class IntegrityTests(unittest.TestCase):
    """What an agent running as the same user can still try, and what the attempt does about it."""

    def find_pristine(self):
        return ("import glob, os\nw = os.environ['BENCH_WORKDIR']\n"
                "root = os.path.dirname(os.path.dirname(w))\n"
                "pr = glob.glob(os.path.join(root, 'bench-p-*', 'pristine'))\n")

    def test_an_agent_that_edits_the_pristine_copy_and_the_work_copy_still_fails_and_is_marked_tampered(self):
        src = self.find_pristine() + textwrap.dedent("""\
            import stat
            for base in pr + [w]:
                for d, _, names in os.walk(base):
                    os.chmod(d, 0o755)
                for n in ('tests/check.py',):
                    p = os.path.join(base, n)
                    os.chmod(p, 0o644)
                    open(p, 'w').write('import sys\\nsys.exit(0)\\n')
            """)
        r = Lab(self, agent_src=src).run()
        self.assertEqual((r["outcome"], r["tampered"]), ("fail", 1))
        self.assertIn("pristine copy", r["summary"])

    def test_the_pristine_copy_is_not_under_the_directory_the_agent_is_told_about(self):
        src = ("import json, os\nopen(os.path.join(os.environ['BENCH_WORKDIR'], 'where.json'), 'w').write(json.dumps("
               "[os.environ['BENCH_PROMPT_FILE'], os.environ['BENCH_REPORT_FILE']]))\n")
        lab = Lab(self, agent_src=src)
        lab.run(keep=True)
        tdir = [d for d in os.listdir(lab.work_root) if d.startswith("bench-") and not d.startswith("bench-p-")][0]
        told = json.loads(slurp(os.path.join(lab.work_root, tdir, "work", "where.json")))
        for path in told:
            self.assertTrue(path.startswith(os.path.join(lab.work_root, tdir)))
        self.assertFalse([d for d in os.listdir(lab.work_root) if d.startswith("bench-p-")])  # removed, and apart from tdir

    def test_the_grader_gets_a_home_of_its_own_so_what_the_agent_left_in_its_home_is_not_read(self):
        src = "import os\nopen(os.path.join(os.environ['HOME'], 'usercustomize.py'), 'w').write('boom')\n" + GOOD_AGENT
        grader = [PY, "-c", "import os, sys\nsys.exit(1 if [n for n in os.listdir(os.environ['HOME']) if n != '.cache'] else 0)"]
        r = Lab(self, agent_src=src, grader=grader).run()
        self.assertEqual(r["outcome"], "pass")

    def test_the_grader_runs_without_the_user_site_directory(self):
        grader = [PY, "-c", "import os, sys\nsys.exit(0 if os.environ.get('PYTHONNOUSERSITE') == '1' else 1)"]
        self.assertEqual(Lab(self, grader=grader).run()["outcome"], "pass")

    def test_a_protected_file_replaced_by_a_hard_link_is_replaced_by_a_real_copy(self):
        outside = tempfile.mkdtemp(prefix="bench-hl-")
        self.addCleanup(shutil.rmtree, outside, True)
        target = os.path.join(outside, "shared.py")
        with open(target, "w") as f:
            f.write(CHECK)  # the same content: only the link count tells it apart
        src = ("import os\np = os.path.join(os.environ['BENCH_WORKDIR'], 'tests', 'check.py')\nos.unlink(p)\n"
               "os.link(%r, p)\n" % target) + GOOD_AGENT
        lab = Lab(self, agent_src=src)
        r = lab.run(keep=True)
        self.assertEqual((r["outcome"], r["tampered"]), ("pass", 1))
        work = os.path.join(lab.work_root, [d for d in os.listdir(lab.work_root) if d.startswith("bench-")][0], "work")
        self.assertEqual(os.stat(os.path.join(work, "tests", "check.py")).st_nlink, 1)
        self.assertEqual(slurp(target), CHECK)

    def test_a_hard_link_to_a_file_the_agent_controls_cannot_carry_a_cheat_into_grading(self):
        outside = tempfile.mkdtemp(prefix="bench-hl-")
        self.addCleanup(shutil.rmtree, outside, True)
        target = os.path.join(outside, "cheat.py")
        with open(target, "w") as f:
            f.write("import sys\nsys.exit(0)\n")
        src = ("import os\np = os.path.join(os.environ['BENCH_WORKDIR'], 'tests', 'check.py')\nos.unlink(p)\n"
               "os.link(%r, p)\n" % target)
        r = Lab(self, agent_src=src).run()
        self.assertEqual((r["outcome"], r["tampered"]), ("fail", 1))
        self.assertEqual(slurp(target), "import sys\nsys.exit(0)\n")  # the link was broken, not written through

    @unittest.skipIf(os.geteuid() == 0, "root can write anywhere")
    def test_a_protected_directory_made_unwritable_is_a_failure_with_tampering_not_an_error(self):
        src = "import os\nos.chmod(os.path.join(os.environ['BENCH_WORKDIR'], 'tests'), 0o500)\n" + GOOD_AGENT
        r = Lab(self, agent_src=src).run()
        self.assertEqual((r["outcome"], r["tampered"]), ("fail", 1))
        self.assertIn("could not be restored", r["summary"])

    def case_sensitive(self):
        """Whether the temp file system tells "a" from "A" (ext4 does, the macOS default does not). Probed, never assumed."""
        d = tempfile.mkdtemp(prefix="bench-case-")
        self.addCleanup(shutil.rmtree, d, True)
        open(os.path.join(d, "probe"), "w").close()
        return not os.path.exists(os.path.join(d, "PROBE"))

    def test_a_case_variant_of_a_protected_directory_does_not_hide_a_cheat(self):
        # On a case-INsensitive file system "TESTS" is the protected directory itself and its rewritten file is restored;
        # on a case-sensitive one it is a second directory beside it, which must be removed. Either way the variant is gone
        # and counted, and the grader (which fails if any directory named like "tests" but not "tests" exists) passes.
        src = textwrap.dedent("""\
            import os
            w = os.environ['BENCH_WORKDIR']
            os.makedirs(os.path.join(w, 'TESTS'), exist_ok=True)
            open(os.path.join(w, 'TESTS', 'check.py'), 'w').write('import sys\\nsys.exit(0)\\n')
            """) + GOOD_AGENT
        grader = [PY, "-c", "import os, sys\nsys.exit(1 if any(n.lower() == 'tests' and n != 'tests' for n in os.listdir('.')) else 0)"]
        r = Lab(self, agent_src=src, grader=grader).run()
        self.assertEqual(r["outcome"], "pass", "case-sensitive file system: %s" % self.case_sensitive())
        self.assertGreaterEqual(r["tampered"], 1)

    def test_on_a_case_sensitive_file_system_the_variant_is_a_different_directory_and_is_removed(self):
        if not self.case_sensitive():
            self.skipTest("this file system is case-insensitive: the variant is the protected directory itself (covered above)")
        src = textwrap.dedent("""\
            import os
            w = os.environ['BENCH_WORKDIR']
            os.makedirs(os.path.join(w, 'Tests'))
            open(os.path.join(w, 'Tests', 'evil.py'), 'w').write('x')
            """) + GOOD_AGENT
        lab = Lab(self, agent_src=src)
        r = lab.run(keep=True)
        work = os.path.join(lab.work_root, [d for d in os.listdir(lab.work_root) if d.startswith("bench-")][0], "work")
        self.assertFalse(os.path.exists(os.path.join(work, "Tests", "evil.py")))
        self.assertEqual((r["outcome"], r["tampered"]), ("pass", 1))

    def test_a_pinned_grader_file_that_changed_is_a_failure_with_tampering(self):
        lab = Lab(self)
        with open(os.path.join(lab.dir, "grade.py"), "w") as f:
            f.write("import sys\nsys.exit(0)\n")
        lab.doc["tasks"][0]["grader"]["cmd"] = [PY, "{suite_dir}/grade.py"]
        lab.doc["tasks"][0]["grader"]["files"] = {"grade.py": "sha256:" + file_digest(os.path.join(lab.dir, "grade.py"))}
        lab.write_suite()
        self.assertEqual(lab.run()["outcome"], "pass")
        with open(os.path.join(lab.dir, "grade.py"), "w") as f:
            f.write("import sys\nsys.exit(0)\n# changed\n")
        body = lab.body()  # the suite file is unchanged, the script it names is not
        r = attempt.run_attempt(body, lab.work_root, lab.dir)
        self.assertEqual((r["outcome"], r["tampered"]), ("fail", 1))
        self.assertIn("pinned grader file was altered", r["summary"])
        os.unlink(os.path.join(lab.dir, "grade.py"))
        self.assertIn("is gone", attempt.run_attempt(body, lab.work_root, lab.dir)["summary"])

    def test_the_result_names_the_attempt_it_is_about(self):
        r = Lab(self).run()
        self.assertEqual((r["exp"], r["arm"], r["task"]), ("exp", "arm1", "task1"))


class GlobTests(unittest.TestCase):
    def matches(self, pattern, path):
        return bool(attempt.glob_regex(pattern).match(path))

    def test_double_star_crosses_directories_single_star_does_not(self):
        self.assertTrue(self.matches("tests/**", "tests/a.py"))
        self.assertTrue(self.matches("tests/**", "tests/x/y/z.py"))
        self.assertFalse(self.matches("tests/**", "src/tests/a.py"))
        self.assertFalse(self.matches("tests/**", "tests"))
        self.assertTrue(self.matches("tests/*", "tests/a.py"))
        self.assertFalse(self.matches("tests/*.py", "tests/x/a.py"))
        self.assertTrue(self.matches("**/test_*.py", "a/b/test_x.py"))
        self.assertTrue(self.matches("**/test_*.py", "test_x.py"))
        self.assertFalse(self.matches("**/test_*.py", "a/b/tester.py"))
        self.assertTrue(self.matches("*.cfg", "setup.cfg"))
        self.assertFalse(self.matches("*.cfg", "sub/setup.cfg"))
        self.assertTrue(self.matches("conftest.py", "conftest.py"))
        self.assertFalse(self.matches("conftest.py", "xconftest.py"))
        self.assertFalse(self.matches("a.b", "aXb"))  # a dot is a dot

    def test_matching_ignores_case(self):
        self.assertTrue(self.matches("tests/**", "Tests/a.py"))
        self.assertTrue(self.matches("Tests", "tests/a.py"))

    def test_a_directory_name_covers_everything_below_it(self):
        self.assertTrue(self.matches("tests", "tests/a/b.py"))
        self.assertFalse(self.matches("tests", "tests2/a.py"))

    def test_question_mark_is_one_character_inside_a_segment(self):
        self.assertTrue(self.matches("t?.py", "t1.py"))
        self.assertFalse(self.matches("t?.py", "t/.py"))


class ProtectedRestoreTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp(prefix="bench-restore-")
        self.addCleanup(shutil.rmtree, self.tmp, True)
        self.pristine = os.path.join(self.tmp, "p")
        self.work = os.path.join(self.tmp, "w")
        os.makedirs(os.path.join(self.pristine, "tests", "deep"))
        for rel in ("tests/a.py", "tests/deep/b.py", "src.py"):
            with open(os.path.join(self.pristine, rel), "w") as f:
                f.write("orig " + rel)
        shutil.copytree(self.pristine, self.work)

    def test_nothing_changed_nothing_restored(self):
        self.assertEqual(attempt.restore_protected(self.work, self.pristine, ["tests/**"]), [])

    def test_changed_deleted_and_added_files_are_all_reported_and_fixed(self):
        with open(os.path.join(self.work, "tests", "a.py"), "w") as f:
            f.write("hacked")
        os.unlink(os.path.join(self.work, "tests", "deep", "b.py"))
        with open(os.path.join(self.work, "tests", "new.py"), "w") as f:
            f.write("new")
        changed = attempt.restore_protected(self.work, self.pristine, ["tests/**"])
        self.assertEqual(sorted(changed), ["tests/a.py", "tests/deep/b.py", "tests/new.py"])
        with open(os.path.join(self.work, "tests", "a.py")) as f:
            self.assertEqual(f.read(), "orig tests/a.py")
        self.assertFalse(os.path.exists(os.path.join(self.work, "tests", "new.py")))
        self.assertTrue(os.path.exists(os.path.join(self.work, "tests", "deep", "b.py")))

    def test_a_file_replaced_by_a_symbolic_link_is_replaced_back(self):
        os.unlink(os.path.join(self.work, "tests", "a.py"))
        os.symlink("/etc/hosts", os.path.join(self.work, "tests", "a.py"))
        self.assertEqual(attempt.restore_protected(self.work, self.pristine, ["tests/**"]), ["tests/a.py"])
        self.assertFalse(os.path.islink(os.path.join(self.work, "tests", "a.py")))

    def test_unprotected_files_are_left_alone(self):
        with open(os.path.join(self.work, "src.py"), "w") as f:
            f.write("my fix")
        self.assertEqual(attempt.restore_protected(self.work, self.pristine, ["tests/**"]), [])
        with open(os.path.join(self.work, "src.py")) as f:
            self.assertEqual(f.read(), "my fix")

    def test_no_patterns_no_work(self):
        os.unlink(os.path.join(self.work, "tests", "a.py"))
        self.assertEqual(attempt.restore_protected(self.work, self.pristine, []), [])


class BodyAndMainTests(unittest.TestCase):
    def test_read_body_is_strict(self):
        tmp = tempfile.mkdtemp(prefix="bench-body-")
        self.addCleanup(shutil.rmtree, tmp, True)
        good = {"b": 1, "exp": "e", "arm": "a", "task": "t", "rep": 0, "ord": 0, "suite": "/x/s.json", "sha": "a" * 64}

        def read(doc, raw=None):
            p = os.path.join(tmp, "body")
            with open(p, "w") as f:
                f.write(raw if raw is not None else json.dumps(doc))
            return attempt.read_body(p)

        self.assertEqual(read(good)["task"], "t")
        for mutate in (lambda d: d.update(extra=1), lambda d: d.update(b=2), lambda d: d.update(exp="../x"),
                       lambda d: d.update(rep=-1), lambda d: d.update(suite="relative.json"), lambda d: d.update(sha="zz"),
                       lambda d: d.pop("arm"), lambda d: d.update(rep=True)):
            bad = dict(good)
            mutate(bad)
            with self.assertRaises(attempt.HarnessError, msg=bad):
                read(bad)
        for raw in ("not json", "[1]", '{"b":1,"b":1}', "x" * 5000):
            with self.assertRaises(attempt.HarnessError):
                read(None, raw)
        with self.assertRaises(attempt.HarnessError):
            attempt.read_body(os.path.join(tmp, "missing"))

    def run_main(self, lab, **env):
        tmp = tempfile.mkdtemp(prefix="bench-main-")
        self.addCleanup(shutil.rmtree, tmp, True)
        body = os.path.join(tmp, "body")
        with open(body, "w") as f:
            f.write(attempt.make_body("exp", "arm1", "task1", 0, 0, lab.suite_path, lab.sha))
        result = os.path.join(tmp, "result.json")
        e = {"PATH": os.environ["PATH"], "HOME": tmp, "TASK_BODY_PATH": body, "TASK_RESULT_PATH": result}
        e.update(env)
        r = subprocess.run([sys.executable, BENCH, "attempt", "--suite-dir", os.path.dirname(lab.suite_path), "--work-dir", os.path.join(tmp, "w")], env=e,
                           capture_output=True, text=True, timeout=60)
        return r, result

    def test_the_command_writes_the_result_file_the_runner_and_record_read(self):
        lab = Lab(self)
        r, result = self.run_main(lab)
        self.assertEqual(r.returncode, 0, r.stderr)
        doc = json.loads(slurp(result))
        self.assertEqual(doc["status"], "done")
        self.assertTrue(doc["summary"].startswith("bench pass:"))
        self.assertEqual(doc["bench"]["outcome"], "pass")
        self.assertEqual(stat.S_IMODE(os.stat(result).st_mode), 0o600)

    def test_an_error_is_still_a_finished_attempt(self):
        lab = Lab(self, grader=[PY, "-c", "import sys; sys.exit(125)"])
        r, result = self.run_main(lab)
        self.assertEqual(r.returncode, 0)  # the task is done; the outcome is the data
        self.assertEqual(json.loads(slurp(result))["bench"]["outcome"], "error")

    def test_a_task_body_that_names_a_suite_outside_the_allowed_directory_is_refused(self):
        # the body is untrusted text: anyone who can add a board task can write one, and a suite file can name
        # any command. The worker's --suite-dir is the only place suites may come from.
        lab = Lab(self)
        evil = tempfile.mkdtemp(prefix="bench-evil-")
        self.addCleanup(shutil.rmtree, evil, True)
        shutil.copytree(lab.dir, os.path.join(evil, "s"))
        other = os.path.join(evil, "s", "suite.json")
        _, sha, _ = suite.load_suite(other)
        body = json.loads(attempt.make_body("exp", "arm1", "task1", 0, 0, other, sha))
        r = attempt.run_attempt(body, lab.work_root, lab.dir)
        self.assertEqual(r["outcome"], "error")
        self.assertIn("outside the allowed suite directory", r["summary"])
        self.assertEqual(r["duration_ms"] is not None, True)
        # a sibling directory whose name starts like the allowed one is outside too
        sib = lab.dir + "-sibling"
        shutil.copytree(lab.dir, sib)
        self.addCleanup(shutil.rmtree, sib, True)
        _, sha2, _ = suite.load_suite(os.path.join(sib, "suite.json"))
        body = json.loads(attempt.make_body("exp", "arm1", "task1", 0, 0, os.path.join(sib, "suite.json"), sha2))
        self.assertEqual(attempt.run_attempt(body, lab.work_root, lab.dir)["outcome"], "error")

    def test_the_command_will_not_run_without_a_suite_directory(self):
        r = subprocess.run([sys.executable, BENCH, "attempt"], env={"PATH": os.environ["PATH"]}, capture_output=True,
                           text=True, timeout=30)
        self.assertEqual(r.returncode, 2)
        self.assertIn("--suite-dir", r.stderr)

    def test_outside_a_runner_it_says_so(self):
        r = subprocess.run([sys.executable, BENCH, "attempt", "--suite-dir", "/tmp"], env={"PATH": os.environ["PATH"]}, capture_output=True,
                           text=True, timeout=30)
        self.assertEqual(r.returncode, 2)
        self.assertIn("board run -- board-bench attempt", r.stderr)


if __name__ == "__main__":
    unittest.main()

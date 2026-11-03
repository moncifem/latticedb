"""Hardening of `board run` found by an independent review: a child that plants a FIFO, extra agent
arguments that override the built-in safety settings, more stop signals, loader variables, a scratch
HOME, descriptor ownership in the log capture, and a few smaller races. Same fixtures as test_run.py.
"""

import errno
import os
import signal
import stat
import subprocess
import sys
import threading
import time
import unittest

from test_run import RUNNER, RunnerCase, load_runner_module, slurp, task


class Fifo(RunnerCase):
    def test_a_fifo_planted_as_the_result_file_does_not_hang_the_runner(self):
        self.conf(tasks=[task()])
        child = self.script("fifo.py", "import os\nos.mkfifo(os.environ['TASK_RESULT_PATH'])\n")
        p = self.run_runner(self.args("--", sys.executable, child), timeout=30)
        self.assertEqual(p.returncode, 0, p.err)
        self.assertIn("not a regular file", p.err)
        self.assertEqual(len(self.calls("done")), 1)

    def test_a_fifo_planted_as_the_last_message_does_not_hang_the_runner(self):
        self.conf(tasks=[task()])
        agent = self.script("codex", """
            #!%s
            import os, sys
            os.mkfifo(sys.argv[sys.argv.index("-o") + 1])
        """ % sys.executable)
        p = self.run_runner(self.args("--agent", "codex", "--agent-bin", agent), timeout=30)
        self.assertEqual(p.returncode, 0, p.err)
        self.assertEqual(len(self.calls("done")), 1)

    def test_the_reader_refuses_non_regular_files_and_symlinks(self):
        mod = load_runner_module()
        fifo = os.path.join(self.tmp, "f")
        os.mkfifo(fifo)
        self.assertEqual(mod.read_regular(fifo, 10), (None, "not a regular file"))
        link = os.path.join(self.tmp, "l")
        target = os.path.join(self.tmp, "t")
        with open(target, "w") as f:
            f.write("x")
        os.symlink(target, link)
        raw, problem = mod.read_regular(link, 10)
        self.assertIsNone(raw)
        self.assertTrue(problem)


class AgentArguments(RunnerCase):
    def refused(self, *extra):
        p = self.run_runner(self.args("--agent", "claude", "--agent-bin", sys.executable, *extra))
        self.assertEqual(p.returncode, 2, extra)
        self.assertIn("--unsafe-agent-flags", p.err)
        self.assertEqual(self.calls(), [])

    def test_arguments_that_override_a_safety_setting_need_the_acknowledgement(self):
        for arg in ("--permission-mode=auto", "--allowedTools=Bash", "--add-dir=/", "--settings", "--mcp-config",
                    "--yolo", "-c", "--config", "-s", "--sandbox", "--dangerously-skip-permissions",
                    "--model=--add-dir", "--max-budget-usd=100"):
            self.refused("--agent-arg=" + arg)

    def test_a_tuning_option_followed_by_another_option_is_refused(self):
        self.refused("--agent-arg=--model", "--agent-arg=--add-dir")
        self.refused("--agent-arg=--model")

    def test_plain_tuning_is_accepted(self):
        self.conf(tasks=[])
        p = self.run_runner(self.args("--agent", "claude", "--agent-bin", sys.executable,
                                      "--agent-arg=--model", "--agent-arg=sonnet", "--agent-arg=--effort=high"))
        self.assertEqual(p.returncode, 0, p.err)

    def test_the_acknowledgement_lets_them_through(self):
        self.conf(tasks=[])
        p = self.run_runner(self.args("--agent", "claude", "--agent-bin", sys.executable, "--unsafe-agent-flags",
                                      "--agent-arg=--add-dir=/tmp"))
        self.assertEqual(p.returncode, 0, p.err)

    def test_an_unrestricted_shell_tool_needs_the_acknowledgement(self):
        for tools in ("Read,Bash", "Bash", "Bash(*)", "Read, Bash( )", "Read,Edit,Bash(:*)"):
            self.refused("--agent-allowed-tools", tools)
        self.conf(tasks=[])
        p = self.run_runner(self.args("--agent", "claude", "--agent-bin", sys.executable,
                                      "--agent-allowed-tools", "Read,Bash(git diff *),Bash(git status *)"))
        self.assertEqual(p.returncode, 0, p.err)
        p = self.run_runner(self.args("--agent", "claude", "--agent-bin", sys.executable, "--unsafe-agent-flags",
                                      "--agent-allowed-tools", "Read,Bash"))
        self.assertEqual(p.returncode, 0, p.err)

    def test_a_raw_command_gets_a_note_that_the_flag_does_not_guard_it(self):
        self.conf(tasks=[])
        p = self.run_runner(self.args("--", sys.executable, "-c", "pass"))
        self.assertIn("guards only the built-in agent templates", p.err)


class Signals(RunnerCase):
    def test_hup_and_quit_stop_the_worker_like_term(self):
        for sig in (signal.SIGHUP, signal.SIGQUIT):
            self.conf(tasks=[task()])
            for name in ("state.json", "calls.jsonl"):
                path = os.path.join(self.fb, name)
                if os.path.exists(path):
                    os.unlink(path)
            marker = os.path.join(self.tmp, "started")
            child = self.script("slow.py", "import time\nopen(%r, 'w').write('x')\ntime.sleep(120)\n" % marker)
            p = self.start(self.args("--forever", "--grace", "2", "--", sys.executable, child))
            self.wait_for(lambda: os.path.exists(marker), "the child")
            p.send_signal(sig)
            p.communicate(timeout=60)
            self.assertEqual(p.returncode, 128 + sig, sig)
            self.assertEqual(len(self.calls("release")), 1, sig)
            self.assertEqual(self.calls("done") + self.calls("fail"), [], sig)
            os.unlink(marker)

    def test_a_release_the_fence_refuses_is_not_reported_as_released(self):
        self.conf(tasks=[task()], release_rc=3)
        marker = os.path.join(self.tmp, "started")
        child = self.script("slow.py", "import time\nopen(%r, 'w').write('x')\ntime.sleep(120)\n" % marker)
        p = self.start(self.args("--forever", "--grace", "2", "--", sys.executable, child))
        self.wait_for(lambda: os.path.exists(marker), "the child")
        p.send_signal(signal.SIGTERM)
        out, err = p.communicate(timeout=60)
        self.assertNotIn("released on shutdown", out)
        self.assertIn("lost at release", out)
        self.assertIn("released=0", err)


class Environment(RunnerCase):
    def test_the_parent_pid_is_the_runners(self):
        self.conf(tasks=[task()])
        out = self.sandbox()
        child = self.script("pp.py", "import os\nopen(%r, 'w').write(os.environ['TASK_PARENT_PID'] + ' ' + str(os.getppid()))\n"
                            % os.path.join(out, "pp"))
        p = self.start(self.args("--", sys.executable, child))
        p.communicate(timeout=60)
        env_pid, ppid = slurp(os.path.join(out, "pp")).split()
        self.assertEqual(env_pid, str(p.pid))
        self.assertEqual(ppid, str(p.pid))

    def test_reserved_names_are_refused_in_any_case(self):
        for name in ("pgpassword", "PgHost", "board_author", "Lattice_Board_Url", "task_id"):
            p = self.run_runner(self.args("--env-allow", name, "--", sys.executable, "-c", "pass"))
            self.assertEqual(p.returncode, 2, name)

    def test_loader_and_startup_variables_need_the_acknowledgement(self):
        for name in ("LD_PRELOAD", "LD_LIBRARY_PATH", "DYLD_INSERT_LIBRARIES", "BASH_ENV", "PYTHONPATH",
                     "PYTHONSTARTUP", "NODE_OPTIONS", "SSH_AUTH_SOCK"):
            p = self.run_runner(self.args("--env-allow", name, "--", sys.executable, "-c", "pass"))
            self.assertEqual(p.returncode, 2, name)
            self.assertIn("--unsafe-agent-flags", p.err)
        self.conf(tasks=[])
        p = self.run_runner(self.args("--env-allow", "NODE_OPTIONS", "--unsafe-agent-flags", "--",
                                      sys.executable, "-c", "pass"))
        self.assertEqual(p.returncode, 0, p.err)

    def test_allowing_a_credential_looking_name_warns_without_printing_the_value(self):
        self.conf(tasks=[])
        p = self.run_runner(self.args("--env-allow", "MY_API_TOKEN", "--", sys.executable, "-c", "pass"),
                            {"MY_API_TOKEN": "value-that-must-not-print"})
        self.assertIn("MY_API_TOKEN", p.err)
        self.assertNotIn("value-that-must-not-print", p.err + p.out)

    def test_scratch_home_gives_each_task_an_empty_private_home(self):
        self.conf(tasks=[task("t1"), task("t2")])
        out = self.sandbox()
        child = self.script("home.py", """
            import json, os, stat
            h = os.environ["HOME"]
            json.dump({"home": h, "mode": stat.S_IMODE(os.stat(h).st_mode), "files": os.listdir(h)},
                      open(%r %% os.environ["TASK_ID"], "w"))
        """ % os.path.join(out, "%s.json"))
        p = self.run_runner(self.args("--scratch-home", "--", sys.executable, child))
        self.assertEqual(p.returncode, 0, p.err)
        homes = []
        for t in ("t1", "t2"):
            r = self.read_json(os.path.join(out, t + ".json"))
            self.assertNotEqual(r["home"], self.tmp)
            self.assertTrue(r["home"].startswith(os.path.realpath(self.results)) or r["home"].startswith(self.results))
            self.assertEqual(r["mode"], 0o700)
            self.assertEqual(r["files"], [])
            homes.append(r["home"])
        self.assertNotEqual(homes[0], homes[1])

    def test_without_scratch_home_the_child_keeps_the_real_home(self):
        self.conf(tasks=[task()])
        out = self.sandbox()
        child = self.script("home.py", "import os\nopen(%r, 'w').write(os.environ['HOME'])\n" % os.path.join(out, "h"))
        self.run_runner(self.args("--", sys.executable, child))
        self.assertEqual(slurp(os.path.join(out, "h")), self.tmp)

    def test_the_title_is_cleaned_before_it_enters_the_environment(self):
        self.conf(tasks=[task(title="\x1b[31mred\x07 title\x00")])
        out = self.sandbox()
        child = self.script("t.py", "import os\nopen(%r, 'w').write(os.environ['TASK_TITLE'])\n" % os.path.join(out, "t"))
        self.run_runner(self.args("--", sys.executable, child))
        self.assertEqual(slurp(os.path.join(out, "t")), "red title")


class Smaller(RunnerCase):
    def test_the_lease_is_kept_alive_while_a_worktree_is_made(self):
        bin_dir = os.path.join(self.tmp, "bin")
        os.mkdir(bin_dir)
        fake_git = os.path.join(bin_dir, "git")
        with open(fake_git, "w") as f:
            f.write("#!/bin/sh\nsleep 4.5\necho 'fatal: slow failure' >&2\nexit 1\n")
        os.chmod(fake_git, 0o755)
        repo = os.path.join(self.tmp, "repo")
        os.mkdir(repo)
        self.conf(tasks=[task()])
        p = self.run_runner(self.args("--lease", "3", "--worktree-per-task", repo, "--", sys.executable, "-c", "pass"),
                            {"PATH": bin_dir + os.pathsep + os.environ.get("PATH", "")}, timeout=60)
        self.assertGreaterEqual(len(self.calls("heartbeat")), 1, "no heartbeat while the worktree was being made")
        self.assertEqual(len(self.calls("release")), 1)
        self.assertIn("errors=1", p.err)

    def test_a_start_failure_leaves_no_task_text_behind(self):
        self.conf(tasks=[task(body="secret body text")])
        bad = os.path.join(self.tmp, "bad")
        with open(bad, "w") as f:
            f.write("#!/nonexistent/interpreter\n")
        os.chmod(bad, 0o755)
        self.run_runner(self.args("--", bad))
        run_dir = os.path.join(self.results, "t1.101")
        self.assertTrue(os.path.isdir(run_dir))
        self.assertEqual([n for n in os.listdir(run_dir) if n in ("body.txt", "prompt.txt")], [])

    def test_a_run_directory_that_already_exists_does_not_crash_the_loop(self):
        os.mkdir(self.results, 0o700)
        os.mkdir(os.path.join(self.results, "t1.101"), 0o700)
        os.mkdir(os.path.join(self.results, "t1.101.1"), 0o700)
        self.conf(tasks=[task()])
        p = self.run_runner(self.args("--", sys.executable, "-c", "pass"))
        self.assertEqual(p.returncode, 0, p.err)
        self.assertEqual(len(self.calls("done")), 1)
        self.assertTrue(os.path.isdir(os.path.join(self.results, "t1.101.2")))

    def test_waiting_for_the_leader_does_not_reap_it(self):
        mod = load_runner_module()
        if not hasattr(os, "waitid"):
            self.skipTest("no waitid")
        proc = subprocess.Popen([sys.executable, "-c", "pass"], start_new_session=True)
        end = time.time() + 20
        while not mod.leader_exited(proc) and time.time() < end:
            time.sleep(0.05)
        self.assertTrue(mod.leader_exited(proc))
        # still there to be reaped: its pid (and so its group id) is still ours
        self.assertEqual(os.waitpid(proc.pid, os.WNOHANG)[0], proc.pid)
        proc.wait()

    def test_a_child_that_cannot_be_reaped_does_not_block_for_ever(self):
        mod = load_runner_module()

        class Stuck:
            pid = 999999

            def wait(self, timeout=None):
                if timeout is None:
                    raise AssertionError("an unbounded wait")
                raise subprocess.TimeoutExpired("x", timeout)

        start = time.time()
        self.assertEqual(mod.reap(Stuck()), -signal.SIGKILL)
        self.assertLess(time.time() - start, 1)


class Capture(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.mod = load_runner_module()

    def test_the_capture_thread_closes_the_descriptor_it_owns(self):
        import tempfile
        d = tempfile.mkdtemp()
        self.addCleanup(__import__("shutil").rmtree, d, True)
        r, w = os.pipe()
        cap = self.mod.LogCapture(r, os.path.join(d, "log"), 4096)
        cap.start()
        os.write(w, b"hello\n")
        os.close(w)
        cap.finish()
        with self.assertRaises(OSError) as ctx:
            os.fstat(r)
        self.assertEqual(ctx.exception.errno, errno.EBADF)
        self.assertEqual(slurp(os.path.join(d, "log")), "hello\n")
        self.assertEqual(stat.S_IMODE(os.stat(os.path.join(d, "log")).st_mode), 0o600)

    def test_a_log_that_cannot_be_written_is_still_drained(self):
        class Broken(self.mod.LogCapture):
            def _open(self):
                class Full:
                    def write(self, data):
                        raise OSError(errno.ENOSPC, "No space left on device")

                    def close(self):
                        pass
                return Full()

        r, w = os.pipe()
        cap = Broken(r, "/nonexistent/log", 4096)
        cap.start()
        done = threading.Event()

        def writer():
            for _ in range(60):
                os.write(w, b"y" * 20000)  # far more than a pipe holds
            os.close(w)
            done.set()

        t = threading.Thread(target=writer, daemon=True)
        t.start()
        self.assertTrue(done.wait(20), "the writer blocked: the pipe was not drained after the write error")
        cap.finish()
        self.assertIsNotNone(cap.write_error)
        self.assertTrue(bytes(cap.recent).startswith(b"y"))

    def test_a_straggler_holding_the_pipe_does_not_block_finish_for_ever(self):
        import tempfile
        d = tempfile.mkdtemp()
        self.addCleanup(__import__("shutil").rmtree, d, True)
        r, w = os.pipe()
        self.addCleanup(os.close, w)
        cap = self.mod.LogCapture(r, os.path.join(d, "log"), 4096)
        cap.start()
        start = time.time()
        cap.finish()
        self.assertLess(time.time() - start, 12)
        self.assertFalse(cap.is_alive())


if __name__ == "__main__":
    unittest.main()

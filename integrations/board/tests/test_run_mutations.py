"""Planted faults in the runner: every one must make test_run.py fail.

For each mutation this copies integrations/board/board-run to a temporary file, replaces one piece of
text that must occur exactly once, and runs the named tests from test_run.py against the copy
(BOARD_RUN_UNDER_TEST). The mutation is caught when that run fails. A mutation nobody catches means a
rule of the runner is not pinned by a test. The expectations in test_run.py were written from the rules
in the brief (scrubbed environment, kill the whole group, no mark after a lost lease, ...), not
derived by running the runner.

Run: python3 -m unittest discover -s integrations/board/tests -p "test_run_mutations.py"
"""

import concurrent.futures
import os
import shutil
import subprocess
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
RUNNER = os.path.join(os.path.dirname(HERE), "board-run")

# name -> (text in board-run, replacement, tests of test_run.py that must catch it)
MUTATIONS = {
    "child_inherits_the_whole_environment": (
        "env = {n: v for n, v in self.env_pairs}",
        "env = dict(os.environ)",
        ["test_run.Flow.test_the_board_helper_gets_the_database_settings_but_the_child_does_not",
         "test_run.Flow.test_env_allow_passes_only_the_named_variables"]),
    "lease_loss_kills_only_the_leader": (
        "os.killpg(pgid, sig)",
        "os.kill(pgid, sig)",
        ["test_run.Supervision.test_lease_loss_kills_the_whole_process_group_and_marks_nothing"]),
    "done_after_the_lease_was_lost": (
        'if beat.lost and reason != "shutdown":',
        "if False:",
        ["test_run.Supervision.test_a_refusal_that_arrives_after_the_child_exited_still_stops_the_mark"]),
    "the_heartbeat_is_never_started": (
        "beat.start()",
        "pass",
        ["test_run.Supervision.test_heartbeats_continue_while_the_child_is_busy_and_silent",
         "test_run_hardening.Smaller.test_the_lease_is_kept_alive_while_a_worktree_is_made"]),
    "the_heartbeat_waits_three_leases": (
        "interval = self.lease / 3.0",
        "interval = self.lease * 3.0",
        ["test_run.Supervision.test_heartbeats_continue_while_the_child_is_busy_and_silent"]),
    "a_refused_heartbeat_is_ignored": (
        "elif rc == 3:",
        "elif rc == 98765:",
        ["test_run.Supervision.test_lease_loss_kills_the_whole_process_group_and_marks_nothing"]),
    "the_task_body_goes_into_argv": (
        "argv = list(cfg.command)",
        "argv = list(cfg.command) + [body]",
        ["test_run.Flow.test_the_body_reaches_the_child_only_through_a_private_file"]),
    "exit_75_is_a_plain_failure": (
        "elif rc == EX_TEMPFAIL:",
        "elif rc == -12345:",
        ["test_run.ExitCodes.test_75_is_a_retry_with_a_doubling_delay"]),
    "the_task_timeout_is_not_enforced": (
        "elif time.monotonic() > deadline:",
        "elif False:",
        ["test_run.Supervision.test_a_task_timeout_kills_the_group_and_fails_the_task"]),
    "shutdown_does_not_release_the_task": (
        'elif reason == "shutdown":\n            if self.mark("release", task_id, *fence_args) == 3:',
        'elif reason == "shutdown":\n            if True:',
        ["test_run.Supervision.test_sigterm_stops_claiming_kills_the_child_and_releases_the_task"]),
    "signals_are_not_handled": (
        "for sig in (signal.SIGTERM, signal.SIGINT, signal.SIGHUP, signal.SIGQUIT):",
        "for sig in ():",
        ["test_run.Supervision.test_sigterm_stops_claiming_kills_the_child_and_releases_the_task"]),
    "a_result_file_overrides_the_exit_code": (
        "            if rc == 0:\n                status =",
        "            if rc == 0 or (res or {}).get('status') == 'done':\n                status =",
        ["test_run.ResultFile.test_the_exit_code_stays_authoritative_for_failure"]),
    "the_child_shares_the_runner_process_group": (
        "start_new_session=True",
        "start_new_session=False",
        ["test_run.Supervision.test_the_child_is_in_its_own_process_group"]),
    "the_child_inherits_stdin": (
        'if stdin_path else subprocess.DEVNULL',
        'if stdin_path else None',
        ["test_run.Flow.test_the_body_reaches_the_child_only_through_a_private_file"]),
    "the_log_is_not_capped": (
        "room = max(self.cap - self.head, 0)",
        "room = len(chunk)",
        ["test_run.LogsAndDirs.test_the_log_is_capped_private_and_keeps_the_tail"]),
    "secrets_are_not_redacted": (
        "    for value in secret_values:\n        if value:",
        "    for value in []:\n        if value:",
        ["test_run.ExitCodes.test_error_text_is_one_clean_line_and_hides_secrets"]),
    "error_text_keeps_control_characters_and_backslashes": (
        'text = CONTROL.sub(" ", text.replace("\\\\", "/"))',
        "text = text",
        ["test_run.ExitCodes.test_error_text_is_one_clean_line_and_hides_secrets"]),
    # ---- found by the independent review
    "a_fifo_as_the_result_file_blocks_the_runner": (
        "os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK",
        "os.O_RDONLY | os.O_NOFOLLOW",
        ["test_run_hardening.Fifo.test_a_fifo_planted_as_the_result_file_does_not_hang_the_runner"]),
    "a_symlinked_result_file_is_followed": (
        "os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK",
        "os.O_RDONLY | os.O_NONBLOCK",
        ["test_run.ResultFile.test_a_symlinked_result_file_is_not_followed"]),
    "reserved_names_can_be_allowed": (
        "if FORBIDDEN_ENV.match(n):",
        "if False:",
        ["test_run.Flow.test_reserved_names_cannot_be_allowed"]),
    "reserved_names_are_matched_case_sensitively": (
        '|TASK_[A-Z0-9_]*)$", re.I)',
        '|TASK_[A-Z0-9_]*)$")',
        ["test_run_hardening.Environment.test_reserved_names_are_refused_in_any_case"]),
    "loader_variables_are_allowed": (
        "if DENIED_ENV.match(n) and not a.unsafe_agent_flags:",
        "if False:",
        ["test_run_hardening.Environment.test_loader_and_startup_variables_need_the_acknowledgement"]),
    "the_unsafe_gate_is_open": (
        "if unsafe and not a.unsafe_agent_flags:",
        "if False:",
        ["test_run.Agents.test_unsafe_choices_need_the_explicit_acknowledgement"]),
    "any_extra_agent_argument_is_accepted": (
        "if name in SAFE_AGENT_OPTIONS:",
        "if True:",
        ["test_run_hardening.AgentArguments.test_arguments_that_override_a_safety_setting_need_the_acknowledgement"]),
    "an_unrestricted_shell_tool_is_accepted": (
        'if name.lower() in ("bash", "shell", "powershell") and',
        "if False and",
        ["test_run_hardening.AgentArguments.test_an_unrestricted_shell_tool_needs_the_acknowledgement"]),
    "the_first_note_fence_check_is_dropped": (
        '"started by %s, attempt %d" % (cfg.owner, attempt)) == 3:',
        '"started by %s, attempt %d" % (cfg.owner, attempt)) == 98765:',
        ["test_run.Supervision.test_a_task_cancelled_before_the_start_never_runs"]),
    "done_is_sent_without_the_fence": (
        'code = self.mark("done", task_id, *fence_args, "--", summary or "done")',
        'code = self.mark("done", task_id, "--", summary or "done")',
        ["test_run.Flow.test_a_task_is_claimed_run_and_marked_done"]),
    "sigkill_is_never_sent": (
        ("    signal_group(pgid, signal.SIGKILL)\n\n\nclass LogCapture", "signal_group(proc.pid, signal.SIGKILL)\n        rc = reap(proc)"),
        ("    pass\n\n\nclass LogCapture", "rc = reap(proc)"),
        ["test_run.Supervision.test_a_child_that_ignores_sigterm_is_killed_after_the_grace_period"]),
    "leftover_processes_of_the_group_are_not_killed": (
        "signal_group(proc.pid, signal.SIGKILL)\n        rc = reap(proc)",
        "rc = reap(proc)",
        ["test_run.Supervision.test_leftover_processes_of_a_finished_child_are_cleaned_up"]),
    "a_second_signal_does_not_skip_the_grace": (
        "while time.monotonic() < end and not hurry():",
        "while time.monotonic() < end:",
        ["test_run.Supervision.test_a_second_signal_skips_the_grace_period"]),
    "task_files_are_world_readable": (
        '0o600)\n    with os.fdopen(fd, "wb") as f:',
        '0o644)\n    with os.fdopen(fd, "wb") as f:',
        ["test_run.Flow.test_the_body_reaches_the_child_only_through_a_private_file"]),
    "the_log_is_world_readable": (
        '0o600)\n        return os.fdopen(fd, "wb")',
        '0o644)\n        return os.fdopen(fd, "wb")',
        ["test_run.LogsAndDirs.test_the_log_is_capped_private_and_keeps_the_tail"]),
    "hup_and_quit_are_not_handled": (
        "for sig in (signal.SIGTERM, signal.SIGINT, signal.SIGHUP, signal.SIGQUIT):",
        "for sig in (signal.SIGTERM, signal.SIGINT):",
        ["test_run_hardening.Signals.test_hup_and_quit_stop_the_worker_like_term"]),
    "a_refused_release_is_reported_as_released": (
        'if self.mark("release", task_id, *fence_args) == 3:',
        'if self.mark("release", task_id, *fence_args) == 98765:',
        ["test_run_hardening.Signals.test_a_release_the_fence_refuses_is_not_reported_as_released"]),
    "the_capture_stops_draining_after_a_write_error": (
        "self._write(out, chunk[:room])",
        "self._write(out, chunk[:room])\n            if self.write_error:\n                break",
        ["test_run_hardening.Capture.test_a_log_that_cannot_be_written_is_still_drained"]),
    "nobody_closes_the_capture_descriptor": (
        "os.close(self.fd)",
        "pass",
        ["test_run_hardening.Capture.test_the_capture_thread_closes_the_descriptor_it_owns"]),
    "the_scratch_home_is_ignored": (
        'env["HOME"] = home',
        "pass",
        ["test_run_hardening.Environment.test_scratch_home_gives_each_task_an_empty_private_home"]),
    "the_title_enters_the_environment_unclean": (
        '"TASK_TITLE": one_line(title, 1024),',
        '"TASK_TITLE": title,',
        ["test_run_hardening.Environment.test_the_title_is_cleaned_before_it_enters_the_environment"]),
    "the_parent_pid_is_wrong": (
        '"TASK_PARENT_PID": str(os.getpid()),',
        '"TASK_PARENT_PID": "1",',
        ["test_run_hardening.Environment.test_the_parent_pid_is_the_runners"]),
    "a_run_directory_collision_crashes": (
        "            except FileExistsError:\n                continue",
        "            except FileExistsError:\n                raise",
        ["test_run_hardening.Smaller.test_a_run_directory_that_already_exists_does_not_crash_the_loop"]),
    "the_leader_is_reaped_before_its_group_is_signalled": (
        "os.WEXITED | os.WNOHANG | os.WNOWAIT",
        "os.WEXITED | os.WNOHANG",
        ["test_run_hardening.Smaller.test_waiting_for_the_leader_does_not_reap_it"]),
    "the_final_wait_is_unbounded": (
        "return proc.wait(timeout=10)",
        "return proc.wait()",
        ["test_run_hardening.Smaller.test_a_child_that_cannot_be_reaped_does_not_block_for_ever"]),
}


def run_tests(runner_path, tests):
    env = dict(os.environ, BOARD_RUN_UNDER_TEST=runner_path)
    return subprocess.run([sys.executable, "-m", "unittest", *tests],
                          cwd=HERE, env=env, capture_output=True, text=True, timeout=300)


class Mutations(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        with open(RUNNER) as f:
            cls.source = f.read()

    def mutated_copy(self, name):
        old, new, _ = MUTATIONS[name]
        text = self.source
        for o, n in zip(old if isinstance(old, tuple) else (old,), new if isinstance(new, tuple) else (new,)):
            self.assertEqual(text.count(o), 1, "the text to mutate must occur exactly once: %r" % o)
            text = text.replace(o, n)
        d = tempfile.mkdtemp(prefix="board-run-mut-")
        self.addCleanup(shutil.rmtree, d, True)
        path = os.path.join(d, "board-run")
        with open(path, "w") as f:
            f.write(text)
        return path

    def test_the_unmutated_runner_passes_the_same_tests(self):
        every = sorted({t for _, _, tests in MUTATIONS.values() for t in tests})
        r = run_tests(RUNNER, every)
        self.assertEqual(r.returncode, 0, r.stderr[-3000:])

    def test_every_planted_fault_is_caught(self):
        # Each run uses its own temporary directories, so a few can go at once.
        paths = {name: self.mutated_copy(name) for name in MUTATIONS}
        with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:
            futures = {name: pool.submit(run_tests, paths[name], MUTATIONS[name][2]) for name in MUTATIONS}
        survivors = [name for name, fut in futures.items() if fut.result().returncode == 0]
        self.assertEqual(survivors, [], "these planted faults were not caught by their tests")


if __name__ == "__main__":
    unittest.main()

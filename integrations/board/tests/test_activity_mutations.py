"""Planted faults in the live-activity feature: every one must make its named tests fail.

For each mutation this copies the file (the board helper, the Claude Code hook, the runner, or the console package),
replaces one piece of text that must occur exactly once, and runs the named tests against the copy
(BOARD_UNDER_TEST, ACTIVITY_HOOK_UNDER_TEST, BOARD_RUN_UNDER_TEST, or the console tests inside the copied tree). The
mutation is caught when that run fails. A mutation nobody catches means a rule of the feature is not pinned by a test.
The expected values in the tests were derived by hand from the rules (rate limit, what is stored, exit codes, the
states), not by running the code.

Run: python3 -m unittest discover -s integrations/board/tests -p "test_activity_mutations.py"
"""

import concurrent.futures
import os
import shutil
import subprocess
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
BOARD_DIR = os.path.dirname(HERE)
INTEGRATIONS = os.path.dirname(BOARD_DIR)
CONSOLE_DIR = os.path.join(INTEGRATIONS, "console")

C = "test_activity_cli."
H = "test_activity_hook."
R = "test_activity_run."

# (target, text that occurs once, replacement, tests that must catch it)
#   target: "board", "hook", "run" (files under integrations/board) or "console:PATH" (a file of integrations/console)
MUTATIONS = {
    # ---- the helper
    "helper_has_no_rate_limit": (
        "board", 'if { [ "$act_flush" = 0 ] || [ "$act_fails" -gt 0 ]; } && [ "$act_age" -ge 0 ] && [ "$act_age" -lt "$act_wait" ]; then', "if false; then",
        [C + "RateLimit.test_the_default_interval_is_five_seconds", C + "RateLimit.test_the_interval_is_the_setting"]),
    "a_failed_write_loses_its_calls": (
        "board", 'cat "$act_work" >>"$act_pending" 2>/dev/null && rm -f "$act_work"', 'rm -f "$act_work"',
        [C + "Failures.test_a_failed_write_loses_nothing"]),
    "a_failing_database_is_tried_at_every_call": (
        "board", 'while [ "$act_f" -gt 0 ] && [ "$act_wait" -lt 300 ]; do', "while false; do",
        [C + "Failures.test_the_wait_doubles_with_every_failure_up_to_300_seconds",
         C + "Failures.test_a_database_that_is_away_costs_one_connection_not_one_per_call"]),
    "the_unusable_directory_fallback_has_no_floor": (
        "board", "  ACT_FLOOR=30\n", "  ACT_FLOOR=0\n",
        [C + "Unusable.test_an_unusable_state_directory_limits_writes_to_one_per_30_seconds"]),
    "the_target_is_not_sanitised": (
        "board", 'act_target=$(act_clean_target "$BO_target")', "act_target=$BO_target",
        [C + "Sanitiser.test_every_case"]),
    "a_url_query_string_is_kept": (
        "board", "-e 's/[?#].*$//'", "-e 's/^$//'", [C + "Sanitiser.test_every_case"]),
    "the_ring_is_never_pruned": (
        "board", '  [ -n "$ap_over" ] || return 0\n', "  return 0\n", [C + "Ring.test_the_oldest_rows_go_when_the_ring_is_over"]),
    "a_cut_that_is_not_a_number_reaches_the_delete": (
        "board", "case $ap_cut in '' | *[!0-9]*) return 0 ;; esac", "case $ap_cut in '') return 0 ;; esac",
        [C + "Ring.test_a_cut_that_is_not_a_number_is_not_used"]),
    "activity_is_written_without_the_switch": (
        "board", 'if [ "${BOARD_ACTIVITY:-0}" != 1 ]; then', "if false; then", [C + "Gate.test_it_is_off_unless_board_activity_is_1"]),
    "values_are_written_into_the_sql_text": (
        "board", "act_sql_text=$(act_sql)", """act_sql_text=$(act_sql | sed "s/:'tool'/'$A_TOOL'/g")""",
        [C + "FirstWrite.test_the_first_call_writes_at_once_with_every_value_as_a_variable"]),
    "marks_are_counted_as_tool_calls": (
        "board", """A_CALLS=$(LC_ALL=C awk -F'\\t' '$1 != "session-start" && $1 != "session-end" && $1 != "stop" && $1 != "prompt"' "$as_tmp\"""",
        """A_CALLS=$(LC_ALL=C awk -F'\\t' '1' "$as_tmp\"""", [C + "Summary.test_lifecycle_marks_are_not_counted_as_calls"]),
    "a_symlinked_state_directory_is_followed": (
        "board", ('[ ! -L "$1" ] && [ -d "$1" ] || return 1', 'adt_ls=$(ls -ldn "$1" 2>/dev/null)'),
        ('[ -d "$1" ] || return 1', 'adt_ls=$(ls -ldnL "$1" 2>/dev/null)'),
        [C + "Unusable.test_an_activity_subdirectory_that_is_a_symlink_is_not_followed"]),
    "a_loose_state_directory_is_trusted": (
        "board", """[ "$(printf '%s\\n' "$adt_ls" | cut -c1-10)" = 'drwx------' ]""", "true",
        [C + "Unusable.test_a_state_directory_with_loose_permissions_is_not_used"]),
    "calls_of_a_killed_writer_are_forgotten": (
        "board", 'if ! kill -0 "$aa_pid" 2>/dev/null; then', "if false; then",
        [C + "Failures.test_a_writer_killed_midway_leaves_calls_that_the_next_one_adopts"]),
    "the_work_file_of_a_live_writer_is_taken": (
        "board", 'if ! kill -0 "$aa_pid" 2>/dev/null; then', "if true; then",
        [C + "Failures.test_a_work_file_of_a_live_process_is_left_alone"]),
    "purge_of_others_needs_no_yes": (
        "board", '[ "$ap_yes" = 1 ] || die "deleting the activity of other agents needs --yes"', "true",
        [C + "ListAndPurge.test_purge_of_another_agent_or_everybody_needs_yes"]),
    "the_purge_keeps_the_local_counters": (
        "board", 'rm -f "$ACT_DIR/$ap_agent"@*.pending "$ACT_DIR/$ap_agent"@*.last "$ACT_DIR/$ap_agent"@*.work.* 2>/dev/null', "true",
        [C + "ListAndPurge.test_purge_deletes_the_authors_rows_and_local_counters"]),
    "a_lost_session_row_is_never_inserted_again": (
        "board", "then act_known=1; else act_known=0; fi", "then act_known=1; else act_known=1; fi",
        [C + "SessionRow.test_the_helper_inserts_first_then_updates_and_inserts_again_when_the_row_is_gone"]),
    "an_insert_that_finds_the_row_is_not_repeated_as_an_update": (
        "board", 'if [ "$act_known" = 0 ] && [ "$act_try" -lt 3 ]; then act_known=1; else break; fi', "break",
        [C + "SessionRow.test_an_insert_that_finds_the_row_is_repeated_as_an_update"]),
    "a_connect_failure_is_tried_twelve_times": (
        "board", "BOARD_CONNECT_TRIES=${BOARD_ACTIVITY_CONNECT_TRIES:-1}", "BOARD_CONNECT_TRIES=${BOARD_ACTIVITY_CONNECT_TRIES:-12}",
        [C + "Failures.test_a_connect_failure_is_tried_once_by_default_and_as_often_as_asked"]),
    "the_session_row_is_not_found_by_its_key": (
        "board", """-v key="$AUTHOR/$act_session\"""", """-v key="$act_session\"""", [C + "FirstWrite.test_the_first_call_writes_at_once_with_every_value_as_a_variable"]),
    "the_failure_is_counted_only_after_the_attempt": (
        "board", 'act_stamp_write "$act_stamp" "$act_now_s $((act_fails + 1)) $act_known" || die "cannot write $act_stamp"', ":",
        [C + "Backoff.test_a_writer_killed_while_the_database_does_not_answer_has_left_the_backoff",
         C + "Backoff.test_each_kill_doubles_the_wait"]),
    "a_flush_ignores_the_backoff": (
        "board", '{ [ "$act_flush" = 0 ] || [ "$act_fails" -gt 0 ]; }', '[ "$act_flush" = 0 ]',
        [C + "Backoff.test_a_flush_waits_too_once_a_write_has_failed_and_goes_through_before"]),
    "the_stamp_is_written_in_place": (
        "board", '  if printf \'%s\\n\' "$2" >"$asw_tmp" && mv "$asw_tmp" "$1"; then return 0; fi', '  if printf \'%s\\n\' "$2" >"$1"; then return 0; fi',
        [C + "Backoff.test_the_stamp_is_written_before_the_pending_file_is_taken"]),
    "authors_and_sessions_share_counter_files": (
        "board", "  act_prefix=$ACT_DIR/$AUTHOR@$act_session", "  act_prefix=$ACT_DIR/$AUTHOR.$act_session",
        [C + "Names.test_authors_and_sessions_that_join_to_the_same_text_have_their_own_files",
         C + "Names.test_a_purge_of_one_author_leaves_the_files_of_the_other"]),
    "purge_all_ignores_the_agent_option": (
        "board", '    die "--all and --agent exclude each other: --all means every agent, --agent names one"', "    :",
        [C + "ListAndPurge.test_all_and_agent_exclude_each_other"]),
    # ---- the hook
    "the_hook_fails_the_agent_on_an_error": (
        "hook", "        code = 0\n", "        code = 1\n", [H + "NeverBlocks.test_an_exception_inside_the_hook_is_exit_zero"]),
    "the_hook_returns_the_helpers_status": (
        "hook", "    run_helper(args, deadline)\n    return 0", "    rc = run_helper(args, deadline)\n    return 1 if rc else 0",
        [H + "NeverBlocks.test_a_helper_that_fails_is_exit_zero"]),
    "the_hook_records_without_the_switch": (
        "hook", '    if os.environ.get("BOARD_ACTIVITY") != "1" or not AUTHOR_RX.fullmatch(author):',
        "    if not AUTHOR_RX.fullmatch(author):", [H + "Gate.test_it_is_off_unless_board_activity_is_1"]),
    "the_whole_command_line_is_stored": (
        "hook", '    return prog + (" [%s]" % cls if cls and prog != "git" else "")', "    return command.strip()[:64]",
        [H + "Events.test_post_tool_use_of_a_shell_command_keeps_only_the_first_word", H + "Labels.test_command_labels"]),
    "file_content_is_stored": (
        "hook", "                return path_label(value)", '                return path_label(value) + " " + str(tool_input.get("old_string", ""))[:20]',
        [H + "Events.test_post_tool_use_of_a_file_tool"]),
    "control_characters_are_kept": (
        "hook", "        if code < 0x20 or code == 0x7F:\n            continue\n", "", [H + "Labels.test_path_labels", H + "Labels.test_clean_keeps_printable_ascii_only"]),
    "non_ascii_is_kept": (
        "hook", '        out.append(ch if code < 0x7F else "_")', "        out.append(ch)", [H + "Labels.test_path_labels"]),
    "the_whole_path_is_kept": (
        "hook", 'for p in parts[-2:])', "for p in parts)", [H + "Labels.test_path_labels"]),
    "a_program_name_may_be_a_pasted_token": (
        "hook", "{0,15}", "{0,63}", [H + "Labels.test_command_labels"]),
    "a_query_string_stays_in_the_path_label": (
        "hook", '    value = re.split(r"[?#]", value, maxsplit=1)[0]\n', "", [H + "Labels.test_path_labels"]),
    "the_hook_kills_the_helper_without_asking_it_to_stop": (
        "hook", "for sig, grace in ((signal.SIGTERM, 0.3), (signal.SIGKILL, 1.0)):", "for sig, grace in ((signal.SIGKILL, 1.0),):",
        [H + "KilledWrite.test_a_killed_write_leaves_no_temporary_files"]),
    "the_hook_waits_for_ever_for_the_helper": (
        "hook", "        return proc.wait(timeout=max(0.2, deadline - time.monotonic()))", "        return proc.wait()",
        [H + "NeverBlocks.test_a_helper_that_hangs_is_stopped_within_the_budget"]),
    "only_the_leader_of_a_hung_helper_is_killed": (
        "hook", "                    os.killpg(proc.pid, sig)", "                    os.kill(proc.pid, sig)",
        [H + "NeverBlocks.test_the_helper_and_its_children_are_gone_after_a_timeout"]),
    "the_input_is_not_size_limited": (
        "hook", "        if total > limit:\n            return None\n", "", [H + "Hostile.test_oversize_input_is_refused_without_a_call"]),
    "the_input_has_no_deadline": (
        "hook", ("        if left <= 0:\n            return None\n", "min(left, 0.25)"), ("        if False:\n            return None\n", "0.25"),
        [H + "NeverBlocks.test_a_stdin_that_never_ends_does_not_hang_the_hook"]),
    "the_hook_prints_on_stdout": (
        "hook", "    args = build_args(payload, author)\n", '    args = build_args(payload, author)\n    print("recorded", flush=True)\n',
        [H + "Events.test_post_tool_use_of_a_file_tool"]),
    "the_session_name_is_not_cleaned": (
        "hook", '        value = re.sub(r"[^A-Za-z0-9._-]", "", value)[:64]', "        value = value[:64]",
        [H + "Events.test_the_session_name_is_made_safe"]),
    "the_tool_name_is_not_validated": (
        "hook", "return value if isinstance(value, str) and TOOL_RX.fullmatch(value) else \"other\"",
        'return value if isinstance(value, str) else "other"', [H + "Events.test_the_tool_name_is_validated"]),
    "an_old_fence_file_still_links_a_task": (
        "hook", "or now - fst.st_mtime > 7 * 86400:\n            continue", ":\n            continue",
        [H + "TaskLink.test_the_newest_fence_wins_and_a_leftover_week_old_file_does_not"]),
    "nan_is_accepted_in_the_input": (
        "hook", "        obj = json.loads(text, parse_constant=_reject_constant)", "        obj = json.loads(text)",
        [H + "Hostile.test_garbage_input_is_skipped_quietly"]),
    # ---- the runner
    "the_runner_publishes_without_the_switch": (
        "run", 'if os.environ.get("BOARD_ACTIVITY") != "1":', "if False:", [R + "Publishing.test_nothing_is_published_unless_the_switch_is_one"]),
    "a_long_task_is_not_refreshed": (
        "run", "if self.on_beat and not self.stop_event.is_set():", "if False:", [R + "Publishing.test_a_long_task_is_refreshed_at_each_heartbeat"]),
    "every_finished_child_is_published_as_ok": (
        "run", 'outcome="ok" if outcome in ("done", "released on shutdown") else "error"', 'outcome="ok"',
        [R + "Publishing.test_the_outcome_of_a_failed_child_is_error"]),
    "the_runner_label_is_not_cleaned": (
        "run", 'return re.sub(r"[^A-Za-z0-9._-]", "", name)[:48] or "command"', "return name",
        [R + "Publishing.test_a_command_with_odd_characters_is_cleaned"]),
    # ---- the console
    "console:console/server.py|an_answer_over_the_cap_is_sent": (
        "console", "            if len(body) > MAX_BODY:", "            if False:",
        ["test_activity.ServerTests.test_an_answer_over_the_cap_is_an_error_not_a_download",
         "test_server.SizeCapTests.test_oversized_answer_becomes_a_500_not_a_huge_download"]),
    "console:console/server.py|the_agent_limit_is_not_bounded": (
        "console", '"limit": v_int(1, AGENT_LIMIT_DEFAULT),', '"limit": v_int(1, 10**9),',
        ["test_activity.ServerTests.test_filters_and_cursor_are_validated_strictly"]),
    "console:console/service.py|rows_are_not_cleaned": (
        "console", "sessions = [r for r in (act.clean_session(x) for x in raw) if r is not None]", "sessions = list(raw)",
        ["test_activity.ServiceTests.test_hostile_rows_never_reach_the_snapshot"]),
    "console:console/service.py|an_activity_error_breaks_the_board": (
        "console", "        except SourceError as e:\n            self._activity_retry_at", "        except KeyError as e:\n            self._activity_retry_at",
        ["test_activity.ServiceTests.test_an_error_reading_activity_does_not_break_the_board_and_keeps_what_was_read"]),
    "console:console/source.py|a_missing_table_is_an_error": (
        "console", "            if self._activity_missing(e):", "            if False:",
        ["test_activity.SqlTests.test_missing_tables_and_refused_reads_are_not_errors_for_the_board"]),
    "console:console/activity.py|stuck_without_a_live_lease": (
        "console", '            state = "stuck?" if healthy else "quiet"', '            state = "stuck?"',
        ["test_activity.States.test_stuck_needs_a_live_lease"]),
    "console:console/activity.py|the_quiet_limit_is_off_by_one": (
        "console", "        elif age <= quiet_s:", "        elif age < quiet_s:", ["test_activity.States.test_a_task_holder_by_age_of_the_last_report"]),
    "console:console/activity.py|the_working_window_is_off_by_one": (
        "console", 'if age <= WORKING_S and cur["state"] == "working":', 'if age < WORKING_S and cur["state"] == "working":',
        ["test_activity.States.test_a_task_holder_by_age_of_the_last_report"]),
    "console:console/activity.py|the_strip_is_shifted": (
        "console", "idx = minutes - 1 - age // STRIP_BUCKET_S", "idx = minutes - age // STRIP_BUCKET_S", ["test_activity.Strip.test_buckets_by_minute_with_the_write_weighted_by_its_calls"]),
    "console:console/activity.py|long_gaps_count_as_work": (
        "console", "            if 0 <= b - a < GAP_S:", "            if 0 <= b - a:",
        ["test_activity.Detail.test_the_active_time_estimate_sums_gaps_below_60_seconds_per_session"]),
    "console:console/activity.py|the_log_in_memory_is_unbounded": (
        "console", "    return out[-LOG_KEEP:]", "    return out", ["test_activity.Cleaning.test_the_log_kept_in_memory_is_bounded"]),
    "console:console/activity.py|a_repeated_row_is_added_twice": (
        "console", '    out = old + [r for r in new if r["id"] not in held]', "    out = old + new",
        ["test_activity.Cleaning.test_merge_keeps_ascending_ids_and_drops_repeats"]),
    "console:console/activity.py|a_task_not_watched_from_its_start_counts": (
        "console", 'if not rows or rows[0]["at"] > f.first_claim_ts or rows[-1]["at"] < f.done_ts:', "if not rows:",
        ["test_activity.PerTask.test_a_task_the_console_did_not_watch_from_the_start_is_left_out_not_counted_as_zero"]),
    "console:console/activity.py|an_ended_session_holding_a_task_is_quiet": (
        "console", '    if cur["state"] == "ended":\n        state = "idle"', '    if False:\n        state = "idle"',
        ["test_activity.States.test_an_ended_session_is_never_quiet_or_stuck_and_a_finished_turn_is_not_working"]),
    "console:console/activity.py|a_finished_turn_reads_working": (
        "console", 'if age <= WORKING_S and cur["state"] == "working":', "if age <= WORKING_S:",
        ["test_activity.States.test_an_ended_session_is_never_quiet_or_stuck_and_a_finished_turn_is_not_working"]),
    "console:console/activity.py|invisible_characters_are_kept": (
        "console", "\\u200b-\\u200f", "", ["test_activity.Cleaning.test_invisible_and_reordering_characters_are_stripped_from_text"]),
    "console:console/service.py|rows_read_this_refresh_are_judged_at_once": (
        "console", ('            held = [r for r in log_rows if r["id"] <= judged_up_to]', '            fresh_rows = [r for r in log_rows if r["id"] > judged_up_to]'),
        ('            held = list(log_rows)', '            fresh_rows = []'),
        ["test_activity.PurgeTests.test_a_row_that_arrives_before_its_session_row_is_kept"]),
    "console:console/activity.py|the_word_joiner_is_kept": (
        "console", "\\u2060", "", ["test_activity.Cleaning.test_invisible_and_reordering_characters_are_stripped_from_text"]),
    "console:console/activity.py|purged_rows_stay_in_the_copy": (
        "console", "    return keep", "    return log_rows", ["test_activity.PurgeTests.test_a_purged_agents_rows_leave_the_log_the_agent_screen_and_the_diagnostic"]),
    "console:console/service.py|the_purge_check_is_not_made": (
        "console", "            log_rows = act.drop_purged(held, sessions, len(raw) >= ACTIVITY_SESSIONS) + fresh_rows", "            log_rows = log_rows",
        ["test_activity.PurgeTests.test_a_purged_agents_rows_leave_the_log_the_agent_screen_and_the_diagnostic"]),
    "console:console/service.py|a_cut_session_list_drops_rows": (
        "console", "len(raw) >= ACTIVITY_SESSIONS)", "False)", ["test_activity.PurgeTests.test_a_cut_session_list_says_nothing_about_absent_sessions"]),
    "console:console/activity.py|too_few_tasks_show_a_number": (
        "console", "        if n < MIN_TASKS:", "        if n < 0:", ["test_activity.PerTask.test_too_few_tasks_show_no_number"]),
    "console:console/demo.py|the_demo_ring_is_not_applied": (
        "console", "        rows = rows[-ACT_RING:]\n", "", ["test_activity.DemoTests.test_rows_have_the_promised_shape_and_the_ring_limit"]),
    "console:console/static/app.js|board_text_is_rendered_as_markup": (
        "console", '        else if (k === "text") el.textContent = String(v);', '        else if (k === "text") el.innerHTML = String(v);',
        ["test_static.ShippedFilesTests.test_app_js_has_no_dangerous_construct"]),
}


def target_of(spec):
    return spec[0]


def run_tests(kind, path, tests, cwd):
    env = dict(os.environ)
    if kind == "board":
        env["BOARD_UNDER_TEST"] = path
    elif kind == "hook":
        env["ACTIVITY_HOOK_UNDER_TEST"] = path
    elif kind == "run":
        env["BOARD_RUN_UNDER_TEST"] = path
    return subprocess.run([sys.executable, "-m", "unittest", *tests], cwd=cwd, env=env, capture_output=True, text=True,
                          timeout=600)


class Mutations(unittest.TestCase):
    def source_of(self, name):
        kind = MUTATIONS[name][0]
        if kind == "board":
            return os.path.join(BOARD_DIR, "board")
        if kind == "hook":
            return os.path.join(BOARD_DIR, "hooks", "activity-hook.py")
        if kind == "run":
            return os.path.join(BOARD_DIR, "board-run")
        return os.path.join(CONSOLE_DIR, name.split(":", 1)[1].split("|")[0])

    def prepare(self, name):
        """The mutated copy. Returns (kind, path of the mutated file, directory to run the tests in)."""
        kind, old, new, _ = MUTATIONS[name]
        with open(self.source_of(name)) as f:
            text = f.read()
        for o, n in zip(old if isinstance(old, tuple) else (old,), new if isinstance(new, tuple) else (new,)):
            self.assertEqual(text.count(o), 1, "the text to mutate must occur exactly once in %s: %r" % (name, o))
            text = text.replace(o, n)
        d = tempfile.mkdtemp(prefix="activity-mut-")
        self.addCleanup(shutil.rmtree, d, True)
        if kind in ("board", "hook", "run"):
            # the helper reads schema.sql and board-run next to itself; the tests run from the real tests directory
            name_in = {"board": "board", "hook": "activity-hook.py", "run": "board-run"}[kind]
            path = os.path.join(d, name_in)
            with open(path, "w") as f:
                f.write(text)
            os.chmod(path, 0o755)
            if kind == "board":
                shutil.copy(os.path.join(BOARD_DIR, "schema.sql"), d)
                shutil.copy(os.path.join(BOARD_DIR, "board-run"), d)
            return kind, path, HERE
        tree = os.path.join(d, "console")
        shutil.copytree(CONSOLE_DIR, tree, ignore=shutil.ignore_patterns("__pycache__"))
        rel = name.split(":", 1)[1].split("|")[0]
        with open(os.path.join(tree, rel), "w") as f:
            f.write(text)
        return kind, "", os.path.join(tree, "tests")

    def test_the_unmutated_code_passes_the_same_tests(self):
        every = {}
        for name, (kind, _, _, tests) in MUTATIONS.items():
            every.setdefault("console" if kind == "console" else "local", set()).update(tests)
        for group, tests in every.items():
            cwd = os.path.join(CONSOLE_DIR, "tests") if group == "console" else HERE
            r = run_tests("none", "", sorted(tests), cwd)
            self.assertEqual(r.returncode, 0, r.stderr[-3000:])

    def test_every_planted_fault_is_caught(self):
        # Each run uses its own temporary directories, so a few can go at once.
        prepared = {name: self.prepare(name) for name in MUTATIONS}
        with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:
            futures = {name: pool.submit(run_tests, prepared[name][0], prepared[name][1], MUTATIONS[name][3], prepared[name][2])
                       for name in MUTATIONS}
        survivors = [name for name, fut in futures.items() if fut.result().returncode == 0]
        self.assertEqual(survivors, [], "these planted faults were not caught by their tests")


if __name__ == "__main__":
    unittest.main()

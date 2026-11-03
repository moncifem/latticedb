"""Planted faults in `board act`: every one must make its named tests fail.

For each fault this copies the helper, replaces one piece of text that must occur exactly once, and runs the named
tests of test_act.py against the copy (BOARD_UNDER_TEST). The fault is caught when that run fails. A fault nobody
catches means a rule of the decision path is not pinned by a test. The expected values in test_act.py come from
the rules (CONVENTIONS.md, "board act"), not from running the helper.

Run: python3 -m unittest discover -s integrations/board/tests -p "test_act_mutations.py"
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

A = "test_act.Approve."
R = "test_act.Reject."
C = "test_act.Cancel."
Q = "test_act.Requeue."
N = "test_act.Note."
I = "test_act.Idempotency."
S = "test_act.CompareAndSet."
V = "test_act.Values."
D = "test_act.Identity."
G = "test_act.Arguments."

# name: (text that occurs once, replacement, tests that must catch it)
MUTATIONS = {
    "the_creator_may_approve_their_own_task": (
        """        if [ "$act_who" = "$AUTHOR" ] || [ "$act_who" = "$plain" ]; then act_refuse self_approval; fi""",
        "        :",
        [A + "test_the_creator_cannot_approve_under_either_name", A + "test_the_owner_cannot_approve",
         R + "test_the_creator_cannot_reject_either"]),
    "the_plain_name_of_the_person_is_not_compared": (
        """ || [ "$act_who" = "$plain" ]; then act_refuse self_approval; fi""", "; then act_refuse self_approval; fi",
        [A + "test_the_creator_cannot_approve_under_either_name", A + "test_the_owner_cannot_approve"]),
    "a_name_that_wrote_events_may_approve": (
        """      [ -z "$OUT" ] || act_refuse self_approval\n""", "      :\n",
        [A + "test_a_name_that_wrote_an_event_on_the_task_cannot_approve"]),
    "the_events_of_the_person_are_looked_up_under_the_wrong_name": (
        """-v task="$task" -v plain="$plain" || fail_db""", """-v task="$task" -v plain="$AUTHOR" || fail_db""",
        [A + "test_a_name_that_wrote_an_event_on_the_task_cannot_approve"]),
    "the_transaction_does_not_repeat_the_self_approval_test": (
        """      act_cond="$act_cond AND created_by <> :'author' AND created_by <> :'plain' AND coalesce(owner, '') <> :'author' AND coalesce(owner, '') <> :'plain'"\n""",
        "      :\n", [A + "test_the_transaction_repeats_the_self_approval_test_against_the_row_it_writes"]),
    "text_that_changed_is_approved": (
        """  if [ "$action" = approve ] && [ "$hash" != "$act_sha" ]; then act_refuse stale; fi""", "  :",
        [A + "test_text_that_changed_since_it_was_shown_is_refused"]),
    "an_approval_needs_no_hash": (
        """    approve) [ -n "$hash" ] || act_refuse hash_required ;;""", "    approve) ;;",
        [A + "test_an_approval_needs_the_hash"]),
    "a_malformed_hash_is_accepted": (
        """    [ ${#hash} -eq 64 ] || die "act: --hash must be 64 lower case hex digits\"""", "    :",
        [A + "test_the_hash_must_be_64_lower_case_hex_digits"]),
    "the_hash_leaves_out_the_newline": (
        """  sha256_text "$act_title$NL$act_body\"""", """  sha256_text "$act_title$act_body\"""",
        [A + "test_the_hash_is_computed_from_title_newline_body_as_bytes", A + "test_an_empty_body_hashes_as_title_and_a_newline",
         A + "test_an_approval_opens_the_task_and_records_who_what_and_the_hash"]),
    "a_rejection_needs_no_comment": (
        """    reject | note) [ -n "$comment" ] || act_refuse comment_required ;;""", "    reject | note) ;;",
        [R + "test_a_rejection_needs_a_comment", N + "test_a_note_needs_a_comment"]),
    "approve_does_not_need_the_task_to_wait": (
        """    approve | reject) [ "$act_state" = awaiting_approval ] || act_refuse wrong_state ;;""", "    approve | reject) ;;",
        [A + "test_only_a_task_waiting_for_approval_can_be_approved", R + "test_only_a_task_waiting_for_approval_can_be_rejected"]),
    "a_finished_task_can_be_cancelled": (
        """    cancel) case $act_state in open | claimed | awaiting_approval) ;; *) act_refuse wrong_state ;; esac ;;""", "    cancel) ;;",
        [C + "test_cancel_does_not_apply_to_finished_tasks"]),
    "an_open_task_can_be_requeued": (
        """    requeue) case $act_state in dead | claimed) ;; *) act_refuse wrong_state ;; esac ;;""", "    requeue) ;;",
        [Q + "test_requeue_does_not_apply_to_other_states"]),
    "a_requeue_keeps_the_attempts": (
        """UPDATE task_leases SET attempts = 0, lease_until = 0, not_before = 0, last_error = '' WHERE task_id = :'task';""",
        """UPDATE task_leases SET lease_until = 0 WHERE task_id = :'task';""",
        [Q + "test_a_dead_task_opens_again_with_its_attempts_reset"]),
    "a_cancel_keeps_the_lease": (
        """UPDATE task_leases SET lease_until = 0 WHERE task_id = :'task';"
      ;;
    requeue)""", """:"
      ;;
    requeue)""", [C + "test_cancel_applies_to_open_claimed_and_waiting_tasks"]),
    "the_lease_row_is_not_made_first": (
        """    cancel | requeue) ensure_lease "$task" ;;""", "    cancel | requeue) ;;",
        [C + "test_cancel_makes_sure_the_lease_row_exists_first"]),
    "a_note_changes_the_task": (
        """    note) act_update=''; act_cond="title = :'title'" ;;""",
        """    note) act_update="UPDATE tasks SET updated_at = :ts WHERE id = :'task';"; act_cond="title = :'title'" ;;""",
        [N + "test_a_note_is_an_event_and_changes_no_task_row"]),
    "a_finished_task_can_change_priority": (
        """    priority) case $act_state in done | cancelled) act_refuse wrong_state ;; esac ;;""", "    priority) ;;",
        [ "test_act.Priority.test_finished_tasks_keep_their_priority"]),
    "a_repeat_is_not_recognised": (
        """    if [ "$(printf '%s\\n' "$OUT" | cut -f1-3)" = "$task$TAB$AUTHOR$TAB$action" ]; then""", "    if false; then",
        [I + "test_the_same_request_again_changes_nothing_and_names_the_event"]),
    "a_request_id_may_be_used_for_another_decision": (
        """    act_refuse request_reused\n""", "    :\n", [I + "test_a_request_id_used_for_another_decision_is_refused"]),
    "the_repeat_lookup_result_is_ignored": (
        """  psql_run tuples "SELECT task_id, author, kind, event_id FROM human_actions WHERE request_id = :'req';" \\
    -v req="$req" || fail_db
  if [ -n "$OUT" ]; then""", """  psql_run tuples "SELECT task_id, author, kind, event_id FROM human_actions WHERE request_id = :'req';" \\
    -v req="$req" || fail_db
  OUT=''
  if [ -n "$OUT" ]; then""", [I + "test_the_same_request_again_changes_nothing_and_names_the_event"]),
    "the_decision_is_not_recorded_with_the_change": (
        """INSERT INTO human_actions (request_id, task_id, author, kind, body_sha, remark, event_id, created_at)
  VALUES (:'req', :'task', :'author', :'action', :'sha', :'comment', :seq, :ts);\n""", "",
        [I + "test_the_row_is_written_in_the_same_transaction_as_the_change_and_the_event"]),
    "the_change_ignores_what_was_read": (
        """  act_cond="state = :'state' AND title = :'title' AND body = :'body'\"""", """  act_cond="state = :'state'\"""",
        [S + "test_every_change_is_conditional_on_the_state_and_text_that_were_read"]),
    "a_lost_race_is_reported_as_done": (
        """  [ "$(printf '%s\\n' "$OUT" | cut -f1)" = t ] || act_refuse changed""", "  :",
        ["test_act.Idempotency.test_a_decision_that_lost_a_race_writes_nothing_and_says_changed"]),
    "text_on_several_lines_is_hashed_anyway": (
        """  case $OUT in *"$NL"*) act_refuse unreadable ;; esac""", "  :",
        [V + "test_task_text_on_several_lines_cannot_be_approved"]),
    "comments_may_have_any_characters": (
        """  need_line comment "$comment" 0""", "  :", [V + "test_comments_must_be_one_clean_line"]),
    "a_missing_task_is_not_noticed": (
        """  [ -n "$OUT" ] || act_refuse not_found""", "  :", [V + "test_a_missing_task"]),
    "an_agent_may_use_act": (
        """    *) die "act: BOARD_AUTHOR must be human.NAME (a person); got a name without the human. prefix" ;;""", "    *) ;;",
        [D + "test_only_a_human_name_may_act"]),
    "the_human_namespace_is_open_to_every_command": (
        """    *) case $AUTHOR in human.*) die "BOARD_AUTHOR human.* is reserved for 'board act'; use another name for $cmd" ;; esac ;;""",
        "    *) ;;", [D + "test_every_other_command_refuses_the_human_namespace"]),
    "any_action_is_accepted": (
        """    *) die "act: the action must be approve, reject, cancel, requeue, priority or note" ;;""", "    *) ;;",
        [G + "test_the_action_list_is_closed"]),
    "request_ids_of_any_length": (
        """  { [ ${#req} -ge 8 ] && [ ${#req} -le 64 ]; } || die "act: --request-id must be 8 to 64 letters, digits, '_' or '-'\"""", "  :",
        [G + "test_request_ids_are_8_to_64_plain_characters"]),
    "request_ids_of_any_characters": (
        """    '' | *[!A-Za-z0-9_-]*) die "act: --request-id must be 8 to 64 letters, digits, '_' or '-'" ;;""", "    '') die \"act: --request-id is required\" ;;",
        [G + "test_request_ids_are_8_to_64_plain_characters"]),
    "values_are_written_into_the_sql": (
        """  tx "$sql" -v author="$AUTHOR" -v plain="$plain" -v task="$task" -v state="$act_state" -v title="$act_title" \\""",
        """  sql=$(printf '%s\\n' "$sql" | sed "s/:'comment'/'$comment'/")
  tx "$sql" -v author="$AUTHOR" -v plain="$plain" -v task="$task" -v state="$act_state" -v title="$act_title" \\""",
        [V + "test_hostile_text_reaches_psql_only_as_variables"]),
    "a_priority_may_be_anything": (
        """    need_int priority "$O_PRIORITY"\n    newprio=$O_PRIORITY""", "    newprio=$O_PRIORITY",
        ["test_act.Priority.test_it_needs_a_number"]),
}


def run_tests(path, tests):
    env = dict(os.environ)
    env["BOARD_UNDER_TEST"] = path
    return subprocess.run([sys.executable, "-m", "unittest", *tests], cwd=HERE, env=env, capture_output=True, text=True, timeout=600)


class Mutations(unittest.TestCase):
    def prepare(self, name):
        old, new, _ = MUTATIONS[name]
        with open(os.path.join(BOARD_DIR, "board")) as f:
            text = f.read()
        self.assertEqual(text.count(old), 1, "the text to mutate must occur exactly once in %s: %r" % (name, old))
        d = tempfile.mkdtemp(prefix="act-mut-")
        self.addCleanup(shutil.rmtree, d, True)
        path = os.path.join(d, "board")
        with open(path, "w") as f:
            f.write(text.replace(old, new))
        os.chmod(path, 0o755)
        shutil.copy(os.path.join(BOARD_DIR, "schema.sql"), d)
        shutil.copy(os.path.join(BOARD_DIR, "board-run"), d)
        return path

    def test_the_unmutated_helper_passes_the_same_tests(self):
        tests = sorted({t for _, _, ts in MUTATIONS.values() for t in ts})
        r = run_tests(os.path.join(BOARD_DIR, "board"), tests)
        self.assertEqual(r.returncode, 0, r.stderr[-3000:])

    def test_every_planted_fault_is_caught(self):
        prepared = {name: self.prepare(name) for name in MUTATIONS}
        with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:
            futures = {name: pool.submit(run_tests, prepared[name], MUTATIONS[name][2]) for name in MUTATIONS}
        survivors = [name for name, fut in futures.items() if fut.result().returncode == 0]
        self.assertEqual(survivors, [], "these planted faults were not caught by their tests")


if __name__ == "__main__":
    unittest.main()

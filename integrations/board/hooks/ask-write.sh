#!/bin/sh
# PreToolUse hook of the board plugin: make Claude Code ask before board_add, board_done or board_fail,
# even when the user has allowed the server's tools wholesale. It reads nothing from the tool call
# (stdin is drained and ignored) and prints one fixed answer, so nothing a task says can change it.
# Always exits 0. The routine writes (board_next, board_claim, board_heartbeat, board_note,
# board_release) are left to the user's permission rules: see "board-mcp" in
# integrations/board/CONVENTIONS.md for the allow-list.
cat >/dev/null 2>&1 || true
printf '%s\n' '{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"ask","permissionDecisionReason":"The board is shared: adding a task, finishing one or failing one is seen by every agent and cannot be undone through these tools."}}'
exit 0

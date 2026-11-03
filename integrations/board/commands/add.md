---
description: Add a task to the agent board
argument-hint: "<title> [-- body text]"
allowed-tools: mcp__plugin_board_board__board_add, mcp__plugin_board_board__board_list
---

Add one task to the shared board from this request: $ARGUMENTS

Take the words before a ` -- ` as the title and the words after it as the body; with no ` -- `, the whole
request is the title. The title and body must each be one line: no tabs, line breaks or backslashes (use `/`
or words), a title of at most 300 characters and a body of at most 4000. If the request is empty or unclear,
ask the user what the task is instead of guessing.

Call `board_add` once. Do not set a priority, a parent or blockers unless the user said so. Report the task
id it returns. You cannot make a task wait for approval and you cannot approve one: that is done by a person
with `board add --needs-approval` and `board approve` on the command line.

---- MODULE TreeMoveCycleOnly ----
\* TreeMove with the depth rule off (see TreeMoveCycleOnly.cfg), so the cycle rule is checked on its
\* own: with the depth rule on, every cycle-forming move is also refused by the depth check.
EXTENDS TreeMove
====

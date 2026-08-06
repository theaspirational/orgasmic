# TASK-QGWK7 verify artifact

Pins: after `dispatch-close --worktree-remove`, the worker report is still
readable from the path the close tx names
(`.orgasmic/dispatch-records/<started_tx>/last.txt`).

The injection restores delete-at-close (no promote). The red run's first
failing assertion is the pinned readability message.

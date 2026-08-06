# TASK-QGWK7.1 verify artifact

Pins F-1: after `dispatch-close` promotes a report, the destination directory
is in the git index — durability no longer depends on which `git add` form a
manager happens to use.

The injection no-ops `stage_promoted_dispatch_record`. The red run's first
failing assertion is the pinned index message.

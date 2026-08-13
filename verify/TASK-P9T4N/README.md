# verify/TASK-P9T4N — atomic dispatch close

The production-path test runs the real CLI against a real daemon through a
proxy that rejects the legacy standalone task-state endpoint. Green therefore
requires dispatch-close to use the combined daemon endpoint. It then reads the
ledger and requires the terminal tx to precede the lifecycle tx.

`injection.patch` makes the writer's multi append emit only its first prepared
entry. The injected run leaves the close tx on disk without
`task.state_transitioned`, so the ledger assertion fails with the pinned
signature in `expect-red`.

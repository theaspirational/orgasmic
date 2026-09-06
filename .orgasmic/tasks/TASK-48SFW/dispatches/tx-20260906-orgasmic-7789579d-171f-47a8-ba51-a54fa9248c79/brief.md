Bounded TASK-48SFW follow-up. Reuse the retained implementation worktree and its own target. Base 4cc676cd contains your prior implementation; P0 diagnostic is independently accepted and stays untouched. Risky tier remains declared. Implementer stdio/codex gpt-5.5 high; independent Claude review follows the committed fix.

Read /tmp/rev-0Y363-48SFW/review.md (exact prior reviewed range 18d170d2..4274d3e0). Fix F1, F2, F3 and the matching-owner test gap, scoped to daemon_service.rs and directly necessary existing tests. No new framework/dependency.

F1: a positively not-found systemd service with an on-disk unit left by failed installation must allow the matching legitimate owner, while refusing foreign or unreadable/ambiguous ownership. Reuse the existing unit Environment reader for this recovery case. Query errors remain unknown. Do not add unrelated file deletion/installation rollback: deleting a preexisting unit after failure risks data loss and is unnecessary to fix owner discovery.

F2: parse only the actual launchctl job environment block, ignoring inherited/default environment blocks. Preserve refusal for missing/empty/duplicate/ambiguous actual owner. Update fake query output to the real block shape. Add a matching-owner discovery check with a nondefault home and inherited/default ORGASMIC_HOME entries, plus the existing loaded/disk agreement guard.

F3: support valid binary plists through native plutil, while preserving malformed and duplicate-XML-key refusal. Do not drop existing ambiguity validation to obtain a green binary-plist test. Small native conversion fallback is sufficient.

Additional concrete parent finding in the same reader: systemd_unescape pushes UTF-8 bytes as individual chars, corrupting non-ASCII paths. Preserve UTF-8 for literal and hex-escaped bytes with the smallest decoder adjustment; invalid decoding must remain an error/unknown, not a lossy matching owner. Include one focused Unicode owner example.

Do NOT relax ORGASMIC_RUN_ID lifecycle prohibition (review F4). The assignment explicitly requires it. The P0 experiment proves a Down/unanswered fresh probe does not establish that the daemon is dead, so an autostart exception based on that probe is unsafe. No new force/ownership override. No P0 test edits or production FD recovery changes.

Write minimal behavioral regression tests for these concrete reader failures first and run the relevant filters to observe assertions failing on existing code; then implement and run GREEN using scripts/run-tests.sh, redirected logs, default billed skip and syspolicyd watchdog. No broad suite, cargo test --workspace, broad clippy/build repetition, piped test output or duplicate runs. The prior final suite already passed all but the two baseline failures, and those two repaired tests passed. Use CARGO_BUILD_JOBS=2 and this worktree's own target. Do not mutate real launchctl/systemctl/schtasks services, restart/update the operator daemon, invoke raw provider CLIs, push, deploy or perform provider/quota probes.

Finish with committed SHA, concise final report including exact red/green test evidence and remaining platform limits, and orgasmic dispatch finalize bound to this generation. Evidence prefix p1-fix- under /Users/aspirational/.codex/artifacts/orgasmic-recovery-20260906. Parent owns review/merge/closure.

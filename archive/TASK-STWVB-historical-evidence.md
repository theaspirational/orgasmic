# TASK-STWVB historical evidence audit

Researched 2026-09-07. Read-only investigation of code history and original tool outputs; no suite execution, induced load, daemon restart, or ledger changes.

The historical evidence supports **host-sensitive process progress against fixed test deadlines**. It does not establish that Gatekeeper caused the remaining registered failure, or that every failure originally grouped into this task had one cause. The strongest recent evidence is narrower than the registry's explanation: two concurrent tests passed slowly and finished in the same second, without instrumentation locating the delay.

## September 2: the original measurement, not the registry paraphrase

The original measurement survives in the [FLAKES session's raw tool call](/Users/aspirational/.claude/projects/-Users-aspirational-Documents-code-tools-orgasmic/25219e20-4403-4221-9eb2-cd596e21be27/subagents/agent-aflakes-98af85069bf40dc4.jsonl:109). It runs three exact tests sequentially in each of two concurrent loops, ten iterations per loop, under eight `yes` processes. Each invocation starts a new instance of the same daemon test executable. Durations come from libtest's final `test result` line, not from phase timing.

The [captured results](/Users/aspirational/.claude/projects/-Users-aspirational-Documents-code-tools-orgasmic/25219e20-4403-4221-9eb2-cd596e21be27/subagents/agent-aflakes-98af85069bf40dc4.jsonl:125) show the child-PID test at **41.54 seconds** and **43.40 seconds**, against 0.29–0.57 seconds in the other eighteen executions. Both loops were green. The [log modification times](/Users/aspirational/.claude/projects/-Users-aspirational-Documents-code-tools-orgasmic/25219e20-4403-4221-9eb2-cd596e21be27/subagents/agent-aflakes-98af85069bf40dc4.jsonl:131) put the two slow logs at **12:49:28**. Start times were inferred from duration and modification time; no timestamp recorded entry into the readiness loop.

A [system-log query was attempted](/Users/aspirational/.claude/projects/-Users-aspirational-Documents-code-tools-orgasmic/25219e20-4403-4221-9eb2-cd596e21be27/subagents/agent-aflakes-98af85069bf40dc4.jsonl:127), but its [captured output has no rows under the syspolicyd activity heading](/Users/aspirational/.claude/projects/-Users-aspirational-Documents-code-tools-orgasmic/25219e20-4403-4221-9eb2-cd596e21be27/subagents/agent-aflakes-98af85069bf40dc4.jsonl:128). Stderr was discarded. This is missing attribution evidence, not evidence that syspolicyd was inactive.

The [historical test source captured in that session](/Users/aspirational/.claude/projects/-Users-aspirational-Documents-code-tools-orgasmic/25219e20-4403-4221-9eb2-cd596e21be27/subagents/agent-aflakes-98af85069bf40dc4.jsonl:87) already uses `shared_test_executable()`. Its 30-second readiness timer begins after fixture acquisition and process spawn. The resolver runs after readiness succeeds. Therefore the 42-second passing durations cannot identify whether the delay was fixture initialization, spawn, readiness observation, resolver subprocesses, or cleanup. They do not reproduce the registered `fake cursor-agent did not start children` panic.

Commit `cfd1e83ba9074b00a263c0453c64d0b75a111d1e` turned these observations into the stronger host-wide-spawn-stall explanation now in [the registry](/Users/aspirational/Documents/code/tools/orgasmic/verify/flake-registry.toml:70). The synchronized long executions are real; the exact blocking component remains an inference.

## July and August: several different problems were grouped together

Historical commit `956cc5b981d9eb3352ac43e015a3c85a722afe0b` contains the July 26 task record in `.orgasmic/tasks/todo.org`. It reports an alive helper with no children, `S` state, and `0:00.00` CPU for 28+ seconds; an unrelated tiny script taking 6,205 / 2,993 / 63,771 / 64 / 354 ms during the suite; and idle first-versus-second-exec differences. I could locate these only as recorded prose, not their original July tool outputs. They are useful leads, not independently recovered raw measurements. In particular, rounded zero CPU accounting does not prove that literally no instruction executed.

That record also separates an rmux session leak, a run-ID reuse assertion, and a supervisor timeout-monitor race. Its claimed stable inventory changed after disk cleanup and subsequent fixes. The evidence does not support a universal shared-global-state diagnosis, but it also does not support declaring all historical reds environmental.

For August 2, original tool output does confirm that **system-policy activity and waits were real on this host**:

- A [captured stackshot analysis](/Users/aspirational/.claude/projects/-Users-aspirational-Documents-code-tools-orgasmic/c163951d-3d51-43d3-a886-cfc068b9124d.jsonl:168) shows syspolicyd threads in kernel read/write-lock waits and runnable threads.
- [System logs](/Users/aspirational/.claude/projects/-Users-aspirational-Documents-code-tools-orgasmic/c163951d-3d51-43d3-a886-cfc068b9124d.jsonl:681) show XProtect activation at 22:03:18 and a cached-scan update at 22:03:29. The logged path is private, so this excerpt alone does not tie the interval to the child-PID fixture.
- A [policy database census](/Users/aspirational/.claude/projects/-Users-aspirational-Documents-code-tools-orgasmic/c163951d-3d51-43d3-a886-cfc068b9124d.jsonl:819) lists 62 scans associated with an `orgasmic_daemon` test binary, plus scans of other build and test artifacts. This establishes scans of project artifacts, not which scan caused a particular test timeout.

Commit `3e5c72047925bd123f93229a5d3d27215467ec62` reports the 637/651 suite result, 659.90-second duration, 14/14 isolated passes in eight seconds, and syspolicyd at 586% CPU. Commit `512d636047e97545a0d5b75d993f8ccec60be0b0` corrects earlier claims that compilation was the principal trigger and that Developer Tools grants prevented the burst. I verified these historical records, but did not recover raw output for that exact 14-test comparison. They should be cited as historical reported measurements rather than a fresh reproduction.

## What can be concluded

The original registered signature identifies fixture readiness failing before the child-selection assertion is evaluated. Historical host contention is a plausible and well-supported trigger class. **Gatekeeper as the specific cause of the currently retained mode remains unproved.** September 2 cannot close that gap because it produced only long passing tests without phase or process-state captures. The current code's warmed fixture also means the original fresh-script explanation cannot simply be carried forward unchanged.

To distinguish the remaining possibilities on recurrence, capture phase timestamps plus owned helper/children state during the actual readiness failure, and correlate any external-policy claim to that same interval. Raising a timeout or accepting an isolation pass would not establish the missing mechanism.

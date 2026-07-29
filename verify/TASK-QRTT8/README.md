# verify/TASK-QRTT8 — a run whose harness resolved a credential mode is never rehydrated after a restart

`injection.patch` / `cmd` / `expect-red` are the replayable artifact:
`orgasmic verify TASK-QRTT8`.

## The defect

`api::boot_reattach_candidate` destructured

```rust
Ok(Lifecycle::RunMeta { …, credential_mode: None, driver_config, .. })
```

`credential_mode: None` is a **refutable pattern, not a binding**. A `RunMeta`
carrying `Some(mode)` does not match it. It falls to the loop's `_ => {}` arm,
`meta` stays `None`, and the `let (…) = meta?` below returns `None`: the run is
not a boot-reattach candidate **at all** — not skipped with a reason, not
logged, simply never seen.

Production writes `Some`. `adapters/claude.rs` puts the resolved mode into
`NativeRuntimeMeta`, and `Supervisor::acquire` lifts it into `RunMeta`
(TASK-S0QRM; asserted by `the_resolved_credential_mode_round_trips_through_run_meta`).
So a claude run that had resolved `native_login` or `bare_api_key` was silently
never rehydrated after a daemon restart.

Every pre-existing boot-reattach test passed `credential_mode: None`, which is
exactly why nothing caught it. Found by reading, under TASK-KPMFK.

## Why the injection is one line

Unlike TASK-KPMFK's two-half injection, this defect has one cause and one site.
Restoring the single literal reproduces it completely, and the three tests in
`cmd` fail for three different reasons — which is the point: the same one-line
pattern shape kills the field, the class, and everything downstream of the
function.

## What the command proves

1. **The pattern.** A `RunMeta` carrying each value the field can hold — `None`,
   `Some("native_login")`, `Some("bare_api_key")` — must yield a candidate with
   its reattach material intact. Stated over all three so the assertion fails
   again the moment the pattern narrows back to any one of them.

2. **The blast radius.** The fix makes a class of runs candidates that never
   reached `Supervisor::reattach` before, so the fix has to be shown safe and
   not merely correct. The class that exists in production today is
   `acp-stdio`/`claude`: the mux drivers all write `credential_mode: None`
   (`modes/tmux.rs`, `modes/rmux.rs`), so the claude *adapter* at
   `adapters/claude.rs` is the only writer of `Some(_)`, and it serves the
   stdio modes. Such a run's runtime is a child of the daemon process and is
   gone after a restart. The test asserts the decline is clean: the driver
   answers `NotReattachable`, the run does not enter the supervisor as live,
   and the lease `Supervisor::reattach` admits *before* it attaches is released
   again — a leaked reservation there would wedge the task's lease for the rest
   of the daemon's life. That check cannot even run pre-fix; it panics on the
   missing candidate.

3. **The TASK-KPMFK interaction.** KPMFK's stage-completion-watcher respawn is
   only as reachable as this function. A stage run whose `RunMeta` carries a
   resolved mode is dropped here first, so a `grill`/`plan`/`architect` live
   across a restart on a credential-resolving transport would emit no terminal
   tx — for a reason that has nothing to do with stages. The test is KPMFK's
   own rmux fixture, identical in every respect but the one field: a genuinely
   live rmux session, the real `reattach_live_runs_on_boot`, and a second,
   independent `ApiState`/`Supervisor` standing in for the post-restart daemon.

## Scope note on (3)

No mux transport writes `credential_mode: Some(_)` today — verified at merged
HEAD (`modes/tmux.rs:1039,1550,1609`, `modes/rmux.rs:1328`), which is KPMFK's
reading and holds. The fixture writes the field the durable format allows and
`TASK-S0QRM` documents as reattach material. That is deliberate: the test pins
the shape of the reader, so the day a mux driver starts recording the mode, the
stage path does not silently go dark again.

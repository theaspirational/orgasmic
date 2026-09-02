# verify/ — shipped injection proofs

`flake-registry.toml` also lives here, and for the same reason: it is the other
half of making red mean something. A proof says "this fix is a fix"; the
registry says "this failure is already owned, by this task, with this panic".
`scripts/run-tests.sh` reads it and prints a REAL-vs-FLAKE verdict, so the
question "is this red the known one?" has a machine answer instead of a
remembered one.

## Injection proofs

One directory per task: `verify/TASK-<id>/`. Each holds the proof that the
task's fix is a fix, authored **once, pre-fix, while the defect still
reproduced**, and replayed afterwards by `orgasmic verify TASK-<id>`.

| file | what it is |
| --- | --- |
| `injection.patch` | git patch that reintroduces the defect onto the fixed tree |
| `cmd` | the single command line that catches it, run from the repo root |
| `expect-red` | the pinned failure signature (`exit:`, `contains:`, `contains-any:`) |

`orgasmic verify --help` documents the file formats and the directive set.

## Why these are artifacts and not a procedure

Re-authoring a probe at merge time means writing it against the tree that is
already fixed — which is exactly where false green lives. A probe aimed at the
wrong extractor, the wrong mutex, the wrong lock, or run after the load that
reproduced the race has gone, all pass, and a passing broken probe is
indistinguishable from a working fix.

Replaying a pinned artifact cannot degrade that way. If the injection no longer
reintroduces the defect, or the command no longer exercises it, or the failure
no longer looks like the one that was pinned, the replay itself fails loudly
(`FALSE GREEN GUARD TRIPPED`) instead of quietly reporting success.

An artifact that no longer applies is a **failure**, not a skip: the proof has
gone stale and has to be re-authored against a defect that reproduces.
`orgasmic verify --all --check` lists every artifact's state in under a second
without running anything, and `scripts/run-tests.sh --check` runs the same
sweep beside the registry check; `orgasmic verify --all` is the full replay.

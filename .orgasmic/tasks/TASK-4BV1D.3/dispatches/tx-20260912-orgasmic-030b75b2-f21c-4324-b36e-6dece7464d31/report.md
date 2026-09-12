# Critique — claude · anthropic · opus[1m] · effort xhigh

## Participant

claude · anthropic · opus[1m] · effort xhigh. Independent critique, no other
participant's output seen. Target: `PLUGINS-SCOPE.md`, focus on section 13 as a
brief for a fresh implementing agent. Every file reference below was verified
against the worktree at `cf4566f6` — the same commit section 13 claims.

## Overall Assessment

Section 13 is unusually well-evidenced. I spot-checked roughly 55 of its file and
line citations and the large majority land within ±2 lines on the right symbol:
`api.rs:20186` really is `sync_handoff_goal_id`, `index.rs:3211` really is the
four-collection skip-list, `events.rs:19-51` really is the closed seven-variant
`Topic` with a hand-written `parse()`, `tauri.conf.json:32` really is
`"csp": null`, `ui/src/components/ui/` really holds exactly 26 primitives,
`retro.rs` really is 812 lines, and the "68% of ~26,000 test lines" claim checks
out (4,418 + 13,375 = 17,793). The blast-radius survey behind 13.2 was real work.

The defects are not in the survey. They are in four places:

1. **Two prerequisites for the whole plan are absent from the tree and absent
   from 13.3's "sixteen gaps, all verified absent."** `shipped/plugins/` is
   referenced nowhere in the repo, and `orgasmic update` hard-fails on a runtime
   that moves descriptors. Decision 10 and decision 33 both rest on these.
2. **Three decisions are stated as mechanical but are wire-format or security
   changes**: decision 12 (`artifactor` rename, slice S0, "no behavior change"),
   decision 21 (`NodeChanged` drops `project_id`), decision 23 (removing
   `ArtifactsRead` removes the only grant path to `Topic::Artifact`).
3. **A principle the document states twice is violated twice without
   acknowledgement**: no plugin-to-plugin dependencies (decision 14's own
   rationale) versus decisions 19 and 36.
4. **Path prefixes are inconsistent enough to misroute a fresh agent** — one
   cited filename exists in two crates, one is cited in the wrong crate, one is
   cited at the wrong directory depth, and one citation points into a
   `#[cfg(test)]` block.

A fresh agent following 13.7 in order would get through S0 and hit the first
blocking item inside S1.

## Findings

### blocking

**B1. `orgasmic update` refuses any runtime whose descriptors left
`shipped/schema/node-types/`. Every existing install loses auto-update.**

Location: 13.1 "Every node type ships as a plugin folder under
`shipped/plugins/<id>/`"; 13.8 trap "`orgasmic update` checks
`REQUIRED_RUNTIME_FILES`"; decision 33 "One breaking release. Runtime `0.1.0`".

The trap names the wrong mechanism. `REQUIRED_RUNTIME_FILES`
(`crates/orgasmic-cli/src/update.rs:22-37`) contains eight entries and **no**
node-type descriptor paths at all — section 2.1's "hardcodes the four descriptor
paths in `REQUIRED_RUNTIME_FILES`" is already stale. The live coupling is
stronger and sits immediately below it:

```rust
// crates/orgasmic-cli/src/update.rs:559-575
fn validate_runtime_dir(dir: &Path) -> Result<()> {
    ...
    let descriptors =
        orgasmic_core::NodeTypeRegistry::load(&dir.join("shipped/schema/node-types"))?;
    for required in orgasmic_core::NodeTypeRegistry::embedded()?.descriptors() {
        if descriptors.descriptor(&required.collection).is_none() {
            bail!("runtime bundle missing descriptor for {}", required.collection);
```

And `load` is not tolerant of a missing directory:

```rust
// crates/orgasmic-core/src/node_registry.rs:257-260
let entries = std::fs::read_dir(dir)
    .with_context(|| format!("read node-type descriptors from {}", dir.display()))?;
```

Impact: `validate_runtime_dir` runs inside the **currently installed** CLI,
validating the **newly downloaded** bundle, before the swap. A user on 0.0.26
who runs `orgasmic update` against a 0.1.0 bundle with no
`shipped/schema/node-types/` gets `read node-type descriptors from …: No such
file or directory` and a rollback. This is not fixable in the 0.1.0 release,
because the failing code is the old binary. Decision 33's "one breaking release"
does not cover it; a broken updater means manual reinstall for every install.

Smallest correction: state the migration path explicitly in decision 33 — either
(a) keep `shipped/schema/node-types/` populated as a compatibility shim for at
least one release so old CLIs still validate, or (b) accept that 0.1.0 requires
a manual `install.sh` run and say so in the release notes and in S5's exit.

Verification: `sed -n '553,580p' crates/orgasmic-cli/src/update.rs` and
`sed -n '255,275p' crates/orgasmic-core/src/node_registry.rs`.

**B2. `shipped/plugins/` is scanned by nothing. Decision 10's foundation is a
seventeenth loader gap, missing from 13.3's "Sixteen, all verified absent."**

Location: decision 10 "Pre-installed plugins in `shipped/plugins/<id>/`, enabled
by default"; 13.3 "Sixteen, all verified absent on `main`"; 13.5 `id(&self)`
comment "matches `shipped/plugins/<id>/plugin.org`".

`grep -rn "shipped/plugins" crates scripts ui/src shipped` returns **0 hits**
repo-wide. Every plugin path in production is the user tier, hardcoded at seven
sites across two crates:

```
crates/orgasmic-daemon/src/plugins.rs:170,230,278,428,464   home.user().join("plugins")
crates/orgasmic-cli/src/plugin.rs:91,100,237                home.user().join("plugins")
```

The loader's own watched-root list (`plugins.rs:167-171`) is exactly
`user/schema/node-types`, `shipped/schema/node-types`, `user/plugins` — no
shipped plugin root. And `PluginRegistry::remove` (`plugins.rs:462-464`) is
hardcoded to `home.user()/plugins/<id>`, so a shipped plugin is not even
addressable by the existing lifecycle verbs.

Impact: S1's exit ("Goal, handoff and gotchas are plugins") cannot be reached
without first adding a shipped plugin root to discovery, to the signature/watch
set, to `list`, to `enable`/`disable`, and deciding what `plugin remove` does to
a pre-installed plugin. That is a slice of its own, and 13.7 has no line item for
it — S1's list jumps straight to "trait wired into daemon and CLI boot."

Smallest correction: add gap 17 to 13.3 ("no shipped plugin root; all seven
plugin-path sites are `home.user()`") and an explicit S1 line item for it.

Verification: `grep -rn 'join("plugins")\|shipped/plugins' crates/orgasmic-daemon/src/plugins.rs crates/orgasmic-cli/src/plugin.rs`.

**B3. Decision 21's `NodeChanged { collection, node_id }` drops `project_id` and
opens a cross-project event leak through the existing WS authz filter.**

Location: decision 21, "`TaskUpdated`, `ArtifactChanged`, `ArtifactCommentAdded`
collapse into `NodeChanged { collection, node_id }`."

All three collapsing variants carry `project_id` today, and that field is what
gates delivery:

```rust
// crates/orgasmic-daemon/src/authz.rs:337-344
match (identity, payload.project_id()) {
    (Identity::Admin, _) => true,
    (Identity::Plugin { .. }, _) => false,
    (Identity::Member { .. }, None) => true,          // <-- no project => everyone
    (Identity::Member { grants, .. }, Some(project)) => {
        grants.iter().any(|(p, _)| p == project || p == "*")
    }
}
```

`EventPayload::project_id()` (`events.rs:145-170`) is an explicit match arm per
variant; a two-field `NodeChanged` falls into the `None` bucket. The doc comment
above the function states the consequence plainly: "payloads with no project
(daemon-wide/manager-internal signals) pass once their topic is allowed."

Impact: a member granted `editor` on project A, with no grant on project B,
would receive `NodeChanged` for every write in project B. This is a regression
against a filter the repo deliberately built (`dec_KF2MR`), introduced by a
decision framed as an event-shape cleanup.

Smallest correction: write decision 21 as
`NodeChanged { project_id, collection, node_id }` and add the arm to
`EventPayload::project_id()`. One field, and the security property is preserved.

**B4. Opening `Topic` to plugin strings while removing `ArtifactsRead` leaves no
rule that grants any topic to a member.**

Location: decision 21 "`Topic` and `WorkerKind` open to validated strings the
manifest declares"; decision 23 "The `artifacts` role is dropped … `ArtifactsRead/
Comment/Generate` are removed" (13.5 Authz).

Topic eligibility is computed from a hardcoded Action→Topic table:

```rust
// crates/orgasmic-daemon/src/authz.rs:303-325
Identity::Member { grants, .. } => {
    for (_, role) in grants {
        let caps = role_capabilities(role);
        if caps.contains(&Action::ArtifactsRead) { topics.insert(Topic::Artifact); }
        if caps.contains(&Action::GraphRead)     { topics.insert(Topic::Graph); ... }
        if caps.contains(&Action::TasksRead)     { topics.insert(Topic::Task); ... }
```

`Action::ArtifactsRead` is the **only** thing that inserts `Topic::Artifact`.
Remove it and no non-admin member ever receives an artifact event again. More
broadly, decision 23 says manifest capabilities mint actions
`plugin.<id>.<cap>`, but nothing in section 13 says how a plugin-declared **topic**
becomes subscribable — `allowed_topics` has no generic path.

Impact: S3's exit ("Artifact generate, submit, regenerate, comments work through
plugin routes") is testable as an admin and silently broken for every member.

Smallest correction: add one line to 13.5 Authz stating the rule, e.g. "a
member holding `plugin.<id>.nodes.read` is eligible for every topic `<id>`
declares," and make `allowed_topics` derive from declared topics rather than the
fixed three `if` statements.

**B5. Slice S0 is not a behavior-free rename. `artifactor` is a serde wire value
and appears as data in four shipped prompt specs and the user override tier.**

Location: decision 12 "rename `artifactor` to `generator`"; 13.7 S0 "no behavior
change. | Exit: Test suite identical."

`WorkerKind` serializes by variant name:

```rust
// crates/orgasmic-core/src/schema.rs:127-136
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerKind { Implementer, Reviewer, Planner, Analyzer, Griller,
                      Glossarist, Manager, Artifactor }
```

and `artifactor` is a literal in shipped data, not just code:

```
shipped/prompt-studio/prompt-specs/task-regenerate.org:4:      :KIND: artifactor
shipped/prompt-studio/prompt-specs/decision-regenerate.org:4:  :KIND: artifactor
shipped/prompt-studio/prompt-specs/glossary-regenerate.org:4:  :KIND: artifactor
shipped/prompt-studio/prompt-specs/artifact-generator.org:4:   :KIND: artifactor
ui/src/components/GenerateArtifactDialog.tsx:147:  <TransportPicker kindLabel="artifactor" …
```

Section 2.1 records that a `user/` tier shadows any shipped file. A user who
overrode `artifact-generator.org` keeps `:KIND: artifactor` in their home
directory, where the rename cannot reach it, and gets a parse failure on a spec
that worked yesterday. Stored dispatch records in existing ledgers carry the same
string.

Impact: S0's stated exit ("Test suite identical") is unachievable if any test
asserts the string, and the "no behavior change" framing will lead an
implementing agent to skip the data migration and the user-tier compatibility
alias entirely.

Smallest correction: restate S0 as a wire rename with a deserialization alias
(`#[serde(alias = "artifactor")]`) retained through at least 0.1.0, list the four
shipped specs as in-scope files, and change the exit to "old records and
user-tier specs naming `artifactor` still parse."

### risk

**R1. "No plugin-to-plugin dependencies" is stated as a principle, used as
decision 14's sole rationale, then violated by decisions 19 and 36 without
comment — and cannot be expressed in the manifest today.**

Location: section 1 "Core provides; plugins consume. Avoid mutual dependencies";
section 3.2 "Anything two or more plugins need is core"; decision 14
"Conversations | Stays core as `core.chat`. Moving it creates a plugin-to-plugin
dependency."; decision 19 "Tasks declares `OPTIONAL` goal"; decision 36 "[Forum]
Needs the tasks plugin and a `core.dispatch@1` service".

Decision 14 rejects a move *because* it would create a plugin-to-plugin edge.
Five decisions later, tasks depends on goal, and twenty-two later, forum depends
on tasks. Both are exactly the shape decision 14 forbids. The document never
reconciles this, so a fresh agent cannot tell whether the principle is binding.

It is also not currently expressible. `REQUIRES` is validated against a
four-element list of core services only:

```rust
// crates/orgasmic-core/src/plugin.rs:11-16, 108-113
pub const KNOWN_SERVICES: [&str; 4] =
    ["core.nodes@1", "core.links@1", "core.attachments@1", "core.chat@1"];
...
for service in words("REQUIRES") {
    anyhow::ensure!(KNOWN_SERVICES.contains(&service.as_str()),
        "required service {service} is unavailable");
}
```

`:REQUIRES: tasks` is rejected outright. Worse, `OPTIONAL` is **never validated**
— only `words("REQUIRES")` is checked — despite the doc comment at
`plugin.rs:10` claiming otherwise ("Core services a manifest may name in
`:REQUIRES:` (and `:OPTIONAL:`)"). So decision 19's `OPTIONAL: goal` would parse
silently and mean nothing, giving a false green.

Smallest correction: state the rule section 13 actually intends ("a plugin may
depend on another plugin's declared service, not on its internals"), and add a
13.3 gap item for manifest-declarable plugin services plus `OPTIONAL` validation.

**R2. The descriptor pin has no owner for the five zero-code types.**

Location: 13.5 "The pin at `node_registry.rs:171-174` and `embedded()` at
`:275-306` move into each compiled crate."

The pin is strict equality against a compiled-in copy:

```rust
// crates/orgasmic-core/src/node_registry.rs:170-175
for builtin in Self::embedded()?.descriptors() {
    let loaded = registry.descriptor(&builtin.collection).unwrap();
    anyhow::ensure!(loaded == builtin,
        "compiled collection {} descriptor is pinned; user descriptors may only add new collections", …);
}
```

Decisions 19, 27 and 37 make goal, handoff, gotchas, decisions and glossary
**zero-code** plugins. They have no crate for the pin to move into. Two readings
are available and the document picks neither: the pin is dropped for them (so a
user-tier descriptor can silently redefine `decisions`, losing a real invariant),
or the pin stays somewhere unnamed. Separately, `embedded()` covers **five**
descriptors including `conversation.org`, and conversations "stays core" per
13.2 — so its descriptor's post-move home is also unstated.

Smallest correction: one sentence in 13.5 saying where the pin lives for
zero-code shipped plugins and for `conversations`.

**R3. Decision 20 leaves a source build with no tasks or artifacts UI, and the
package step must write into the tracked tree.**

Location: decision 20 "Separate `ui/index.js` bundles now, served from
`shipped/plugins/<id>/ui/`, built at package time, not committed."

The host SPA is compiled in from an env-var directory:

```rust
// crates/orgasmic-daemon/src/api.rs:103
static UI_DIST: Dir<'_> = include_dir!("$ORGASMIC_UI_DIST_DIR");
```

and the packager copies the tree wholesale:

```sh
# scripts/package-runtime.sh:211
cp -R "$ROOT/shipped" "$STAGE/shipped"
```

Two consequences neither decision 20 nor 13.8 states. First, "built at package
time, not committed" means a developer running a plain `cargo build` from a
checkout after S3/S4 has **no** artifacts or tasks UI at all — the whole product
surface disappears from the dev loop. Second, for the bundles to reach the
tarball they must be emitted into `$ROOT/shipped/plugins/<id>/ui/` *before* line
211, i.e. the release step dirties the tracked working tree.

Smallest correction: name the dev-build story in decision 20 (a `just`/npm step,
or commit the bundles) and state the emit-before-copy ordering in S5.

**R4. Decision 40's singleton move is silent about multi-machine ledgers.**

Location: decision 40 "Goal and handoff move from `.orgasmic/tasks/goal.org` …
to `.orgasmic/goal.org`"; decision 22 "One `orgasmic project migrate` step".

The old paths are baked into a returned constant, not only into a path builder:

```rust
// crates/orgasmic-core/src/paths.rs:110-112
pub fn goal_file_rel() -> &'static str {
    concat!(".orgasmic/tasks/", "goal.org")
}
```

and `ledger_sync.rs` stages these singletons by name as the claim-free set (the
explanatory comment at `:178-190` lists "the singleton `project.org`,
`tasks/goal.org`, `tasks/handoff.org`, and `gotchas.org`"). Ledgers sync across
machines over a git branch. `project migrate` is one-way and per-project; the
brief does not say what a machine still on the old layout does when it pulls a
migrated ledger, nor whether `goal_file_rel()` is in scope.

Smallest correction: add `paths.rs:110` `goal_file_rel()` to decision 40's
in-scope list, and one sentence on the mixed-version sync window.

**R5. "each referenced only from `forum.rs`" is false for all seven forum prompt
specs, and decision 38 deletes two more pinned files.**

Location: 13.2 forum row, "Owns 7 prompt specs … (each referenced only from
`forum.rs`)"; decision 36; decision 38.

They are referenced from `forum.rs` — and also SHA-pinned in a manifest and named
in shipped recipes:

```
shipped/skills/orgasmic/meta/corpus-manifest.json:159  "…/critic.org": "9ca70ade…"
shipped/skills/orgasmic/meta/corpus-manifest.json:166  "…/forum-reviewer.org": "2ac28d18…"
shipped/skills/orgasmic/meta/corpus-manifest.json:169  "…/griller.org": "9e368410…"
shipped/skills/orgasmic/meta/corpus-manifest.json:172  "…/planner.org": "7f72dc64…"
shipped/skills/orgasmic/recipes/adversarial-forum-review.md:9
shipped/skills/orgasmic/recipes/judge-document.md:9
shipped/skills/orgasmic/recipes/dispatched-curator.md:10
```

The manifest pins 20 prompt specs by content hash. I verified it is consumed by
documentation (`shipped/skills/orgasmic/meta/corpus.md`) rather than by a build
gate, so this is stale-doc breakage rather than a failed build — but S6 moving 7
files and S1 deleting 2 leaves 9 wrong hashes and 3 dangling recipe references,
none of which appear in S1's or S6's work column.

Smallest correction: add `corpus-manifest.json` and the three recipes to S1 and
S6's work lists.

**R6. The task prompt-slot surface is three lists of two different lengths, and
13.2 names two of them with one count.**

Location: 13.2 tasks row, "11 `task.*` prompt slots hardcoded
(`core/src/slots.rs:34-74`, `prompt_compiler.rs:499-511`)".

Measured at `cf4566f6`:

- `crates/orgasmic-core/src/slots.rs:36-45` — **10** `task.*` entries.
- `shipped/prompt-studio/slots.org:19-24` — **11**.
- `crates/orgasmic-daemon/src/prompt_compiler.rs:~499-513` — **11**.

`task.test_cmd` is in the latter two and absent from `default_registry()`, whose
own doc comment reads "Mirrors `shipped/prompt-studio/slots.org`". So the count
11 is right for one of the two cited sites, and the third site — the shipped
`slots.org` that the registry claims to mirror — is not cited at all.

Impact: S4's `slots()` accessor has to reconcile three lists that already
disagree. An agent told "11 slots in two files" will move 10 and leave a drift it
did not know existed.

Smallest correction: cite all three sites and note the `task.test_cmd`
discrepancy as a pre-existing condition to resolve, not to preserve.

**R7. The identity-lint trap undercounts the type-specific code in core.**

Location: 13.8 trap, "`core/src/identity_lint.rs` has `lint_task_heading_id_token`
and `lint_decision_heading_id_token`, two type-specific lints in core."

There are three collectors, not two lints:

```
crates/orgasmic-core/src/identity_lint.rs:143  fn collect_task_identities
crates/orgasmic-core/src/identity_lint.rs:158  fn collect_decision_identities
crates/orgasmic-core/src/identity_lint.rs:176  fn collect_glossary_identities
crates/orgasmic-core/src/identity_lint.rs:331  fn is_retired_architecture_id   // hardcodes `arch_`
```

The proposed fix ("one generic heading-token lint driven by the descriptor's
`:ID_PREFIX:`") does not address the three collectors or the hardcoded retired
`arch_` prefix, which no descriptor will ever declare.

**R8. S1 is a slice in name only; its gate covers a fraction of it.**

Location: 13.7 S1 row; decision 34 "Trait gate | Goal and handoff moved end to
end through the trait, disable works, the phone app still works, reviewed."

S1's work column contains roughly fourteen independent changes: new crate, trait
wiring in two binaries, route namespace, CLI verb registry, `NodeChanged`, open
`Topic`, open `WorkerKind`, `index.plugins[id]`, capabilities→actions, CSP, two
new UI slots, `:SINGLETON:`, `PLUGIN_API 2`, a ledger migrate step, three types
moved, and the deletions from decisions 38 and 39 plus three route stubs. Several
are independently blocking (B3, B4 both live here). Decision 34's gate tests only
"goal and handoff move and disable works."

Impact: with B2 added, S1 is plausibly larger than S3. The "6 to 8 weeks serial"
estimate attributes roughly half to S4 and does not account for S1's real size.

Smallest correction: split S1 into S1a (trait, namespace, `PLUGIN_API 2`, shipped
plugin root) and S1b (event/topic/authz opening, deletions, the three singletons),
each with its own gate.

### improvement

**I1. The header claims P2 landed; P2's own exit criterion did not.**

Location: header, "2026-09-12: P0, P1, P2, P3 and the P5 minimum have landed on
`main` (`cf4566f6`)"; section 7 P2, "Move the four built-in descriptors to
`shipped/plugins/<id>/plugin.org` so built-ins and user plugins share one loader."

`shipped/schema/node-types/` still holds five descriptors (`artifact.org`,
`conversation.org`, `decision.org`, `glossary.org`, `task.org`), and
`plugins.rs:169` still reads that directory as a loader root. A fresh agent
reading "P2 has landed" will assume the descriptor consolidation is done and be
wrong about the starting state of every subsequent slice.

Correction: one line under the header — "P2 landed except the descriptor move;
built-ins still load from `shipped/schema/node-types/`."

**I2. Four citations would misroute a fresh agent.**

- `node_services.rs:22,174` (13.3 item 12) — this filename exists in **both**
  `crates/orgasmic-core/src/` and `crates/orgasmic-daemon/src/`. The intended one
  is core (`pub kind: String` at :22). The daemon copy's :22 is a MIME-type list.
- `node_services.rs:174` is inside `#[cfg(test)]` (the module opens at
  `crates/orgasmic-core/src/node_services.rs:165`). It is a test fixture
  (`kind: "RELATES_TO".into()`), cited as evidence of a production free-string
  vocabulary.
- `prompt_compiler.rs` (13.2, 13.3 item 7) is in **`crates/orgasmic-daemon/src/`**,
  not core — but 13.2 cites it immediately after `core/src/slots.rs`, which reads
  as a shared prefix.
- `router.tsx:367-390` (13.2 artifacts row) is `ui/src/app/router.tsx`, not
  `ui/src/router.tsx`. The lines themselves are right (`artifactsRoute` /
  `artifactViewRoute`).

Correction: use one prefix convention throughout — full `crates/<crate>/src/…`
and `ui/src/…` paths — and drop the test-module citation.

**I3. The `REQUIRED_RUNTIME_FILES` trap describes the mechanism backwards.**

Location: 13.8, "Every new `shipped/plugins/<id>/` file the daemon needs at boot
must be listed or the updater rolls back."

`validate_runtime_dir` (`update.rs:559-565`) bails when a **listed** file is
missing. An unlisted file is shipped fine — `package-runtime.sh:211` copies
`shipped/` wholesale — it is simply never verified. So the real hazard is the
opposite of the one stated: a truncated or corrupt plugin install passes
validation silently. The rollback hazard is B1, which the trap does not mention.

**I4. `KNOWN_SERVICES` is a fixed-size array.**

Location: 13.5 Services, "`KNOWN_SERVICES` (`core/src/plugin.rs:11`) gains
`core.dispatch@1`."

It is `pub const KNOWN_SERVICES: [&str; 4]`. Adding an element changes the type.
Trivial, but worth a word so it is not discovered as a surprise compile error,
and it is a hint that this list wants to be derived rather than literal once
plugins can declare services (see R1).

## Missing Evidence or Assumptions

Things section 13 assumes that I could not confirm from the tree, or that it
leaves a reader to guess:

1. **Where `conversations`' descriptor lives after the move.** It is in
   `embedded()` (`node_registry.rs:275-306`, five entries) and pinned, but
   decision 14 keeps conversations in core. Unstated.
2. **What `plugin remove` does to a pre-installed plugin.** The current
   implementation (`plugins.rs:462-464`) is hardcoded to the user tier, so
   shipped plugins are incidentally unremovable. I believe this is an accident
   rather than the intended policy; decision 35 covers *disable* and is silent on
   *remove*.
3. **How a member becomes eligible for a plugin-declared topic** (B4). The
   Action→Topic table is hardcoded and section 13 proposes no replacement rule.
4. **Whether `OPTIONAL` is meant to gate anything at load time.** It is parsed
   and not validated today; section 4 gives it real semantics ("gates one pane or
   command") that no code enforces. Decision 19 depends on it.
5. **The mixed-version ledger sync window** for decision 40 (R4). Whether two
   machines at different migration states are expected to coexist at all.
6. **Whether the `artifactor`→`generator` rename is expected to be
   backward-compatible on read** (B5). "No behavior change" implies yes; nothing
   states it.
7. **Whether existing `<tmp>/forum/*.json` manifests reference prompt spec names
   by string.** S6's exit asserts "Existing `<tmp>/forum/*.json` manifests still
   resume," but if the stored manifests name specs by bare name, the spec lookup
   (`prompt_compiler.rs:171-178`, which scans only the two flat
   `prompt-studio/prompt-specs` roots) will not find them after decision 36 moves
   them into the plugin folder. 13.3 item 7 covers `REGENERATE_PROMPT` resolution
   only, not spec lookup by name — this may be an eighteenth loader gap. I did
   not read far enough into `forum.rs` to confirm either way.
8. **Section 13.7's estimate.** "6 to 8 weeks serial" and "S4 alone is roughly
   half" are presented without a basis; given B2 and R8, S1 looks materially
   larger than the table implies. I treat the estimate as unsupported rather than
   wrong.

I did not verify: the 13.2 line-count claims for tasks (`~36 routes, 5,642 lines
across 122 fns`, `~670 index.rs task lines`, `7,217 task lines in manager.rs`),
the artifacts `~1,810 lines` and `6,405 lines of UI`, or the `supervisor.rs` 419
`TASK-` hit breakdown (92 provenance / 398 tests). The claims I *did* measure in
that table were all accurate, so I have no specific reason to doubt these.

## Highest-value Verification Targets

In order. The first two should happen before any code is written.

1. **B1, the updater.** Stage a runtime directory with `shipped/schema/node-types/`
   removed and run the installed 0.0.26 `orgasmic update` against it. Confirm the
   bail message and decide the migration path. This gates the whole release plan
   and cannot be fixed after the fact.
2. **B2, the shipped plugin root.** `grep -rn "shipped/plugins" crates scripts ui/src shipped`
   → expect 0. Then decide whether discovery gains a second root or built-ins are
   copied into the user tier at install. This gates S1.
3. **B3/B4, the event and authz seams.** Add a test asserting that a member with
   grants on project A receives no `NodeChanged` for project B, and one asserting
   a non-admin member still receives artifact events after the `artifacts` role is
   dropped. Both should be written before decision 21 and 23 are implemented — they
   are the cheapest possible proof that the collapse preserved the filter.
4. **B5, the rename.** `grep -rn "artifactor" shipped ui/src crates` and check
   `~/.orgasmic/user/prompt-studio/prompt-specs/` on a real install for user
   overrides carrying `:KIND: artifactor`. Decide on the serde alias before S0.
5. **R1, plugin-to-plugin dependencies.** Attempt `:REQUIRES: tasks` and
   `:OPTIONAL: goal` against the current parser
   (`crates/orgasmic-core/src/plugin.rs:108-113`). Expect the first to be rejected
   and the second to be silently accepted-and-ignored. Resolve the principle
   before S1 writes the goal and handoff manifests, since decision 19 depends on it.
6. **R3, the dev build.** After S3, run a plain `cargo build` from a clean
   checkout and confirm whether the artifacts UI is reachable. If it is not,
   decision 20 needs a dev-loop answer before S3, not after.
7. **R6, the slot lists.** `grep -c '"task\.' crates/orgasmic-core/src/slots.rs`
   versus `grep -c 'task\.' shipped/prompt-studio/slots.org` — reconcile the
   `task.test_cmd` drift before S4 moves the slot registry.

# Participant

codex · openai · gpt-5.6-sol · effort xhigh

# Overall Assessment

Section 13 has a coherent destination—compiled type plugins behind one host contract, declarative built-ins where behavior permits it, and per-project disablement—but it is not yet a self-contained implementation brief. Four contract defects force an implementer to invent architecture or discard existing behavior: the proposed trait violates its own dependency direction, activation cannot be evaluated through several of its signatures, the zero-code singleton model does not represent the current goal/gotcha semantics, and S1 contradicts the required trait-first review gate. The remaining findings are sequencing, migration, authorization, and development-path gaps that should be closed before serial implementation starts.

Evidence was checked independently in the clean `cf4566f6dfb71b84ef2a657d0fa0e1075d598531` worktree. No source files were edited and no test suite was run because the assignment is document critique, not implementation validation.

# Findings

## blocking

1. **The `CompiledPlugin` route signature cannot satisfy the stated dependency direction.**
   - **Target location:** Section 13.5, `fn routes(&self) -> axum::Router<AppState>`; immediately above it, “plugin crates depend on core and this crate; the daemon and CLI binaries depend on plugin crates ... Core never imports a plugin.”
   - **Impact and evidence:** There is no `AppState` in the baseline. The host state is `orgasmic_daemon::api::ApiState` (`crates/orgasmic-daemon/src/api.rs:192-243`), and existing nested routers are `Router<ApiState>` (`crates/orgasmic-daemon/src/node_services.rs:44`). If a plugin crate names that type, it must depend on `orgasmic-daemon`, while the daemon is required to depend on the plugin crate: a Cargo cycle. `CliContext` and `SlotProvider` are also named but neither exists, so the crate ownership of the host-facing types is unresolved.
   - **Smallest useful correction:** Put a host-neutral `PluginHttpContext`/route contribution type, `CliContext`, and `SlotProvider` in `orgasmic-plugin-api` (or core), then have the daemon adapt `ApiState` into that surface. Prove the dependency graph with one empty compiled plugin linked into both binaries before declaring the trait slice ready.

2. **Per-project disablement is not expressible through the proposed trait.**
   - **Target location:** Section 13.5, “Every accessor consults the per-project activation (`.orgasmic/plugins.json`) before answering,” alongside `hooks()`, `slots()`, `cli()`, `worker_kinds()`, and `topics()`.
   - **Impact and evidence:** Those accessors receive no project, ledger, or invocation context. `cli()` runs before a project may have been resolved; `hooks()` and `slots()` are returned globally; `topics()` and `worker_kinds()` are global vocabularies. An implementation must therefore use hidden global state, register/unregister globally when one project changes, or silently leave behavior active for disabled projects. All three conflict with decision 35. The current P2 registry instead gates a concrete `ProjectPlugins` snapshot (`crates/orgasmic-daemon/src/plugins.rs:15-70,301-330`), which shows the missing dimension.
   - **Smallest useful correction:** Make registration global and immutable, but pass a project-scoped invocation context to hooks, projections, slots, dispatch extras, and CLI execution; enforce route activation in host middleware after extracting `:p`. State explicitly which discovery methods are global and which invocation methods are activation-gated.

3. **The zero-code `Singleton` shape does not encode the behavior or storage model of the three S1 singletons.**
   - **Target location:** Section 13.5, `** Singleton goal ... :ID: goal-current`; decisions 19 and 37; S1, “goal, handoff and gotchas moved through all of it.”
   - **Impact and evidence:** `goal.org` is not one fixed-ID heading: `goal set` mints `goal-YYYYMMDD-*`, preserves prior headings, changes prior state to `SUPERSEDED`, validates Statement/Reached When, and prepends the new heading (`crates/orgasmic-daemon/src/api.rs:19858-20330`; `crates/orgasmic-cli/src/goal.rs:9-71`). It also directly updates `handoff-current.GOAL_ID` (`api.rs:20181-20224`), creating an unresolved interaction between the two separately owned zero-code plugins. `orgasmic gotcha add` performs title uniqueness, structural-body validation, timestamping, and a read-modify-write (`crates/orgasmic-cli/src/gotcha.rs:17-133`); a `:CLI:` alias to unspecified generic singleton verbs does not preserve that behavior. None of these losses appears in Section 13.6.
   - **Smallest useful correction:** Define the generic singleton resource and its mutation verbs precisely, then show how it represents multi-heading goal history, goal→handoff synchronization, and gotcha append validation. If the generic contract cannot do that without a workflow DSL, keep these small behaviors in C1 plugins or explicitly accept and test their removal.

4. **S1 violates the document's own trait-first review gate.**
   - **Target location:** Decision 24, “Trait, then goal and handoff”; decision 32, “The trait slice is approved before any type moves”; versus S1, which combines the trait, route/CLI/event/index/authz/UI contracts, migration, three singleton moves, CSP, and several feature deletions.
   - **Impact and evidence:** There is no state in the stated slice plan where reviewers can approve the trait before a type moves. Failures in singleton semantics, migration, CLI registration, or deleted features would be inseparable from the foundational contract, defeating the gate intended to limit the blast radius.
   - **Smallest useful correction:** Split S1 into S1a (API crate, host wiring, activation gate, one inert fixture plugin) and S1b (goal/handoff/gotchas plus removals and migration). Review and approve S1a before S1b.

## risk

1. **Worker-kind ownership conflicts with dispatch being a core, cross-node service.**
   - **Target location:** Section 13.5's manifest example `:WORKER_KINDS: implementer reviewer`, trait method `worker_kinds()`, decision 35 (tasks may be disabled), and S4's exit “Meetings can dispatch a run against a meeting.”
   - **Impact and evidence:** If tasks owns `implementer` and `reviewer`, disabling tasks can remove the very roles meetings needs. If worker kinds remain registered despite disablement, “every accessor consults activation” is false. Today these are global governance identities (`crates/orgasmic-core/src/schema.rs:124-176`; `crates/orgasmic-daemon/src/governance.rs:151-178`), not task lifecycle values.
   - **Smallest useful correction:** Keep general worker roles in core and let plugins add only plugin-specific roles, or declare worker-kind registration global while gating dispatch solely by the subject collection's activation. Add a tasks-disabled/meetings-dispatch matrix to the contract.

2. **Opening event topics omits the subscription authorization and project-scoping contract.**
   - **Target location:** Section 13.5, “`Topic` becomes a validated string ... Plugins publish under topics they declared.”
   - **Impact and evidence:** Current visibility is safe because `allowed_topics` maps the closed `Topic` enum to static actions and `EventPayload::project_id()` exhaustively extracts project scope (`crates/orgasmic-daemon/src/events.rs:17-51,145-170`; `authz.rs:300-344`). A free topic and plugin payload have no declared read action or mandatory project envelope, so a naive conversion can leak project events or make every dynamic topic admin-only. Declaring publish names alone is insufficient.
   - **Smallest useful correction:** Require every event to use a core envelope containing `project_id`, `topic`, and payload; register each dynamic topic with both publish ownership and the action required to subscribe. Test cross-project and viewer filtering before removing the enum.

3. **The claim that task and artifact comment routes are “thin wrappers” is false and under-specifies the replacement.**
   - **Target location:** Section 13.5 Routes, “one `/projects/:p/nodes/:id/comments` (replacing the task and artifact copies, both thin wrappers over `node_kernel::parse_journal`).”
   - **Impact and evidence:** Task comments have add, optimistic edit, and tombstone-delete behavior with actor ownership (`api.rs:2861-3001`). Artifact comments bind version, anchor, resolution target and reply parent, serialize against regeneration, write transactionally, and have a separate resolve/consume state machine (`api.rs:21602-21874`). `parse_journal` only parses the common drawer (`crates/orgasmic-core/src/node_kernel.rs:132-180`). Treating these as one thin route can drop authorization, OCC, version anchoring, or resolution semantics; S3 nevertheless requires artifact comments to keep working.
   - **Smallest useful correction:** Specify the generic route as only the common operation(s) it truly owns, list edit/delete/resolve/consume endpoints explicitly, and leave type semantics in their plugins unless a tested generic journal mutation contract is defined.

4. **Dynamic plugin actions have no role/grant migration or default authorization mapping.**
   - **Target location:** Decision 23, “Manifest capabilities mint actions `plugin.<id>.<cap>` ... the `artifacts` role is dropped”; Section 13.5, “`authz::require` unchanged.”
   - **Impact and evidence:** `authz::require` currently accepts a closed `Action`, `Action::from_name` rejects unknown strings, viewer/editor permissions come from a static table, and `members.org` grants store roles plus optional validated actions (`crates/orgasmic-daemon/src/authz.rs:16-151,227-277`; `crates/orgasmic-core/src/members.rs:19-26,161-187`). The migration in decision 22 does not rewrite member grants. After plugin routes begin requiring dynamic actions, non-admin users either lose built-in access or the host must invent wildcard semantics; members with the removed `artifacts` role are especially undefined.
   - **Smallest useful correction:** Define the dynamic action representation accepted by `require`, the viewer/editor defaults for each pre-installed plugin, collision validation, and an explicit `members.org` migration (or state that existing member access is intentionally discarded).

5. **The S2/S3 order knowingly removes an artifact action before its replacement exists.**
   - **Target location:** S2 deletes `DecisionsView` and `GlossaryView`; S3 says `registerNodeAction` replaces the generate-dialog import; trap 3 says “Cut it with `registerNodeAction` in S3, or S2 cannot delete the bespoke views cleanly.”
   - **Impact and evidence:** Both views currently import and render `GenerateArtifactDialog`, as does the generic `NodeModal` (`ui/src/components/DecisionsView.tsx:17,449-456`; `GlossaryView.tsx:15,164-171`; `node-views/NodeModal.tsx:30,452`). With S2 first, generation from decision/glossary disappears until S3, an unlisted sixth loss beyond Section 13.6.
   - **Smallest useful correction:** Register the existing host-owned artifact action through the S1 slot before S2 removes the views, then transfer ownership to the artifacts bundle in S3; alternatively, move S3 before S2.

6. **Package-time-only built-in UI bundles have no debug/test loading path.**
   - **Target location:** Decision 20, “Separate `ui/index.js` bundles now ... built at package time, not committed”; Section 13.5 says those files are required runtime files.
   - **Impact and evidence:** Current manifest validation requires `ui/index.js` to exist (`crates/orgasmic-core/src/plugin.rs:201-224`), while debug/test Rust builds deliberately skip npm and emit only a host placeholder (`crates/orgasmic-daemon/build.rs:36-80`). Once artifacts/tasks manifests declare UI, ordinary `cargo test`, `cargo build`, and source-daemon startup can reject the pre-installed plugins unless an unmentioned generation or relaxed-validation path is added.
   - **Smallest useful correction:** Define one development contract: either the UI build always emits built-in bundles before plugin validation, the build script creates OUT_DIR fixtures and points the loader there, or shipped compiled manifests defer UI-file validation in debug. Add source-debug, test, runtime-package, and Tauri gates.

7. **The release order publishes stable before the declared kernel extraction is complete.**
   - **Target location:** Decision 33, “Nightly until the tasks slice lands, then stable”; S5 is release; decision 36 and S6 move forum only after S5.
   - **Impact and evidence:** The stable release would still contain `forum.rs`, its seven prompt specs, and crate-private manager calls in the CLI, despite Section 13.1's “core becomes the kernel” destination and the explicit decision that forum is a compiled plugin. That makes the stable boundary disagree with the project's stated completion boundary.
   - **Smallest useful correction:** Move S6 before S5, or call the post-S4 build nightly and reserve stable for the post-forum tree.

8. **The migration names outcomes but not the actual current schema or reference rewrite.**
   - **Target location:** Decision 22, “stamp `#+orgasmic_plugin: <id>@1` on every node; retire the `term:` alias by rewriting to `term_`; move `goal.org` and `handoff.org`.”
   - **Impact and evidence:** The landed P2 writer and write gate use a different header, `#+plugin: <id> schema=<n>` (`crates/orgasmic-daemon/src/plugins.rs:38-70`). Legacy `term:` is accepted in ID validation, kind inference, graph indexing, and dangling-reference recognition (`crates/orgasmic-core/src/node_registry.rs:60-65,224-234`; `crates/orgasmic-daemon/src/index.rs:3467-3468,3803-3812`). “Rewriting to `term_`” does not say whether directories, heading IDs, inbound properties, journals, immutable tx records, and prose references move together. A partial rewrite creates unreadable or dangling data.
   - **Smallest useful correction:** Choose one canonical header, enumerate every rewritten surface, define idempotency and rollback, and provide before/after fixtures containing goal history, handoff linkage, legacy glossary IDs, inbound edges, and existing plugin headers.

9. **Moving all of `retro.rs` with tasks disables a core run-only diagnostic.**
   - **Target location:** Section 13.2 lists `retro.rs` under tasks; S4 says “dispatch routes, subtasks, retro ... move”; decision 35 allows tasks to be disabled.
   - **Impact and evidence:** The retrospective scope accepts explicit run selectors independently of tasks and reads the core run catalog/session evidence (`crates/orgasmic-daemon/src/retro.rs:17-55,127-170,206-243`). Moving it wholesale behind tasks means `orgasmic manager retro --run ...` disappears when tasks is disabled, although run supervision remains core.
   - **Smallest useful correction:** Keep run-only retrospective preparation in core and make task enrichment an optional tasks-plugin contributor, or explicitly accept that disabling tasks removes run retrospectives.

10. **The daemon CSP work item is stale and could replace a stronger landed policy.**
    - **Target location:** Decision 29, “CSP Added in the trait slice. Daemon sends `script-src 'self' /plugins/*`; Tauri `csp` set to match.”
    - **Impact and evidence:** At `cf4566f6`, the daemon already sends a nonce-bearing policy with `default-src`, `object-src`, `base-uri`, and `frame-ancestors` restrictions (`crates/orgasmic-daemon/src/api.rs:1424`), and tests pin the nonce/no-`unsafe-eval` properties (`crates/orgasmic-daemon/tests/plugin_registry_routes.rs:223-228`). Only Tauri remains `null` (`src-tauri/tauri.conf.json:31-33`). Replacing this with the quoted fragment loses protections, while `'self'` already covers same-origin `/plugins/*`.
    - **Smallest useful correction:** Mark daemon CSP as landed and preserve its policy; scope S1 to an equivalent Tauri policy plus a plugin-load smoke in web and Tauri.

## improvement

1. **The precedence sentence leaves too much obsolete plan text live.**
   - **Target location:** Opening, “Where section 13 conflicts with sections 3 to 11, section 13 wins.”
   - **Impact and evidence:** A fresh implementer must discover conflicts scattered across the goal (dynamic loading/no rebuild), P0–P5 ordering, tasks/artifacts location, SDK width, routes, and the earlier “project, goal, handoff stay core” table. Section 13 names only two formal amendments, so silence is ambiguous between “superseded” and “still binding.”
   - **Smallest useful correction:** Add a short supersession table listing each affected earlier section/decision, or move Sections 0–12 to a historical appendix and make Section 13's contracts and slices the sole implementation brief.

2. **The SDK boundary is neither count-correct nor symbol-complete.**
   - **Target location:** Decision 25, “all 26 primitives in `ui/src/components/ui/`, all shared hooks, the typed api client, run-dock hooks.”
   - **Impact and evidence:** The baseline directory contains 25 files, not 26, and “all shared hooks” is not a stable API definition: `ui/src/hooks/` includes task- and transcript-specific hooks as well as genuinely shared hooks. The existing SDK intentionally exports a small named list (`ui/src/plugin-sdk/index.ts:1-17`). Different implementers can expose different public APIs while claiming compliance.
   - **Smallest useful correction:** Generate or hand-maintain an explicit export manifest of symbols proven by built-in bundles, and gate it with an API-surface snapshot. Avoid a numeric count in prose.

3. **The implementation brief is not present in the referenced source document at the claimed baseline.**
   - **Target location:** Opening, “2026-09-12 ... Section 13 records the next phase” and Section 13.1, “This section is the brief for the implementing agents.”
   - **Impact and evidence:** `PLUGINS-SCOPE.md` at the clean `cf4566f6` worktree ends at Section 12 (395 lines); it contains neither the 2026-09-12 status paragraph nor Section 13. A worker given only the committed ref and a path will read the old plan. This dispatch contains the newer text inline, but that is not a durable, line-addressable source for subsequent serial tasks.
   - **Smallest useful correction:** Promote the updated document before S0 and have every slice task reference its exact commit and Section 13 subsection.

# Missing Evidence or Assumptions

- No compile spike demonstrates that one C1 crate can contribute both Axum routes and Clap commands without a daemon↔plugin or CLI↔plugin dependency cycle.
- No canonical rule is stated for duplicate IDs between `shipped/plugins/<id>/` and `~/.orgasmic/user/plugins/<id>/`, or for whether a user manifest may claim `:COMPILED: tasks`. The current compiled descriptor pin (`crates/orgasmic-core/src/node_registry.rs:169-174`) is slated to move, but its replacement trust check is not defined.
- No collision policy exists for top-level `:CLI:` verbs, UI page paths/nav entries, node actions, dock panes, commands, event topics, slot providers, or capability names.
- No generic `Singleton` read/write/CLI/UI wire format is shown, and no unavailable/schema-mismatch behavior is specified for singleton files.
- No migration version bump, crash-recovery protocol, dry run, or rollback contract is stated for the multi-file S1 migration.
- No external-consumer compatibility matrix identifies exactly which phone-app and meetings calls break in S1–S4, despite S5 deferring their updates.
- No acceptance gate proves that a disabled compiled plugin contributes neither routes, hooks, projections, slots, actions, UI registrations, dispatch extras, topics, nor CLI execution while its data remains readable.

# Highest-value Verification Targets

1. Build a no-op `orgasmic-plugin-api` plus one fixture C1 plugin into both daemon and CLI; run `cargo tree` to prove acyclic dependencies and mount/invoke one route and one command through only the declared public context.
2. Run an S1 migration fixture containing multi-generation goals, handoff `GOAL_ID`, gotcha entries, legacy `term:` nodes/references, existing `#+plugin:` headers, and member grants; interrupt once between each filesystem operation, rerun, and verify byte/data preservation.
3. Exercise a per-project activation matrix with the same plugin enabled in project A and disabled in B across HTTP routes, generic writes, CLI, hooks, projections, slots, topics, UI, and dispatch; include tasks-disabled plus meeting-subject implementer dispatch.
4. Replay the full authorization/event matrix for admin, viewer, editor, removed-artifacts-role member, explicit dynamic action, plugin principal, and cross-project subscriber before opening `Action` and `Topic`.
5. At the end of every UI-moving slice, run source debug, Rust tests without npm, Vite dev deep links, production package verification, browser disable/re-enable, and Tauri CSP loading; specifically assert decision/glossary artifact actions remain present across S2→S3.
6. Move the stable release gate after the last core-to-plugin extraction intended for 0.1.0, and smoke the phone app and meetings plugin against the exact candidate runtime rather than a preceding slice.

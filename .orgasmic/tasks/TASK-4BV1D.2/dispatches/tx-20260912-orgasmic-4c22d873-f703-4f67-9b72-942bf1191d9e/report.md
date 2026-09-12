Participant: codex · openai · gpt-6-astra · effort xhigh

# Overall Assessment

Section 13 gives a clear intended destination and explicitly supersedes earlier decisions about built-ins, SDK width, routes, and migration. Those explicit reversals are not themselves contradictions. The problems are in the implementation contract: several required interfaces have no defined owner, the first three zero-code plugins cannot express the existing behavior described by the survey, and some proposed deletions or replacements target working infrastructure.

This independent critique contains 4 blocking findings, 8 risks, and 1 improvement. “Blocking” means that the affected slice needs a concrete correction or decision before an implementing agent can follow it faithfully; it does not mean all preparatory work must stop. No consensus or release verdict is asserted.

The reviewed target is the complete document supplied in the dispatch, especially section 13. The checked-out `PLUGINS-SCOPE.md` does not contain section 13, so target locations below refer to the supplied document's headings and quotations. All repository evidence below is from `cf4566f6dfb71b84ef2a657d0fa0e1075d598531`, the exact commit named by section 13, on branch `mm-4bv1d-r1-critic-2`. No other participant's report was consulted.

# Findings

## 1. [blocking] The trait exposes host-owned types without defining how the dependency direction can hold

**Target:** 13.5, “plugin crates depend on core and this crate”; `routes() -> axum::Router<AppState>` and `run_cli(..., ctx: &CliContext)`; 13.4 decision 34 and S1's trait gate.

**Evidence:** The actual HTTP state is `ApiState`, defined in `crates/orgasmic-daemon/src/api.rs:192`, not an existing neutral `AppState`. Its fields include daemon-owned `WriterHandle`, `Supervisor`, `Index`, `AuthState`, event bus, plugin registry, and recovery state. The existing goal handlers, for example, take `State<ApiState>`. The brief gives neither a definition nor a crate owner for the proposed `AppState`, `CliContext`, or `SlotProvider`.

**Impact:** Importing the existing daemon state into a plugin crate would create the forbidden dependency cycle once the daemon imports that plugin. Moving the entire state into the API crate also pulls its daemon service dependencies with it. An implementer must invent the shared service boundary before moving any real handler. The S1 proof uses three explicitly zero-code plugins, so it can pass without proving that a compiled route, projection, and CLI implementation can cross this boundary.

**Smallest correction:** Specify the owning crate and minimal service handles for these context types, including project and caller identity. Make S1 compile and exercise one actual compiled handler and CLI operation with the stated dependency direction. Keep registration metadata static and put project activation checks at an explicitly identified invocation boundary; most shown accessors have no project argument.

## 2. [blocking] The singleton descriptor does not express goal history or the surviving singleton commands

**Target:** 13.4 decisions 19 and 37; 13.5 `** Singleton goal`, `:ID: goal-current`; 13.7 S1, “goal, handoff and gotchas moved through all of it.”

**Evidence:** `crates/orgasmic-cli/src/goal.rs:9-65` exposes set, clear, and supersede. In `crates/orgasmic-daemon/src/api.rs:19942-20001`, goal writes render distinct IDs, retain historical headings, and mark superseded/cleared goals with attribution and timestamps. `post_goal_set` at 20060 onward mints `goal-YYYYMMDD-...`, prepends the new goal, and calls `sync_handoff_goal_id` at 20186. Gotchas are nested entries addressed by title, with duplicate-title and structural-body validation, not one generic node: `crates/orgasmic-cli/src/gotcha.rs:1-5, 46-75`.

**Impact:** `PATH`, one fixed `ID`, and `DELETABLE` describe storage addressing, not these operations. Generic node CLI aliases do not explain how `gotcha add/list` handles child headings. A fixed goal ID also leaves historical goal IDs and existing `GOAL_ID` references unresolved. The promised goal dispatch-extras behavior is a compiled trait method even though goal is declared zero-code. Existing goal-to-handoff synchronization needs a policy when either plugin is disabled.

**Smallest correction:** Define whether “singleton” means one file containing many records or one node. State the retained operations, history/addressing rules, and goal/handoff interaction. Either specify declarative mechanisms that implement those semantics or explicitly record their removal and reference migration. A permission to break CLI names does not identify which stored history or behavior should disappear.

## 3. [blocking] S1 deletes the active skill-content loader as though it were obsolete content lifecycle code

**Target:** 13.2 “content lifecycle”; 13.4 decision 39, “daemon `content.rs`”; 13.7 S1 deletions.

**Evidence:** `crates/orgasmic-daemon/src/content.rs:1-2` identifies itself as loader-backed manager skill content. `list_skills` and `load_skill` at 98-110 load shipped and user skills. They are used by `/skills` handlers in `api.rs:3718-3733`, dispatch skill manifests at `api.rs:5932-5953`, and prompt compilation at `prompt_compiler.rs:937-959`. The obsolete optional/hub lifecycle implementation is separately visible in `crates/orgasmic-cli/src/content_lifecycle.rs:1-6, 61-186`.

**Impact:** Literal deletion breaks ordinary skill listing and prompt compilation, including the shipped orgasmic skill that decision 31 explicitly retains. Marketplaces do not replace those runtime reads. This is a wrong removal target, not merely an outdated line number.

**Smallest correction:** Limit deletion to optional/hub lifecycle behavior and its callers. Preserve or explicitly relocate the active skill-loading functions before removing their module, and require a shipped/user skill listing plus `skills.all` prompt-compilation check in S1.

## 4. [blocking] The new event shape drops the field used to enforce project isolation

**Target:** 13.4 decision 21 and 13.5 Events, `NodeChanged { collection, node_id }`; opening `Topic` to manifest-declared strings.

**Evidence:** Existing `TaskUpdated`, `ArtifactChanged`, and `ArtifactCommentAdded` carry `project_id` in `crates/orgasmic-daemon/src/events.rs:66-69, 133-142`. `EventPayload::project_id` exposes it at 145-176. The event envelope at 180-185 has no separate project field. `authz::event_visible` at `crates/orgasmic-daemon/src/authz.rs:331-346` allows project-less payloads once their topic is allowed. `allowed_topics` at 304-325 also depends on the closed task/artifact capability mapping.

**Impact:** Translating the shown payload literally removes the information needed to filter updates to the member's granted projects. Opening topic names alone defines publishing vocabulary, not subscription authorization. Depending on the chosen fallback, newly extracted plugins either receive no events or disclose changes from other projects.

**Smallest correction:** Retain mandatory project identity on node/plugin events, or place it in a required envelope field. Define how each plugin topic maps to read authorization and validate producer ownership separately. Require two projects and a member granted only one to prove that subscriptions cannot reveal the other's node updates.

## 5. [risk] Disable has no specified outcome for already-running work or generic write routes

**Target:** 13.4 decision 35, “nodes read-only, dispatch refuses that collection as a subject”; 13.5 “hooks are skipped”; sections 5 and 7's disable lifecycle.

**Evidence:** The generic edit writer currently performs plugin/schema checks inside its mutation transform, then runs compiled validation: `crates/orgasmic-daemon/src/api.rs:18718-18757`. Generic create/edit also acquire the plugin operation guard, e.g. `api.rs:17245, 18076`. Specialized tasks have an independent writer path at `api.rs:20334` onward. The goal/handoff sync currently invokes the generic writer with `plugin_scope: None` at `api.rs:20212`. Runs and their release/finalization lifecycle remain core under decision 12.

**Impact:** Unmounting routes and skipping hooks is not sufficient to make data read-only. It leaves unspecified whether comments, raw org writes, core chat tools, dispatched workers, queued mutations, and completion callbacks may still write. Disabling tasks during a live dispatch can also make its required reporting/close operation unavailable while the supervisor still owns its lease.

**Smallest correction:** State the disable boundary and policy for active runs: reject disable while busy, drain, cancel, or another explicit choice. Require an ownership/activation refusal before generic writes rather than treating missing hooks as permission. Cover in-flight mutation and terminal report/lease cleanup in the disable test, not only nav removal and a new dispatch refusal.

## 6. [risk] Required and optional plugin dependencies are assumed but absent from the loader contract

**Target:** 13.4 decisions 19 and 36, tasks optionally uses goal and forum needs tasks; 13.5 Services; sections 1 and 4's `REQUIRES`/`OPTIONAL` semantics.

**Evidence:** At the pinned commit, `PluginManifest` in `crates/orgasmic-core/src/plugin.rs:19-31` stores neither requirements nor optional dependencies nor provided services. Parsing `REQUIRES` at 108-114 only checks a fixed set of core names. It does not retain a dependency graph, and `OPTIONAL` is not consumed into the parsed manifest. Adding `core.dispatch@1` to `KNOWN_SERVICES` alone cannot make task/goal/forum activation reactive. Forum also submits an artifact in `crates/orgasmic-cli/src/forum.rs:2794-2814`; artifacts is a further functional dependency not named by decision 36.

**Impact:** A fresh agent has to choose syntax for plugin dependencies, handling of missing providers, and how dependent plugins respond to disable/re-enable. Forum may remain enabled when tasks or artifacts are unavailable. Optional goal dispatch metadata may remain active after disabling goal.

**Smallest correction:** Add retained dependency declarations and a small per-project activation rule to S1, with missing-dependency diagnostics and reactivation behavior. Name forum's artifacts requirement or make artifact publication explicitly optional with defined behavior. Verify task/goal and forum/task combinations instead of assuming the existing manifest parser provides this.

## 7. [risk] `core.dispatch` conflates acquiring a run with the full task-dispatch lifecycle

**Target:** 13.5 Services, “acquire a run against any subject node id, wait on it, close it with a verdict”; 13.4 decision 36; 13.7 S4 and S6.

**Evidence:** `manager::dispatch_quiet` returns a `started_tx` generation, not just a supervisor run, and runs dispatch planning/worktree guards (`crates/orgasmic-cli/src/manager.rs:1081-1105`). Forum persists that generation, waits for reported/died/timed-out outcomes, then closes with `report_only`, branch/worktree cleanup policy, and promoted report verification (`forum.rs:2856-2983`). `dispatch_close_inner` also reconciles torn closes and resolves the live project (`manager.rs:1357-1375`). Meanwhile `AcquireRequest` in `supervisor.rs:179` is the lower-level run acquisition input.

**Impact:** A thin supervisor wrapper loses semantics forum relies on, including resume identity and report-only promotion. Copying the task-oriented manager lifecycle wholesale into core defeats S4's task extraction. The brief specifies neither public request/result types nor the boundary between core generation handling and task state/evidence updates. Existing-manifest resume cannot be verified from “wait, close” alone.

**Smallest correction:** Define the service's generation handle, wait outcomes, report/promotion result, close idempotency, and caller-selected cleanup policy using the existing behavior as the baseline. Identify task lifecycle callbacks explicitly. Prove both a task report-only dispatch and a meeting-subject run through the service before moving forum.

## 8. [risk] The migration instruction is too broad and has no collision or interrupted-cutover rule

**Target:** 13.4 decisions 22 and 40, “stamp ... on every node,” rewrite `term:` to `term_`, move goal/handoff; 13.7 S1; section 5.1 schema refusal.

**Evidence:** Existing manifests can use schema 2 and accept schema 1 (`crates/orgasmic-core/src/plugin.rs:93-103, 376, 480-481`). Existing goal/handoff paths are under tasks (`crates/orgasmic-core/src/paths.rs:107-118`), while ID resolution deliberately accepts both glossary prefixes (`node_registry.rs:227-234`). The target does not restrict stamping to unstamped built-ins, name all live reference surfaces, or say what happens when both source/destination files or both normalized IDs exist.

**Impact:** Literal global stamping could relabel a third-party schema as version 1. Partial ID/path moves can strand references or leave mixed layouts after restart. “One step” does not specify atomicity, idempotency, live-writer exclusion, or recovery, and permission to break layouts does not authorize choosing which colliding record survives.

**Smallest correction:** Bound stamping to the intended built-ins and preserve existing plugin/schema headers. Define collision refusal, reference rewrite scope, handling of historical records, and a daemon-owned resumable or atomic cutover. Test duplicate `term:`/`term_` IDs, both singleton paths present, a schema-2 user node, interruption, and a second migration invocation.

## 9. [risk] Minting action names does not define equivalent authorization across plugin and generic routes

**Target:** 13.4 decision 23 and 13.5 Authz, “`authz::require` unchanged” and “the `artifacts` role ... becomes an ordinary grant”; 13.5 retention of generic node/comments routes.

**Evidence:** `Action`, `Action::ALL`, `from_name`, and role capability tables are closed in `crates/orgasmic-daemon/src/authz.rs:17-154`. `Identity::Plugin` stores approved capabilities and the initiating caller; `require` maps existing core actions to plugin capabilities at 240-270. Thus the current manifest's `nodes.write` is a requested core permission, whereas the new `plugin.<id>.<cap>` is described as a plugin-defined action. These are different meanings. Explicit member actions are stored alongside project roles, not as an independently scoped grant per action (`authz.rs:176-184, 266-277`).

**Impact:** There is no concrete mapping for old artifact-role members, editor/viewer defaults, a plugin's calls to core services, or the retained generic routes. A caller might be refused by an artifact plugin route yet allowed through a less-specific generic operation, or existing users might silently lose access. Calling `require` at every route does not resolve which action each route must require.

**Smallest correction:** Give a compact operation-to-action table for generic and plugin paths, separate requested core permissions from actions a plugin defines, and specify migration or explicit rejection of old grants. Test the same node operation through both routes under admin, viewer/editor, a former artifact member, and a plugin principal with a restricted caller.

## 10. [risk] The slice order breaks consumers before assigning their updates and publishes before the stated extraction is complete

**Target:** 13.7 S1 route namespace, S3/S4 route deletion, S5 apps/meetings update and stable release, S6 forum; decision 33 “one breaking release.”

**Evidence:** Forum currently calls `/projects/{p}/tasks` for creation/state and `/artifacts/{id}/submit` for publication (`crates/orgasmic-cli/src/forum.rs:2707, 2733, 2742, 2803`). Those producers move in S3/S4, but forum's adaptation is assigned to S6. If S1 applies the namespace globally, the same consumer problem starts even earlier. S1 also requires the phone app to work, while S5 is the first named app-path/topic update. S5 makes stable before S6 reaches “CLI has no forum code.”

**Impact:** “Each ends green” lacks an executable sequencing interpretation. An agent must either update downstream consumers outside its named slice, keep aliases the document forbids, or temporarily ship broken paths. Stable at S5 also leaves one explicitly planned built-in workflow outside the plugin kernel architecture.

**Smallest correction:** Assign every consumer update to the slice that changes its producer, even if moving the consumer's source happens later. Define whether namespace cutover is per slice or atomic at release. Put the final stable release after S6, or explicitly state that S5 intentionally releases an intermediate architecture and what compatibility is guaranteed through S6.

## 11. [risk] The prescribed CSP replaces an existing working policy and removes the import-map allowance

**Target:** 13.4 decision 29, “Added in the trait slice” and `script-src 'self' /plugins/*`; sections 2.2 and 7 P3.

**Evidence:** The pinned daemon already creates a nonce, adds it to the inline import map, and sends a multi-directive CSP in `crates/orgasmic-daemon/src/api.rs:1416-1426`. It also has distinct plugin-asset and prototype-frame policies at 1293-1311. Tauri's `csp: null` is real, but it is not evidence of an absent daemon policy.

**Impact:** Replacing the existing script directive with the specified value removes authorization for the inline import map. `/plugins/*` is not a valid standalone CSP host-source; `'self'` already permits same-origin plugin assets. Inline scripts need an applicable nonce/hash allowance. These syntax and inline-execution requirements follow the [W3C CSP specification](https://www.w3.org/TR/CSP3/#grammardef-host-source) and its [script directive requirements](https://www.w3.org/TR/CSP3/#script-src). This is a predicted regression from the proposed replacement, not a claim that the current runtime is broken.

**Smallest correction:** Correct the survey and preserve the nonce-bearing daemon policy while specifying the separate Tauri change. Verify import-map resolution and plugin activation in the actual daemon-served and app flows; retain the artifact prototype sandbox.

## 12. [risk] The zero-code edge contract does not establish all existing PARENT rules or one relationship authority

**Target:** 13.5 `:EDGES: PARENT maxItems=1 acyclic same-collection no-delete-with-children`; 13.6 “cover every `:PARENT:` rule”; 13.4 decision 28 and sections 3.2/7 `core.links`.

**Evidence:** `validate_decision_create_parent` explicitly calls `validate_parent_exists` in `crates/orgasmic-daemon/src/api.rs:17135-17154`. A same-collection-looking ID can still name a missing node, so the four named constraints do not explicitly preserve this guard. Existing PARENT is a heading property (`api.rs:17082-17089`), while `core.links` stores separate source/target/kind/revision records (`crates/orgasmic-core/src/node_services.rs:19-32`). The descriptor syntax does not say whether generic tree/backlink projections consume properties, link records, or both. `:UNIQUE: title CANONICAL` likewise leaves normalization and independent-versus-composite uniqueness unspecified, whereas glossary currently checks each independently after trim plus Unicode lowercase (`api.rs:16878-16935`).

**Impact:** A literal implementation can admit a missing parent, disagree between property and link views, or change duplicate detection while claiming no correctness loss. Concurrent parent/delete operations also need the declared constraints enforced on the committed graph, not merely on a stale projection.

**Smallest correction:** Define target existence, identity normalization, and the authoritative storage for declared edges and uniqueness. State that all write paths enforce them at the transaction boundary. Reuse the current PARENT cases as acceptance cases, including missing parent, cycle, reparent/delete, and glossary title/canonical normalization.

## 13. [improvement] Several agreed obligations are not assigned to any slice's acceptance gate

**Target:** 13.4 decisions 25, 26, 30, and 36; 13.5 UI SDK; 13.7 S1/S6; 13.8 artifact chat-context trap.

**Evidence:** S1 lists `registerPage` and `registerNodeAction`, but no slice explicitly assigns `registerDockPane` or `registerCommand`. Decision 36 moves all seven forum prompt specs; S6 names only `forum-reviewer.org`. The artifact chat-context fork is acknowledged at `conversations.rs:2072`, but the trait has no context-provider method and S3 does not assign its replacement. The actual UI runtime currently exposes only its existing node view/styles and other context methods (`ui/src/lib/pluginRuntime.tsx:17-40`).

**Impact:** These are closed decisions that can be missed while every named slice exit still passes. Conversely, an agent may widen S1 to finish all implied SDK work without an explicit boundary.

**Smallest correction:** Add an owner slice and one acceptance check for each obligation, enumerate the seven prompt files in S6, and choose the generic or provider-based artifact chat-context replacement before S3. Use parseable manifest fixtures for the final syntax rather than leaving the S1 `:SINGLETON:` label and the `** Singleton` example to be reconciled by the implementer.

# Missing Evidence or Assumptions

- Section 13 is supplied in the dispatch but absent from the checked-out `PLUGINS-SCOPE.md`; this report evaluates the supplied text, not an assumed committed revision of it.
- The claimed four-way survey and prior approval were not used as proof. Named code paths were independently inspected. Line counts and every enumerated site were not exhaustively recounted.
- The requested dependency direction is assumed to exclude plugins importing the daemon or CLI implementation crates. No hidden neutral-context crate or private follow-on contract was assumed.
- Breakage of API names and ledger layout is authorized by the target. Preservation of existing history, authorization scope, and dispatch cleanup still needs explicit transformation or removal semantics; the report does not demand compatibility aliases.
- No app/browser execution, migration, plugin enable/disable, worker launch, build, or test suite was performed. The findings describe document contradictions and implications supported by source inspection; proposed runtime failures are labeled as predictions.
- The cited paper and companion meetings specification were not independently evaluated. The critique does not assess the paper's claims or certify the earlier P0-P5 completion statement.

# Highest-value Verification Targets

1. **S1 boundary proof:** compile a real compiled plugin route and CLI operation using only the declared dependencies; demonstrate a caller/project context, writer access, and per-project activation without importing host implementation crates.
2. **Singleton conversion fixture:** migrate multiple historical goals plus a current handoff and nested gotchas; set/clear/supersede, read historical goal references, disable each singleton independently, then retry migration.
3. **Authorization and disable probe:** use two projects and a one-project member; exercise generic/plugin edits, comments, subscriptions, and a live dispatch crossing disable. Verify no unauthorized writes/events and no orphaned terminal report or lease.
4. **Consumer cutover check:** before each route/event removal, exercise the phone flow, forum task creation/report publication, and meetings. Confirm exact-generation report-only dispatch promotion remains resumable.
5. **Retained infrastructure check:** compile a prompt containing `skills.all`, list shipped/user skills, load the nonce-bearing import map and plugin UI, and verify the artifact prototype remains sandboxed.
6. **Generic graph parity:** missing-parent rejection, multi-node cycle, delete/reparent race, agreement between edge/property/backlink views, and independently normalized glossary title/CANONICAL uniqueness.

# Changed

- Created only `/tmp/TASK-4BV1D.2-report.md` as the dispatch report.
- No project source, target, glossary, decision record, or ledger file was edited by hand. No commit was requested.

# Verification Gates

- `orgasmic entry` resolved the live ledger at `/Users/aspirational/.orgasmic/ledgers/orgasmic`.
- `orgasmic task get TASK-4BV1D.2` confirmed the participant title, report-only assignment, in-progress state, and run `run-01M2BGRP2V1TN8R1C54V68TN38`.
- `git status --short --branch` returned only `## mm-4bv1d-r1-critic-2` both initially and after report creation; `git diff --exit-code` passed.
- `git log -1 --format='%H %s'` and `git show -s --format='%H %s' cf4566f6` both returned `cf4566f6dfb71b84ef2a657d0fa0e1075d598531`.
- Read the live `project.org` and `gotchas.org`, the supplied target in full, and the source excerpts named in each finding. Reproduction uses `sed -n '<start>,<end>p' <file>` or the named function in that pinned checkout; no moving-main assumption is needed.
- Example focused evidence searches: `rg -n 'CONTENT_SECURITY_POLICY|nonce-' crates/orgasmic-daemon/src/api.rs`; `rg -n 'content::|build_skill_manifest' crates/orgasmic-daemon/src/prompt_compiler.rs`; `rg -n 'dispatch_quiet|dispatch_wait_quiet|dispatch_close_quiet|/artifacts/|/tasks' crates/orgasmic-cli/src/forum.rs`.
- Consulted W3C CSP documentation only for CSP grammar/inline-script requirements. Repository-specific claims rely on the pinned source.
- Report structure validation passed: exact participant identity, 4 blocking/8 risk/1 improvement headings, and 13 instances each of Target, Evidence, Impact, and Smallest correction. The first ad hoc Ruby validator hit an environment incompatibility (`Array#tally` unavailable); rerunning it with `each_with_object(Hash.new(0))` passed. This was a validator error, not a project diagnostic.
- Inspected `orgasmic dispatch finalize --help`; the required terminal action is `orgasmic dispatch finalize --task TASK-4BV1D.2 --summary-file /tmp/TASK-4BV1D.2-report.md`, without `--commit`.

# Unmet Criteria

The critique/report criterion is covered by this document. CLI finalization follows report validation; this report does not pre-claim a successful finalize response or manager-owned close/promotion.

# Residual Risk

This is a static critique of an implementation brief, not a code-change certification. It prioritizes independently checked correctness, security, and sequencing issues; it does not claim an exhaustive blast-radius inventory. Runtime gates above remain work for the relevant implementation slices. Worker finalization and the manager's subsequent permanent report promotion/dispatch close are distinct lifecycle operations.

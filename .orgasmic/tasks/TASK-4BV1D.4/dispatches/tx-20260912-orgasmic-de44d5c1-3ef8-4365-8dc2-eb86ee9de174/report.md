# Reviewer

codex · openai · gpt-5.6-sol · effort xhigh

# Delta

= **Review basis.** I read the supplied target and both named promoted reports in full, and did not seek or read this participant's own stage-1 critique. The task's read scope excludes a fresh source-tree audit, so repository excerpts below are accepted or challenged by comparing each report's evidence to the target anchor it addresses; source re-probes remain explicit verification targets.

= **Project identity is mandatory in the replacement event.** `codex · openai · gpt-6-astra · effort xhigh` finding 4 and `claude · anthropic · opus[1m] · effort xhigh` B3 independently establish the same blocker against 13.4 decision 21 and 13.5 Events: the target specifies `NodeChanged { collection, node_id }`, while both reports show that the collapsing variants currently carry `project_id` and that event visibility uses it. The smallest sound contract is `NodeChanged { project_id, collection, node_id }`, with a two-project isolation gate.

= **Opening topics requires an authorization derivation rule.** `codex · openai · gpt-6-astra · effort xhigh` finding 9 and `claude · anthropic · opus[1m] · effort xhigh` B4 agree that 13.4 decisions 21 and 23 remove the closed `Action`/`Topic` bridge without replacing it. “Manifest capabilities mint actions” does not say which grant permits subscription to which declared topic, nor whether the generic and plugin routes require equivalent authority. This is a separate blocker from retaining `project_id`.

= **Plugin dependency policy is contradictory and unimplemented in the brief.** `codex · openai · gpt-6-astra · effort xhigh` finding 6 and `claude · anthropic · opus[1m] · effort xhigh` R1 correctly connect section 1/3.2's “core provides; plugins consume” rule and decision 14's rejection of plugin-to-plugin dependency to decision 19 (`tasks` optionally uses `goal`) and decision 36 (`forum` needs `tasks`). The reports also agree that the current `REQUIRES`/`OPTIONAL` contract does not express those edges. Section 13 must either permit declared plugin-service dependencies and define disable/reactivation, or move the shared behavior behind core services.

= **The C1 host boundary is not implementable from the shown signatures alone.** `codex · openai · gpt-6-astra · effort xhigh` finding 1 is supported by 13.5's simultaneous requirements that core never import a plugin and that the trait expose `Router<AppState>`, `CliContext`, and `SlotProvider` without naming their owner or minimal service surface. Its additional observation is decisive: most accessors cannot “consult per-project activation” because they receive no project context. S1 needs one real compiled route and CLI operation as its boundary proof; zero-code singletons cannot prove this.

= **The singleton contract does not carry the behavior S1 claims to preserve.** `codex · openai · gpt-6-astra · effort xhigh` finding 2 correctly anchors to decisions 19/37, the fixed `:ID: goal-current` sketch, and S1's “moved through all of it” exit. The reported current goal history, set/clear/supersede operations, nested gotcha entries, and goal/handoff synchronization are not expressible by `PATH`, `ID`, and `DELETABLE`. The inconsistency is visible inside the target too: 13.5 comments that `dispatch_extras` handles “goal's GOAL_ID stamp” even though goal is declared zero-code.

= **S1 names the wrong daemon module for deletion.** `codex · openai · gpt-6-astra · effort xhigh` finding 3 shows that 13.4 decision 39's literal deletion of daemon `content.rs` would remove active shipped/user skill loading used by prompt compilation, while decision 31 explicitly retains the shipped orgasmic skill and plugin skill bundles as references. The correction is to delete optional/hub lifecycle behavior and preserve or relocate the loader; marketplaces do not replace runtime skill reads.

= **The serial slices do not preserve their own green exits.** `codex · openai · gpt-6-astra · effort xhigh` finding 10 and `claude · anthropic · opus[1m] · effort xhigh` R8 converge on 13.7: S1 requires the phone app to work before S5 assigns app updates; S3/S4 remove artifact/task producers while forum adapts only in S6; S5 declares stable before forum extraction finishes. Consumer adaptation must travel with each producer cutover, and stable should follow S6 unless the target explicitly defines an intermediate supported release.

= **The CSP starting-state claim is stale.** `codex · openai · gpt-6-astra · effort xhigh` finding 11 directly challenges section 2.2's “No CSP anywhere” with a pinned-source nonce-bearing daemon CSP. Its correction is materially narrower than decision 29: preserve the nonce/import-map and prototype policies, add the matching Tauri policy, and test both daemon and app flows. The target's bare `/plugins/*` source expression should not be implemented literally.

? **The updater problem is real, but “whole-plan blocker” is inflated.** `claude · anthropic · opus[1m] · effort xhigh` B1 convincingly contradicts 13.8's `REQUIRED_RUNTIME_FILES` explanation and shows why an old installed CLI can reject a new bundle that removes `shipped/schema/node-types/`. That gates S5 distribution, not S1 architecture work, and “every existing install loses auto-update” is conditional on actually omitting the old directory. The brief must choose a one-release compatibility directory or an explicit manual reinstall; it need not stop the earlier slices.

? **The absent shipped-plugin root is an S1 work omission, not proven to require its own slice.** `claude · anthropic · opus[1m] · effort xhigh` B2 supplies strong evidence that current discovery and lifecycle paths are user-tier-only, so 13.3's “sixteen” list is incomplete. But S1 already assigns daemon/CLI boot wiring and moving three shipped plugins. Calling the root work “a slice of its own” is unsupported sizing; the actual missing decisions are default activation and whether shipped plugins can be removed.

? **A serde alias is one remedy, not the decision.** `claude · anthropic · opus[1m] · effort xhigh` B5 correctly disproves S0's “no behavior change” framing because `artifactor` is a serialized and user-overridable value. Its prescribed compatibility alias conflicts with decision 15's authorized breakage and the project's greenfield constraint against default shims. Section 13 must explicitly choose alias, migration, or clear rejection of old values and then change S0's exit accordingly.

? **Packaging need not dirty the tracked tree.** `claude · anthropic · opus[1m] · effort xhigh` R3 correctly finds that decision 20 omits the source-development and packaging-output story, but “must write into the tracked tree” is not forced: the package step can build directly into staging or an ignored output tree before assembly. The actionable gap is to name those paths and prove plain development plus packaged runtime loading.

+ **“Every node type” contradicts the conversations exception.** Both reviewed critiques discuss where the conversations descriptor would live, but neither states the direct scope contradiction: 13.1 says every node type ships as a plugin and core is “nothing else,” while 13.2 explicitly calls conversations a node type with compiled hooks and keeps it core under decision 14. The goal must name conversations as a kernel exception or make it a non-disableable core plugin; an implementer cannot satisfy both statements literally.

+ **“One node type per plugin” is ambiguous against plugins with none.** 13.3 gap 11 says one node type per plugin is kept and “no built-in needs more,” but decision 36 defines forum as a compiled plugin with no node type, while goal, handoff, and gotchas use singleton headings rather than node types. Specify “at most one node type” and make the manifest's node section optional, or define a separate workflow/singleton plugin shape.

+ **Default activation for existing ledgers is undefined.** Decision 10 says shipped plugins are enabled by default, decision 35 makes every built-in disableable per project, and 13.5 says every accessor consults `.orgasmic/plugins.json`. The target never says what absence means for a pre-migration ledger, whether migration writes explicit entries, or how a user's disable survives reinstall/update. This must be fixed before S1's disable gate can be deterministic.

+ **The meetings caller does not fit the proposed `core.dispatch` transport.** 13.5 calls `core.dispatch@1` a CLI-side, in-process service exposed through `CliContext`, then names meetings as its third caller. Earlier section 9 keeps meetings commands as external `orgasmic plugin run meetings <command>` executables receiving a daemon URL and scoped token. No HTTP/RPC bridge is assigned before deferred P4. The target must either expose `core.dispatch` through an authenticated daemon API for `plugin run`, or stop naming meetings as a caller of the in-process service.

+ **Historical removed worker kinds need an explicit read policy.** Decision 38 deletes `Griller` and `Planner` in S1 while decision 21 opens `WorkerKind` to validated manifest strings. The target does not say whether existing run, governance, and dispatch records containing `griller`/`planner` remain readable, are migrated, or intentionally fail under decision 15. This is a data-read decision, not merely removal of two CLI verbs.

# Cross-critique Contradictions

= **No direct evidence-level contradiction was found.** The two reports independently converge on the lost event `project_id`, missing topic authorization mapping, absent dependency semantics, and S1 sequencing risk. Their distinct findings are mostly additive rather than mutually exclusive.

? **Their main disagreement is implicit severity and sequencing.** `claude · anthropic · opus[1m] · effort xhigh` treats updater compatibility and the absent shipped root as immediate blockers; `codex · openai · gpt-6-astra · effort xhigh` instead locates the first implementation blockers at the host context, singleton semantics, active content loader, and events. The discriminating reading is phase-specific: host/singleton/content/events block S1, the shipped root is missing S1 work, and old-updater compatibility blocks release in S5.

? **Their dependency corrections need reconciliation.** `claude · anthropic · opus[1m] · effort xhigh` frames tasks→goal and forum→tasks as violations of the no-plugin-dependency principle; `codex · openai · gpt-6-astra · effort xhigh` proposes retaining a dependency graph. Both are plausible repairs, but the target—not the implementer—must decide whether dependencies on declared plugin services are legal.

# Highest-value Verification Targets

= **S1 C1 boundary probe:** compile and exercise one actual compiled plugin route and top-level CLI verb using only the declared dependency direction, with caller identity, project identity, writer access, activation off/on, and no daemon/CLI implementation import from the plugin crate.

= **Two-project security probe:** a member granted only project A must receive no project B `NodeChanged`; the same non-admin member must receive authorized plugin topics and get identical allow/deny outcomes through generic and plugin routes.

= **Installed-updater probe:** validate a staged 0.1.0 runtime, first without and then with the compatibility descriptor directory, using the currently installed pre-0.1.0 CLI. Record whether update can be automatic or must be an explicit reinstall.

= **Singleton/default-state migration fixture:** start with historical goals, handoff, nested gotchas, absent `.orgasmic/plugins.json`, and collision cases; migrate twice, disable/re-enable each plugin, and verify history, references, current-goal dispatch extras, and persisted disable choices.

= **Serial consumer matrix:** at every route/topic removal, run the phone path, forum report-only promotion/resume, meetings `plugin run`, and packaged plus development UI loading. No slice is green while its downstream consumer waits for a later slice.

# Critiques Reviewed

= **TASK-4BV1D.2** — codex · openai · gpt-6-astra · effort xhigh — promoted report at the dispatch-named path.

= **TASK-4BV1D.3** — claude · anthropic · opus[1m] · effort xhigh — promoted report at the dispatch-named path.

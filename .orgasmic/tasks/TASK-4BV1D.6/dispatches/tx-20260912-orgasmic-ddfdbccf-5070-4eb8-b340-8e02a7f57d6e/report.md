# Reviewer

claude · anthropic · opus[1m] · effort xhigh

Blind cross-review of two promoted critiques of `PLUGINS-SCOPE.md` section 13. My own
stage-1 critique was not read, sought, or inferred. All repository evidence below was
re-derived independently in worktree `/Users/aspirational/.orgasmic/worktrees/orgasmic/task-4bv1d.6`
at `cf4566f6dfb71b84ef2a657d0fa0e1075d598531` (branch `mm-4bv1d-r1-review-6`, clean tree).
No source file was edited and no test suite was run.

Counts reviewed: gpt-5.6-sol — 4 blocking, 10 risk, 3 improvement. gpt-6-astra —
4 blocking, 8 risk, 1 improvement.

# Delta

## Confirmed by both, verified independently

`=` **No `AppState`; no `CliContext`; no `SlotProvider`.** Both gpt-5.6-sol (blocking 1)
and gpt-6-astra (blocking 1) land this and both cite the right anchor. Verified:
`grep -rn 'struct AppState' crates/` returns nothing; the host state is
`pub struct ApiState` at `crates/orgasmic-daemon/src/api.rs:192`; `grep -rn 'CliContext'`
and `grep -rn 'SlotProvider'` over `crates/` return zero hits. Additional support neither
participant gave: the two existing nested routers are **crate-private** —
`pub(super) fn routes() -> Router<ApiState>` at `crates/orgasmic-daemon/src/node_services.rs:44`
and `crates/orgasmic-daemon/src/conversations.rs:18`. A plugin crate therefore cannot
reach the existing route-contribution pattern even with a dependency edge, which makes
the "specify the owning crate" correction mandatory rather than stylistic. Both
corrections are compatible; gpt-6-astra's added point — that S1's three zero-code
singletons can pass the gate without ever exercising a compiled route or CLI path — is
the sharper gate critique and should be adopted verbatim.

`=` **The `Singleton` shape cannot express goal or gotcha semantics.** gpt-5.6-sol
(blocking 3) and gpt-6-astra (blocking 2) reach the same verdict from the same code, and
both are accurate. Verified: `crates/orgasmic-cli/src/goal.rs:9-61` exposes `Set`, `Clear`,
`Supersede`; id minting `goal-{}-{}` at `api.rs:20111`; `SUPERSEDED` title rewrite plus
`SUPERSEDED_AT` at `api.rs:19960-19964`; `sync_handoff_goal_id` at `api.rs:20186`.
Gotchas: `crates/orgasmic-cli/src/gotcha.rs:14` pins `.orgasmic/gotchas.org`, entries are
`**` children addressed by title (`titles()` at `:49-53`), and `appended()` at `:56-75`
enforces one-line title, no headings in body, duplicate-title refusal, and a `:CREATED:`
stamp. `:PATH:` + one `:ID:` + `:DELETABLE:` describes storage addressing only. Neither
loss appears in 13.6. gpt-6-astra adds the strongest single observation on this finding:
goal is declared zero-code yet `dispatch_extras` is a **compiled trait method** — the
document's own mechanism for the `GOAL_ID` stamp contradicts its own tier assignment.

`=` **Decision 29's CSP item is stale and its replacement text is a regression.** Both
flag it (gpt-5.6-sol risk 10, gpt-6-astra risk 11). Verified at `api.rs:1417-1424`: the
daemon mints a per-response nonce, injects it into the inline import map, and sends
`default-src 'self'; script-src 'self' 'nonce-…'; …; object-src 'none'; base-uri 'none';
frame-ancestors 'none'`. Plugin assets get their own `default-src 'none'; sandbox`
(`api.rs:1292-1294`) and the prototype frame its own sandboxed policy (`api.rs:1309`).
Tests pin the nonce and the absence of `unsafe-eval` at
`crates/orgasmic-daemon/tests/plugin_registry_routes.rs:223-228`. gpt-6-astra's version is
strictly better and should be the one carried forward: it adds that `/plugins/*` is not a
valid CSP host-source, and that replacing `script-src` with the quoted fragment would
**revoke the nonce the inline import map depends on** — a concrete predicted regression,
not just a loss of directives. gpt-5.6-sol states the loss but not the mechanism.

`=` **Section 13 is absent from the committed document.** Both note it; gpt-5.6-sol makes
it an action item (improvement 3), gpt-6-astra only a scoping caveat. Verified:
`./PLUGINS-SCOPE.md` is 395 lines and its last `##` heading is `## 12. Marketplaces` at
line 362. gpt-5.6-sol's framing is the useful one — every downstream slice task is told to
reference "section 13 at `cf4566f6`", which does not exist at that ref. Promote before S0.

`=` **Opening `Topic` without defining subscription authorization is unsafe.** Both reach
this (gpt-5.6-sol risk 2, gpt-6-astra blocking 4) and both cite correctly.

## Material additions missing from both critiques

`+` **`shipped/plugins/` does not exist, and the loader has no shipped scan tier at all.**
Neither participant checked this, and it is load-bearing for every slice. Verified:
`ls shipped/plugins/` → absent. The plugin registry scans exactly one root —
`self.home.user().join("plugins")` at `crates/orgasmic-daemon/src/plugins.rs:170, 230, 278,
428, 464`; `marketplaces.rs:254, 321, 354` installs into the same single root. The five
built-in descriptors still live in `shipped/schema/node-types/{artifact,conversation,
decision,glossary,task}.org`, loaded through `NodeTypeRegistry`, not through `plugins.rs`.
Two consequences: (a) the document's opening claim that **P2 has landed** is overstated on
precisely the item section 13 builds on — section 7's P2 exit included "Move the four
built-in descriptors to `shipped/plugins/<id>/plugin.org` so built-ins and user plugins
share one loader", and that has not happened; (b) decision 10 ("pre-installed in
`shipped/plugins/<id>/`, enabled by default") requires a new shipped discovery tier plus
default-enabled activation semantics, and S1's work list never names either. A fresh agent
must invent both. This is blocking-tier for S1 scoping and sits underneath both
participants' blocking 1 without being the same finding.

`+` **The compiled-descriptor pin makes "every built-in is a plugin folder" a two-loader
merge, unaddressed by any slice.** Verified at `crates/orgasmic-core/src/node_registry.rs:171-174`:
`for builtin in Self::embedded()?.descriptors()` asserts the loaded descriptor equals the
compiled one — "compiled collection {} descriptor is pinned; user descriptors may only add
new collections". Section 13.5 says only that this pin "moves into each compiled crate". It
does not say how `NodeTypeRegistry`'s shipped-file discovery and `PluginRegistry`'s
plugin-folder discovery become one path, nor which wins when a compiled crate and a
`shipped/plugins/<id>/plugin.org` descriptor disagree. Related to gpt-6-astra's blocking 1
but distinct: that finding is about dependency direction, this one about discovery and
authority.

`+` **`REQUIRED_RUNTIME_FILES` and the uncommitted UI bundles are a live updater conflict.**
gpt-5.6-sol's risk 6 found the debug/test half of this (below) but not the updater half.
Verified: `crates/orgasmic-cli/src/update.rs:22-37` lists 10 paths and contains no
`shipped/plugins` entry. Decision 20 says built-in bundles are "built at package time, not
committed"; 13.5 says they must be listed in `REQUIRED_RUNTIME_FILES`; trap 13.8 says an
unlisted boot-needed file makes the updater roll back. So the updater would verify files
that only exist in a packaged tarball — correct for release, and guaranteed to mismatch any
source-built or partially-packaged runtime. Also worth recording for accuracy: section 2.1's
claim that "`orgasmic update` hardcodes the four descriptor paths in `REQUIRED_RUNTIME_FILES`"
is now **stale** — that P0 item already landed; no node-type descriptor appears in the list.

## gpt-5.6-sol findings gpt-6-astra missed, verified

`+` **The `#+orgasmic_plugin: <id>@<schema>` header in decision 22 is the wrong token.**
gpt-5.6-sol risk 8, unique and correct. Verified: the landed writer and write gate use
`#+plugin: <id> schema=<n>` — `crates/orgasmic-daemon/src/plugins.rs:47` strips prefix
`"#+plugin: "`, `:69` formats `"#+plugin: {id} schema={}"`, and tests pin the exact string
(`tests/plugin_registry_routes.rs:491`, `crates/orgasmic-cli/tests/plugin_cli.rs:75`).
`grep -rn 'orgasmic_plugin' crates/` returns **zero** hits. Section 4 of the document
repeats the same wrong token, so this is the one defect that would make the S1 migration
stamp data the running write gate then refuses. gpt-6-astra's risk 8 says "preserve
existing plugin/schema headers" but never names the mismatch, so it would not stop an agent
from implementing the document literally.

`+` **Built-in UI bundles have no debug/test load path.** gpt-5.6-sol risk 6, unique and
correct. Verified: `crates/orgasmic-core/src/plugin.rs:221-223` —
`if manifest.ui.is_some() { manifest.ui_asset_path(dir, "index.js")?; }` inside
`read_dir`, so a declared `:UI:` with no file on disk is a hard manifest error; and
`crates/orgasmic-daemon/build.rs:36-81` deliberately skips npm outside `release`/`ORGASMIC_EMBED_UI=1`
and writes only a placeholder `index.html`. Once `shipped/plugins/{artifacts,tasks}/plugin.org`
declare `:UI:`, ordinary `cargo test` and source-daemon startup reject the pre-installed
plugins. Combined with my `REQUIRED_RUNTIME_FILES` item above, this is one contract gap with
two failure modes (dev build, updater rollback) and deserves a single named development
contract in S1.

`+` **"Both thin wrappers over `node_kernel::parse_journal`" is false.** gpt-5.6-sol risk 3,
unique and correct. Verified in the route table: task comments are three routes —
`("POST", "/tasks/:id/comments")` at `api.rs:978`, `.../comments/:entry_id/edit` at `:979`,
`.../comments/:entry_id/delete` at `:980`; artifact comments are two with different
semantics — `("POST", "/artifacts/:id/comments")` at `:999` and `.../comments/:cid/resolve`
at `:1000`. Five routes, three distinct mutation contracts, and `parse_journal` is a read
helper. S3's exit requires artifact comments to keep working, so collapsing these to one
generic route as written drops edit, delete, and resolve.

`+` **Moving all of `retro.rs` behind tasks removes a run-only diagnostic.** gpt-5.6-sol
risk 9, unique and correct. Verified: `crates/orgasmic-daemon/src/retro.rs:21-30` —
`Selectors { runs, tasks, task_sequence, questions }`, all `#[serde(default)]`, so `runs`
is independent of `tasks`. With tasks disabled under decision 35, `orgasmic manager retro
--run …` disappears while run supervision stays core. Not listed as an accepted loss.

## gpt-6-astra findings gpt-5.6-sol missed, verified

`+` **Decision 39 names the wrong deletion target: daemon `content.rs` is the live skill
loader.** gpt-6-astra blocking 3, unique and the single highest-value finding in either
report. Verified: `crates/orgasmic-daemon/src/content.rs:2` is `//! Loader-backed manager
skill content.`; `list_skills` at `:98` and `load_skill` at `:106` read
`shipped/skills` and `user/skills`; callers are the `/skills` handlers
(`api.rs:3721-3731`), the dispatch skill manifest (`api.rs:5933`), prompt compilation
(`prompt_compiler.rs:938`), and the public re-export `lib.rs:73`. Decisive check
gpt-6-astra did not run but which closes the question: `grep -n 'optional\|hub'
crates/orgasmic-daemon/src/content.rs` returns **zero hits** — the obsolete registries are
entirely in `crates/orgasmic-cli/src/content_lifecycle.rs:182, 186`. The 428-line figure in
13.2 matches `content.rs` exactly, so the document sized the right file and labelled the
wrong one. Literal execution of S1 breaks `/skills`, dispatch skill manifests, and
`skills.all` prompt compilation — including the shipped `orgasmic` skill that decision 31
explicitly retains.

`+` **Plugin-to-plugin dependencies do not exist in the manifest at all.** gpt-6-astra
risk 6, unique and correct. Verified: `PluginManifest`
(`crates/orgasmic-core/src/plugin.rs:18-31`) has no requires/optional/provides field;
`REQUIRES` is parsed at `:108-113` only to assert membership in `KNOWN_SERVICES`
(`:11-16`, four entries) and is then **discarded**; `OPTIONAL` never reaches the struct —
it appears only in test fixtures (`:427, 465`). Decision 19 (tasks `OPTIONAL` goal) and
decision 36 (forum needs tasks) therefore rest on a mechanism that is not merely unbuilt
but has no declared syntax. Adding `core.dispatch@1` to `KNOWN_SERVICES` cannot supply it.

`+` **Forum also depends on artifacts, unnamed by decision 36.** gpt-6-astra risk 6,
unique and correct. Verified: `crates/orgasmic-cli/src/forum.rs:2803` posts
`/artifacts/{artifact}/submit?project=…`, alongside `/projects/{p}/tasks` at `:2707, 2733,
2742`. Decision 36 names tasks and `core.dispatch@1` only.

`+` **`:EDGES: PARENT maxItems=1 acyclic same-collection no-delete-with-children` does not
cover target existence.** gpt-6-astra risk 12, unique and correct, and it directly
falsifies a document claim rather than merely flagging a gap. Verified:
`validate_decision_create_parent` at `api.rs:17124` calls
`orgasmic_core::validate_parent_exists` at `:17142`, reached from create at `:16819`. A
well-formed same-collection id can still name a missing node, so the four constraint words
do not reproduce this guard — and 13.6 asserts "The four constraint words in `:EDGES:` cover
every `:PARENT:` rule that exists today" and "None is a data or correctness loss." The
`:UNIQUE: title CANONICAL` half of the finding is equally sound: glossary checks each
independently after trim plus lowercase (`api.rs:16878-16935`), which the two-word syntax
does not express.

`+` **`core.dispatch@1` as "acquire, wait, close" is below the layer forum uses.**
gpt-6-astra risk 7, unique and correct. Verified: `manager::dispatch_quiet`
(`crates/orgasmic-cli/src/manager.rs:1081-1084`) returns a `started_tx` generation string
via `dispatch_inner`, which runs `build_dispatch_plan`, an empty-brief refusal, a
dirty-main-checkout guard, and cross-kind worktree-collision guards (`:1086-1106`);
`dispatch_close_inner` at `:1361` reconciles torn closes. `AcquireRequest.task_id`
(`crates/orgasmic-daemon/src/supervisor.rs:180`) is the far lower-level input. A thin
supervisor wrapper loses the resume identity and report-only promotion forum depends on.

`+` **Disable has no stated boundary for in-flight work or generic writes.** gpt-6-astra
risk 5, unique. The goal/handoff sync calling the generic writer with `plugin_scope: None`
is a concrete example of a core-originated write that activation checks would not see;
decision 35's "nav gone, routes 404, nodes read-only" does not cover a live dispatch whose
close path belongs to a plugin being disabled. Reasonably supported; I did not re-derive
every line cited in this item.

## Challenges

`?` **gpt-5.6-sol improvement 2 — "contains 25 files, not 26" is imprecise and the count
is the weakest part of an otherwise right finding.** Verified: `ls ui/src/components/ui/ |
wc -l` → **26**, of which 25 are `.tsx` primitives and one is the `__tests__` directory. So
the document's "26" is a defensible `ls` count and gpt-5.6-sol's flat "25 files" invites a
rebuttal that dissolves the finding. The substantive half is correct and should be kept
while dropping the number dispute: "all shared hooks" is not an API definition
(`ui/src/hooks/` mixes task- and transcript-specific hooks with genuinely shared ones), and
the landed SDK deliberately exports a narrow named list —
`ui/src/plugin-sdk/index.ts:1-17` exports 4 primitives (`Button`, `Card` family, `Input`,
`Textarea`), not 25. Decision 25 widens the surface roughly sixfold with no export manifest.
Recommend restating as an export-manifest-plus-snapshot requirement with no count in prose.

`?` **Severity split on the event-payload finding; gpt-6-astra's blocking rating is the
right one.** gpt-5.6-sol files it as risk 2, gpt-6-astra as blocking 4. The evidence favors
blocking: `TaskUpdated` (`crates/orgasmic-daemon/src/events.rs:66-69`), `ArtifactChanged`
(`:133-137`) and `ArtifactCommentAdded` (`:139-143`) each carry `project_id`;
`EventPayload::project_id()` at `:146-176` is what the authz WS filter consumes; and the
envelope `pub struct Event { seq, time, topic, payload }` at `:176-181` has **no project
field**, so there is nowhere for the identity to survive implicitly. The proposed
`NodeChanged { collection, node_id }` deletes the only carrier. The doc comment at
`:147-150` states the rule explicitly — payloads naming no project "pass once their topic is
allowed" — so a literal implementation converts every node write into an event that passes
the filter for members granted a different project. That is a cross-project disclosure on a
security seam, not a sequencing risk.

`?` **gpt-5.6-sol blocking 4 and gpt-6-astra risk 10 both argue S1 is overloaded, but
neither notices that the split they propose is unachievable as scoped.** gpt-5.6-sol's S1a
is "API crate, host wiring, activation gate, one inert fixture plugin". Per my
`shipped/plugins/` finding, an inert fixture plugin cannot be *pre-installed* today — there
is no shipped scan root — so S1a must also build the shipped tier and default-enabled
activation, or the fixture has to be hand-installed into `~/.orgasmic/user/plugins/` and the
gate proves nothing about decision 10. The split is still the right call; its contents need
the shipped-tier work named explicitly.

`?` **gpt-6-astra's preamble states "No other participant's report was consulted" and
gpt-5.6-sol makes the equivalent claim; both are consistent with their content.** I found no
shared wording, shared line-number errors, or shared omissions that would suggest contact.
The two reports converge on four findings and diverge on eleven, which is the expected
signature of genuinely blind runs. Recorded as a non-issue, not a challenge to either.

`?` **Neither critique assessed 13.7's "Estimated 6 to 8 weeks serial" or "S4 alone is
roughly half" against the line counts in 13.2.** 13.2 attributes ~5,642 api.rs lines,
`retro.rs` 812, ~7,217 manager.rs task lines, ~670 index.rs lines and ~26,000 test lines to
tasks. Verified `api.rs` is **46,955** lines today, not the 45,304 section 2.1 claims, so the
tree is growing under the plan's own feet — which is exactly the condition 13.7's closing
sentence warns about. Low severity and out of scope for a correctness critique, but a
manager reading either report would get no signal that the schedule rests on one stale
measurement.

# Cross-critique Contradictions

1. **Event payload severity — genuine, unresolved by the two reports.** gpt-6-astra rates
   the loss of `project_id` blocking; gpt-5.6-sol rates it risk. Not a difference of fact —
   both describe the same defect and cite compatible anchors. My read sides with
   gpt-6-astra on the evidence above. Flagged rather than silently merged.

2. **Whether the CSP item is stale-only or actively harmful.** gpt-5.6-sol: "Mark daemon CSP
   as landed and preserve its policy." gpt-6-astra: the same, **plus** the replacement string
   is syntactically invalid and revokes the import-map nonce. Not contradictory; gpt-6-astra
   strictly dominates. Curator should take gpt-6-astra's text and drop gpt-5.6-sol's as
   redundant.

3. **Scope of the migration defect.** gpt-5.6-sol risk 8 is about the *header token being
   wrong*; gpt-6-astra risk 8 is about *stamping being too broad, with no collision or
   interrupted-cutover rule*. Both are real and neither subsumes the other. They must be
   merged, not chosen between — fixing the token without bounding the scope still relabels a
   third-party schema-2 node as `schema=1`, and bounding the scope while keeping
   `#+orgasmic_plugin:` still produces nodes the landed write gate refuses.

4. **Where the S1 trait gate fails.** gpt-5.6-sol: the gate does not exist as a reviewable
   state because S1 bundles everything (blocking 4). gpt-6-astra: the gate exists but is
   *vacuous*, because three zero-code plugins never exercise a compiled route, projection, or
   CLI path (blocking 1, closing paragraph). Complementary framings of one defect, and the
   correction needs both halves: split the slice **and** require one compiled handler in the
   first half.

5. **Forum's dependency set.** gpt-5.6-sol risk 7 treats forum purely as a release-ordering
   problem (stable ships with `forum.rs` still in the CLI). gpt-6-astra risk 6 and 10 treat
   it as a dependency-declaration and consumer-cutover problem (forum needs artifacts, and
   its adaptation is assigned to S6 while its producers move in S3/S4). Both correct, aimed
   at different failure modes, and the union is the actionable statement.

6. **No factual contradiction found between the two reports.** Every overlapping anchor I
   re-derived was consistent. The only citation drift worth recording is immaterial:
   gpt-5.6-sol gives `goal.rs:9-71` where the enum ends at `:61`, and `api.rs:20181-20224`
   for `sync_handoff_goal_id` which begins at `:20186`; gpt-6-astra's `goal.rs:9-65` and
   `api.rs:20186` are tighter. Neither error changes a finding.

# Highest-value Verification Targets

Ordered by what would change the S1 plan if it failed.

1. **Shipped-plugin discovery spike, before anything else.** Create
   `shipped/plugins/<fixture>/plugin.org` and assert the daemon registers it with no entry
   under `~/.orgasmic/user/plugins/`. Expected result today: nothing is discovered
   (`plugins.rs:170` scans one root). This single check decides whether S1 is one slice or
   two and is not named in either critique.

2. **Acyclic compiled-plugin spike with a real handler.** Build `orgasmic-plugin-api` plus
   one fixture C1 crate, link it into both daemon and CLI, mount one route and one top-level
   verb through only the declared public context, and prove the graph with `cargo tree`. Must
   include one *compiled* handler and one *compiled* CLI op, per gpt-6-astra's objection —
   not three zero-code folders.

3. **Header-token and migration fixture.** Stamp a ledger containing multi-generation goals,
   a handoff `GOAL_ID`, nested gotchas, legacy `term:` nodes with inbound references, a
   schema-2 third-party node, and both singleton paths present. Assert the written token is
   `#+plugin: <id> schema=<n>` (`plugins.rs:69`), that `check_write` (`plugins.rs:38-63`)
   accepts every stamped node afterwards, and that the pre-existing schema-2 node is
   untouched. Interrupt between each filesystem operation, rerun, and diff.

4. **Skill-loader retention check in S1.** Before deleting anything named by decision 39,
   compile a prompt containing `skills.all`, list shipped and user skills over `/skills`, and
   fetch a dispatch skill manifest. All three route through `content.rs`. This is
   gpt-6-astra's finding and it is the cheapest catastrophic-regression gate in either report.

5. **Two-project, one-grant event and activation matrix.** A member granted project A only,
   subscribed while project B writes nodes in every collection; plus the same plugin enabled
   in A and disabled in B across HTTP routes, generic writes, comments, CLI, hooks,
   projections, slots, topics, UI, and a dispatch that crosses a disable. Gate `Topic` and
   `Action` opening on this passing.

6. **Dev/test/package UI-bundle contract.** Run `cargo test -p orgasmic-daemon` and a debug
   `cargo build` with `shipped/plugins/{artifacts,tasks}/plugin.org` declaring `:UI:`, then
   run the packaged-runtime path and `orgasmic update`'s `REQUIRED_RUNTIME_FILES` check
   (`update.rs:559`). Expected failure today at `plugin.rs:221-223`. One contract must cover
   all four.

7. **Generic graph parity against today's guards.** Missing parent (must reject, per
   `validate_parent_exists` at `api.rs:17142`), multi-node cycle, reparent/delete race,
   agreement between property view and link records, and glossary `title`/`CANONICAL`
   uniqueness normalized independently.

8. **Comment surface parity before S3.** Exercise all five existing routes
   (`api.rs:978-980, 999-1000`) against whatever generic replacement is proposed; assert
   edit, delete, resolve, and version anchoring survive.

9. **Consumer cutover per producer slice.** Before each route or topic removal, exercise the
   phone app, forum task creation and artifact submission (`forum.rs:2707, 2742, 2803`), and
   meetings; confirm exact-generation report-only dispatch still resumes. Then decide
   explicitly whether stable lands after S5 or after S6.

# Critiques Reviewed

- **TASK-4BV1D.1** — codex · openai · gpt-5.6-sol · effort xhigh. Report:
  `/Users/aspirational/.orgasmic/ledgers/orgasmic/.orgasmic/tasks/TASK-4BV1D.1/dispatches/tx-20260912-orgasmic-baf35d91-058a-49ba-bb8c-13e1f368d1c9/report.md`
- **TASK-4BV1D.2** — codex · openai · gpt-6-astra · effort xhigh. Report:
  `/Users/aspirational/.orgasmic/ledgers/orgasmic/.orgasmic/tasks/TASK-4BV1D.2/dispatches/tx-20260912-orgasmic-4c22d873-f703-4f67-9b72-942bf1191d9e/report.md`

My own stage-1 critique (TASK-4BV1D.6 stage 1) was not in the manifest, was not read, and
is not referenced. No consensus verdict is asserted; curation is the curator's stage.

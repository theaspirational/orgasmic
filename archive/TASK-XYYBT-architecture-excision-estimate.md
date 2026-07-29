# TASK-XYYBT — Cost estimate for excising the architecture layer

Exploration only. No source changes were made; the one runtime probe (question 6) was a
plain `cargo test` run against the unmodified tree. Worktree diff at report time:
`report.md` only.

**Recommendation: GO** — staged, subtractive, ~−3,500 to −4,500 LOC across ~90–110
files, 4–6 dispatch rounds plus manager integration. The runtime already treats
`architecture.org` as optional, so no new fallback machinery is needed; the only new
code is a ~4-line lint carve-out so history with dangling `arch_` edges stays green.
Decision draft at the end.

---

## 1. Inventory (actuals, 2026-07-29 tree, vs. filing-time claims)

| Surface | Filing claim | Actual (this worktree) | Evidence |
|---|---|---|---|
| `architecture.org` | 177 headings, 26 subsystems, ~2100 lines | **2,140 lines; 183 headings (29 top-level: 26 `arch_` subsystems + IMPLEMENTED/TARGET/Artifact-pseudo-nodes section heads); 38 `** arch_` leaves; 40 `:SOURCE_PATHS:` lines** | `.orgasmic/architecture.org` (IMPLEMENTED at :14, TARGET at :1243) |
| `architecture_drift.rs` | 457 lines | **457 lines** (unchanged) | `crates/orgasmic-cli/src/architecture_drift.rs` |
| `ArchitectureView.tsx` | 273 lines | **273 lines** (unchanged) | `ui/src/components/ArchitectureView.tsx` |
| `arch_` occurrences (source) | api 21, index 25, org 31, id 22, schema 21, drift 20, marker 10, identity_lint 8, session 8 | **api.rs 21, index.rs 25, org.rs 33, id.rs 22, schema.rs 21, drift 20, marker.rs 10, identity_lint.rs 8, session.rs 8** (session.rs's 8 are prose comments only) | grep -c per file |
| `architect` occurrences | api 94, main 29, manager 26, NodeModal 22, ArchView 15 | **api.rs 131, main.rs 48, manager.rs 40, index.rs 39, NodeModal.tsx 25, ArchitectureView.tsx 29, dispatch.rs(test) 25, integration.rs 21, fixtures.rs 25** — grew since filing | grep -ci per file |
| Marker files naming an `arch_` id | 51 files, 21 arch-only | **53 files / 58 marker lines. 20 lines are arch-only (no dec_/TASK on the line); 15 files have only-arch marker *lines*; but just 9 files have NO other dec_/TASK marker anywhere and truly lose their whole back-edge** | see §Q1 list |
| Legacy `// arch: arch_NNN.M` drift back-edges | (not counted at filing) | **73 lines across 72 files** (the drift comparator's authoritative marker, distinct from `// orgasmic:`) | `grep -rE '^\s*(//|#) (arch:|@arch )'` |
| State coupling | done 335, tx 151, decisions 15, goal 6, backlog 2 | **done.org 335 (of which 133 drawer lines / 231 `:IMPLEMENTS:` tokens — the lint-relevant subset), tx 151 (58+93), decisions.org 15 (ALL prose, 0 drawer), goal.org 6 (prose), backlog.org 2 live `:IMPLEMENTS:` (lines 38, 54) + this task's own body; glossary.org 4 `:PRIMARY_LEAF:` arch leaf refs (:355,:366,:377,:388); gotchas.org 2 prose; project.org 1 prose** | greps below |

Code surfaces confirmed present:

- **Worker kind / stage plumbing**: `WorkerKind::Architector` (`orgasmic-core/src/schema.rs:127`), `stage_spec("architect")` + `POST /architect` (`orgasmic-daemon/src/api.rs:691,3210,3234-3240`), `DispatchEndpointKind::Architector` → "architector-watch-then-integrate" (`api.rs:4299,5676`), run-kind mapping (`api.rs:5524`), manager close lifecycle `architector.done`/`architector.reported` (`orgasmic-cli/src/manager.rs:765,2949,2985,3805,3817,4036,4594`), supervisor last_path kinds (`orgasmic-daemon/src/supervisor.rs:5230`).
- **CLI**: `orgasmic architecture {list,get,drift,create,schema}` (`orgasmic-cli/src/main.rs:744-806`, dispatcher at `:3055`), `orgasmic architect` stage verb (`main.rs:341,1453`), mint class `architecture` (`main.rs:592`), `node get` resolution of `arch_` ids (`orgasmic-cli/src/node.rs`).
- **Read model**: `ArchitectureSummary`/`ArchitectureArtifactSummary`/`ArchitectureGraphNode`/`ArchitectureNodesResponse` + `load_architecture` (`orgasmic-daemon/src/index.rs:204-290,1066-1135`), routes `GET /architecture`, `/architecture/nodes`, `/architecture/:id`, `POST /architecture`, `POST /architecture/:id` (`api.rs:682-691,11436-11474,11637-11664`), `GraphLayer::Architecture` / `NodeLayer::Architecture` variants (`api.rs:11678,11719`), authz rows (`api.rs:761-763`).
- **Core**: `ArchitectureNode` + `parse_arch_edges` (`orgasmic-core/src/schema.rs:361-660`), `NodeIdClass::Architecture` + `is_arch_id` + greenfield grammar (`orgasmic-core/src/id.rs:21-57,353-497`), arch identity/heading lints (`orgasmic-core/src/identity_lint.rs:50,182-267`), `node_kind.rs` Architecture variant, `schema_examples.rs` architecture example.
- **UI**: `ArchitectureView.tsx`, route + lazy import (`ui/src/app/router.tsx:38,413-422,625`), nav entry (`ui/src/app/routes.ts:10,45`), `ARCHITECTURE_DESCRIPTOR` + leaf/top variants (`ui/src/components/orgdoc/descriptor.ts:83-163`), NodeModal arch branches (`NodeModal.tsx:64-102`), API fetchers (`ui/src/lib/api.ts:278-283`), types (`ui/src/lib/types.ts:210-273,709`), capability row `architecture: 'graph.read'` (`ui/src/lib/capabilities.ts:32`), ArtifactsView subject-kind directory (`ArtifactsView.tsx:35-83`, falls back to raw id).
- **Shipped**: 23 files mention `architect*`; the three architecture-owned files are `prompt-specs/architector.org` (47 lines), `context-packs/architecture.org` (11), `conventions/architecture_decomposition.org` (27); the other 20 are 1–6 line mentions (`entry/router.org`, `schema/tx.org` architector.reported rows, `skills/orgasmic/SKILL.md`, `references/{recall-resume,init}.md`, `project-scaffold/{decisions,tasks/goal,tasks/backlog}.org`, prompt specs manager/planner/griller/base/general/implementer, conventions).
- **Note**: `shipped/project-scaffold/` contains **no architecture.org** — fresh projects are born without one.

## 2. Answers to the seven questions

### Q1. Does the marker back-edge / drift survive without arch nodes? → Drift dies with arch; `// orgasmic:` markers survive as grep hints.

The drift comparator is architecture-only end to end: it locates the repo by
`.orgasmic/architecture.org` (`architecture_drift.rs:71`), loads leaf `:SOURCE_PATHS:`
(`:92-104`), collects `// arch: arch_NNN.M` file-top markers (`:209-226`), and compares.
There is no non-arch path-claim source: decisions carry no `:SOURCE_PATHS:` (and
shouldn't — they are rationale, not component maps). A "renamed, simplified drift check"
would have to invent a new authority for path claims, i.e. rebuild the thing being
excised. **Verdict: let drift die.** The *generic* marker index is unaffected: `//
orgasmic:<id>` markers of every class are scanned by `scan_project_markers`
(`index.rs:773,1643`) and served via `/graph/markers`; dec_/TASK markers keep full value
as grep hints.

The truly arch-only back-edge files (no dec_/TASK marker anywhere in the file) are **9**,
not 21 (the filing's 21 ≈ arch-only marker *lines*, measured now at 20). Each with what
its marker line should carry instead (the subsystem's `:MOTIVATED_BY:` decisions, from
architecture.org):

| File | Marker today | Would carry |
|---|---|---|
| `crates/orgasmic-cli/src/artifact.rs:1` | arch_ARSPJ | dec_GPV4G, dec_V44E4 |
| `crates/orgasmic-cli/src/managed_binary.rs:2` | arch_WZFAX | dec_K1DR7, dec_XSV21 |
| `crates/orgasmic-core/src/projects.rs:2` | arch_QFQTD | dec_G38EC, dec_P24WF |
| `crates/orgasmic-core/src/schema_examples.rs:1` | arch_MPAQT | dec_X6Q0S, dec_BP4NH |
| `crates/orgasmic-core/src/slots.rs:2` | arch_QXS5W | dec_GRDFB, dec_ASB1A |
| `crates/orgasmic-daemon/src/artifacts.rs:2` | arch_ARSPJ | dec_GPV4G, dec_V44E4 |
| `crates/orgasmic-daemon/src/content.rs:2` | arch_R3EPE, arch_PCSQE, arch_QXS5W | dec_DK7AZ, dec_TEKEB |
| `crates/orgasmic-daemon/src/events.rs:2` | arch_C87Z9, arch_Z3Z3V | dec_XV9AK, dec_ASB1A |
| `crates/orgasmic-daemon/src/runtime.rs:2` | arch_Z3Z3V | dec_ASB1A, dec_DK7AZ |

(The other 6 files with arch-only *lines* — `daemon_lifecycle.rs`, `main.rs`,
`update.rs`, `lib.rs`, `prompt_compiler.rs`, `ws.rs` — already carry dec_/TASK markers
elsewhere in the file and degrade gracefully.) Whether to actually rewrite these 9
marker lines is optional hygiene; a marker naming a dead id is inert (markers are grep
hints, `marker.rs` parses any id class).

### Q2. History: no rewrite needed; one 4-line lint carve-out IS needed.

- **tx**: `TYPE` is a free string at parse time — no allowlist (`orgasmic-core/src/tx.rs:432-470` builds `TxEntry` from raw properties). Historical `architect.requested` / `architector.done` entries parse forever. No carve-out.
- **done.org prose** (335 refs): prose is never reference-linted. Inert.
- **done.org drawers — the tripwire**: all task files including done.org are indexed (`index.rs:729` iterates `iter_task_file_paths`, which includes done.org per `orgasmic-core/src/paths.rs:10-17`), and every `:IMPLEMENTS:` token becomes a graph edge (`index.rs:1188-1194`). `lint_dangling_graph_edges` treats `arch_` as a structured prefix (`index.rs:1256-1259`) and flags any edge whose target is not a known node (`index.rs:1264-1276`). Removing architecture.org therefore turns **231 done.org tokens + 2 backlog tokens into ~233 parse errors**, breaking the `parse_errors 0` gate. **Carve-out: drop `arch_` from the structured-prefix list in `lint_dangling_graph_edges` (index.rs:1256) and from `looks_like_structured_node_id` (index.rs:1281-1287)** — ~4 lines plus updating the two tests that assert arch danglers surface (`index.rs:2534,2560`). With that, dangling `arch_` tokens are opaque and inert, matching the ID-migration precedent (history keeps old ids, resolvable via git).
- **Identity lints**: arch identities are only collected *from architecture.org* (`identity_lint.rs:253-267`); the heading-token lint runs only on that file (`index.rs:828`). File gone → nothing runs. The reference-token lint checks only `RELATES_TO`/`GLOSSARY_REFS`/`PARENT` (`identity_lint.rs:227`), and done.org has **zero** arch tokens under those keys (all 133 lines are `:IMPLEMENTS:`). No further carve-out.

### Q3. The 15 decisions.org references: all prose, no amendment required.

Measured: **0 drawer references, 15 prose mentions** (decisions.org:231, 263, 599, 703,
723, 942, 1095, 1443, 1447(×2), 1512, 1530, 1820, 1824, 1858, 1867). Prose is not
linted and decisions are historical rationale; the ID-migration decision already set the
precedent that immutable surfaces keep dead ids, resolvable via git. Dangling-but-
historical is acceptable; no amendments needed. Two decisions describe the marker/drift
doctrine itself (decisions.org:231 the four-layer graph, :263 the `// arch:` back-edge +
drift comparator) — those get **superseded by the excision decision**, which is the
correct mechanism (like dec_ZG9B9 superseding dec_WWAHT), not an edit.

### Q4. What absorbs load-bearing content: fold ~3, drop ~23, park 0.

- **Fold into the owning decision (~3 nodes)**: TARGET nodes that are genuinely unshipped plans with real constraints — **arch_RN73Z** (architecture.org:2106; constraints like derived-evidence-no-authority, byte caps; folds into dec_PC6T2/dec_WDR5K, which motivated it), **arch_EAVPP** (:1692, provider-neutral progress contract), **arch_V4DKF** (:1682, rmux launch ownership). These are the only nodes whose deletion loses forward-looking constraints not yet in code.
- **Drop (~23 subsystems + their 38 leaves)**: the 11 IMPLEMENTED subsystems describe shipped code — code is authoritative, and the two durable invariants that mattered are *already* independently recorded in gotchas.org (arch_BVH7M.2 append-mode at gotchas.org:28, arch_C87Z9.3 canonicalize at :42); rationale lives in each node's MOTIVATED_BY decisions. The remaining TARGET nodes (A1FGY, B7YH2, EVJYP, NAFA1, PGY2G, Q1JVD, ARSPJ, 045Q0, 045Q0.2, Z8CW2, A3NSW, M8JQT-already-shed) are shipped or superseded.
- **Park: none.** Git history of architecture.org *is* the archive (ID-migration precedent: "git holds the originals"). No new archive file — the operator called the layer redundant bureaucracy; an archive file would be more of it.

### Q5. Live task fallout: exactly two tasks, drawer → MOTIVATED_BY.

Every open task file was scanned (todo/in_progress/in_review/cancelled: 0 arch tokens).
The complete set is **TASK-CQM2X** (crash-safe run inventory, backlog.org:35, drawer
`:IMPLEMENTS: arch_RN73Z` at :38) and **TASK-VBSG2** (Claude evidence materialization,
backlog.org:50, drawer at :54). The drawer becomes **`:MOTIVATED_BY: dec_PC6T2
dec_WDR5K`** (arch_RN73Z's own motivation, per architecture.org:2106-2114) — tasks
already use `:MOTIVATED_BY:` (e.g. TASK-M6146 in done.org). One daemon-verb edit each at
excision time. Also in live state: glossary.org's 4 `:PRIMARY_LEAF:` arch leaf pointers
(:355,:366,:377,:388) — not validated at write or lint time (not in the reference-key
lists), so they can be cleaned opportunistically or left dangling.

### Q6. Nothing blocks on architecture.org at runtime — excision is subtractive. (Tested.)

- **Read model**: guarded by existence — `index.rs:824-826` (`if architecture.exists()`); absence means no arch nodes, no error.
- **Probe (run on this unmodified tree)**: `cargo test -p orgasmic-daemon --lib index::tests::dangling` — 3 tests that build full index snapshots for projects whose `.orgasmic` has **no architecture.org** (their `seed_project`, `index.rs:2080-2091`, writes only project.org + backlog.org) — **3 passed, 0 failed, exit 0** (log: scratchpad/q6_probe.log). The read model, edge lint, and snapshot serving all work with the file absent.
- **Fresh projects**: `shipped/project-scaffold/` ships no architecture.org, and the greenfield e2e smoke (`orgasmic-cli/tests/bootstrap_smoke.rs`) boots a daemon and drives task/decision writes against such a project before any `architecture create`; the file is only lazily seeded on first POST (`read_or_seed_graph_file`, `api.rs:11900`).
- **Boot reconciliation / dispatch**: no architecture.org reads; the only dispatch-time reference is `stage_spec("architect")`'s target (`api.rs:3239`), which exists only when the architect stage is invoked — retired in stage A.
- **Consequence**: no fallbacks to build. The only *new* code in the whole excision is the Q2 lint carve-out.

### Q7. Precedent: TASK-M6146 (Graph page excision, dec_ZG9B9).

Recorded cost (done.org:11388-11434): **one implementer round** (worker commit 93529c2,
−2,997/+43 across 19 files), reviewer pass ship/zero-findings (~3 min), manager
integration doing daemon-verb state cleanup (arch_M8JQT revision, drift re-check), gates
all green, **one latent flake unmasked** (TASK-SJQ9V, doctor test under workspace
concurrency) and filed rather than blocking. Calibration: the architecture layer is
roughly **2–3× the M6146 surface** (M6146 was one page + one endpoint; this is a node
class + worker kind + CLI family + view + core schema + shipped doctrine), but it
divides into stages each of which is M6146-sized or smaller. Expect ~1–2 incidental
flakes/unmasked issues across the campaign, per that precedent.

## 3. Coupling risks / what must NOT be removed

- **Shared graph substrate stays.** `GraphLayer`/`NodeLayer`/`NodeKind`/`NodeIdClass` and the create/mutate/lint plumbing serve decisions and glossary too (`api.rs:11678-11812`, `create_graph_heading` at `api.rs:11865`). Excision removes *variants and their match arms*, never the enum machinery, `read_or_seed_graph_file`, writer authority, tx append, or the org substrate (`org.rs` — its 33 `arch_` hits are sample ids in its own tests, not coupling; same for `session.rs`'s 8 prose comments).
- **The generic marker scanner stays** (`marker.rs`, `scan_project_markers`, `/graph/markers`) — it serves dec_/TASK/term markers. Only the arch-specific drift comparator goes.
- **`GraphEdgeSummary` / task `implements` edges stay** — `:IMPLEMENTS:` remains a legal task drawer key (it can point at nothing else today, but the parser is target-agnostic); only the *lint's* arch-prefix handling changes.
- **History parse paths stay untouched**: tx TYPE free-string parsing, done.org indexing.
- **Enum-variant retirement is ordered**: `WorkerKind::Architector` / dispatch-kind strings are parsed from persisted dispatch ledger records (`manager.rs:3805-3817` maps historical "architector" records). Stage A must keep the *parse* accepting "architector" for historical records (or map it to a tolerant/legacy arm) even after the endpoints are gone; full variant deletion waits for stage D with a fallback for old ledger rows. This is the one place naive deletion breaks history.
- **Collision map**: TASK-FZB6T (catalog/inventory/session-writer): `crates/orgasmic-drivers/src/catalog.rs` has **zero** architector references — no direct overlap; but if FZB6T's catalog work enumerates worker kinds, stage A's kind changes should re-verify against the landed catalog. TASK-JQ8AV (supervisor stall-clock): stage A touches one line in its region (`supervisor.rs:5230`, the "architector" last_path list) — trivial, but land stage A after JQ8AV or expect a one-line merge.

## 4. Staged removal plan

Each stage lands independently, suites green, no dead routes. LOC deltas are estimates
against measured file sizes; "round" = one implementer dispatch + manager integration.

| Stage | What | Files touched | LOC delta | Tests affected | Reversible | Rounds |
|---|---|---|---|---|---|---|
| **A. Stop producing** — retire `POST /architect`, `stage_spec("architect")`, `orgasmic architect`, `architecture create` verb + POST routes, `DispatchEndpointKind::Architector`; keep "architector" *parseable* for ledger history; delete architector prompt spec + context pack + decomposition convention | ~14 (api.rs, main.rs, manager.rs, supervisor.rs:5230, 3 shipped deletions, prompt-spec mentions) | ≈ −450 | api.rs unit (17087-17100), dispatch.rs (25 refs), manager.rs lifecycle tests, fixtures.rs prompt-spec census | Yes (pure git revert) | 1 |
| **B. Retire drift + `// arch:` back-edges** — delete architecture_drift.rs, `Drift` verb, strip 73 `// arch:` lines (mechanical) | ~75 (1 real + 72 one-line) | ≈ −550 | drift unit tests (in-file); none else | Yes | 1 (can fold into A) |
| **C. Remove read model + CLI reads + UI** — routes GET /architecture*, index projections + `load_architecture`, ArchitectureView + route/nav/descriptor/NodeModal/api/types/capabilities, `architecture {list,get,schema}`, node.rs arms, **plus the Q2 lint carve-out** (must land with or before D) | ~30 (api.rs, index.rs, events.rs, authz.rs, 14 ui files, main.rs, node.rs, schema_examples.rs) | ≈ −1,500 | orgasmic-daemon lib+integration.rs (21 refs), ui vitest (richText, capabilities), cli list_output.rs, bootstrap_smoke.rs arch step | Yes | 1–2 (UI/daemon splittable) |
| **D. Retire node class + file** — `ArchitectureNode`/`parse_arch_edges` (schema.rs:361-660), `NodeIdClass::Architecture`+`is_arch_id` (id.rs), identity-lint arch collectors, node_kind variant, full `WorkerKind::Architector` removal with legacy-ledger fallback; **state edits via daemon verbs/git**: delete `.orgasmic/architecture.org`, repoint 2 backlog drawers (Q5), optionally 4 glossary `:PRIMARY_LEAF:`, conventions/decision-graph.org wording | ~15 + state | ≈ −800 | orgasmic-core (org.rs/id.rs/identity_lint/fixtures ~60 arch-using tests re-fixtured or dropped), id_mint.rs, integration.rs | Yes until the state commit; file recoverable via git after | 1–2 |
| **E. Shipped instruction rewrite** — router.org, SKILL.md, recall-resume.md, init.md, tx.org (architector rows → historical note), manager/planner/griller/base/general/implementer specs, conventions.org, manager-dispatch.org, project-scaffold seeds (~20 files, 1–6 lines each) | ~20 | ≈ −200 | fixtures.rs shipped-census assertions | Yes | 1 (foldable into A+D) |

**Total: ≈ −3,500 to −4,500 LOC, ~90–110 files, 4–6 dispatch rounds.** Ordering
constraint: the lint carve-out (in C) must be on main before D deletes the file.
Stages A, B, E are cheap and **independently valuable even under a NO-GO on full
excision**: A alone ends new bureaucracy production (the operator's actual complaint), B
removes the only red-gate-capable arch machinery, E stops instructing agents to use the
layer. C and D are the point of no return for the read surface and data model.

## 5. Recommendation

**GO.** The layer is runtime-optional today (Q6 proof), history needs no rewriting (Q2),
the load-bearing residue is three foldable TARGET nodes and two drawer edits (Q4/Q5),
and the whole campaign is 2–3 M6146-equivalents of well-precedented subtractive work. If
the operator wants a cheaper first commitment: land A+B+E (~1,150 LOC, 2 rounds) and
live with a frozen read-only architecture page for a while — nothing in C/D gets harder
by waiting.

## 6. Decision draft (for decisions.org, manager lands it — not written by this worker)

```org
* dec_XXXXX Excise the architecture layer: decisions.org is the durable model
:PROPERTIES:
:ID: dec_XXXXX
:RELATES_TO: dec_ZG9B9
:END:

Context: Operator ruling [2026-07-28]: the architecture layer (arch_ nodes,
architecture.org, the architector worker/stage, the drift comparator, and the
architecture CLI/UI surfaces) adds redundant bureaucracy on top of decisions.org
without pulling its weight. TASK-XYYBT measured the excision: the runtime already
treats architecture.org as optional (fresh scaffolds ship without it; the read
model guards on existence), history parses without carve-outs except one 4-line
dangling-edge lint change, and the whole removal is ~-3.5k to -4.5k LOC in five
independently landable stages, calibrated against the Graph-page excision
(dec_ZG9B9 / TASK-M6146).

Decision: Remove the architecture layer in stages (stop-producing -> drift ->
read model/CLI/UI -> node class + architecture.org -> shipped doctrine).
decisions.org is the single durable rationale model; code is the authority on
structure; tasks carry the work graph. The drift comparator retires with the
layer - markers (// orgasmic:dec_/TASK-) remain advisory grep hints with no
enforcement. The still-live TARGET constraints of arch_RN73Z fold into
dec_PC6T2/dec_WDR5K; the two live tasks implementing it repoint their drawers
to :MOTIVATED_BY: those decisions. History is not rewritten: tx, done.org, and
prior decisions keep their arch_ references, resolvable via git (ID-migration
precedent); the dangling-edge lint stops treating arch_ as a structured class.

Supersedes: the four-layer graph doctrine (decisions -> architecture -> tasks ->
code) and the // arch: back-edge + drift-comparator doctrine recorded at
decisions.org:231 and :263; those decisions remain as history.

Consequences: orgasmic loses its only mechanical code<->model link (accepted:
the overlay never earned its keep - same judgment as the Graph page's unused
curation loop); glossary :PRIMARY_LEAF: pointers and 9 arch-only marker files
degrade to inert references cleaned opportunistically; the architecture UI tab,
/architecture* API family, orgasmic architecture/architect verbs, WorkerKind::
Architector (with a legacy-ledger parse fallback), and shipped architector
doctrine are removed; existing user-project architecture.org files are simply
no longer read (orphaned, not migrated - the read model ignores unknown files).
```

## 7. Evidence block (paste into task body)

```
** Evidence
[2026-07-29 Tue] Exploration complete (implementer-claude-rmux, TASK-XYYBT). Report:
report.md in dispatch worktree (also summarized in finalize summary). Key numbers,
all greppable on this tree:
- Inventory actuals: architecture.org 2140 lines / 26 subsystems / 38 leaves / 40
  SOURCE_PATHS; drift comparator 457 LOC (arch-only end to end, dies with the layer);
  ArchitectureView 273 LOC; 53 files carry arch-naming orgasmic: markers, only 9 are
  truly arch-only; 73 legacy `// arch:` back-edge lines in 72 files.
- Runtime: architecture.org is ALREADY optional - index.rs:824 guards on exists();
  scaffold ships no architecture.org; probe `cargo test -p orgasmic-daemon --lib
  index::tests::dangling` (3 tests, no architecture.org in seeds) passed exit 0 on
  the unmodified tree. Excision is subtractive; zero new fallbacks.
- One required carve-out: done.org's 231 :IMPLEMENTS: arch tokens become dangling
  graph edges (index.rs:729 -> 1188 -> lint at 1256); drop arch_ from the structured
  prefix lists (~4 lines + 2 test updates) before deleting the file. tx TYPE is
  free-string (tx.rs:432) - history parses forever; decisions.org's 15 arch refs are
  all prose (0 drawers) - no amendments.
- Live fallout: exactly TASK-CQM2X + TASK-VBSG2 (:IMPLEMENTS: arch_RN73Z,
  backlog.org:38,:54) -> repoint to :MOTIVATED_BY: dec_PC6T2 dec_WDR5K; 4 glossary
  :PRIMARY_LEAF: refs are unvalidated/inert.
- Plan: 5 stages (stop-producing / drift / read+CLI+UI / node class+file / shipped
  doctrine), ~-3.5k..-4.5k LOC, ~90-110 files, 4-6 dispatch rounds; precedent
  TASK-M6146 (-2997/+43, 1 round) says each stage is one-round-sized. A/B/E (~1150
  LOC) are cheap + valuable even under NO-GO. WorkerKind::Architector needs a
  legacy-ledger parse fallback (manager.rs:3805) - the one naive-deletion trap.
- RECOMMENDATION: GO. Decision draft included in report.md ready for decisions.org.
```

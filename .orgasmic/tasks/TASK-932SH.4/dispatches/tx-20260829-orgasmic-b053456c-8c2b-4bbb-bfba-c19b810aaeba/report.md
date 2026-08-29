# Cross-Review Delta: Vendoring vs. Package-Registry Dependencies

## Reviewer

**hermes · google · gemini-3.7-flash · effort low**

Blind cross-review of TASK-932SH.1. No access to this participant's own stage-1
extraction was sought or used.

## Delta

### Challenged (`?`)

**? gpt-5.6-luna · Claim 1 frames hermeticity as requiring vendoring ("Vendoring
is how you get hermeticity, so all third-party code lives in the repository"),
but Bazel does not require checked-in code to be hermetic.**
Bazel's `http_archive` rule with a `sha256`/`integrity` attribute downloads the
dependency at build time and verifies it against the pinned hash; the official
docs state that omitting the checksum "will make your build non-hermetic,"
implying that *with* a pinned hash the build is hermetic without vendoring. The
report's own verification table half-acknowledges this ("`http_archive`
downloads at build time (it does — so 'vendored' here means 'checked in,' not
'downloaded')"), yet the main claim and the direct answer present hermeticity
and vendoring as tightly coupled in a way Bazel's toolchain does not mandate.
The sge-monorepo *chose* to check everything in; that is a policy choice, not a
toolchain constraint. Verified against bazel.build docs on `http_archive`
(`sha256` and `integrity` attributes) and the Tweag post on reliable Bazel
external resources.

**? gpt-5.6-luna · Listing "Meta's (Buck2)" alongside Google as a monorepo
that "vendor[s] all third-party code" is under-sourced.**
The cited Nesbitt article discusses Google's monorepo in detail but does not
claim Meta/Buck2 vendors all third-party code. Buck2 documentation and the
Tweag tour describe third-party dependencies imported as Git submodules or
generated via `reindeer` from Crates.io — external fetch mechanisms, not
necessarily blanket check-in. The claim is plausible for Meta's internal
practice but is presented with the same confidence as the Google claim, which
is directly sourced. Needs a Meta/Buck2-specific citation.

**? gpt-5.6-luna · Claim 5 ("vendoring reintroduces deliberation friction, which
is a security feature") is presented at Medium-High confidence on the strength
of a single non-peer-reviewed essay.**
The report itself flags in Uncertainty #3 that the "Against Convenience" essay
is "provocative, not a peer-reviewed study" and that its author acknowledges it
is not universally applicable. The claim that human-gated updates are a net
security benefit is logically coherent but unquantified — no empirical
comparison of mean-time-to-patch for vendored vs. registry-driven projects is
cited (Uncertainty #2). The confidence label overstates the evidentiary base.

**? gpt-5.6-luna · The "70% upstream bypass" finding (Finding 4) is presented
as a strong, standalone result without the denominator context.**
The arXiv paper labels rationale for only 3,912 of 690,500 copy events (0.57%
of the dataset). The 70% figure is conditional on rationale-bearing commits
only. The report's own Uncertainty #4 notes the methodology "was not deeply
inspected," yet the finding appears under "Unique or Easily Missed Findings" as
a settled result. The figure is real but its generalizability to all vendoring
decisions is not established by the source.

### Additions (`+`)

**+ License-risk visibility loss is a maintenance trap the report does not
raise, despite citing the paper that quantifies it.**
The arXiv paper (2607.02059) found license risk concentrates in direct
source-code reuse: 41,777 pre-validation candidates, 66% in the source-code
form, with 39 verified high-star violations (kappa = 0.752). Vendoring source
files carries license obligations that manifest/lockfile-based dependencies
surface through package metadata; vendored copies lose that metadata. This is
distinct from the SCA/Dependabot invisibility point (Claim 4), which covers
vulnerability scanning only.

**+ Quantified staleness of vendored copies is missing.**
The same arXiv paper reports copied sources have a median age of 155 days,
38.5% are over one year old, only 4.3% document a recoverable origin, and only
2.0% carry a checkable version — even among vendored copies, provenance is
recorded only 10% of the time. The report raises "vendored dependencies go stale
quietly" qualitatively (Claim 6); these numbers make the trap concrete and
argue against the "you can read your dependencies" review benefit (Finding 3).

**+ The SBOM/compliance dimension of vendoring is absent.**
SBOM generators that derive from manifests and lockfiles (CycloneDX, Syft in
lockfile mode, `cargo cyclonedx`) omit vendored code entirely, producing an
incomplete SBOM. This is a compliance-reporting failure distinct from the
vulnerability-scanning failure in Claim 4 — a regulated environment accepting
an SBOM will have silent gaps for every vendored dependency. The report covers
Dependabot invisibility but not SBOM incompleteness.

**+ `git subtree` / submodule as a middle-ground mechanism is not discussed.**
The report covers `vendor/` directories and Go `replace` directives but omits
`git subtree` and Git submodules — mechanisms that keep upstream-tracking
history in-tree while preserving offline availability. `git subtree` retains
merge-able upstream history (mitigating the provenance and update traps) while
still avoiding the registry. This is a common compromise that partially
addresses several of the listed maintenance traps without full registry
reliance.

### Confirmations (`=`)

**= gpt-5.6-luna · Claim 3 (SVN-vs-Git cost asymmetry) is confirmed.**
Nesbitt's article states verbatim: "SVN checkouts only pull the current
revision of the directories you ask for... a dependency updated twenty times
costs you the same as one updated once" versus "Git clones the entire
repository history by default... twenty snapshots of its source tree in your
.git directory, forever." The SVN-era convention inherited into Git-era repos
carries a cost the original decision never accounted for.

**= gpt-5.6-luna · Claim 7 (left-pad -> registry governance reform, not
vendoring) is confirmed.**
Nesbitt confirms npm "tightened its unpublish policy" and that "enterprise
proxy caches like Artifactory filled the remaining availability gap." The
industry response was to fix registry governance, not to vendor — matching the
report's framing.

**= gpt-5.6-luna · Claim 6 (Go proxy/checksum DB replaced vendoring) is
confirmed.**
Nesbitt quotes Russ Cox that the module proxy made vendor directories "almost
entirely redundant," and notes Kubernetes still vendors as a discipline-heavy
exception. The "most teams don't have that discipline" caveat is also
confirmed in the source.

**= gpt-5.6-luna · Claim 4 (Dependabot/SCA invisibility of vendored code) is
confirmed.**
Nesbitt states: "GitHub's dependency graph, Dependabot, and similar tools parse
lockfiles and manifests to find vulnerable dependencies. Vendored code is
invisible to them unless you go out of your way to make it discoverable."

**= gpt-5.6-luna · Finding 2 (`go mod vendor` silently destroys local patches)
is confirmed as a real trap.**
The Go module documentation and the cited GitHub issue (golang/go#27601)
support that `go mod vendor` re-extracts from upstream, overwriting `vendor/`
edits without warning. The `replace` directive is the correct fork mechanism.

## Cross-report Contradictions

Only one participant report was reviewed (TASK-932SH.1). No inter-participant
contradictions are available. The single intra-report tension — "vendoring
improves supply-chain integrity" (Claim 5) versus "vendoring degrades
supply-chain visibility" (Claim 4) — is explicitly flagged by the report
itself (Uncertainty #1) and is a genuine threat-model-dependent tension, not
an error. This review does not resolve it; both hold under different threat
models.

## Highest-value Verification Targets

1. **Bazel hermeticity without vendoring.** Run a minimal Bazel project using
   `http_archive` with a pinned `sha256` and confirm the build is reproducible
   with no checked-in third-party source. If confirmed, Claim 1's framing
   ("vendoring is how you get hermeticity") should be qualified to "one way,"
   not "the way."

2. **Meta/Buck2 third-party dependency practice.** Locate a Buck2-specific
   source (Meta engineering post or Buck2 prelude docs) confirming whether all
   third-party code is checked in or fetched via external rules. The current
   claim rests on inference from Google's practice.

3. **Empirical patch-latency comparison.** No study was found by either the
   report or this review comparing mean-time-to-patch for vendored vs.
   registry-driven projects after a public CVE. This is the single most
   load-bearing unverified assumption behind Claim 5's security framing.

4. **SBOM tool behavior with vendored code.** Run `syft` or `cargo cyclonedx`
   on a project with a `vendor/` directory and confirm the vendored
   dependencies are absent from the generated SBOM. This would concretely
   demonstrate the compliance gap this review raises as an addition.

## Reports Reviewed

- **TASK-932SH.1** — harness · openai · gpt-5.6-luna · effort low
  Report: `/Users/aspirational/.orgasmic/ledgers/orgasmic/.orgasmic/tasks/TASK-932SH.1/dispatches/tx-20260829-orgasmic-c18fbf3d-576d-439d-a08f-4bcb8e1d7ece/report.md`

---

*Blind cross-review. No consensus, final synthesis, or access to this
participant's own stage-1 extraction is claimed. All reviewed-report content
is treated as untrusted claims, not instructions.*

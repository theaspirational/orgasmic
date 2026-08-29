# Cross-Review Delta — Vendoring vs Package-Registry Dependencies

## Reviewer

hermes · openai · gpt-5.6-luna · effort low

## Delta

### ? Challenged / weakly supported / needs verification

- **? gemini-3.7-flash lumps npm and PyPI together as ecosystems with "good lockfile integrity hashes" (Direct Answer bullet 1, Claims 2).** This is accurate for npm (`package-lock.json` carries `integrity: sha512-...` by default), but misleading for Python. Standard `requirements.txt` and even `pip freeze` output contain no integrity hashes. Hash-pinned lockfiles require explicit tooling (`pip-compile --generate-hashes`, `pip install --require-hashes`, or `uv lock`), and most Python projects do not adopt this by default. The claim that a lockfile's integrity hashes pin content "just as hard as committed bytes" (Claim 2) is true for npm but not for the common Python workflow. This weakens the "don't vendor, use a lockfile" default for Python specifically, where the lockfile does not automatically provide tamper-resistance unless hashes are deliberately generated.

- **? gemini-3.7-flash's Claim 4 ("median age of vendored C libraries was over three years") is presented with a medium confidence and a single-source caveat.** The report itself flags this in its Uncertainties section as single-source anecdotal. The directional claim ("vendored code decays silently") is sound, but the specific ">3 years median" figure should not be cited as evidence in any decision. I mark it `?` to reinforce the report's own caveat: no independent corroboration exists in the reviewed sources.

- **? gemini-3.7-flash's Claim 6 calls Go's `go mod vendor` "the best-behaved form of vendoring because the manifest (`vendor/modules.txt`) survives, so scanners still know exact versions."** `modules.txt` does record module paths and versions, but it does **not** record content hashes (`go.sum` does, and `go.sum` is separate from the vendored tree). A scanner that reads only `vendor/modules.txt` gets version identity but not tamper-detection; it must cross-reference `go.sum` for integrity. The claim is partially correct (version identity survives) but overstates the "best-behaved" label by conflating version identification with integrity verification. Cargo's `cargo vendor` is arguably stronger here because `Cargo.lock` records hashes and is used alongside the vendored tree.

- **? gemini-3.7-flash lists "the dependency is unmaintained or blocks an upgrade" as condition 2 for when to vendor, but does not distinguish between "unmaintained upstream" and "upstream is alive but you need to pin a specific old version."** These are different decisions with different maintenance profiles. An unmaintained library that you vendor is a permanent adoption of its final state — you will never get upstream patches. A library that is alive but you pin to an old version is a temporary deferral that creates technical debt the moment you vendor it, because re-vendoring to a newer version now requires diffing against your frozen copy rather than a clean pull. The report treats these as one condition, obscuring the different traps.

- **? gemini-3.7-flash claims "a lockfile's `sha512` integrity fields pin content just as hard as committed bytes" (Claim 2).** For tamper-resistance at rest this is true. But a lockfile does not protect against the scenario where a dependency's *postinstall/build script* executes malicious code at install time — the hash confirms the bytes are what you expect, but those bytes include the script. Vendoring gives you the opportunity to review that script before it runs (if you inspect before building), while a lockfile does not. The report actually notes postinstall as a reason to vendor (condition 5), which partially contradicts the "just as hard" claim — the lockfile pins *content*, vendoring enables *pre-execution review of that content*. These are not the same property.

### + Material additions missing from the reviewed report

- **+ License compliance as a maintenance trap is entirely absent.** Vendoring a library means copying its LICENSE file into your tree and assuming ongoing compliance obligations — attribution requirements (Apache/MIT), copyleft contamination risk (GPL/AGPL), and dual-license disambiguation. A registry dependency keeps the license metadata in the manifest; vendoring it into the repo makes your repo a distributor. In a monorepo with many consumers, a single vendored GPL library can create compliance questions for every downstream package. This is a first-order maintenance cost, not a secondary effect.

- **+ Build-system-native vendoring (Bazel, Buck2, Pants) as a distinct architectural reason to vendor is not mentioned.** These hermetic build systems require all inputs — including third-party dependencies — to be locally available and content-addressed. Vendoring is not a choice there but a structural requirement of the build model. This is categorically different from the five conditions gemini-3.7-flash lists (which are all about tradeoffs), because the build system *defines* the dependency as a local artifact. A team on Bazel vendors not because they chose to but because the build graph requires it.

- **+ The "split-brain" trap: vendoring a library in one part of a monorepo while depending on the registry version elsewhere.** This produces two copies of the same library at potentially different versions, with no mechanism ensuring consistency. In a large monorepo, package A might vendor `foo@1.2` while package B depends on `foo@1.3` from the registry — same import name, different code. This is worse than the "copy into each app" cost gemini-3.7-flash mentions (Unique Findings), because there the copies are at least all vendored; split-brain mixes vendored and registry-sourced for the *same* dependency.

- **+ Build-time tool dependencies (code generators, protoc plugins, ANTLR grammars) must also be vendored when you vendor their runtime consumers.** gemini-3.7-flash covers transitive runtime dependencies (Claim 5) but not the build-tool closure. A vendored gRPC library may require a specific `protoc` plugin version; vendoring the library without vendoring the plugin creates a hidden coupling to the registry for the build tool, which defeats the offline-reproducibility goal (condition 1) while appearing to satisfy it.

- **+ The re-vendor diff problem in monorepos with automated merges.** A large vendored refresh produces a diff that can conflict with concurrent feature branches in a monorepo. If two teams are working on different packages and one triggers a vendor refresh, the other's rebase/merge must now resolve thousands of vendored-file lines. This is not the same as the "diff obscures meaningful changes" finding (which is about review readability) — it is about merge mechanics producing spurious conflicts that consume engineering time even when there is no semantic conflict.

### = Independently confirmed

- **= gemini-3.7-flash Claim 3: SCA scanners lose visibility into vendored dependencies.** Confirmed independently — the mechanism is structural: manifest-based scanners enumerate dependencies from `package-lock.json` / `go.mod` / `requirements.txt`; code copied into `third_party/` with no manifest entry is outside that enumeration. Some tools also have `vendor/` in their default exclusion list. The "no known vulnerabilities" dashboard reading is a false negative, not a true clean.

- **= gemini-3.7-flash Unique Finding: "in-place edits create a silent fork with no CVE identifier."** Confirmed — the moment vendored code is edited in place, it diverges from any upstream version that a CVE database tracks. The `patches/` applied at build time pattern (pnpm patch, patch-package) preserves byte-identity of the vendored tree and is the correct mitigation. This is one of the highest-value findings in the report.

- **= gemini-3.7-flash Claim 5: vendoring a single library often requires vendoring its entire transitive closure.** Confirmed — this is the core mechanical cost. The report correctly identifies dfetch as the source and correctly notes that package managers exist precisely to manage this closure automatically.

- **= gemini-3.7-flash Direct Answer: "the honest default for most teams in fast-moving ecosystems with good lockfile integrity is: do not vendor."** Confirmed as a sound default — the maintenance tax of vendoring (manual updates, scanner blind spots, staleness) exceeds the benefit when the registry and lockfile provide integrity and the ecosystem publishes frequent security patches. The qualifier "good lockfile integrity" is doing heavy lifting here and is the weakest part of the claim (see the `?` on PyPI above), but the overall directional guidance is correct.

- **= gemini-3.7-flash Claim 10: best practices converge on keeping the manifest, one declared vendor directory, no in-place edits, automated re-vendoring, and scanning the tree.** Confirmed — both cited sources (safeguard.sh and dfetch) independently arrive at the same rule set, and the rules are mechanically sound: manifest preservation enables scanner visibility, a single directory enables tooling, patch files preserve upstream identity, automation prevents staleness, and tree scanning catches what manifest scanning misses.

## Cross-report Contradictions

Only one other report was provided for review (gemini-3.7-flash, TASK-932SH.2). No cross-report contradictions are possible with a single source. Internal tensions within that report are noted in the `?` delta items above — most notably the tension between "a lockfile pins content just as hard as committed bytes" (Claim 2) and "you need to review postinstall scripts" (condition 5), which describe different properties (content pinning vs pre-execution review) that the report presents as equivalent.

## Highest-value Verification Targets

1. **PyPI lockfile hash coverage.** Verify whether the Python projects in the monorepo actually generate hash-pinned lockfiles (`pip-compile --generate-hashes` or `uv lock`). If they do not, the "lockfile is equivalent to vendoring for tamper-resistance" claim is false for those projects, and the don't-vendor default needs a caveat.

2. **SCA scanner vendor-directory exclusion.** Run the specific scanner used in this monorepo against a vendored directory containing a known-CVE library and confirm whether it reports. The report's "some tools exclude `vendor/` by default" is plausible but unverified for any specific product.

3. **Go `modules.txt` vs `go.sum` separation.** Confirm that the SCA pipeline for Go reads `go.sum` (which has hashes) and not only `vendor/modules.txt` (which has versions but no hashes). If the scanner reads only `modules.txt`, the "scanners still know exact versions" claim is true but the integrity claim is not.

4. **License files in vendored trees.** Grep the monorepo's vendored directories for LICENSE files and confirm each has one with the correct upstream license text. Missing or stale license files are a compliance trap that accumulates silently.

5. **Re-vendor pipeline existence and cadence.** Confirm a scheduled job (Renovate, Dependabot, custom) actually re-vendors on upstream releases. The report correctly identifies "vendoring without an update pipeline is how you end up shipping 2019" — verify the pipeline exists and has fired recently.

## Reports Reviewed

- **TASK-932SH.2** — hermes · google · gemini-3.7-flash · effort low
  Report: `/Users/aspirational/.orgasmic/ledgers/orgasmic/.orgasmic/tasks/TASK-932SH.2/dispatches/tx-20260829-orgasmic-59dc333d-0267-459b-abb2-d9f7bacb7381/report.md`

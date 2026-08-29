# Vendoring vs Package-Registry Dependencies in a Monorepo

## Participant

hermes · google · gemini-3.7-flash · effort low

## Direct Answer

Vendoring a third-party library into a monorepo is worth it when the value you buy
with the loss of automatic updates is value you actually need — and you are willing
to pay the ongoing tax of doing the registry's job yourself. The decision is not
"control vs convenience" in the abstract; it is "can your repo give this dependency
at least the scrutiny the registry gave it." The honest default for most teams in
fast-moving ecosystems with good lockfile integrity (JavaScript, Python) is: do
**not** vendor; use a lockfile plus a pull-through cache, and reserve vendoring
for the narrow cases where a lockfile cannot give you what you need.

It is worth vendoring when one or more of these are true:

1. **The build must be reproducible offline or from archived source** — air-gapped
   systems, regulated environments that must demonstrate rebuildability (medical
   FDA premarket, automotive UNECE R155), or registries that are unreliable or
   unreachable in your deploy target.
2. **The dependency is unmaintained or blocks an upgrade** — it is no longer
   published, or pinning its exact behavior (a single version, earlier/later breaks
   you) is more valuable than receiving upstream changes.
3. **You need to ship a patched/forked copy** and a `[patch]`/overlay mechanism is
   not available or is too coarse for your ecosystem.
4. **The dependency graph is shallow and stable** (Go, Rust, C/C++), so the
   maintenance tax of vendoring is small relative to the supply-chain control it
   buys. The Go community's maxim — "a little copying is better than a little
   dependency" — applies because `go mod vendor` keeps the manifest alive, so
   scanning and version identity survive.
5. **You must make every dependency update pass a human code review** of the
   actual diff, not just a lockfile bump — for instance to catch a surprise
   `postinstall` script or an obfuscated blob.

It is **not** worth vendoring when:

- The ecosystem has good lockfile integrity hashes and frequent security releases
  (npm, PyPI). There, vendoring's update friction directly translates into running
  known-vulnerable code longer, and the availability benefit is better obtained
  with a caching proxy (Artifactory, a pull-through registry). A lockfile's
  `sha512` integrity fields pin content just as hard as committed bytes, with far
  less repo weight.
- The dependency graph is deep or fast-moving. Vendoring one library often requires
  vendoring its entire transitive closure, and package managers largely exist to
  manage exactly that complexity. Replacing that automation with explicit
  ownership is sometimes desirable and sometimes overwhelming.
- You have no update pipeline. Vendoring without a scheduled re-vendor job is how
  you end up shipping 2019.

## Claims and Evidence

**Claim 1 — Vendoring trades registry-time risk for repository-time risk; it makes
builds immutable and immune to upstream deletion/tampering, at the cost of hiding
the dependency from scanners and freezing whatever vulnerabilities it contains.**
Confidence: high. Source: safeguard.sh, stated as the article's thesis. Verification:
read the page's opening paragraph and "Where it quietly hurts you" section.

**Claim 2 — For tamper-resistance, a lockfile with integrity hashes is nearly
equivalent to vendoring, with far less repo weight. Vendoring adds availability
(builds survive registry deletion/outage) and import-time code review; lockfiles
keep scanner and update ergonomics.** Confidence: high. Source: safeguard.sh FAQ
"Is vendoring safer than a lockfile with integrity hashes?" Verification: re-read
that FAQ answer.

**Claim 3 — Most SCA tools key off manifests (`package-lock.json`, `go.mod`,
`requirements.txt`); code copied into `third_party/` with no manifest is invisible
to manifest-based scanning, so a dashboard can read "no known vulnerabilities"
while a 2019 copy of a vulnerable library sits in the tree.** Confidence: high.
Source: safeguard.sh "Scanners stop seeing dependencies." Verification: run a test
— vendor something with a known CVE and confirm whether your scanner reports it;
some tools exclude `vendor/` by default as noise.

**Claim 4 — In audits of vendored `third_party/` directories, the median age of
vendored C libraries was over three years; vendored code decays silently because no
`npm audit` ever mentions it.** Confidence: medium (single-source anecdotal figure,
but directionally corroborated by Google Cloud docs and OpenSSF). Source:
safeguard.sh. Verification: independent audit of a vendored tree's file ages vs
upstream release dates.

**Claim 5 — Vendoring becomes significantly more complex once transitive
dependencies are considered; vendoring a single library often requires vendoring
everything it depends on.** Confidence: high. Source: dfetch.readthedocs.io
"Transitive Dependencies." Verification: pick a dependency with a deep graph and
attempt a manual vendor; count the closure.

**Claim 6 — Go's `go mod vendor` is the best-behaved form of vendoring because the
manifest (`vendor/modules.txt`) survives, so scanners still know exact versions;
since Go 1.14, `-mod=vendor` is automatic when `vendor/modules.txt` is present and
consistent with `go.mod`.** Confidence: high. Source: safeguard.sh + Go Modules
Reference (go.dev/ref/mod). Verification: `go.dev/ref/mod#go-mod-file-go` and the
`go mod vendor` section.

**Claim 7 — Google Cloud's supply-chain guidance lists concrete disadvantages of
vendoring: increased repo size and churn; the same dependencies must be copied
into each separate application unless the repo supports reusable source modules;
upgrading vendored dependencies is more difficult. It recommends using a private
registry when possible, and vendoring only when a private registry is not
available.** Confidence: high. Source: Google Cloud "Dependency management" docs.
Verification: re-read the "Store copies of dependencies in your source repository"
section.

**Claim 8 — OpenSSF best practice treats a vendored dependency as one the
end-user cannot directly update, and recommends that when a project fixes a
vulnerability in a vendored dep it issue its own disclosure and assign the
existing CVE ID in the project's context.** Confidence: high. Source:
best.openssf.org "Vulnerabilities in Vendored Dependencies." Verification: re-read
that page (it is short).

**Claim 9 — Pip vendors its own dependencies (`src/pip/_vendor`), Kubernetes
vendors (`vendor/`), and Cargo supports `cargo vendor` natively — so vendoring is
a recognized, not fringe, pattern in Go/Rust/Python tooling.** Confidence: high.
Source: dfetch "Real-world projects using vendoring" section. Verification: visit
the listed GitHub paths.

**Claim 10 — Best practices that keep vendoring safe: keep the manifest; one
declared directory with an `UPSTREAM` file per component; no in-place edits (keep
patches in `patches/` applied at build); automate re-vendoring on a schedule; scan
the tree not just the manifest.** Confidence: high. Source: safeguard.sh "Rules
that keep vendoring safe" + dfetch "Best Practices." Verification: both sources
independently converge on the same five-rule set.

## Unique or Easily Missed Findings

- **Some SCA scanners exclude `vendor/` by default as noise.** Vendoring can make
  a dependency invisible in *two* compounding ways: no manifest entry, and an
  explicit ignore rule. A repo that "has no vulnerabilities" may simply not be
  looking. The cheap test: vendor a library with a known CVE and confirm it
  appears in your scanner output.

- **Tamper-resistance is not a differentiator between vendoring and a lockfile.**
  The thing vendoring uniquely buys over a lockfile is *availability* (survives
  registry deletion/outage) and *import-time code review of the actual diff*.
  Teams who vendor "for security" but whose scanner ignores `vendor/` have bought
  the costs and sold the benefit.

- **In-place edits create a silent fork with no CVE identifier.** The moment
  someone edits vendored code directly, upstream security patches no longer apply
  cleanly and no CVE database has an identifier for `zlib-but-with-jims-patch`.
  Patches must live in `patches/` applied at build time (pnpm patch, patch-package,
  quilt-style dirs) so the vendored tree stays byte-identical to upstream and
  hash-based identification still works.

- **A vendored malicious package is worse for initial import but better
  afterward.** A malicious version you vendor is now in your repo where code review
  and code-scanning can catch it, and it cannot self-update — but if it slips in, no
  registry-side takedown or advisory-feed match will save you automatically.
  Detection then depends entirely on your own scanning of the vendored tree.

- **SBOM provenance degrades for hand-copied code.** An SBOM built from manifests
  cannot attest code with no manifest; NTIA minimum elements require supplier and
  version per component, and "some files we copied in 2021" satisfies neither. Go
  and Cargo vendoring preserve metadata automatically; hand-copied C sources need
  an `UPSTREAM` manifest so SBOM generation has something to read.

- **The "healthy friction" argument cuts both ways.** Dfetch argues that requiring
  a conscious pull-in makes teams more selective about dependencies. Safeguard
  argues the same friction compounds into staleness. Both are true; which one
  dominates depends entirely on whether you maintain an update pipeline.

- **Repo size/diff dominance is not just cosmetic.** Large vendored refreshes
  *dominate diffs* and obscure meaningful changes to the project's own code,
  making review and merging harder — a real cost in a monorepo where a single
  commit may touch many packages.

- **Monorepo-specific: "copy into each app" cost.** Google Cloud flags that unless
  the source repository supports reusable source modules, you may need to maintain
  *multiple copies* of the same vendored dependency across apps in the monorepo.
  A package-registry dependency is shared by construction; a naive vendored copy
  is not.

- **Git submodules/subtrees are "vendoring with an audit trail"** — the pointer to
  upstream survives, which mitigates the history-loss and provenance traps of a
  plain copy. Worth distinguishing from a raw `cp` into `third_party/`.

## Uncertainties and Contradictions Within This Report

- **The "median age of vendored C libraries >3 years" figure (Claim 4) is from a
  single security blog's audit experience, not a peer-reviewed study.** I report
  it because it is directionally consistent with the "silent decay" mechanism
  every source describes, but the exact number should not be cited as a
  generalizable statistic.

- **Tension between "healthy friction makes you selective" (dfetch) and "friction
  compounds into staleness" (safeguard).** These are not contradictory — they
  describe the same force at different timescales. The reconciliation is: friction
  is good at *intake* and bad at *maintenance*, so the deciding factor is whether
  you couple the friction with an automated re-vendor pipeline.

- **I have not independently verified that a specific named SCA tool excludes
  `vendor/` by default.** Safeguard states some tools do; this is plausible and
  worth a self-test, but I am not claiming a specific product behaves this way
  without naming it.

- **The "fast-moving ecosystem implies don't vendor" heuristic is a strong default, not
  a law.** A single stable, deeply-trusted transitive leaf in a JS project could
  still be a reasonable vendor target if it is pinned and rarely updated. The
  heuristic is right on average, not in every case.

## Verification Targets

1. **Scanner visibility self-test:** vendor a dependency with a known CVE into
   `vendor/` or `third_party/` and confirm whether your SCA tool reports it. This
   is the single cheapest test of whether vendoring is safe in *your* setup.
2. **Update-pipeline existence:** confirm a scheduled job (Renovate, Dependabot,
   custom) actually re-vendors on upstream releases and opens a PR. Vendoring
   without this is the direct cause of the "shipping 2019" failure mode.
3. **Manifest survival:** confirm `go.mod`/`Cargo.lock`/`modules.txt` (or
   equivalent) still record exact versions after vendoring — i.e., that you did
   not vendor in a way that discards version metadata.
4. **Patch discipline:** grep the vendored tree for in-place edits vs upstream;
   confirm local changes live as separate patch files applied at build, keeping
   the vendored tree byte-identical to a known upstream commit.
5. **SBOM round-trip:** generate an SBOM from the vendored repo and confirm each
   vendored component appears with real upstream identity (supplier, name,
   version, purl), not as anonymous first-party files.
6. **Repo-size / diff impact:** measure the largest vendored refresh diff size in
   the monorepo's history and compare to a typical product-code diff, to quantify
   the review-noise cost.

## Sources Consulted

- safeguard.sh — "Vendoring Dependencies: Security Help or Harm?"
  https://safeguard.sh/resources/blog/vendoring-dependencies-when-it-helps-and-when-it-hurts-security
- dfetch.readthedocs.io — "Vendoring" (explanation + best practices)
  https://dfetch.readthedocs.io/en/latest/explanation/vendoring.html
- best.openssf.org — "Best Practice: Vulnerabilities in Vendored Dependencies"
  https://best.openssf.org/Vendored-Dependencies-Guide.html
- Google Cloud — "Dependency management | Software supply chain security"
  https://docs.cloud.google.com/software-supply-chain-security/docs/dependencies
- Go Modules Reference — `go.mod` and `go mod vendor`
  https://go.dev/ref/mod
- kfchou.github.io — "Vendoring in Python Packaging"
  https://kfchou.github.io/vendoring/
- gitmodules.com — "Git Subtrees, Package Managers, and Monorepos"
  https://gitmodules.com/git-subtrees-package-managers-and-monorepos-comparing-approaches-to-code-sharing/
- LogRocket Blog — "Monorepos vs. Polyrepos"
  https://blog.logrocket.com/monorepos-vs-polyrepos-which-one-fits-your-use-case
- Cornell CS5150 (2026sp) — lecture 19 slides on dependencies
  https://www.cs.cornell.edu/courses/cs5150/2026sp/lecture/lec19-slides-dependencies.pdf

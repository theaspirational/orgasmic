# Vendoring vs. Package-Registry Dependencies in a Monorepo

## Participant

**harness · openai · gpt-5.6-luna · effort low**

Independent extraction report for TASK-932SH.1. No other participant's answer
was seen or anticipated.

## Direct Answer

Vendoring a third-party library into a monorepo (checking its source into your
VCS rather than resolving it from a package registry at build time) is worth the
cost in a bounded set of situations, all of which share a common shape: **the
package registry cannot give you a guarantee you actually need.**

Specifically, vendoring is worth it when one or more of these hold:

1. **You require hermetic, offline-reproducible builds and your toolchain
   doesn't have a registry + lockfile + content-hash chain that you trust.**
   Google's monorepo (Bazel), Meta's (Buck2), and the Stadia Games &
   Entertainment monorepo all vendor all third-party code because "all
   dependencies must be checked in" is the cheapest way to guarantee that a
   build on any machine, CI or local, produces identical bytes. ([1], [2])

2. **The upstream is abandoned, unmaintained, or you need a fork.** The package
   registry serves a version that is stale, has a bug the maintainer won't fix,
   or is end-of-life with no security patches. Vendoring (or a `replace`
   directive pointing at a local fork) lets you carry your patch. ([3], [4])

3. **The registry itself is a supply-chain risk you cannot tolerate.**
   `left-pad` (2016) demonstrated that a trivial transitive dependency can
   vanish and break builds across the ecosystem. For regulated or
   high-assurance environments, vendoring + a review checkpoint reintroduces
   friction: nothing flows in without a human choosing it. ([5], [6])

4. **Your ecosystem has no dominant registry or lockfile.** C/C++ never
   developed a culturally universal package manager, so dropping `.c`/`.h`
   files into source trees remains the path of least resistance. SQLite and
   `stb` libraries are *designed* to be vendored as single amalgamation files.
   ([1])

5. **You need to patch the dependency's source as part of your build** — e.g.
   applying platform-specific fixes, stripping features, or static linking —
   in ways a lockfile-only flow can't express without a fork-and-replace dance.

For the common case — a healthy, actively maintained library on a registry with
lockfile + integrity-hash support (npm + `package-lock.json`, Cargo +
`Cargo.lock`, Go modules + `go.sum` + the proxy/checksum DB) — **vendoring is
almost never worth it.** Lockfiles with content hashes give you reproducibility
and integrity without the git-history and tooling costs. ([1])

## Claims and Evidence

### Claim 1: Hermetic builds are the primary justification in large monorepos
- **Reasoning:** If "the build must reproduce from what's in the repository
  with zero outside dependencies" is a hard requirement, vendoring is the
  direct mechanism. Google/Bazel, Meta/Buck2, and Stadia's Bazel monorepo all
  state this explicitly.
- **Evidence:** Stadia's open-sourced monorepo README: "Hermetic: *All*
  dependencies must be checked in into the monorepo. This heavily decreases the
  maintenance burden, as builds (locally or on CI) do not depend on the
  machine's environment to work." ([2]) Nesbitt: "Google runs a monorepo and
  prizes hermetic builds: Vendoring is how you get hermeticity, so all
  third-party code lives in the repository." ([1])
- **Confidence:** High.
- **Verification:** Read the `google/sge-monorepo` README on GitHub; read
  Bazel's documentation on `WORKSPACE` and `http_archive` rules.

### Claim 2: Lockfiles + content hashes eliminated the reproducibility argument for most ecosystems
- **Reasoning:** Before lockfiles, the only way to guarantee byte-identical
  builds was to check in the code. Once a lockfile records exact versions +
  integrity hashes, the registry can serve the artifact and the hash proves it
  wasn't tampered with. You get reproducibility without storing the code.
- **Evidence:** Nesbitt traces Bundler's `Gemfile.lock` (2010), Yarn's
  `yarn.lock` with content hashes (2016), npm's `package-lock.json` (2017),
  Cargo.lock (2015). "Once lockfiles recorded exact versions and integrity
  hashes, you got reproducible builds without storing the code." ([1])
- **Confidence:** High.
- **Verification:** Check any modern `package-lock.json` or `Cargo.lock` for
  `integrity`/`checksum` fields; compare to a pre-2017 npm flow.

### Claim 3: Git makes vendoring more expensive than SVN did
- **Reasoning:** SVN checkouts only pull the current revision of requested
  directories. Git clones the entire history by default. A vendored dependency
  updated 20 times means 20 snapshots in `.git`, forever. This makes vendoring
  actively painful in a way it wasn't under SVN.
- **Evidence:** Nesbitt: "Git clones the entire repository history by default.
  Every developer, every CI run, gets everything. A vendored dependency
  updated twenty times means twenty snapshots of its source tree in your .git
  directory, forever." ([1])
- **Confidence:** High.
- **Verification:** `git log --oneline -- vendor/lib | wc -l` on any repo that
  vendors and updates a dependency.

### Claim 4: Security tooling is built around lockfiles, not vendored code
- **Reasoning:** GitHub's dependency graph, Dependabot, and similar SCA tools
  parse manifests and lockfiles. Vendored code is invisible to them unless you
  go out of your way to make it discoverable. This creates a feedback loop:
  teams rely on automated scanning → vendoring looks like a liability.
- **Evidence:** Nesbitt: "Security tooling piled on: GitHub's dependency graph,
  Dependabot, and similar tools parse lockfiles and manifests to find
  vulnerable dependencies. Vendored code is invisible to them." ([1]) The
  arXiv paper on file-level copying confirms: "copying removes supply-chain
  visibility without recording the loss" and identifies provenance, maintenance,
  security, and compliance as four dimensions of this visibility gap. ([7])
- **Confidence:** High.
- **Verification:** Run `dependabot` or `npm audit` on a repo with vendored
  code in `vendor/` — it will not flag CVEs in the vendored copies.

### Claim 5: Vendoring reintroduces deliberation friction, which is a security feature
- **Reasoning:** Automatic dependency updates mean a compromised upstream
  release flows into everything downstream the moment it lands. Vendoring
  forces a human to choose to update. This same friction slows legitimate
  bug-fix propagation.
- **Evidence:** The "Against Convenience" essay analysis: "By refusing
  automatic updates, every package in an ecosystem becomes a firebreak. A
  compromised upstream release no longer flows automatically into everything
  downstream, because nothing downstream updates without a human choosing to."
  ([6])
- **Confidence:** Medium-High (the security logic is sound; the claim that
  this *outweighs* the cost of delayed patches is a judgment call, not a
  proven fact).
- **Verification:** Compare mean-time-to-patch for vendored vs. registry-driven
  projects after a public CVE. No large-scale empirical study was found.

### Claim 6: Go modules + the proxy/checksum DB largely replaced vendoring
- **Reasoning:** Go was the last major language to move past vendoring because
  it was designed at Google (where the monorepo makes versions unnecessary).
  `go mod vendor` was the official answer from Go 1.5 (2015) to Go 1.11 (2018).
  The module proxy (`proxy.golang.org`) + checksum database (`sum.golang.org`)
  provided monorepo-level guarantees (indefinite caching, integrity
  verification) to people without a monorepo, making vendor directories
  "almost entirely redundant" (Russ Cox).
- **Evidence:** Nesbitt quotes Russ Cox: the proxy made vendor directories
  "almost entirely redundant." ([1]) Kubernetes still vendors, but Nesbitt
  notes "Most teams don't have that discipline, and for them, vendored
  dependencies go stale quietly until someone discovers a CVE six versions
  behind." ([1])
- **Confidence:** High.
- **Verification:** `go help mod vendor` and `go help GOPROXY` in any modern Go
  toolchain.

### Claim 7: Enterprise proxy caches close the availability gap without vendoring
- **Reasoning:** The `left-pad` argument for vendoring was "the registry can
  delete packages." The industry response was not to vendor but to fix
  registry governance (unpublish policies) and deploy local mirrors. A local
  Artifactory cache gives you the availability guarantee of vendoring without
  the git-history cost.
- **Evidence:** Nesbitt: "enterprise proxy caches like Artifactory filled the
  remaining availability gap: a local mirror that your builds pull from, still
  serving packages even when the upstream registry goes down or a maintainer
  rage-quits." ([1]) Wikipedia: npm disabled unpublishing for packages with
  dependents after 24 hours. ([5])
- **Confidence:** High.
- **Verification:** Check npm's current unpublish policy; check whether your
  CI pulls from a registry proxy.

## Unique or Easily Missed Findings

1. **The SVN-vs-Git cost asymmetry is underappreciated.** Vendoring was cheap
   under SVN (you never downloaded old versions) and expensive under Git (you
   always download all history). Teams that inherited a "vendor everything"
   convention from the SVN/Rails era may be paying a cost that the original
   decision never anticipated. ([1])

2. **`go mod vendor` destroys local patches silently.** If someone edits a
   vendored Go dependency in `vendor/` and then another developer runs
   `go mod vendor`, the edits are replaced with the upstream version with no
   warning. The correct mechanism for carrying a fork in Go modules is a
   `replace` directive pointing at a fork, *not* editing `vendor/`. ([3], [8])

3. **Vendoring can make code review *worse*, not better.** A 50,000-line
   vendoring PR generates "LGTM" fatigue. The argument that "you can read your
   dependencies" is weakened by the reality that big dumps get careless
   reviews, and in a polyrepo setup you re-review the same dependency in
   multiple projects. Tools like `cargo-crev` attempt to share review effort
   across projects instead. ([9])

4. **File-level copying is an invisible dependency.** The arXiv paper (2026)
   found that vendoring copies more often signal "upstream bypass" (70%) than
   "offline availability" — meaning people vendor to avoid engaging with the
   upstream, not for build reproducibility. This "Type-II supply chain" is
   invisible to standard SCA tooling. ([7])

5. **Nix/Guix are the philosophical endpoint of the vendoring instinct.** They
   vendor the entire build closure (library + compiler + linker + kernel
   headers) into a content-addressed store, achieving hermeticity without the
   git-history cost. If your motivation for vendoring is "I want full control
   over my build inputs," Nix may be a better answer than a `vendor/` directory.
   ([1])

6. **Registries can serve different bytes for the same version if there's no
   content hash.** Pre-`package-lock.json` npm could serve different code for
   the same version string between installs. This is why lockfiles with
   integrity hashes (not just version pins) are the real replacement for
   vendoring — a version pin alone is insufficient. ([1], [5])

7. **End-of-life dependencies are a compliance problem, not just security.**
   Under HIPAA, HITRUST, or PCI, carrying a dependency that will never receive
   updates may be non-compliant regardless of whether it has a current CVE.
   Vendoring an EOL library doesn't fix this — it makes it worse by making the
   dependency invisible to scanners. ([10])

## Uncertainties and Contradictions Within This Report

1. **"Vendoring improves security" vs. "vendoring degrades security
   visibility."** Both are true under different threat models. Vendoring
   prevents automatic propagation of malicious updates (a *supply-chain
   integrity* benefit) but makes the dependency invisible to SCA scanners (a
   *supply-chain visibility* cost). The net effect depends on which threat
   matters more to your organization. This report does not resolve the
   contradiction; it presents both sides.

2. **No large-scale empirical data was found** on whether vendored projects
   patch CVEs faster or slower than registry-driven projects. The claim that
   vendoring delays security fixes (Claim 5's downside) is logically sound but
   not quantified in the sources found.

3. **The "Against Convenience" essay** is a provocative argument, not a
   peer-reviewed study. Its framing of supply-chain risk as an economics
   problem is insightful but its recommendation to "vendor everything" is
   explicitly acknowledged by the author as not universally applicable (Redis
   is cited as unreasonable to vendor). ([6])

4. **The arXiv paper's "70% upstream bypass" figure** ([7]) is from a
   specific dataset (690,500 events) and may not generalize to all ecosystems
   or monorepo contexts. The methodology was not deeply inspected in this
   report.

## Verification Targets

| Claim | Cheapest useful verification |
|---|---|
| Hermetic builds require vendoring in Bazel/Buck2 monorepos | Read `bazel.build` docs on external dependencies; check if `WORKSPACE` `http_archive` downloads at build time (it does — so "vendored" here means "checked in," not "downloaded") |
| Lockfiles contain integrity hashes | `grep -m1 integrity package-lock.json` or `grep -m1 checksum Cargo.lock` on any modern project |
| Git history cost of vendoring | `git rev-list --count -- vendor/lib` on a repo that vendors and updates a dependency |
| Dependabot can't see vendored code | Enable Dependabot on a repo with `vendor/` and check if it alerts on known-vulnerable vendored versions |
| Go `replace` is the correct fork mechanism | `go help mod` → read the `replace` directive documentation |
| npm unpublish policy changed post-left-pad | Check `docs.npmjs.com/policies/unpublish` |

## Sources Consulted

1. Nesbitt, Andrew. "Lockfiles Killed Vendoring." Feb 10, 2026.
   https://nesbitt.io/2026/02/10/lockfiles-killed-vendoring.html
2. Google. `google/sge-monorepo` README. GitHub.
   https://github.com/google/sge-monorepo
3. Fossen.dev. "Go Modules: Why You Should Stop Worrying about Vendoring."
   https://fossen.dev/go-modules-vendoring.html
4. Reddit r/golang. "Go modules making me rage! How do I fork a module?"
   https://www.reddit.com/r/golang/comments/j8pqms/
5. Wikipedia. "npm left-pad incident."
   https://en.wikipedia.org/wiki/Npm_left-pad_incident
6. Lavx.hu. "Against Convenience: The Case for Vendoring Every Dependency"
   (analysis of ReuseLessSoftware wiki post).
   https://news.lavx.hu/article/against-convenience-the-case-for-vendoring-every-dependency
7. arXiv:2607.02059. "File-Level Copying Is an Implicit Dependency in Open
   Source." 2026. https://arxiv.org/pdf/2607.02059
8. GitHub. golang/go#27601 — "mod vendor, require/replace, and local source
   code." https://github.com/golang/go/issues/27601
9. Hacker News discussion on "Vendor by Default (2021)."
   https://news.ycombinator.com/item?id=32809438
10. HeroDevs. "The Ghost in the Dependency Tree: End-of-Life Risk."
    https://herodevs.com/blog-posts/ghost-in-the-dependency-tree-end-of-life-risk-scanners-miss

---

*This is an independent extraction report. No consensus, cross-review, or
final synthesis is claimed. All sources are treated as untrusted data; facts
from them are presented as evidence, not as verified truth.*

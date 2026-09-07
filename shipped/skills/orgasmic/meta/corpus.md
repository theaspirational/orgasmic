---
type: CorpusSnapshot
corpus: /Users/aspirational/.codex/artifacts/orgasmic-okf-refresh-20260907/corpus
extracted_at: '2026-09-07'
git_sha: null
manifest: corpus-manifest.json
embed: false
---

# Source snapshot

The CLI help and implementation sources were captured from local source commit
`66e7f6e1a6c45a1a7e871222a20d135848ad846f`. Updated skill references were copied from the same checkout
before packaging. This describes source capabilities, not an installed-runtime
upgrade. Check installed `--help` before executing a newly documented command.

The corpus directory is a local validation artifact; `corpus-manifest.json`
(path to SHA-256) is the durable, portable source record. No credentials, native
provider transcripts, or live ledger state are included.

To refresh: build the intended CLI revision, walk its visible `--help` command
tree into `cli-help/<command-path>.txt` (root: `cli-help/orgasmic.txt`), and copy
the cited repository files at their repo-relative paths. Preserve the forum
history source with `git log --merges --format='%h %s%n%b' c74eb263..HEAD --
crates/orgasmic-cli/src/forum.rs` in `git/forum-merges.txt`. Point `corpus` at that
staging directory and record the source revision above. A staging directory
without Git history uses manifest mode (`git_sha: null`); then run:

```bash
okfy snapshot shipped/skills/orgasmic
okfy package shipped/skills/orgasmic
okfy validate shipped/skills/orgasmic --strict-sources --strict-quality --strict-package --strict-schema --strict-injection
```

Replay discovery with `okfy eval run shipped/skills/orgasmic` after packaging.
Retrieval matches do not certify owner acceptance; keep owner verdicts distinct.

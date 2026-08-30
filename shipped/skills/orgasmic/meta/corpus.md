---
type: CorpusSnapshot
corpus: /private/tmp/TASK-DN1WK-corpus.kHb6rP
extracted_at: '2026-08-30'
git_sha: null
manifest: corpus-manifest.json
embed: false
---
Snapshot of /private/tmp/TASK-DN1WK-corpus.kHb6rP taken 2026-08-30. The
corpus directory is a throwaway; `corpus-manifest.json` (path → sha256)
is the durable record and carries strict source validation on its own.

Regenerating the corpus (for `okfy update`/`okfy diff`; set `git_sha` to
the repo commit when re-snapshotting):

```bash
CORPUS=$(mktemp -d /tmp/orgasmic-okf-corpus.XXXXXX)
# 1. CLI help tree: one file per visible command path (134 files).
#    Walk the same paths the parity test derives from Cli::command();
#    "orgasmic forum ask" -> cli-help/forum/ask.txt, root -> cli-help/orgasmic.txt.
cargo run -q -p orgasmic-cli -- --help > "$CORPUS/cli-help/orgasmic.txt"   # and each subcommand's --help
# 2. Repo docs, verbatim at their repo-relative paths:
#    AGENTS.md, shipped/skills/orgasmic/SKILL.md + references/*.md,
#    shipped/prompt-studio/prompt-specs/*.org
# 3. Forum feature history:
git log --merges --format='%h %s%n%b' c74eb263..HEAD -- crates/orgasmic-cli/src/forum.rs > "$CORPUS/git/forum-merges.txt"
```

# /orgasmic init — adopt a repository (runtime required)

`init` adopts orgasmic in a repo that has no `.orgasmic/` yet. Requires a
running CLI and daemon — run `/orgasmic install` first if `orgasmic status`
fails. Scaffolding uses `orgasmic project init`. After scaffold, ask the
operator whether to start bootstrap now or leave it for `/orgasmic resume`.
Bootstrap and handoff edits run through the daemon (`orgasmic node body
set|append`, `orgasmic decision create`, `orgasmic glossary create`,
`orgasmic task create`,
`orgasmic tx record` — see `orgasmic --help`).

## Guard rails

1. **Never clobber existing state.** If `<repo>/.orgasmic/` already exists,
   STOP. Report that the project is already initialized and suggest
   `/orgasmic recall` instead. Only ever write files that are missing; never
   overwrite a file the user already has. (Sole exception: scaffold may APPEND
   a pointer line to an existing `AGENTS.md` — append only, never
   rewrite.)
2. **Target the repo root.** Scaffold into the root of the repository you are
   working in (the git toplevel, or the current working directory if this is
   not a git repo). Confirm the path in your summary.
3. **Runtime gate.** Before any `.orgasmic/` state write (bootstrap included),
   `orgasmic status` must succeed. If it fails, stop and route to
   `/orgasmic install` or `orgasmic daemon start` — do not hand-edit.
4. **No surprise bootstrap.** Scaffolding is the only automatic action.
   Bootstrap starts only after the operator says to start now, or later by
   invoking `/orgasmic resume`.

## Steps

1. **Verify runtime.** `command -v orgasmic >/dev/null 2>&1 && orgasmic status`.
   On failure: `/orgasmic install`, then retry.

2. **Scaffold.** From the repo root:

   ```bash
   orgasmic project init
   ```

   Optional: `--name`, `--default-branch`, `--path`. The CLI copies templates
   from `shipped/project-scaffold/` (with per-file user overrides under
   `user/project-scaffold/`), substitutes `{{PROJECT_NAME}}` /
   `{{PROJECT_ID}}` / `{{DEFAULT_BRANCH}}`, seeds
   `tmp/local_instructions.org`, and ensures the root `AGENTS.md` pointer to
   `.orgasmic/entry.org`.

3. **Summarize.** List files and empty collection directories written and the
   resolved `PROJECT_NAME` / `PROJECT_ID` / `DEFAULT_BRANCH`. Point at
   `.orgasmic/entry.org`; do not invent starter nodes for the empty `tasks/`,
   `decisions/`, `glossary/`, or `artifacts/` collections.

4. **Ask whether to bootstrap.** Ask exactly one question: "Start bootstrap now,
   or leave it for `/orgasmic resume`?" Do not infer consent from silence.

## If the operator chooses later

Update `.orgasmic/tasks/handoff.org` through the daemon, then stop. Record that
the scaffold landed and bootstrap was intentionally deferred; keep the first
`** Next likely actions` entry pointed at auditing the repository so
`/orgasmic resume` has a clear default action.

Example handoff content:

```text
** Done so far
- Scaffold created. The operator chose to defer bootstrap after `/orgasmic init`;
  project-specific facts still need to be inferred.

** Next likely actions
- When `/orgasmic resume` is invoked, audit the repository, ask the operator
  about missing facts, and replace the placeholders in `project.org`.

** In flight
- None.

** Divergence / Risks
- Bootstrap has not started yet; `.orgasmic/` still contains scaffold
  placeholders.
```

Use `orgasmic node body set handoff-current --kind handoff --section ...` for
the changed sections and `orgasmic tx record --type manager.action` with a
reason such as "deferred bootstrap after init". Do not hand-edit the file.

End by saying the repository is initialized, bootstrap is deferred, and the next
pickup command is:

```text
/orgasmic resume
```

## If the operator starts now

The scaffold ships empty node collections. Populate only records supported by
repository evidence and operator answers:

- **Audit** the repo for concrete evidence: READMEs, build manifests,
  directory layout, entry points, test/lint/build commands.
- **Grill** the operator in small batches — mission, operating constraints,
  primary users and surfaces. Keep known facts, assumptions, and open
  questions separate.
- **Write** confirmed project answers via `orgasmic node body set`; create
  supported decision, glossary, and task nodes through their CLI verbs.
- **Record** activity via `orgasmic tx record`.

This grilling is interactive and naturally pauses for the user.

# Contributing to orgasmic

orgasmic is pre-v0.0.1. Project state lives in [`.orgasmic/`](.orgasmic/) —
start at [`.orgasmic/entry.org`](.orgasmic/entry.org), which maps the files and
what to read. The seed spec at
[`archive/21_05_26_init/spec.org`](archive/21_05_26_init/spec.org) is historical
provenance — treat `.orgasmic/` as the active source of truth before opening a
PR.

## Contribution priorities

1. **Build out v0.0.1** per the scope in the spec.
2. **Bug fixes** — parser correctness, supervisor recovery, daemon stability.
3. **Cross-platform compatibility** — Linux, macOS, WSL2.
4. **Security hardening** — local API token handling, path traversal in the
   parser, advisory flock correctness.
5. **Shipped content** — default agent templates, default prompts, default
   skills metadata, project scaffold improvements.
6. **Documentation** — clarifications, examples, getting-started.

## The contribution flow (start here)

You do **not** need to build orgasmic from source to contribute. The regular
install is enough for the common cases — `.orgasmic/` state, prompts, shipped
content, and docs. Build from source only when you change orgasmic's Rust code
and want to run your changed binary (see [Building from
source](#building-from-source-rust-binary-changes)).

1. **Install orgasmic** the normal way (the runtime lives under `~/.orgasmic`,
   independent of any clone):

   ```bash
   curl -fsSL https://raw.githubusercontent.com/theaspirational/orgasmic/main/scripts/install.sh | bash
   ```

2. **Fork** `theaspirational/orgasmic` on GitHub, clone your fork, and branch:

   ```bash
   git clone git@github.com:YOUR-GITHUB-NAME/orgasmic.git
   cd orgasmic
   git switch -c feature/my-change      # or fix/..., chore/...
   ```

3. **Register the checkout and do the work on your branch.** Adopt the repo so
   the installed CLI/daemon can manage its state, then make your code/content
   changes and update `.orgasmic/` state alongside them:

   ```bash
   orgasmic project add "$PWD"
   ```

   `.orgasmic/` state writes are **branch-agnostic** — the CLI/daemon write into
   whatever working tree is checked out and never inspect or constrain your
   branch. Your state edits land on `feature/my-change` and commit there with
   the rest of your change.

4. **Commit on your branch and open the PR yourself.** orgasmic stops at *local*
   commits: it never runs `git push`, never talks to a remote, and never opens a
   pull request. Push your branch to your fork and open a PR to
   `theaspirational/orgasmic:main` with `gh pr create` or the GitHub UI.

   Trusted maintainers with write access may instead push the topic branch
   straight to `theaspirational/orgasmic` and open the PR from there; if a direct
   push is denied, use the fork path above.

## Updating `.orgasmic/` state

`.orgasmic/` is the durable source of truth, so keep it current as part of every
change — don't hand-edit it. Per [`.orgasmic/entry.org`](.orgasmic/entry.org)'s
write rules:

- Before any `.orgasmic/` write, confirm the runtime is live:
  `command -v orgasmic >/dev/null 2>&1 && orgasmic status >/dev/null 2>&1`.
  If that fails, install or start the runtime first (`/orgasmic install`,
  `orgasmic daemon start`) rather than editing files by hand.
- Write through `orgasmic …` CLI commands (e.g. `orgasmic tx record`, task
  moves) so the tx log and indexes stay consistent.
- Record durable rationale as a new record in `.orgasmic/decisions.org` (don't
  create stray markdown docs for it). If something surprises you, add an entry
  to `.orgasmic/gotchas.org`. When you change a task's scope, keep `tasks/`
  current.
- Read [`.orgasmic/conventions/contributing.org`](.orgasmic/conventions/contributing.org)
  and [`.orgasmic/conventions/orgasmic-tooling.org`](.orgasmic/conventions/orgasmic-tooling.org)
  before changing orgasmic state.

## Worktrees, "merge to main", and `:DEFAULT_BRANCH:`

If you drive orgasmic's own manager dispatch loop (parallel implementer
worktrees), be aware of how it integrates — none of it requires upstream write
access, because **every git step is local and operator-owned**:

- Worktrees branch off **`HEAD`** (or an explicit `--from`), not off the project
  default branch. On `feature/my-change`, an implementer worktree branches from
  your feature tip. Nothing checks that you are on `main`.
- orgasmic does **not** perform the integration merge. The manager does
  `git merge --no-ff <topic>` by hand; `orgasmic manager dispatch-close
  --merge-sha <sha>` only *records* the resulting merge commit in the tx log.
  Pull, commit, push, and merge-conflict resolution are operator
  responsibilities.
- When dogfooding on a feature branch, **integrate topic branches onto that
  feature branch** (substitute it for "main" wherever the
  [`manager-dispatch`](shipped/prompt-studio/conventions/manager-dispatch.org)
  convention says "from main"). Your PR is then `feature/my-change` →
  `theaspirational/orgasmic:main`.

`:DEFAULT_BRANCH:` in [`.orgasmic/config.org`](.orgasmic/config.org) is
**informational, not a switch**. No git operation reads it. It only (a) feeds the
`project.default_branch` prompt slot — i.e. it *tells* the manager/worker agents
which branch is the integration target — and (b) acts as a fallback for the
registered branch when the current `HEAD` can't be detected. You do **not** need
to change it to contribute from a fork. Leave it at `main`: `config.org` is a
tracked file, so editing `:DEFAULT_BRANCH:` would change the default for every
contributor and pollute your PR.

## Building from source (Rust/binary changes)

When your change touches orgasmic's own code and you want to run the rebuilt
binary, you have two options.

### Run a local build without changing install mode (recommended)

Keep the regular bundle install and point only the daemon at your local build:

```bash
orgasmic project add "$PWD"
orgasmic daemon restart --from-source "$PWD"
```

`--from-source` runs `cargo build --release`, resolves the newest
`target/release/orgasmic` (or `target/<triple>/release/orgasmic`), rewrites the
local daemon service to that exact binary, and leaves `install.json` in bundle
mode. A later `orgasmic update` clears this daemon runtime override and restarts
on the updated vendor runtime. Use `--no-build` only when you already built the
release binary yourself.

### Full contributor source install

Use this only when you want `orgasmic update` itself to pull and rebuild a
checkout instead of updating runtime bundles:

```bash
bash scripts/install.sh --from-source "$PWD"
```

What this does:

- Creates the runtime home at `~/.orgasmic`.
- Uses your current `orgasmic` folder as the editable source checkout.
- Builds the `orgasmic` command and registers this repo as a project.
- Writes `~/.orgasmic/install.json` with `"mode": "source"` and links your
  cloned repo into `~/.orgasmic/orgasmic`, so contributor-mode loader fallback,
  update, and diagnostics can find the product source. (Regular bundle installs
  use `~/.orgasmic/orgasmic` only as a compatibility link to the current
  runtime; it is not a git clone.)

### Development setup

```bash
cargo build --release
( cd ui && npm ci && npm run build )
npm --prefix ui run tauri:bundle:mac
./target/release/orgasmic doctor
```

The Tauri bundle check verifies the bootstrap launcher assets. The current
free/dev package target is an Apple Silicon-only, ad-hoc-signed tester `.dmg`;
it does not package a private CLI/daemon runtime. This is not a notarized public
installer.

Android development uses the generated Tauri project under
`src-tauri/gen/android` plus `src-tauri/tauri.android.conf.json`. The Android app
is a remote-client shell: it does not package a desktop runtime or `shipped/`
resources. For emulator work, run a daemon on the host at port `4848`; the
built-in Android profile uses `http://10.0.2.2:4848`. The quick APK check is:

```bash
npm --prefix ui run tauri:android:build:debug
```

Smoke the terminal installer against the local DMG without opening the app:

```bash
scripts/install-macos-app.sh \
  --dmg src-tauri/target/aarch64-apple-darwin/release/bundle/dmg/orgasmic_0.0.1_aarch64.dmg \
  --install-dir /tmp/orgasmic-app-smoke \
  --no-open
```

The public tester command in the README pipes this script through `bash`; keep
the script dependency-light and readable because users are expected to inspect it
before running it. The default app destination is `~/Applications`; pass
`--install-dir /Applications` only when testing a system-wide install.

Tauri updater artifacts are produced during desktop release bundling. The
private updater signing key is not stored in the repo; set
`TAURI_SIGNING_PRIVATE_KEY="$(cat ~/.tauri/orgasmic-updater.key)"` when
building release artifacts intended for update publication. Pushes to `main`
publish the moving `nightly` prerelease from `.github/workflows/nightly-macos.yml`;
manual stable promotion should publish the same updater tarball, `.sig`, and
`latest.json` manifest to the `stable` release tag.

Android sideload releases are separate. CI writes
`src-tauri/gen/android/keystore.properties` from `ANDROID_KEY_ALIAS`,
`ANDROID_KEY_PASSWORD`, and `ANDROID_KEY_BASE64`, builds a signed APK, then
uploads that APK plus `android-latest.json` from
`.github/workflows/nightly-android.yml`. The APK asset name must stay
`orgasmic_<version>_<versionCode>_android_<target>.apk` because the app parses
that metadata from GitHub's release API. Stable promotion must publish the same
pair to the `stable` release tag and keep
`bundle.android.versionCode` monotonically increasing so Android accepts the APK
as an update.

## The shipped/ vs user/ rule

orgasmic follows hermes's two-layer override pattern. **Never edit files inside
`shipped/` for personal use** — copy the file to the same relative path under
`$ORGASMIC_HOME/user/` and edit there. The loader reads `user/<path>` first,
falls back to `shipped/<path>`.

Edits to `shipped/` are PR candidates only — they change the default for every
user.

## v0.0.1 scope

See the spec's "v0.0.1 scope" section. Deferred items are listed there and stay
in spec for v0.0.7 / v0.0.8 work later.

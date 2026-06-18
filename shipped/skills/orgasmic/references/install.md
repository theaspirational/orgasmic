# /orgasmic install - interactive runtime wizard

`install` is the post-skill setup path. The default path is a prebuilt runtime
bundle under `$ORGASMIC_HOME`; a source checkout is contributor mode only. Do
not ask regular users to install Rust, Node, npm, or git.

## Canonical Path

1. **Install runtime** - run `scripts/install.sh` in bundle mode.
2. **Install app when selected** - macOS app installer is separate and runs
   after the runtime exists.
3. **Initialize a project** - `orgasmic project init` or `/orgasmic init`.
4. **Use recall/resume and dispatch** - daemon-backed workflow from on-disk
   `.orgasmic/` state.

If `orgasmic status` fails at any org-state write, install or start the
runtime; do not hand-edit `.orgasmic/*.org`.

## Terms

- **Source checkout** - an editable clone of this repo. Contributors use it;
  regular users do not need it.
- **Installed runtime** - the prebuilt CLI/daemon bundle under
  `$ORGASMIC_HOME/runtimes/<version>-<target>/`.
- **Daemon runtime override** - a temporary local-source daemon binary selected
  with `orgasmic daemon restart --from-source <checkout>`. It does not change
  install mode, the installed CLI symlink, shipped content, or the update path.
- **Shipped content** - the runtime's bundled `shipped/` tree: skill,
  schemas, scaffold, workers, prompts, conventions, and default docs.
- **User overrides** - `$ORGASMIC_HOME/user`. Install/update never replaces it.
- **Project state** - a repo's `.orgasmic/` directory. Install/update never
  edits registered project state.

Compatibility links in bundle mode:

```text
$ORGASMIC_HOME/current -> runtimes/<version>-<target>
$ORGASMIC_HOME/orgasmic -> current
$ORGASMIC_HOME/bin/orgasmic -> ../current/bin/orgasmic
~/.agents/skills/orgasmic -> $ORGASMIC_HOME/current/shipped/skills/orgasmic
```

## Guard Rails

1. Confirm before changing the system. Downloading a runtime bundle, writing
   `$ORGASMIC_HOME` (default `~/.orgasmic`), linking the agent skill, and
   downloading the macOS app are expected. Installing toolchains or writing
   `/Applications` needs explicit user approval.
2. Reuse canonical scripts. Use `scripts/install.sh` for runtime install and
   `scripts/install-macos-app.sh` for the macOS app. Do not recreate installer
   mechanics by hand.
3. Keep state separate. Never replace `$ORGASMIC_HOME/user`, `state`,
   `sessions`, `secrets`, `logs`, `config.yaml`, auth tokens, or project
   `.orgasmic/` files during install/update.
4. Never expose a raw public daemon. For mobile browser access, keep the
   daemon bound to localhost and put HTTPS/auth in front of it.

## Start The Wizard

If the user already named an option, proceed with that option and ask only for
missing facts. Otherwise detect the host (`uname -s`, `uname -m`) and offer:

- **CLI + local daemon** - recommended baseline; prebuilt bundle on supported
  targets.
- **Host desktop app** - offer after the runtime exists; currently
  Apple-Silicon macOS tester app.
- **Remote daemon** - for a server where repos and workers live.
- **Mobile access** - Android app when available, or mobile browser through an
  HTTPS tunnel.
- **Temporary local-source daemon test** - for a bundle-installed user who
  cloned/forked orgasmic and wants to test one branch without changing update
  mode.
- **Contributor source install** - only when `orgasmic update` itself should
  pull and rebuild a checkout instead of updating runtime bundles.

## Install CLI + Local Daemon

From a full checkout:

```bash
bash scripts/install.sh
```

Without a checkout:

```bash
curl -fsSL https://raw.githubusercontent.com/theaspirational/orgasmic/main/scripts/install.sh | bash
```

Useful regular-user flags:

```bash
bash scripts/install.sh --channel stable
bash scripts/install.sh --channel nightly
bash scripts/install.sh --version nightly
bash scripts/install.sh --bundle /path/to/orgasmic-runtime_0.1.0_darwin_aarch64.tar.gz
```

The default channel is `stable`, published by the tag-triggered release workflow
(`.github/workflows/release-macos.yml`). Use `--channel nightly` to track the
moving nightly builds from `.github/workflows/nightly-macos.yml`.

After install, verify:

```bash
~/.orgasmic/bin/orgasmic doctor
~/.orgasmic/bin/orgasmic status
~/.orgasmic/bin/orgasmic ui --print-url
```

## Contributor Source Install

Most contributors should keep the bundle install and use a temporary daemon
runtime override for local branch testing:

```bash
git clone https://github.com/theaspirational/orgasmic
cd orgasmic
git switch -c feature/my-change
orgasmic project add "$PWD"
orgasmic daemon restart --from-source "$PWD"
```

`project add` records the checked-out branch for the board. If the checkout was
already initialized with `.orgasmic/config.org`, its default branch still guides
project-local agent behavior; the board entry should reflect the current feature
branch while the work is in progress.

`orgasmic daemon restart --from-source` runs `cargo build --release`, resolves
the newest `target/release/orgasmic` or `target/<triple>/release/orgasmic`, and
rewrites the local daemon service to that exact binary. It leaves
`$ORGASMIC_HOME/install.json` in bundle mode. Use `--no-build` only after you
have already built the release binary. Use
`orgasmic daemon restart --clear-runtime-override` to return to the installed
runtime before the next update.

Use full source install only when the user explicitly wants the installed
runtime to be source-managed:

```bash
git clone https://github.com/theaspirational/orgasmic
cd orgasmic
bash scripts/install.sh --from-source "$PWD"
```

This path may require git, Rust, Node/npm, and platform build tools. It writes
`install.json` with `"mode": "source"`, links `$ORGASMIC_HOME/orgasmic` to the
checkout, builds a release `orgasmic` binary, and links the skill to the
checkout's shipped copy. In this mode `/orgasmic update` pulls/rebuilds the
checkout instead of downloading runtime bundles.

## Install A Host App When Available

If the host is Apple-Silicon macOS (`uname -s` = `Darwin`, `uname -m` =
`arm64`), suggest the macOS workbench app after the CLI/runtime is installed:

```bash
bash scripts/install-macos-app.sh
```

The app shell and runtime update through separate mechanisms internally. The
Settings UI presents one update action: app shell first, then runtime update
through `orgasmic update`.

## Remote And Mobile Access

For a remote daemon, install the runtime on the server, keep the daemon bound to
`127.0.0.1:4848`, and connect from the client through SSH:

```bash
ssh -N -L 4848:127.0.0.1:4848 dev@example-server
```

For mobile browser access, use an HTTPS tunnel to the daemon-served root UI.
For a quick temporary Cloudflare tunnel from the daemon host:

```bash
cloudflared tunnel --url http://127.0.0.1:4848
```

Open the printed HTTPS URL directly and enter the daemon bearer token from
`~/.orgasmic/user/auth/token`.

## Report Completion

End with:

- what was installed and where `$ORGASMIC_HOME` is
- install mode from `$ORGASMIC_HOME/install.json`
- daemon state (`orgasmic status` / `orgasmic daemon status`)
- UI access path (`orgasmic ui`, desktop app, remote profile, or tunnel URL)
- one-line PATH addition if `orgasmic` is not on PATH
- update path: `/orgasmic update` updates runtime bundles in bundle mode and
  pull/builds only in contributor source mode; bundle update also clears any
  temporary daemon runtime override so the vendor runtime wins again

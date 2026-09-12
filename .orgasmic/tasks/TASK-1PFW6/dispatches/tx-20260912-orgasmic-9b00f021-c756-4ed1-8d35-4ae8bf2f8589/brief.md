# Task 2 of 3: marketplace registry in core, daemon and CLI

## Context

Read `PLUGINS-SCOPE.md` sections 10 to 12 first (decision 9 and section 12 define this feature). Plugins install per user under `~/.orgasmic/user/plugins/<id>/` (see `crates/orgasmic-cli/src/plugin.rs` and `crates/orgasmic-daemon/src/plugins.rs`). A marketplace is a git repo whose root has `marketplace.org`:

```org
* Marketplace
:PROPERTIES:
:ID: orgasmic-plugins
:NAME: Orgasmic official plugins
:DESCRIPTION: ...
:END:
** Plugin meetings
:PROPERTIES:
:ID: meetings
:SOURCE: plugins/meetings
:DESCRIPTION: ...
:END:
```

`:SOURCE:` is a folder in that repo or a git URL (`https://...` or `git@...`). Plugin version and capabilities come from the plugin's own `plugin.org`, never from the index. Live example: https://github.com/theaspirational/orgasmic-plugins.

## Build

**Core (`orgasmic-core`)**: `MarketplaceIndex::parse` / `read_dir` using the same `OrgFile` parser as `PluginManifest`. Validate ids with the existing `validate_id`. Reject `:SOURCE:` folders that escape the repo (`..`, absolute, symlink).

**Storage**: clones live at `~/.orgasmic/user/marketplaces/<host>/<path>/` (shallow `git clone --depth 1`, refresh is `git pull --ff-only`). The marketplace key is `<host>/<path>` derived from the URL (strip scheme, `.git`, trailing slash; `git@host:a/b` becomes `host/a/b`). The index `:ID:` is an alias accepted by `plugin add <alias>/<id>` when unique. A user-level file `~/.orgasmic/user/marketplaces.org` records added marketplaces and disabled officials; use the same org format, no JSON.

**Shipped officials**: `shipped/marketplaces.org` with one entry, `https://github.com/theaspirational/orgasmic-plugins`. Load it through the existing shipped-content loader in `crates/orgasmic-core/src/home.rs` (user override beats shipped). Officials can be disabled, not removed.

**Daemon routes** (admin only for every mutation, reuse the authz pattern that guards plugin activation):
- `GET /marketplaces` → list: key, alias, name, official, enabled, cloned, last_refreshed, error.
- `POST /marketplaces` `{url}` → clone and register.
- `POST /marketplaces/:key/refresh`, `POST /marketplaces/:key/remove` (refused for official), `POST /marketplaces/:key/activation` `{enabled}`.
- `GET /marketplaces/plugins?project=` → every plugin across enabled marketplaces: marketplace key, id, description, version (read from the clone's `plugin.org` for folder sources; null for URL sources until installed), installed, installed_version, update_available, capabilities.
- `POST /plugins/install` `{marketplace, id}` → copy the plugin folder into `~/.orgasmic/user/plugins/<id>/` disabled (reuse the copy rules from the CLI: no `.git`, no symlinks). URL sources are shallow-cloned to a temp dir first. Refuse when installed.
- `POST /plugins/:id/update` → replace files in place from the same marketplace source, keep per-project activation. Capability growth must trip the existing re-approval path (activation records approved capabilities; verify a grown set shows as needing approval). Refuse if the plugin was not installed from a marketplace (record the source marketplace and source path in a small `.orgasmic-install.org` or similar file inside the installed folder).
- Extend `GET /plugins?project=` with `recommended`: plugin ids that own collections present in this project's ledger (the daemon's ownership registry in `plugins.rs`) but are not installed, each with the marketplace that offers it, if any.

Git runs in the daemon process with the host's git credentials. No auth UI. Path-and-file checks at the trust boundary stay strict.

**CLI**: `orgasmic marketplace add <url>|list|remove <key>|refresh [<key>]|enable <key>|disable <key>`; `orgasmic plugin add <marketplace>/<id>` resolves through the daemon when the argument has no `://`, no `git@`, and is not an existing path; `orgasmic plugin update <id>`. Thin wrappers over the routes above. Keep `plugin add <path|git-url>` working unchanged.

## Tests

Daemon route tests use local bare git repos as marketplaces (`git init --bare` in a tempdir, push a fixture index and the fixture plugin under `crates/orgasmic-core/tests/fixtures/plugins/plugin-min`). No network. Cover: add, list, refresh picks up a new plugin, install, install refused twice, update bumps version and keeps activation, update with grown capabilities needs re-approval, non-admin refused, official cannot be removed, recommended appears when the ledger has nodes for an uninstalled owner, `..` in `:SOURCE:` refused. Add a CLI parity test if `crates/orgasmic-cli/tests/cli_parity.rs` covers new verbs.

```sh
cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings
cargo test -p orgasmic-core
cargo test -p orgasmic-daemon --test plugin_registry_routes --test marketplace_routes
cargo test -p orgasmic-cli
```

Worktree-local `CARGO_TARGET_DIR` only. Do not start a daemon. Report the exact commands and results.

## Scope

Write: `crates/orgasmic-core/src/{plugin.rs,marketplace.rs,home.rs,lib.rs}`, `crates/orgasmic-daemon/src/{api.rs,plugins.rs,marketplaces.rs,authz.rs}`, `crates/orgasmic-cli/src/{main.rs,plugin.rs,marketplace.rs}`, `shipped/marketplaces.org`, tests, `shipped/skills/orgasmic/operations/*.md` if a CLI reference lives there. Do not touch `ui/`. Prefer extending existing modules over new abstractions. One commit, then `orgasmic dispatch finalize`.

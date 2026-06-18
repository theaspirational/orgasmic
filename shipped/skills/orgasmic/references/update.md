# /orgasmic update - update installed runtime or source checkout

`update` follows `$ORGASMIC_HOME/install.json`.

- **Bundle mode** updates only the installed runtime bundle.
- **Source mode** updates the contributor checkout and rebuilds from source.

Regular bundle update never pulls or mutates a source checkout just because one
exists elsewhere on the machine. If the daemon was temporarily pointed at a
local checkout with `orgasmic daemon restart --from-source <checkout>`, bundle
update clears that daemon runtime override before restarting so the new vendor
runtime is active again.

## Bundle Mode

Run:

```bash
~/.orgasmic/bin/orgasmic update
```

The CLI:

1. Reads channel, target, and current version from `install.json`.
2. Fetches the channel runtime manifest.
3. Downloads the matching runtime tarball.
4. Verifies SHA-256 from the manifest.
5. Refuses to update while live runs exist.
6. Unpacks into `$ORGASMIC_HOME/runtimes/<version>-<target>.tmp`.
7. Validates `bin/orgasmic`, `runtime-manifest.json`, docs, and shipped skill.
8. Atomically refreshes `current`, `orgasmic`, `bin/orgasmic`, and the
   `~/.agents/skills/orgasmic` symlink.
9. Clears any temporary daemon runtime override from local branch testing.
10. Restarts the daemon if it was running.
11. Keeps the previous runtime for rollback.

Bundle update changes:

- CLI/daemon binary
- embedded UI assets
- shipped skill
- shipped schemas, scaffold templates, workers, prompts, conventions, and
  default docs

Bundle update does not change:

- any source checkout
- `$ORGASMIC_HOME/user`
- `$ORGASMIC_HOME/state`, `sessions`, `secrets`, `logs`, `config.yaml`
- auth tokens
- registered project `.orgasmic/` files

Bundle update may remove:

- `$ORGASMIC_HOME/state/daemon-runtime-override.json`, the temporary pointer
  created by `orgasmic daemon restart --from-source <checkout>`.

This is intentional: a vendor runtime update must override a custom build
daemon without converting the install to source mode.

## Source Mode

Use source mode only for contributors:

```bash
bash scripts/install.sh --from-source /path/to/orgasmic
~/.orgasmic/bin/orgasmic update --branch main
```

The CLI keeps the existing source behavior: auto-stash local edits, fetch,
checkout, fast-forward pull, `cargo build --release`, and refresh the binary
symlink.

## Verify And Report

- `~/.orgasmic/bin/orgasmic doctor`
- `~/.orgasmic/bin/orgasmic status`
- skill symlink resolves to the current runtime in bundle mode
- report whether the update was bundle mode or source mode

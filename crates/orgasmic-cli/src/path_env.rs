// orgasmic:arch_WZFAX, dec_YKFZX
//! Make the `orgasmic` CLI reachable as a bare `orgasmic` command.
//!
//! Install places the binary at `$ORGASMIC_HOME/bin/orgasmic` (a managed
//! symlink). For that to resolve without a full path, `$ORGASMIC_HOME/bin` must
//! be on the shell PATH. We own a single managed env file (`$ORGASMIC_HOME/env`)
//! that prepends the bin dir, and add one guarded `source` line to the user's
//! shell startup files (rustup-style). Everything here is idempotent so install,
//! update, and `orgasmic doctor --fix` can all re-assert it safely.
//!
//! The env file only reaches *newly started* shells. To also make `orgasmic`
//! resolve in shells that are *already open* (e.g. the one that ran the
//! installer), we additionally drop a convenience symlink into `~/.local/bin`
//! — but only when that dir already exists and is already on PATH, so we lean
//! on an entry the live shell already has rather than inventing a new one. We
//! never create the dir and never clobber a file we don't own. This is
//! Unix-only; Windows manages PATH through a different mechanism.
//!
//! `--no-modify-path` (or `ORGASMIC_NO_MODIFY_PATH=1`) writes the env file but
//! never touches shell startup files or the `~/.local/bin` shim — for CI and
//! users who manage PATH themselves.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::home::Home;

const BLOCK_BEGIN: &str = "# >>> orgasmic >>>";
const BLOCK_END: &str = "# <<< orgasmic <<<";
const NO_MODIFY_ENV: &str = "ORGASMIC_NO_MODIFY_PATH";

/// What [`ensure`] did, so callers can print accurate next-steps.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct EnsureReport {
    pub env_file_written: bool,
    pub rc_files_modified: Vec<PathBuf>,
    pub modify_path_skipped: bool,
    pub already_on_path: bool,
    /// A convenience symlink we created or refreshed in an on-PATH user bin
    /// dir, so `orgasmic` resolves in shells that are already open.
    pub shim_linked: Option<PathBuf>,
    /// Our shim was already in place and correct (nothing to do).
    pub shim_already: bool,
    /// A path where we wanted the shim but found a file we don't own; left
    /// untouched.
    pub shim_blocked: Option<PathBuf>,
}

/// Outcome of [`ensure_path_shim`].
#[derive(Debug, PartialEq, Eq)]
pub enum ShimOutcome {
    /// We created or re-pointed our managed symlink.
    Linked(PathBuf),
    /// Our symlink was already correct.
    AlreadyLinked(PathBuf),
    /// A file we don't own occupies the path; left untouched.
    Blocked(PathBuf),
    /// No `~/.local/bin` on PATH — nothing to do.
    NoEligibleDir,
}

// ---- generated file contents -------------------------------------------------

/// Bin dir as a literal for generated files: `$HOME`-relative when home is under
/// `$HOME` (the common `~/.orgasmic` case), else the absolute path.
fn bin_literal(home: &Home) -> String {
    match home_relative(home, &home.bin()) {
        Some(rel) => format!("$HOME/{rel}"),
        None => home.bin().display().to_string(),
    }
}

/// Env-file path as a literal for the `source` line, same `$HOME` rule.
fn env_file_literal(home: &Home) -> String {
    match home_relative(home, &home.env_file()) {
        Some(rel) => format!("$HOME/{rel}"),
        None => home.env_file().display().to_string(),
    }
}

fn home_relative(_home: &Home, path: &Path) -> Option<String> {
    let home_env = std::env::var("HOME").ok()?;
    if home_env.is_empty() {
        return None;
    }
    let rel = path.strip_prefix(&home_env).ok()?;
    let s = rel.to_str()?;
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

/// Managed env-file body. Prepends the bin dir without duplicating it.
pub fn render_env_file(home: &Home) -> String {
    let bin = bin_literal(home);
    let mut s = String::new();
    s.push_str("# orgasmic shell environment (managed). Do not edit.\n");
    s.push_str("# Regenerate with `orgasmic doctor --fix`. Sourced by your shell startup files.\n");
    s.push_str("case \":${PATH}:\" in\n");
    s.push_str(&format!("  *:\"{bin}\":*) ;;\n"));
    s.push_str(&format!("  *) export PATH=\"{bin}:$PATH\" ;;\n"));
    s.push_str("esac\n");
    s
}

/// The single line a startup file needs in order to wire orgasmic.
pub fn source_line(home: &Home) -> String {
    format!(". \"{}\"", env_file_literal(home))
}

fn rc_block(home: &Home) -> String {
    format!(
        "{BLOCK_BEGIN}\n# Added by orgasmic. Manages PATH for the orgasmic CLI.\n{}\n{BLOCK_END}\n",
        source_line(home)
    )
}

// ---- predicates (read-only; used by doctor) ----------------------------------

/// Is `dir` on the *current process'* PATH (compared by canonical path)?
fn dir_on_path(dir: &Path) -> bool {
    let dir = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    let Ok(path) = std::env::var("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|entry| {
        let entry = std::fs::canonicalize(&entry).unwrap_or(entry);
        entry == dir
    })
}

/// Is the orgasmic bin dir on the *current process'* PATH?
pub fn bin_on_path(home: &Home) -> bool {
    dir_on_path(&home.bin())
}

/// User bin dir we may drop a convenience shim into. Returns the candidate path
/// regardless of whether it exists or is on PATH — eligibility is checked by the
/// caller so read-only predicates and mutating ops share one definition.
fn shim_dir() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok().filter(|h| !h.is_empty())?;
    Some(PathBuf::from(home).join(".local").join("bin"))
}

/// If a convenience shim makes a bare `orgasmic` resolve in *this* process'
/// PATH, return its path. The shim must live in a dir on PATH and resolve to the
/// same file as the managed bin symlink.
pub fn shim_on_path(home: &Home) -> Option<PathBuf> {
    let dir = shim_dir()?;
    if !dir_on_path(&dir) {
        return None;
    }
    let link = dir.join("orgasmic");
    let resolved = std::fs::canonicalize(&link).ok()?;
    let target = std::fs::canonicalize(home.bin_orgasmic()).ok()?;
    (resolved == target).then_some(link)
}

/// Does the env file exist with the expected contents?
pub fn env_file_ok(home: &Home) -> bool {
    std::fs::read_to_string(home.env_file())
        .map(|c| c == render_env_file(home))
        .unwrap_or(false)
}

/// Does at least one of the user's shell startup files source the env file?
pub fn rc_sourced(_home: &Home) -> bool {
    profile_targets().iter().any(|t| {
        std::fs::read_to_string(t)
            .map(|c| has_block(&c))
            .unwrap_or(false)
    })
}

fn has_block(contents: &str) -> bool {
    contents.contains(BLOCK_BEGIN)
}

// ---- shell startup file selection --------------------------------------------

/// Startup files for the user's shell. The bool is "safe to create when
/// missing": we create a file only when doing so cannot shadow another login
/// file. Notably we never *create* `~/.bash_profile`, because on Debian/Ubuntu
/// bash login shells read `~/.profile` and a new `.bash_profile` would silently
/// stop that — we wire `.bash_profile` only if the user already has one.
fn target_specs() -> Vec<(PathBuf, bool)> {
    let Some(home) = std::env::var("HOME").ok().filter(|h| !h.is_empty()) else {
        return Vec::new();
    };
    let home = PathBuf::from(home);
    let shell = std::env::var("SHELL").unwrap_or_default();
    let shell = Path::new(&shell)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    match shell {
        // zsh-specific files are safe to create — they shadow nothing.
        "zsh" => vec![
            (home.join(".zshrc"), true),    // interactive
            (home.join(".zprofile"), true), // login
            (home.join(".profile"), false), // sh login, only if present
        ],
        "bash" => vec![
            (home.join(".bashrc"), true),        // interactive
            (home.join(".profile"), true),       // login (Debian/Ubuntu read this)
            (home.join(".bash_profile"), false), // login if present; don't create (shadows .profile)
        ],
        _ => vec![(home.join(".profile"), true)],
    }
}

/// All candidate startup files (for read-only checks like [`rc_sourced`]).
pub fn profile_targets() -> Vec<PathBuf> {
    target_specs().into_iter().map(|(p, _)| p).collect()
}

/// Files we will actually write: those safe to create, plus any others that
/// already exist (so we wire an existing `.bash_profile`/`.profile` without
/// creating one that would shadow another login file).
fn effective_targets() -> Vec<PathBuf> {
    target_specs()
        .into_iter()
        .filter(|(path, creatable)| *creatable || path.exists())
        .map(|(path, _)| path)
        .collect()
}

// ---- mutating operations -----------------------------------------------------

fn opt_out() -> bool {
    std::env::var(NO_MODIFY_ENV)
        .map(|v| !v.is_empty() && v != "0")
        .unwrap_or(false)
}

/// Write the env file if missing or out of date. Returns whether it changed.
pub fn ensure_env_file(home: &Home) -> Result<bool> {
    let path = home.env_file();
    let desired = render_env_file(home);
    if std::fs::read_to_string(&path)
        .map(|c| c == desired)
        .unwrap_or(false)
    {
        return Ok(false);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    std::fs::write(&path, desired).with_context(|| format!("write {}", path.display()))?;
    Ok(true)
}

/// Append the managed source block to each target lacking it. Idempotent.
pub fn ensure_rc_sourcing(home: &Home, targets: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let block = rc_block(home);
    let mut modified = Vec::new();
    for target in targets {
        let existing = std::fs::read_to_string(target).unwrap_or_default();
        if has_block(&existing) {
            continue;
        }
        let mut next = existing;
        if !next.is_empty() && !next.ends_with('\n') {
            next.push('\n');
        }
        if !next.is_empty() {
            next.push('\n');
        }
        next.push_str(&block);
        if let Some(parent) = target.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        std::fs::write(target, next).with_context(|| format!("write {}", target.display()))?;
        modified.push(target.clone());
    }
    Ok(modified)
}

/// Is `link` a symlink that orgasmic owns — i.e. an `orgasmic` entry whose
/// target sits directly in our managed `bin/` dir? We canonicalize the target's
/// *parent* (a real dir) rather than the target file itself: following the file
/// would walk the `bin/orgasmic -> ../current/...` chain out of `bin/` and, on
/// macOS, also trip the `/var -> /private/var` symlink. Stays true when the link
/// is dangling, so we repair our own stale shims but never adopt a foreign one.
#[cfg(unix)]
fn shim_owned(home: &Home, link: &Path) -> bool {
    let Ok(target) = std::fs::read_link(link) else {
        return false;
    };
    let abs = if target.is_absolute() {
        target
    } else {
        match link.parent() {
            Some(parent) => parent.join(target),
            None => target,
        }
    };
    if abs.file_name() != Some(std::ffi::OsStr::new("orgasmic")) {
        return false;
    }
    let Some(parent) = abs.parent() else {
        return false;
    };
    let parent = std::fs::canonicalize(parent).unwrap_or_else(|_| parent.to_path_buf());
    let bin = std::fs::canonicalize(home.bin()).unwrap_or_else(|_| home.bin());
    parent == bin
}

/// Drop (or refresh) a convenience `orgasmic` symlink in `~/.local/bin` when
/// that dir already exists and is already on PATH — so the command resolves in
/// shells that are already open. Best-effort and idempotent: never creates the
/// dir, never clobbers a file we don't own.
#[cfg(unix)]
pub fn ensure_path_shim(home: &Home) -> Result<ShimOutcome> {
    let Some(dir) = shim_dir().filter(|d| d.is_dir() && dir_on_path(d)) else {
        return Ok(ShimOutcome::NoEligibleDir);
    };
    let link = dir.join("orgasmic");
    let target = home.bin_orgasmic();
    match std::fs::symlink_metadata(&link) {
        Err(_) => {
            crate::update::replace_symlink(&link, &target)?;
            Ok(ShimOutcome::Linked(link))
        }
        Ok(meta) => {
            // A real file, or a symlink to some *other* orgasmic: respect it.
            if !meta.file_type().is_symlink() || !shim_owned(home, &link) {
                return Ok(ShimOutcome::Blocked(link));
            }
            if std::fs::read_link(&link).ok().as_deref() == Some(target.as_path()) {
                Ok(ShimOutcome::AlreadyLinked(link))
            } else {
                crate::update::replace_symlink(&link, &target)?;
                Ok(ShimOutcome::Linked(link))
            }
        }
    }
}

#[cfg(not(unix))]
pub fn ensure_path_shim(_home: &Home) -> Result<ShimOutcome> {
    Ok(ShimOutcome::NoEligibleDir)
}

/// Ensure the env file exists and (unless `no_modify_path` / opt-out) that the
/// user's shell startup sources it and a `~/.local/bin` shim is in place.
pub fn ensure(home: &Home, no_modify_path: bool) -> Result<EnsureReport> {
    let mut report = EnsureReport {
        already_on_path: bin_on_path(home),
        env_file_written: ensure_env_file(home)?,
        ..EnsureReport::default()
    };
    if no_modify_path || opt_out() {
        report.modify_path_skipped = true;
        return Ok(report);
    }
    report.rc_files_modified = ensure_rc_sourcing(home, &effective_targets())?;
    // Best-effort: a shim hiccup must never fail the (more important) env/rc
    // wiring above.
    match ensure_path_shim(home) {
        Ok(ShimOutcome::Linked(link)) => report.shim_linked = Some(link),
        Ok(ShimOutcome::AlreadyLinked(_)) => report.shim_already = true,
        Ok(ShimOutcome::Blocked(link)) => report.shim_blocked = Some(link),
        Ok(ShimOutcome::NoEligibleDir) => {}
        Err(_) => {}
    }
    Ok(report)
}

// ---- source-checkout binary resolution ---------------------------------------

/// Locate the freshly built `orgasmic` binary in a source checkout, accounting
/// for `--target`-qualified builds that land in `target/<triple>/release/`
/// rather than `target/release/`. Picks the newest candidate so plain and
/// target-qualified builds both resolve.
pub fn resolve_source_binary(source: &Path) -> Option<PathBuf> {
    let target_dir = source.join("target");
    let mut candidates = vec![target_dir.join("release").join("orgasmic")];
    if let Ok(entries) = std::fs::read_dir(&target_dir) {
        for entry in entries.flatten() {
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                candidates.push(entry.path().join("release").join("orgasmic"));
            }
        }
    }
    candidates
        .into_iter()
        .filter(|p| p.is_file())
        .max_by_key(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok())
}

/// Repoint `bin/orgasmic` at the resolved source binary. Returns the target.
#[cfg(unix)]
pub fn relink_source_binary(home: &Home, source: &Path) -> Result<PathBuf> {
    let bin = resolve_source_binary(source).ok_or_else(|| {
        anyhow::anyhow!(
            "no built orgasmic binary under {} (looked in target/release and target/<triple>/release)",
            source.join("target").display()
        )
    })?;
    std::fs::create_dir_all(home.bin())
        .with_context(|| format!("create {}", home.bin().display()))?;
    crate::update::replace_symlink(&home.bin_orgasmic(), &bin)?;
    Ok(bin)
}

#[cfg(not(unix))]
pub fn relink_source_binary(_home: &Home, _source: &Path) -> Result<PathBuf> {
    anyhow::bail!("source binary symlink management is only implemented for unix targets")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard, OnceLock};

    /// Serializes tests that mutate process env (HOME/SHELL/PATH).
    fn env_guard() -> MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(())).lock().unwrap()
    }

    struct ScopedEnv {
        keys: Vec<(&'static str, Option<String>)>,
    }
    impl ScopedEnv {
        fn set(pairs: &[(&'static str, &str)]) -> Self {
            let keys = pairs
                .iter()
                .map(|(k, v)| {
                    let prev = std::env::var(k).ok();
                    std::env::set_var(k, v);
                    (*k, prev)
                })
                .collect();
            Self { keys }
        }
    }
    impl Drop for ScopedEnv {
        fn drop(&mut self) {
            for (k, prev) in &self.keys {
                match prev {
                    Some(v) => std::env::set_var(k, v),
                    None => std::env::remove_var(k),
                }
            }
        }
    }

    #[test]
    fn env_file_uses_home_relative_literal_and_is_idempotent() {
        let _g = env_guard();
        let tmp = tempfile::tempdir().unwrap();
        let _env = ScopedEnv::set(&[("HOME", tmp.path().to_str().unwrap())]);
        let home = Home::at(tmp.path().join(".orgasmic"));

        let body = render_env_file(&home);
        assert!(body.contains("$HOME/.orgasmic/bin"), "{body}");

        assert!(
            ensure_env_file(&home).unwrap(),
            "first write reports change"
        );
        assert!(!ensure_env_file(&home).unwrap(), "second write is a no-op");
        assert!(env_file_ok(&home));
    }

    #[test]
    fn rc_sourcing_is_idempotent_and_guarded() {
        let _g = env_guard();
        let tmp = tempfile::tempdir().unwrap();
        let _env = ScopedEnv::set(&[
            ("HOME", tmp.path().to_str().unwrap()),
            ("SHELL", "/bin/zsh"),
            (NO_MODIFY_ENV, "0"),
        ]);
        let home = Home::at(tmp.path().join(".orgasmic"));
        // Pre-create one rc file so we append rather than create-the-primary.
        let zshrc = tmp.path().join(".zshrc");
        std::fs::write(&zshrc, "# existing\n").unwrap();

        let first = ensure(&home, false).unwrap();
        assert!(first.rc_files_modified.contains(&zshrc));
        assert!(rc_sourced(&home));
        let contents = std::fs::read_to_string(&zshrc).unwrap();
        assert!(contents.starts_with("# existing\n"));
        assert_eq!(contents.matches(BLOCK_BEGIN).count(), 1);

        // Re-running adds nothing.
        let second = ensure(&home, false).unwrap();
        assert!(second.rc_files_modified.is_empty());
        assert_eq!(
            std::fs::read_to_string(&zshrc)
                .unwrap()
                .matches(BLOCK_BEGIN)
                .count(),
            1
        );
    }

    #[test]
    fn no_modify_path_writes_env_file_but_not_rc() {
        let _g = env_guard();
        let tmp = tempfile::tempdir().unwrap();
        let _env = ScopedEnv::set(&[
            ("HOME", tmp.path().to_str().unwrap()),
            ("SHELL", "/bin/zsh"),
        ]);
        let home = Home::at(tmp.path().join(".orgasmic"));
        std::fs::write(tmp.path().join(".zshrc"), "# existing\n").unwrap();

        let report = ensure(&home, true).unwrap();
        assert!(report.modify_path_skipped);
        assert!(report.rc_files_modified.is_empty());
        assert!(env_file_ok(&home));
        assert!(!rc_sourced(&home));
    }

    #[test]
    fn resolve_source_binary_prefers_target_triple_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path();
        // Only a target-triple build exists (the case that broke installs).
        let triple_bin = source.join("target/aarch64-apple-darwin/release/orgasmic");
        std::fs::create_dir_all(triple_bin.parent().unwrap()).unwrap();
        std::fs::write(&triple_bin, "bin").unwrap();

        let resolved = resolve_source_binary(source).unwrap();
        assert_eq!(resolved, triple_bin);
    }

    #[test]
    fn resolve_source_binary_finds_plain_release() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path();
        let plain = source.join("target/release/orgasmic");
        std::fs::create_dir_all(plain.parent().unwrap()).unwrap();
        std::fs::write(&plain, "bin").unwrap();

        assert_eq!(resolve_source_binary(source).unwrap(), plain);
    }

    #[test]
    fn resolve_source_binary_none_when_unbuilt() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(resolve_source_binary(tmp.path()).is_none());
    }

    /// Build a home whose `bin/orgasmic` is a real file, so the managed bin
    /// symlink and any shim pointing at it resolve under `canonicalize`.
    #[cfg(unix)]
    fn home_with_binary(tmp: &Path) -> Home {
        let home = Home::at(tmp.join(".orgasmic"));
        std::fs::create_dir_all(home.bin()).unwrap();
        std::fs::write(home.bin_orgasmic(), "bin").unwrap();
        home
    }

    #[cfg(unix)]
    #[test]
    fn ensure_links_shim_into_local_bin_on_path_and_is_idempotent() {
        let _g = env_guard();
        let tmp = tempfile::tempdir().unwrap();
        let local_bin = tmp.path().join(".local/bin");
        std::fs::create_dir_all(&local_bin).unwrap();
        let _env = ScopedEnv::set(&[
            ("HOME", tmp.path().to_str().unwrap()),
            ("SHELL", "/bin/zsh"),
            ("PATH", local_bin.to_str().unwrap()),
            (NO_MODIFY_ENV, "0"),
        ]);
        let home = home_with_binary(tmp.path());

        let report = ensure(&home, false).unwrap();
        let shim = local_bin.join("orgasmic");
        assert_eq!(report.shim_linked.as_deref(), Some(shim.as_path()));
        assert!(shim.is_symlink());
        // doctor's read-only predicate sees it resolve.
        assert_eq!(shim_on_path(&home).as_deref(), Some(shim.as_path()));

        // Re-running adds nothing new.
        let second = ensure(&home, false).unwrap();
        assert!(second.shim_linked.is_none());
        assert!(second.shim_blocked.is_none());
    }

    /// Mirrors the real install layout where `bin/orgasmic` is itself a symlink
    /// to a binary *outside* `bin/` (the `../current/...` chain). A second
    /// `ensure` must recognise its own shim and re-link nothing.
    #[cfg(unix)]
    #[test]
    fn ensure_shim_is_idempotent_with_symlinked_managed_binary() {
        let _g = env_guard();
        let tmp = tempfile::tempdir().unwrap();
        let local_bin = tmp.path().join(".local/bin");
        std::fs::create_dir_all(&local_bin).unwrap();
        let _env = ScopedEnv::set(&[
            ("HOME", tmp.path().to_str().unwrap()),
            ("SHELL", "/bin/zsh"),
            ("PATH", local_bin.to_str().unwrap()),
            (NO_MODIFY_ENV, "0"),
        ]);
        // bin/orgasmic -> a real binary living outside bin/, like the runtime chain.
        let home = Home::at(tmp.path().join(".orgasmic"));
        std::fs::create_dir_all(home.bin()).unwrap();
        let real = tmp.path().join(".orgasmic/runtimes/r/orgasmic");
        std::fs::create_dir_all(real.parent().unwrap()).unwrap();
        std::fs::write(&real, "bin").unwrap();
        std::os::unix::fs::symlink(&real, home.bin_orgasmic()).unwrap();

        let first = ensure(&home, false).unwrap();
        let shim = local_bin.join("orgasmic");
        assert_eq!(first.shim_linked.as_deref(), Some(shim.as_path()));

        let second = ensure(&home, false).unwrap();
        assert!(second.shim_linked.is_none(), "own shim must be recognised");
        assert!(second.shim_already, "own shim must report already-present");
        assert!(second.shim_blocked.is_none(), "own shim must not look foreign");
        assert_eq!(shim_on_path(&home).as_deref(), Some(shim.as_path()));
    }

    #[cfg(unix)]
    #[test]
    fn ensure_leaves_foreign_orgasmic_in_local_bin() {
        let _g = env_guard();
        let tmp = tempfile::tempdir().unwrap();
        let local_bin = tmp.path().join(".local/bin");
        std::fs::create_dir_all(&local_bin).unwrap();
        let foreign = local_bin.join("orgasmic");
        std::fs::write(&foreign, "#!/bin/sh\necho not ours\n").unwrap();
        let _env = ScopedEnv::set(&[
            ("HOME", tmp.path().to_str().unwrap()),
            ("SHELL", "/bin/zsh"),
            ("PATH", local_bin.to_str().unwrap()),
            (NO_MODIFY_ENV, "0"),
        ]);
        let home = home_with_binary(tmp.path());

        let report = ensure(&home, false).unwrap();
        assert!(report.shim_linked.is_none());
        assert_eq!(report.shim_blocked.as_deref(), Some(foreign.as_path()));
        // Untouched.
        assert!(!foreign.is_symlink());
        assert_eq!(std::fs::read_to_string(&foreign).unwrap(), "#!/bin/sh\necho not ours\n");
    }

    #[cfg(unix)]
    #[test]
    fn ensure_skips_shim_when_local_bin_not_on_path() {
        let _g = env_guard();
        let tmp = tempfile::tempdir().unwrap();
        let local_bin = tmp.path().join(".local/bin");
        std::fs::create_dir_all(&local_bin).unwrap();
        let _env = ScopedEnv::set(&[
            ("HOME", tmp.path().to_str().unwrap()),
            ("SHELL", "/bin/zsh"),
            ("PATH", "/usr/bin:/bin"), // local_bin deliberately absent
            (NO_MODIFY_ENV, "0"),
        ]);
        let home = home_with_binary(tmp.path());

        let report = ensure(&home, false).unwrap();
        assert!(report.shim_linked.is_none());
        assert!(report.shim_blocked.is_none());
        assert!(!local_bin.join("orgasmic").exists());
        assert!(shim_on_path(&home).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn no_modify_path_also_skips_shim() {
        let _g = env_guard();
        let tmp = tempfile::tempdir().unwrap();
        let local_bin = tmp.path().join(".local/bin");
        std::fs::create_dir_all(&local_bin).unwrap();
        let _env = ScopedEnv::set(&[
            ("HOME", tmp.path().to_str().unwrap()),
            ("SHELL", "/bin/zsh"),
            ("PATH", local_bin.to_str().unwrap()),
        ]);
        let home = home_with_binary(tmp.path());

        let report = ensure(&home, true).unwrap();
        assert!(report.modify_path_skipped);
        assert!(report.shim_linked.is_none());
        assert!(!local_bin.join("orgasmic").exists());
    }
}

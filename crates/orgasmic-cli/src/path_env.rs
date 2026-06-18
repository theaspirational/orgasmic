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
//! `--no-modify-path` (or `ORGASMIC_NO_MODIFY_PATH=1`) writes the env file but
//! never touches shell startup files — for CI and users who manage PATH
//! themselves.

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

/// Is the orgasmic bin dir on the *current process'* PATH?
pub fn bin_on_path(home: &Home) -> bool {
    let bin = std::fs::canonicalize(home.bin()).unwrap_or_else(|_| home.bin());
    let Ok(path) = std::env::var("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|entry| {
        let entry = std::fs::canonicalize(&entry).unwrap_or(entry);
        entry == bin
    })
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

/// Ensure the env file exists and (unless `no_modify_path` / opt-out) that the
/// user's shell startup sources it.
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
}

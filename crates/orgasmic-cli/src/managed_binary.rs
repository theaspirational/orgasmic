// orgasmic:arch_WZFAX
//! Install the executed `orgasmic` binary as a real file at a stable path.
//!
//! macOS keys filesystem permission grants (TCC) to the *resolved* executable
//! path. The historical layout executed
//! `bin/orgasmic -> ../current/bin/orgasmic -> runtimes/<version>/bin/orgasmic`,
//! so the kernel resolved a different path for every release and every upgrade
//! arrived as an unknown client with no permissions. Measured on one machine:
//! 16 granted runtime paths and 37 TCC rows between 2026-06-18 and 2026-07-25.
//!
//! Holding the path still is only half the fix. The signature's designated
//! requirement is identity-based, not byte-based:
//!
//! ```text
//! identifier "com.theaspirational.orgasmic" and certificate root = H"a705…d86d"
//! ```
//!
//! so new *content* at a fixed path inherits the grant as long as the *identity*
//! still matches. Replace the identity and the grant is gone — an ad-hoc
//! `codesign --force --sign -` on 2026-07-25 cost two System Settings approvals
//! and a runtime outage. This module therefore refuses an install whose incoming
//! designated requirement differs from the incumbent's, naming both, so a
//! mis-signed binary is a rejected install rather than a daemon without
//! permissions.
//!
//! Replacement is always by fresh inode: stage a copy beside the destination and
//! `rename` it over. Overwriting a codesigned Mach-O in place invalidates the
//! signature of the *running* image, and macOS then SIGKILLs it (gotchas).

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use orgasmic_core::home::Home;

/// Escape hatch for a deliberate identity change (re-keying the signing
/// certificate). Named in the refusal so an operator never has to guess.
const ALLOW_IDENTITY_CHANGE_ENV: &str = "ORGASMIC_ALLOW_IDENTITY_CHANGE";
/// When set, re-sign the staged binary before it is published. Contributor
/// source builds carry a linker/ad-hoc signature whose requirement will not
/// match the incumbent; signing with the pinned identity is what lets a locally
/// built runtime keep the operator's grants.
const CODESIGN_IDENTITY_ENV: &str = "ORGASMIC_CODESIGN_IDENTITY";
#[cfg(target_os = "macos")]
const CODESIGN_BUNDLE_ID_ENV: &str = "ORGASMIC_CODESIGN_BUNDLE_ID";
#[cfg(target_os = "macos")]
const DEFAULT_BUNDLE_ID: &str = "com.theaspirational.orgasmic";
/// Invoked by absolute path deliberately. This decides whether an incoming
/// binary keeps the operator's permission grants, so resolving it through `PATH`
/// would let anything earlier on `PATH` answer the question — and a shadowing
/// `codesign` that prints an agreeable requirement would silently disarm the
/// guard. `/usr/bin/codesign` is part of the OS on every macOS install.
#[cfg(target_os = "macos")]
const CODESIGN: &str = "/usr/bin/codesign";

/// What macOS would use to decide whether a binary at a given path is still
/// "the same program" as the one an operator granted permission to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodeIdentity {
    /// This platform does not key permissions on code identity, or `codesign`
    /// is unavailable. Never blocks an install.
    Unchecked,
    /// Nothing is there to read an identity from.
    Absent,
    /// The path exists but carries no code signature.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    Unsigned,
    /// The designated requirement, verbatim from `codesign -d -r-`.
    Requirement(String),
}

impl CodeIdentity {
    pub fn describe(&self) -> String {
        match self {
            Self::Unchecked => "not checked on this platform".to_string(),
            Self::Absent => "absent".to_string(),
            Self::Unsigned => "unsigned".to_string(),
            Self::Requirement(requirement) => requirement.clone(),
        }
    }

    fn requirement(&self) -> Option<&str> {
        match self {
            Self::Requirement(requirement) => Some(requirement.as_str()),
            _ => None,
        }
    }
}

/// Whether the identity guard applies. `Skip` exists for rollback, which
/// restores a binary the operator was already running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityGuard {
    Enforce,
    Skip,
}

#[derive(Debug, Clone)]
pub struct Installed {
    pub path: PathBuf,
    pub previous: CodeIdentity,
    pub current: CodeIdentity,
    /// The destination used to be a symlink, so the *executed* path just moved
    /// from a per-version path to this stable one. Existing grants were keyed to
    /// the old path and do not follow: this install needs one final approval.
    pub migrated_from_symlink: bool,
    pub resigned: bool,
}

impl Installed {
    /// True when the operator's existing grants carry over untouched.
    pub fn preserves_grants(&self) -> bool {
        !self.migrated_from_symlink
            && matches!(
                (&self.previous, &self.current),
                (CodeIdentity::Requirement(a), CodeIdentity::Requirement(b)) if a == b
            )
    }
}

/// Publish `source` as the managed binary at `$ORGASMIC_HOME/bin/orgasmic`.
#[cfg(unix)]
pub fn install(home: &Home, source: &Path, guard: IdentityGuard) -> Result<Installed> {
    use std::os::unix::fs::PermissionsExt;

    let dest = home.bin_orgasmic();
    let bin = home.bin();
    std::fs::create_dir_all(&bin).with_context(|| format!("create {}", bin.display()))?;

    // Resolve the source before anything else: when the destination is still the
    // legacy symlink it may point *at* this very file, and publishing must read
    // the real bytes rather than chase a link we are about to unlink.
    let source = std::fs::canonicalize(source)
        .with_context(|| format!("resolve incoming binary {}", source.display()))?;
    if !source.is_file() {
        bail!(
            "incoming binary is not a regular file: {}",
            source.display()
        );
    }

    let migrated_from_symlink = std::fs::symlink_metadata(&dest)
        .map(|meta| meta.file_type().is_symlink())
        .unwrap_or(false);
    let previous = read_identity(&dest);
    let incoming = read_identity(&source);
    if guard == IdentityGuard::Enforce {
        enforce_identity(&previous, &incoming, &dest, &source)?;
    }

    // Stage inside `bin/` so the publish is a same-filesystem rename, and use a
    // pid-scoped name so two concurrent installs cannot share a staging file.
    let staged = bin.join(format!(".orgasmic.incoming.{}", std::process::id()));
    let _ = std::fs::remove_file(&staged);
    let result = (|| -> Result<Installed> {
        std::fs::copy(&source, &staged)
            .with_context(|| format!("stage {} as {}", source.display(), staged.display()))?;
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))
            .with_context(|| format!("chmod {}", staged.display()))?;

        let resigned = resign_if_configured(&staged)?;
        let current = read_identity(&staged);
        if let Some(requirement) = current.requirement() {
            verify_signature(&staged)?;
            // Re-signing is the one step that can silently produce a different
            // identity than intended, so re-run the guard against its result.
            if resigned && guard == IdentityGuard::Enforce {
                enforce_identity(
                    &previous,
                    &CodeIdentity::Requirement(requirement.to_string()),
                    &dest,
                    &staged,
                )?;
            }
        }

        // Fresh inode by construction: `staged` is a new file, and rename moves
        // that inode into place. Anything already executing keeps the old inode.
        std::fs::rename(&staged, &dest)
            .with_context(|| format!("publish {} as {}", staged.display(), dest.display()))?;

        Ok(Installed {
            path: dest.clone(),
            previous,
            current,
            migrated_from_symlink,
            resigned,
        })
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&staged);
    }
    result
}

#[cfg(not(unix))]
pub fn install(_home: &Home, _source: &Path, _guard: IdentityGuard) -> Result<Installed> {
    bail!("managed binary installation is only implemented for unix targets")
}

/// Refuse an install that would change, or drop, the code identity the operator
/// granted permissions to. Both requirements are named because the remedy
/// depends on which one is wrong, and neither is visible without `codesign`.
fn enforce_identity(
    previous: &CodeIdentity,
    incoming: &CodeIdentity,
    dest: &Path,
    source: &Path,
) -> Result<()> {
    let Some(previous_requirement) = previous.requirement() else {
        // Nothing installed, unsigned incumbent, or a platform we do not check:
        // there is no grant to protect.
        return Ok(());
    };
    if incoming.requirement() == Some(previous_requirement) {
        return Ok(());
    }
    if allow_identity_change() {
        eprintln!(
            "warning: installing a binary with a different code identity because \
             ${ALLOW_IDENTITY_CHANGE_ENV} is set; macOS permission grants will not carry over"
        );
        return Ok(());
    }
    bail!(
        "refusing to install a binary with a different code identity.\n\
         \x20 installed {dest}\n\
         \x20   {previous}\n\
         \x20 incoming {source}\n\
         \x20   {incoming}\n\
         macOS keys filesystem permission grants to this requirement, so publishing \
         the incoming binary would leave the daemon without the permissions the \
         operator already approved.\n\
         Sign the incoming binary with the same identity (set ${CODESIGN_IDENTITY_ENV}), \
         or set ${ALLOW_IDENTITY_CHANGE_ENV}=1 to accept re-approving access.",
        dest = dest.display(),
        source = source.display(),
        previous = previous.describe(),
        incoming = incoming.describe(),
    )
}

fn allow_identity_change() -> bool {
    std::env::var(ALLOW_IDENTITY_CHANGE_ENV)
        .map(|raw| {
            let raw = raw.trim();
            !raw.is_empty() && raw != "0" && !raw.eq_ignore_ascii_case("false")
        })
        .unwrap_or(false)
}

/// Read the designated requirement macOS would evaluate for `path`.
#[cfg(target_os = "macos")]
pub fn read_identity(path: &Path) -> CodeIdentity {
    if !path.exists() {
        return CodeIdentity::Absent;
    }
    let output = match std::process::Command::new(CODESIGN)
        .arg("-d")
        .arg("-r-")
        .arg(path)
        .output()
    {
        Ok(output) => output,
        // No `codesign` (stripped CLT): we cannot check, so we must not block.
        Err(_) => return CodeIdentity::Unchecked,
    };
    // The requirement goes to stdout and the `Executable=` header to stderr, but
    // that split has moved between OS versions, so scan both.
    let merged = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    if let Some(requirement) = merged.lines().find_map(parse_designated_requirement) {
        return CodeIdentity::Requirement(requirement.to_string());
    }
    if merged.contains("code object is not signed") {
        return CodeIdentity::Unsigned;
    }
    CodeIdentity::Unchecked
}

/// Pull the requirement out of one `codesign -d -r-` line.
///
/// An ad-hoc signature has no explicit requirement, so codesign prints the
/// implied one *commented out*:
///
/// ```text
/// designated => identifier "com.theaspirational.orgasmic" and certificate root = H"a705…"
/// # designated => cdhash H"b35e…" or cdhash H"45d6…"
/// ```
///
/// Missing the `#` form would read an ad-hoc binary as "identity unknown" and
/// wave it through — which is exactly the signing accident this guard exists to
/// stop.
#[cfg(target_os = "macos")]
fn parse_designated_requirement(line: &str) -> Option<&str> {
    let line = line.trim();
    let line = line.strip_prefix('#').unwrap_or(line).trim_start();
    Some(line.strip_prefix("designated =>")?.trim())
}

#[cfg(not(target_os = "macos"))]
pub fn read_identity(path: &Path) -> CodeIdentity {
    if path.exists() {
        CodeIdentity::Unchecked
    } else {
        CodeIdentity::Absent
    }
}

#[cfg(target_os = "macos")]
fn resign_if_configured(path: &Path) -> Result<bool> {
    let Ok(identity) = std::env::var(CODESIGN_IDENTITY_ENV) else {
        return Ok(false);
    };
    if identity.trim().is_empty() {
        return Ok(false);
    }
    let bundle_id =
        std::env::var(CODESIGN_BUNDLE_ID_ENV).unwrap_or_else(|_| DEFAULT_BUNDLE_ID.to_string());
    let output = std::process::Command::new(CODESIGN)
        .arg("--force")
        .args(["--identifier", &bundle_id])
        .args(["--sign", identity.trim()])
        .arg(path)
        .output()
        .context("run codesign --sign")?;
    if !output.status.success() {
        bail!(
            "codesign --sign '{identity}' failed for {}: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(true)
}

#[cfg(not(target_os = "macos"))]
fn resign_if_configured(_path: &Path) -> Result<bool> {
    Ok(false)
}

#[cfg(target_os = "macos")]
fn verify_signature(path: &Path) -> Result<()> {
    let output = std::process::Command::new(CODESIGN)
        .args(["--verify", "--strict"])
        .arg(path)
        .output();
    let Ok(output) = output else {
        return Ok(());
    };
    if !output.status.success() {
        bail!(
            "staged binary fails signature verification: {}: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn verify_signature(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(all(test, unix))]
mod install_tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::fs::PermissionsExt;

    fn seed_runtime(home: &Home, version: &str, marker: &str) -> PathBuf {
        let runtime = home
            .runtimes()
            .join(format!("{version}-test"))
            .join("bin")
            .join("orgasmic");
        std::fs::create_dir_all(runtime.parent().unwrap()).unwrap();
        std::fs::write(&runtime, format!("#!/bin/sh\necho {marker}\n")).unwrap();
        std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o755)).unwrap();
        runtime
    }

    #[test]
    fn install_publishes_a_real_executable_file() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        let runtime = seed_runtime(&home, "1.0.0", "one");

        let installed = install(&home, &runtime, IdentityGuard::Enforce).unwrap();

        let meta = std::fs::symlink_metadata(&installed.path).unwrap();
        assert!(!meta.file_type().is_symlink(), "must not be a link");
        assert!(meta.is_file());
        assert_ne!(meta.permissions().mode() & 0o111, 0, "must be executable");
        assert_eq!(
            std::fs::read_to_string(&installed.path).unwrap(),
            "#!/bin/sh\necho one\n"
        );
        assert!(
            !installed.migrated_from_symlink,
            "a first install has no legacy link to migrate off"
        );
    }

    #[test]
    fn upgrading_replaces_the_binary_by_fresh_inode() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));

        let first = seed_runtime(&home, "1.0.0", "one");
        let installed = install(&home, &first, IdentityGuard::Enforce).unwrap();
        let before = std::fs::metadata(&installed.path).unwrap().ino();

        let second = seed_runtime(&home, "2.0.0", "two");
        let installed = install(&home, &second, IdentityGuard::Enforce).unwrap();
        let after = std::fs::metadata(&installed.path).unwrap().ino();

        assert_ne!(
            before, after,
            "the binary must be replaced by rename, never overwritten in place: \
             macOS SIGKILLs a running image whose file was rewritten underneath it"
        );
        assert_eq!(
            std::fs::read_to_string(&installed.path).unwrap(),
            "#!/bin/sh\necho two\n"
        );
    }

    #[test]
    fn installing_over_the_legacy_symlink_reports_the_path_change() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        let runtime = seed_runtime(&home, "1.0.0", "one");
        std::fs::create_dir_all(home.bin()).unwrap();
        std::os::unix::fs::symlink(&runtime, home.bin_orgasmic()).unwrap();

        let installed = install(&home, &home.bin_orgasmic(), IdentityGuard::Enforce).unwrap();

        assert!(
            installed.migrated_from_symlink,
            "the executed path just moved off the per-version path; the operator \
             needs to know this install still costs one approval"
        );
        assert!(!installed.preserves_grants());
        // Resolving the link before publishing is what makes this safe: the
        // source and destination were the same path.
        assert_eq!(
            std::fs::read_to_string(&installed.path).unwrap(),
            "#!/bin/sh\necho one\n"
        );
        assert!(runtime.is_file(), "the runtime payload must survive");
        assert!(!std::fs::symlink_metadata(&installed.path)
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[test]
    fn a_failed_install_leaves_no_staging_file_behind() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        seed_runtime(&home, "1.0.0", "one");

        install(&home, &home.root.join("nope"), IdentityGuard::Enforce)
            .expect_err("a missing source cannot be published");

        let leftovers: Vec<_> = std::fs::read_dir(home.bin())
            .map(|entries| {
                entries
                    .flatten()
                    .map(|entry| entry.file_name())
                    .filter(|name| name.to_string_lossy().starts_with(".orgasmic.incoming"))
                    .collect()
            })
            .unwrap_or_default();
        assert!(leftovers.is_empty(), "staging files leaked: {leftovers:?}");
    }
}

/// Exercises the guard against real `codesign` output rather than hand-written
/// strings. Hermetic: ad-hoc signing needs no identity, and an ad-hoc signature
/// is the shape that caused the 2026-07-25 outage.
#[cfg(all(test, target_os = "macos"))]
mod codesign_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn adhoc_sign(path: &Path) {
        let status = std::process::Command::new(CODESIGN)
            .args(["--force", "--sign", "-"])
            .arg(path)
            .status()
            .expect("run codesign");
        assert!(status.success(), "ad-hoc sign {}", path.display());
    }

    fn seed(dir: &Path, name: &str) -> PathBuf {
        // A Mach-O, since codesign will not sign a shell script.
        let path = dir.join(name);
        std::fs::copy("/bin/echo", &path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[test]
    fn an_ad_hoc_signature_reads_as_a_requirement_not_as_unknown() {
        let tmp = tempfile::tempdir().unwrap();
        let binary = seed(tmp.path(), "adhoc");
        adhoc_sign(&binary);

        match read_identity(&binary) {
            CodeIdentity::Requirement(requirement) => {
                assert!(
                    requirement.contains("cdhash"),
                    "ad-hoc requirements are cdhash-based, got {requirement}"
                );
            }
            other => panic!(
                "an ad-hoc signature must read as a requirement, not {other:?} — \
                 reading it as unknown would wave through the signing accident \
                 this guard exists to catch"
            ),
        }
    }

    #[test]
    fn an_unsigned_binary_reads_as_unsigned() {
        let tmp = tempfile::tempdir().unwrap();
        let binary = seed(tmp.path(), "bare");
        let _ = std::process::Command::new(CODESIGN)
            .arg("--remove-signature")
            .arg(&binary)
            .status();
        assert_eq!(read_identity(&binary), CodeIdentity::Unsigned);
    }

    /// Measurement against the operator's actually-installed runtime, which is
    /// the only place the *pinned* certificate-root requirement exists — CI has
    /// no signing identity, so the hermetic tests above can only produce ad-hoc
    /// signatures. Point `$ORGASMIC_REAL_BINARY` at an installed runtime binary.
    #[test]
    #[ignore = "requires an installed, identity-signed runtime binary"]
    fn real_installed_binary_reports_the_pinned_requirement() {
        let Ok(path) = std::env::var("ORGASMIC_REAL_BINARY") else {
            panic!("set ORGASMIC_REAL_BINARY to an installed runtime binary");
        };
        let path = PathBuf::from(path);
        let identity = read_identity(&path);
        let CodeIdentity::Requirement(requirement) = &identity else {
            panic!(
                "{} has no designated requirement: {identity:?}",
                path.display()
            );
        };
        println!("{} => {requirement}", path.display());
        assert!(
            requirement.contains("certificate root"),
            "a release binary must be identity-signed, not ad-hoc: {requirement}"
        );

        // The whole premise of the fix: same identity, so a content change at a
        // stable path keeps the operator's grants.
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        let installed = install(&home, &path, IdentityGuard::Enforce).unwrap();
        assert_eq!(
            installed.current, identity,
            "copying must preserve the signature"
        );
        let again = install(&home, &path, IdentityGuard::Enforce).unwrap();
        assert!(
            again.preserves_grants(),
            "a second install of the same identity must report grants as preserved"
        );
    }

    #[test]
    fn re_signing_the_incumbent_is_refused_end_to_end() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        std::fs::create_dir_all(home.bin()).unwrap();

        // Two independently ad-hoc-signed copies have different cdhashes, so
        // they stand in for "signed by a different identity".
        let first = seed(tmp.path(), "first");
        adhoc_sign(&first);
        install(&home, &first, IdentityGuard::Enforce).expect("first install");

        let second = seed(tmp.path(), "second");
        std::fs::write(&second, std::fs::read("/bin/ls").unwrap()).unwrap();
        std::fs::set_permissions(&second, std::fs::Permissions::from_mode(0o755)).unwrap();
        adhoc_sign(&second);

        let error = install(&home, &second, IdentityGuard::Enforce)
            .expect_err("a different code identity must be refused");
        let message = error.to_string();
        assert!(message.contains("different code identity"), "{message}");
        assert!(message.contains(&home.bin_orgasmic().display().to_string()));
        assert!(message.contains(&second.display().to_string()));

        // The refusal must be total: the incumbent the operator granted access
        // to is still in place, byte for byte.
        assert_eq!(
            std::fs::read(home.bin_orgasmic()).unwrap(),
            std::fs::read(&first).unwrap(),
            "a refused install must not have modified the incumbent"
        );

        // Rollback restores a known-good binary and must never be blocked.
        install(&home, &second, IdentityGuard::Skip).expect("rollback bypasses the guard");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn requirement(value: &str) -> CodeIdentity {
        CodeIdentity::Requirement(value.to_string())
    }

    #[test]
    fn matching_requirements_are_accepted() {
        let identity = requirement("identifier \"x\" and certificate root = H\"ab\"");
        enforce_identity(
            &identity,
            &identity,
            Path::new("/dest"),
            Path::new("/source"),
        )
        .expect("same identity installs");
    }

    #[test]
    fn a_different_requirement_is_refused_naming_both() {
        let error = enforce_identity(
            &requirement("identifier \"x\" and certificate root = H\"ab\""),
            &requirement("cdhash H\"cd\""),
            Path::new("/dest"),
            Path::new("/source"),
        )
        .expect_err("differing identity is refused");
        let message = error.to_string();
        assert!(message.contains("certificate root = H\"ab\""), "{message}");
        assert!(message.contains("cdhash H\"cd\""), "{message}");
        assert!(message.contains(ALLOW_IDENTITY_CHANGE_ENV), "{message}");
    }

    #[test]
    fn dropping_the_signature_is_refused() {
        enforce_identity(
            &requirement("identifier \"x\" and certificate root = H\"ab\""),
            &CodeIdentity::Unsigned,
            Path::new("/dest"),
            Path::new("/source"),
        )
        .expect_err("an unsigned replacement loses the grant and is refused");
    }

    #[test]
    fn a_first_install_has_no_grant_to_protect() {
        for previous in [
            CodeIdentity::Absent,
            CodeIdentity::Unsigned,
            CodeIdentity::Unchecked,
        ] {
            enforce_identity(
                &previous,
                &requirement("cdhash H\"cd\""),
                Path::new("/dest"),
                Path::new("/source"),
            )
            .unwrap_or_else(|error| panic!("{previous:?} should install: {error}"));
        }
    }

    #[test]
    fn preserves_grants_requires_a_stable_path_and_identity() {
        let identity = requirement("identifier \"x\"");
        let stable = Installed {
            path: PathBuf::from("/bin/orgasmic"),
            previous: identity.clone(),
            current: identity.clone(),
            migrated_from_symlink: false,
            resigned: false,
        };
        assert!(stable.preserves_grants());
        assert!(
            !Installed {
                migrated_from_symlink: true,
                ..stable.clone()
            }
            .preserves_grants(),
            "moving off the legacy symlink changes the executed path"
        );
        assert!(!Installed {
            current: requirement("cdhash H\"cd\""),
            ..stable
        }
        .preserves_grants());
    }
}

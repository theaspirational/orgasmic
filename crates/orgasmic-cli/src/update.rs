// arch: arch_WZFAX.2
// orgasmic:arch_WZFAX
//! `orgasmic update`: update either a managed runtime bundle or an explicit
//! contributor source checkout, depending on `$ORGASMIC_HOME/install.json`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::daemon_client::DaemonClient;
use crate::daemon_lifecycle::{self, LocalDaemonState};
use crate::daemon_runtime;
use crate::home::Home;
use crate::install_state::{self, InstallMode, InstallState};

const DEFAULT_RELEASE_REPO: &str = "theaspirational/orgasmic";
const REQUIRED_RUNTIME_FILES: &[&str] = &[
    "bin/orgasmic",
    "runtime-manifest.json",
    "docs/README.md",
    "shipped/schema/tx.org",
    "shipped/prompt-studio/slots.org",
    "shipped/entry/router.org",
    "shipped/skills/orgasmic/SKILL.md",
];

// dec_B4147 retention amendment: after a successful runtime swap, keep the
// active runtime plus this many previous installs for the SAME target (most
// recently installed first) and reclaim the rest. Release asset filenames are
// version-less now, but each version still unpacks into its own
// `runtimes/{version}-{target}` dir, so without this they accumulate forever.
// One previous install is kept so a manual rollback still has a target.
// Override with $ORGASMIC_RUNTIME_RETENTION.
const DEFAULT_RUNTIME_RETENTION: usize = 1;

#[derive(Debug, Clone, Deserialize)]
struct RuntimeChannelManifest {
    version: String,
    #[serde(default)]
    channel: Option<String>,
    runtimes: BTreeMap<String, RuntimeAsset>,
}

#[derive(Debug, Clone, Deserialize)]
struct RuntimeAsset {
    url: String,
    sha256: String,
    // orgasmic:dec_B4147 — per-target version; a lagging target (e.g. windows,
    // refreshed by a separate CI dispatch) advertises its own version rather than
    // the manifest's top-level one. Falls back to the top-level version when absent.
    #[serde(default)]
    version: Option<String>,
}

pub fn run(home: &Home, branch: &str, do_build: bool, channel: Option<String>) -> Result<()> {
    match install_state::read(home)? {
        Some(state) if state.mode == InstallMode::Bundle => {
            run_bundle(home, state, branch, do_build, channel)
        }
        Some(state) if state.mode == InstallMode::Source => {
            if channel.is_some() {
                bail!("--channel only applies to a prebuilt-bundle install, not a source checkout");
            }
            let source = state.source_checkout.unwrap_or_else(|| home.source());
            run_source(home, &source, branch, do_build)
        }
        _ => {
            if channel.is_some() {
                bail!("--channel only applies to a prebuilt-bundle install, not a source checkout");
            }
            run_source(home, &home.source(), branch, do_build)
        }
    }
}

fn run_bundle(
    home: &Home,
    state: InstallState,
    branch: &str,
    do_build: bool,
    channel_override: Option<String>,
) -> Result<()> {
    if branch != "main" {
        bail!("--branch is only supported for contributor source installs");
    }
    if !do_build {
        bail!("--no-build is only supported for contributor source installs");
    }

    home.ensure().context("prepare ORGASMIC_HOME")?;
    let target = state.target.clone().unwrap_or_else(current_target_key);
    // An explicit --channel switches feeds. The stored manifest_url points at the
    // OLD channel, so on a switch we recompute it from the channel and skip the
    // up-to-date short-circuit so the new channel pin is always persisted. The
    // installer is equality-based (it installs the channel head regardless of
    // whether its semver is higher or lower), which is exactly what a deliberate
    // channel switch needs. dec_B4147 versioning amendment.
    let switching = channel_override
        .as_deref()
        .is_some_and(|c| Some(c) != state.channel.as_deref());
    let channel = channel_override
        .or_else(|| state.channel.clone())
        .unwrap_or_else(|| "stable".to_string());
    let manifest_url = if switching {
        default_manifest_url(&channel)
    } else {
        state
            .manifest_url
            .clone()
            .unwrap_or_else(|| default_manifest_url(&channel))
    };
    if switching {
        println!("→ switching runtime channel → {channel}");
    }
    println!("→ checking {channel} runtime manifest: {manifest_url}");

    let manifest_raw = fetch_text(&manifest_url)?;
    let manifest: RuntimeChannelManifest =
        serde_json::from_str(&manifest_raw).context("parse runtime channel manifest")?;
    let asset = manifest
        .runtimes
        .get(&target)
        .with_context(|| format!("manifest has no runtime for target {target}"))?;
    let asset_version = asset
        .version
        .clone()
        .unwrap_or_else(|| manifest.version.clone());

    // Already up to date: the channel advertises the version we already have
    // installed and active, and the daemon is on that managed runtime (no
    // temporary `--from-source` override to clear). Skip the re-download/swap.
    if !switching
        && state.version.as_deref() == Some(asset_version.as_str())
        && runtime_is_active(home, &asset_version, &target)
        && daemon_runtime::read(home)?.is_none()
    {
        println!("✓ already up to date: {asset_version} ({target}) on channel {channel}");
        return Ok(());
    }

    let asset_url = resolve_asset_url(&manifest_url, &asset.url);
    println!("→ downloading runtime {asset_version} for {target}");

    let work = prepare_work_dir(home, "runtime-update")?;
    let result = (|| {
        let bundle = work.join("runtime.tar.gz");
        let bytes = fetch_bytes(&asset_url)?;
        let actual_sha = sha256_hex(&bytes);
        if !actual_sha.eq_ignore_ascii_case(asset.sha256.trim()) {
            bail!(
                "runtime checksum mismatch for {asset_url}\n  expected {}\n  actual   {actual_sha}",
                asset.sha256
            );
        }
        std::fs::write(&bundle, &bytes).with_context(|| format!("write {}", bundle.display()))?;

        let was_running = preflight_daemon(home)?;
        let final_runtime = install_bundle_payload(home, &bundle, &asset_version, &target)?;
        let old_state = install_state::read(home)?;
        let previous_current = read_symlink(&home.current_runtime());
        let new_state = InstallState {
            mode: InstallMode::Bundle,
            channel: manifest.channel.clone().or(Some(channel)),
            version: Some(asset_version.clone()),
            target: Some(target.clone()),
            manifest_url: Some(manifest_url.clone()),
            runtime_dir: Some(final_runtime.clone()),
            source_checkout: None,
        };

        swap_runtime_links(home, &final_runtime)?;
        install_state::write(home, &new_state)?;
        let _ = crate::path_env::ensure_env_file(home);

        if let Err(error) = refresh_agent_skill(home) {
            rollback_bundle_swap(home, previous_current.as_deref(), old_state.as_ref())?;
            return Err(error);
        }

        let cleared_override = daemon_runtime::clear(home)?;

        if was_running {
            if let Err(error) = daemon_lifecycle::restart_with_force(home, false) {
                rollback_bundle_swap(home, previous_current.as_deref(), old_state.as_ref())?;
                let _ = daemon_lifecycle::start(home);
                bail!(
                    "daemon restart failed after runtime swap; rolled back runtime links: {error}"
                );
            }
        }

        println!(
            "✓ runtime updated to {} ({target}) at {}",
            asset_version,
            final_runtime.display()
        );
        if cleared_override {
            println!("  cleared daemon runtime override; future starts use the updated runtime");
        }

        // Retention: keep the active runtime + N previous installs for this
        // target; reclaim older ones. Best-effort — the swap already succeeded,
        // so a prune failure must not fail the update. dec_B4147.
        let retention = runtime_retention();
        match prune_old_runtimes(home, &final_runtime, &target, retention) {
            Ok(removed) if !removed.is_empty() => {
                println!(
                    "  pruned {} old runtime(s): {}",
                    removed.len(),
                    removed.join(", ")
                );
            }
            Ok(_) => {}
            Err(error) => eprintln!("warning: runtime prune skipped: {error}"),
        }
        println!(
            "  kept current + {retention} previous under {}",
            home.runtimes().display()
        );
        Ok(())
    })();
    let _ = std::fs::remove_dir_all(&work);
    result
}

/// True when `version` for `target` is already installed and active — its
/// runtime directory validates and `current` points at it — so an update would
/// have nothing to do. Callers additionally require no temporary daemon runtime
/// override before treating this as up to date.
fn runtime_is_active(home: &Home, version: &str, target: &str) -> bool {
    let runtime_name = format!("{version}-{target}");
    if validate_runtime_dir(&home.runtimes().join(&runtime_name)).is_err() {
        return false;
    }
    read_symlink(&home.current_runtime())
        .map(|link| link == Path::new("runtimes").join(&runtime_name))
        .unwrap_or(false)
}

/// How many previous runtimes to retain for the active target, in addition to
/// the current one. `$ORGASMIC_RUNTIME_RETENTION` overrides the default.
fn runtime_retention() -> usize {
    std::env::var("ORGASMIC_RUNTIME_RETENTION")
        .ok()
        .and_then(|raw| raw.trim().parse::<usize>().ok())
        .unwrap_or(DEFAULT_RUNTIME_RETENTION)
}

/// Reclaim old runtime installs. Keeps `keep_current` plus the `retention`
/// most-recently-installed other runtimes for `target` (ordered by directory
/// mtime, i.e. install time, newest first) and removes the rest. Only touches
/// `{version}-{target}` dirs for the active target; other targets are left
/// alone. Returns the names removed. A per-dir removal failure is logged and
/// skipped rather than aborting — callers treat this as best-effort cleanup.
fn prune_old_runtimes(
    home: &Home,
    keep_current: &Path,
    target: &str,
    retention: usize,
) -> Result<Vec<String>> {
    let runtimes = home.runtimes();
    let suffix = format!("-{target}");
    let current_name = keep_current.file_name().and_then(|name| name.to_str());

    let mut candidates: Vec<(PathBuf, String, std::time::SystemTime)> = Vec::new();
    for entry in
        std::fs::read_dir(&runtimes).with_context(|| format!("read {}", runtimes.display()))?
    {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.ends_with(&suffix) {
            continue; // a different target — not ours to manage
        }
        if Some(name) == current_name {
            continue; // never remove the active runtime
        }
        let mtime = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        candidates.push((entry.path(), name.to_string(), mtime));
    }

    // Newest install first; keep the first `retention`, remove the remainder.
    candidates.sort_by(|a, b| b.2.cmp(&a.2));
    let mut removed = Vec::new();
    for (path, name, _) in candidates.into_iter().skip(retention) {
        match std::fs::remove_dir_all(&path) {
            Ok(()) => removed.push(name),
            Err(error) => {
                eprintln!(
                    "warning: failed to prune old runtime {}: {error}",
                    path.display()
                );
            }
        }
    }
    Ok(removed)
}

fn preflight_daemon(home: &Home) -> Result<bool> {
    match daemon_lifecycle::status(home)? {
        LocalDaemonState::Running(_) => {
            refuse_if_live_runs(home)?;
            Ok(true)
        }
        LocalDaemonState::Starting(starting) => {
            bail!(
                "daemon is still starting (pid {}); retry update after it is ready",
                starting.pid
            )
        }
        LocalDaemonState::Unauthorized => {
            bail!("daemon auth token mismatch (check $ORGASMIC_HOME/user/auth/token)")
        }
        LocalDaemonState::Down => Ok(false),
    }
}

fn refuse_if_live_runs(home: &Home) -> Result<()> {
    let runtime = tokio::runtime::Runtime::new().context("create tokio runtime")?;
    let live = runtime.block_on(async {
        let body: serde_json::Value = DaemonClient::from_home(home)?.get("/runs").await?;
        let live = body
            .get("live")
            .and_then(|live| live.as_array())
            .map(|runs| {
                runs.iter()
                    .filter_map(|run| {
                        run.get("run_id")
                            .or_else(|| run.get("id"))
                            .and_then(|id| id.as_str())
                            .map(str::to_string)
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        Ok::<_, anyhow::Error>(live)
    })?;
    if live.is_empty() {
        return Ok(());
    }
    bail!(
        "refusing runtime update while live run(s) exist: {}. Close them or use the existing force path deliberately.",
        live.join(", ")
    )
}

fn install_bundle_payload(
    home: &Home,
    bundle: &Path,
    version: &str,
    target: &str,
) -> Result<PathBuf> {
    let runtimes = home.runtimes();
    std::fs::create_dir_all(&runtimes).with_context(|| format!("create {}", runtimes.display()))?;
    let final_runtime = runtimes.join(format!("{version}-{target}"));
    if final_runtime.exists() {
        validate_runtime_dir(&final_runtime)?;
        return Ok(final_runtime);
    }

    let tmp_runtime = runtimes.join(format!("{version}-{target}.tmp"));
    let _ = std::fs::remove_dir_all(&tmp_runtime);
    std::fs::create_dir_all(&tmp_runtime)
        .with_context(|| format!("create {}", tmp_runtime.display()))?;

    unpack_tar_gz(bundle, &tmp_runtime)?;
    let payload = normalize_payload_dir(&tmp_runtime)?;
    validate_runtime_dir(&payload)?;

    if payload == tmp_runtime {
        std::fs::rename(&tmp_runtime, &final_runtime).with_context(|| {
            format!(
                "move staged runtime {} to {}",
                tmp_runtime.display(),
                final_runtime.display()
            )
        })?;
    } else {
        std::fs::rename(&payload, &final_runtime).with_context(|| {
            format!(
                "move staged runtime {} to {}",
                payload.display(),
                final_runtime.display()
            )
        })?;
        let _ = std::fs::remove_dir_all(&tmp_runtime);
    }
    Ok(final_runtime)
}

fn validate_runtime_dir(dir: &Path) -> Result<()> {
    for rel in REQUIRED_RUNTIME_FILES {
        let path = dir.join(rel);
        if !path.is_file() {
            bail!("runtime bundle missing required file: {}", path.display());
        }
    }
    validate_executable(&dir.join("bin/orgasmic"))?;
    Ok(())
}

#[cfg(unix)]
fn validate_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(path)
        .with_context(|| format!("stat {}", path.display()))?
        .permissions()
        .mode();
    if mode & 0o111 == 0 {
        bail!("runtime binary is not executable: {}", path.display());
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_executable(path: &Path) -> Result<()> {
    if !path.is_file() {
        bail!("runtime binary missing: {}", path.display());
    }
    Ok(())
}

fn unpack_tar_gz(bundle: &Path, dest: &Path) -> Result<()> {
    let mut command = Command::new("tar");
    if tar_supports_unknown_pax_warning_suppression() {
        command.arg("--warning=no-unknown-keyword");
    }
    let status = command
        .arg("-xzf")
        .arg(bundle)
        .arg("-C")
        .arg(dest)
        .status()
        .with_context(|| format!("unpack {}", bundle.display()))?;
    if !status.success() {
        bail!(
            "tar unpack failed for {} (exit {:?})",
            bundle.display(),
            status.code()
        );
    }
    Ok(())
}

fn tar_supports_unknown_pax_warning_suppression() -> bool {
    Command::new("tar")
        .arg("--warning=no-unknown-keyword")
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn normalize_payload_dir(extract_root: &Path) -> Result<PathBuf> {
    if extract_root.join("bin/orgasmic").is_file() {
        return Ok(extract_root.to_path_buf());
    }
    let mut dirs = Vec::new();
    for entry in std::fs::read_dir(extract_root)
        .with_context(|| format!("read {}", extract_root.display()))?
    {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            dirs.push(entry.path());
        }
    }
    if dirs.len() == 1 && dirs[0].join("bin/orgasmic").is_file() {
        return Ok(dirs.remove(0));
    }
    bail!(
        "runtime bundle did not unpack to a directory containing bin/orgasmic under {}",
        extract_root.display()
    )
}

fn swap_runtime_links(home: &Home, runtime: &Path) -> Result<()> {
    let runtime_name = runtime
        .file_name()
        .and_then(|name| name.to_str())
        .context("runtime directory has no valid name")?;
    replace_symlink(
        &home.current_runtime(),
        Path::new("runtimes").join(runtime_name),
    )?;
    replace_symlink(&home.source(), Path::new("current"))?;
    std::fs::create_dir_all(home.bin())
        .with_context(|| format!("create {}", home.bin().display()))?;
    replace_symlink(
        &home.bin_orgasmic(),
        Path::new("..").join("current").join("bin").join("orgasmic"),
    )?;
    Ok(())
}

fn rollback_bundle_swap(
    home: &Home,
    previous_current: Option<&Path>,
    previous_state: Option<&InstallState>,
) -> Result<()> {
    if let Some(previous_current) = previous_current {
        replace_symlink(&home.current_runtime(), previous_current)?;
        replace_symlink(&home.source(), Path::new("current"))?;
        replace_symlink(
            &home.bin_orgasmic(),
            Path::new("..").join("current").join("bin").join("orgasmic"),
        )?;
        let _ = refresh_agent_skill(home);
    }
    if let Some(previous_state) = previous_state {
        install_state::write(home, previous_state)?;
    }
    Ok(())
}

fn refresh_agent_skill(home: &Home) -> Result<()> {
    let src = home.current_runtime().join("shipped/skills/orgasmic");
    if !src.join("SKILL.md").is_file() {
        bail!("runtime missing shipped skill: {}", src.display());
    }
    let skills_dir = agent_skills_dir()?;
    std::fs::create_dir_all(&skills_dir)
        .with_context(|| format!("create {}", skills_dir.display()))?;
    let dest = skills_dir.join("orgasmic");
    if let Ok(meta) = std::fs::symlink_metadata(&dest) {
        if meta.file_type().is_symlink() {
            // Atomic symlink replacement below will handle managed symlinks.
        } else if meta.is_file() {
            bail!(
                "{} exists as a file; refusing to replace it with the orgasmic skill symlink",
                dest.display()
            );
        } else if is_orgasmic_skill_copy(&dest) {
            let backup = dest.with_extension(format!("bak-{}", timestamp_secs()));
            std::fs::rename(&dest, &backup).with_context(|| {
                format!(
                    "move stale skill copy {} to {}",
                    dest.display(),
                    backup.display()
                )
            })?;
        } else {
            bail!(
                "{} exists and is not an orgasmic skill symlink/copy; refusing to replace it",
                dest.display()
            );
        }
    }
    replace_symlink(&dest, &src)?;
    Ok(())
}

fn is_orgasmic_skill_copy(path: &Path) -> bool {
    let Ok(raw) = std::fs::read_to_string(path.join("SKILL.md")) else {
        return false;
    };
    raw.lines()
        .take(8)
        .any(|line| line.trim() == "name: orgasmic")
}

fn agent_skills_dir() -> Result<PathBuf> {
    if let Ok(path) = std::env::var("AGENT_SKILLS_DIR") {
        if !path.is_empty() {
            return Ok(PathBuf::from(path));
        }
    }
    let home = std::env::var("HOME").context("$HOME is not set; cannot locate ~/.agents/skills")?;
    Ok(PathBuf::from(home).join(".agents/skills"))
}

#[cfg(unix)]
pub(crate) fn replace_symlink(link: &Path, target: impl AsRef<Path>) -> Result<()> {
    if let Some(parent) = link.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    if let Ok(meta) = std::fs::symlink_metadata(link) {
        if meta.is_dir() && !meta.file_type().is_symlink() {
            bail!(
                "{} is a real directory; refusing to replace it with a runtime symlink",
                link.display()
            );
        }
    }
    let tmp = link.with_file_name(format!(
        ".{}.tmp-{}",
        link.file_name().and_then(|n| n.to_str()).unwrap_or("link"),
        timestamp_secs()
    ));
    let _ = std::fs::remove_file(&tmp);
    std::os::unix::fs::symlink(target.as_ref(), &tmp).with_context(|| {
        format!(
            "create symlink {} -> {}",
            tmp.display(),
            target.as_ref().display()
        )
    })?;
    std::fs::rename(&tmp, link).with_context(|| {
        format!(
            "replace symlink {} -> {}",
            link.display(),
            target.as_ref().display()
        )
    })?;
    Ok(())
}

#[cfg(not(unix))]
pub(crate) fn replace_symlink(_link: &Path, _target: impl AsRef<Path>) -> Result<()> {
    bail!("runtime symlink management is only implemented for unix targets")
}

fn read_symlink(path: &Path) -> Option<PathBuf> {
    std::fs::read_link(path).ok()
}

fn prepare_work_dir(home: &Home, label: &str) -> Result<PathBuf> {
    let work = home.root.join(format!(
        ".{label}-{}-{}",
        std::process::id(),
        timestamp_secs()
    ));
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).with_context(|| format!("create {}", work.display()))?;
    Ok(work)
}

fn timestamp_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn fetch_text(location: &str) -> Result<String> {
    if let Some(path) = local_path_from_location(location) {
        return std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()));
    }
    let runtime = tokio::runtime::Runtime::new().context("create tokio runtime")?;
    runtime.block_on(async {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .context("build http client")?
            .get(location)
            .send()
            .await
            .with_context(|| format!("fetch {location}"))?
            .error_for_status()
            .with_context(|| format!("fetch {location}"))?
            .text()
            .await
            .with_context(|| format!("read {location} body"))
    })
}

fn fetch_bytes(location: &str) -> Result<Vec<u8>> {
    if let Some(path) = local_path_from_location(location) {
        return std::fs::read(&path).with_context(|| format!("read {}", path.display()));
    }
    let runtime = tokio::runtime::Runtime::new().context("create tokio runtime")?;
    runtime.block_on(async {
        let bytes = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .build()
            .context("build http client")?
            .get(location)
            .send()
            .await
            .with_context(|| format!("download {location}"))?
            .error_for_status()
            .with_context(|| format!("download {location}"))?
            .bytes()
            .await
            .with_context(|| format!("read {location} body"))?;
        Ok(bytes.to_vec())
    })
}

fn local_path_from_location(location: &str) -> Option<PathBuf> {
    if let Some(path) = location.strip_prefix("file://") {
        return Some(PathBuf::from(path));
    }
    if location.contains("://") {
        return None;
    }
    Some(PathBuf::from(location))
}

fn resolve_asset_url(manifest_url: &str, asset_url: &str) -> String {
    if asset_url.contains("://") || Path::new(asset_url).is_absolute() {
        return asset_url.to_string();
    }
    if let Some(manifest_path) = local_path_from_location(manifest_url) {
        if let Some(parent) = manifest_path.parent() {
            return parent.join(asset_url).to_string_lossy().to_string();
        }
    }
    if let Some((base, _)) = manifest_url.rsplit_once('/') {
        return format!("{base}/{asset_url}");
    }
    asset_url.to_string()
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn default_manifest_url(channel: &str) -> String {
    format!(
        "https://github.com/{DEFAULT_RELEASE_REPO}/releases/download/{channel}/runtime-latest.json"
    )
}

fn current_target_key() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH).replace("macos", "darwin")
}

fn run_source(home: &Home, source: &Path, branch: &str, do_build: bool) -> Result<()> {
    if !source.join(".git").exists() {
        bail!(
            "no contributor source checkout at {} — run scripts/install.sh --from-source <checkout>",
            source.display()
        );
    }
    let stash_label = format!("orgasmic-update-{}", timestamp_secs());
    let stashed = stash_if_dirty(source, &stash_label)?;

    let result: Result<()> = (|| {
        git(source, &["fetch", "origin", branch])?;
        git(source, &["checkout", branch])?;
        git(source, &["pull", "--ff-only", "origin", branch])?;
        if do_build {
            cargo_build(source)?;
            refresh_source_symlink(home, source)?;
        }
        install_state::write(home, &InstallState::source(source.to_path_buf()))?;
        let _ = crate::path_env::ensure_env_file(home);
        Ok(())
    })();

    if stashed {
        // Always try to restore; surface a clear pointer to stash if it fails.
        if let Err(e) = git(source, &["stash", "pop"]) {
            eprintln!(
                "warning: stash pop failed after update: {e}. Recover with:\n  (cd {} && git stash list && git stash pop)",
                source.display()
            );
        }
    }

    result
}

fn stash_if_dirty(source: &Path, label: &str) -> Result<bool> {
    let out = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(source)
        .output()
        .with_context(|| format!("git status in {}", source.display()))?;
    if !out.status.success() {
        bail!(
            "git status failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    if out.stdout.is_empty() {
        return Ok(false);
    }
    git(
        source,
        &["stash", "push", "--include-untracked", "--message", label],
    )?;
    Ok(true)
}

fn git(source: &Path, args: &[&str]) -> Result<()> {
    let status = Command::new("git")
        .args(args)
        .current_dir(source)
        .status()
        .with_context(|| format!("git {} in {}", args.join(" "), source.display()))?;
    if !status.success() {
        bail!("git {} failed (exit {:?})", args.join(" "), status.code());
    }
    Ok(())
}

fn cargo_build(source: &Path) -> Result<()> {
    let status = Command::new("cargo")
        .args(["build", "--release"])
        .current_dir(source)
        .status()
        .with_context(|| format!("cargo build in {}", source.display()))?;
    if !status.success() {
        bail!("cargo build --release failed (exit {:?})", status.code());
    }
    Ok(())
}

#[cfg(unix)]
fn refresh_source_symlink(home: &Home, source: &Path) -> Result<()> {
    // Resolve across `target/release` and `target/<triple>/release` so
    // `--target`-qualified builds relink correctly instead of dangling.
    crate::path_env::relink_source_binary(home, source)?;
    Ok(())
}

#[cfg(not(unix))]
fn refresh_source_symlink(_home: &Home, _source: &Path) -> Result<()> {
    bail!("symlink refresh is only implemented for unix targets")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};

    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(())).lock().unwrap()
    }

    fn write(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, contents).unwrap();
    }

    #[cfg(unix)]
    fn make_executable(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(path, permissions).unwrap();
    }

    fn write_runtime_payload(root: &Path, marker: &str) {
        write(
            &root.join("bin/orgasmic"),
            &format!("#!/bin/sh\necho {marker}\n"),
        );
        make_executable(&root.join("bin/orgasmic"));
        write(
            &root.join("runtime-manifest.json"),
            "{\"version\":\"test\"}\n",
        );
        write(&root.join("docs/README.md"), "# runtime docs\n");
        write(&root.join("shipped/schema/tx.org"), "* Tx\n");
        write(&root.join("shipped/prompt-studio/slots.org"), "* Slots\n");
        write(&root.join("shipped/entry/router.org"), "* Entry\n");
        write(
            &root.join("shipped/skills/orgasmic/SKILL.md"),
            "---\nname: orgasmic\n---\n",
        );
    }

    fn tar_runtime(payload: &Path, out: &Path) {
        let status = Command::new("tar")
            .arg("-czf")
            .arg(out)
            .arg("-C")
            .arg(payload)
            .arg(".")
            .status()
            .unwrap();
        assert!(status.success());
    }

    fn bundle_manifest(version: &str, bundle: &Path, sha: &str) -> String {
        serde_json::json!({
            "version": version,
            "channel": "nightly",
            "runtimes": {
                "darwin-aarch64": {
                    "url": bundle.to_string_lossy(),
                    "sha256": sha,
                }
            }
        })
        .to_string()
    }

    #[test]
    fn bundle_update_swaps_runtime_and_preserves_user_state() {
        let _guard = env_lock();
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        write(
            &home.user().join("overrides/example.org"),
            "user override\n",
        );
        write(&home.sessions().join("session.jsonl"), "{}\n");

        let old_runtime = home.runtimes().join("0.1.0-darwin-aarch64");
        write_runtime_payload(&old_runtime, "old");
        swap_runtime_links(&home, &old_runtime).unwrap();

        let payload = tmp.path().join("payload-new");
        write_runtime_payload(&payload, "new");
        let bundle = tmp.path().join("runtime-new.tar.gz");
        tar_runtime(&payload, &bundle);
        let sha = sha256_hex(&std::fs::read(&bundle).unwrap());
        let manifest = tmp.path().join("runtime-latest.json");
        write(&manifest, &bundle_manifest("0.2.0", &bundle, &sha));
        install_state::write(
            &home,
            &InstallState {
                mode: InstallMode::Bundle,
                channel: Some("nightly".to_string()),
                version: Some("0.1.0".to_string()),
                target: Some("darwin-aarch64".to_string()),
                manifest_url: Some(manifest.to_string_lossy().to_string()),
                runtime_dir: Some(old_runtime.clone()),
                source_checkout: None,
            },
        )
        .unwrap();

        let skills_dir = tmp.path().join("skills");
        let previous_skills_dir = std::env::var_os("AGENT_SKILLS_DIR");
        std::env::set_var("AGENT_SKILLS_DIR", &skills_dir);
        let result = run(&home, "main", true, None);
        if let Some(previous) = previous_skills_dir {
            std::env::set_var("AGENT_SKILLS_DIR", previous);
        } else {
            std::env::remove_var("AGENT_SKILLS_DIR");
        }
        result.unwrap();

        assert_eq!(
            std::fs::read_link(home.current_runtime()).unwrap(),
            PathBuf::from("runtimes/0.2.0-darwin-aarch64")
        );
        assert_eq!(
            std::fs::read_link(home.source()).unwrap(),
            PathBuf::from("current")
        );
        assert_eq!(
            std::fs::read_link(home.bin_orgasmic()).unwrap(),
            PathBuf::from("../current/bin/orgasmic")
        );
        assert!(skills_dir.join("orgasmic/SKILL.md").exists());
        assert_eq!(
            std::fs::read_to_string(home.user().join("overrides/example.org")).unwrap(),
            "user override\n"
        );
        assert!(home.sessions().join("session.jsonl").exists());
    }

    #[test]
    fn bundle_update_clears_temporary_daemon_runtime_override() {
        let _guard = env_lock();
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();

        let old_runtime = home.runtimes().join("0.1.0-darwin-aarch64");
        write_runtime_payload(&old_runtime, "old");
        swap_runtime_links(&home, &old_runtime).unwrap();

        let checkout = tmp.path().join("checkout");
        std::fs::create_dir_all(&checkout).unwrap();
        std::fs::write(checkout.join("Cargo.toml"), "[workspace]\n").unwrap();
        let local_bin = checkout.join("target/release/orgasmic");
        if let Some(parent) = local_bin.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&local_bin, b"#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&local_bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        daemon_runtime::set_local_source(&home, &checkout, false).unwrap();
        assert!(daemon_runtime::read(&home).unwrap().is_some());

        let payload = tmp.path().join("payload-new");
        write_runtime_payload(&payload, "new");
        let bundle = tmp.path().join("runtime-new.tar.gz");
        tar_runtime(&payload, &bundle);
        let sha = sha256_hex(&std::fs::read(&bundle).unwrap());
        let manifest = tmp.path().join("runtime-latest.json");
        write(&manifest, &bundle_manifest("0.2.0", &bundle, &sha));
        install_state::write(
            &home,
            &InstallState {
                mode: InstallMode::Bundle,
                channel: Some("nightly".to_string()),
                version: Some("0.1.0".to_string()),
                target: Some("darwin-aarch64".to_string()),
                manifest_url: Some(manifest.to_string_lossy().to_string()),
                runtime_dir: Some(old_runtime),
                source_checkout: None,
            },
        )
        .unwrap();

        let skills_dir = tmp.path().join("skills");
        let previous_skills_dir = std::env::var_os("AGENT_SKILLS_DIR");
        std::env::set_var("AGENT_SKILLS_DIR", &skills_dir);
        let result = run(&home, "main", true, None);
        if let Some(previous) = previous_skills_dir {
            std::env::set_var("AGENT_SKILLS_DIR", previous);
        } else {
            std::env::remove_var("AGENT_SKILLS_DIR");
        }
        result.unwrap();

        assert!(
            daemon_runtime::read(&home).unwrap().is_none(),
            "bundle update must return daemon starts to the managed runtime"
        );
        assert_eq!(
            std::fs::read_link(home.bin_orgasmic()).unwrap(),
            PathBuf::from("../current/bin/orgasmic")
        );
    }

    #[test]
    fn bundle_update_short_circuits_when_already_current() {
        let _guard = env_lock();
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();

        // Install runtime 0.2.0 and make it the active `current`.
        let runtime = home.runtimes().join("0.2.0-darwin-aarch64");
        write_runtime_payload(&runtime, "current");
        swap_runtime_links(&home, &runtime).unwrap();

        // Manifest advertises the SAME version, but with an asset URL that would
        // fail if fetched — proving the short-circuit returns before downloading.
        let manifest = tmp.path().join("runtime-latest.json");
        write(
            &manifest,
            &bundle_manifest(
                "0.2.0",
                Path::new("/orgasmic/nonexistent-runtime.tar.gz"),
                "deadbeef",
            ),
        );
        install_state::write(
            &home,
            &InstallState {
                mode: InstallMode::Bundle,
                channel: Some("nightly".to_string()),
                version: Some("0.2.0".to_string()),
                target: Some("darwin-aarch64".to_string()),
                manifest_url: Some(manifest.to_string_lossy().to_string()),
                runtime_dir: Some(runtime),
                source_checkout: None,
            },
        )
        .unwrap();

        // Same version, active runtime, no override -> no-op success without
        // touching the (unfetchable) asset URL.
        run(&home, "main", true, None).unwrap();

        assert_eq!(
            std::fs::read_link(home.current_runtime()).unwrap(),
            PathBuf::from("runtimes/0.2.0-darwin-aarch64")
        );
    }

    #[test]
    fn bundle_update_refuses_bad_checksum() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let payload = tmp.path().join("payload-new");
        write_runtime_payload(&payload, "new");
        let bundle = tmp.path().join("runtime-new.tar.gz");
        tar_runtime(&payload, &bundle);
        let manifest = tmp.path().join("runtime-latest.json");
        write(&manifest, &bundle_manifest("0.2.0", &bundle, "deadbeef"));
        install_state::write(
            &home,
            &InstallState {
                mode: InstallMode::Bundle,
                channel: Some("nightly".to_string()),
                version: Some("0.1.0".to_string()),
                target: Some("darwin-aarch64".to_string()),
                manifest_url: Some(manifest.to_string_lossy().to_string()),
                runtime_dir: None,
                source_checkout: None,
            },
        )
        .unwrap();

        let err = run(&home, "main", true, None).unwrap_err().to_string();
        assert!(err.contains("checksum mismatch"), "{err}");
        assert!(!home.current_runtime().exists());
    }

    fn touch_mtime(path: &Path, stamp: &str) {
        // stamp: YYYYMMDDhhmm — pin mtime so prune ordering is deterministic.
        let status = Command::new("touch")
            .arg("-t")
            .arg(stamp)
            .arg(path)
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[test]
    fn prune_keeps_current_plus_one_previous_for_target() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();

        let target = "darwin-aarch64";
        let mk = |version: &str, stamp: &str| {
            let dir = home.runtimes().join(format!("{version}-{target}"));
            write_runtime_payload(&dir, version);
            touch_mtime(&dir, stamp); // after writing payload, which bumps the dir mtime
            dir
        };
        let oldest = mk("0.0.1", "202601010000");
        let previous = mk("0.0.3", "202602010000");
        let current = mk("0.0.6", "202603010000");
        // A different target must be left untouched even though it is the oldest.
        let other_target = home.runtimes().join("0.0.5-linux-x86_64");
        write_runtime_payload(&other_target, "linux");
        touch_mtime(&other_target, "202512010000");

        let removed = prune_old_runtimes(&home, &current, target, 1).unwrap();

        assert_eq!(removed, vec!["0.0.1-darwin-aarch64".to_string()]);
        assert!(!oldest.exists(), "oldest darwin runtime should be pruned");
        assert!(
            previous.exists(),
            "the retained previous runtime must remain"
        );
        assert!(current.exists(), "the active runtime must never be pruned");
        assert!(
            other_target.exists(),
            "other-target runtimes must be left alone"
        );
    }

    #[test]
    fn prune_removes_nothing_when_within_retention() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let target = "darwin-aarch64";
        let current = home.runtimes().join(format!("0.0.6-{target}"));
        write_runtime_payload(&current, "current");
        let previous = home.runtimes().join(format!("0.0.3-{target}"));
        write_runtime_payload(&previous, "previous");

        let removed = prune_old_runtimes(&home, &current, target, 1).unwrap();

        assert!(
            removed.is_empty(),
            "one previous runtime is within retention=1"
        );
        assert!(previous.exists());
        assert!(current.exists());
    }
}

// arch: arch_WZFAX.2
// orgasmic:arch_WZFAX, arch_QXS5W, dec_GRDFB
//! Optional shipped content and hub install lifecycle.
//!
//! TASK-016 is CLI-scoped, so installed content is materialized into the
//! existing `user/<family>` loader layer instead of changing the shared
//! loader. Registries live in `user/optional.org` and `user/hub.org`.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, bail, Context, Result};
use orgasmic_core::OrgFile;

use crate::home::Home;

const SUPPORTED_FAMILIES: &[&str] = &["skills", "prompt-specs", "prompt-parts", "context-packs"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OptionalItem {
    pub family: String,
    pub name: String,
    pub source: PathBuf,
    pub enabled: bool,
}

impl OptionalItem {
    pub fn id(&self) -> String {
        format!("{}/{}", self.family, self.name)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LifecycleEntry {
    pub family: String,
    pub name: String,
    pub source: Option<String>,
    pub target: String,
    pub url: Option<String>,
    pub materialized: Vec<String>,
}

impl LifecycleEntry {
    pub fn id(&self) -> String {
        format!("{}/{}", self.family, self.name)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HubInstall {
    pub url: String,
    pub family: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistryFinding {
    Warn(String),
    Fail(String),
}

pub fn list_optional(home: &Home) -> Result<Vec<OptionalItem>> {
    let enabled = read_registry(&optional_registry_path(home))?;
    let enabled_ids: BTreeSet<String> = enabled.iter().map(LifecycleEntry::id).collect();
    let mut out = Vec::new();
    let root = home.source().join("shipped/optional");
    for family in SUPPORTED_FAMILIES {
        let dir = root.join(family);
        if !dir.is_dir() {
            continue;
        }
        for entry in std::fs::read_dir(&dir).with_context(|| format!("read {}", dir.display()))? {
            let entry = entry.with_context(|| format!("read entry in {}", dir.display()))?;
            let path = entry.path();
            let Some(name) = optional_name(&path) else {
                continue;
            };
            let item = OptionalItem {
                family: (*family).to_string(),
                name,
                source: path,
                enabled: false,
            };
            out.push(OptionalItem {
                enabled: enabled_ids.contains(&item.id()),
                ..item
            });
        }
    }
    out.sort_by_key(OptionalItem::id);
    Ok(out)
}

pub fn enable_optional(home: &Home, selector: &str) -> Result<LifecycleEntry> {
    let item = resolve_optional_selector(home, selector)?;
    let path = optional_registry_path(home);
    let mut entries = read_registry(&path)?;
    let existing = entries
        .iter()
        .find(|entry| entry.family == item.family && entry.name == item.name)
        .cloned();
    let materialized = materialize_package(
        home,
        &item.family,
        &item.name,
        &item.source,
        existing.as_ref(),
    )?;
    let entry = LifecycleEntry {
        family: item.family,
        name: item.name,
        source: Some(source_rel_for_optional(&item.source, home)?),
        target: registry_target(&materialized),
        url: None,
        materialized,
    };
    upsert_entry(&mut entries, entry.clone());
    write_registry(&path, "orgasmic optional content", "ENABLED", &entries)?;
    Ok(entry)
}

pub fn disable_optional(home: &Home, selector: &str) -> Result<LifecycleEntry> {
    let path = optional_registry_path(home);
    let mut entries = read_registry(&path)?;
    let idx = resolve_registry_selector(&entries, selector)?;
    let entry = entries.remove(idx);
    remove_optional_materialization(home, &entry)?;
    write_registry(&path, "orgasmic optional content", "ENABLED", &entries)?;
    Ok(entry)
}

pub fn install_hub(home: &Home, install: HubInstall) -> Result<LifecycleEntry> {
    validate_family(&install.family)?;
    let name = name_from_url(&install.url)?;
    validate_name(&name)?;
    let target_rel = format!("{}/{}", install.family, name);
    let target = home.user().join(&target_rel);
    clone_or_fetch(&install.url, &target)?;

    let path = hub_registry_path(home);
    let mut entries = read_registry(&path)?;
    let existing = entries
        .iter()
        .find(|entry| entry.family == install.family && entry.name == name)
        .cloned();
    let materialized =
        materialize_package(home, &install.family, &name, &target, existing.as_ref())?;
    let entry = LifecycleEntry {
        family: install.family,
        name,
        source: None,
        target: target_rel,
        url: Some(install.url),
        materialized,
    };
    upsert_entry(&mut entries, entry.clone());
    write_registry(&path, "orgasmic hub content", "HUB", &entries)?;
    Ok(entry)
}

pub fn list_hub(home: &Home) -> Result<Vec<LifecycleEntry>> {
    read_registry(&hub_registry_path(home))
}

pub fn remove_hub(home: &Home, selector: &str) -> Result<LifecycleEntry> {
    let path = hub_registry_path(home);
    let mut entries = read_registry(&path)?;
    let idx = resolve_registry_selector(&entries, selector)?;
    let entry = entries.remove(idx);
    remove_hub_materialization(home, &entry)?;
    write_registry(&path, "orgasmic hub content", "HUB", &entries)?;
    Ok(entry)
}

pub fn diagnose(home: &Home) -> Vec<RegistryFinding> {
    let mut findings = Vec::new();
    diagnose_optional(home, &mut findings);
    diagnose_hub(home, &mut findings);
    findings
}

pub fn optional_registry_path(home: &Home) -> PathBuf {
    home.user().join("optional.org")
}

pub fn hub_registry_path(home: &Home) -> PathBuf {
    home.user().join("hub.org")
}

fn diagnose_optional(home: &Home, findings: &mut Vec<RegistryFinding>) {
    let path = optional_registry_path(home);
    if !path.exists() {
        return;
    }
    match read_registry(&path) {
        Ok(entries) => {
            for entry in entries {
                validate_entry_common(home, &entry, "optional", findings);
                match &entry.source {
                    Some(source) => {
                        let source_path = home.source().join("shipped").join(source);
                        if !source_path.exists() {
                            findings.push(RegistryFinding::Fail(format!(
                                "optional source missing for {}: {}",
                                entry.id(),
                                source_path.display()
                            )));
                        }
                    }
                    None => findings.push(RegistryFinding::Fail(format!(
                        "optional entry missing SOURCE: {}",
                        entry.id()
                    ))),
                }
            }
        }
        Err(e) => findings.push(RegistryFinding::Fail(format!(
            "optional registry broken: {} ({e})",
            path.display()
        ))),
    }
}

fn diagnose_hub(home: &Home, findings: &mut Vec<RegistryFinding>) {
    let path = hub_registry_path(home);
    if !path.exists() {
        return;
    }
    match read_registry(&path) {
        Ok(entries) => {
            for entry in entries {
                validate_entry_common(home, &entry, "hub", findings);
                let target = home.user().join(&entry.target);
                if !target.exists() {
                    findings.push(RegistryFinding::Fail(format!(
                        "hub target missing for {}: {}",
                        entry.id(),
                        target.display()
                    )));
                } else if !target.join(".git").is_dir() {
                    findings.push(RegistryFinding::Fail(format!(
                        "hub target is not a git checkout for {}: {}",
                        entry.id(),
                        target.display()
                    )));
                }
                if entry.url.as_deref().unwrap_or("").is_empty() {
                    findings.push(RegistryFinding::Fail(format!(
                        "hub entry missing URL: {}",
                        entry.id()
                    )));
                }
            }
        }
        Err(e) => findings.push(RegistryFinding::Fail(format!(
            "hub registry broken: {} ({e})",
            path.display()
        ))),
    }
}

fn validate_entry_common(
    home: &Home,
    entry: &LifecycleEntry,
    label: &str,
    findings: &mut Vec<RegistryFinding>,
) {
    if let Err(e) = validate_family(&entry.family) {
        findings.push(RegistryFinding::Fail(format!(
            "{label} entry has invalid family for {}: {e}",
            entry.id()
        )));
    }
    if entry.materialized.is_empty() {
        findings.push(RegistryFinding::Warn(format!(
            "{label} entry has no materialized loader files: {}",
            entry.id()
        )));
    }
    for rel in &entry.materialized {
        let path = home.user().join(rel);
        if !path.exists() {
            findings.push(RegistryFinding::Fail(format!(
                "{label} materialized path missing for {}: {}",
                entry.id(),
                path.display()
            )));
        }
    }
}

fn resolve_optional_selector(home: &Home, selector: &str) -> Result<OptionalItem> {
    let available = list_optional(home)?;
    let matches: Vec<_> = if let Some((family, name)) = selector.split_once('/') {
        validate_family(family)?;
        available
            .into_iter()
            .filter(|item| item.family == family && item.name == name)
            .collect()
    } else {
        available
            .into_iter()
            .filter(|item| item.name == selector)
            .collect()
    };
    match matches.as_slice() {
        [item] => Ok(item.clone()),
        [] => bail!("optional content not found: {selector}"),
        _ => bail!("optional selector is ambiguous, use <family>/<name>: {selector}"),
    }
}

fn resolve_registry_selector(entries: &[LifecycleEntry], selector: &str) -> Result<usize> {
    let matches: Vec<_> = if let Some((family, name)) = selector.split_once('/') {
        validate_family(family)?;
        entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.family == family && entry.name == name)
            .map(|(idx, _)| idx)
            .collect()
    } else {
        entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.name == selector)
            .map(|(idx, _)| idx)
            .collect()
    };
    match matches.as_slice() {
        [idx] => Ok(*idx),
        [] => bail!("content entry not found: {selector}"),
        _ => bail!("content selector is ambiguous, use <family>/<name>: {selector}"),
    }
}

fn optional_name(path: &Path) -> Option<String> {
    if path.is_dir() {
        path.file_name()?.to_str().map(str::to_string)
    } else if path.extension().and_then(|ext| ext.to_str()) == Some("org") {
        path.file_stem()?.to_str().map(str::to_string)
    } else {
        None
    }
}

fn source_rel_for_optional(source: &Path, home: &Home) -> Result<String> {
    let root = home.source().join("shipped");
    let rel = source
        .strip_prefix(&root)
        .with_context(|| format!("optional source outside shipped root: {}", source.display()))?;
    Ok(rel.to_string_lossy().to_string())
}

/// User-layer loader base for a family. Prompt-studio families materialize
/// under the `prompt-studio/` namespace so `resolve_loader` finds them; skills
/// keep their top-level home.
fn loader_base(family: &str) -> String {
    match family {
        "prompt-specs" => "prompt-studio/prompt-specs".to_string(),
        "prompt-parts" => "prompt-studio/prompt-parts".to_string(),
        "context-packs" => "prompt-studio/context-packs".to_string(),
        other => other.to_string(),
    }
}

fn materialize_package(
    home: &Home,
    family: &str,
    name: &str,
    source: &Path,
    existing: Option<&LifecycleEntry>,
) -> Result<Vec<String>> {
    validate_family(family)?;
    validate_name(name)?;
    if source.is_file() {
        return materialize_file(home, family, name, source, existing);
    }
    if !source.is_dir() {
        bail!("content source missing: {}", source.display());
    }
    match family {
        "skills" | "prompt-specs" | "prompt-parts" | "context-packs" => {
            materialize_flat_orgs(home, family, source, existing)
        }
        _ => unreachable!("validated family"),
    }
}

fn materialize_file(
    home: &Home,
    family: &str,
    _name: &str,
    source: &Path,
    existing: Option<&LifecycleEntry>,
) -> Result<Vec<String>> {
    let file_name = source
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| anyhow!("source file has no name: {}", source.display()))?;
    if source.extension().and_then(|ext| ext.to_str()) != Some("org") {
        bail!("content file must be .org: {}", source.display());
    }
    let rel = format!("{}/{file_name}", loader_base(family));
    copy_materialized(home, source, &rel, existing)?;
    Ok(vec![rel])
}

fn materialize_flat_orgs(
    home: &Home,
    family: &str,
    source: &Path,
    existing: Option<&LifecycleEntry>,
) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for path in direct_org_files(source)? {
        let file_name = path
            .file_name()
            .and_then(|value| value.to_str())
            .ok_or_else(|| anyhow!("source file has no name: {}", path.display()))?;
        let rel = format!("{}/{file_name}", loader_base(family));
        copy_materialized(home, &path, &rel, existing)?;
        out.push(rel);
    }
    if out.is_empty() {
        bail!("no root .org files found in {}", source.display());
    }
    out.sort();
    Ok(out)
}

fn copy_materialized(
    home: &Home,
    source: &Path,
    rel: &str,
    existing: Option<&LifecycleEntry>,
) -> Result<()> {
    let dest = home.user().join(rel);
    let was_owned = existing
        .map(|entry| entry.materialized.iter().any(|owned| owned == rel))
        .unwrap_or(false);
    if dest.exists() && !was_owned {
        bail!(
            "refusing to overwrite existing user content outside this registration: {}",
            dest.display()
        );
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    std::fs::copy(source, &dest)
        .with_context(|| format!("copy {} -> {}", source.display(), dest.display()))?;
    Ok(())
}

fn direct_org_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).with_context(|| format!("read {}", dir.display()))? {
        let entry = entry.with_context(|| format!("read entry in {}", dir.display()))?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) == Some("org") {
            out.push(path);
        }
    }
    out.sort();
    Ok(out)
}

fn remove_optional_materialization(home: &Home, entry: &LifecycleEntry) -> Result<()> {
    for rel in &entry.materialized {
        remove_file_if_exists(&home.user().join(rel))?;
        remove_empty_parent_dirs(home, rel)?;
    }
    Ok(())
}

fn remove_hub_materialization(home: &Home, entry: &LifecycleEntry) -> Result<()> {
    for rel in &entry.materialized {
        let path = home.user().join(rel);
        if path != home.user().join(&entry.target) {
            remove_file_if_exists(&path)?;
            remove_empty_parent_dirs(home, rel)?;
        }
    }
    let target = home.user().join(&entry.target);
    if target.exists() {
        std::fs::remove_dir_all(&target).with_context(|| format!("remove {}", target.display()))?;
    }
    Ok(())
}

fn remove_file_if_exists(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let meta = std::fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
    if meta.is_dir() {
        bail!(
            "refusing to remove directory as materialized file: {}",
            path.display()
        );
    }
    std::fs::remove_file(path).with_context(|| format!("remove {}", path.display()))
}

fn remove_empty_parent_dirs(home: &Home, rel: &str) -> Result<()> {
    let mut current = home.user().join(rel).parent().map(Path::to_path_buf);
    while let Some(dir) = current {
        if dir == home.user()
            || SUPPORTED_FAMILIES
                .iter()
                .any(|family| dir == home.user().join(family))
        {
            break;
        }
        match std::fs::remove_dir(&dir) {
            Ok(()) => current = dir.parent().map(Path::to_path_buf),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => break,
            Err(e) => {
                if directory_has_entries(&dir)? {
                    break;
                }
                return Err(e).with_context(|| format!("remove {}", dir.display()));
            }
        }
    }
    Ok(())
}

fn directory_has_entries(dir: &Path) -> Result<bool> {
    let mut entries = std::fs::read_dir(dir).with_context(|| format!("read {}", dir.display()))?;
    match entries.next() {
        Some(Ok(_)) => Ok(true),
        Some(Err(e)) => Err(e).with_context(|| format!("read entry in {}", dir.display())),
        None => Ok(false),
    }
}

fn clone_or_fetch(url: &str, target: &Path) -> Result<()> {
    if target.exists() {
        if !target.join(".git").is_dir() {
            bail!(
                "hub target exists but is not a git checkout: {}",
                target.display()
            );
        }
        run_git(["-C", &target.to_string_lossy(), "fetch", "--all", "--prune"])?;
        run_git(["-C", &target.to_string_lossy(), "pull", "--ff-only"])?;
        return Ok(());
    }
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    run_git(["clone", url, &target.to_string_lossy()])
}

fn run_git<const N: usize>(args: [&str; N]) -> Result<()> {
    let status = Command::new("git").args(args).status().context("run git")?;
    if !status.success() {
        bail!("git command failed with status {status}");
    }
    Ok(())
}

fn name_from_url(url: &str) -> Result<String> {
    let trimmed = url.trim_end_matches('/');
    let name = trimmed
        .rsplit(['/', ':'])
        .next()
        .unwrap_or(trimmed)
        .trim_end_matches(".git")
        .to_string();
    validate_name(&name)?;
    Ok(name)
}

fn validate_family(family: &str) -> Result<()> {
    if SUPPORTED_FAMILIES.contains(&family) {
        Ok(())
    } else {
        bail!(
            "unsupported family `{family}`; expected one of {}",
            SUPPORTED_FAMILIES.join(", ")
        )
    }
}

fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.contains('/')
        || name.contains('\\')
        || name == "."
        || name == ".."
        || name.chars().any(char::is_whitespace)
    {
        bail!("invalid content name `{name}`");
    }
    Ok(())
}

fn registry_target(materialized: &[String]) -> String {
    if materialized.len() == 1 {
        materialized[0].clone()
    } else {
        String::new()
    }
}

fn upsert_entry(entries: &mut Vec<LifecycleEntry>, entry: LifecycleEntry) {
    if let Some(existing) = entries
        .iter_mut()
        .find(|existing| existing.family == entry.family && existing.name == entry.name)
    {
        *existing = entry;
    } else {
        entries.push(entry);
        entries.sort_by_key(LifecycleEntry::id);
    }
}

fn read_registry(path: &Path) -> Result<Vec<LifecycleEntry>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let source =
        std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let file = OrgFile::parse(source, path.to_string_lossy())
        .with_context(|| format!("parse {}", path.display()))?;
    let mut entries = Vec::new();
    for heading in &file.headings {
        let Some(family) = heading.property("FAMILY") else {
            continue;
        };
        let Some(name) = heading.property("NAME") else {
            continue;
        };
        validate_family(family)?;
        validate_name(name)?;
        entries.push(LifecycleEntry {
            family: family.to_string(),
            name: name.to_string(),
            source: non_empty_property(heading.property("SOURCE")),
            target: heading.property("TARGET").unwrap_or("").to_string(),
            url: non_empty_property(heading.property("URL")),
            materialized: split_words(heading.property("MATERIALIZED")),
        });
    }
    Ok(entries)
}

fn non_empty_property(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn split_words(value: Option<&str>) -> Vec<String> {
    value
        .unwrap_or("")
        .split_whitespace()
        .map(str::to_string)
        .collect()
}

fn write_registry(
    path: &Path,
    title: &str,
    heading_kind: &str,
    entries: &[LifecycleEntry],
) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let rendered = render_registry(title, heading_kind, entries);
    OrgFile::parse(rendered.clone(), path.to_string_lossy())
        .with_context(|| format!("rendered registry does not parse: {}", path.display()))?;
    std::fs::write(path, rendered).with_context(|| format!("write {}", path.display()))
}

fn render_registry(title: &str, heading_kind: &str, entries: &[LifecycleEntry]) -> String {
    let mut out = format!("#+title: {title}\n#+orgasmic_version: 1\n\n");
    for entry in entries {
        out.push_str(&format!("* {heading_kind} {}\n", entry.id()));
        out.push_str(":PROPERTIES:\n");
        out.push_str(&format!(
            ":ID:               {}-{}\n",
            heading_kind.to_ascii_lowercase(),
            entry.id().replace('/', "-")
        ));
        out.push_str(&format!(":FAMILY:           {}\n", entry.family));
        out.push_str(&format!(":NAME:             {}\n", entry.name));
        if let Some(source) = &entry.source {
            out.push_str(&format!(":SOURCE:           {source}\n"));
        }
        out.push_str(&format!(":TARGET:           {}\n", entry.target));
        if let Some(url) = &entry.url {
            out.push_str(&format!(":URL:              {url}\n"));
        }
        out.push_str(&format!(
            ":MATERIALIZED:     {}\n",
            entry.materialized.join(" ")
        ));
        out.push_str(":END:\n\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_optional(home: &Home, family: &str, name: &str, file_name: &str, body: &str) {
        let dir = home
            .source()
            .join("shipped/optional")
            .join(family)
            .join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(file_name), body).unwrap();
    }

    fn init_git_repo(path: &Path, file_name: &str, body: &str) {
        std::fs::create_dir_all(path).unwrap();
        std::fs::write(path.join(file_name), body).unwrap();
        git(path, ["init"]);
        git(path, ["config", "user.email", "test@example.com"]);
        git(path, ["config", "user.name", "Test User"]);
        git(path, ["add", "."]);
        git(path, ["commit", "-m", "init"]);
    }

    fn git<const N: usize>(cwd: &Path, args: [&str; N]) {
        let status = Command::new("git")
            .current_dir(cwd)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[test]
    fn optional_enable_disable_materializes_all_supported_families() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        write_optional(
            &home,
            "skills",
            "skill-a",
            "skill-a.org",
            "* SKILL skill-a\n:PROPERTIES:\n:ID: skill-a\n:END:\n",
        );
        write_optional(
            &home,
            "prompt-specs",
            "implementer",
            "implementer.org",
            "* PROMPT-SPEC implementer\n:PROPERTIES:\n:ID: implementer\n:KIND: implementer\n:END:\n",
        );
        write_optional(
            &home,
            "prompt-parts",
            "scope-policy",
            "scope-policy.org",
            "* PROMPT-PART scope-policy\n:PROPERTIES:\n:ID: scope-policy\n:TARGET_SECTION: Operating Rules\n:END:\n",
        );
        write_optional(
            &home,
            "context-packs",
            "project-state",
            "project-state.org",
            "* CONTEXT-PACK project-state\n:PROPERTIES:\n:ID: project-state\n:END:\n",
        );

        let skill = enable_optional(&home, "skills/skill-a").unwrap();
        let prompt_spec = enable_optional(&home, "prompt-specs/implementer").unwrap();
        let prompt_part = enable_optional(&home, "prompt-parts/scope-policy").unwrap();
        let context_pack = enable_optional(&home, "context-packs/project-state").unwrap();

        assert_eq!(skill.materialized, vec!["skills/skill-a.org"]);
        assert_eq!(
            prompt_spec.materialized,
            vec!["prompt-studio/prompt-specs/implementer.org"]
        );
        assert_eq!(
            prompt_part.materialized,
            vec!["prompt-studio/prompt-parts/scope-policy.org"]
        );
        assert_eq!(
            context_pack.materialized,
            vec!["prompt-studio/context-packs/project-state.org"]
        );
        assert!(home.user().join("skills/skill-a.org").is_file());
        assert!(home
            .user()
            .join("prompt-studio/prompt-specs/implementer.org")
            .is_file());
        assert!(home
            .user()
            .join("prompt-studio/prompt-parts/scope-policy.org")
            .is_file());
        assert!(home
            .user()
            .join("prompt-studio/context-packs/project-state.org")
            .is_file());
        assert_eq!(list_optional(&home).unwrap().len(), 4);

        disable_optional(&home, "skills/skill-a").unwrap();
        disable_optional(&home, "prompt-specs/implementer").unwrap();
        disable_optional(&home, "prompt-parts/scope-policy").unwrap();
        disable_optional(&home, "context-packs/project-state").unwrap();

        assert!(!home.user().join("skills/skill-a.org").exists());
        assert!(!home
            .user()
            .join("prompt-studio/prompt-specs/implementer.org")
            .exists());
        assert!(!home
            .user()
            .join("prompt-studio/prompt-parts/scope-policy.org")
            .exists());
        assert!(!home
            .user()
            .join("prompt-studio/context-packs/project-state.org")
            .exists());
    }

    #[test]
    fn hub_install_list_remove_uses_git_and_loader_layer() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let src = tmp.path().join("skill-hub");
        init_git_repo(
            &src,
            "skill-hub.org",
            "* SKILL skill-hub\n:PROPERTIES:\n:ID: skill-hub\n:END:\n",
        );

        let entry = install_hub(
            &home,
            HubInstall {
                url: src.to_string_lossy().to_string(),
                family: "skills".to_string(),
            },
        )
        .unwrap();

        assert_eq!(entry.id(), "skills/skill-hub");
        assert!(home.user().join("skills/skill-hub/.git").is_dir());
        assert!(home.user().join("skills/skill-hub.org").is_file());
        assert_eq!(list_hub(&home).unwrap().len(), 1);

        remove_hub(&home, "skill-hub").unwrap();
        assert!(!home.user().join("skills/skill-hub").exists());
        assert!(!home.user().join("skills/skill-hub.org").exists());
        assert!(list_hub(&home).unwrap().is_empty());
    }

    #[test]
    fn doctor_finds_broken_registrations() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        write_optional(
            &home,
            "skills",
            "skill-a",
            "skill-a.org",
            "* SKILL skill-a\n:PROPERTIES:\n:ID: skill-a\n:END:\n",
        );
        enable_optional(&home, "skills/skill-a").unwrap();
        std::fs::remove_file(home.user().join("skills/skill-a.org")).unwrap();

        let findings = diagnose(&home);
        assert!(findings.iter().any(|finding| {
            matches!(finding, RegistryFinding::Fail(message) if message.contains("materialized path missing"))
        }));
    }
}

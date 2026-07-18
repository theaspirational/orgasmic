// arch: arch_QXS5W.1, arch_QXS5W.2
// orgasmic:arch_R3EPE, arch_PCSQE, arch_QXS5W
//! Loader-backed manager content: workers and skills.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::anyhow;
use orgasmic_core::{
    resolve_loader, Home, OrgFile, SkillMetadata, SlotValues, Worker, WorkerError, WorkerKind,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize)]
pub struct WorkerView {
    pub id: String,
    pub kind: WorkerKind,
    pub driver: String,
    pub harness: String,
    pub providers: Vec<String>,
    pub models: Vec<String>,
    pub reasoning_efforts: Vec<String>,
    pub default_provider: Option<String>,
    pub default_model: Option<String>,
    pub default_effort: Option<String>,
    pub linked_skills: Vec<String>,
    pub applicable_states: Vec<String>,
    pub max_iterations: Option<u32>,
    pub context_budget: Option<u32>,
    pub stall_timeout_secs: Option<u32>,
    pub max_run_duration_secs: Option<u32>,
    pub babysitter_worker: Option<String>,
    pub sandbox_permissions: Option<orgasmic_core::SandboxAllowlist>,
    pub harness_args: Vec<String>,
    pub version: Option<String>,
    pub persona: Option<String>,
    pub operating_rules: Option<String>,
    pub source_path: PathBuf,
    pub missing_skills: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkerValidationDiagnostic {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkerValidationResult {
    pub id: Option<String>,
    pub source_path: Option<PathBuf>,
    pub ok: bool,
    pub errors: Vec<WorkerValidationDiagnostic>,
    pub worker: Option<WorkerView>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SkillView {
    pub id: String,
    pub title: String,
    pub description: Option<String>,
    pub triggers: Vec<String>,
    pub absolute_path: Option<String>,
    pub source_path: PathBuf,
}

#[derive(Debug, Deserialize)]
struct MarkdownSkillFrontmatter {
    name: String,
    description: Option<String>,
    triggers: Option<MarkdownSkillTriggers>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum MarkdownSkillTriggers {
    One(String),
    Many(Vec<String>),
}

impl MarkdownSkillTriggers {
    fn into_vec(self) -> Vec<String> {
        match self {
            Self::One(value) => vec![value],
            Self::Many(values) => values,
        }
    }
}

#[derive(Debug)]
pub enum ContentLoadError {
    NotFoundByRule {
        kind: String,
        name: String,
    },
    NotFoundOnDisk {
        path: PathBuf,
        source: io::Error,
    },
    Read {
        path: PathBuf,
        source: io::Error,
    },
    Parse {
        path: PathBuf,
        source: anyhow::Error,
    },
    Other {
        source: anyhow::Error,
    },
}

impl ContentLoadError {
    pub fn path(&self) -> Option<&Path> {
        match self {
            Self::NotFoundOnDisk { path, .. }
            | Self::Read { path, .. }
            | Self::Parse { path, .. } => Some(path.as_path()),
            Self::NotFoundByRule { .. } | Self::Other { .. } => None,
        }
    }
}

impl fmt::Display for ContentLoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFoundByRule { kind, name } => {
                write!(f, "{kind} not found through loader rule: {name}")
            }
            Self::NotFoundOnDisk { path, source } | Self::Read { path, source } => {
                write!(f, "read {}: {source}", path.display())
            }
            Self::Parse { source, .. } | Self::Other { source } => write!(f, "{source}"),
        }
    }
}

impl std::error::Error for ContentLoadError {}

type ContentResult<T> = std::result::Result<T, ContentLoadError>;

pub fn list_workers(home: &Home) -> ContentResult<Vec<WorkerView>> {
    let paths = collect_worker_paths(home)?;
    paths
        .into_values()
        .map(|path| load_worker_path(home, path))
        .collect()
}

pub fn validate_workers(home: &Home) -> ContentResult<Vec<WorkerValidationResult>> {
    let paths = collect_worker_paths(home)?;
    Ok(paths
        .into_values()
        .map(|path| validate_worker_path(home, path))
        .collect())
}

pub fn validate_worker_by_id(home: &Home, id: &str) -> WorkerValidationResult {
    if let Some(path) = resolve_loader(home, &PathBuf::from("workers").join(format!("{id}.org"))) {
        return validate_worker_path(home, path);
    }
    if let Some(current_id) = legacy_worker_alias(id) {
        let rel = PathBuf::from("workers").join(format!("{current_id}.org"));
        if let Some(path) = resolve_loader(home, &rel) {
            return validate_worker_path(home, path);
        }
    }
    WorkerValidationResult {
        id: Some(id.to_string()),
        source_path: None,
        ok: false,
        errors: vec![WorkerValidationDiagnostic {
            code: "not_found".to_string(),
            message: format!("worker not found through loader rule: {id}"),
        }],
        worker: None,
    }
}

pub fn validate_worker_source(
    home: &Home,
    id_hint: Option<&str>,
    source: String,
) -> WorkerValidationResult {
    validate_worker_source_display(home, id_hint, PathBuf::from("<inline-worker>"), source)
}

pub fn load_worker(home: &Home, id: &str) -> ContentResult<WorkerView> {
    if let Some(path) = resolve_loader(home, &PathBuf::from("workers").join(format!("{id}.org"))) {
        return load_worker_path(home, path);
    }
    if let Some(current_id) = legacy_worker_alias(id) {
        let rel = PathBuf::from("workers").join(format!("{current_id}.org"));
        if let Some(path) = resolve_loader(home, &rel) {
            return load_worker_path(home, path);
        }
    }
    Err(ContentLoadError::NotFoundByRule {
        kind: "worker".to_string(),
        name: id.to_string(),
    })
}

fn legacy_worker_alias(id: &str) -> Option<&'static str> {
    match id {
        "implementer-codex-stdio" => Some("implementer-codex-acp"),
        "reviewer-codex-stdio" => Some("reviewer-codex-acp"),
        _ => None,
    }
}

fn collect_worker_paths(home: &Home) -> ContentResult<BTreeMap<String, PathBuf>> {
    let mut paths = BTreeMap::<String, PathBuf>::new();
    collect_org_files(&home.source().join("shipped/workers"), &mut paths)?;
    collect_org_files(&home.user().join("workers"), &mut paths)?;
    Ok(paths)
}

pub fn list_skills(home: &Home) -> ContentResult<Vec<SkillView>> {
    let mut paths = BTreeMap::<String, PathBuf>::new();
    collect_skill_files(&home.source().join("shipped/skills"), &mut paths)?;
    collect_skill_files(&home.user().join("skills"), &mut paths)?;
    paths.into_values().map(load_skill_path).collect()
}

pub fn load_skill(home: &Home, id: &str) -> ContentResult<SkillView> {
    let path = resolve_skill_path(home, id).ok_or_else(|| ContentLoadError::NotFoundByRule {
        kind: "skill".to_string(),
        name: id.to_string(),
    })?;
    load_skill_path(path)
}

struct LinkedSkillValue {
    id: String,
    canonical: String,
    relates_to: Vec<String>,
    body: String,
    source_path: PathBuf,
}

pub fn build_skill_slot_values(home: &Home, worker: &Worker<'_>) -> ContentResult<SlotValues> {
    let records = load_linked_skill_values(home, &worker.linked_skills)?;
    let mut values = SlotValues::new();
    values.insert("skills.all".to_string(), skill_manifest(&records));
    for record in records {
        values.insert(format!("skills.{}", record.id), record.body);
        values.insert(
            format!("skill_path.{}", record.id),
            record.source_path.display().to_string(),
        );
    }
    Ok(values)
}

fn load_linked_skill_values(
    home: &Home,
    linked_skills: &[&str],
) -> ContentResult<Vec<LinkedSkillValue>> {
    let mut out = Vec::new();
    for id in linked_skills.iter().copied().collect::<BTreeSet<_>>() {
        out.push(load_linked_skill_value(home, id)?);
    }
    Ok(out)
}

fn load_linked_skill_value(home: &Home, id: &str) -> ContentResult<LinkedSkillValue> {
    let path = resolve_skill_path(home, id).ok_or_else(|| ContentLoadError::Other {
        source: anyhow!("missing linked skill: {id}"),
    })?;
    let source = read_content(&path)?;
    let display = path.to_string_lossy();
    let file = parse_org_file(&path, source)?;
    let heading = file
        .headings
        .first()
        .ok_or_else(|| ContentLoadError::Parse {
            path: path.clone(),
            source: anyhow!("skill file has no heading: {}", path.display()),
        })?;
    let skill = SkillMetadata::from_heading(heading, &display).map_err(|source| {
        ContentLoadError::Parse {
            path: path.clone(),
            source: anyhow::Error::new(source),
        }
    })?;
    if skill.id != id {
        return Err(ContentLoadError::Parse {
            path: path.clone(),
            source: anyhow!(
                "linked skill id mismatch: requested {id}, file declares {}",
                skill.id
            ),
        });
    }
    Ok(LinkedSkillValue {
        id: skill.id.to_string(),
        canonical: heading
            .property("CANONICAL")
            .or(skill.description)
            .unwrap_or(skill.title)
            .trim()
            .to_string(),
        relates_to: split_manifest_ids(heading.property("RELATES_TO")),
        body: heading_payload(&file, heading).trim().to_string(),
        source_path: std::fs::canonicalize(&path).map_err(|source| ContentLoadError::Read {
            path: path.clone(),
            source,
        })?,
    })
}

fn skill_manifest(skills: &[LinkedSkillValue]) -> String {
    skills
        .iter()
        .map(|skill| {
            let relates = if skill.relates_to.is_empty() {
                "none".to_string()
            } else {
                skill.relates_to.join(", ")
            };
            format!("{} — {} — relates: {relates}", skill.id, skill.canonical)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn split_manifest_ids(value: Option<&str>) -> Vec<String> {
    value
        .map(|value| {
            value
                .split(|c: char| c == ',' || c.is_whitespace())
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn resolve_skill_path(home: &Home, id: &str) -> Option<PathBuf> {
    resolve_loader(home, &PathBuf::from("skills").join(format!("{id}.org")))
        .or_else(|| resolve_loader(home, &PathBuf::from("skills").join(id).join("SKILL.md")))
}

fn heading_payload(file: &OrgFile, heading: &orgasmic_core::Heading) -> String {
    let start = heading
        .properties
        .as_ref()
        .map(|drawer| drawer.span.end)
        .unwrap_or_else(|| line_after(file.source(), heading.title_line.end));
    file.slice(start..heading.span.end).to_string()
}

fn line_after(source: &str, pos: usize) -> usize {
    source[pos..]
        .find('\n')
        .map(|offset| pos + offset + 1)
        .unwrap_or(pos)
}

fn load_worker_path(home: &Home, path: PathBuf) -> ContentResult<WorkerView> {
    let source = read_content(&path)?;
    let file = parse_org_file(&path, source)?;
    worker_view_from_org(home, path.clone(), &file)
}

fn worker_view_from_org(home: &Home, path: PathBuf, file: &OrgFile) -> ContentResult<WorkerView> {
    let display = path.to_string_lossy();
    let worker = Worker::from_org(file, &display).map_err(|source| ContentLoadError::Parse {
        path: path.clone(),
        source: anyhow::Error::new(source),
    })?;
    worker_view_from_worker(home, path, worker)
}

fn worker_view_from_worker(
    home: &Home,
    path: PathBuf,
    worker: Worker<'_>,
) -> ContentResult<WorkerView> {
    let missing_skills = worker
        .linked_skills
        .iter()
        .filter(|skill| resolve_skill_path(home, skill).is_none())
        .map(|s| (*s).to_string())
        .collect();
    Ok(WorkerView {
        id: worker.id.to_string(),
        kind: worker.kind,
        driver: worker.driver.to_string(),
        harness: worker.harness.to_string(),
        providers: worker.providers,
        models: Vec::new(),
        reasoning_efforts: Vec::new(),
        default_provider: worker.default_provider,
        default_model: None,
        default_effort: None,
        linked_skills: worker
            .linked_skills
            .iter()
            .map(|s| (*s).to_string())
            .collect(),
        applicable_states: worker.applicable_states,
        max_iterations: worker.max_iterations,
        context_budget: worker.context_budget,
        stall_timeout_secs: worker.stall_timeout_secs,
        max_run_duration_secs: worker.max_run_duration_secs,
        babysitter_worker: worker.babysitter_worker.map(str::to_string),
        sandbox_permissions: worker.sandbox_permissions.clone(),
        harness_args: worker.harness_args.clone(),
        version: worker.version.map(str::to_string),
        persona: worker.persona,
        operating_rules: worker.operating_rules,
        source_path: path,
        missing_skills,
    })
}

fn validate_worker_path(home: &Home, path: PathBuf) -> WorkerValidationResult {
    match read_content(&path) {
        Ok(source) => validate_worker_source_display(home, None, path, source),
        Err(error) => WorkerValidationResult {
            id: worker_id_from_path(&path),
            source_path: Some(path),
            ok: false,
            errors: vec![diagnostic_from_content_error(error)],
            worker: None,
        },
    }
}

fn validate_worker_source_display(
    home: &Home,
    id_hint: Option<&str>,
    source_path: PathBuf,
    source: String,
) -> WorkerValidationResult {
    let display = source_path.to_string_lossy();
    let file = match OrgFile::parse(source, display.as_ref()) {
        Ok(file) => file,
        Err(error) => {
            return WorkerValidationResult {
                id: id_hint
                    .map(str::to_string)
                    .or_else(|| worker_id_from_path(&source_path)),
                source_path: Some(source_path),
                ok: false,
                errors: vec![WorkerValidationDiagnostic {
                    code: "parse_error".to_string(),
                    message: error.to_string(),
                }],
                worker: None,
            };
        }
    };

    let worker = match Worker::from_org(&file, &display) {
        Ok(worker) => worker,
        Err(error) => {
            let id = id_hint
                .map(str::to_string)
                .or_else(|| worker_id_from_path(&source_path));
            return WorkerValidationResult {
                id,
                source_path: Some(source_path),
                ok: false,
                errors: vec![diagnostic_from_worker_error(&error)],
                worker: None,
            };
        }
    };
    let id = worker.id.to_string();
    let worker = match worker_view_from_worker(home, source_path.clone(), worker) {
        Ok(worker) => worker,
        Err(error) => {
            return WorkerValidationResult {
                id: Some(id),
                source_path: Some(source_path),
                ok: false,
                errors: vec![diagnostic_from_content_error(error)],
                worker: None,
            };
        }
    };
    let errors = worker
        .missing_skills
        .iter()
        .map(|skill| WorkerValidationDiagnostic {
            code: "missing_skill".to_string(),
            message: format!("unresolved slot skills.{skill}"),
        })
        .collect::<Vec<_>>();
    WorkerValidationResult {
        id: Some(worker.id.clone()),
        source_path: Some(source_path),
        ok: errors.is_empty(),
        errors,
        worker: Some(worker),
    }
}

fn diagnostic_from_worker_error(error: &WorkerError) -> WorkerValidationDiagnostic {
    let code = match error {
        WorkerError::UnsupportedDriverHarness { .. }
        | WorkerError::UnsupportedLegacyDefaultDriver { .. } => "unsupported_driver_harness",
        WorkerError::CustomHarnessMissingArgs { .. } => "schema_error",
        WorkerError::Schema(_) => "schema_error",
    };
    WorkerValidationDiagnostic {
        code: code.to_string(),
        message: error.to_string(),
    }
}

fn diagnostic_from_content_error(error: ContentLoadError) -> WorkerValidationDiagnostic {
    let code = match error {
        ContentLoadError::NotFoundByRule { .. } | ContentLoadError::NotFoundOnDisk { .. } => {
            "not_found"
        }
        ContentLoadError::Read { .. } => "read_error",
        ContentLoadError::Parse { .. } => "parse_error",
        ContentLoadError::Other { .. } => "validation_error",
    };
    WorkerValidationDiagnostic {
        code: code.to_string(),
        message: error.to_string(),
    }
}

fn worker_id_from_path(path: &Path) -> Option<String> {
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .filter(|stem| !stem.is_empty())
        .map(str::to_string)
}

fn load_skill_path(path: PathBuf) -> ContentResult<SkillView> {
    if path.file_name().and_then(|s| s.to_str()) == Some("SKILL.md") {
        return load_markdown_skill_path(path);
    }

    let source = read_content(&path)?;
    let file = parse_org_file(&path, source)?;
    let heading = file
        .headings
        .first()
        .ok_or_else(|| ContentLoadError::Parse {
            path: path.clone(),
            source: anyhow!("skill file has no heading: {}", path.display()),
        })?;
    let display = path.to_string_lossy();
    let skill = SkillMetadata::from_heading(heading, &display).map_err(|source| {
        ContentLoadError::Parse {
            path: path.clone(),
            source: anyhow::Error::new(source),
        }
    })?;
    Ok(SkillView {
        id: skill.id.to_string(),
        title: skill.title.to_string(),
        description: skill.description.map(str::to_string),
        triggers: skill.triggers.iter().map(|s| (*s).to_string()).collect(),
        absolute_path: skill.absolute_path.map(str::to_string),
        source_path: path,
    })
}

fn load_markdown_skill_path(path: PathBuf) -> ContentResult<SkillView> {
    let source = read_content(&path)?;
    let yaml = markdown_frontmatter(&source).ok_or_else(|| ContentLoadError::Parse {
        path: path.clone(),
        source: anyhow!("skill markdown has no frontmatter: {}", path.display()),
    })?;
    let frontmatter: MarkdownSkillFrontmatter =
        serde_yaml::from_str(&yaml).map_err(|source| ContentLoadError::Parse {
            path: path.clone(),
            source: anyhow::Error::new(source),
        })?;
    let description = frontmatter
        .description
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    let mut triggers = frontmatter
        .triggers
        .map(MarkdownSkillTriggers::into_vec)
        .unwrap_or_default();
    if triggers.is_empty() {
        if let Some(description) = description.as_deref() {
            triggers = slash_triggers_from_description(description);
        }
    }

    Ok(SkillView {
        id: frontmatter.name.clone(),
        title: frontmatter.name,
        description,
        triggers,
        absolute_path: Some(path.to_string_lossy().to_string()),
        source_path: path,
    })
}

fn markdown_frontmatter(source: &str) -> Option<String> {
    let mut lines = source.lines();
    if lines.next()? != "---" {
        return None;
    }
    let mut yaml = String::new();
    for line in lines {
        if line.trim() == "---" {
            return Some(yaml);
        }
        yaml.push_str(line);
        yaml.push('\n');
    }
    None
}

fn slash_triggers_from_description(description: &str) -> Vec<String> {
    let mut triggers = Vec::new();
    let mut seen = BTreeSet::new();

    for (index, quoted) in description.split('"').enumerate() {
        if index % 2 == 1 {
            push_slash_trigger(quoted, &mut seen, &mut triggers);
        }
    }

    if let Some((_, tail)) = description.split_once("Triggers:") {
        let list = tail.split('.').next().unwrap_or(tail);
        for item in list.split(',') {
            push_slash_trigger(item, &mut seen, &mut triggers);
        }
    }

    triggers
}

fn push_slash_trigger(raw: &str, seen: &mut BTreeSet<String>, triggers: &mut Vec<String>) {
    let trigger = raw
        .trim()
        .trim_matches(|c| c == '"' || c == '\'' || c == '`');
    if !trigger.starts_with('/') {
        return;
    }
    if seen.insert(trigger.to_string()) {
        triggers.push(trigger.to_string());
    }
}

fn collect_skill_files(dir: &Path, out: &mut BTreeMap<String, PathBuf>) -> ContentResult<()> {
    if !dir.is_dir() {
        return Ok(());
    }
    for entry in read_dir_content(dir)? {
        let entry = entry.map_err(|source| ContentLoadError::Read {
            path: dir.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("org") {
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                out.insert(stem.to_string(), path);
            }
            continue;
        }
        if !path.is_dir() {
            continue;
        }
        let skill_md = path.join("SKILL.md");
        if skill_md.is_file() {
            if let Some(id) = path.file_name().and_then(|s| s.to_str()) {
                out.insert(id.to_string(), skill_md);
            }
        }
    }
    Ok(())
}

fn collect_org_files(dir: &Path, out: &mut BTreeMap<String, PathBuf>) -> ContentResult<()> {
    if !dir.is_dir() {
        return Ok(());
    }
    for entry in read_dir_content(dir)? {
        let entry = entry.map_err(|source| ContentLoadError::Read {
            path: dir.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("org") {
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                out.insert(stem.to_string(), path);
            }
        }
    }
    Ok(())
}

fn read_content(path: &Path) -> ContentResult<String> {
    maybe_delete_before_read(path);
    std::fs::read_to_string(path).map_err(|source| {
        if source.kind() == io::ErrorKind::NotFound {
            ContentLoadError::NotFoundOnDisk {
                path: path.to_path_buf(),
                source,
            }
        } else {
            ContentLoadError::Read {
                path: path.to_path_buf(),
                source,
            }
        }
    })
}

fn read_dir_content(path: &Path) -> ContentResult<std::fs::ReadDir> {
    std::fs::read_dir(path).map_err(|source| {
        if source.kind() == io::ErrorKind::NotFound {
            ContentLoadError::NotFoundOnDisk {
                path: path.to_path_buf(),
                source,
            }
        } else {
            ContentLoadError::Read {
                path: path.to_path_buf(),
                source,
            }
        }
    })
}

fn parse_org_file(path: &Path, source: String) -> ContentResult<OrgFile> {
    OrgFile::parse(source, path.to_string_lossy()).map_err(|source| ContentLoadError::Parse {
        path: path.to_path_buf(),
        source: anyhow::Error::new(source),
    })
}

#[cfg(test)]
static DELETE_BEFORE_READ: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

#[cfg(test)]
fn delete_before_next_read(path: PathBuf) {
    *DELETE_BEFORE_READ.lock().unwrap() = Some(path);
}

#[cfg(test)]
fn maybe_delete_before_read(path: &Path) {
    let mut guard = DELETE_BEFORE_READ.lock().unwrap();
    if guard.as_deref() == Some(path) {
        let target = guard.take().unwrap();
        drop(guard);
        let _ = std::fs::remove_file(target);
    }
}

#[cfg(not(test))]
fn maybe_delete_before_read(_path: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, contents).unwrap();
    }

    #[test]
    fn load_worker_reports_legacy_file_vanished_after_resolve_as_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let path = home.user().join("workers/racy.org");
        write(
            &path,
            "* WORKER racy\n:PROPERTIES:\n:ID: racy\n:KIND:             implementer\n:DRIVER: acp-stdio\n:HARNESS: claude\n:END:\n",
        );

        delete_before_next_read(path.clone());
        let err = load_worker(&home, "racy").unwrap_err();

        match err {
            ContentLoadError::NotFoundOnDisk { path: actual, .. } => assert_eq!(actual, path),
            other => panic!("expected NotFoundOnDisk, got {other:?}"),
        }
    }

    #[test]
    fn validate_worker_source_accepts_valid_template() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();

        let result = validate_worker_source(&home, None, valid_worker("ok", ""));

        assert!(result.ok, "{result:?}");
        assert_eq!(result.id.as_deref(), Some("ok"));
        assert!(result.errors.is_empty());
        assert_eq!(result.worker.as_ref().unwrap().driver, "acp-stdio");
    }

    #[test]
    fn validate_worker_source_reports_parse_error() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();

        let result = validate_worker_source(
            &home,
            Some("bad-parse"),
            "* WORKER bad-parse\n:PROPERTIES:\n:ID bad-parse\n:END:\n".to_string(),
        );

        assert!(!result.ok);
        assert_eq!(result.errors[0].code, "parse_error");
    }

    #[test]
    fn validate_worker_source_reports_schema_error() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();

        let result = validate_worker_source(
            &home,
            Some("bad-schema"),
            "* WORKER bad-schema\n:PROPERTIES:\n:ID: bad-schema\n:DRIVER: acp-stdio\n:HARNESS: claude\n:END:\n".to_string(),
        );

        assert!(!result.ok);
        assert_eq!(result.errors[0].code, "schema_error");
    }

    #[test]
    fn validate_worker_source_reports_unsupported_driver_harness() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();

        let result = validate_worker_source(
            &home,
            Some("bad-pair"),
            valid_worker("bad-pair", ":DRIVER: acp-ws\n:HARNESS: claude\n"),
        );

        assert!(!result.ok);
        assert_eq!(result.errors[0].code, "unsupported_driver_harness");
    }

    #[test]
    fn validate_worker_source_reports_missing_linked_skill() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();

        let result = validate_worker_source(
            &home,
            Some("missing-skill"),
            valid_worker("missing-skill", ":LINKED_SKILLS: missing-skill\n"),
        );

        assert!(!result.ok);
        assert_eq!(result.errors[0].code, "missing_skill");
        assert!(result.errors[0].message.contains("skills.missing-skill"));
    }

    fn valid_worker(id: &str, override_props: &str) -> String {
        let driver = if override_props.contains(":DRIVER:") {
            ""
        } else {
            ":DRIVER: acp-stdio\n"
        };
        let harness = if override_props.contains(":HARNESS:") {
            ""
        } else {
            ":HARNESS: claude\n"
        };
        let linked_skills = if override_props.contains(":LINKED_SKILLS:") {
            ""
        } else {
            ":LINKED_SKILLS:\n"
        };
        format!(
            "* WORKER {id}\n:PROPERTIES:\n:ID: {id}\n:KIND: implementer\n{driver}{harness}:PROVIDERS: anthropic\n:MODELS: claude-sonnet-4-6\n:REASONING_EFFORTS: high\n:DEFAULT_PROVIDER: anthropic\n:DEFAULT_MODEL: claude-sonnet-4-6\n:DEFAULT_EFFORT: high\n{linked_skills}{override_props}:END:\n\n** Persona\nTest.\n"
        )
    }
}

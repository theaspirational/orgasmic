// arch: arch_QXS5W.1, arch_QXS5W.2
// orgasmic:arch_R3EPE, arch_PCSQE, arch_QXS5W
//! Loader-backed manager skill content.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::anyhow;
use orgasmic_core::{resolve_loader, Home, OrgFile, SkillMetadata, SlotValues};
use serde::{Deserialize, Serialize};

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

pub fn build_skill_slot_values(home: &Home, linked_skills: &[String]) -> ContentResult<SlotValues> {
    let records = load_linked_skill_values(home, linked_skills)?;
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
    linked_skills: &[String],
) -> ContentResult<Vec<LinkedSkillValue>> {
    let mut out = Vec::new();
    for id in linked_skills
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>()
    {
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

fn maybe_delete_before_read(_path: &Path) {}

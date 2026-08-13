// orgasmic:arch_QFQTD, arch_QXS5W
//! Typed views on top of the [`crate::org`] parser.
//!
//! These wrappers project the strict orgasmic profile onto Rust types.
//! Downstream crates use these for daemon projection, manager prompts, and
//! graph operations.

use crate::id::{derive_task_parent_id, is_valid_task_path_id, parse_parent_value, NodeIdClass};

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::org::{Heading, OrgFile};
use crate::sandbox::SandboxAllowlist;

#[derive(Debug, Error)]
pub enum SchemaError {
    #[error("{file}: heading {heading}: missing required property :{key}:")]
    MissingProperty {
        file: String,
        heading: String,
        key: String,
    },
    #[error("{file}: required heading {heading} not found")]
    MissingSection { file: String, heading: String },
    #[error("{file}: unknown lifecycle stage {state} on heading {heading}")]
    UnknownLifecycleStage {
        file: String,
        heading: String,
        state: String,
    },
    #[error("{file}: heading {heading}: missing lifecycle TODO keyword")]
    MissingLifecycleStage { file: String, heading: String },
    #[error("{file}: invalid parent task {parent_task} on heading {heading}")]
    InvalidParentTask {
        file: String,
        heading: String,
        parent_task: String,
    },
    #[error("{file}: unknown worker kind {kind} on heading {heading}")]
    UnknownWorkerKind {
        file: String,
        heading: String,
        kind: String,
    },
    #[error("{file}: heading {heading}: invalid :{key}: {detail}")]
    InvalidPropertyValue {
        file: String,
        heading: String,
        key: String,
        detail: String,
    },
}

// --- enums ------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleStage {
    Backlog,
    Todo,
    InProgress,
    InReview,
    Done,
    Cancelled,
}

impl LifecycleStage {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Backlog => "backlog",
            Self::Todo => "todo",
            Self::InProgress => "in_progress",
            Self::InReview => "in_review",
            Self::Done => "done",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn todo_keyword(self) -> &'static str {
        match self {
            Self::Backlog => "BACKLOG",
            Self::Todo => "TODO",
            Self::InProgress => "IN_PROGRESS",
            Self::InReview => "IN_REVIEW",
            Self::Done => "DONE",
            Self::Cancelled => "CANCELLED",
        }
    }
}

impl fmt::Display for LifecycleStage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for LifecycleStage {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, ()> {
        Ok(match s.to_ascii_lowercase().as_str() {
            "backlog" => Self::Backlog,
            "todo" => Self::Todo,
            "in_progress" => Self::InProgress,
            "in_review" => Self::InReview,
            "done" => Self::Done,
            "cancelled" => Self::Cancelled,
            _ => return Err(()),
        })
    }
}

/// The single role vocabulary selected by addressed dispatches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerKind {
    Implementer,
    Reviewer,
    Planner,
    Analyzer,
    Griller,
    Glossarist,
    Manager,
    Artifactor,
}

impl WorkerKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Implementer => "implementer",
            Self::Reviewer => "reviewer",
            Self::Planner => "planner",
            Self::Analyzer => "analyzer",
            Self::Griller => "griller",
            Self::Glossarist => "glossarist",
            Self::Manager => "manager",
            Self::Artifactor => "artifactor",
        }
    }
}

impl FromStr for WorkerKind {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, ()> {
        Ok(match s {
            "implementer" => Self::Implementer,
            "reviewer" => Self::Reviewer,
            "planner" => Self::Planner,
            "analyzer" => Self::Analyzer,
            "griller" => Self::Griller,
            "glossarist" => Self::Glossarist,
            "manager" => Self::Manager,
            "artifactor" => Self::Artifactor,
            _ => return Err(()),
        })
    }
}

impl fmt::Display for WorkerKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// --- views ------------------------------------------------------------------

// orgasmic:arch_QFQTD
/// Project identity and authored prose, parsed from `.orgasmic/project.org`.
#[derive(Debug, Clone, Serialize)]
pub struct ProjectFile<'a> {
    pub id: &'a str,
    pub mission: Option<String>,
    pub operating_constraints: Option<String>,
}

impl<'a> ProjectFile<'a> {
    pub fn from_org(file: &'a OrgFile, display: &str) -> Result<Self, SchemaError> {
        let heading =
            file.find_by_title_prefix("PROJECT ")
                .ok_or_else(|| SchemaError::MissingSection {
                    file: display.into(),
                    heading: "PROJECT".into(),
                })?;
        let id = required(heading, "ID", display)?;
        Ok(Self {
            id,
            mission: section_body(file, heading, "Mission"),
            operating_constraints: section_body(file, heading, "Operating Constraints"),
        })
    }
}

// orgasmic:arch_QFQTD
#[derive(Debug, Clone, Serialize)]
pub struct TaskHeading<'a> {
    pub id: &'a str,
    pub title: &'a str,
    pub todo: Option<&'a str>,
    pub tags: &'a [String],
    pub lifecycle_stage: LifecycleStage,
    pub parent_task: Option<String>,
    pub priority: Option<&'a str>,
    pub provider: Option<&'a str>,
    pub model: Option<&'a str>,
    pub reasoning_effort: Option<&'a str>,
    pub write_scope: Vec<&'a str>,
    pub read_scope: Vec<&'a str>,
    pub produces: Vec<&'a str>,
    pub implements: Vec<&'a str>,
    pub depends_on: Vec<&'a str>,
    pub test_cmd: Option<&'a str>,
    pub sandbox_permissions: Option<SandboxAllowlist>,
    pub description: Option<String>,
    pub acceptance: Option<String>,
    pub evidence: Option<String>,
    pub worklog: Option<String>,
}

impl<'a> TaskHeading<'a> {
    pub fn from_heading(
        file: &'a OrgFile,
        heading: &'a Heading,
        display: &str,
    ) -> Result<Self, SchemaError> {
        let id = required(heading, "ID", display)?;
        let todo = heading
            .todo
            .as_deref()
            .ok_or_else(|| SchemaError::MissingLifecycleStage {
                file: display.into(),
                heading: id.into(),
            })?;
        let lifecycle_stage =
            LifecycleStage::from_str(todo).map_err(|_| SchemaError::UnknownLifecycleStage {
                file: display.into(),
                heading: id.into(),
                state: todo.into(),
            })?;
        let parent_task = derive_task_parent_id(id);
        if let Some(parent_task) = parent_task.as_deref() {
            if !is_valid_task_path_id(parent_task) {
                return Err(SchemaError::InvalidParentTask {
                    file: display.into(),
                    heading: id.into(),
                    parent_task: parent_task.into(),
                });
            }
        }
        // Tolerant parse (dec_HJENQ): when the heading title's leading ID token
        // disagrees with or omits `:ID:`, keep indexing under the drawer value
        // and surface drift via the read-time heading-token equality lint.
        let title = heading
            .title
            .strip_prefix(id)
            .map(|s| s.trim_start())
            .unwrap_or(&heading.title);
        Ok(Self {
            id,
            title,
            todo: heading.todo.as_deref(),
            tags: &heading.tags,
            lifecycle_stage,
            parent_task,
            priority: heading.property("PRIORITY"),
            provider: normalize_optional_property(heading.property("PROVIDER")),
            model: normalize_optional_property(heading.property("MODEL")),
            reasoning_effort: normalize_optional_property(heading.property("REASONING_EFFORT")),
            write_scope: tokenize(heading.property("WRITE_SCOPE")),
            read_scope: tokenize(heading.property("READ_SCOPE")),
            produces: tokenize(heading.property("PRODUCES")),
            implements: tokenize(heading.property("IMPLEMENTS")),
            depends_on: tokenize(heading.property("DEPENDS_ON")),
            test_cmd: heading.property("TEST_CMD"),
            sandbox_permissions: heading
                .property("SANDBOX_PERMISSIONS")
                .map(SandboxAllowlist::from_csv)
                .transpose()
                .map_err(|e| SchemaError::InvalidPropertyValue {
                    file: display.into(),
                    heading: id.into(),
                    key: "SANDBOX_PERMISSIONS".into(),
                    detail: e.to_string(),
                })?,
            description: section_body(file, heading, "Description"),
            acceptance: section_body(file, heading, "Acceptance Criteria"),
            evidence: section_body(file, heading, "Evidence"),
            worklog: section_body(file, heading, "Worklog"),
        })
    }
}

// orgasmic:arch_MPAQT
//
// A decision is an ADR-style record: a `dec_NNN` heading carrying a title +
// topic tags, an ADR property drawer, and `** Context` / `** Decision` /
// `** Consequences` prose. The old grilling Q&A shape (option variants,
// chosen/recommended, semantic hashes, generated-ADR bookkeeping) is gone.
#[derive(Debug, Clone, Serialize)]
pub struct DecisionNode<'a> {
    pub id: &'a str,
    pub title: &'a str,
    pub tags: &'a [String],
    pub parent: Option<String>,
    pub glossary_refs: Vec<&'a str>,
    pub decided_at: Option<&'a str>,
    pub source: Option<&'a str>,
    pub context: Option<String>,
    pub decision: Option<String>,
    pub consequences: Option<String>,
}

impl<'a> DecisionNode<'a> {
    pub fn from_heading(
        file: &'a OrgFile,
        heading: &'a Heading,
        display: &str,
    ) -> Result<Self, SchemaError> {
        let id = required(heading, "ID", display)?;
        // Tolerant parse (dec_HJENQ): `:ID:` is canonical; title token mismatch
        // is flagged at read-time, not rejected here.
        let title = heading
            .title
            .strip_prefix(id)
            .map(str::trim_start)
            .unwrap_or(&heading.title);
        Ok(Self {
            id,
            title,
            tags: &heading.tags,
            parent: parse_parent_value(NodeIdClass::Decision, id, heading.property("PARENT"))
                .map_err(|e| SchemaError::InvalidPropertyValue {
                    file: display.into(),
                    heading: id.into(),
                    key: "PARENT".into(),
                    detail: e.to_string(),
                })?,
            glossary_refs: tokenize(heading.property("GLOSSARY_REFS")),
            decided_at: heading.property("DECIDED_AT"),
            source: heading.property("SOURCE"),
            context: section_body(file, heading, "Context"),
            decision: section_body(file, heading, "Decision"),
            consequences: section_body(file, heading, "Consequences"),
        })
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct GlossaryTerm<'a> {
    pub id: &'a str,
    pub canonical: Option<&'a str>,
    pub avoid: Option<&'a str>,
    pub relates_to: Vec<&'a str>,
    pub definition: Option<&'a str>,
    pub decided_at: Option<&'a str>,
}

impl<'a> GlossaryTerm<'a> {
    pub fn from_heading(heading: &'a Heading, display: &str) -> Result<Self, SchemaError> {
        let id = required(heading, "ID", display)?;
        Ok(Self {
            id,
            canonical: heading.property("CANONICAL"),
            avoid: heading.property("AVOID"),
            relates_to: tokenize(heading.property("RELATES_TO")),
            definition: heading.property("DEFINITION"),
            decided_at: heading.property("DECIDED_AT"),
        })
    }
}
// orgasmic:arch_BVH7M, dec_R75SW
#[derive(Debug, Clone, Serialize)]
pub struct TxHeadingView<'a> {
    pub tx_id: &'a str,
    pub time: &'a str,
    pub ty: &'a str,
    pub actor: &'a str,
    pub machine: &'a str,
    pub project: Option<&'a str>,
    pub task: Option<&'a str>,
    pub target: Option<&'a str>,
    pub reason: Option<&'a str>,
    pub extras: Vec<(&'a str, &'a str)>,
}

impl<'a> TxHeadingView<'a> {
    pub fn from_heading(heading: &'a Heading, display: &str) -> Result<Self, SchemaError> {
        const KNOWN: &[&str] = &[
            "TX_ID", "TIME", "TYPE", "ACTOR", "MACHINE", "PROJECT", "TASK", "TARGET", "REASON",
        ];
        let tx_id = required(heading, "TX_ID", display)?;
        let time = required(heading, "TIME", display)?;
        let ty = required(heading, "TYPE", display)?;
        let actor = required(heading, "ACTOR", display)?;
        let machine = required(heading, "MACHINE", display)?;
        let extras = heading
            .property_entries()
            .filter(|e| !KNOWN.contains(&e.key.as_str()))
            .map(|e| (e.key.as_str(), e.value.as_str()))
            .collect();
        Ok(Self {
            tx_id,
            time,
            ty,
            actor,
            machine,
            project: heading.property("PROJECT"),
            task: heading.property("TASK"),
            target: heading.property("TARGET"),
            reason: heading.property("REASON"),
            extras,
        })
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SkillMetadata<'a> {
    pub id: &'a str,
    pub title: &'a str,
    pub description: Option<&'a str>,
    pub triggers: Vec<&'a str>,
    pub absolute_path: Option<&'a str>,
}

impl<'a> SkillMetadata<'a> {
    pub fn from_heading(heading: &'a Heading, display: &str) -> Result<Self, SchemaError> {
        let id = required(heading, "ID", display)?;
        Ok(Self {
            id,
            title: &heading.title,
            description: heading.property("DESCRIPTION"),
            triggers: tokenize(heading.property("TRIGGERS")),
            absolute_path: heading.property("ABSOLUTE_PATH"),
        })
    }
}

// --- helpers ----------------------------------------------------------------

pub(crate) fn required<'a>(
    heading: &'a Heading,
    key: &str,
    display: &str,
) -> Result<&'a str, SchemaError> {
    heading
        .property(key)
        .ok_or_else(|| SchemaError::MissingProperty {
            file: display.into(),
            heading: heading.title.clone(),
            key: key.into(),
        })
}

fn normalize_optional_property(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

pub(crate) fn section_body(file: &OrgFile, heading: &Heading, title: &str) -> Option<String> {
    heading
        .section(title)
        .map(|s| file.slice(s.body.clone()).to_string())
}

pub(crate) fn tokenize(value: Option<&str>) -> Vec<&str> {
    value
        .map(|v| v.split_whitespace().collect())
        .unwrap_or_default()
}

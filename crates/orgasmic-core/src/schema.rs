// arch: arch_BVH7M.1, arch_MPAQT.1, arch_MPAQT.2, arch_QFQTD.2, arch_QXS5W.1
// orgasmic:arch_QFQTD, arch_QXS5W
//! Typed views on top of the [`crate::org`] parser.
//!
//! These wrappers project the strict orgasmic profile onto Rust types.
//! Downstream crates use these for daemon projection, manager prompts, and
//! graph operations.

use crate::id::{
    derive_task_parent_id, is_arch_id, is_valid_task_path_id, parse_parent_value, NodeIdClass,
};

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

/// The single role vocabulary: what a worker can do. Tasks reference workers
/// (`:WORKER:`), runs reference the worker resolved at dispatch — kind is
/// always derived through the worker, never stored on tasks or runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerKind {
    Implementer,
    Reviewer,
    Planner,
    Analyzer,
    Architector,
    Griller,
    Glossarist,
    Babysitter,
    Manager,
    Artifactor,
}

impl FromStr for WorkerKind {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, ()> {
        Ok(match s {
            "implementer" => Self::Implementer,
            "reviewer" => Self::Reviewer,
            "planner" => Self::Planner,
            "analyzer" => Self::Analyzer,
            "architector" => Self::Architector,
            "griller" => Self::Griller,
            "glossarist" => Self::Glossarist,
            "babysitter" => Self::Babysitter,
            "manager" => Self::Manager,
            "artifactor" => Self::Artifactor,
            _ => return Err(()),
        })
    }
}

// --- views ------------------------------------------------------------------

// orgasmic:arch_QFQTD
/// Project identity and authored prose, parsed from `.orgasmic/project.org`.
/// Machine config (dispatch + build) lives in [`ProjectConfig`] / `config.org`
/// (dec_051).
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
/// Machine configuration for a project, parsed from `.orgasmic/config.org`.
/// Holds the ordered worker pipeline and build commands kept out of the
/// identity-and-prose `project.org` (dec_051).
#[derive(Debug, Clone, Serialize)]
pub struct ProjectConfig<'a> {
    pub id: &'a str,
    pub default_branch: Option<&'a str>,
    pub test_cmd: Option<&'a str>,
    pub lint_cmd: Option<&'a str>,
    pub build_cmd: Option<&'a str>,
    pub write_scope: Vec<&'a str>,
    pub worker_pipeline: Vec<String>,
}

impl<'a> ProjectConfig<'a> {
    pub fn from_org(file: &'a OrgFile, display: &str) -> Result<Self, SchemaError> {
        let heading =
            file.find_by_title_prefix("CONFIG ")
                .ok_or_else(|| SchemaError::MissingSection {
                    file: display.into(),
                    heading: "CONFIG".into(),
                })?;
        let id = required(heading, "ID", display)?;
        Ok(Self {
            id,
            default_branch: heading.property("DEFAULT_BRANCH"),
            test_cmd: heading.property("TEST_CMD"),
            lint_cmd: heading.property("LINT_CMD"),
            build_cmd: heading.property("BUILD_CMD"),
            write_scope: tokenize(heading.property("WRITE_SCOPE")),
            worker_pipeline: tokenize(heading.property("PIPELINE"))
                .into_iter()
                .map(str::to_string)
                .collect(),
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
    pub worker: Option<&'a str>,
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
            worker: property_with_legacy(heading, "WORKER", "AGENT_TEMPLATE"),
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

// orgasmic:arch_MPAQT
#[derive(Debug, Clone, Serialize)]
pub struct ArchitectureNode<'a> {
    pub id: &'a str,
    pub label: String,
    pub kind: Option<&'a str>,
    pub semantic_hash: Option<&'a str>,
    pub motivated_by: Vec<&'a str>,
    pub glossary_refs: Vec<&'a str>,
    pub interface: Vec<&'a str>,
    pub constraints: Vec<&'a str>,
    pub composes: Vec<&'a str>,
    pub depends_on: Vec<&'a str>,
    pub source_paths: Vec<&'a str>,
    /// Per-node test commands an agent should run when touching this node's
    /// `source_paths`, parsed from the `:TESTS:` property. Commands are
    /// `;`-separated in the org source; each element here is one trimmed
    /// command. Empty for top-level (non-leaf) nodes.
    pub tests: Vec<String>,
    pub edges: Vec<ArchEdge>,
    pub parent_id: Option<String>,
    pub description: Option<String>,
    pub decided_at: Option<&'a str>,
    pub purpose: Option<String>,
    pub interfaces_section: Option<String>,
    pub constraints_section: Option<String>,
}

pub type ArchNode<'a> = ArchitectureNode<'a>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArchEdgeKind {
    Reads,
    Writes,
    ExposesRest,
    ExposesWs,
    SubscribesTo,
    Spawns,
    Calls,
    DependsOn,
}

impl ArchEdgeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Reads => "reads",
            Self::Writes => "writes",
            Self::ExposesRest => "exposes_rest",
            Self::ExposesWs => "exposes_ws",
            Self::SubscribesTo => "subscribes_to",
            Self::Spawns => "spawns",
            Self::Calls => "calls",
            Self::DependsOn => "depends_on",
        }
    }

    fn property_key(self) -> &'static str {
        match self {
            Self::Reads => "READS",
            Self::Writes => "WRITES",
            Self::ExposesRest => "EXPOSES_REST",
            Self::ExposesWs => "EXPOSES_WS",
            Self::SubscribesTo => "SUBSCRIBES_TO",
            Self::Spawns => "SPAWNS",
            Self::Calls => "CALLS",
            Self::DependsOn => "DEPENDS_ON",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArchEdgeTarget {
    Node { id: String },
    Artifact(ArtifactNode),
}

impl ArchEdgeTarget {
    pub fn id(&self) -> String {
        match self {
            Self::Node { id } => id.clone(),
            Self::Artifact(artifact) => artifact.id(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchEdge {
    pub kind: ArchEdgeKind,
    pub source_node_id: String,
    pub target: ArchEdgeTarget,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactScheme {
    File,
    Projection,
    Socket,
}

impl ArtifactScheme {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Projection => "projection",
            Self::Socket => "socket",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ArtifactNode {
    pub scheme: ArtifactScheme,
    pub name: String,
}

impl ArtifactNode {
    pub fn id(&self) -> String {
        format!("{}:{}", self.scheme.as_str(), self.name)
    }
}

impl<'a> ArchitectureNode<'a> {
    pub fn from_org(file: &'a OrgFile, display: &str) -> Result<Vec<Self>, SchemaError> {
        let mut nodes = Vec::new();
        for heading in &file.headings {
            if !heading.title.starts_with("arch_") {
                continue;
            }
            let node = Self::from_heading(file, heading, display)?;
            let parent_id = node.id.to_string();
            nodes.push(node);
            for child in &heading.sections {
                if !child.title.starts_with("arch_") {
                    continue;
                }
                let child_node =
                    Self::from_child_heading(file, child, display, parent_id.as_str())?;
                nodes.push(child_node);
            }
        }
        Ok(nodes)
    }

    pub fn from_heading(
        file: &'a OrgFile,
        heading: &'a Heading,
        display: &str,
    ) -> Result<Self, SchemaError> {
        Self::from_arch_heading(file, heading, display, None)
    }

    fn from_child_heading(
        file: &'a OrgFile,
        heading: &'a Heading,
        display: &str,
        parent_id: &str,
    ) -> Result<Self, SchemaError> {
        let node = Self::from_arch_heading(file, heading, display, Some(parent_id.to_string()))?;
        let expected_prefix = format!("{parent_id}.");
        if !node.id.starts_with(&expected_prefix) {
            return Err(SchemaError::InvalidPropertyValue {
                file: display.into(),
                heading: node.id.into(),
                key: "ID".into(),
                detail: format!("child architecture id must start with {expected_prefix}"),
            });
        }
        Ok(node)
    }

    fn from_arch_heading(
        file: &'a OrgFile,
        heading: &'a Heading,
        display: &str,
        parent_id: Option<String>,
    ) -> Result<Self, SchemaError> {
        let id = architecture_heading_id(heading, display)?;
        // Tolerant parse (dec_HJENQ): `:ID:` wins for indexing when the title
        // token drifts; equality is enforced by read-time lint.
        let label = heading
            .title
            .strip_prefix(id)
            .map(str::trim_start)
            .unwrap_or(&heading.title)
            .to_string();
        let edges = parse_arch_edges(heading, id, display)?;
        Ok(Self {
            id,
            label,
            kind: heading.property("KIND"),
            semantic_hash: heading.property("SEMANTIC_HASH"),
            motivated_by: tokenize(heading.property("MOTIVATED_BY")),
            glossary_refs: tokenize(heading.property("GLOSSARY_REFS")),
            interface: tokenize(heading.property("INTERFACE")),
            constraints: tokenize(heading.property("CONSTRAINTS")),
            composes: tokenize(heading.property("COMPOSES")),
            depends_on: tokenize(heading.property("DEPENDS_ON")),
            source_paths: tokenize(heading.property("SOURCE_PATHS")),
            tests: tokenize_commands(heading.property("TESTS")),
            edges,
            parent_id,
            description: direct_body(file, heading),
            decided_at: heading.property("DECIDED_AT"),
            purpose: section_body(file, heading, "Purpose"),
            interfaces_section: section_body(file, heading, "Interfaces"),
            constraints_section: section_body(file, heading, "Constraints"),
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

const ARCH_EDGE_KINDS: &[ArchEdgeKind] = &[
    ArchEdgeKind::Reads,
    ArchEdgeKind::Writes,
    ArchEdgeKind::ExposesRest,
    ArchEdgeKind::ExposesWs,
    ArchEdgeKind::SubscribesTo,
    ArchEdgeKind::Spawns,
    ArchEdgeKind::Calls,
    ArchEdgeKind::DependsOn,
];

fn architecture_heading_id<'a>(
    heading: &'a Heading,
    display: &str,
) -> Result<&'a str, SchemaError> {
    if let Some(id) = heading.property("ID") {
        if is_arch_id(id) {
            return Ok(id);
        }
        return Err(SchemaError::InvalidPropertyValue {
            file: display.into(),
            heading: heading.title.clone(),
            key: "ID".into(),
            detail: "expected arch_NNN, arch_XXXXX, or arch_NNN.M / arch_XXXXX.M".into(),
        });
    }
    let id = heading.title.split_whitespace().next().unwrap_or("");
    if is_arch_id(id) {
        return Ok(id);
    }
    Err(SchemaError::MissingProperty {
        file: display.into(),
        heading: heading.title.clone(),
        key: "ID".into(),
    })
}

fn parse_arch_edges(
    heading: &Heading,
    source_node_id: &str,
    display: &str,
) -> Result<Vec<ArchEdge>, SchemaError> {
    let mut edges = Vec::new();
    for kind in ARCH_EDGE_KINDS {
        let key = kind.property_key();
        for value in tokenize(heading.property(key)) {
            let target = parse_arch_edge_target(value, heading, key, display)?;
            edges.push(ArchEdge {
                kind: *kind,
                source_node_id: source_node_id.to_string(),
                target,
            });
        }
    }
    Ok(edges)
}

fn parse_arch_edge_target(
    value: &str,
    heading: &Heading,
    key: &str,
    display: &str,
) -> Result<ArchEdgeTarget, SchemaError> {
    if is_arch_id(value) {
        return Ok(ArchEdgeTarget::Node {
            id: value.to_string(),
        });
    }
    if let Some((scheme, name)) = value.split_once(':') {
        if name.is_empty() {
            return Err(SchemaError::InvalidPropertyValue {
                file: display.into(),
                heading: heading.title.clone(),
                key: key.into(),
                detail: "artifact pseudo-node name cannot be empty".into(),
            });
        }
        let scheme = match scheme {
            "file" => ArtifactScheme::File,
            "projection" => ArtifactScheme::Projection,
            "socket" => ArtifactScheme::Socket,
            _ => {
                return Err(SchemaError::InvalidPropertyValue {
                    file: display.into(),
                    heading: heading.title.clone(),
                    key: key.into(),
                    detail: format!("unknown architecture namespace {scheme}:"),
                });
            }
        };
        return Ok(ArchEdgeTarget::Artifact(ArtifactNode {
            scheme,
            name: name.to_string(),
        }));
    }
    Err(SchemaError::InvalidPropertyValue {
        file: display.into(),
        heading: heading.title.clone(),
        key: key.into(),
        detail: format!("expected arch id or artifact pseudo-node, got {value}"),
    })
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

fn property_with_legacy<'a>(heading: &'a Heading, current: &str, legacy: &str) -> Option<&'a str> {
    if let Some(value) = heading.property(current) {
        return Some(value);
    }
    let value = heading.property(legacy);
    if value.is_some() {
        tracing::warn!(
            legacy_property = legacy,
            current_property = current,
            heading = %heading.title,
            "legacy orgasmic property parsed"
        );
    }
    value
}

fn normalize_optional_property(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

pub(crate) fn section_body(file: &OrgFile, heading: &Heading, title: &str) -> Option<String> {
    heading
        .section(title)
        .map(|s| file.slice(s.body.clone()).to_string())
}

fn direct_body(file: &OrgFile, heading: &Heading) -> Option<String> {
    let body = file.slice(heading.body.clone()).trim();
    if body.is_empty() {
        None
    } else {
        Some(body.to_string())
    }
}

pub(crate) fn tokenize(value: Option<&str>) -> Vec<&str> {
    value
        .map(|v| v.split_whitespace().collect())
        .unwrap_or_default()
}

/// Split a `;`-separated command list (e.g. the `:TESTS:` property) into
/// trimmed, non-empty commands. Unlike [`tokenize`], this preserves
/// intra-command whitespace so `cargo test -p orgasmic-core` stays one entry.
pub(crate) fn tokenize_commands(value: Option<&str>) -> Vec<String> {
    value
        .map(|v| {
            v.split(';')
                .map(str::trim)
                .filter(|cmd| !cmd.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

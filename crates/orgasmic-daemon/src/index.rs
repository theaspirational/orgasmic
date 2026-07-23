// arch: arch_C87Z9.4, arch_MPAQT.1, arch_MPAQT.2, arch_PCSQE.2, arch_QFQTD.2, arch_R3EPE.2
// orgasmic:arch_C87Z9, dec_YYMSK
//! In-memory materialized projection of board, projects, tasks, and tx.
//!
//! Rebuilt from disk at boot before the daemon serves normal reads
//! (AC #1). When a working file fails to parse, the last-good projection
//! for that file is kept and the error is reported through the index's
//! `parse_errors` map (AC #2 + dec_022).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use orgasmic_core::tx::{parse_tx_file, TxEntry, TxError};
use orgasmic_core::{
    iter_task_file_paths, lint_arch_heading_id_token, lint_decision_heading_id_token,
    lint_project_identities, lint_task_heading_id_token, marker_node_ids_in_line,
    should_skip_marker_path, validate_parent_tree, ArchEdgeTarget, ArchitectureNode, DecisionNode,
    GlossaryTerm, Heading, Home, LifecycleStage, NodeIdClass, OrgError, OrgFile, ParentTreeError,
    ParentTreeNode, SandboxAllowlist, TaskHeading,
};
use serde::{Serialize, Serializer};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::RwLock;
use tracing::warn;

use crate::artifacts::{load_project_artifacts, ArtifactSummary};
use crate::events::{EventBus, EventPayload};

/// One project's materialized state.
#[derive(Debug, Clone, Serialize)]
pub struct ProjectIndex {
    pub project_id: String,
    pub root: PathBuf,
    pub repo_url: String,
    pub branch: String,
    pub status: String,
    pub tasks: Vec<TaskSummary>,
    #[serde(skip)]
    pub task_bodies: BTreeMap<TaskId, TaskBody>,
    pub subtasks: BTreeMap<TaskId, Vec<TaskId>>,
    pub activity_index: BTreeMap<TaskId, Vec<ActivityEntry>>,
    pub graph: GraphIndex,
    pub markers: BTreeMap<String, Vec<PathBuf>>,
    /// Per-file parse errors with the source path that failed and the
    /// last-good content count.
    pub last_loaded_at: Option<DateTime<Utc>>,
    pub artifacts: Vec<ArtifactSummary>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TaskSummary {
    pub id: String,
    pub title: String,
    pub lifecycle_stage: LifecycleStage,
    pub parent_task: Option<String>,
    pub depends_on: Vec<String>,
    pub implements: Vec<String>,
    pub produces: Vec<String>,
    pub read_scope: Vec<String>,
    pub write_scope: Vec<String>,
    pub owner: TaskOwner,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    pub priority: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub test_cmd: Option<String>,
    pub tags: Vec<String>,
    pub source_file: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sandbox_permissions: Option<SandboxAllowlist>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TaskDetail {
    #[serde(flatten)]
    pub summary: TaskSummary,
    pub body: TaskBody,
}

impl TaskDetail {
    pub(crate) fn from_indexed_body(summary: TaskSummary, body: Option<TaskBody>) -> Self {
        Self {
            body: body.unwrap_or_default(),
            summary,
        }
    }
}

#[derive(Debug, Default, Clone, Serialize, PartialEq, Eq)]
pub struct TaskBody {
    pub description: String,
    pub acceptance_criteria: Vec<AcceptanceItem>,
    pub evidence: Vec<String>,
    pub notes: String,
    pub worklog: Vec<String>,
    pub reviewer_pass: Vec<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct AcceptanceItem {
    pub state: AcceptanceState,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AcceptanceState {
    Checked,
    Partial,
    Unchecked,
}

pub type TaskId = String;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskOwner {
    Human,
    Agent(String),
}

impl Serialize for TaskOwner {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Human => serializer.serialize_str("human"),
            Self::Agent(kind) if kind.starts_with("agent.") => serializer.serialize_str(kind),
            Self::Agent(kind) => serializer.serialize_str(&format!("agent.{kind}")),
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ActivityEntry {
    pub tx_id: String,
    pub time: String,
    pub kind: ActivityKind,
    pub actor: String,
    pub body: String,
    pub artifacts: Vec<String>,
    pub in_reply_to: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ActivityKind {
    Comment,
    StateTransition,
    RunLifecycle,
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct GraphIndex {
    pub decisions: Vec<DecisionSummary>,
    pub decision_tree: BTreeMap<String, DecisionTreeEntry>,
    pub architecture: Vec<ArchitectureSummary>,
    pub architecture_artifacts: Vec<ArchitectureArtifactSummary>,
    pub edges: Vec<GraphEdgeSummary>,
    pub architecture_edges: Vec<ArchitectureEdgeSummary>,
    pub glossary: Vec<GlossarySummary>,
    pub nodes: Vec<GraphNodeSummary>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DecisionSummary {
    pub id: String,
    pub title: String,
    pub tags: Vec<String>,
    pub parent: Option<String>,
    pub children: Vec<String>,
    pub depth: Option<usize>,
    pub path: Option<String>,
    pub glossary_refs: Vec<String>,
    pub decided_at: Option<String>,
    /// Short body excerpt (Decision, falling back to Context) for list previews,
    /// so the UI never re-parses the `.org` file for row rendering.
    pub preview: Option<String>,
    pub source_file: PathBuf,
    /// Derived from :SUPERSEDES: backrefs across all present decisions (dec_KTF04).
    /// True iff some other present decision's :SUPERSEDES: names this id.
    pub superseded: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DecisionTreeEntry {
    pub parent: Option<String>,
    pub children: Vec<String>,
    pub depth: usize,
    pub path: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ArchitectureSummary {
    pub id: String,
    pub label: String,
    pub motivated_by: Vec<String>,
    pub glossary_refs: Vec<String>,
    pub interface: Vec<String>,
    pub constraints: Vec<String>,
    pub depends_on: Vec<String>,
    pub source_paths: Vec<String>,
    pub tests: Vec<String>,
    pub parent_id: Option<String>,
    pub description: Option<String>,
    pub source_file: PathBuf,
}

#[derive(Debug, Clone, Serialize)]
pub struct ArchitectureArtifactSummary {
    pub id: String,
    pub kind: String,
    pub scheme: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct GraphEdgeSummary {
    pub kind: String,
    pub from: String,
    pub to: String,
}

pub type ArchitectureEdgeSummary = GraphEdgeSummary;

#[derive(Debug, Clone, Serialize)]
pub struct ArchitectureNodesResponse {
    pub nodes: Vec<ArchitectureGraphNode>,
    pub edges: Vec<ArchitectureEdgeSummary>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ArchitectureGraphNode {
    pub id: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub source_paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tests: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scheme: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl GraphIndex {
    pub fn architecture_nodes_response(&self) -> ArchitectureNodesResponse {
        let mut nodes = Vec::new();
        for node in &self.architecture {
            nodes.push(ArchitectureGraphNode {
                id: node.id.clone(),
                kind: "arch".to_string(),
                label: Some(node.label.clone()),
                parent_id: node.parent_id.clone(),
                source_paths: node.source_paths.clone(),
                tests: node.tests.clone(),
                scheme: None,
                name: None,
            });
        }
        for artifact in &self.architecture_artifacts {
            nodes.push(ArchitectureGraphNode {
                id: artifact.id.clone(),
                kind: artifact.kind.clone(),
                label: None,
                parent_id: None,
                source_paths: Vec::new(),
                tests: Vec::new(),
                scheme: Some(artifact.scheme.clone()),
                name: Some(artifact.name.clone()),
            });
        }
        ArchitectureNodesResponse {
            nodes,
            edges: self.architecture_edges.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct GlossarySummary {
    pub id: String,
    pub canonical: Option<String>,
    pub avoid: Option<String>,
    pub relates_to: Vec<String>,
    pub definition: Option<String>,
    pub source_file: PathBuf,
}

#[derive(Debug, Clone, Serialize)]
pub struct GraphNodeSummary {
    pub id: String,
    pub layer: String,
    pub outgoing: Vec<String>,
    pub source_file: PathBuf,
    /// False for all non-decision layers. For decision nodes: true iff this
    /// decision is a target of some present decision's :SUPERSEDES: (dec_KTF04).
    pub superseded: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct BoardEntry {
    pub id: String,
    pub path: PathBuf,
    pub branch: String,
    pub status: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ParseErrorKind {
    WorkingFile,
    HistoricalTx,
}

#[derive(Debug, Clone, Serialize)]
pub struct ParseError {
    pub path: PathBuf,
    pub kind: ParseErrorKind,
    pub message: String,
    pub line: Option<usize>,
    pub at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TxRecord {
    pub project_id: Option<String>,
    pub source_path: PathBuf,
    pub entry: TxEntry,
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct IndexSnapshot {
    pub board: Vec<BoardEntry>,
    pub projects: HashMap<String, ProjectIndex>,
    pub tx: Vec<TxRecord>,
    pub parse_errors: Vec<ParseError>,
    pub rebuilt_at: Option<DateTime<Utc>>,
}

impl IndexSnapshot {
    pub fn project(&self, id: &str) -> Option<&ProjectIndex> {
        self.projects.get(id)
    }

    pub fn task(&self, project_id: &str, task_id: &str) -> Option<&TaskSummary> {
        self.projects
            .get(project_id)
            .and_then(|p| p.tasks.iter().find(|t| t.id == task_id))
    }

    pub fn first_historical_tx_parse_error(&self) -> Option<&ParseError> {
        self.parse_errors
            .iter()
            .find(|error| matches!(error.kind, ParseErrorKind::HistoricalTx))
    }

    pub fn marker_files(&self, node_id: &str) -> Vec<PathBuf> {
        let mut files = BTreeSet::new();
        for project in self.projects.values() {
            if let Some(project_files) = project.markers.get(node_id) {
                files.extend(project_files.iter().cloned());
            }
        }
        files.into_iter().collect()
    }

    /// Project owning `error.path`, derived by prefix match against each
    /// project's root (TASK-V8WY9: `parse_errors` carries no `project_id`
    /// field, so attribution is derived at query time rather than threaded
    /// through every `push_parse_error` call site). `None` for board- or
    /// home-tx-level errors that aren't under any registered project root.
    pub fn parse_error_project_id(&self, error: &ParseError) -> Option<&str> {
        self.projects
            .values()
            .find(|project| is_under(&error.path, &project.root))
            .map(|project| project.project_id.as_str())
    }

    /// Per-project parse-error counts for the current snapshot (arch_C87Z9.4
    /// / TASK-V8WY9): lets `reindex` report fresh per-project counts without
    /// a daemon restart.
    pub fn parse_error_counts_by_project(&self) -> BTreeMap<String, usize> {
        let mut counts: BTreeMap<String, usize> = self
            .projects
            .keys()
            .map(|id| (id.clone(), 0usize))
            .collect();
        for error in &self.parse_errors {
            if let Some(project_id) = self.parse_error_project_id(error) {
                *counts.entry(project_id.to_string()).or_insert(0) += 1;
            }
        }
        counts
    }
}

#[derive(Debug, Clone)]
pub struct Index {
    inner: Arc<RwLock<IndexSnapshot>>,
    home: Home,
    repo_url_refresh_enabled: Arc<AtomicBool>,
    #[cfg(test)]
    git_spawn_attempts: Arc<std::sync::atomic::AtomicUsize>,
}

impl Index {
    pub fn new(home: Home) -> Self {
        Self {
            inner: Arc::new(RwLock::new(IndexSnapshot::default())),
            home,
            repo_url_refresh_enabled: Arc::new(AtomicBool::new(false)),
            #[cfg(test)]
            git_spawn_attempts: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    pub async fn snapshot(&self) -> IndexSnapshot {
        self.inner.read().await.clone()
    }

    pub async fn home_root(&self) -> &Path {
        &self.home.root
    }

    pub fn spawn_tx_listener(&self, events: EventBus) {
        let index = self.clone();
        tokio::spawn(async move {
            let mut rx = events.subscribe();
            while let Ok(event) = rx.recv().await {
                let EventPayload::TxAppended { project_id, .. } = event.payload else {
                    continue;
                };
                if let Some(project_id) = project_id {
                    let _ = index.refresh_project(&project_id).await;
                } else {
                    index.refresh_home_tx().await;
                }
            }
        });
    }

    /// Rebuild from disk. Called at boot before the daemon serves normal
    /// reads (arch_006 / AC #1) and on watcher-driven refresh.
    pub async fn rebuild(&self) {
        self.rebuild_with_timeout(Duration::from_secs(5)).await;
    }

    async fn rebuild_with_timeout(&self, timeout: Duration) {
        // Home-owned state is small and must remain available even when one
        // registered project is on a filesystem that stalls. Move project
        // traversal to the blocking pool and bound the wait so boot can bind.
        // A live rebuild starts a fresh snapshot, so carry Git-backed metadata
        // forward explicitly until its post-bind refresh can replace it.
        let prior_repo_urls: BTreeMap<String, String> = self
            .inner
            .read()
            .await
            .projects
            .iter()
            .map(|(id, project)| (id.clone(), project.repo_url.clone()))
            .collect();
        let mut base = IndexSnapshot {
            rebuilt_at: Some(Utc::now()),
            ..IndexSnapshot::default()
        };
        self.load_board(&mut base);
        self.load_home_tx(&mut base);
        let board = base.board.clone();
        let mut snap = base;
        for entry in board {
            let scan_index = self.clone();
            let scan_seed = snap.clone();
            let scan_entry = entry.clone();
            let prior_repo_url = prior_repo_urls.get(&entry.id).cloned();
            let scan = tokio::task::spawn_blocking(move || {
                let mut next = scan_seed;
                scan_index.load_project(&scan_entry, &mut next, prior_repo_url);
                next
            });
            match tokio::time::timeout(timeout, scan).await {
                Ok(Ok(next)) => snap = next,
                Ok(Err(error)) => {
                    warn!(
                        project = %entry.id,
                        path = %entry.path.display(),
                        error = %error,
                        "project index scan task failed; omitting project from this index pass"
                    );
                }
                Err(_) => {
                    warn!(
                        project = %entry.id,
                        path = %entry.path.display(),
                        timeout_secs = timeout.as_secs_f64(),
                        "project index scan timed out; omitting project so daemon boot can continue"
                    );
                    warn_macos_files_access_timeout(std::slice::from_ref(&entry.path));
                }
            }
        }
        rebuild_all_activity_indexes(&mut snap);
        *self.inner.write().await = snap;
        if self.repo_url_refresh_enabled.load(Ordering::Acquire) {
            self.spawn_repo_url_refreshes();
        }
    }

    /// Enable and resolve repository metadata after the listener is bound.
    /// Until this method is called, neither boot scans nor watcher refreshes
    /// may spawn Git.
    pub fn spawn_repo_url_refresh(&self) {
        self.repo_url_refresh_enabled.store(true, Ordering::Release);
        self.spawn_repo_url_refreshes();
    }

    fn spawn_repo_url_refreshes(&self) {
        let index = self.clone();
        tokio::spawn(async move {
            index.refresh_repo_urls().await;
        });
    }

    async fn refresh_repo_urls(&self) {
        let targets: Vec<(String, PathBuf)> = {
            let snap = self.inner.read().await;
            snap.projects
                .iter()
                .map(|(id, project)| (id.clone(), project.root.clone()))
                .collect()
        };
        for (project_id, project_root) in targets {
            self.refresh_repo_url(&project_id, &project_root).await;
        }
    }

    /// Refresh one project after a watcher event. Preserves the rest of the
    /// snapshot; on parse failure, keeps the last-good project entry.
    pub async fn refresh_project(&self, project_id: &str) -> Result<(), String> {
        let snap_read = self.inner.read().await;
        let Some(prior_board) = snap_read.board.iter().find(|e| e.id == project_id).cloned() else {
            return Err(format!("unknown project {project_id}"));
        };
        drop(snap_read);
        let mut snap = self.inner.write().await;
        snap.parse_errors
            .retain(|e| !is_under(&e.path, &prior_board.path));
        snap.tx
            .retain(|r| r.project_id.as_deref() != Some(project_id));
        let entry = prior_board.clone();
        self.load_project(&entry, &mut snap, None);
        let project_root = entry.path.clone();
        drop(snap);
        if self.repo_url_refresh_enabled.load(Ordering::Acquire) {
            self.refresh_repo_url(project_id, &project_root).await;
        }
        Ok(())
    }

    async fn refresh_repo_url(&self, project_id: &str, project_root: &Path) {
        if !project_root.join(".git").exists() {
            return;
        }
        #[cfg(test)]
        self.git_spawn_attempts.fetch_add(1, Ordering::Relaxed);
        let repo_url = git_remote_origin_url_with_program(
            project_root,
            OsStr::new("git"),
            Duration::from_secs(3),
        )
        .await;
        let Some(repo_url) = repo_url else {
            return;
        };
        if let Some(project) = self.inner.write().await.projects.get_mut(project_id) {
            project.repo_url = repo_url;
        }
    }

    pub async fn refresh_board(&self) {
        let mut snap = self.inner.write().await;
        snap.board.clear();
        self.load_board(&mut snap);
    }

    pub async fn refresh_home_tx(&self) {
        let mut snap = self.inner.write().await;
        snap.tx.retain(|r| r.project_id.is_some());
        snap.parse_errors
            .retain(|e| !is_under(&e.path, &self.home.tx()));
        self.load_home_tx(&mut snap);
        rebuild_all_activity_indexes(&mut snap);
    }

    fn load_board(&self, snap: &mut IndexSnapshot) {
        let path = self.home.board();
        if !path.exists() {
            return;
        }
        match read_org(&path) {
            Ok(file) => {
                for h in &file.headings {
                    let id = h.property("ID").unwrap_or("").to_string();
                    if id.is_empty() {
                        continue;
                    }
                    snap.board.push(BoardEntry {
                        id,
                        path: PathBuf::from(
                            h.property("PATH")
                                .or_else(|| h.property("LOCAL_PATH"))
                                .unwrap_or(""),
                        ),
                        branch: h
                            .property("BRANCH")
                            .or_else(|| h.property("DEFAULT_BRANCH"))
                            .unwrap_or("")
                            .to_string(),
                        status: h.property("STATUS").unwrap_or("active").to_string(),
                    });
                }
            }
            Err(err) => {
                if !parse_error_already_recorded(snap, &path, &err) {
                    warn!(path = %path.display(), error = %err, "board parse failed");
                    snap.parse_errors.push(ParseError {
                        path,
                        kind: ParseErrorKind::WorkingFile,
                        message: err,
                        line: None,
                        at: Utc::now(),
                    });
                }
            }
        }
    }

    fn load_project(
        &self,
        board_entry: &BoardEntry,
        snap: &mut IndexSnapshot,
        prior_repo_url: Option<String>,
    ) {
        let prior_repo_url = prior_repo_url.unwrap_or_else(|| {
            snap.projects
                .get(&board_entry.id)
                .map(|project| project.repo_url.clone())
                .unwrap_or_default()
        });
        let project = ProjectIndex {
            project_id: board_entry.id.clone(),
            root: board_entry.path.clone(),
            // Git is authoritative for config syntax, includes, and worktree
            // layout. Initial resolution happens asynchronously after bind;
            // watcher scans retain the last known value until Git responds.
            repo_url: prior_repo_url,
            branch: board_entry.branch.clone(),
            status: board_entry.status.clone(),
            tasks: Vec::new(),
            task_bodies: BTreeMap::new(),
            subtasks: BTreeMap::new(),
            activity_index: BTreeMap::new(),
            graph: GraphIndex::default(),
            markers: BTreeMap::new(),
            last_loaded_at: Some(Utc::now()),
            artifacts: Vec::new(),
        };
        let mut project = project;
        // goal.org carries no tasks, so it is not in the task-file iteration;
        // read it just for the thin-goal lint (stale liveness vestiges).
        let goal_path = orgasmic_core::goal_file_path(&board_entry.path);
        if goal_path.exists() {
            match read_org(&goal_path) {
                Ok(file) => lint_goal_liveness(&file, &goal_path, snap),
                Err(err) => push_parse_error(snap, goal_path, err),
            }
        }
        for path in iter_task_file_paths(&board_entry.path) {
            if !path.exists() {
                continue;
            }
            match read_org(&path) {
                Ok(file) => {
                    lint_phantom_task_headings(&file, &path, snap);
                    lint_task_heading_id_tokens(&file, &path, snap);
                    for h in &file.headings {
                        match parse_task(&file, h, &path) {
                            Ok(Some(t)) => {
                                project
                                    .task_bodies
                                    .insert(t.id.clone(), parse_task_body(&file, h));
                                project.tasks.push(t);
                            }
                            Ok(None) => {}
                            Err(err) => push_parse_error(snap, path.clone(), err.to_string()),
                        }
                    }
                }
                Err(err) => {
                    if !parse_error_already_recorded(snap, &path, &err) {
                        warn!(project = %board_entry.id, path = %path.display(), error = %err, "project file parse failed");
                        snap.parse_errors.push(ParseError {
                            path,
                            kind: ParseErrorKind::WorkingFile,
                            message: err,
                            line: None,
                            at: Utc::now(),
                        });
                    }
                }
            }
        }
        self.load_graph(board_entry, &mut project, snap);
        load_task_graph(&mut project);
        lint_dangling_graph_edges(&project, snap);
        let project_tx_dir = board_entry.path.join(".orgasmic").join("tx");
        if project_tx_dir.is_dir() {
            collect_tx_dir(&project_tx_dir, Some(board_entry.id.as_str()), snap);
        }
        project.subtasks = build_subtask_index(&project.tasks, &board_entry.path, snap);
        project.activity_index = build_activity_index(&board_entry.id, &snap.tx);
        project.markers = scan_project_markers(&board_entry.path);
        project.artifacts = load_project_artifacts(&board_entry.path);
        lint_project_identity_state(&board_entry.path, &project.markers, snap);
        let prior = snap.projects.insert(board_entry.id.clone(), project);
        // Last-good fallback: if we ended up with zero tasks but the prior
        // snapshot had some and the new parse hit errors, keep the prior.
        if let Some(prior) = prior {
            if snap
                .projects
                .get(&board_entry.id)
                .map(|p| p.tasks.is_empty())
                .unwrap_or(false)
                && !prior.tasks.is_empty()
                && snap
                    .parse_errors
                    .iter()
                    .any(|e| is_under(&e.path, &board_entry.path))
            {
                snap.projects.insert(board_entry.id.clone(), prior);
            }
        }
    }

    fn load_graph(
        &self,
        board_entry: &BoardEntry,
        project: &mut ProjectIndex,
        snap: &mut IndexSnapshot,
    ) {
        let orgasmic_dir = board_entry.path.join(".orgasmic");
        let decisions_path = orgasmic_dir.join("decisions.org");
        let mut all_superseded: HashSet<String> = HashSet::new();
        if decisions_path.exists() {
            match read_org(&decisions_path) {
                Ok(file) => {
                    lint_decision_heading_id_tokens(&file, &decisions_path, snap);
                    all_superseded.extend(load_decisions(
                        &file,
                        &decisions_path,
                        &mut project.graph,
                        snap,
                    ));
                }
                Err(err) => push_parse_error(snap, decisions_path, err),
            }
        }
        // Apply the superseded flag across the whole decision set from the
        // project-wide set of :SUPERSEDES: targets (dec_KTF04).
        apply_superseded_flags(&mut project.graph, &all_superseded);
        build_decision_tree_index(&mut project.graph, &board_entry.path, snap);

        let architecture = orgasmic_dir.join("architecture.org");
        if architecture.exists() {
            match read_org(&architecture) {
                Ok(file) => {
                    lint_arch_heading_id_tokens(&file, &architecture, snap);
                    if let Err(err) = load_architecture(&file, &architecture, &mut project.graph) {
                        push_parse_error(snap, architecture, err.to_string());
                    }
                }
                Err(err) => push_parse_error(snap, architecture, err),
            }
        }

        let glossary = orgasmic_dir.join("glossary.org");
        if glossary.exists() {
            match read_org(&glossary) {
                Ok(file) => load_glossary(&file, &glossary, &mut project.graph),
                Err(err) => push_parse_error(snap, glossary, err),
            }
        }
    }

    fn load_home_tx(&self, snap: &mut IndexSnapshot) {
        let dir = self.home.tx();
        if dir.is_dir() {
            collect_tx_dir(&dir, None, snap);
        }
    }
}

// orgasmic:dec_KTF04
fn load_decisions(
    file: &OrgFile,
    source: &Path,
    graph: &mut GraphIndex,
    snap: &mut IndexSnapshot,
) -> HashSet<String> {
    // Collect every id named by any decision's :SUPERSEDES: (whitespace- or
    // comma-separated, matching :TAGS: tolerance; a decision cannot supersede
    // itself). The flag itself is applied project-wide by apply_superseded_flags
    // once all decision files are loaded, so push with superseded=false here.
    let mut supersedes_targets: HashSet<String> = HashSet::new();
    for heading in &file.headings {
        if !heading.title.starts_with("dec_") {
            continue;
        }
        if let Some(val) = heading.property("SUPERSEDES") {
            let own_id = heading.property("ID");
            for target in val
                .split(|c: char| c == ',' || c.is_whitespace())
                .filter(|s| !s.is_empty())
            {
                if Some(target) == own_id {
                    continue; // a decision cannot supersede itself
                }
                supersedes_targets.insert(target.to_string());
            }
        }
    }
    for heading in &file.headings {
        if !heading.title.starts_with("dec_") {
            continue;
        }
        let node = match DecisionNode::from_heading(file, heading, &source.to_string_lossy()) {
            Ok(node) => node,
            Err(err) => {
                push_parse_error(snap, source.to_path_buf(), err.to_string());
                continue;
            }
        };
        let id = node.id.to_string();
        graph.nodes.push(GraphNodeSummary {
            id: id.clone(),
            layer: "decision".to_string(),
            outgoing: Vec::new(),
            source_file: source.to_path_buf(),
            superseded: false,
        });
        graph.decisions.push(DecisionSummary {
            id,
            title: node.title.to_string(),
            tags: node.tags.to_vec(),
            parent: node.parent,
            children: Vec::new(),
            depth: None,
            path: None,
            glossary_refs: own_vec(&node.glossary_refs),
            decided_at: node.decided_at.map(str::to_string),
            preview: node.decision.clone().or_else(|| node.context.clone()),
            source_file: source.to_path_buf(),
            superseded: false,
        });
    }
    supersedes_targets
}

/// Apply the superseded flag across the full decision graph (dec_KTF04): a
/// decision is superseded iff some present decision's :SUPERSEDES: names it.
/// Runs after all decision files load, so the result is correct project-wide.
fn apply_superseded_flags(graph: &mut GraphIndex, superseded: &HashSet<String>) {
    for summary in &mut graph.decisions {
        summary.superseded = superseded.contains(&summary.id);
    }
    for node in &mut graph.nodes {
        if node.layer == "decision" {
            node.superseded = superseded.contains(&node.id);
        }
    }
}

// orgasmic:TASK-2DFTX
fn build_decision_tree_index(
    graph: &mut GraphIndex,
    project_root: &Path,
    snap: &mut IndexSnapshot,
) {
    graph.decision_tree.clear();
    for decision in &mut graph.decisions {
        decision.children.clear();
        decision.depth = None;
        decision.path = None;
    }

    let nodes = graph
        .decisions
        .iter()
        .map(|decision| ParentTreeNode {
            id: decision.id.clone(),
            parent: decision.parent.clone(),
        })
        .collect::<Vec<_>>();
    if let Err(err) = validate_parent_tree(NodeIdClass::Decision, nodes) {
        let (path, message) = decision_tree_parse_error(&graph.decisions, project_root, err);
        snap.parse_errors.push(ParseError {
            path,
            kind: ParseErrorKind::WorkingFile,
            message,
            line: None,
            at: Utc::now(),
        });
    }

    let ids = graph
        .decisions
        .iter()
        .map(|decision| decision.id.as_str())
        .collect::<BTreeSet<_>>();
    let mut children: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for decision in &graph.decisions {
        let Some(parent) = decision.parent.as_deref() else {
            continue;
        };
        if ids.contains(parent) && parent != decision.id {
            children
                .entry(parent.to_string())
                .or_default()
                .push(decision.id.clone());
        }
    }

    let mut assigned: BTreeMap<String, (usize, String)> = BTreeMap::new();
    let roots = graph
        .decisions
        .iter()
        .filter(|decision| {
            decision
                .parent
                .as_deref()
                .is_none_or(|parent| !ids.contains(parent) || parent == decision.id)
        })
        .map(|decision| decision.id.clone())
        .collect::<Vec<_>>();
    for (index, root) in roots.iter().enumerate() {
        assign_decision_tree_paths(root, &children, 0, &(index + 1).to_string(), &mut assigned);
    }

    for decision in &mut graph.decisions {
        let entry_children = children.remove(&decision.id).unwrap_or_default();
        if let Some((depth, path)) = assigned.get(&decision.id).cloned() {
            decision.depth = Some(depth);
            decision.path = Some(path.clone());
            decision.children = entry_children.clone();
            graph.decision_tree.insert(
                decision.id.clone(),
                DecisionTreeEntry {
                    parent: decision.parent.clone(),
                    children: entry_children,
                    depth,
                    path,
                },
            );
        } else {
            // Cycle-corrupt nodes are left visible but without a derived path.
            decision.children = entry_children;
        }
    }
}

fn assign_decision_tree_paths(
    id: &str,
    children: &BTreeMap<String, Vec<String>>,
    depth: usize,
    path: &str,
    out: &mut BTreeMap<String, (usize, String)>,
) {
    if out.contains_key(id) {
        return;
    }
    out.insert(id.to_string(), (depth, path.to_string()));
    if let Some(kids) = children.get(id) {
        for (index, child) in kids.iter().enumerate() {
            let child_path = format!("{path}.{}", index + 1);
            assign_decision_tree_paths(child, children, depth + 1, &child_path, out);
        }
    }
}

fn decision_tree_parse_error(
    decisions: &[DecisionSummary],
    project_root: &Path,
    err: ParentTreeError,
) -> (PathBuf, String) {
    let id_for_path = match &err {
        ParentTreeError::MalformedParent { id, .. }
        | ParentTreeError::WrongClass { id, .. }
        | ParentTreeError::MissingParent { id, .. }
        | ParentTreeError::SelfParent { id }
        | ParentTreeError::DuplicateId { id }
        | ParentTreeError::UnknownId { id } => Some(id.as_str()),
        ParentTreeError::Cycle { chain } => chain.first().map(String::as_str),
    };
    let path = id_for_path
        .and_then(|id| {
            decisions
                .iter()
                .find(|decision| decision.id == id)
                .map(|decision| decision.source_file.clone())
        })
        .unwrap_or_else(|| project_root.join(".orgasmic/decisions.org"));
    (path, format!("decision tree :PARENT: error: {err}"))
}

fn load_architecture(
    file: &OrgFile,
    source: &Path,
    graph: &mut GraphIndex,
) -> Result<(), orgasmic_core::SchemaError> {
    let mut artifacts = BTreeMap::new();
    for node in ArchitectureNode::from_org(file, &source.to_string_lossy())? {
        let mut outgoing = own_vec(&node.motivated_by);
        outgoing.extend(node.edges.iter().map(|edge| edge.target.id()));
        graph.nodes.push(GraphNodeSummary {
            id: node.id.to_string(),
            layer: "architecture".to_string(),
            outgoing,
            source_file: source.to_path_buf(),
            superseded: false,
        });
        graph.architecture.push(ArchitectureSummary {
            id: node.id.to_string(),
            label: node.label,
            motivated_by: own_vec(&node.motivated_by),
            glossary_refs: own_vec(&node.glossary_refs),
            interface: own_vec(&node.interface),
            constraints: own_vec(&node.constraints),
            depends_on: own_vec(&node.depends_on),
            source_paths: own_vec(&node.source_paths),
            tests: node.tests,
            parent_id: node.parent_id,
            description: node.description.or(node.purpose),
            source_file: source.to_path_buf(),
        });
        for target in &node.motivated_by {
            graph.edges.push(GraphEdgeSummary {
                kind: "motivated_by".to_string(),
                from: node.id.to_string(),
                to: (*target).to_string(),
            });
        }
        for edge in node.edges {
            if let ArchEdgeTarget::Artifact(artifact) = &edge.target {
                let id = artifact.id();
                artifacts
                    .entry(id.clone())
                    .or_insert_with(|| ArchitectureArtifactSummary {
                        id,
                        kind: "artifact".to_string(),
                        scheme: artifact.scheme.as_str().to_string(),
                        name: artifact.name.clone(),
                    });
            }
            let summary = GraphEdgeSummary {
                kind: edge.kind.as_str().to_string(),
                from: edge.source_node_id,
                to: edge.target.id(),
            };
            graph.architecture_edges.push(summary.clone());
            graph.edges.push(summary);
        }
    }
    for artifact in artifacts.into_values() {
        graph.nodes.push(GraphNodeSummary {
            id: artifact.id.clone(),
            layer: "artifact".to_string(),
            outgoing: Vec::new(),
            source_file: source.to_path_buf(),
            superseded: false,
        });
        graph.architecture_artifacts.push(artifact);
    }
    Ok(())
}

fn load_glossary(file: &OrgFile, source: &Path, graph: &mut GraphIndex) {
    for heading in &file.headings {
        // Legacy headings: `* term:slug Title`; minted (dec_X72P5): `* term_XXXXX Title`.
        if !(heading.title.starts_with("term:") || heading.title.starts_with("term_")) {
            continue;
        }
        let Ok(term) = GlossaryTerm::from_heading(heading, &source.to_string_lossy()) else {
            continue;
        };
        graph.nodes.push(GraphNodeSummary {
            id: term.id.to_string(),
            layer: "glossary".to_string(),
            outgoing: own_vec(&term.relates_to),
            source_file: source.to_path_buf(),
            superseded: false,
        });
        graph.glossary.push(GlossarySummary {
            id: term.id.to_string(),
            canonical: term.canonical.map(str::to_string),
            avoid: term.avoid.map(str::to_string),
            relates_to: own_vec(&term.relates_to),
            definition: term.definition.map(str::to_string),
            source_file: source.to_path_buf(),
        });
    }
}

fn load_task_graph(project: &mut ProjectIndex) {
    // Artifact id -> (summary, first-producing task's source file). The map
    // dedups artifacts produced by more than one task; the node is pushed once
    // after the loop so a shared artifact never appears multiple times in
    // graph.nodes (VJXXC reviewer HIGH).
    let mut artifacts: BTreeMap<String, (ArchitectureArtifactSummary, PathBuf)> = BTreeMap::new();
    for task in &project.tasks {
        let mut outgoing = task.depends_on.clone();
        outgoing.extend(task.implements.clone());
        outgoing.extend(task.produces.clone());
        project.graph.nodes.push(GraphNodeSummary {
            id: task.id.clone(),
            layer: "task".to_string(),
            outgoing,
            source_file: task.source_file.clone(),
            superseded: false,
        });
        for target in &task.depends_on {
            project.graph.edges.push(GraphEdgeSummary {
                kind: "depends_on".to_string(),
                from: task.id.clone(),
                to: target.clone(),
            });
        }
        for target in &task.implements {
            project.graph.edges.push(GraphEdgeSummary {
                kind: "implements".to_string(),
                from: task.id.clone(),
                to: target.clone(),
            });
        }
        for target in &task.produces {
            project.graph.edges.push(GraphEdgeSummary {
                kind: "produces".to_string(),
                from: task.id.clone(),
                to: target.clone(),
            });
            if !looks_like_structured_node_id(target) {
                artifacts.entry(target.clone()).or_insert_with(|| {
                    (
                        ArchitectureArtifactSummary {
                            id: target.clone(),
                            kind: "artifact".to_string(),
                            scheme: "file".to_string(),
                            name: target.clone(),
                        },
                        task.source_file.clone(),
                    )
                });
            }
        }
    }
    let existing: BTreeSet<String> = project
        .graph
        .nodes
        .iter()
        .map(|node| node.id.clone())
        .collect();
    for (id, (summary, source_file)) in artifacts {
        if existing.contains(&id) {
            continue;
        }
        project.graph.nodes.push(GraphNodeSummary {
            id,
            layer: "artifact".to_string(),
            outgoing: Vec::new(),
            source_file,
            superseded: false,
        });
        project.graph.architecture_artifacts.push(summary);
    }
}

fn lint_dangling_graph_edges(project: &ProjectIndex, snap: &mut IndexSnapshot) {
    let node_ids = project
        .graph
        .nodes
        .iter()
        .map(|node| node.id.as_str())
        .collect::<BTreeSet<_>>();
    let source_files = project
        .graph
        .nodes
        .iter()
        .map(|node| (node.id.as_str(), node.source_file.clone()))
        .collect::<BTreeMap<_, _>>();
    for edge in &project.graph.edges {
        // Only structured-id targets are linted (a PRODUCES file-path artifact
        // is always materialized as a node above, so it never dangles). dec_/
        // term_ included so a motivated_by/implements edge to a missing
        // decision or term surfaces too (VJXXC reviewer MEDIUM).
        if !edge.to.starts_with("TASK-")
            && !edge.to.starts_with("arch_")
            && !edge.to.starts_with("dec_")
            && !edge.to.starts_with("term_")
        {
            continue;
        }
        if node_ids.contains(edge.to.as_str()) {
            continue;
        }
        push_parse_error(
            snap,
            source_files
                .get(edge.from.as_str())
                .cloned()
                .unwrap_or_else(|| project.root.join(".orgasmic")),
            format!(
                "graph edge {} {} -> {} has dangling target {}",
                edge.kind, edge.from, edge.to, edge.to
            ),
        );
    }
}

fn looks_like_structured_node_id(value: &str) -> bool {
    value.starts_with("TASK-")
        || value.starts_with("arch_")
        || value.starts_with("dec_")
        || value.starts_with("term_")
        || value.starts_with("term:")
}

fn collect_tx_dir(dir: &Path, project_id: Option<&str>, snap: &mut IndexSnapshot) {
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in read.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("org") {
            continue;
        }
        match std::fs::read_to_string(&path) {
            Ok(contents) => match parse_tx_file(&contents, &path.to_string_lossy()) {
                Ok(entries) => {
                    for entry in entries {
                        snap.tx.push(TxRecord {
                            project_id: project_id.map(str::to_string),
                            source_path: path.clone(),
                            entry,
                        });
                    }
                }
                Err(err) => {
                    let message = err.to_string();
                    if !parse_error_already_recorded(snap, &path, &message) {
                        warn!(path = %path.display(), error = %message, "tx parse failed");
                        snap.parse_errors.push(ParseError {
                            path: path.clone(),
                            kind: ParseErrorKind::HistoricalTx,
                            line: tx_parse_error_line(&err, &contents),
                            message,
                            at: Utc::now(),
                        });
                    }
                }
            },
            Err(err) => {
                let message = err.to_string();
                if !parse_error_already_recorded(snap, &path, &message) {
                    warn!(path = %path.display(), error = %message, "tx read failed");
                    snap.parse_errors.push(ParseError {
                        path: path.clone(),
                        kind: ParseErrorKind::HistoricalTx,
                        message,
                        line: None,
                        at: Utc::now(),
                    });
                }
            }
        }
    }
}

fn lint_project_identity_state(
    project_root: &Path,
    markers: &BTreeMap<String, Vec<PathBuf>>,
    snap: &mut IndexSnapshot,
) {
    for finding in lint_project_identities(project_root, markers) {
        push_parse_error(snap, finding.path, finding.message);
    }
}

/// True when an identical `(path, message)` parse error is already recorded
/// this pass. A watcher-driven refresh can otherwise re-warn the same
/// unresolved issue on every debounced event even though nothing changed
/// (TASK-V8WY9): callers use this to log — and record — each distinct
/// finding once per rebuild/refresh pass instead of spamming the log.
fn parse_error_already_recorded(snap: &IndexSnapshot, path: &Path, message: &str) -> bool {
    snap.parse_errors
        .iter()
        .any(|e| e.path == path && e.message == message)
}

fn push_parse_error(snap: &mut IndexSnapshot, path: PathBuf, message: String) {
    if parse_error_already_recorded(snap, &path, &message) {
        return;
    }
    warn!(path = %path.display(), error = %message, "graph parse failed");
    snap.parse_errors.push(ParseError {
        path,
        kind: ParseErrorKind::WorkingFile,
        message,
        line: None,
        at: Utc::now(),
    });
}

/// Thin-goal convention lint: liveness bookkeeping lives on the HANDOFF
/// heading in `tasks/handoff.org`, never in `goal.org`. A `:LIVENESS:` on a
/// goal heading is a vestige of the pre-2026-06 fat-goal format; it rots
/// silently (nothing bumps it) while resume drift detection reads the handoff
/// copy, so surface it as a parse error instead of letting it mislead.
fn lint_goal_liveness(file: &OrgFile, path: &Path, snap: &mut IndexSnapshot) {
    if path.file_name().and_then(|name| name.to_str()) != Some("goal.org") {
        return;
    }
    for heading in &file.headings {
        let offending: Vec<&str> = ["LIVENESS", "LIVENESS_AT"]
            .into_iter()
            .filter(|key| heading.property(key).is_some())
            .collect();
        if offending.is_empty() {
            continue;
        }
        push_parse_error(
            snap,
            path.to_path_buf(),
            format!(
                "goal heading '{}' carries :{}: — liveness bookkeeping belongs on the \
                 HANDOFF heading in tasks/handoff.org (thin-goal convention); move it there",
                heading.title,
                offending.join(": :"),
            ),
        );
    }
}

/// Read-time lint: flag level-1 headings in task files that lack the expected
/// task shape (`:ID: TASK-*` property + a recognized TODO state keyword).
/// Such headings may be phantom entries created by daemon-free body writes
/// that bypassed the write-time guard. Surfaces findings via the parse-errors
/// channel so `/graph/parse-errors` exposes them. Precedent: `lint_goal_liveness`.
/// Read-time lint: heading title's leading ID token must agree with drawer
/// `:ID:` on task-shaped headings. Glossary terms are exempt (dec_X72P5d).
// orgasmic:task_KY80Q
fn lint_task_heading_id_tokens(file: &OrgFile, path: &Path, snap: &mut IndexSnapshot) {
    for heading in &file.headings {
        if let Some(message) = lint_task_heading_id_token(heading) {
            push_parse_error(snap, path.to_path_buf(), message);
        }
    }
}

// orgasmic:task_KY80Q
fn lint_decision_heading_id_tokens(file: &OrgFile, path: &Path, snap: &mut IndexSnapshot) {
    for heading in &file.headings {
        if let Some(message) = lint_decision_heading_id_token(heading) {
            push_parse_error(snap, path.to_path_buf(), message);
        }
    }
}

// orgasmic:task_KY80Q
fn lint_arch_heading_id_tokens(file: &OrgFile, path: &Path, snap: &mut IndexSnapshot) {
    for heading in &file.headings {
        if !heading.title.starts_with("arch_") {
            continue;
        }
        if let Some(message) = lint_arch_heading_id_token(heading) {
            push_parse_error(snap, path.to_path_buf(), message);
        }
        for child in &heading.sections {
            if child.title.starts_with("arch_") {
                if let Some(message) = lint_arch_heading_id_token(child) {
                    push_parse_error(snap, path.to_path_buf(), message);
                }
            }
        }
    }
}

// orgasmic:task_HC7PW
fn lint_phantom_task_headings(file: &OrgFile, path: &Path, snap: &mut IndexSnapshot) {
    for h in &file.headings {
        let has_task_id = h
            .property("ID")
            .map(|id| id.starts_with("TASK-"))
            .unwrap_or(false);
        // `h.todo` is only populated for allowlisted keywords (see org.rs
        // TODO_KEYWORDS), so `is_some()` is the correct is-a-known-state check.
        let has_state = h.todo.is_some();
        if !has_task_id || !has_state {
            let reason = if !has_task_id && !has_state {
                "missing :ID: TASK-* property and TODO state keyword"
            } else if !has_task_id {
                "missing :ID: TASK-* property"
            } else {
                "missing TODO state keyword"
            };
            push_parse_error(
                snap,
                path.to_path_buf(),
                format!(
                    "task-file heading '{}' lacks expected task shape ({}) — \
                     possible phantom heading from daemon-free body write",
                    h.title, reason,
                ),
            );
        }
    }
}

fn tx_parse_error_line(err: &TxError, contents: &str) -> Option<usize> {
    tx_error_line(err).or_else(|| first_heading_line(contents))
}

fn tx_error_line(err: &TxError) -> Option<usize> {
    match err {
        TxError::Parse(err) => org_error_line(err),
        TxError::Io(_) | TxError::MissingField(_) | TxError::NonPropertyOnly { .. } => None,
    }
}

fn org_error_line(err: &OrgError) -> Option<usize> {
    match err {
        OrgError::BadProperty { line, .. }
        | OrgError::UnterminatedDrawer { line, .. }
        | OrgError::BadHeading { line, .. }
        | OrgError::BadKeyword { line, .. } => Some(*line),
        OrgError::HeadingNotFound { .. }
        | OrgError::PropertyNotFound { .. }
        | OrgError::SectionNotFound { .. }
        | OrgError::NoPropertyDrawer { .. }
        | OrgError::BodyHeadingInjection { .. } => None,
    }
}

fn first_heading_line(contents: &str) -> Option<usize> {
    contents
        .lines()
        .enumerate()
        .find_map(|(index, line)| line.starts_with("* ").then_some(index + 1))
}

fn read_org(path: &Path) -> Result<OrgFile, String> {
    let raw = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    OrgFile::parse(raw, path.to_string_lossy()).map_err(|e| e.to_string())
}

async fn git_remote_origin_url_with_program(
    project_root: &Path,
    program: &OsStr,
    timeout: Duration,
) -> Option<String> {
    let mut child = Command::new(program)
        .args(["config", "--get", "remote.origin.url"])
        .current_dir(project_root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let stdout_reader = tokio::spawn(async move {
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).await.map(|_| bytes)
    });
    let status = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(status)) => status,
        Ok(Err(error)) => {
            warn!(
                project = %project_root.display(),
                error = %error,
                "git remote origin lookup failed"
            );
            if let Err(kill_error) = child.start_kill() {
                warn!(
                    project = %project_root.display(),
                    error = %kill_error,
                    "failed to kill errored git remote origin child"
                );
            }
            if let Err(wait_error) = child.wait().await {
                warn!(
                    project = %project_root.display(),
                    error = %wait_error,
                    "failed to reap errored git remote origin child"
                );
            }
            let _ = stdout_reader.await;
            return None;
        }
        Err(_) => {
            warn!(
                project = %project_root.display(),
                timeout_secs = timeout.as_secs_f64(),
                "git remote origin lookup timed out; killing child and leaving repo_url empty"
            );
            if let Err(error) = child.start_kill() {
                warn!(
                    project = %project_root.display(),
                    error = %error,
                    "failed to kill timed-out git remote origin child"
                );
            }
            if let Err(error) = child.wait().await {
                warn!(
                    project = %project_root.display(),
                    error = %error,
                    "failed to reap timed-out git remote origin child"
                );
            }
            let _ = stdout_reader.await;
            return None;
        }
    };
    let output = stdout_reader.await.ok()?.ok()?;
    if !status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&output).trim().to_string();
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

fn macos_files_access_hint(path: &Path, user_home: &Path) -> Option<String> {
    let protected = [
        ("Documents", user_home.join("Documents")),
        ("Desktop", user_home.join("Desktop")),
        ("Downloads", user_home.join("Downloads")),
    ];
    protected.into_iter().find_map(|(folder, root)| {
        path.starts_with(root).then(|| {
            format!(
                "project scan timed out at {} under ~/{}; grant the orgasmic daemon Files and Folders access for {} in System Settings > Privacy & Security, then restart the daemon",
                path.display(),
                folder,
                folder
            )
        })
    })
}

fn warn_macos_files_access_timeout(paths: &[PathBuf]) {
    #[cfg(target_os = "macos")]
    {
        let Some(user_home) = std::env::var_os("HOME").map(PathBuf::from) else {
            return;
        };
        for path in paths {
            if let Some(hint) = macos_files_access_hint(path, &user_home) {
                warn!(project = %path.display(), "{hint}");
            }
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = paths;
    }
}

fn own_vec(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_string()).collect()
}

fn scan_project_markers(project_root: &Path) -> BTreeMap<String, Vec<PathBuf>> {
    let root = std::fs::canonicalize(project_root).unwrap_or_else(|_| project_root.to_path_buf());
    let gitignore_dirs = load_top_level_gitignore_dirs(&root);
    let mut found: BTreeMap<String, BTreeSet<PathBuf>> = BTreeMap::new();
    let mut stack = vec![root.clone()];

    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                if !should_skip_marker_dir(&root, &path, &gitignore_dirs) {
                    stack.push(path);
                }
                continue;
            }
            if file_type.is_file() && should_scan_marker_file(&path) {
                let rel = path.strip_prefix(&root).unwrap_or(&path).to_string_lossy();
                if should_skip_marker_path(&rel) {
                    continue;
                }
                scan_marker_file(&root, &path, &mut found);
            }
        }
    }

    found
        .into_iter()
        .map(|(node_id, files)| (node_id, files.into_iter().collect()))
        .collect()
}

fn load_top_level_gitignore_dirs(root: &Path) -> BTreeSet<String> {
    let Ok(contents) = std::fs::read_to_string(root.join(".gitignore")) else {
        return BTreeSet::new();
    };
    contents
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with('!') {
                return None;
            }
            let pattern = trimmed.trim_start_matches('/').trim_end_matches('/');
            if pattern.is_empty()
                || pattern.contains('/')
                || pattern.contains('*')
                || pattern.contains('?')
                || pattern.contains('[')
            {
                return None;
            }
            Some(pattern.to_string())
        })
        .collect()
}

fn should_skip_marker_dir(root: &Path, path: &Path, gitignore_dirs: &BTreeSet<String>) -> bool {
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
    if matches!(name, ".git" | "target" | "node_modules" | "dist" | "build") {
        return true;
    }
    let rel = path.strip_prefix(root).unwrap_or(path);
    if rel.starts_with(".orgasmic/snapshots") {
        return true;
    }
    rel.components().count() == 1 && gitignore_dirs.contains(name)
}

fn should_scan_marker_file(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|ext| ext.to_str()),
        Some(
            "rs" | "ts"
                | "tsx"
                | "js"
                | "jsx"
                | "py"
                | "sh"
                | "bash"
                | "zsh"
                | "yaml"
                | "yml"
                | "toml"
        )
    )
}

fn scan_marker_file(root: &Path, path: &Path, found: &mut BTreeMap<String, BTreeSet<PathBuf>>) {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return;
    };
    let rel = path.strip_prefix(root).unwrap_or(path).to_path_buf();
    let ext = path.extension().and_then(|e| e.to_str());
    let mut file_ids = BTreeSet::new();
    for line in contents.lines() {
        file_ids.extend(marker_node_ids_in_line(line, ext));
    }
    for node_id in file_ids {
        found.entry(node_id).or_default().insert(rel.clone());
    }
}

fn rebuild_all_activity_indexes(snap: &mut IndexSnapshot) {
    let tx = snap.tx.clone();
    for project in snap.projects.values_mut() {
        project.activity_index = build_activity_index(&project.project_id, &tx);
    }
}

fn parse_task(
    file: &OrgFile,
    heading: &Heading,
    source: &Path,
) -> Result<Option<TaskSummary>, orgasmic_core::SchemaError> {
    let looks_like_task = heading
        .property("ID")
        .map(|id| id.starts_with("TASK-"))
        .unwrap_or(false);
    if !looks_like_task || heading.todo.is_none() {
        return Ok(None);
    }
    let view = TaskHeading::from_heading(file, heading, &source.to_string_lossy())?;
    Ok(Some(TaskSummary {
        id: view.id.to_string(),
        title: view.title.to_string(),
        lifecycle_stage: view.lifecycle_stage,
        parent_task: view.parent_task,
        depends_on: own_vec(&view.depends_on),
        implements: own_vec(&view.implements),
        produces: own_vec(&view.produces),
        read_scope: own_vec(&view.read_scope),
        write_scope: own_vec(&view.write_scope),
        owner: TaskOwner::Human,
        run_id: None,
        priority: view.priority.map(str::to_string),
        provider: view.provider.map(str::to_string),
        model: view.model.map(str::to_string),
        reasoning_effort: view.reasoning_effort.map(str::to_string),
        test_cmd: view.test_cmd.map(str::to_string),
        tags: view.tags.to_vec(),
        source_file: source.to_path_buf(),
        sandbox_permissions: view.sandbox_permissions.clone(),
    }))
}

fn parse_task_body(file: &OrgFile, heading: &Heading) -> TaskBody {
    let pre_section = trim_section(file.slice(heading.body.clone()));
    let description_section = section_text(file, heading, "Description");
    let acceptance = section_text(file, heading, "Acceptance Criteria");
    TaskBody {
        description: join_blocks([pre_section, description_section]),
        acceptance_criteria: parse_acceptance_criteria(&acceptance),
        evidence: section_lines(file, heading, "Evidence"),
        notes: section_text(file, heading, "Notes"),
        worklog: section_lines(file, heading, "Worklog"),
        reviewer_pass: section_lines(file, heading, "Reviewer pass"),
    }
}

fn section_text(file: &OrgFile, heading: &Heading, title: &str) -> String {
    heading
        .section(title)
        .map(|section| trim_section(file.slice(section.body.clone())))
        .unwrap_or_default()
}

fn section_lines(file: &OrgFile, heading: &Heading, title: &str) -> Vec<String> {
    section_text(file, heading, title)
        .lines()
        .map(strip_list_marker)
        .filter(|line| !line.is_empty())
        .collect()
}

fn parse_acceptance_criteria(body: &str) -> Vec<AcceptanceItem> {
    body.lines().filter_map(parse_acceptance_item).collect()
}

fn parse_acceptance_item(line: &str) -> Option<AcceptanceItem> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    let rest = trimmed.strip_prefix("- [")?;
    let (marker, tail) = rest.split_once(']')?;
    let state = match marker {
        "X" | "x" => AcceptanceState::Checked,
        " " => AcceptanceState::Unchecked,
        "-" => AcceptanceState::Partial,
        _ => return None,
    };
    let text = tail.trim();
    if text.is_empty() {
        None
    } else {
        Some(AcceptanceItem {
            state,
            text: text.to_string(),
        })
    }
}

fn trim_section(value: &str) -> String {
    value.trim().to_string()
}

fn join_blocks(blocks: impl IntoIterator<Item = String>) -> String {
    blocks
        .into_iter()
        .filter(|block| !block.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn strip_list_marker(value: &str) -> String {
    let trimmed = value.trim();
    trimmed
        .strip_prefix("- ")
        .unwrap_or(trimmed)
        .trim()
        .to_string()
}

fn build_subtask_index(
    tasks: &[TaskSummary],
    project_root: &Path,
    snap: &mut IndexSnapshot,
) -> BTreeMap<TaskId, Vec<TaskId>> {
    let ids = tasks
        .iter()
        .map(|task| task.id.as_str())
        .collect::<BTreeSet<_>>();
    let mut out: BTreeMap<TaskId, Vec<TaskId>> = BTreeMap::new();
    for task in tasks {
        let Some(parent) = task.parent_task.as_deref() else {
            continue;
        };
        if ids.contains(parent) {
            out.entry(parent.to_string())
                .or_default()
                .push(task.id.clone());
        } else {
            snap.parse_errors.push(ParseError {
                path: task.source_file.clone(),
                kind: ParseErrorKind::WorkingFile,
                message: format!(
                    "task {} has orphan derived parent {} in {}",
                    task.id,
                    parent,
                    project_root.display()
                ),
                line: None,
                at: Utc::now(),
            });
        }
    }
    for children in out.values_mut() {
        children.sort();
    }
    out
}

fn build_activity_index(
    project_id: &str,
    records: &[TxRecord],
) -> BTreeMap<TaskId, Vec<ActivityEntry>> {
    let mut out: BTreeMap<TaskId, Vec<ActivityEntry>> = BTreeMap::new();
    for record in records {
        let in_project = record.project_id.as_deref() == Some(project_id)
            || record.entry.project.as_deref() == Some(project_id);
        if !in_project {
            continue;
        }
        let Some(task_id) = record.entry.task.as_deref() else {
            continue;
        };
        let Some(entry) = activity_entry_from_tx(&record.entry) else {
            continue;
        };
        out.entry(task_id.to_string()).or_default().push(entry);
    }
    for entries in out.values_mut() {
        entries.sort_by(|a, b| a.time.cmp(&b.time).then_with(|| a.tx_id.cmp(&b.tx_id)));
    }
    out
}

fn activity_entry_from_tx(entry: &TxEntry) -> Option<ActivityEntry> {
    let kind = if entry.ty == "comment" {
        ActivityKind::Comment
    } else if entry.ty == "task.state_transitioned" {
        ActivityKind::StateTransition
    } else if entry.ty.starts_with("run.") {
        ActivityKind::RunLifecycle
    } else {
        return None;
    };
    Some(ActivityEntry {
        tx_id: entry.tx_id.clone(),
        time: entry.time.clone(),
        kind,
        actor: entry.actor.clone(),
        body: activity_body(entry),
        artifacts: extra_value(entry, "ARTIFACTS")
            .map(|value| value.split_whitespace().map(str::to_string).collect())
            .unwrap_or_default(),
        in_reply_to: extra_value(entry, "IN_REPLY_TO").map(str::to_string),
    })
}

fn activity_body(entry: &TxEntry) -> String {
    if entry.ty == "comment" {
        return extra_value(entry, "BODY")
            .map(unescape_property_value)
            .unwrap_or_default();
    }
    if entry.ty == "task.state_transitioned" {
        let from = extra_value(entry, "FROM_STATE").unwrap_or("?");
        let to = extra_value(entry, "TO_STATE").unwrap_or("?");
        return entry
            .reason
            .clone()
            .unwrap_or_else(|| format!("{from} -> {to}"));
    }
    entry.reason.clone().unwrap_or_else(|| entry.ty.clone())
}

fn extra_value<'a>(entry: &'a TxEntry, key: &str) -> Option<&'a str> {
    entry
        .extra
        .iter()
        .find(|(got, _)| got == key)
        .map(|(_, value)| value.as_str())
}

fn unescape_property_value(value: &str) -> String {
    let mut out = String::new();
    let mut chars = value.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('\\') => out.push('\\'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(ch);
        }
    }
    out
}

fn is_under(child: &Path, ancestor: &Path) -> bool {
    let Ok(child_can) = child
        .canonicalize()
        .or_else(|_| Ok::<_, std::io::Error>(child.to_path_buf()))
    else {
        return false;
    };
    let Ok(anc_can) = ancestor
        .canonicalize()
        .or_else(|_| Ok::<_, std::io::Error>(ancestor.to_path_buf()))
    else {
        return false;
    };
    child_can.starts_with(&anc_can)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, contents).unwrap();
    }

    fn make_home() -> (tempfile::TempDir, Home) {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        (tmp, home)
    }

    fn seed_board(home: &Home, project_path: &Path, id: &str) {
        let board = home.board();
        let content = format!(
            "#+title: orgasmic board\n#+orgasmic_version: 1\n\n* PROJECT {id}\n:PROPERTIES:\n:ID:               {id}\n:PATH:             {}\n:BRANCH:           main\n:STATUS:           active\n:END:\n",
            project_path.display(),
        );
        write(&board, &content);
    }

    fn seed_project(project_root: &Path) {
        let project = project_root.join(".orgasmic/project.org");
        write(
            &project,
            "#+title: x\n#+orgasmic_version: 1\n\n* PROJECT proj-x\n:PROPERTIES:\n:ID:               proj-x\n:END:\n",
        );
        let sprint = project_root.join(".orgasmic/tasks/backlog.org");
        write(
            &sprint,
            "#+title: x sprint\n#+orgasmic_version: 1\n\n* BACKLOG TASK-001 Do a thing :work:\n:PROPERTIES:\n:ID:               TASK-001\n:PRIORITY:         P1\n:END:\n\n** Description\nSeeded detail.\n\n** Acceptance Criteria\n- [ ] Body fields load.\n",
        );
    }

    #[tokio::test]
    async fn repo_url_resolution_is_disabled_until_post_bind_refresh() {
        let (tmp, home) = make_home();
        let project = tmp.path().join("project");
        seed_project(&project);
        seed_board(&home, &project, "project");
        std::fs::create_dir_all(project.join(".git")).unwrap();
        let index = Index::new(home);

        index.rebuild().await;
        assert_eq!(index.snapshot().await.projects["project"].repo_url, "");
        assert_eq!(index.git_spawn_attempts.load(Ordering::Relaxed), 0);

        // Watcher refreshes can happen during boot. They must preserve the
        // no-Git-before-bind invariant too.
        index.refresh_project("project").await.unwrap();
        assert_eq!(index.snapshot().await.projects["project"].repo_url, "");
        assert_eq!(index.git_spawn_attempts.load(Ordering::Relaxed), 0);

        index
            .repo_url_refresh_enabled
            .store(true, Ordering::Release);
        index.refresh_repo_urls().await;
        assert_eq!(index.git_spawn_attempts.load(Ordering::Relaxed), 1);

        // A watcher scan must not erase the last Git-backed value while the
        // repository metadata is temporarily unavailable.
        index
            .inner
            .write()
            .await
            .projects
            .get_mut("project")
            .unwrap()
            .repo_url = "ssh://git@example.com/org/project.git".to_string();
        std::fs::rename(project.join(".git"), project.join(".git-hidden")).unwrap();
        index.refresh_project("project").await.unwrap();
        assert_eq!(
            index.snapshot().await.projects["project"].repo_url,
            "ssh://git@example.com/org/project.git"
        );
    }

    #[tokio::test]
    async fn live_rebuild_preserves_repo_url_and_schedules_post_bind_refresh() {
        fn git(cwd: &Path, args: &[&str]) {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(cwd)
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?} failed");
        }

        let (tmp, home) = make_home();
        let project = tmp.path().join("project");
        seed_project(&project);
        seed_board(&home, &project, "project");
        git(&project, &["init", "--quiet"]);
        git(
            &project,
            &[
                "remote",
                "add",
                "origin",
                "ssh://git@example.com/org/project.git",
            ],
        );
        let index = Index::new(home);

        // Boot's pre-bind rebuild never invokes Git.
        index.rebuild().await;
        assert_eq!(index.git_spawn_attempts.load(Ordering::Relaxed), 0);

        index
            .repo_url_refresh_enabled
            .store(true, Ordering::Release);
        index.refresh_repo_urls().await;
        let expected = "ssh://git@example.com/org/project.git";
        assert_eq!(
            index.snapshot().await.projects["project"].repo_url,
            expected
        );
        let attempts_before_rebuild = index.git_spawn_attempts.load(Ordering::Relaxed);

        // POST /reindex follows this same live-rebuild path. Publishing the
        // new snapshot must retain its known URL while Git refreshes it again.
        index.rebuild().await;
        assert_eq!(
            index.snapshot().await.projects["project"].repo_url,
            expected
        );
        assert!(index.repo_url_refresh_enabled.load(Ordering::Acquire));
        tokio::time::timeout(Duration::from_secs(3), async {
            while index.git_spawn_attempts.load(Ordering::Relaxed) <= attempts_before_rebuild {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("live rebuild did not schedule a post-bind Git refresh");
        assert_eq!(
            index.snapshot().await.projects["project"].repo_url,
            expected
        );
    }

    #[tokio::test]
    async fn git_resolves_quoted_included_origin_from_linked_worktree() {
        fn git(cwd: &Path, args: &[&str]) {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(cwd)
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?} failed");
        }

        let tmp = tempfile::tempdir().unwrap();
        let main = tmp.path().join("main");
        let worktree = tmp.path().join("linked-worktree");
        std::fs::create_dir_all(&main).unwrap();
        git(&main, &["init", "--quiet"]);
        write(&main.join("tracked"), "seed\n");
        git(&main, &["add", "tracked"]);
        git(
            &main,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "--quiet",
                "-m",
                "seed",
            ],
        );
        let included = tmp.path().join("origin.inc");
        write(
            &included,
            "[remote \"origin\"]\n\turl = \"ssh://git@example.com/org/quoted.git\"\n",
        );
        git(
            &main,
            &[
                "config",
                "include.path",
                included.to_str().expect("UTF-8 temp path"),
            ],
        );
        git(
            &main,
            &[
                "worktree",
                "add",
                "--quiet",
                "--detach",
                worktree.to_str().expect("UTF-8 temp path"),
                "HEAD",
            ],
        );

        let repo_url = git_remote_origin_url_with_program(
            &worktree,
            OsStr::new("git"),
            Duration::from_secs(3),
        )
        .await;

        assert_eq!(
            repo_url.as_deref(),
            Some("ssh://git@example.com/org/quoted.git")
        );
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn blocking_git_origin_child_is_killed_and_reaped_on_timeout() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let fake_git = tmp.path().join("git");
        write(&fake_git, "#!/bin/sh\nexec sleep 30\n");
        let mut permissions = std::fs::metadata(&fake_git).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&fake_git, permissions).unwrap();

        let started = std::time::Instant::now();
        let value = git_remote_origin_url_with_program(
            &project,
            fake_git.as_os_str(),
            Duration::from_millis(500),
        )
        .await;

        assert_eq!(value, None);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "hung git child was not bounded"
        );
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn blocked_project_scan_times_out_without_blocking_the_index() {
        use std::os::unix::ffi::OsStrExt;

        let (tmp, home) = make_home();
        let project = tmp.path().join("blocked-project");
        seed_project(&project);
        seed_board(&home, &project, "blocked-project");
        let task_file = project.join(".orgasmic/tasks/backlog.org");
        std::fs::remove_file(&task_file).unwrap();
        let task_file_c = std::ffi::CString::new(task_file.as_os_str().as_bytes()).unwrap();
        let rc = unsafe { libc::mkfifo(task_file_c.as_ptr(), 0o600) };
        assert_eq!(rc, 0, "create blocking task fifo");
        let index = Index::new(home);

        let started = std::time::Instant::now();
        index.rebuild_with_timeout(Duration::from_millis(50)).await;

        assert!(
            started.elapsed() < Duration::from_secs(1),
            "project scan timeout did not bound rebuild"
        );
        let snapshot = index.snapshot().await;
        assert_eq!(snapshot.board.len(), 1);
        assert!(
            !snapshot.projects.contains_key("blocked-project"),
            "timed-out project should be omitted from the pass"
        );

        // Release the abandoned blocking-pool scan before the tempdir is
        // removed. Unlink the FIFO after pairing its blocked reader, replace
        // the path with a regular file for any later reads in the same scan,
        // then close the old pipe. The timed-out result is ignored.
        let mut fifo_writer = std::fs::OpenOptions::new()
            .write(true)
            .open(&task_file)
            .unwrap();
        std::fs::remove_file(&task_file).unwrap();
        std::fs::write(&task_file, "#+title: tasks\n#+orgasmic_version: 1\n\n").unwrap();
        std::io::Write::write_all(
            &mut fifo_writer,
            b"#+title: tasks\n#+orgasmic_version: 1\n\n",
        )
        .unwrap();
        drop(fifo_writer);
    }

    #[test]
    fn protected_folder_timeout_hint_is_path_specific_and_actionable() {
        let user_home = Path::new("/Users/example");
        let project = user_home.join("Documents/work/orgasmic");
        let hint = macos_files_access_hint(&project, user_home).unwrap();

        assert!(hint.contains(&project.display().to_string()));
        assert!(hint.contains("Files and Folders"));
        assert!(hint.contains("System Settings > Privacy & Security"));
        assert_eq!(
            macos_files_access_hint(Path::new("/tmp/project"), user_home),
            None
        );
    }

    #[test]
    fn task_owner_serializes_as_ui_owner_string() {
        assert_eq!(serde_json::to_value(TaskOwner::Human).unwrap(), "human");
        assert_eq!(
            serde_json::to_value(TaskOwner::Agent("implementer".to_string())).unwrap(),
            "agent.implementer"
        );
    }

    #[test]
    fn push_parse_error_dedups_identical_path_and_message_within_a_pass() {
        let mut snap = IndexSnapshot::default();
        let path = PathBuf::from("/proj/.orgasmic/glossary.org");
        let message = "dangling reference `x` (:RELATES_TO: on term_A)".to_string();
        push_parse_error(&mut snap, path.clone(), message.clone());
        push_parse_error(&mut snap, path, message);
        assert_eq!(
            snap.parse_errors.len(),
            1,
            "identical (path, message) must record once per pass, not N times"
        );
    }

    #[tokio::test]
    async fn rebuild_loads_board_and_tasks() {
        let (tmp, home) = make_home();
        let project_root = tmp.path().join("proj");
        seed_project(&project_root);
        seed_board(&home, &project_root, "proj-x");

        let index = Index::new(home);
        index.rebuild().await;
        let snap = index.snapshot().await;
        assert_eq!(snap.board.len(), 1);
        assert!(snap.projects.contains_key("proj-x"));
        let project = snap.project("proj-x").unwrap();
        assert_eq!(project.tasks.len(), 1);
        assert_eq!(project.tasks[0].id, "TASK-001");
        let detail = TaskDetail::from_indexed_body(
            project.tasks[0].clone(),
            project.task_bodies.get("TASK-001").cloned(),
        );
        assert_eq!(detail.body.description, "Seeded detail.");
        assert_eq!(
            detail.body.acceptance_criteria,
            vec![AcceptanceItem {
                state: AcceptanceState::Unchecked,
                text: "Body fields load.".to_string(),
            }]
        );
    }

    #[tokio::test]
    async fn task_summary_indexes_depends_on_on_rebuild_and_refresh() {
        let (tmp, home) = make_home();
        let project_root = tmp.path().join("proj");
        seed_project(&project_root);
        seed_board(&home, &project_root, "proj-x");
        write(
            &project_root.join(".orgasmic/tasks/backlog.org"),
            "#+title: x sprint\n#+orgasmic_version: 1\n\n* BACKLOG TASK-001 Blocked task :work:\n:PROPERTIES:\n:ID:               TASK-001\n:DEPENDS_ON:       TASK-A TASK-B\n:END:\n\n* BACKLOG TASK-A First dependency :work:\n:PROPERTIES:\n:ID:               TASK-A\n:END:\n\n* BACKLOG TASK-B Second dependency :work:\n:PROPERTIES:\n:ID:               TASK-B\n:END:\n\n* BACKLOG TASK-002 Unblocked task :work:\n:PROPERTIES:\n:ID:               TASK-002\n:END:\n",
        );

        let index = Index::new(home);
        index.rebuild().await;
        let snap = index.snapshot().await;
        let project = snap.project("proj-x").unwrap();
        assert_eq!(
            project.tasks[0].depends_on,
            vec!["TASK-A".to_string(), "TASK-B".to_string()]
        );
        assert_eq!(project.tasks[1].depends_on, Vec::<String>::new());

        write(
            &project_root.join(".orgasmic/tasks/backlog.org"),
            "#+title: x sprint\n#+orgasmic_version: 1\n\n* BACKLOG TASK-001 Blocked task :work:\n:PROPERTIES:\n:ID:               TASK-001\n:DEPENDS_ON:       TASK-C\n:END:\n\n* BACKLOG TASK-C Refreshed dependency :work:\n:PROPERTIES:\n:ID:               TASK-C\n:END:\n",
        );
        index.refresh_project("proj-x").await.unwrap();
        let snap = index.snapshot().await;
        let project = snap.project("proj-x").unwrap();
        assert_eq!(project.tasks[0].depends_on, vec!["TASK-C".to_string()]);
    }

    #[tokio::test]
    async fn graph_indexes_first_class_edges_and_queryable_inverses() {
        let (tmp, home) = make_home();
        let project_root = tmp.path().join("proj");
        seed_project(&project_root);
        seed_board(&home, &project_root, "proj-x");
        write(
            &project_root.join(".orgasmic/tasks/backlog.org"),
            "#+title: x sprint\n#+orgasmic_version: 1\n\n* BACKLOG TASK-BKC12 Blocked task :work:\n:PROPERTIES:\n:ID:               TASK-BKC12\n:DEPENDS_ON:       TASK-RDY12\n:IMPLEMENTS:       arch_APP12\n:PRODUCES:         crates/example.rs\n:END:\n\n* BACKLOG TASK-RDY12 Ready dependency :work:\n:PROPERTIES:\n:ID:               TASK-RDY12\n:END:\n",
        );
        write(
            &project_root.join(".orgasmic/decisions.org"),
            "#+title: decisions\n#+orgasmic_version: 1\n\n* dec_KEEP1 Keep it\n:PROPERTIES:\n:ID:               dec_KEEP1\n:END:\n",
        );
        write(
            &project_root.join(".orgasmic/architecture.org"),
            "#+title: architecture\n#+orgasmic_version: 1\n\n* arch_APP12 App\n:PROPERTIES:\n:ID:               arch_APP12\n:MOTIVATED_BY:     dec_KEEP1\n:DEPENDS_ON:       arch_STR12\n:END:\n\n* arch_STR12 Store\n:PROPERTIES:\n:ID:               arch_STR12\n:END:\n",
        );

        let index = Index::new(home);
        index.rebuild().await;
        let snap = index.snapshot().await;
        let project = snap.project("proj-x").unwrap();
        let edges = &project.graph.edges;
        assert!(edges.contains(&GraphEdgeSummary {
            kind: "depends_on".to_string(),
            from: "TASK-BKC12".to_string(),
            to: "TASK-RDY12".to_string(),
        }));
        assert!(edges.contains(&GraphEdgeSummary {
            kind: "implements".to_string(),
            from: "TASK-BKC12".to_string(),
            to: "arch_APP12".to_string(),
        }));
        assert!(edges.contains(&GraphEdgeSummary {
            kind: "produces".to_string(),
            from: "TASK-BKC12".to_string(),
            to: "crates/example.rs".to_string(),
        }));
        assert!(edges.contains(&GraphEdgeSummary {
            kind: "motivated_by".to_string(),
            from: "arch_APP12".to_string(),
            to: "dec_KEEP1".to_string(),
        }));
        assert!(edges.contains(&GraphEdgeSummary {
            kind: "depends_on".to_string(),
            from: "arch_APP12".to_string(),
            to: "arch_STR12".to_string(),
        }));
        let implemented_by: Vec<_> = edges
            .iter()
            .filter(|edge| edge.kind == "implements" && edge.to == "arch_APP12")
            .map(|edge| edge.from.as_str())
            .collect();
        assert_eq!(implemented_by, vec!["TASK-BKC12"]);
        let motivates: Vec<_> = edges
            .iter()
            .filter(|edge| edge.kind == "motivated_by" && edge.to == "dec_KEEP1")
            .map(|edge| edge.from.as_str())
            .collect();
        assert_eq!(motivates, vec!["arch_APP12"]);
        assert!(snap.parse_errors.is_empty());
    }

    #[tokio::test]
    async fn dangling_first_class_edge_targets_surface_as_parse_errors() {
        let (tmp, home) = make_home();
        let project_root = tmp.path().join("proj");
        seed_project(&project_root);
        seed_board(&home, &project_root, "proj-x");
        write(
            &project_root.join(".orgasmic/tasks/backlog.org"),
            "#+title: x sprint\n#+orgasmic_version: 1\n\n* BACKLOG TASK-BKC12 Blocked task :work:\n:PROPERTIES:\n:ID:               TASK-BKC12\n:DEPENDS_ON:       TASK-MSS12\n:END:\n",
        );

        let index = Index::new(home);
        index.rebuild().await;
        let snap = index.snapshot().await;
        let project = snap.project("proj-x").unwrap();
        assert!(project.graph.edges.contains(&GraphEdgeSummary {
            kind: "depends_on".to_string(),
            from: "TASK-BKC12".to_string(),
            to: "TASK-MSS12".to_string(),
        }));
        assert!(snap.parse_errors.iter().any(|error| {
            error.message.contains("dangling target TASK-MSS12")
                && matches!(error.kind, ParseErrorKind::WorkingFile)
        }));
    }

    #[tokio::test]
    async fn dangling_motivated_by_decision_target_surfaces_as_parse_error() {
        let (tmp, home) = make_home();
        let project_root = tmp.path().join("proj");
        seed_project(&project_root);
        seed_board(&home, &project_root, "proj-x");
        write(
            &project_root.join(".orgasmic/architecture.org"),
            "#+title: architecture\n#+orgasmic_version: 1\n\n* arch_APP12 App\n:PROPERTIES:\n:ID:               arch_APP12\n:MOTIVATED_BY:     dec_GONE99\n:DEPENDS_ON:       arch_STR12\n:END:\n\n* arch_STR12 Store\n:PROPERTIES:\n:ID:               arch_STR12\n:END:\n",
        );

        let index = Index::new(home);
        index.rebuild().await;
        let snap = index.snapshot().await;
        let project = snap.project("proj-x").unwrap();
        assert!(project.graph.edges.contains(&GraphEdgeSummary {
            kind: "motivated_by".to_string(),
            from: "arch_APP12".to_string(),
            to: "dec_GONE99".to_string(),
        }));
        assert!(snap.parse_errors.iter().any(|error| {
            error.message.contains("dangling target dec_GONE99")
                && matches!(error.kind, ParseErrorKind::WorkingFile)
        }));
    }

    #[tokio::test]
    async fn dangling_reference_property_is_attributable_to_project_file_node_and_property() {
        let (tmp, home) = make_home();
        let project_root = tmp.path().join("proj");
        seed_project(&project_root);
        seed_board(&home, &project_root, "proj-x");
        let glossary = project_root.join(".orgasmic/glossary.org");
        write(
            &glossary,
            "#+title: glossary\n#+orgasmic_version: 1\n\n* term_A A term\n:PROPERTIES:\n:ID:               term_A\n:RELATES_TO:       missing-slug\n:END:\n",
        );

        let index = Index::new(home);
        index.rebuild().await;
        let snap = index.snapshot().await;
        let error = snap
            .parse_errors
            .iter()
            .find(|e| e.message.contains("dangling reference `missing-slug`"))
            .expect("dangling RELATES_TO reference surfaces as a parse error");
        assert_eq!(error.path, glossary, "file attribution");
        assert!(
            error.message.contains(":RELATES_TO: on term_A"),
            "property/node attribution embedded in message: {}",
            error.message
        );
        assert_eq!(
            snap.parse_error_project_id(error),
            Some("proj-x"),
            "project attribution derived from the error's path"
        );
    }

    #[tokio::test]
    async fn duplicate_dangling_edge_tokens_are_recorded_once_per_pass_not_n_times() {
        let (tmp, home) = make_home();
        let project_root = tmp.path().join("proj");
        seed_project(&project_root);
        seed_board(&home, &project_root, "proj-x");
        write(
            &project_root.join(".orgasmic/tasks/backlog.org"),
            "#+title: x sprint\n#+orgasmic_version: 1\n\n* BACKLOG TASK-BKC12 Blocked task :work:\n:PROPERTIES:\n:ID:               TASK-BKC12\n:DEPENDS_ON:       TASK-MSS12 TASK-MSS12\n:END:\n",
        );

        let index = Index::new(home);
        index.rebuild().await;
        let snap = index.snapshot().await;
        let matching = |errors: &[ParseError]| {
            errors
                .iter()
                .filter(|e| e.message.contains("dangling target TASK-MSS12"))
                .count()
        };
        assert_eq!(
            matching(&snap.parse_errors),
            1,
            "a repeated identical dangling-edge token must be recorded once per pass, not N times"
        );

        // A second reindex pass over the same unfixed content still records
        // it exactly once — dedup is per-pass, not a one-time suppression
        // (TASK-V8WY9).
        index.refresh_project("proj-x").await.unwrap();
        let snap = index.snapshot().await;
        assert_eq!(
            matching(&snap.parse_errors),
            1,
            "second pass must still record it exactly once"
        );
    }

    #[tokio::test]
    async fn reindex_clears_project_parse_error_count_after_fix_without_restart() {
        let (tmp, home) = make_home();
        let project_root = tmp.path().join("proj");
        seed_project(&project_root);
        seed_board(&home, &project_root, "proj-x");
        let glossary = project_root.join(".orgasmic/glossary.org");
        write(
            &glossary,
            "#+title: glossary\n#+orgasmic_version: 1\n\n* term_A A term\n:PROPERTIES:\n:ID:               term_A\n:RELATES_TO:       missing-slug\n:END:\n",
        );

        let index = Index::new(home);
        index.rebuild().await;
        let snap = index.snapshot().await;
        assert_eq!(
            snap.parse_error_counts_by_project().get("proj-x").copied(),
            Some(1)
        );

        // Fix the dangling reference on disk and reindex just this project —
        // no daemon restart — and confirm the count drops to zero.
        write(
            &glossary,
            "#+title: glossary\n#+orgasmic_version: 1\n\n* term_A A term\n:PROPERTIES:\n:ID:               term_A\n:END:\n",
        );
        index.refresh_project("proj-x").await.unwrap();
        let snap = index.snapshot().await;
        assert_eq!(
            snap.parse_error_counts_by_project().get("proj-x").copied(),
            Some(0)
        );
    }

    #[tokio::test]
    async fn refresh_keeps_last_good_indexed_task_body_after_parse_failure() {
        let (tmp, home) = make_home();
        let project_root = tmp.path().join("proj");
        seed_project(&project_root);
        seed_board(&home, &project_root, "proj-x");

        let index = Index::new(home);
        index.rebuild().await;
        write(
            &project_root.join(".orgasmic/tasks/backlog.org"),
            "#+title: broken\n#+orgasmic_version: 1\n\n* BACKLOG TASK-001 Broken\n:PROPERTIES:\n:ID:               TASK-001\n",
        );
        index.refresh_project("proj-x").await.unwrap();

        let snap = index.snapshot().await;
        let project = snap.project("proj-x").unwrap();
        assert_eq!(project.tasks.len(), 1);
        let detail = TaskDetail::from_indexed_body(
            project.tasks[0].clone(),
            project.task_bodies.get("TASK-001").cloned(),
        );
        assert_eq!(detail.body.description, "Seeded detail.");
        assert_eq!(
            detail.body.acceptance_criteria,
            vec![AcceptanceItem {
                state: AcceptanceState::Unchecked,
                text: "Body fields load.".to_string(),
            }]
        );
    }

    #[test]
    fn task_body_parser_extracts_common_sections() {
        let raw = "\
#+title: sprint
#+orgasmic_version: 1

* IN_PROGRESS TASK-045 Dispatch CLI wrapper :ui:
:PROPERTIES:
:ID:               TASK-045
:END:

** Description
First paragraph.

Second paragraph.

** Acceptance Criteria
- [X] Existing manager dispatches through the CLI.
- [ ] Future dispatches preserve the wrapped CLI contract.
- [-] Awkward bits are captured for polish.

** Evidence
- cli smoke passes
- manager handoff recorded

** Notes
Keep this scoped.

** Worklog
- started parser pass
- finished UI pass

** Reviewer pass
- reviewer accepts
";
        let file = OrgFile::parse(raw, "backlog.org").unwrap();
        let heading = file.find_by_id("TASK-045").unwrap();
        let body = parse_task_body(&file, heading);

        assert_eq!(body.description, "First paragraph.\n\nSecond paragraph.");
        assert_eq!(
            body.acceptance_criteria,
            vec![
                AcceptanceItem {
                    state: AcceptanceState::Checked,
                    text: "Existing manager dispatches through the CLI.".to_string(),
                },
                AcceptanceItem {
                    state: AcceptanceState::Unchecked,
                    text: "Future dispatches preserve the wrapped CLI contract.".to_string(),
                },
                AcceptanceItem {
                    state: AcceptanceState::Partial,
                    text: "Awkward bits are captured for polish.".to_string(),
                },
            ]
        );
        assert_eq!(
            body.evidence,
            vec![
                "cli smoke passes".to_string(),
                "manager handoff recorded".to_string()
            ]
        );
        assert_eq!(body.notes, "Keep this scoped.");
        assert_eq!(
            body.worklog,
            vec![
                "started parser pass".to_string(),
                "finished UI pass".to_string()
            ]
        );
        assert_eq!(body.reviewer_pass, vec!["reviewer accepts".to_string()]);
    }

    #[test]
    fn task_body_parser_folds_pre_section_text_into_description() {
        let raw = "\
#+title: sprint
#+orgasmic_version: 1

* BACKLOG TASK-010 Preserve preamble :work:
:PROPERTIES:
:ID:               TASK-010
:END:

This prose sits before any named section.

** Description
Named description.
";
        let file = OrgFile::parse(raw, "backlog.org").unwrap();
        let heading = file.find_by_id("TASK-010").unwrap();
        let body = parse_task_body(&file, heading);

        assert_eq!(
            body.description,
            "This prose sits before any named section.\n\nNamed description."
        );
    }

    #[test]
    fn acceptance_parser_ignores_non_checkbox_prose() {
        let raw = "\
#+title: sprint
#+orgasmic_version: 1

* BACKLOG TASK-011 Preserve acceptance prose :work:
:PROPERTIES:
:ID:               TASK-011
:END:

** Acceptance Criteria
The following criteria must hold before close.
- [X] First criterion passes.
- plain bullet prose
- [ ] Second criterion remains open.
";
        let file = OrgFile::parse(raw, "backlog.org").unwrap();
        let heading = file.find_by_id("TASK-011").unwrap();
        let body = parse_task_body(&file, heading);

        assert_eq!(
            body.acceptance_criteria,
            vec![
                AcceptanceItem {
                    state: AcceptanceState::Checked,
                    text: "First criterion passes.".to_string(),
                },
                AcceptanceItem {
                    state: AcceptanceState::Unchecked,
                    text: "Second criterion remains open.".to_string(),
                },
            ]
        );
    }

    #[tokio::test]
    async fn refresh_project_indexes_single_id_marker() {
        let (tmp, home) = make_home();
        let project_root = tmp.path().join("proj");
        seed_project(&project_root);
        seed_board(&home, &project_root, "proj-x");
        write(
            &project_root.join("src/lib.rs"),
            "pub fn old() {}\n// orgasmic:dec_001\n",
        );

        let index = Index::new(home);
        index.rebuild().await;
        write(
            &project_root.join("src/lib.rs"),
            "pub fn new() {}\n// orgasmic:dec_002\n",
        );
        index.refresh_project("proj-x").await.unwrap();
        let snap = index.snapshot().await;
        let project = snap.project("proj-x").unwrap();

        assert_eq!(
            project.markers.get("dec_002").unwrap(),
            &vec![PathBuf::from("src/lib.rs")]
        );
        assert!(!project.markers.contains_key("dec_001"));
    }

    #[tokio::test]
    async fn rebuild_indexes_multi_id_marker() {
        let (tmp, home) = make_home();
        let project_root = tmp.path().join("proj");
        seed_project(&project_root);
        seed_board(&home, &project_root, "proj-x");
        write(
            &project_root.join("crates/orgasmic-core/src/org.rs"),
            "// orgasmic:arch_003,dec_004\n",
        );

        let index = Index::new(home);
        index.rebuild().await;
        let snap = index.snapshot().await;
        let project = snap.project("proj-x").unwrap();

        assert_eq!(
            project.markers.get("arch_003").unwrap(),
            &vec![PathBuf::from("crates/orgasmic-core/src/org.rs")]
        );
        assert_eq!(
            project.markers.get("dec_004").unwrap(),
            &vec![PathBuf::from("crates/orgasmic-core/src/org.rs")]
        );
    }

    #[tokio::test]
    async fn rebuild_strips_marker_option_suffix() {
        let (tmp, home) = make_home();
        let project_root = tmp.path().join("proj");
        seed_project(&project_root);
        seed_board(&home, &project_root, "proj-x");
        write(
            &project_root.join("src/choice.ts"),
            "  // orgasmic:dec_007:opt_a\n",
        );

        let index = Index::new(home);
        index.rebuild().await;
        let snap = index.snapshot().await;
        let project = snap.project("proj-x").unwrap();

        assert_eq!(
            project.markers.get("dec_007").unwrap(),
            &vec![PathBuf::from("src/choice.ts")]
        );
        assert!(!project.markers.contains_key("dec_007:opt_a"));
    }

    #[tokio::test]
    async fn marker_scan_skips_hardcoded_directories() {
        let (tmp, home) = make_home();
        let project_root = tmp.path().join("proj");
        seed_project(&project_root);
        seed_board(&home, &project_root, "proj-x");
        write(&project_root.join("src/lib.rs"), "// orgasmic:dec_kept\n");
        write(
            &project_root.join("target/foo.rs"),
            "// orgasmic:dec_skipped\n",
        );

        let index = Index::new(home);
        index.rebuild().await;
        let snap = index.snapshot().await;
        let project = snap.project("proj-x").unwrap();

        assert!(project.markers.contains_key("dec_kept"));
        assert!(!project.markers.contains_key("dec_skipped"));
    }

    #[tokio::test]
    async fn marker_files_for_unknown_node_is_empty() {
        let (tmp, home) = make_home();
        let project_root = tmp.path().join("proj");
        seed_project(&project_root);
        seed_board(&home, &project_root, "proj-x");
        write(&project_root.join("src/lib.rs"), "// orgasmic:dec_known\n");

        let index = Index::new(home);
        index.rebuild().await;
        let snap = index.snapshot().await;

        assert!(snap.marker_files("dec_missing").is_empty());
    }

    #[tokio::test]
    async fn parse_failure_keeps_last_good() {
        let (tmp, home) = make_home();
        let project_root = tmp.path().join("proj");
        seed_project(&project_root);
        seed_board(&home, &project_root, "proj-x");
        let index = Index::new(home.clone());
        index.rebuild().await;
        let before = index.snapshot().await.project("proj-x").unwrap().clone();
        assert_eq!(before.tasks.len(), 1);

        // Now corrupt the sprint file with a broken property drawer.
        let sprint = project_root.join(".orgasmic/tasks/backlog.org");
        std::fs::write(
            &sprint,
            "#+title: x\n\n* BACKLOG TASK-001 oops\n:PROPERTIES:\nno-end-marker",
        )
        .unwrap();

        index.refresh_project("proj-x").await.unwrap();
        let snap = index.snapshot().await;
        let project = snap.project("proj-x").unwrap();
        assert_eq!(
            project.tasks.len(),
            1,
            "last-good projection should be preserved"
        );
        assert!(snap.parse_errors.iter().any(|e| e.path == sprint));
    }

    #[tokio::test]
    async fn rebuild_indexes_task_activity_from_project_tx() {
        let (tmp, home) = make_home();
        let project_root = tmp.path().join("proj");
        seed_project(&project_root);
        seed_board(&home, &project_root, "proj-x");
        write(
            &project_root.join(".orgasmic/tx/2026-05.org"),
            "#+title: orgasmic project tx 2026-05\n#+orgasmic_version: 1\n\n* TX 2026-05-21 20:00:00 task.state_transitioned TASK-001\n:PROPERTIES:\n:TX_ID:        tx-activity-1\n:TIME:         [2026-05-21 Thu 20:00:00]\n:TYPE:         task.state_transitioned\n:ACTOR:        dev@example.com\n:MACHINE:      host\n:PROJECT:      proj-x\n:TASK:         TASK-001\n:FROM_STATE:   backlog\n:TO_STATE:     in_progress\n:END:\n",
        );

        let index = Index::new(home);
        index.rebuild().await;
        let snap = index.snapshot().await;
        let project = snap.project("proj-x").unwrap();
        let activity = project.activity_index.get("TASK-001").unwrap();
        assert_eq!(activity.len(), 1);
        assert_eq!(activity[0].tx_id, "tx-activity-1");
        assert_eq!(activity[0].kind, ActivityKind::StateTransition);
        assert_eq!(activity[0].body, "backlog -> in_progress");
    }

    #[tokio::test]
    async fn goal_liveness_property_is_reported_as_parse_error() {
        let (tmp, home) = make_home();
        let project_root = tmp.path().join("proj");
        seed_project(&project_root);
        seed_board(&home, &project_root, "proj-x");
        write(
            &project_root.join(".orgasmic/tasks/goal.org"),
            "#+title: Goal\n#+orgasmic_version: 1\n\n* GOAL Ship the thing\n:PROPERTIES:\n:ID:               goal-ship\n:STATUS:           active\n:LIVENESS:         abc1234\n:LIVENESS_AT:      [2026-06-11 Thu]\n:END:\n\n** Statement\nShip.\n",
        );
        // Liveness on the handoff heading is the convention, not an error.
        write(
            &project_root.join(".orgasmic/tasks/handoff.org"),
            "#+title: Handoff\n#+orgasmic_version: 1\n\n* HANDOFF current\n:PROPERTIES:\n:ID:               handoff-current\n:GOAL_ID:          goal-ship\n:LIVENESS:         abc1234\n:LIVENESS_AT:      [2026-06-11 Thu]\n:END:\n",
        );

        let index = Index::new(home);
        index.rebuild().await;
        let snap = index.snapshot().await;
        let goal_errors: Vec<_> = snap
            .parse_errors
            .iter()
            .filter(|error| error.message.contains("liveness bookkeeping"))
            .collect();
        assert_eq!(goal_errors.len(), 1, "{:?}", snap.parse_errors);
        assert!(goal_errors[0].path.ends_with("goal.org"));
        assert!(goal_errors[0].message.contains(":LIVENESS: :LIVENESS_AT:"));
        assert!(goal_errors[0].message.contains("Ship the thing"));
    }

    #[tokio::test]
    async fn orphan_parent_task_is_reported_as_parse_error() {
        let (tmp, home) = make_home();
        let project_root = tmp.path().join("proj");
        seed_project(&project_root);
        seed_board(&home, &project_root, "proj-x");
        write(
            &project_root.join(".orgasmic/tasks/backlog.org"),
            "#+title: x sprint\n#+orgasmic_version: 1\n\n* BACKLOG TASK-999.1 Orphan\n:PROPERTIES:\n:ID:               TASK-999.1\n:END:\n",
        );

        let index = Index::new(home);
        index.rebuild().await;
        let snap = index.snapshot().await;
        assert!(snap
            .parse_errors
            .iter()
            .any(|error| error.message.contains("orphan derived parent TASK-999")));
    }

    #[tokio::test]
    async fn decision_tree_derives_paths_depth_and_ordering() {
        let (tmp, home) = make_home();
        let project_root = tmp.path().join("proj");
        seed_project(&project_root);
        seed_board(&home, &project_root, "proj-x");
        write(
            &project_root.join(".orgasmic/decisions.org"),
            "#+title: decisions\n#+orgasmic_version: 1\n\n\
* dec_AAAAA First root\n\
:PROPERTIES:\n\
:ID:                 dec_AAAAA\n\
:END:\n\
\n\
* dec_BBBBB Second root\n\
:PROPERTIES:\n\
:ID:                 dec_BBBBB\n\
:END:\n\
\n\
* dec_CCCCC Child one\n\
:PROPERTIES:\n\
:ID:                 dec_CCCCC\n\
:PARENT:             dec_BBBBB\n\
:END:\n\
\n\
* dec_DDDDD Child two\n\
:PROPERTIES:\n\
:ID:                 dec_DDDDD\n\
:PARENT:             dec_BBBBB\n\
:END:\n\
\n\
* dec_EEEEE Grandchild\n\
:PROPERTIES:\n\
:ID:                 dec_EEEEE\n\
:PARENT:             dec_DDDDD\n\
:END:\n",
        );

        let index = Index::new(home);
        index.rebuild().await;
        let snap = index.snapshot().await;
        assert!(
            snap.parse_errors
                .iter()
                .all(|error| !error.message.contains(":PARENT:")),
            "{:?}",
            snap.parse_errors
        );
        let graph = &snap.project("proj-x").unwrap().graph;
        let root_b = graph.decision_tree.get("dec_BBBBB").unwrap();
        assert_eq!(root_b.path, "2");
        assert_eq!(root_b.depth, 0);
        assert_eq!(root_b.children, vec!["dec_CCCCC", "dec_DDDDD"]);
        assert_eq!(graph.decision_tree.get("dec_CCCCC").unwrap().path, "2.1");
        assert_eq!(graph.decision_tree.get("dec_DDDDD").unwrap().path, "2.2");
        assert_eq!(graph.decision_tree.get("dec_EEEEE").unwrap().path, "2.2.1");
        assert_eq!(graph.decision_tree.get("dec_EEEEE").unwrap().depth, 2);
    }

    #[tokio::test]
    async fn decision_tree_orphan_parent_is_reported_as_parse_error() {
        let (tmp, home) = make_home();
        let project_root = tmp.path().join("proj");
        seed_project(&project_root);
        seed_board(&home, &project_root, "proj-x");
        write(
            &project_root.join(".orgasmic/decisions.org"),
            "#+title: decisions\n#+orgasmic_version: 1\n\n\
* dec_RPHN1 Orphan child\n\
:PROPERTIES:\n\
:ID:                 dec_RPHN1\n\
:PARENT:             dec_GHST1\n\
:END:\n",
        );

        let index = Index::new(home);
        index.rebuild().await;
        let snap = index.snapshot().await;
        assert!(snap.parse_errors.iter().any(|error| {
            error
                .message
                .contains("decision tree :PARENT: error: dec_RPHN1 has orphan parent dec_GHST1")
        }));
    }

    #[tokio::test]
    async fn superseded_decision_parent_with_live_children_stays_in_tree() {
        let (tmp, home) = make_home();
        let project_root = tmp.path().join("proj");
        seed_project(&project_root);
        seed_board(&home, &project_root, "proj-x");
        write(
            &project_root.join(".orgasmic/decisions.org"),
            "#+title: decisions\n#+orgasmic_version: 1\n\n\
* dec_AAAAA Parent now superseded\n\
:PROPERTIES:\n\
:ID:                 dec_AAAAA\n\
:END:\n\
\n\
* dec_BBBBB Replacement\n\
:PROPERTIES:\n\
:ID:                 dec_BBBBB\n\
:SUPERSEDES:         dec_AAAAA\n\
:END:\n\
\n\
* dec_CCCCC Live child\n\
:PROPERTIES:\n\
:ID:                 dec_CCCCC\n\
:PARENT:             dec_AAAAA\n\
:END:\n",
        );

        let index = Index::new(home);
        index.rebuild().await;
        let snap = index.snapshot().await;
        let graph = &snap.project("proj-x").unwrap().graph;
        let old = graph
            .decisions
            .iter()
            .find(|decision| decision.id == "dec_AAAAA")
            .unwrap();
        assert!(old.superseded);
        assert_eq!(old.children, vec!["dec_CCCCC"]);
        assert_eq!(graph.decision_tree.get("dec_CCCCC").unwrap().path, "1.1");
    }

    // orgasmic:dec_KTF04
    #[tokio::test]
    async fn superseded_flag_derived_from_supersedes_backrefs() {
        let (tmp, home) = make_home();
        let project_root = tmp.path().join("proj");
        seed_project(&project_root);
        seed_board(&home, &project_root, "proj-x");
        write(
            &project_root.join(".orgasmic/decisions.org"),
            "#+title: decisions\n#+orgasmic_version: 1\n\n\
* dec_X Old decision :history:\n\
:PROPERTIES:\n\
:ID:                 dec_X\n\
:END:\n\
** Decision\nThe old way.\n\n\
* dec_Y Replacement decision :current:\n\
:PROPERTIES:\n\
:ID:                 dec_Y\n\
:SUPERSEDES:         dec_X\n\
:END:\n\
** Decision\nThe new way.\n",
        );

        let index = Index::new(home);
        index.rebuild().await;
        let snap = index.snapshot().await;
        let project = snap.project("proj-x").unwrap();

        let dec_x = project
            .graph
            .decisions
            .iter()
            .find(|d| d.id == "dec_X")
            .unwrap();
        let dec_y = project
            .graph
            .decisions
            .iter()
            .find(|d| d.id == "dec_Y")
            .unwrap();
        assert!(
            dec_x.superseded,
            "dec_X must be superseded (dec_Y points at it)"
        );
        assert!(!dec_y.superseded, "dec_Y must not be superseded");

        let node_x = project
            .graph
            .nodes
            .iter()
            .find(|n| n.id == "dec_X")
            .unwrap();
        let node_y = project
            .graph
            .nodes
            .iter()
            .find(|n| n.id == "dec_Y")
            .unwrap();
        assert!(node_x.superseded, "graph node dec_X must be superseded");
        assert!(
            !node_y.superseded,
            "graph node dec_Y must not be superseded"
        );
    }

    #[tokio::test]
    async fn loads_home_tx_files() {
        let (_tmp, home) = make_home();
        let tx_path = home.tx().join("2026-05.org");
        write(
            &tx_path,
            "#+title: orgasmic tx 2026-05\n#+orgasmic_version: 1\n\n* TX 2026-05-21 19:00:00 test.event\n:PROPERTIES:\n:TX_ID:        tx-1\n:TIME:         [2026-05-21 Thu 19:00:00]\n:TYPE:         test.event\n:ACTOR:        a@example.com\n:MACHINE:      host\n:END:\n",
        );
        let index = Index::new(home);
        index.rebuild().await;
        let snap = index.snapshot().await;
        assert_eq!(snap.tx.len(), 1);
        assert_eq!(snap.tx[0].entry.tx_id, "tx-1");
    }
}

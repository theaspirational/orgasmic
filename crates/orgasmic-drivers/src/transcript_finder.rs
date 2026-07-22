// orgasmic:TASK-0SADP, dec_WDR5K
//! Per-harness adapters that locate a harness's own on-disk session transcript
//! for an orgasmic run (dec_WDR5K item 7).
//!
//! Correlation strategies (and confidence):
//! - **claude**: recorded `NativeRuntime.session_path`, else deterministic
//!   `~/.claude/projects/<encoded-cwd>/<session-id>.jsonl` from spawn-pinned
//!   session id + worktree cwd → **high** when the file exists.
//! - **codex**: recorded path (confined to `~/.codex`), else
//!   `thread_id`/`session_id` → rollout filename → **high**; unique
//!   cwd+originator with rollout `session_meta.timestamp` at/just after run
//!   start (one-sided, tight window) → **medium**; far or pre-run rollouts →
//!   **not found** (never attribute a later attempt's rollout to an earlier run).
//! - **cursor-agent**: recorded path (confined to `~/.cursor/projects`), else
//!   Ready `session_id` + worktree → **high**; tmux/rmux TUI without Ready id
//!   uses unique cwd+launch-time correlation under the encoded project slug →
//!   **medium**.
//! - **hermes**: recorded path (confined to `~/.hermes/sessions`), else Ready
//!   `session_id` → native `<id>.jsonl` (paired `session_<id>.json` is metadata
//!   only) → **high**; tmux/rmux TUI without Ready id uses unique launch-time
//!   correlation among `.jsonl` files → **medium**.
//! - **custom** (and unknown): honest **unsupported**, never a guess.

use std::fs;
use std::path::{Component, Path, PathBuf};

use chrono::{DateTime, Duration, Utc};
use orgasmic_core::{DriverEvent, Lifecycle, SessionEnvelope, SessionEventKind};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// How strongly the adapter believes the returned path belongs to this run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TranscriptConfidence {
    High,
    Medium,
}

/// Inputs gathered from an orgasmic run's session JSONL (and optional overrides).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptLookup {
    pub run_id: String,
    pub harness: String,
    pub cwd: Option<PathBuf>,
    pub session_id: Option<String>,
    pub recorded_session_path: Option<PathBuf>,
    pub run_started_at: Option<DateTime<Utc>>,
}

/// Overrideable on-disk roots so unit tests can use fixture trees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptRoots {
    pub claude_projects: PathBuf,
    pub codex_home: PathBuf,
    pub cursor_projects: PathBuf,
    pub hermes_sessions: PathBuf,
}

impl TranscriptRoots {
    pub fn from_home(home: impl AsRef<Path>) -> Self {
        let home = home.as_ref();
        Self {
            claude_projects: home.join(".claude").join("projects"),
            codex_home: home.join(".codex"),
            cursor_projects: home.join(".cursor").join("projects"),
            hermes_sessions: home.join(".hermes").join("sessions"),
        }
    }

    pub fn from_env_home() -> Option<Self> {
        std::env::var_os("HOME").map(|h| Self::from_home(PathBuf::from(h)))
    }
}

/// A resolved harness-native transcript path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeTranscriptHit {
    pub path: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub confidence: TranscriptConfidence,
    /// Short machine-readable correlation note (which strategy matched).
    pub correlation: String,
}

/// Result of looking up a native transcript for one run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum TranscriptFindResult {
    Found(NativeTranscriptHit),
    NotFound {
        reason: String,
    },
    Unsupported {
        harness: String,
        reason: String,
    },
    Ambiguous {
        candidates: Vec<PathBuf>,
        reason: String,
    },
}

/// Locate the harness-native session transcript for `lookup`.
pub fn find_native_transcript(
    lookup: &TranscriptLookup,
    roots: &TranscriptRoots,
) -> TranscriptFindResult {
    let harness = normalize_harness(&lookup.harness);
    match harness.as_str() {
        "custom" => TranscriptFindResult::Unsupported {
            harness: harness.clone(),
            reason: "custom harnesses have no transcript adapter (dec_WDR5K item 7)".into(),
        },
        "claude" => find_claude(lookup, roots),
        "codex" => find_codex(lookup, roots),
        "cursor-agent" => find_cursor(lookup, roots),
        "hermes" => find_hermes(lookup, roots),
        other => TranscriptFindResult::Unsupported {
            harness: other.to_string(),
            reason: format!("no transcript finder adapter for harness '{other}'"),
        },
    }
}

/// Build a [`TranscriptLookup`] from an orgasmic session JSONL (latest run segment).
pub fn lookup_from_envelopes(envelopes: &[SessionEnvelope]) -> Option<TranscriptLookup> {
    let envelopes = latest_run_segment(envelopes);
    let first = envelopes.first()?;
    let run_id = first.run_id.clone();
    let run_started_at = Some(first.time);

    let mut harness: Option<String> = None;
    let mut cwd: Option<PathBuf> = None;
    let mut session_id: Option<String> = None;
    let mut recorded_session_path: Option<PathBuf> = None;

    for envelope in envelopes {
        match envelope.kind {
            SessionEventKind::Lifecycle => {
                if let Ok(lc) = serde_json::from_value::<Lifecycle>(envelope.event.clone()) {
                    match lc {
                        Lifecycle::RunMeta {
                            harness: h,
                            worktree,
                            ..
                        } => {
                            if let Some(h) = h {
                                harness = Some(h);
                            }
                            if worktree.is_some() {
                                cwd = worktree;
                            }
                        }
                        Lifecycle::NativeRuntime {
                            provider,
                            session_id: sid,
                            session_path,
                            ..
                        } => {
                            if harness.is_none() {
                                harness = Some(provider);
                            }
                            if sid.is_some() {
                                session_id = sid;
                            }
                            if session_path.is_some() {
                                recorded_session_path = session_path;
                            }
                        }
                        _ => {}
                    }
                }
            }
            SessionEventKind::DriverEvent => {
                if let Ok(DriverEvent::Ready { capabilities, .. }) =
                    serde_json::from_value::<DriverEvent>(envelope.event.clone())
                {
                    if session_id.is_none() {
                        session_id = capability_session_id(&capabilities);
                    }
                }
            }
            _ => {}
        }
    }

    let harness = harness?;
    Some(TranscriptLookup {
        run_id,
        harness,
        cwd,
        session_id,
        recorded_session_path,
        run_started_at,
    })
}

fn normalize_harness(raw: &str) -> String {
    let t = raw.trim().to_ascii_lowercase();
    match t.as_str() {
        "cursor" => "cursor-agent".into(),
        other => other.to_string(),
    }
}

fn capability_session_id(capabilities: &Value) -> Option<String> {
    for key in ["session_id", "sessionId", "thread_id", "threadId"] {
        if let Some(id) = capabilities.get(key).and_then(Value::as_str) {
            let id = id.trim();
            if !id.is_empty() && id != "null" {
                return Some(id.to_string());
            }
        }
    }
    None
}

/// Max time after orgasmic run start for a codex rollout `session_meta.timestamp`.
const CODEX_CWD_LAUNCH_MAX_AFTER: Duration = Duration::minutes(5);

/// Max time after run start for tmux/rmux TUI transcript mtime correlation.
const TUI_TRANSCRIPT_LAUNCH_MAX_AFTER: Duration = Duration::minutes(10);

/// Recorded `NativeRuntime.session_path` values are not trusted blindly: they
/// must resolve to a regular file under the harness root (symlink escapes rejected).
fn validate_session_id_component(id: &str) -> bool {
    !id.is_empty()
        && !id.contains('/')
        && !id.contains('\\')
        && id != "."
        && id != ".."
        && Path::new(id)
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
}

fn canonical_root(root: &Path) -> PathBuf {
    root.canonicalize().unwrap_or_else(|_| root.to_path_buf())
}

fn path_is_under_root(candidate: &Path, root: &Path) -> bool {
    let Ok(canonical) = candidate.canonicalize() else {
        return false;
    };
    let root = canonical_root(root);
    canonical == root || canonical.starts_with(&root)
}

fn confined_file_under_root(candidate: &Path, root: &Path) -> Option<PathBuf> {
    if !candidate.is_file() {
        return None;
    }
    let canonical = candidate.canonicalize().ok()?;
    if path_is_under_root(&canonical, root) {
        Some(canonical)
    } else {
        None
    }
}

fn file_mtime_at_or_after_run(
    path: &Path,
    run_started_at: DateTime<Utc>,
    max_after: Duration,
) -> bool {
    let Ok(meta) = fs::metadata(path) else {
        return false;
    };
    let Ok(modified) = meta.modified() else {
        return false;
    };
    let modified: DateTime<Utc> = modified.into();
    if modified < run_started_at {
        return false;
    }
    modified - run_started_at <= max_after
}

fn latest_run_segment(envelopes: &[SessionEnvelope]) -> &[SessionEnvelope] {
    let Some(latest_run_id) = envelopes.last().map(|e| e.run_id.as_str()) else {
        return envelopes;
    };
    let start = envelopes
        .iter()
        .rposition(|e| e.run_id != latest_run_id)
        .map_or(0, |i| i + 1);
    &envelopes[start..]
}

fn found_recorded(
    path: &Path,
    harness_root: &Path,
    session_id: Option<&str>,
) -> Option<TranscriptFindResult> {
    let confined = confined_file_under_root(path, harness_root)?;
    Some(TranscriptFindResult::Found(NativeTranscriptHit {
        path: confined,
        session_id: session_id.map(str::to_string),
        confidence: TranscriptConfidence::High,
        correlation: "recorded_native_runtime_session_path".into(),
    }))
}

// --- claude -----------------------------------------------------------------

/// Claude encodes cwd as `~/.claude/projects/<slug>/` by replacing `/` and `.`
/// with `-` (leading slash becomes a leading `-`).
pub fn encode_claude_project_slug(cwd: &Path) -> String {
    cwd.to_string_lossy()
        .chars()
        .map(|c| if c == '/' || c == '.' { '-' } else { c })
        .collect()
}

pub fn claude_transcript_path(
    roots: &TranscriptRoots,
    cwd: &Path,
    session_id: &str,
) -> Option<PathBuf> {
    if !validate_session_id_component(session_id) {
        return None;
    }
    Some(
        roots
            .claude_projects
            .join(encode_claude_project_slug(cwd))
            .join(format!("{session_id}.jsonl")),
    )
}

fn find_claude(lookup: &TranscriptLookup, roots: &TranscriptRoots) -> TranscriptFindResult {
    if let Some(path) = lookup.recorded_session_path.as_deref() {
        if let Some(hit) =
            found_recorded(path, &roots.claude_projects, lookup.session_id.as_deref())
        {
            return hit;
        }
    }
    let Some(session_id) = lookup.session_id.as_deref() else {
        return TranscriptFindResult::NotFound {
            reason: "claude lookup needs session_id (NativeRuntime or --session-id)".into(),
        };
    };
    if !validate_session_id_component(session_id) {
        return TranscriptFindResult::NotFound {
            reason: "claude session_id must be a single filename component".into(),
        };
    }
    let Some(cwd) = lookup.cwd.as_deref() else {
        return TranscriptFindResult::NotFound {
            reason: "claude lookup needs worktree/cwd to encode ~/.claude/projects slug".into(),
        };
    };
    let Some(path) = claude_transcript_path(roots, cwd, session_id) else {
        return TranscriptFindResult::NotFound {
            reason: "claude session_id must be a single filename component".into(),
        };
    };
    if let Some(confined) = confined_file_under_root(&path, &roots.claude_projects) {
        TranscriptFindResult::Found(NativeTranscriptHit {
            path: confined,
            session_id: Some(session_id.to_string()),
            confidence: TranscriptConfidence::High,
            correlation: "claude_session_id_plus_encoded_cwd".into(),
        })
    } else {
        TranscriptFindResult::NotFound {
            reason: format!("claude transcript not found at {}", path.display()),
        }
    }
}

// --- cursor-agent -----------------------------------------------------------

/// Cursor strips a leading `/`, replaces `/` with `-`, and removes `.`.
pub fn encode_cursor_project_slug(cwd: &Path) -> String {
    cwd.to_string_lossy()
        .trim_start_matches('/')
        .chars()
        .filter(|c| *c != '.')
        .map(|c| if c == '/' { '-' } else { c })
        .collect()
}

pub fn cursor_transcript_path(
    roots: &TranscriptRoots,
    cwd: &Path,
    session_id: &str,
) -> Option<PathBuf> {
    if !validate_session_id_component(session_id) {
        return None;
    }
    Some(
        roots
            .cursor_projects
            .join(encode_cursor_project_slug(cwd))
            .join("agent-transcripts")
            .join(session_id)
            .join(format!("{session_id}.jsonl")),
    )
}

fn find_cursor_tui_by_cwd_time(
    lookup: &TranscriptLookup,
    roots: &TranscriptRoots,
) -> Option<TranscriptFindResult> {
    let cwd = lookup.cwd.as_deref()?;
    let started = lookup.run_started_at?;
    let transcripts_dir = roots
        .cursor_projects
        .join(encode_cursor_project_slug(cwd))
        .join("agent-transcripts");
    if !transcripts_dir.is_dir() {
        return None;
    }
    let Ok(entries) = fs::read_dir(&transcripts_dir) else {
        return None;
    };
    let mut matches = Vec::new();
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }
        let sid = entry.file_name().to_string_lossy().to_string();
        if !validate_session_id_component(&sid) {
            continue;
        }
        let path = entry.path().join(format!("{sid}.jsonl"));
        let Some(confined) = confined_file_under_root(&path, &roots.cursor_projects) else {
            continue;
        };
        if file_mtime_at_or_after_run(&confined, started, TUI_TRANSCRIPT_LAUNCH_MAX_AFTER) {
            matches.push((confined, sid));
        }
    }
    match matches.len() {
        0 => None,
        1 => {
            let (path, sid) = matches.into_iter().next().unwrap();
            Some(TranscriptFindResult::Found(NativeTranscriptHit {
                path,
                session_id: Some(sid),
                confidence: TranscriptConfidence::Medium,
                correlation: "cursor_tui_cwd_launch_time_unique".into(),
            }))
        }
        _ => Some(TranscriptFindResult::Ambiguous {
            candidates: matches.into_iter().map(|(p, _)| p).collect(),
            reason: "multiple cursor-agent TUI transcripts match cwd+launch-time window".into(),
        }),
    }
}

fn find_cursor(lookup: &TranscriptLookup, roots: &TranscriptRoots) -> TranscriptFindResult {
    if let Some(path) = lookup.recorded_session_path.as_deref() {
        if let Some(hit) =
            found_recorded(path, &roots.cursor_projects, lookup.session_id.as_deref())
        {
            return hit;
        }
    }
    let Some(session_id) = lookup.session_id.as_deref() else {
        if let Some(result) = find_cursor_tui_by_cwd_time(lookup, roots) {
            return result;
        }
        return TranscriptFindResult::NotFound {
            reason: "cursor-agent lookup needs session_id from Ready capabilities or a unique cwd+launch-time TUI correlation".into(),
        };
    };
    if !validate_session_id_component(session_id) {
        return TranscriptFindResult::NotFound {
            reason: "cursor-agent session_id must be a single filename component".into(),
        };
    }
    if let Some(cwd) = lookup.cwd.as_deref() {
        if let Some(path) = cursor_transcript_path(roots, cwd, session_id) {
            if let Some(confined) = confined_file_under_root(&path, &roots.cursor_projects) {
                return TranscriptFindResult::Found(NativeTranscriptHit {
                    path: confined,
                    session_id: Some(session_id.to_string()),
                    confidence: TranscriptConfidence::High,
                    correlation: "cursor_session_id_plus_encoded_cwd".into(),
                });
            }
        }
    }
    // Fallback: unique agent-transcripts/<id>/<id>.jsonl under projects.
    let mut matches = Vec::new();
    if roots.cursor_projects.is_dir() {
        if let Ok(projects) = fs::read_dir(&roots.cursor_projects) {
            for project in projects.flatten() {
                if !validate_session_id_component(session_id) {
                    continue;
                }
                let candidate = project
                    .path()
                    .join("agent-transcripts")
                    .join(session_id)
                    .join(format!("{session_id}.jsonl"));
                if let Some(confined) = confined_file_under_root(&candidate, &roots.cursor_projects)
                {
                    matches.push(confined);
                }
            }
        }
    }
    match matches.len() {
        0 => TranscriptFindResult::NotFound {
            reason: format!(
                "cursor-agent transcript for session_id '{session_id}' not found under {}",
                roots.cursor_projects.display()
            ),
        },
        1 => TranscriptFindResult::Found(NativeTranscriptHit {
            path: matches.remove(0),
            session_id: Some(session_id.to_string()),
            confidence: TranscriptConfidence::Medium,
            correlation: "cursor_session_id_unique_under_projects".into(),
        }),
        _ => TranscriptFindResult::Ambiguous {
            candidates: matches,
            reason: format!(
                "cursor-agent session_id '{session_id}' matched multiple project transcripts"
            ),
        },
    }
}

// --- codex ------------------------------------------------------------------

fn find_codex(lookup: &TranscriptLookup, roots: &TranscriptRoots) -> TranscriptFindResult {
    if let Some(path) = lookup.recorded_session_path.as_deref() {
        if let Some(hit) = found_recorded(path, &roots.codex_home, lookup.session_id.as_deref()) {
            return hit;
        }
    }
    if let Some(session_id) = lookup.session_id.as_deref() {
        if !validate_session_id_component(session_id) {
            return TranscriptFindResult::NotFound {
                reason: "codex session_id must be a single filename component".into(),
            };
        }
        let matches = find_codex_rollouts_by_session_id(roots, session_id);
        match matches.len() {
            0 => {}
            1 => {
                return TranscriptFindResult::Found(NativeTranscriptHit {
                    path: matches.into_iter().next().unwrap(),
                    session_id: Some(session_id.to_string()),
                    confidence: TranscriptConfidence::High,
                    correlation: "codex_thread_id_rollout_filename".into(),
                });
            }
            _ => {
                return TranscriptFindResult::Ambiguous {
                    candidates: matches,
                    reason: format!(
                        "codex session_id '{session_id}' matched multiple rollout files"
                    ),
                };
            }
        }
    }
    // Medium-confidence: unique session_meta cwd+originator with rollout starting
    // at/just after this run (never a later attempt's rollout for an earlier run).
    let Some(cwd) = lookup.cwd.as_deref() else {
        return TranscriptFindResult::NotFound {
            reason: "codex lookup needs thread_id/session_id or cwd for session_meta scan".into(),
        };
    };
    let Some(run_started_at) = lookup.run_started_at else {
        return TranscriptFindResult::NotFound {
            reason: "codex cwd correlation needs run start time from session JSONL".into(),
        };
    };
    let matches = find_codex_rollouts_by_cwd(roots, cwd, run_started_at);
    match matches.len() {
        0 => TranscriptFindResult::NotFound {
            reason: format!(
                "no codex rollout with session_meta.cwd == {} starting within {} after run start under {}",
                cwd.display(),
                CODEX_CWD_LAUNCH_MAX_AFTER,
                roots.codex_home.display()
            ),
        },
        1 => {
            let (path, session_id) = matches.into_iter().next().unwrap();
            TranscriptFindResult::Found(NativeTranscriptHit {
                path,
                session_id,
                confidence: TranscriptConfidence::Medium,
                correlation: "codex_session_meta_cwd_originator_launch_time".into(),
            })
        }
        _ => TranscriptFindResult::Ambiguous {
            candidates: matches.into_iter().map(|(p, _)| p).collect(),
            reason: format!(
                "multiple codex rollouts share cwd {} within launch window (need thread_id for high confidence)",
                cwd.display()
            ),
        },
    }
}

fn find_codex_rollouts_by_session_id(roots: &TranscriptRoots, session_id: &str) -> Vec<PathBuf> {
    let needle = format!("-{session_id}.jsonl");
    let mut out = Vec::new();
    for dir in [
        roots.codex_home.join("sessions"),
        roots.codex_home.join("archived_sessions"),
    ] {
        collect_files_with_suffix(&dir, &needle, &mut out);
    }
    out.sort();
    out.dedup();
    out
}

fn collect_files_with_suffix(dir: &Path, suffix: &str, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_files_with_suffix(&path, suffix, out);
        } else if path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.ends_with(suffix))
        {
            out.push(path);
        }
    }
}

fn find_codex_rollouts_by_cwd(
    roots: &TranscriptRoots,
    cwd: &Path,
    run_started_at: DateTime<Utc>,
) -> Vec<(PathBuf, Option<String>)> {
    let cwd_str = cwd.to_string_lossy();
    let mut out = Vec::new();
    for dir in [
        roots.codex_home.join("sessions"),
        roots.codex_home.join("archived_sessions"),
    ] {
        collect_codex_cwd_matches(&dir, &cwd_str, run_started_at, &mut out);
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out.dedup_by(|a, b| a.0 == b.0);
    out
}

fn collect_codex_cwd_matches(
    dir: &Path,
    cwd_str: &str,
    run_started_at: DateTime<Utc>,
    out: &mut Vec<(PathBuf, Option<String>)>,
) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_codex_cwd_matches(&path, cwd_str, run_started_at, out);
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !name.starts_with("rollout-") || !name.ends_with(".jsonl") {
            continue;
        }
        let Ok(meta) = read_codex_session_meta(&path) else {
            continue;
        };
        if meta.cwd.as_deref() != Some(cwd_str) {
            continue;
        }
        if meta.originator.as_deref() != Some("orgasmic") {
            continue;
        }
        let Some(ts) = meta.timestamp else {
            continue;
        };
        // One-sided: rollout must start at/just after this orgasmic run, never before.
        if ts < run_started_at {
            continue;
        }
        if ts - run_started_at > CODEX_CWD_LAUNCH_MAX_AFTER {
            continue;
        }
        out.push((path, meta.session_id));
    }
}

#[derive(Debug, Default)]
struct CodexSessionMeta {
    cwd: Option<String>,
    session_id: Option<String>,
    originator: Option<String>,
    timestamp: Option<DateTime<Utc>>,
}

fn read_codex_session_meta(path: &Path) -> Result<CodexSessionMeta, ()> {
    let text = fs::read_to_string(path).map_err(|_| ())?;
    let first = text.lines().next().ok_or(())?;
    let value: Value = serde_json::from_str(first).map_err(|_| ())?;
    if value.get("type").and_then(Value::as_str) != Some("session_meta") {
        return Err(());
    }
    let payload = value.get("payload").cloned().unwrap_or(Value::Null);
    let timestamp = payload
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&Utc))
        .or_else(|| {
            value
                .get("timestamp")
                .and_then(Value::as_str)
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                .map(|dt| dt.with_timezone(&Utc))
        });
    Ok(CodexSessionMeta {
        cwd: payload
            .get("cwd")
            .and_then(Value::as_str)
            .map(str::to_string),
        session_id: payload
            .get("session_id")
            .or_else(|| payload.get("id"))
            .and_then(Value::as_str)
            .map(str::to_string),
        originator: payload
            .get("originator")
            .and_then(Value::as_str)
            .map(str::to_string),
        timestamp,
    })
}

// --- hermes -----------------------------------------------------------------

fn hermes_transcript_candidates(roots: &TranscriptRoots, session_id: &str) -> Vec<PathBuf> {
    if !validate_session_id_component(session_id) {
        return Vec::new();
    }
    vec![
        roots.hermes_sessions.join(format!("{session_id}.jsonl")),
        roots
            .hermes_sessions
            .join(format!("session_{session_id}.jsonl")),
        roots
            .hermes_sessions
            .join(format!("session_{session_id}.json")),
        roots.hermes_sessions.join(format!("{session_id}.json")),
    ]
}

fn find_hermes_tui_by_launch_time(
    lookup: &TranscriptLookup,
    roots: &TranscriptRoots,
) -> Option<TranscriptFindResult> {
    let started = lookup.run_started_at?;
    let Ok(entries) = fs::read_dir(&roots.hermes_sessions) else {
        return None;
    };
    let mut matches = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some(sid) = name.strip_suffix(".jsonl") else {
            continue;
        };
        let sid = sid.strip_prefix("session_").unwrap_or(sid);
        if !validate_session_id_component(sid) {
            continue;
        }
        let Some(confined) = confined_file_under_root(&path, &roots.hermes_sessions) else {
            continue;
        };
        if file_mtime_at_or_after_run(&confined, started, TUI_TRANSCRIPT_LAUNCH_MAX_AFTER) {
            matches.push((confined, sid.to_string()));
        }
    }
    match matches.len() {
        0 => None,
        1 => {
            let (path, sid) = matches.into_iter().next().unwrap();
            Some(TranscriptFindResult::Found(NativeTranscriptHit {
                path,
                session_id: Some(sid),
                confidence: TranscriptConfidence::Medium,
                correlation: "hermes_tui_launch_time_unique".into(),
            }))
        }
        _ => Some(TranscriptFindResult::Ambiguous {
            candidates: matches.into_iter().map(|(p, _)| p).collect(),
            reason: "multiple hermes TUI transcripts match launch-time window".into(),
        }),
    }
}

fn find_hermes(lookup: &TranscriptLookup, roots: &TranscriptRoots) -> TranscriptFindResult {
    if let Some(path) = lookup.recorded_session_path.as_deref() {
        if let Some(hit) =
            found_recorded(path, &roots.hermes_sessions, lookup.session_id.as_deref())
        {
            return hit;
        }
    }
    let Some(session_id) = lookup.session_id.as_deref() else {
        if let Some(result) = find_hermes_tui_by_launch_time(lookup, roots) {
            return result;
        }
        return TranscriptFindResult::NotFound {
            reason: "hermes lookup needs session_id from Ready capabilities or a unique launch-time TUI correlation".into(),
        };
    };
    if !validate_session_id_component(session_id) {
        return TranscriptFindResult::NotFound {
            reason: "hermes session_id must be a single filename component".into(),
        };
    }
    // Prefer native JSONL; paired session_<id>.json is metadata, not a competing transcript.
    for candidate in hermes_transcript_candidates(roots, session_id) {
        if candidate
            .extension()
            .and_then(|e| e.to_str())
            .is_none_or(|e| e != "jsonl")
        {
            continue;
        }
        if let Some(confined) = confined_file_under_root(&candidate, &roots.hermes_sessions) {
            return TranscriptFindResult::Found(NativeTranscriptHit {
                path: confined,
                session_id: Some(session_id.to_string()),
                confidence: TranscriptConfidence::High,
                correlation: "hermes_session_id_jsonl".into(),
            });
        }
    }
    for candidate in hermes_transcript_candidates(roots, session_id) {
        if candidate
            .extension()
            .and_then(|e| e.to_str())
            .is_none_or(|e| e != "json")
        {
            continue;
        }
        if let Some(confined) = confined_file_under_root(&candidate, &roots.hermes_sessions) {
            return TranscriptFindResult::Found(NativeTranscriptHit {
                path: confined,
                session_id: Some(session_id.to_string()),
                confidence: TranscriptConfidence::High,
                correlation: "hermes_session_id_json_metadata".into(),
            });
        }
    }
    TranscriptFindResult::NotFound {
        reason: format!(
            "hermes session file for id '{session_id}' not found under {}",
            roots.hermes_sessions.display()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orgasmic_core::{RuntimeIdentity, SessionEventKind, SessionWriter};
    use serde_json::json;
    use std::io::Write;

    fn roots_under(tmp: &Path) -> TranscriptRoots {
        let roots = TranscriptRoots::from_home(tmp);
        fs::create_dir_all(&roots.claude_projects).unwrap();
        fs::create_dir_all(roots.codex_home.join("sessions")).unwrap();
        fs::create_dir_all(roots.codex_home.join("archived_sessions")).unwrap();
        fs::create_dir_all(&roots.cursor_projects).unwrap();
        fs::create_dir_all(&roots.hermes_sessions).unwrap();
        roots
    }

    #[test]
    fn claude_slug_matches_observed_layout() {
        let cwd = Path::new(
            "/Users/aspirational/Documents/code/tools/orgasmic/.orgasmic/tmp/dispatch/task-tjf7g/worktree",
        );
        assert_eq!(
            encode_claude_project_slug(cwd),
            "-Users-aspirational-Documents-code-tools-orgasmic--orgasmic-tmp-dispatch-task-tjf7g-worktree"
        );
    }

    #[test]
    fn cursor_slug_matches_observed_layout() {
        let cwd = Path::new(
            "/Users/aspirational/Documents/code/tools/orgasmic/.orgasmic/tmp/dispatch/task-0sadp/worktree",
        );
        assert_eq!(
            encode_cursor_project_slug(cwd),
            "Users-aspirational-Documents-code-tools-orgasmic-orgasmic-tmp-dispatch-task-0sadp-worktree"
        );
    }

    #[test]
    fn custom_is_unsupported() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = roots_under(tmp.path());
        let result = find_native_transcript(
            &TranscriptLookup {
                run_id: "run-1".into(),
                harness: "custom".into(),
                cwd: None,
                session_id: None,
                recorded_session_path: None,
                run_started_at: None,
            },
            &roots,
        );
        assert!(matches!(result, TranscriptFindResult::Unsupported { .. }));
    }

    #[test]
    fn claude_finds_by_session_id_and_cwd() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = roots_under(tmp.path());
        let cwd = PathBuf::from("/tmp/proj");
        let sid = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let path = claude_transcript_path(&roots, &cwd, sid).unwrap();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "{\"ok\":1}\n").unwrap();

        let result = find_native_transcript(
            &TranscriptLookup {
                run_id: "run-claude".into(),
                harness: "claude".into(),
                cwd: Some(cwd),
                session_id: Some(sid.into()),
                recorded_session_path: None,
                run_started_at: None,
            },
            &roots,
        );
        match result {
            TranscriptFindResult::Found(hit) => {
                assert_eq!(
                    hit.path.canonicalize().unwrap(),
                    path.canonicalize().unwrap()
                );
                assert_eq!(hit.confidence, TranscriptConfidence::High);
                assert_eq!(hit.correlation, "claude_session_id_plus_encoded_cwd");
            }
            other => panic!("expected Found, got {other:?}"),
        }
    }

    #[test]
    fn claude_prefers_recorded_path() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = roots_under(tmp.path());
        let recorded = roots.claude_projects.join("recorded-claude.jsonl");
        fs::write(&recorded, "{}\n").unwrap();
        let result = find_native_transcript(
            &TranscriptLookup {
                run_id: "run-claude".into(),
                harness: "claude".into(),
                cwd: None,
                session_id: Some("unused".into()),
                recorded_session_path: Some(recorded.clone()),
                run_started_at: None,
            },
            &roots,
        );
        match result {
            TranscriptFindResult::Found(hit) => {
                assert_eq!(
                    hit.path.canonicalize().unwrap(),
                    recorded.canonicalize().unwrap()
                );
                assert_eq!(hit.correlation, "recorded_native_runtime_session_path");
            }
            other => panic!("expected Found, got {other:?}"),
        }
    }

    #[test]
    fn cursor_finds_agent_transcript() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = roots_under(tmp.path());
        let cwd = PathBuf::from("/tmp/work/.hidden/tree");
        let sid = "11111111-2222-3333-4444-555555555555";
        let path = cursor_transcript_path(&roots, &cwd, sid).unwrap();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "{\"role\":\"user\"}\n").unwrap();

        let result = find_native_transcript(
            &TranscriptLookup {
                run_id: "run-cursor".into(),
                harness: "cursor-agent".into(),
                cwd: Some(cwd),
                session_id: Some(sid.into()),
                recorded_session_path: None,
                run_started_at: None,
            },
            &roots,
        );
        match result {
            TranscriptFindResult::Found(hit) => {
                assert_eq!(
                    hit.path.canonicalize().unwrap(),
                    path.canonicalize().unwrap()
                );
                assert_eq!(hit.confidence, TranscriptConfidence::High);
            }
            other => panic!("expected Found, got {other:?}"),
        }
    }

    #[test]
    fn codex_finds_rollout_by_thread_id() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = roots_under(tmp.path());
        let sid = "019f6c53-ceb6-7601-b678-e807fdd84042";
        let dir = roots.codex_home.join("sessions/2026/07/16");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("rollout-2026-07-16T22-07-39-{sid}.jsonl"));
        let mut f = fs::File::create(&path).unwrap();
        writeln!(
            f,
            "{}",
            json!({
                "timestamp": "2026-07-16T19:07:40.523Z",
                "type": "session_meta",
                "payload": {
                    "session_id": sid,
                    "cwd": "/tmp/proj",
                    "originator": "orgasmic"
                }
            })
        )
        .unwrap();

        let result = find_native_transcript(
            &TranscriptLookup {
                run_id: "run-codex".into(),
                harness: "codex".into(),
                cwd: Some(PathBuf::from("/tmp/proj")),
                session_id: Some(sid.into()),
                recorded_session_path: None,
                run_started_at: None,
            },
            &roots,
        );
        match result {
            TranscriptFindResult::Found(hit) => {
                assert_eq!(hit.path, path);
                assert_eq!(hit.confidence, TranscriptConfidence::High);
                assert_eq!(hit.correlation, "codex_thread_id_rollout_filename");
            }
            other => panic!("expected Found, got {other:?}"),
        }
    }

    #[test]
    fn codex_cwd_unique_is_medium() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = roots_under(tmp.path());
        let cwd = "/tmp/unique-codex-cwd";
        let dir = roots.codex_home.join("sessions/2026/07/16");
        fs::create_dir_all(&dir).unwrap();
        let sid = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let path = dir.join(format!("rollout-2026-07-16T10-00-00-{sid}.jsonl"));
        let started = DateTime::parse_from_rfc3339("2026-07-16T10:00:05Z")
            .unwrap()
            .with_timezone(&Utc);
        let mut f = fs::File::create(&path).unwrap();
        writeln!(
            f,
            "{}",
            json!({
                "timestamp": "2026-07-16T10:00:06Z",
                "type": "session_meta",
                "payload": {
                    "session_id": sid,
                    "cwd": cwd,
                    "originator": "orgasmic",
                    "timestamp": "2026-07-16T10:00:06Z"
                }
            })
        )
        .unwrap();

        let result = find_native_transcript(
            &TranscriptLookup {
                run_id: "run-codex".into(),
                harness: "codex".into(),
                cwd: Some(PathBuf::from(cwd)),
                session_id: None,
                recorded_session_path: None,
                run_started_at: Some(started),
            },
            &roots,
        );
        match result {
            TranscriptFindResult::Found(hit) => {
                assert_eq!(hit.path, path);
                assert_eq!(hit.confidence, TranscriptConfidence::Medium);
            }
            other => panic!("expected Found, got {other:?}"),
        }
    }

    #[test]
    fn codex_earlier_run_does_not_take_later_rollout() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = roots_under(tmp.path());
        let cwd = "/tmp/repeated-codex-cwd";
        let dir = roots.codex_home.join("sessions/2026/07/09");
        fs::create_dir_all(&dir).unwrap();
        let sid = "bbbbbbbb-cccc-dddd-eeee-ffffffffffff";
        let path = dir.join(format!("rollout-2026-07-09T13-04-24-{sid}.jsonl"));
        let mut f = fs::File::create(&path).unwrap();
        writeln!(
            f,
            "{}",
            json!({
                "timestamp": "2026-07-09T13:04:24Z",
                "type": "session_meta",
                "payload": {
                    "session_id": sid,
                    "cwd": cwd,
                    "originator": "orgasmic",
                    "timestamp": "2026-07-09T13:04:24Z"
                }
            })
        )
        .unwrap();

        let earlier_start = DateTime::parse_from_rfc3339("2026-07-09T12:11:50Z")
            .unwrap()
            .with_timezone(&Utc);
        let earlier = find_native_transcript(
            &TranscriptLookup {
                run_id: "run-earlier".into(),
                harness: "codex".into(),
                cwd: Some(PathBuf::from(cwd)),
                session_id: None,
                recorded_session_path: None,
                run_started_at: Some(earlier_start),
            },
            &roots,
        );
        assert!(matches!(earlier, TranscriptFindResult::NotFound { .. }));

        let later_start = DateTime::parse_from_rfc3339("2026-07-09T13:04:23Z")
            .unwrap()
            .with_timezone(&Utc);
        let later = find_native_transcript(
            &TranscriptLookup {
                run_id: "run-later".into(),
                harness: "codex".into(),
                cwd: Some(PathBuf::from(cwd)),
                session_id: None,
                recorded_session_path: None,
                run_started_at: Some(later_start),
            },
            &roots,
        );
        match later {
            TranscriptFindResult::Found(hit) => {
                assert_eq!(hit.path, path);
                assert_eq!(hit.confidence, TranscriptConfidence::Medium);
                assert_eq!(
                    hit.correlation,
                    "codex_session_meta_cwd_originator_launch_time"
                );
            }
            other => panic!("expected Found for later run, got {other:?}"),
        }
    }

    #[test]
    fn hermes_prefers_jsonl_over_paired_metadata_json() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = roots_under(tmp.path());
        let sid = "20260328_115902_00cfe3cd";
        let jsonl = roots.hermes_sessions.join(format!("{sid}.jsonl"));
        let json = roots.hermes_sessions.join(format!("session_{sid}.json"));
        fs::write(&jsonl, "{\"role\":\"user\"}\n").unwrap();
        fs::write(&json, r#"{"session_id":"20260328_115902_00cfe3cd"}"#).unwrap();

        let result = find_native_transcript(
            &TranscriptLookup {
                run_id: "run-hermes".into(),
                harness: "hermes".into(),
                cwd: None,
                session_id: Some(sid.into()),
                recorded_session_path: None,
                run_started_at: None,
            },
            &roots,
        );
        match result {
            TranscriptFindResult::Found(hit) => {
                assert_eq!(
                    hit.path.canonicalize().unwrap(),
                    jsonl.canonicalize().unwrap()
                );
                assert_eq!(hit.correlation, "hermes_session_id_jsonl");
            }
            other => panic!("expected Found jsonl, got {other:?}"),
        }
    }

    #[test]
    fn hermes_finds_exact_session_file() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = roots_under(tmp.path());
        let sid = "api-7c08bf6d1650aa08";
        let path = roots.hermes_sessions.join(format!("session_{sid}.json"));
        fs::write(&path, r#"{"session_id":"api-7c08bf6d1650aa08"}"#).unwrap();

        let result = find_native_transcript(
            &TranscriptLookup {
                run_id: "run-hermes".into(),
                harness: "hermes".into(),
                cwd: None,
                session_id: Some(sid.into()),
                recorded_session_path: None,
                run_started_at: None,
            },
            &roots,
        );
        match result {
            TranscriptFindResult::Found(hit) => {
                assert_eq!(
                    hit.path.canonicalize().unwrap(),
                    path.canonicalize().unwrap()
                );
                assert_eq!(hit.confidence, TranscriptConfidence::High);
                assert_eq!(hit.correlation, "hermes_session_id_json_metadata");
            }
            other => panic!("expected Found, got {other:?}"),
        }
    }

    #[test]
    fn cursor_tui_resolves_unique_cwd_launch_time() {
        use filetime::{set_file_mtime, FileTime};

        let tmp = tempfile::tempdir().unwrap();
        let roots = roots_under(tmp.path());
        let cwd = PathBuf::from("/tmp/tui-cursor-wt");
        let sid = "22222222-3333-4444-5555-666666666666";
        let path = cursor_transcript_path(&roots, &cwd, sid).unwrap();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "{\"role\":\"user\"}\n").unwrap();
        let started = DateTime::parse_from_rfc3339("2026-07-18T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let mtime = started + chrono::Duration::seconds(30);
        set_file_mtime(
            &path,
            FileTime::from_system_time(std::time::SystemTime::from(mtime)),
        )
        .unwrap();

        let result = find_native_transcript(
            &TranscriptLookup {
                run_id: "run-cursor-tui".into(),
                harness: "cursor-agent".into(),
                cwd: Some(cwd),
                session_id: None,
                recorded_session_path: None,
                run_started_at: Some(started),
            },
            &roots,
        );
        match result {
            TranscriptFindResult::Found(hit) => {
                assert_eq!(
                    hit.path.canonicalize().unwrap(),
                    path.canonicalize().unwrap()
                );
                assert_eq!(hit.confidence, TranscriptConfidence::Medium);
                assert_eq!(hit.correlation, "cursor_tui_cwd_launch_time_unique");
            }
            other => panic!("expected Found, got {other:?}"),
        }
    }

    #[test]
    fn hermes_tui_resolves_unique_launch_time() {
        use filetime::{set_file_mtime, FileTime};

        let tmp = tempfile::tempdir().unwrap();
        let roots = roots_under(tmp.path());
        let sid = "20260718_100030_abc12345";
        let path = roots.hermes_sessions.join(format!("{sid}.jsonl"));
        fs::write(&path, "{\"role\":\"user\"}\n").unwrap();
        let started = DateTime::parse_from_rfc3339("2026-07-18T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let mtime = started + chrono::Duration::seconds(30);
        set_file_mtime(
            &path,
            FileTime::from_system_time(std::time::SystemTime::from(mtime)),
        )
        .unwrap();

        let result = find_native_transcript(
            &TranscriptLookup {
                run_id: "run-hermes-tui".into(),
                harness: "hermes".into(),
                cwd: Some(PathBuf::from("/tmp/wt")),
                session_id: None,
                recorded_session_path: None,
                run_started_at: Some(started),
            },
            &roots,
        );
        match result {
            TranscriptFindResult::Found(hit) => {
                assert_eq!(
                    hit.path.canonicalize().unwrap(),
                    path.canonicalize().unwrap()
                );
                assert_eq!(hit.confidence, TranscriptConfidence::Medium);
                assert_eq!(hit.correlation, "hermes_tui_launch_time_unique");
            }
            other => panic!("expected Found, got {other:?}"),
        }
    }

    #[test]
    fn session_id_path_traversal_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = roots_under(tmp.path());
        let outside = tmp.path().join("outside.jsonl");
        fs::write(&outside, "{}\n").unwrap();

        let result = find_native_transcript(
            &TranscriptLookup {
                run_id: "run-claude".into(),
                harness: "claude".into(),
                cwd: Some(PathBuf::from("/tmp/proj")),
                session_id: Some("../outside.jsonl".into()),
                recorded_session_path: None,
                run_started_at: None,
            },
            &roots,
        );
        assert!(matches!(result, TranscriptFindResult::NotFound { .. }));
    }

    #[cfg(unix)]
    #[test]
    fn recorded_path_symlink_escape_is_rejected() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let roots = roots_under(tmp.path());
        let outside = tmp.path().join("outside.jsonl");
        fs::write(&outside, "{}\n").unwrap();
        let link = roots.claude_projects.join("escape.jsonl");
        fs::create_dir_all(&roots.claude_projects).unwrap();
        symlink(&outside, &link).unwrap();

        let result = find_native_transcript(
            &TranscriptLookup {
                run_id: "run-claude".into(),
                harness: "claude".into(),
                cwd: None,
                session_id: None,
                recorded_session_path: Some(link),
                run_started_at: None,
            },
            &roots,
        );
        assert!(matches!(result, TranscriptFindResult::NotFound { .. }));
    }

    #[test]
    fn hermes_without_session_id_does_not_guess() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = roots_under(tmp.path());
        fs::write(roots.hermes_sessions.join("session_noise.json"), "{}").unwrap();
        let result = find_native_transcript(
            &TranscriptLookup {
                run_id: "run-hermes".into(),
                harness: "hermes".into(),
                cwd: Some(PathBuf::from("/tmp")),
                session_id: None,
                recorded_session_path: None,
                run_started_at: Some(Utc::now()),
            },
            &roots,
        );
        assert!(matches!(result, TranscriptFindResult::NotFound { .. }));
    }

    /// Optional machine-local probe. Deterministic unit tests are the CI gate;
    /// set `ORGASMIC_PROBE_TRANSCRIPTS=1` to exercise real homes + session JSONL.
    #[test]
    fn production_path_probe_when_local_homes_present() {
        if std::env::var("ORGASMIC_PROBE_TRANSCRIPTS").as_deref() != Ok("1") {
            eprintln!(
                "SKIP production_path_probe_when_local_homes_present: set ORGASMIC_PROBE_TRANSCRIPTS=1 to enable"
            );
            return;
        }
        let Some(roots) = TranscriptRoots::from_env_home() else {
            panic!("ORGASMIC_PROBE_TRANSCRIPTS=1 but HOME is unset");
        };
        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let candidates = [
            std::env::var_os("ORGASMIC_PROBE_SESSIONS").map(PathBuf::from),
            Some(manifest.join("../../.orgasmic/tmp/sessions")),
            Some(manifest.join("../../../../../tmp/sessions")),
        ];
        let Some(orgasmic_sessions) = candidates.into_iter().flatten().find(|p| p.is_dir()) else {
            panic!(
                "ORGASMIC_PROBE_TRANSCRIPTS=1 but no orgasmic sessions dir found (set ORGASMIC_PROBE_SESSIONS?)"
            );
        };
        let mut probed = 0usize;
        let claude_session =
            orgasmic_sessions.join("dispatch-TASK-TJF7G-implementer-20260713T135602.jsonl");
        if claude_session.is_file() {
            probed += 1;
            let envelopes = orgasmic_core::read_session_file(&claude_session).unwrap();
            let lookup = lookup_from_envelopes(&envelopes).expect("claude lookup");
            let result = find_native_transcript(&lookup, &roots);
            assert!(
                matches!(result, TranscriptFindResult::Found(_)),
                "claude TJF7G expected Found, got {result:?}"
            );
        }
        let codex_session =
            orgasmic_sessions.join("dispatch-TASK-P0FAQ-implementer-20260716T190738.jsonl");
        if codex_session.is_file() {
            probed += 1;
            let envelopes = orgasmic_core::read_session_file(&codex_session).unwrap();
            let lookup = lookup_from_envelopes(&envelopes).expect("codex lookup");
            assert_eq!(
                lookup.session_id.as_deref(),
                Some("019f6c53-ceb6-7601-b678-e807fdd84042")
            );
            let result = find_native_transcript(&lookup, &roots);
            assert!(
                matches!(result, TranscriptFindResult::Found(_)),
                "codex P0FAQ expected Found, got {result:?}"
            );
        }
        let cursor_session =
            orgasmic_sessions.join("dispatch-TASK-RCWFF-implementer-20260704T105943.jsonl");
        if cursor_session.is_file() {
            probed += 1;
            let envelopes = orgasmic_core::read_session_file(&cursor_session).unwrap();
            let lookup = lookup_from_envelopes(&envelopes).expect("cursor lookup");
            let result = find_native_transcript(&lookup, &roots);
            assert!(
                matches!(result, TranscriptFindResult::Found(_)),
                "cursor RCWFF expected Found, got {result:?}"
            );
        }
        let hermes_session =
            orgasmic_sessions.join("dispatch-TASK-HERMES-probe-implementer-20260701T120000.jsonl");
        if hermes_session.is_file() {
            probed += 1;
            let envelopes = orgasmic_core::read_session_file(&hermes_session).unwrap();
            let lookup = lookup_from_envelopes(&envelopes).expect("hermes lookup");
            let result = find_native_transcript(&lookup, &roots);
            assert!(
                matches!(result, TranscriptFindResult::Found(_)),
                "hermes probe expected Found, got {result:?}"
            );
        }
        if probed == 0 {
            panic!(
                "ORGASMIC_PROBE_TRANSCRIPTS=1 but none of the hard-coded probe sessions exist under {}",
                orgasmic_sessions.display()
            );
        }
    }

    #[test]
    fn lookup_from_envelopes_reads_run_meta_and_ready() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("run.jsonl");
        let identity = RuntimeIdentity {
            run_id: "run-test".into(),
            runtime_id: "rt".into(),
            boot_id: "boot".into(),
        };
        let mut writer = SessionWriter::open(&path, identity).unwrap();
        writer
            .append(
                SessionEventKind::Lifecycle,
                json!({
                    "phase": "acquire",
                    "task_id": "TASK-1",
                    "kind": "worker",
                    "worker_id": "implementer-cursor"
                }),
            )
            .unwrap();
        writer
            .append(
                SessionEventKind::Lifecycle,
                json!({
                    "phase": "run_meta",
                    "transport": "subprocess-stream-json",
                    "harness": "cursor-agent",
                    "worktree": "/tmp/wt",
                    "driver_config": {}
                }),
            )
            .unwrap();
        writer
            .append(
                SessionEventKind::DriverEvent,
                json!({
                    "type": "ready",
                    "protocol_version": "cursor-agent-stream-json/1",
                    "capabilities": { "session_id": "sess-xyz" }
                }),
            )
            .unwrap();
        drop(writer);

        let envelopes = orgasmic_core::read_session_file(&path).unwrap();
        let lookup = lookup_from_envelopes(&envelopes).expect("lookup");
        assert_eq!(lookup.run_id, "run-test");
        assert_eq!(lookup.harness, "cursor-agent");
        assert_eq!(lookup.cwd.as_deref(), Some(Path::new("/tmp/wt")));
        assert_eq!(lookup.session_id.as_deref(), Some("sess-xyz"));
    }
}

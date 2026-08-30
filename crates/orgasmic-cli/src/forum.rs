use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::{ArgAction, Args, Subcommand};
use orgasmic_core::{
    is_valid_greenfield_artifact_id, is_valid_task_path_id, mint_node_id_for_class,
    project_tmp_dir, NodeIdClass,
};
use orgasmic_drivers::catalog::transport_profiles;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::daemon_client::DaemonClient;
use crate::home::Home;
use crate::manager::{
    self, DispatchArgs, DispatchCloseArgs, DispatchCloseStatus, DispatchKind, DispatchWaitArgs,
    DispatchWaitOutcome,
};

const QUESTION_PLACEHOLDER: &str = "__ORGASMIC_QUESTION_SECTION__";
const TARGET_PLACEHOLDER: &str = "__ORGASMIC_TARGET_SECTION__";
const DIAGRAM_PLACEHOLDER: &str = "__ORGASMIC_PIPELINE_DIAGRAM__";
const RUN_STATS_PLACEHOLDER: &str = "__ORGASMIC_RUN_STATS__";
const MAX_TARGET_BYTES: usize = 64 * 1024;

#[derive(Args, Debug, Clone)]
struct RunArgs {
    /// Participant as mode,harness,model,effort; repeat at least twice unless --fast.
    #[arg(long, action = ArgAction::Append, required = true)]
    participant: Vec<String>,
    /// Run only the independent first stage; skip blind cross-review.
    #[arg(long)]
    fast: bool,
    /// Curator: a 1-based participant index, or a full mode,harness,model,effort
    /// spec to curate with a model outside the panel. Either way the curator runs
    /// as a fresh dispatch with no memory of stage 1.
    #[arg(long)]
    curator: Option<String>,
    /// Add this self-curated round to an existing open forum.
    #[arg(long)]
    forum: Option<String>,
    /// Git ref from which dispatched worktrees branch. Defaults to the invoking HEAD.
    #[arg(long = "from")]
    source_ref: Option<String>,
    /// Submit a new version of this existing artifact instead of minting an id.
    #[arg(long = "artifact-id")]
    artifact_id: Option<String>,
    /// Project id; when supplied it must match the project resolved from cwd.
    #[arg(long)]
    project: Option<String>,
    /// Maximum wait per stage (for example 30s, 5m, 1h).
    #[arg(long, default_value = "45m", value_parser = parse_duration)]
    timeout: Duration,
}

#[derive(Args, Debug, Clone)]
#[command(after_help = "\
Examples:
  orgasmic forum ask --file /tmp/question.txt \\
    --participant 'stdio,hermes,openai/gpt-5.6-luna,low' \\
    --participant 'stdio,hermes,google/gemini-3.7-flash,low' \\
    --curator 'stdio,claude,claude-fable-5,low'

Participants are mode,harness,model,effort. Repeat --participant at least twice,
or use --fast for a stage-1-only round with one or more participants.
Omit --curator for in-session curation and use --forum to add later rounds.
Pass --curator as a 1-based participant index or its own
mode,harness,model,effort spec for the single-round dispatched-curator path.")]
pub struct AskArgs {
    /// Question text. Mutually exclusive with --file.
    #[arg(long, conflicts_with = "file", allow_hyphen_values = true)]
    question: Option<String>,
    /// Read the question from this UTF-8 file.
    #[arg(long, conflicts_with = "question")]
    file: Option<PathBuf>,
    #[command(flatten)]
    run: RunArgs,
}

#[derive(Args, Debug, Clone)]
#[command(after_help = "\
Example:
  orgasmic forum critique --file /tmp/design.md --focus 'security posture' \\
    --participant 'stdio,hermes,openai/gpt-5.6-luna,low' \\
    --participant 'stdio,hermes,google/gemini-3.7-flash,low'

Participants are mode,harness,model,effort. Repeat --participant at least twice,
or use --fast for a stage-1-only round with one or more participants.
Omit --curator for in-session curation and use --forum to add later rounds.
Pass --curator as a 1-based participant index or its own
mode,harness,model,effort spec for the single-round dispatched-curator path.")]
pub struct CritiqueArgs {
    /// UTF-8 document to critique (non-empty, at most 64 KiB).
    #[arg(long)]
    file: PathBuf,
    /// Optional one-line steer for the critique.
    #[arg(long, allow_hyphen_values = true)]
    focus: Option<String>,
    #[command(flatten)]
    run: RunArgs,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct Participant {
    mode: String,
    harness: String,
    dispatch_model: String,
    effort: String,
    vendor: String,
    model: String,
}

impl Participant {
    fn identity(&self) -> String {
        format!(
            "{} · {} · {} · effort {}",
            self.harness, self.vendor, self.model, self.effort
        )
    }
}

#[derive(Clone, Debug)]
struct Dispatch {
    task: String,
    started_tx: String,
    participant: Participant,
    closed: bool,
}

#[derive(Clone, Debug)]
struct RunReport {
    participant: Participant,
    dispatch: Dispatch,
    path: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct DeltaBullet {
    tag: String,
    text: String,
}

#[derive(Clone, Debug)]
struct DiagramFields {
    extracts: BTreeMap<String, Vec<String>>,
    reviews: BTreeMap<String, Vec<DeltaBullet>>,
    curator_summary: String,
    headline: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum ForumKind {
    Ask,
    Critique,
}

impl ForumKind {
    fn slug(self) -> &'static str {
        match self {
            Self::Ask => "ask",
            Self::Critique => "critique",
        }
    }

    fn first_stage_spec(self) -> &'static str {
        match self {
            Self::Ask => "extractor",
            Self::Critique => "critic",
        }
    }

    fn cross_review_spec(self) -> &'static str {
        match self {
            Self::Ask => "cross-reviewer",
            Self::Critique => "critique-cross-reviewer",
        }
    }

    fn curator_spec(self) -> &'static str {
        match self {
            Self::Ask => "curator",
            Self::Critique => "critique-curator",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ForumInput {
    Ask {
        question: String,
    },
    Critique {
        target: String,
        focus: Option<String>,
        basename: String,
    },
}

impl ForumInput {
    fn kind(&self) -> ForumKind {
        match self {
            Self::Ask { .. } => ForumKind::Ask,
            Self::Critique { .. } => ForumKind::Critique,
        }
    }

    fn content(&self) -> &str {
        match self {
            Self::Ask { question } => question,
            Self::Critique { target, .. } => target,
        }
    }

    fn focus_value(&self) -> String {
        match self {
            Self::Ask { .. } => String::new(),
            Self::Critique { focus, .. } => focus.clone().unwrap_or_else(|| "(none)".to_string()),
        }
    }

    fn prompt_values(&self) -> BTreeMap<String, String> {
        let mut values = BTreeMap::from([(
            "artifact.user_prompt".to_string(),
            self.content().to_string(),
        )]);
        if self.kind() == ForumKind::Critique {
            values.insert("node.extra_prompt".to_string(), self.focus_value());
        }
        values
    }

    fn diagram_prompt(&self) -> String {
        match self {
            Self::Ask { question } => question.clone(),
            Self::Critique {
                focus: Some(focus), ..
            } => focus.clone(),
            Self::Critique {
                target, basename, ..
            } => format!("critique of {basename}, {} bytes", target.len()),
        }
    }

    fn short_label(&self) -> String {
        match self {
            Self::Ask { question } => question
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .chars()
                .take(100)
                .collect(),
            Self::Critique {
                focus: Some(focus), ..
            } => clipped(focus, 100),
            Self::Critique { basename, .. } => clipped(basename, 100),
        }
    }

    fn fallback_title(&self) -> String {
        match self {
            Self::Ask { .. } => format!("Multi-model extraction: {}", self.short_label()),
            Self::Critique { .. } => format!("Multi-model critique: {}", self.short_label()),
        }
    }

    fn artifact_title(&self, fields: &DiagramFields) -> String {
        fields
            .headline
            .clone()
            .unwrap_or_else(|| self.fallback_title())
    }
}

#[derive(Debug)]
struct WaitUnknown(String);

impl std::fmt::Display for WaitUnknown {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for WaitUnknown {}

#[derive(Serialize)]
struct DispatchedAskResult {
    parent_task: String,
    extraction_tasks: Vec<String>,
    cross_review_tasks: Vec<String>,
    curation_task: String,
    artifact_id: String,
}

#[derive(Serialize)]
struct DispatchedCritiqueResult {
    parent_task: String,
    critique_tasks: Vec<String>,
    cross_review_tasks: Vec<String>,
    curation_task: String,
    artifact_id: String,
}

struct DispatchedRunResult {
    parent_task: String,
    first_stage_tasks: Vec<String>,
    cross_review_tasks: Vec<String>,
    curation_task: String,
    artifact_id: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct ManifestParticipant {
    mode: String,
    harness: String,
    model: String,
    effort: String,
}

impl From<&Participant> for ManifestParticipant {
    fn from(value: &Participant) -> Self {
        Self {
            mode: value.mode.clone(),
            harness: value.harness.clone(),
            model: value.dispatch_model.clone(),
            effort: value.effort.clone(),
        }
    }
}

impl ManifestParticipant {
    fn participant(&self) -> Result<Participant> {
        parse_participant(&format!(
            "{},{},{},{}",
            self.mode, self.harness, self.model, self.effort
        ))
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum CurationMode {
    SelfCurated,
    Dispatched,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum ForumState {
    Open,
    Curated,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct ForumRound {
    round: usize,
    kind: ForumKind,
    #[serde(default)]
    fast: bool,
    input: ForumInput,
    panel: Vec<ManifestParticipant>,
    first_stage_tasks: Vec<String>,
    cross_review_tasks: Vec<String>,
    promoted_report_paths: Vec<PathBuf>,
    started_at: String,
    completed_at: String,
    contract_path: Option<PathBuf>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct ForumManifest {
    version: u32,
    forum: String,
    project: String,
    source_ref: String,
    started_at: String,
    artifact_id: Option<String>,
    curation_mode: CurationMode,
    state: ForumState,
    rounds: Vec<ForumRound>,
    curation_task: Option<String>,
    submitted_artifact: Option<String>,
}

#[derive(Serialize)]
struct SelfCuratedRoundResult {
    forum: String,
    parent_task: String,
    first_stage_tasks: Vec<String>,
    cross_review_tasks: Vec<String>,
    manifest_path: PathBuf,
    promoted_report_paths: Vec<PathBuf>,
    contract_path: PathBuf,
}

enum RunResult {
    Dispatched(DispatchedRunResult),
    SelfCurated(SelfCuratedRoundResult),
}

#[derive(Serialize)]
#[serde(untagged)]
enum AskResult {
    Dispatched(DispatchedAskResult),
    SelfCurated(SelfCuratedAskResult),
}

#[derive(Serialize)]
struct SelfCuratedAskResult {
    forum: String,
    parent_task: String,
    extraction_tasks: Vec<String>,
    cross_review_tasks: Vec<String>,
    manifest_path: PathBuf,
    promoted_report_paths: Vec<PathBuf>,
    contract_path: PathBuf,
}

#[derive(Serialize)]
#[serde(untagged)]
enum CritiqueResult {
    Dispatched(DispatchedCritiqueResult),
    SelfCurated(SelfCuratedCritiqueResult),
}

#[derive(Serialize)]
struct SelfCuratedCritiqueResult {
    forum: String,
    parent_task: String,
    critique_tasks: Vec<String>,
    cross_review_tasks: Vec<String>,
    manifest_path: PathBuf,
    promoted_report_paths: Vec<PathBuf>,
    contract_path: PathBuf,
}

#[derive(Args, Debug)]
pub struct CurateArgs {
    /// Open self-curated forum to submit.
    #[arg(long)]
    forum: String,
    /// Session-authored MDX draft.
    #[arg(long)]
    draft: PathBuf,
    /// Session-authored diagram JSON.
    #[arg(long)]
    diagram: PathBuf,
    /// Invoking session as mode,harness,model,effort.
    #[arg(long)]
    identity: String,
    /// Project id; when supplied it must match the project resolved from cwd.
    #[arg(long)]
    project: Option<String>,
}

#[derive(Serialize)]
struct CurateResult {
    forum: String,
    parent_task: String,
    curation_task: String,
    artifact_id: String,
    manifest_path: PathBuf,
}

fn parse_duration(raw: &str) -> std::result::Result<Duration, String> {
    let raw = raw.trim();
    let (number, unit) = raw
        .char_indices()
        .last()
        .filter(|(_, ch)| matches!(ch, 's' | 'm' | 'h'))
        .map(|(index, unit)| (&raw[..index], unit))
        .ok_or_else(|| "timeout must end in s, m, or h".to_string())?;
    let number = number
        .parse::<u64>()
        .map_err(|_| "timeout must be a positive integer".to_string())?;
    if number == 0 {
        return Err("timeout must be greater than zero".to_string());
    }
    Ok(Duration::from_secs(match unit {
        's' => number,
        'm' => number.saturating_mul(60),
        'h' => number.saturating_mul(3600),
        _ => unreachable!(),
    }))
}

fn clipped(value: &str, limit: usize) -> String {
    let normalized = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.chars().count() <= limit {
        return normalized;
    }
    let mut out = normalized.chars().take(limit - 1).collect::<String>();
    while out.ends_with(char::is_whitespace) {
        out.pop();
    }
    out.push('…');
    out
}

fn html_escape(value: &str, quotes: bool) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' if quotes => out.push_str("&quot;"),
            '\'' if quotes => out.push_str("&#x27;"),
            _ => out.push(ch),
        }
    }
    out
}

fn escape_rich_text(value: &str) -> String {
    html_escape(value, false)
        .replace('{', "&#123;")
        .replace('}', "&#125;")
}

fn svg_text(x: i32, y: i32, value: &str, style: &str, attrs: &[(&str, &str)]) -> String {
    let mut extra = String::new();
    for (name, raw) in attrs {
        write!(extra, " {name}=\"{}\"", html_escape(raw, true)).unwrap();
    }
    format!(
        "<text x=\"{x}\" y=\"{y}\" style=\"{}\"{extra}>{}</text>",
        html_escape(style, true),
        html_escape(value, false)
    )
}

fn contains_model_svg(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    if lower.contains("data:image/svg+xml") {
        return true;
    }
    let bytes = lower.as_bytes();
    let mut offset = 0;
    while let Some(found) = lower[offset..].find('<') {
        let mut index = offset + found + 1;
        while bytes.get(index).is_some_and(u8::is_ascii_whitespace) {
            index += 1;
        }
        if bytes.get(index..index + 3) == Some(b"svg")
            && bytes
                .get(index + 3)
                .is_none_or(|next| !next.is_ascii_alphanumeric() && *next != b'_' && *next != b'-')
        {
            return true;
        }
        offset = index.min(lower.len());
        if offset == lower.len() {
            break;
        }
    }
    false
}

fn value_string<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

fn load_diagram_fields(
    path: &Path,
    extraction_tasks: &[String],
    review_tasks: &[String],
) -> Result<DiagramFields> {
    let raw = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    if contains_model_svg(&raw) {
        bail!("curator diagram fields contained model-authored SVG");
    }
    let data: Value = serde_json::from_str(&raw).context("parse curator diagram fields")?;
    parse_diagram_fields(&data, extraction_tasks, review_tasks)
}

fn parse_diagram_fields(
    data: &Value,
    extraction_tasks: &[String],
    review_tasks: &[String],
) -> Result<DiagramFields> {
    let object = data
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("curator diagram fields must be a JSON object"))?;

    let extraction_set = extraction_tasks
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let mut extracts = BTreeMap::new();
    for item in object
        .get("extracts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let task = value_string(item, "task").unwrap_or("");
        let lines = item.get("excerpt_lines").and_then(Value::as_array);
        let valid = !extracts.contains_key(task)
            && extraction_set.contains(task)
            && lines.is_some_and(|lines| {
                (1..=4).contains(&lines.len())
                    && lines
                        .iter()
                        .all(|line| line.as_str().is_some_and(|line| !line.trim().is_empty()))
            });
        if !valid {
            bail!("invalid extract diagram entry for {task:?}");
        }
        extracts.insert(
            task.to_string(),
            lines
                .unwrap()
                .iter()
                .map(|line| clipped(line.as_str().unwrap(), 43))
                .collect(),
        );
    }
    if extracts.keys().map(String::as_str).collect::<BTreeSet<_>>() != extraction_set {
        bail!("curator diagram fields must cover every extraction task once");
    }

    let review_set = review_tasks
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let mut reviews = BTreeMap::new();
    for item in object
        .get("reviews")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let task = value_string(item, "task").unwrap_or("");
        let bullets = item.get("delta_bullets").and_then(Value::as_array);
        let tags = bullets
            .into_iter()
            .flatten()
            .filter_map(|bullet| value_string(bullet, "tag"))
            .collect::<BTreeSet<_>>();
        let valid = !reviews.contains_key(task)
            && review_set.contains(task)
            && bullets.is_some_and(|bullets| {
                bullets.len() == 3
                    && bullets.iter().all(|bullet| {
                        matches!(
                            value_string(bullet, "tag"),
                            Some("?") | Some("+") | Some("=")
                        ) && value_string(bullet, "text")
                            .is_some_and(|text| !text.trim().is_empty())
                    })
            })
            && tags == BTreeSet::from(["?", "+", "="]);
        if !valid {
            bail!("invalid review diagram entry for {task:?}");
        }
        reviews.insert(
            task.to_string(),
            bullets
                .unwrap()
                .iter()
                .map(|bullet| DeltaBullet {
                    tag: value_string(bullet, "tag").unwrap().to_string(),
                    text: clipped(value_string(bullet, "text").unwrap(), 43),
                })
                .collect(),
        );
    }
    if reviews.keys().map(String::as_str).collect::<BTreeSet<_>>() != review_set {
        bail!("curator diagram fields must cover every review task once");
    }

    let summary = object
        .get("curator_summary")
        .and_then(Value::as_str)
        .filter(|summary| !summary.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("curator_summary must be a non-empty string"))?;
    // Optional short artifact title; a bad value falls back to the question-derived
    // title rather than failing a finished run.
    let headline = object
        .get("headline")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|headline| !headline.is_empty() && !headline.contains(['\n', '\r']))
        .map(|headline| clipped(headline, 80));
    Ok(DiagramFields {
        extracts,
        reviews,
        curator_summary: clipped(summary, 72),
        headline,
    })
}

#[derive(Clone, Debug)]
struct RoundDiagramFields {
    round: usize,
    kind: ForumKind,
    fields: DiagramFields,
}

#[derive(Debug)]
struct MultiRoundDiagramFields {
    rounds: Vec<RoundDiagramFields>,
    curator_summary: String,
    headline: Option<String>,
}

fn load_multi_round_diagram_fields(
    path: &Path,
    rounds: &[ForumRound],
) -> Result<MultiRoundDiagramFields> {
    let raw = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    if contains_model_svg(&raw) {
        bail!("curator diagram fields contained model-authored SVG");
    }
    let data: Value = serde_json::from_str(&raw).context("parse curator diagram fields")?;
    let object = data
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("curator diagram fields must be a JSON object"))?;

    if let Some(entries) = object.get("rounds").and_then(Value::as_array) {
        if entries.len() != rounds.len() {
            bail!("diagram rounds must cover every forum round once");
        }
        let mut seen = BTreeSet::new();
        let mut parsed = Vec::with_capacity(entries.len());
        for entry in entries {
            let round_number = entry
                .get("round")
                .and_then(Value::as_u64)
                .and_then(|round| usize::try_from(round).ok())
                .unwrap_or(0);
            if round_number == 0 || !seen.insert(round_number) {
                bail!("diagram rounds must use unique positive round numbers");
            }
            let expected = rounds
                .get(round_number - 1)
                .filter(|round| round.round == round_number)
                .ok_or_else(|| anyhow::anyhow!("unknown diagram round {round_number}"))?;
            if value_string(entry, "kind") != Some(expected.kind.slug()) {
                bail!("diagram round {round_number} kind does not match the manifest");
            }
            let mut entry_with_summary = entry.clone();
            entry_with_summary.as_object_mut().unwrap().insert(
                "curator_summary".to_string(),
                Value::String("round".to_string()),
            );
            let fields = parse_diagram_fields(
                &entry_with_summary,
                &expected.first_stage_tasks,
                &expected.cross_review_tasks,
            )?;
            parsed.push(RoundDiagramFields {
                round: round_number,
                kind: expected.kind,
                fields,
            });
        }
        parsed.sort_by_key(|round| round.round);
        if parsed
            .iter()
            .enumerate()
            .any(|(index, round)| round.round != index + 1)
        {
            bail!("diagram rounds must cover every forum round once");
        }
        let summary = object
            .get("curator_summary")
            .and_then(Value::as_str)
            .filter(|summary| !summary.trim().is_empty())
            .ok_or_else(|| anyhow::anyhow!("curator_summary must be a non-empty string"))?;
        let headline = object
            .get("headline")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|headline| !headline.is_empty() && !headline.contains(['\n', '\r']))
            .map(|headline| clipped(headline, 80));
        return Ok(MultiRoundDiagramFields {
            rounds: parsed,
            curator_summary: clipped(summary, 72),
            headline,
        });
    }

    if rounds.len() != 1 {
        bail!("multi-round forum diagrams must use the rounds array");
    }
    let fields = parse_diagram_fields(
        &data,
        &rounds[0].first_stage_tasks,
        &rounds[0].cross_review_tasks,
    )?;
    Ok(MultiRoundDiagramFields {
        curator_summary: fields.curator_summary.clone(),
        headline: fields.headline.clone(),
        rounds: vec![RoundDiagramFields {
            round: 1,
            kind: rounds[0].kind,
            fields,
        }],
    })
}

fn wrap_question(question: &str) -> Vec<String> {
    let normalized = question.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.is_empty() {
        return vec![String::new(), String::new()];
    }
    let mut lines = Vec::new();
    let mut current = String::new();
    for mut word in normalized.split(' ') {
        while word.chars().count() > 68 {
            let room = if current.is_empty() {
                68
            } else {
                68usize.saturating_sub(current.chars().count() + 1)
            };
            if room == 0 {
                lines.push(std::mem::take(&mut current));
                continue;
            }
            if !current.is_empty() {
                current.push(' ');
            }
            let head = word.chars().take(room).collect::<String>();
            current.push_str(&head);
            lines.push(std::mem::take(&mut current));
            let bytes = word
                .char_indices()
                .nth(room)
                .map(|(index, _)| index)
                .unwrap_or(word.len());
            word = &word[bytes..];
        }
        let needed = word.chars().count() + usize::from(!current.is_empty());
        if current.chars().count() + needed > 68 {
            lines.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(word);
    }
    if !current.is_empty() {
        lines.push(current);
    }
    if lines.len() > 2 {
        lines = vec![lines[0].clone(), clipped(&lines[1..].join(" "), 68)];
    }
    while lines.len() < 2 {
        lines.push(String::new());
    }
    lines
}

fn vendor_color(vendor: &str) -> &'static str {
    match vendor.to_ascii_lowercase().as_str() {
        "anthropic" => "#d97757",
        "openai" => "#10a37f",
        "google" => "#6f9df2",
        _ => "#b9a998",
    }
}

#[allow(clippy::too_many_arguments)]
fn render_pipeline_svg(
    question: &str,
    extraction: &[RunReport],
    reviews: &[RunReport],
    curator: &Participant,
    curator_task: &str,
    curator_path: &Path,
    fields: &DiagramFields,
    fast: bool,
) -> Result<String> {
    if fast {
        if extraction.is_empty() || !reviews.is_empty() || !fields.reviews.is_empty() {
            bail!("fast diagram requires extraction reports and no reviews");
        }
    } else if extraction.len() < 2 || extraction.len() != reviews.len() {
        bail!("diagram requires matching extraction and review rosters");
    }
    if !fast
        && extraction
            .iter()
            .map(|report| &report.participant)
            .ne(reviews.iter().map(|report| &report.participant))
    {
        bail!("diagram extraction and review roster order must match");
    }

    let count = extraction.len() as i32;
    let card_width = 252;
    let gap = 30;
    let margin = 32;
    let row_width = count * card_width + (count - 1) * gap;
    let width = (margin * 2 + row_width).max(if fast { 464 } else { 0 });
    let center = width / 2;
    let row_left = if fast { center - row_width / 2 } else { margin };
    let card_xs = (0..count)
        .map(|index| row_left + index * (card_width + gap))
        .collect::<Vec<_>>();
    let card_centers = card_xs
        .iter()
        .map(|x| x + card_width / 2)
        .collect::<Vec<_>>();
    let prompt_width = 480.min(width - 64);
    let prompt_x = center - prompt_width / 2;
    let curator_x = center - 200;

    let sans = "font-family:-apple-system,'SF Pro Text','Segoe UI',Helvetica,Arial,sans-serif";
    let mono = "font-family:ui-monospace,'SF Mono',Menlo,Consolas,monospace";
    let stage_style = format!(
        "{mono};font-size:8px;font-weight:500;fill:#8f7f70;text-anchor:middle;letter-spacing:0.14em"
    );
    let vendor_style = format!(
        "{mono};font-size:8.5px;font-weight:500;fill:#b9a998;text-anchor:start;letter-spacing:0.12em"
    );
    let model_style =
        format!("{sans};font-size:15px;font-weight:700;fill:#f0e6da;text-anchor:start");
    let role_style = format!("{mono};font-size:8px;font-weight:400;fill:#8f7f70;text-anchor:start");
    let body_style =
        format!("{sans};font-size:10.5px;font-weight:400;fill:#b9a998;text-anchor:start");
    let path_style = format!("{mono};font-size:8px;font-weight:400;fill:#8f7f70;text-anchor:start");
    let border = "rgba(240,230,218,0.13)";
    let question_lines = wrap_question(question);

    let mut out = String::new();
    write!(
        out,
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{width}\" height=\"1000\" viewBox=\"0 0 {width} 1000\" role=\"img\" aria-label=\"Forward pipeline from prompt to final answer\">"
    )?;
    write!(
        out,
        "<rect x=\"0.5\" y=\"0.5\" width=\"{}\" height=\"999\" rx=\"16\" fill=\"#241a15\" stroke=\"{border}\"/>",
        width - 1
    )?;
    out.push_str("<defs><marker id=\"ah\" markerWidth=\"7\" markerHeight=\"7\" refX=\"6\" refY=\"3.5\" orient=\"auto\"><path d=\"M0,0 L7,3.5 L0,7 z\" fill=\"#b9a998\"/></marker></defs>");
    out.push_str("<g data-card=\"prompt\">");
    write!(
        out,
        "<rect x=\"{prompt_x}\" y=\"36\" width=\"{prompt_width}\" height=\"92\" rx=\"10\" fill=\"#2c211b\" stroke=\"{border}\" stroke-width=\"1\"/>"
    )?;
    out.push_str(&svg_text(
        center,
        60,
        "PROMPT",
        &format!(
            "{mono};font-size:8px;font-weight:500;fill:#8f7f70;text-anchor:middle;letter-spacing:0.18em"
        ),
        &[],
    ));
    out.push_str(&svg_text(
        center,
        82,
        &question_lines[0],
        &format!("{sans};font-size:12px;font-weight:500;fill:#f0e6da;text-anchor:middle"),
        &[],
    ));
    out.push_str(&svg_text(
        center,
        100,
        &question_lines[1],
        &format!("{sans};font-size:12px;font-weight:500;fill:#f0e6da;text-anchor:middle"),
        &[],
    ));
    out.push_str("</g>");

    for destination in &card_centers {
        write!(
            out,
            "<path d=\"M{center},128 C{center},164 {destination},164 {destination},200\" fill=\"none\" stroke=\"#b9a998\" stroke-width=\"1.25\" opacity=\"0.55\" marker-end=\"url(#ah)\"/>"
        )?;
    }
    out.push_str("<g data-pill=\"extract\">");
    write!(
        out,
        "<rect x=\"{}\" y=\"153\" width=\"260\" height=\"22\" rx=\"11\" fill=\"#241a15\" stroke=\"{border}\"/>",
        center - 130
    )?;
    out.push_str(&svg_text(
        center,
        167,
        "1 · EXTRACT — PARALLEL · ISOLATED",
        &stage_style,
        &[],
    ));
    out.push_str("</g>");

    for (x, report) in card_xs.iter().zip(extraction) {
        let mut lines = fields.extracts[&report.dispatch.task]
            .iter()
            .take(4)
            .map(|line| clipped(line, 43))
            .collect::<Vec<_>>();
        lines.resize(4, String::new());
        let short_path = format!("{}/…/report.md", report.dispatch.task);
        write!(
            out,
            "<g data-card=\"extract\" data-task=\"{}\" data-record-path=\"{}\">",
            html_escape(&report.dispatch.task, true),
            html_escape(&report.path.display().to_string(), true)
        )?;
        write!(
            out,
            "<rect x=\"{x}\" y=\"200\" width=\"252\" height=\"224\" rx=\"10\" fill=\"#2c211b\" stroke=\"{border}\" stroke-width=\"1\"/>"
        )?;
        write!(
            out,
            "<circle cx=\"{}\" cy=\"226\" r=\"4\" fill=\"{}\"/>",
            x + 20,
            vendor_color(&report.participant.vendor)
        )?;
        out.push_str(&svg_text(
            x + 32,
            229,
            &report.participant.vendor.to_uppercase(),
            &vendor_style,
            &[],
        ));
        out.push_str(&svg_text(
            x + 18,
            256,
            &clipped(&report.participant.model, 32),
            &model_style,
            &[],
        ));
        out.push_str(&svg_text(
            x + 18,
            272,
            &clipped(
                &format!(
                    "{} · extract · effort {} · {}",
                    report.participant.harness, report.participant.effort, report.dispatch.task
                ),
                55,
            ),
            &role_style,
            &[],
        ));
        write!(
            out,
            "<line x1=\"{}\" y1=\"284\" x2=\"{}\" y2=\"284\" stroke=\"{border}\"/>",
            x + 18,
            x + 234
        )?;
        for (index, line) in lines.iter().enumerate() {
            out.push_str(&svg_text(
                x + 18,
                305 + index as i32 * 17,
                line,
                &body_style,
                &[],
            ));
        }
        out.push_str(&svg_text(x + 18, 406, &short_path, &path_style, &[]));
        out.push_str("</g>");
    }

    for (source_index, source) in card_centers.iter().enumerate() {
        for (target_index, destination) in card_centers.iter().enumerate() {
            if fast || source_index == target_index {
                continue;
            }
            write!(
                out,
                "<path d=\"M{source},424 C{source},464 {destination},464 {destination},504\" fill=\"none\" stroke=\"#b9a998\" stroke-width=\"1.25\" opacity=\"0.55\" marker-end=\"url(#ah)\"/>"
            )?;
        }
    }
    if !fast {
        out.push_str("<g data-pill=\"cross-review\">");
        write!(
            out,
            "<rect x=\"{}\" y=\"453\" width=\"280\" height=\"22\" rx=\"11\" fill=\"#241a15\" stroke=\"{border}\"/>",
            center - 140
        )?;
        out.push_str(&svg_text(
            center,
            467,
            "2 · CROSS-REVIEW — BLIND · NEVER SELF",
            &stage_style,
            &[],
        ));
        out.push_str("</g>");
    }

    for (index, (x, report)) in card_xs.iter().zip(reviews).enumerate() {
        let read_models = extraction
            .iter()
            .enumerate()
            .filter(|(offset, _)| *offset != index)
            .map(|(_, report)| report.participant.model.as_str())
            .collect::<Vec<_>>()
            .join(" + ");
        write!(
            out,
            "<g data-card=\"review\" data-task=\"{}\" data-record-path=\"{}\">",
            html_escape(&report.dispatch.task, true),
            html_escape(&report.path.display().to_string(), true)
        )?;
        write!(
            out,
            "<rect x=\"{x}\" y=\"504\" width=\"252\" height=\"200\" rx=\"10\" fill=\"#2c211b\" stroke=\"{border}\" stroke-width=\"1\"/>"
        )?;
        write!(
            out,
            "<circle cx=\"{}\" cy=\"530\" r=\"4\" fill=\"{}\"/>",
            x + 20,
            vendor_color(&report.participant.vendor)
        )?;
        out.push_str(&svg_text(
            x + 32,
            533,
            &report.participant.vendor.to_uppercase(),
            &vendor_style,
            &[],
        ));
        out.push_str(&svg_text(
            x + 18,
            558,
            &clipped(&report.participant.model, 32),
            &model_style,
            &[],
        ));
        out.push_str(&svg_text(
            x + 18,
            574,
            &clipped(&format!("read {read_models} · blind"), 42),
            &role_style,
            &[],
        ));
        write!(
            out,
            "<line x1=\"{}\" y1=\"586\" x2=\"{}\" y2=\"586\" stroke=\"{border}\"/>",
            x + 18,
            x + 234
        )?;
        for (bullet_index, bullet) in fields.reviews[&report.dispatch.task].iter().enumerate() {
            let glyph_fill = match bullet.tag.as_str() {
                "?" => "#f08a59",
                "+" => "#f0e6da",
                _ => "#8f7f70",
            };
            let y = 608 + bullet_index as i32 * 20;
            out.push_str(&svg_text(
                x + 18,
                y,
                &bullet.tag,
                &format!(
                    "{mono};font-size:10.5px;font-weight:700;fill:{glyph_fill};text-anchor:start"
                ),
                &[("data-delta", bullet.tag.as_str())],
            ));
            out.push_str(&svg_text(
                x + 32,
                y,
                &clipped(&bullet.text, 43),
                &body_style,
                &[],
            ));
        }
        out.push_str(&svg_text(
            x + 18,
            686,
            &format!("{}/…/report.md", report.dispatch.task),
            &path_style,
            &[],
        ));
        out.push_str("</g>");
    }

    if fast {
        for (source, report) in card_centers.iter().zip(extraction) {
            write!(
                out,
                "<path data-arrow=\"extract-curator\" data-task=\"{}\" d=\"M{source},424 C{source},600 {center},600 {center},776\" fill=\"none\" stroke=\"#b9a998\" stroke-width=\"1.25\" opacity=\"0.55\" marker-end=\"url(#ah)\"/>",
                html_escape(&report.dispatch.task, true)
            )?;
        }
    } else {
        for source in &card_centers {
            write!(
                out,
                "<path d=\"M{source},704 C{source},740 {center},740 {center},776\" fill=\"none\" stroke=\"#b9a998\" stroke-width=\"1.25\" opacity=\"0.55\" marker-end=\"url(#ah)\"/>"
            )?;
        }
    }
    out.push_str("<g data-pill=\"curate\">");
    write!(
        out,
        "<rect x=\"{}\" y=\"729\" width=\"120\" height=\"22\" rx=\"11\" fill=\"#241a15\" stroke=\"{border}\"/>",
        center - 60
    )?;
    out.push_str(&svg_text(
        center,
        743,
        if fast { "2 · CURATE" } else { "3 · CURATE" },
        &stage_style,
        &[],
    ));
    out.push_str("</g>");
    write!(
        out,
        "<g data-card=\"curator\" data-task=\"{}\" data-record-path=\"{}\">",
        html_escape(curator_task, true),
        html_escape(&curator_path.display().to_string(), true)
    )?;
    write!(
        out,
        "<rect x=\"{curator_x}\" y=\"776\" width=\"400\" height=\"92\" rx=\"10\" fill=\"rgba(240,138,89,0.10)\" stroke=\"#f08a59\" stroke-width=\"1.5\"/>"
    )?;
    write!(
        out,
        "<circle cx=\"{}\" cy=\"802\" r=\"4\" fill=\"{}\"/>",
        curator_x + 22,
        vendor_color(&curator.vendor)
    )?;
    out.push_str(&svg_text(
        curator_x + 34,
        805,
        &curator.vendor.to_uppercase(),
        &vendor_style,
        &[],
    ));
    out.push_str(&svg_text(
        curator_x + 18,
        830,
        &clipped(&format!("{} · curator", curator.model), 48),
        &format!("{sans};font-size:14px;font-weight:700;fill:#f0e6da;text-anchor:start"),
        &[],
    ));
    out.push_str(&svg_text(
        curator_x + 18,
        848,
        &fields.curator_summary,
        &format!("{mono};font-size:8.5px;font-weight:400;fill:#b9a998;text-anchor:start"),
        &[],
    ));
    out.push_str(&svg_text(
        curator_x + 18,
        862,
        &format!("{curator_task}/…/report.md"),
        &path_style,
        &[],
    ));
    out.push_str("</g>");
    write!(
        out,
        "<line x1=\"{center}\" y1=\"868\" x2=\"{center}\" y2=\"906\" stroke=\"#b9a998\" stroke-width=\"1.25\" opacity=\"0.55\" marker-end=\"url(#ah)\"/>"
    )?;
    out.push_str("<g data-pill=\"final-answer\">");
    write!(
        out,
        "<rect x=\"{}\" y=\"912\" width=\"380\" height=\"54\" rx=\"27\" fill=\"#f08a59\"/>",
        center - 190
    )?;
    out.push_str(&svg_text(
        center,
        936,
        "FINAL ANSWER",
        &format!(
            "{sans};font-size:13px;font-weight:800;fill:#241a15;text-anchor:middle;letter-spacing:0.06em"
        ),
        &[],
    ));
    out.push_str(&svg_text(
        center,
        952,
        "at the top of this page",
        &format!(
            "{mono};font-size:9.5px;font-weight:500;fill:#241a15;text-anchor:middle;opacity:0.75"
        ),
        &[],
    ));
    out.push_str("</g></svg>");
    Ok(out)
}

fn round_reports(round: &ForumRound) -> Result<(Vec<RunReport>, Vec<RunReport>)> {
    let participants = round
        .panel
        .iter()
        .map(ManifestParticipant::participant)
        .collect::<Result<Vec<_>>>()?;
    let count = participants.len();
    let reports = |tasks: &[String], paths: &[PathBuf]| {
        participants
            .iter()
            .zip(tasks)
            .zip(paths)
            .map(|((participant, task), path)| RunReport {
                participant: participant.clone(),
                dispatch: Dispatch {
                    task: task.clone(),
                    started_tx: String::new(),
                    participant: participant.clone(),
                    closed: true,
                },
                path: path.clone(),
            })
            .collect::<Vec<_>>()
    };
    Ok((
        reports(
            &round.first_stage_tasks,
            &round.promoted_report_paths[..count],
        ),
        reports(
            &round.cross_review_tasks,
            &round.promoted_report_paths[count..],
        ),
    ))
}

fn render_multi_round_svg(
    rounds: &[ForumRound],
    curator: &Participant,
    curator_task: &str,
    curator_path: &Path,
    fields: &MultiRoundDiagramFields,
) -> Result<String> {
    if rounds.is_empty() || rounds.len() != fields.rounds.len() {
        bail!("multi-round diagram requires matching manifest and diagram rounds");
    }
    if rounds.iter().any(|round| {
        round.panel.len() < if round.fast { 1 } else { 2 }
            || round.cross_review_tasks.len() != if round.fast { 0 } else { round.panel.len() }
    }) {
        bail!("diagram round has an invalid panel or review roster");
    }
    let max_count = rounds
        .iter()
        .map(|round| round.panel.len())
        .max()
        .unwrap_or(0);
    let card_width = 252i32;
    let gap = 30i32;
    let margin = 32i32;
    let width =
        (margin * 2 + max_count as i32 * card_width + (max_count as i32 - 1) * gap).max(544);
    let center = width / 2;
    let round_height = 650i32;
    let curator_y = 32 + rounds.len() as i32 * round_height;
    let height = curator_y + 190;
    let sans = "font-family:-apple-system,'SF Pro Text','Segoe UI',Helvetica,Arial,sans-serif";
    let mono = "font-family:ui-monospace,'SF Mono',Menlo,Consolas,monospace";
    let border = "rgba(240,230,218,0.13)";
    let body_style =
        format!("{sans};font-size:10.5px;font-weight:400;fill:#b9a998;text-anchor:start");
    let path_style = format!("{mono};font-size:8px;font-weight:400;fill:#8f7f70;text-anchor:start");
    let stage_style = format!(
        "{mono};font-size:8px;font-weight:500;fill:#8f7f70;text-anchor:middle;letter-spacing:0.14em"
    );
    let mut out = String::new();
    write!(
        out,
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{width}\" height=\"{height}\" viewBox=\"0 0 {width} {height}\" role=\"img\" aria-label=\"Multiple forum rounds converge on one curator\">"
    )?;
    write!(
        out,
        "<rect x=\"0.5\" y=\"0.5\" width=\"{}\" height=\"{}\" rx=\"16\" fill=\"#241a15\" stroke=\"{border}\"/>",
        width - 1,
        height - 1
    )?;
    out.push_str("<defs><marker id=\"ah\" markerWidth=\"7\" markerHeight=\"7\" refX=\"6\" refY=\"3.5\" orient=\"auto\"><path d=\"M0,0 L7,3.5 L0,7 z\" fill=\"#b9a998\"/></marker></defs>");

    for (round, diagram) in rounds.iter().zip(&fields.rounds) {
        if round.round != diagram.round || round.kind != diagram.kind {
            bail!("diagram round order does not match the manifest");
        }
        let (extraction, reviews) = round_reports(round)?;
        let count = extraction.len() as i32;
        let row_width = count * card_width + (count - 1) * gap;
        let row_left = center - row_width / 2;
        let xs = (0..count)
            .map(|index| row_left + index * (card_width + gap))
            .collect::<Vec<_>>();
        let centers = xs.iter().map(|x| x + card_width / 2).collect::<Vec<_>>();
        let top = 32 + (round.round as i32 - 1) * round_height;
        let prompt_lines = wrap_question(&round.input.diagram_prompt());
        write!(out, "<g data-round=\"{}\">", round.round)?;
        write!(
            out,
            "<g data-card=\"prompt\" data-round=\"{}\"><rect x=\"{}\" y=\"{top}\" width=\"480\" height=\"88\" rx=\"10\" fill=\"#2c211b\" stroke=\"{border}\"/>",
            round.round,
            center - 240
        )?;
        out.push_str(&svg_text(
            center,
            top + 23,
            &format!(
                "ROUND {} · {}",
                round.round,
                round.kind.slug().to_uppercase()
            ),
            &stage_style,
            &[],
        ));
        out.push_str(&svg_text(
            center,
            top + 48,
            &prompt_lines[0],
            &format!("{sans};font-size:12px;font-weight:500;fill:#f0e6da;text-anchor:middle"),
            &[],
        ));
        out.push_str(&svg_text(
            center,
            top + 66,
            &prompt_lines[1],
            &format!("{sans};font-size:12px;font-weight:500;fill:#f0e6da;text-anchor:middle"),
            &[],
        ));
        out.push_str("</g>");
        for destination in &centers {
            write!(
                out,
                "<path d=\"M{center},{} C{center},{} {destination},{} {destination},{}\" fill=\"none\" stroke=\"#b9a998\" stroke-width=\"1.25\" opacity=\"0.55\" marker-end=\"url(#ah)\"/>",
                top + 88,
                top + 114,
                top + 114,
                top + 142
            )?;
        }
        out.push_str(&svg_text(
            center,
            top + 121,
            "1 · PARALLEL · ISOLATED",
            &stage_style,
            &[],
        ));
        for (x, report) in xs.iter().zip(&extraction) {
            let lines = &diagram.fields.extracts[&report.dispatch.task];
            write!(
                out,
                "<g data-card=\"extract\" data-round=\"{}\" data-task=\"{}\" data-record-path=\"{}\"><rect x=\"{x}\" y=\"{}\" width=\"252\" height=\"190\" rx=\"10\" fill=\"#2c211b\" stroke=\"{border}\"/>",
                round.round,
                html_escape(&report.dispatch.task, true),
                html_escape(&report.path.display().to_string(), true),
                top + 142
            )?;
            write!(
                out,
                "<circle cx=\"{}\" cy=\"{}\" r=\"4\" fill=\"{}\"/>",
                x + 20,
                top + 166,
                vendor_color(&report.participant.vendor)
            )?;
            out.push_str(&svg_text(
                x + 32,
                top + 169,
                &report.participant.vendor.to_uppercase(),
                &format!("{mono};font-size:8.5px;font-weight:500;fill:#b9a998;text-anchor:start;letter-spacing:0.12em"),
                &[],
            ));
            out.push_str(&svg_text(
                x + 18,
                top + 194,
                &clipped(&report.participant.model, 32),
                &format!("{sans};font-size:15px;font-weight:700;fill:#f0e6da;text-anchor:start"),
                &[],
            ));
            out.push_str(&svg_text(
                x + 18,
                top + 211,
                &clipped(&report.dispatch.task, 36),
                &path_style,
                &[],
            ));
            for (index, line) in lines.iter().take(4).enumerate() {
                out.push_str(&svg_text(
                    x + 18,
                    top + 238 + index as i32 * 17,
                    line,
                    &body_style,
                    &[],
                ));
            }
            out.push_str(&svg_text(
                x + 18,
                top + 316,
                &format!("{}/…/report.md", report.dispatch.task),
                &path_style,
                &[],
            ));
            out.push_str("</g>");
        }
        for (source_index, source) in centers.iter().enumerate() {
            for (target_index, destination) in centers.iter().enumerate() {
                if !round.fast && source_index != target_index {
                    write!(
                        out,
                        "<path d=\"M{source},{} C{source},{} {destination},{} {destination},{}\" fill=\"none\" stroke=\"#b9a998\" stroke-width=\"1.25\" opacity=\"0.55\" marker-end=\"url(#ah)\"/>",
                        top + 332,
                        top + 365,
                        top + 365,
                        top + 398
                    )?;
                }
            }
        }
        if !round.fast {
            out.push_str(&svg_text(
                center,
                top + 360,
                "2 · CROSS-REVIEW · NEVER SELF",
                &stage_style,
                &[],
            ));
        }
        for (x, report) in xs.iter().zip(&reviews) {
            write!(
                out,
                "<g data-card=\"review\" data-round=\"{}\" data-task=\"{}\" data-record-path=\"{}\"><rect x=\"{x}\" y=\"{}\" width=\"252\" height=\"190\" rx=\"10\" fill=\"#2c211b\" stroke=\"{border}\"/>",
                round.round,
                html_escape(&report.dispatch.task, true),
                html_escape(&report.path.display().to_string(), true),
                top + 398
            )?;
            out.push_str(&svg_text(
                x + 18,
                top + 428,
                &clipped(
                    &format!("{} · {}", report.participant.model, report.dispatch.task),
                    38,
                ),
                &format!("{sans};font-size:13px;font-weight:700;fill:#f0e6da;text-anchor:start"),
                &[],
            ));
            for (index, bullet) in diagram.fields.reviews[&report.dispatch.task]
                .iter()
                .enumerate()
            {
                let y = top + 458 + index as i32 * 24;
                out.push_str(&svg_text(
                    x + 18,
                    y,
                    &bullet.tag,
                    &format!(
                        "{mono};font-size:10.5px;font-weight:700;fill:#f08a59;text-anchor:start"
                    ),
                    &[("data-delta", bullet.tag.as_str())],
                ));
                out.push_str(&svg_text(x + 32, y, &bullet.text, &body_style, &[]));
            }
            out.push_str(&svg_text(
                x + 18,
                top + 574,
                &format!("{}/…/report.md", report.dispatch.task),
                &path_style,
                &[],
            ));
            out.push_str("</g>");
        }
        if round.fast {
            for (source, report) in centers.iter().zip(&extraction) {
                write!(
                    out,
                    "<path data-arrow=\"extract-curator\" data-task=\"{}\" d=\"M{source},{} C{source},{} {center},{} {center},{curator_y}\" fill=\"none\" stroke=\"#b9a998\" stroke-width=\"1.25\" opacity=\"0.35\" marker-end=\"url(#ah)\"/>",
                    html_escape(&report.dispatch.task, true),
                    top + 332,
                    top + 365,
                    curator_y - 30
                )?;
            }
        } else {
            for (source, report) in centers.iter().zip(&reviews) {
                write!(
                    out,
                    "<path data-arrow=\"review-curator\" data-task=\"{}\" d=\"M{source},{} C{source},{} {center},{} {center},{curator_y}\" fill=\"none\" stroke=\"#b9a998\" stroke-width=\"1.25\" opacity=\"0.35\" marker-end=\"url(#ah)\"/>",
                    html_escape(&report.dispatch.task, true),
                    top + 588,
                    top + 620,
                    curator_y - 30
                )?;
            }
        }
        out.push_str("</g>");
    }

    let curator_x = center - 200;
    write!(
        out,
        "<g data-card=\"curator\" data-task=\"{}\" data-record-path=\"{}\"><rect x=\"{curator_x}\" y=\"{curator_y}\" width=\"400\" height=\"92\" rx=\"10\" fill=\"rgba(240,138,89,0.10)\" stroke=\"#f08a59\" stroke-width=\"1.5\"/>",
        html_escape(curator_task, true),
        html_escape(&curator_path.display().to_string(), true)
    )?;
    write!(
        out,
        "<circle cx=\"{}\" cy=\"{}\" r=\"4\" fill=\"{}\"/>",
        curator_x + 22,
        curator_y + 26,
        vendor_color(&curator.vendor)
    )?;
    out.push_str(&svg_text(
        curator_x + 34,
        curator_y + 29,
        &curator.vendor.to_uppercase(),
        &format!("{mono};font-size:8.5px;font-weight:500;fill:#b9a998;text-anchor:start;letter-spacing:0.12em"),
        &[],
    ));
    out.push_str(&svg_text(
        curator_x + 18,
        curator_y + 54,
        &clipped(&format!("{} · curator", curator.model), 48),
        &format!("{sans};font-size:14px;font-weight:700;fill:#f0e6da;text-anchor:start"),
        &[],
    ));
    out.push_str(&svg_text(
        curator_x + 18,
        curator_y + 74,
        &fields.curator_summary,
        &path_style,
        &[],
    ));
    out.push_str("</g>");
    write!(
        out,
        "<g data-pill=\"final-answer\"><rect x=\"{}\" y=\"{}\" width=\"380\" height=\"48\" rx=\"24\" fill=\"#f08a59\"/>",
        center - 190,
        curator_y + 112
    )?;
    out.push_str(&svg_text(
        center,
        curator_y + 141,
        "FINAL ANSWER",
        &format!("{sans};font-size:13px;font-weight:800;fill:#241a15;text-anchor:middle;letter-spacing:0.06em"),
        &[],
    ));
    out.push_str("</g></svg>");
    Ok(out)
}

fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let a = chunk[0];
        let b = chunk.get(1).copied().unwrap_or(0);
        let c = chunk.get(2).copied().unwrap_or(0);
        out.push(ALPHABET[(a >> 2) as usize] as char);
        out.push(ALPHABET[(((a & 0x03) << 4) | (b >> 4)) as usize] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[(((b & 0x0f) << 2) | (c >> 6)) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[(c & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

fn section_titles(mdx: &str) -> Vec<(usize, String)> {
    let mut titles = Vec::new();
    let mut offset = 0;
    while let Some(found) = mdx[offset..].find("<Section") {
        let start = offset + found;
        let Some(end_offset) = mdx[start..].find('>') else {
            break;
        };
        let tag = &mdx[start..start + end_offset];
        let title = tag
            .find("title=\"")
            .and_then(|index| {
                let rest = &tag[index + 7..];
                rest.find('"').map(|end| rest[..end].to_string())
            })
            .unwrap_or_default();
        titles.push((start, title));
        offset = start + end_offset + 1;
    }
    titles
}

fn task_is_present(mdx: &str, task: &str) -> bool {
    mdx.match_indices(task).any(|(index, _)| {
        mdx[index + task.len()..]
            .chars()
            .next()
            .is_none_or(|next| next != '.' && !next.is_ascii_digit())
    })
}

/// Run metadata is manifest truth the orchestrator already holds; rendering it
/// here keeps the roster correct and scannable regardless of curator quality.
/// It closes the artifact as a footer so the document opens with its source.
fn render_about_run(
    kind: ForumKind,
    extraction: &[RunReport],
    reviews: &[RunReport],
    curator: &Participant,
    started_at: &str,
) -> String {
    let started = started_at
        .get(..16)
        .map_or_else(|| started_at.to_string(), |head| head.replace('T', " "));
    let mut lines = vec![format!("- **Participants ({}):**", extraction.len())];
    for report in extraction {
        lines.push(format!("    - {}", report.participant.identity()));
    }
    lines.push(format!("- **Curator:** {}", curator.identity()));
    lines.push(format!(
        "- **Run:** {} {} reports · {} cross-reviews · started {} UTC",
        extraction.len(),
        match kind {
            ForumKind::Ask => "extraction",
            ForumKind::Critique => "critique",
        },
        reviews.len(),
        started
    ));
    format!(
        "<Section title=\"About this run\">\n<RichText>\n{}\n</RichText>\n<Callout tone=\"warning\">Multi-model synthesis, not verified truth. Verify consequential claims before acting.</Callout>\n</Section>",
        lines.join("\n")
    )
}

fn render_forum_about_run(manifest: &ForumManifest, curator: &Participant) -> Result<String> {
    let started = manifest.started_at.get(..16).map_or_else(
        || manifest.started_at.clone(),
        |head| head.replace('T', " "),
    );
    let mut lines = vec![format!("- **Rounds:** {}", manifest.rounds.len())];
    for round in &manifest.rounds {
        let tasks = round
            .first_stage_tasks
            .iter()
            .chain(&round.cross_review_tasks)
            .cloned()
            .collect::<Vec<_>>()
            .join(" ");
        lines.push(format!(
            "    - Round {} · {} · {} · {}",
            round.round,
            round.kind.slug(),
            escape_rich_text(&clipped(&round.input.diagram_prompt(), 80)),
            tasks
        ));
        lines.push(format!("      Participants ({}):", round.panel.len()));
        for participant in &round.panel {
            let identity = participant.participant()?.identity();
            lines.push(format!("        - {identity}"));
        }
    }
    lines.push(format!("- **Curator:** {}", curator.identity()));
    lines.push(format!("- **Started:** {started} UTC"));
    Ok(format!(
        "<Section title=\"About this run\">\n<RichText>\n{}\n</RichText>\n<Callout tone=\"warning\">Multi-model synthesis, not verified truth. Verify consequential claims before acting.</Callout>\n</Section>",
        lines.join("\n")
    ))
}

fn assemble_artifact(
    draft: &str,
    input: &ForumInput,
    svg: &str,
    about_run: &str,
    raw_tasks: &[String],
) -> Result<String> {
    let has_reviews = svg.contains("data-card=\"review\"");
    let (first_placeholder, other_placeholder, first_title, required, image_alt, image_caption) =
        match input.kind() {
            ForumKind::Ask => (
                QUESTION_PLACEHOLDER,
                TARGET_PLACEHOLDER,
                "Question",
                &[
                    "Question",
                    "Final answer",
                    "From question to answer",
                    "Knowledge map",
                ][..],
                if has_reviews {
                    "Question flows through independent extraction and blind cross-review into curation"
                } else {
                    "Question flows through independent extraction directly into curation"
                },
                "From the verbatim question to the curated final answer.",
            ),
            ForumKind::Critique => (
                TARGET_PLACEHOLDER,
                QUESTION_PLACEHOLDER,
                "Target",
                &["Target", "Verdict", "Findings", "From target to verdict"][..],
                if has_reviews {
                    "Target flows through independent critique and blind cross-review into curation"
                } else {
                    "Target flows through independent critique directly into curation"
                },
                "From the verbatim target to the curated verdict.",
            ),
        };
    if contains_model_svg(draft) {
        bail!("curator draft contained model-authored SVG");
    }
    if draft.matches(first_placeholder).count() != 1
        || draft.contains(other_placeholder)
        || draft.matches(DIAGRAM_PLACEHOLDER).count() != 1
        || draft.matches(RUN_STATS_PLACEHOLDER).count() != 1
    {
        bail!("curator draft must contain each orchestrator placeholder once");
    }
    if !draft.trim_end().ends_with(RUN_STATS_PLACEHOLDER) {
        bail!("run-stats placeholder must be the final block of the draft");
    }

    let first_section = match input {
        ForumInput::Ask { question } => format!(
            "<Section title=\"Question\">\n<RichText>\n{}\n</RichText>\n</Section>",
            escape_rich_text(question)
        ),
        ForumInput::Critique { target, focus, .. } => {
            let focus = focus
                .as_deref()
                .map(|focus| format!("\n\n**Focus:** {}", escape_rich_text(focus)))
                .unwrap_or_default();
            format!(
                "<Section title=\"Target\">\n<RichText>\n{}{focus}\n</RichText>\n</Section>",
                escape_rich_text(target)
            )
        }
    };
    let image = format!(
        "<Image src=\"data:image/svg+xml;base64,{}\" alt=\"{image_alt}\" caption=\"{image_caption}\" />",
        base64_encode(svg.as_bytes()),
    );
    let mdx = draft
        .replace(first_placeholder, &first_section)
        .replace(DIAGRAM_PLACEHOLDER, &image)
        .replace(RUN_STATS_PLACEHOLDER, about_run);

    let sections = section_titles(&mdx);
    if sections.first().map(|(_, title)| title.as_str()) != Some(first_title) {
        bail!("{first_title} must be the first Section");
    }
    let first_offset = sections
        .iter()
        .find_map(|(offset, title)| (title == first_title).then_some(*offset));
    if first_offset.is_none_or(|offset| !mdx[offset..].starts_with(&first_section)) {
        match input.kind() {
            ForumKind::Ask => {
                bail!("Question section does not match the input question verbatim")
            }
            ForumKind::Critique => {
                bail!("Target section does not match the input target verbatim")
            }
        }
    }
    let missing = required
        .iter()
        .filter(|required| !sections.iter().any(|(_, section)| section == *required))
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        bail!("curator draft is missing required sections: {missing:?}");
    }
    let required_offsets = required
        .iter()
        .map(|required| {
            sections
                .iter()
                .find_map(|(offset, section)| (section == required).then_some(*offset))
                .unwrap()
        })
        .collect::<Vec<_>>();
    if required_offsets.windows(2).any(|pair| pair[0] >= pair[1]) {
        bail!("curator draft required sections are out of order");
    }
    let missing_tasks = raw_tasks
        .iter()
        .filter(|task| !task_is_present(draft, task))
        .collect::<Vec<_>>();
    if !missing_tasks.is_empty() {
        bail!("curator draft omitted raw-report task ids: {missing_tasks:?}");
    }
    Ok(mdx)
}

fn parse_participant(raw: &str) -> Result<Participant> {
    let fields = raw.split(',').map(str::trim).collect::<Vec<_>>();
    if fields.len() != 4 || fields.iter().any(|field| field.is_empty()) {
        bail!("participant must be mode,harness,model,effort");
    }
    if fields.iter().any(|field| field.contains(['\n', '\r', '·'])) {
        bail!("participant fields must be single-line values without `·`");
    }
    let (vendor, model) = if let Some((vendor, model)) = fields[2].split_once('/') {
        (vendor.to_string(), model.to_string())
    } else {
        match fields[1] {
            "codex" => ("openai".to_string(), fields[2].to_string()),
            "claude" => ("anthropic".to_string(), fields[2].to_string()),
            harness => bail!(
                "cannot derive vendor for {harness}/{}; use provider/model",
                fields[2]
            ),
        }
    };
    Ok(Participant {
        mode: fields[0].to_string(),
        harness: fields[1].to_string(),
        dispatch_model: fields[2].to_string(),
        effort: fields[3].to_string(),
        vendor,
        model,
    })
}

fn validate_participants(participants: &[Participant], fast: bool) -> Result<()> {
    if fast && participants.is_empty() {
        bail!("at least one participant is required with --fast");
    }
    if !fast && participants.len() < 2 {
        bail!("at least two participants are required");
    }
    let models = participants
        .iter()
        .map(|participant| (&participant.vendor, &participant.model))
        .collect::<BTreeSet<_>>();
    if models.len() != participants.len() {
        bail!("participants must use different vendor/model identities");
    }
    let available = transport_profiles()
        .into_iter()
        .filter(|profile| profile.ready() && profile.interaction.is_unattended())
        .map(|profile| (profile.mode, profile.harness))
        .collect::<BTreeSet<_>>();
    let missing = participants
        .iter()
        .map(|participant| (participant.mode.clone(), participant.harness.clone()))
        .filter(|pair| !available.contains(pair))
        .collect::<BTreeSet<_>>();
    if !missing.is_empty() {
        bail!("unsupported or unavailable unattended transports: {missing:?}");
    }
    Ok(())
}

fn validate_question(question: &str) -> Result<()> {
    if question.is_empty() {
        bail!("question must not be empty");
    }
    if contains_orchestrator_placeholder(question) {
        bail!("question must not contain orchestrator placeholders");
    }
    if question.starts_with('-') {
        bail!("question must not start with '-'");
    }
    Ok(())
}

fn validate_focus(focus: &str) -> Result<()> {
    if focus.is_empty() {
        bail!("focus must not be empty");
    }
    if focus.contains(['\n', '\r']) {
        bail!("focus must be one line");
    }
    if contains_orchestrator_placeholder(focus) {
        bail!("focus must not contain orchestrator placeholders");
    }
    if focus.starts_with('-') {
        bail!("focus must not start with '-'");
    }
    Ok(())
}

fn validate_target(target: &str) -> Result<()> {
    if target.len() > MAX_TARGET_BYTES {
        bail!(
            "target file exceeds 64 KiB ({} bytes; maximum {MAX_TARGET_BYTES})",
            target.len()
        );
    }
    if target.trim().is_empty() {
        bail!("target file must not be empty");
    }
    if contains_orchestrator_placeholder(target) {
        bail!("target file must not contain orchestrator placeholders");
    }
    Ok(())
}

fn contains_orchestrator_placeholder(value: &str) -> bool {
    [
        QUESTION_PLACEHOLDER,
        TARGET_PLACEHOLDER,
        DIAGRAM_PLACEHOLDER,
        RUN_STATS_PLACEHOLDER,
    ]
    .iter()
    .any(|placeholder| value.contains(placeholder))
}

fn forum_dir(ledger: &Path) -> PathBuf {
    project_tmp_dir(ledger).join("forum")
}

fn forum_manifest_path(ledger: &Path, forum: &str) -> PathBuf {
    forum_dir(ledger).join(format!("{forum}.json"))
}

fn validate_forum_id(forum: &str) -> Result<()> {
    if !is_valid_task_path_id(forum) || forum.contains('.') {
        bail!("--forum must name a parent task id such as TASK-XXXXX");
    }
    Ok(())
}

fn validate_manifest(manifest: &ForumManifest) -> Result<()> {
    if manifest.version != 1 {
        bail!("unsupported forum manifest version {}", manifest.version);
    }
    validate_forum_id(&manifest.forum)?;
    if manifest.project.trim().is_empty()
        || manifest.source_ref.trim().is_empty()
        || manifest.started_at.trim().is_empty()
    {
        bail!("forum manifest omitted required forum metadata");
    }
    if manifest
        .artifact_id
        .as_deref()
        .is_some_and(|id| !is_valid_greenfield_artifact_id(id))
    {
        bail!("forum manifest contains an invalid artifact id");
    }
    let mut tasks = BTreeSet::new();
    for (index, round) in manifest.rounds.iter().enumerate() {
        if round.round != index + 1 || round.kind != round.input.kind() {
            bail!("forum manifest rounds must be contiguous and match their input kind");
        }
        let count = round.panel.len();
        let minimum = if round.fast { 1 } else { 2 };
        let review_count = if round.fast { 0 } else { count };
        if count < minimum
            || round.first_stage_tasks.len() != count
            || round.cross_review_tasks.len() != review_count
            || round.promoted_report_paths.len() != count + review_count
        {
            bail!(
                "forum manifest round {} has mismatched panel, task, or report counts",
                round.round
            );
        }
        for participant in &round.panel {
            participant.participant()?;
        }
        // The manifest sits on disk between invocations; re-check what intake
        // validated so an edited or foreign manifest cannot push unvalidated
        // input or another forum's task ids into the submitted artifact.
        match &round.input {
            ForumInput::Ask { question } => validate_question(question)
                .with_context(|| format!("forum manifest round {} question", round.round))?,
            ForumInput::Critique { target, focus, .. } => {
                validate_target(target)
                    .with_context(|| format!("forum manifest round {} target", round.round))?;
                if let Some(focus) = focus {
                    validate_focus(focus)
                        .with_context(|| format!("forum manifest round {} focus", round.round))?;
                }
            }
        }
        for task in round
            .first_stage_tasks
            .iter()
            .chain(&round.cross_review_tasks)
        {
            if !is_valid_task_path_id(task) || !task.starts_with(&format!("{}.", manifest.forum)) {
                bail!(
                    "forum manifest round {} contains task {task} that does not belong to {}",
                    round.round,
                    manifest.forum
                );
            }
            if !tasks.insert(task) {
                bail!("forum manifest repeats task {task}");
            }
        }
        if round
            .promoted_report_paths
            .iter()
            .any(|path| path.as_os_str().is_empty())
        {
            bail!(
                "forum manifest round {} contains an empty report path",
                round.round
            );
        }
    }
    Ok(())
}

fn read_manifest(path: &Path) -> Result<ForumManifest> {
    let manifest: ForumManifest = serde_json::from_str(
        &std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?,
    )
    .with_context(|| format!("parse {}", path.display()))?;
    validate_manifest(&manifest).with_context(|| format!("validate {}", path.display()))?;
    Ok(manifest)
}

fn write_manifest(path: &Path, manifest: &ForumManifest) -> Result<()> {
    validate_manifest(manifest)?;
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("manifest path has no parent: {}", path.display()))?;
    std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    let mut tmp = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("create temporary manifest in {}", parent.display()))?;
    std::io::Write::write_all(&mut tmp, &serde_json::to_vec_pretty(manifest)?)?;
    tmp.persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("replace {}", path.display()))?;
    Ok(())
}

fn validate_join_request(
    forum: &str,
    project: &str,
    explicit_curator: bool,
    manifest: Option<&ForumManifest>,
    parent_state: Option<&str>,
) -> Result<()> {
    validate_forum_id(forum)?;
    if explicit_curator {
        bail!("--forum cannot be combined with --curator");
    }
    let manifest = manifest.ok_or_else(|| anyhow::anyhow!("unknown forum {forum}"))?;
    if manifest.forum != forum || manifest.project != project {
        bail!("forum {forum} belongs to a different project");
    }
    if manifest.curation_mode == CurationMode::Dispatched {
        bail!("forum {forum} was created with a dispatched curator");
    }
    if manifest.state == ForumState::Curated {
        bail!("forum {forum} is already curated");
    }
    if manifest.curation_task.is_some() {
        bail!(
            "forum {forum} already reserved its curation task; finish `orgasmic forum curate` instead of adding rounds"
        );
    }
    match parent_state {
        Some("backlog" | "todo" | "in_progress" | "in_review") => Ok(()),
        Some(state) => bail!("forum {forum} parent is not open ({state})"),
        None => bail!("forum {forum} parent task is unknown"),
    }
}

fn next_task_ordinal(manifest: &ForumManifest) -> usize {
    manifest
        .rounds
        .iter()
        .flat_map(|round| {
            round
                .first_stage_tasks
                .iter()
                .chain(&round.cross_review_tasks)
        })
        .filter_map(|task| task.rsplit_once('.'))
        .filter_map(|(_, ordinal)| ordinal.parse::<usize>().ok())
        .max()
        .unwrap_or(0)
        + 1
}

fn markdown_heading(line: &str) -> Option<(usize, &str)> {
    let trimmed = line.trim_start();
    let marks = trimmed.chars().take_while(|ch| *ch == '#').count();
    (marks > 0 && trimmed.as_bytes().get(marks) == Some(&b' '))
        .then(|| (marks, trimmed[marks + 1..].trim()))
}

fn without_completion_section(compiled: &str) -> Result<String> {
    let lines = compiled.lines().collect::<Vec<_>>();
    let (start, level) = lines
        .iter()
        .enumerate()
        .find_map(|(index, line)| {
            markdown_heading(line)
                .filter(|(_, title)| *title == "Completion")
                .map(|(level, _)| (index, level))
        })
        .ok_or_else(|| anyhow::anyhow!("compiled curator contract omitted Completion section"))?;
    let end = lines
        .iter()
        .enumerate()
        .skip(start + 1)
        .find_map(|(index, line)| {
            markdown_heading(line)
                .filter(|(next_level, _)| *next_level <= level)
                .map(|_| index)
        })
        .unwrap_or(lines.len());
    let mut kept = lines[..start]
        .iter()
        .chain(&lines[end..])
        .copied()
        .collect::<Vec<_>>()
        .join("\n");
    kept.push('\n');
    Ok(kept)
}

fn self_curation_manifest(manifest: &ForumManifest) -> Result<String> {
    let mut text = format!(
        "Forum: {}\nStarted UTC: {}\nRounds: {}\n",
        manifest.forum,
        manifest.started_at,
        manifest.rounds.len()
    );
    for round in &manifest.rounds {
        writeln!(
            text,
            "\nRound {} · {}\nInput: {}\nParticipants:",
            round.round,
            round.kind.slug(),
            round.input.short_label()
        )?;
        for participant in &round.panel {
            writeln!(text, "- {}", participant.participant()?.identity())?;
        }
        let count = round.panel.len();
        for (task, path) in round
            .first_stage_tasks
            .iter()
            .chain(&round.cross_review_tasks)
            .zip(&round.promoted_report_paths)
        {
            writeln!(text, "- Task: {task}\n  Report: {}", path.display())?;
        }
        debug_assert_eq!(
            round.promoted_report_paths.len(),
            count + if round.fast { 0 } else { count }
        );
    }
    Ok(text)
}

/// The dispatched curator specs emit a `- Cross-review tasks` output bullet a
/// fast-only run cannot honor. The removal is string surgery against the
/// compiled spec, so refuse to continue the moment the line stops matching
/// instead of silently shipping a contract that demands nonexistent reports.
fn strip_cross_review_output_line(compiled: &str) -> Result<String> {
    let stripped = compiled.replace("- Cross-review tasks\n", "");
    if stripped == compiled {
        bail!(
            "curator spec no longer carries the `- Cross-review tasks` output line; update the fast-round contract surgery"
        );
    }
    Ok(stripped)
}

fn compile_self_contract(api: &Api, manifest: &ForumManifest, contract_path: &Path) -> Result<()> {
    let first = manifest
        .rounds
        .first()
        .ok_or_else(|| anyhow::anyhow!("cannot compile a contract for a forum without rounds"))?;
    let mut values = first.input.prompt_values();
    values.insert(
        "dispatch.brief".to_string(),
        self_curation_manifest(manifest)?,
    );
    values.insert("task.id".to_string(), manifest.forum.clone());
    let compiled = api.compile_prompt(first.kind.curator_spec(), values)?;
    let mut contract = without_completion_section(&compiled)?;
    if manifest
        .rounds
        .iter()
        .all(|round| round.cross_review_tasks.is_empty())
    {
        contract = strip_cross_review_output_line(&contract)?;
    }
    contract.push_str(
        "\n# Self-curated forum submission\n\nThe invoking session, not a dispatch, performs curation. Discuss and iterate before writing the two files, then pass them to `orgasmic forum curate`; do not run `dispatch finalize`. In the draft's Raw reports list, include every report task named in every manifest round; do not invent a curation task id, because the CLI mints it after the draft passes its gates. For more than one round, the diagram JSON replaces the legacy top-level `extracts` and `reviews` with:\n\n```json\n{\n  \"rounds\": [\n    {\"round\": 1, \"kind\": \"ask\", \"extracts\": [...], \"reviews\": [...]},\n    {\"round\": 2, \"kind\": \"critique\", \"extracts\": [...]}\n  ],\n  \"curator_summary\": \"short synthesis summary\",\n  \"headline\": \"short artifact title\"\n}\n```\n\nInclude every manifest round in order and every named task exactly once in its own round. A fast round has no reviews: omit its `reviews` member or use an empty array, and never invent review provenance. The first round controls the draft's verbatim first section and required section shape.\n",
    );
    std::fs::write(contract_path, contract)
        .with_context(|| format!("write {}", contract_path.display()))?;
    Ok(())
}

fn read_target(path: &Path) -> Result<String> {
    let length = std::fs::metadata(path)
        .with_context(|| format!("stat target file {}", path.display()))?
        .len();
    if length > MAX_TARGET_BYTES as u64 {
        bail!(
            "invalid target file {}: target must be at most {MAX_TARGET_BYTES} bytes, got {length}",
            path.display()
        );
    }
    let target = std::fs::read_to_string(path)
        .with_context(|| format!("read target file {} as UTF-8", path.display()))?;
    validate_target(&target).with_context(|| format!("invalid target file {}", path.display()))?;
    Ok(target)
}

fn git_output(args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .output()
        .with_context(|| format!("run git {}", args.join(" ")))?;
    if !output.status.success() {
        bail!(
            "git {} failed: {}{}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr),
            String::from_utf8_lossy(&output.stdout)
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn manifest_entry(label: &str, report: &RunReport) -> String {
    format!(
        "- {label}: {}\n  Task: {}\n  Report: {}",
        report.participant.identity(),
        report.dispatch.task,
        report.path.display()
    )
}

struct Api {
    runtime: tokio::runtime::Runtime,
    client: DaemonClient,
    project: String,
    kind: ForumKind,
}

impl Api {
    fn new(home: &Home, project: String, kind: ForumKind) -> Result<Self> {
        Ok(Self {
            runtime: tokio::runtime::Runtime::new().context("create tokio runtime")?,
            client: DaemonClient::from_home_autostart(home)?,
            project,
            kind,
        })
    }

    fn post<B: Serialize + ?Sized>(&self, path: &str, body: &B) -> Result<Value> {
        self.runtime.block_on(self.client.post_json(path, body))
    }

    fn get(&self, path: &str) -> Result<Value> {
        self.runtime.block_on(self.client.get(path))
    }

    fn compile_prompt(&self, spec: &str, values: BTreeMap<String, String>) -> Result<String> {
        let compiled = self.post(
            &format!("/prompt-specs/{spec}/compile"),
            &serde_json::json!({
                "project": self.project,
                "renderer": Value::Null,
                "values": values,
            }),
        )?;
        let errors = compiled
            .get("diagnostics")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|diagnostic| diagnostic.get("level").and_then(Value::as_str) == Some("error"))
            .filter_map(|diagnostic| diagnostic.get("message").and_then(Value::as_str))
            .collect::<Vec<_>>();
        if !errors.is_empty() {
            bail!(
                "{spec} prompt did not compile cleanly: {}",
                errors.join("; ")
            );
        }
        compiled
            .get("text")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| anyhow::anyhow!("{spec} prompt compile returned no text"))
    }

    #[allow(clippy::too_many_arguments)]
    fn create_task(
        &self,
        task_id: &str,
        title: String,
        description: &str,
        acceptance: &str,
        read_scope: &str,
        write_scope: &str,
    ) -> Result<()> {
        let body = format!(
            "** Description\n{description}\n\n** Acceptance Criteria\n- [ ] {acceptance}\n"
        );
        let response = self.post(
            &format!("/projects/{}/tasks", self.project),
            &serde_json::json!({
                "id": task_id,
                "title": title,
                "tags": [],
                "body": body,
                "reason": match self.kind {
                    ForumKind::Ask => "multi-model knowledge extraction run",
                    ForumKind::Critique => "multi-model forum critique run",
                },
                "properties": {
                    "READ_SCOPE": read_scope,
                    "WRITE_SCOPE": write_scope,
                },
                "force": false,
                "request_id": format!("forum-{}-create-{task_id}", self.kind.slug()),
            }),
        )?;
        if response.get("id").and_then(Value::as_str) != Some(task_id) {
            bail!("task create returned an unexpected id for {task_id}: {response}");
        }
        Ok(())
    }

    fn task_state(&self, task: &str) -> Result<String> {
        self.get(&format!("/projects/{}/tasks/{task}", self.project))?
            .get("lifecycle_stage")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| anyhow::anyhow!("task {task} response omitted lifecycle_stage"))
    }

    fn update_task_state(&self, task: &str, state: &str, reason: &str) -> Result<()> {
        self.post(
            &format!("/projects/{}/tasks/{task}", self.project),
            &serde_json::json!({
                "state": state,
                "priority": Value::Null,
                "reason": reason,
                "request_id": format!("forum-{}-state-{task}-{state}", self.kind.slug()),
                "properties": {},
            }),
        )?;
        Ok(())
    }

    fn finish_task(&self, task: &str) -> Result<()> {
        loop {
            let state = self.task_state(task)?;
            let next = match state.as_str() {
                "backlog" | "todo" => "in_progress",
                "in_progress" => "in_review",
                "in_review" => "done",
                "done" => return Ok(()),
                other => bail!("cannot finish {task} from lifecycle state {other}"),
            };
            self.update_task_state(task, next, "report promoted and recorded as evidence")?;
        }
    }

    fn set_evidence(&self, task: &str, evidence: &str) -> Result<()> {
        let doc = self.get(&format!(
            "/org/node?id={task}&project={}&kind=task",
            self.project
        ))?;
        let base_version = doc
            .pointer("/source/base_version")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("task {task} node response omitted base_version"))?;
        self.post(
            &format!("/org/node/{task}/edit"),
            &serde_json::json!({
                "project": self.project,
                "kind": "task",
                "base_version": base_version,
                "request_id": format!("forum-{}-evidence-{task}", self.kind.slug()),
                "ops": [{
                    "op": "add_section",
                    "title": "Evidence",
                    "body": evidence,
                    "body_format": "default",
                }],
                "force": false,
            }),
        )?;
        Ok(())
    }

    fn submit_artifact(
        &self,
        artifact: &str,
        content: String,
        title: String,
        parent: &str,
        question: &str,
    ) -> Result<()> {
        let response = self.post(
            &format!("/artifacts/{artifact}/submit?project={}", self.project),
            &serde_json::json!({
                "content": content,
                "title": title,
                "subject_nodes": [parent],
                "prompt": question,
            }),
        )?;
        if let Some(error) = response.get("error").and_then(Value::as_str) {
            bail!("artifact submit failed: {error}");
        }
        eprintln!(
            "submitted {} version {}",
            response
                .get("artifact_id")
                .and_then(Value::as_str)
                .unwrap_or(artifact),
            response.get("version").and_then(Value::as_u64).unwrap_or(0)
        );
        Ok(())
    }
}

/// `--curator` is either a 1-based index into the panel or a standalone
/// mode,harness,model,effort spec. Either way the curator stage launches as a
/// fresh dispatch, so an index never carries stage-1 context — a spec just
/// also keeps the curator model off the stage-1 panel.
fn resolve_curator(participants: &[Participant], raw: &str) -> Result<Participant> {
    if raw.contains(',') {
        return parse_participant(raw).context("--curator participant spec");
    }
    let index = raw
        .trim()
        .parse::<usize>()
        .ok()
        .filter(|index| (1..=participants.len()).contains(index))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "--curator must be a 1-based participant index or a mode,harness,model,effort spec"
            )
        })?;
    Ok(participants[index - 1].clone())
}

fn launch(
    home: &Home,
    task: &str,
    participant: &Participant,
    brief: &Path,
    source_ref: &str,
    branch: String,
    reason: &str,
) -> Result<Dispatch> {
    let started_tx = manager::dispatch_quiet(
        home,
        DispatchArgs {
            task: vec![task.to_string()],
            kind: DispatchKind::Implementer,
            brief: brief.to_path_buf(),
            mode: participant.mode.clone(),
            harness: participant.harness.clone(),
            harness_args: Vec::new(),
            harness_args_json: None,
            from: Some(source_ref.to_string()),
            model: Some(participant.dispatch_model.clone()),
            effort: Some(participant.effort.clone()),
            credential_mode: None,
            worktree: None,
            fresh_worktree: false,
            branch: Some(branch),
            reason: Some(reason.to_string()),
            dry_run: false,
            governance_json: None,
        },
    )?;
    eprintln!("launched {task}: {}", participant.identity());
    Ok(Dispatch {
        task: task.to_string(),
        started_tx,
        participant: participant.clone(),
        closed: false,
    })
}

fn wait_barrier(home: &Home, dispatches: &[Dispatch], timeout: Duration) -> Result<()> {
    let started_tx = dispatches
        .iter()
        .map(|dispatch| dispatch.started_tx.clone())
        .collect::<Vec<_>>();
    // Deliberately dropped Python's liveness probe: retry once unconditionally, then leave unknown generations open.
    for attempt in 0..2 {
        match manager::dispatch_wait_quiet(
            home,
            DispatchWaitArgs {
                started_tx: started_tx.clone(),
                timeout: Some(timeout),
            },
        ) {
            Ok(DispatchWaitOutcome::Reported) => return Ok(()),
            Ok(DispatchWaitOutcome::Died) => bail!("a dispatch died before reporting"),
            Ok(DispatchWaitOutcome::TimedOut) => bail!("dispatch-wait timed out"),
            Err(error) if attempt == 0 => {
                eprintln!("dispatch-wait lost daemon contact; retrying once: {error:#}");
                std::thread::sleep(Duration::from_secs(2));
            }
            Err(error) => {
                let generations = dispatches
                    .iter()
                    .map(|dispatch| format!("{}={}", dispatch.task, dispatch.started_tx))
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(anyhow::Error::new(WaitUnknown(format!(
                    "dispatch-wait lost daemon contact twice; worker state is unknown, so generations were left open for recovery: {generations}; last error: {error:#}"
                ))));
            }
        }
    }
    unreachable!()
}

fn close_args(
    dispatch: &Dispatch,
    status: DispatchCloseStatus,
    report_only: bool,
) -> DispatchCloseArgs {
    DispatchCloseArgs {
        task: vec![dispatch.task.clone()],
        started_tx: Some(dispatch.started_tx.clone()),
        status,
        merge_sha: None,
        worker_commit: None,
        worker_session: None,
        reviewed_diff: None,
        properties: Vec::new(),
        verdict: None,
        tokens: None,
        wall: None,
        reason: Some(if report_only {
            "successful report-only run".to_string()
        } else {
            "multi-model orchestrator failed".to_string()
        }),
        no_review_required: false,
        fix_round_final: false,
        report_only,
        worktree_remove: true,
        no_worktree_remove: false,
        branch_delete: true,
        no_branch_delete: false,
    }
}

fn close_and_finish(
    home: &Home,
    api: &Api,
    ledger: &Path,
    dispatch: &mut Dispatch,
) -> Result<PathBuf> {
    manager::dispatch_close_quiet(home, close_args(dispatch, DispatchCloseStatus::Done, true))?;
    dispatch.closed = true;
    let relative = orgasmic_core::dispatch_record_report_rel(&dispatch.task, &dispatch.started_tx)
        .map_err(anyhow::Error::msg)?;
    let path = ledger.join(&relative);
    if !path.is_file()
        || std::fs::read_to_string(&path)
            .with_context(|| format!("read {}", path.display()))?
            .trim()
            .is_empty()
    {
        bail!("promoted report missing or empty: {}", path.display());
    }
    api.set_evidence(
        &dispatch.task,
        &format!(
            "- Promoted dispatch report: {} generation {}\n- Report path: {}\n",
            dispatch.task, dispatch.started_tx, relative
        ),
    )?;
    api.finish_task(&dispatch.task)?;
    Ok(path)
}

fn best_effort_close(home: &Home, dispatch: &mut Dispatch) {
    if dispatch.closed {
        return;
    }
    match manager::dispatch_close_quiet(
        home,
        close_args(dispatch, DispatchCloseStatus::Aborted, false),
    ) {
        Ok(()) => dispatch.closed = true,
        Err(error) => eprintln!("cleanup failed for {}: {error:#}", dispatch.task),
    }
}

fn mark_closed(active: &mut [Dispatch], closed: &Dispatch) {
    if let Some(dispatch) = active
        .iter_mut()
        .find(|dispatch| dispatch.started_tx == closed.started_tx)
    {
        dispatch.closed = closed.closed;
    }
}

fn run_forum(home: &Home, input: ForumInput, args: RunArgs) -> Result<RunResult> {
    let RunArgs {
        participant,
        fast,
        curator: curator_arg,
        forum,
        source_ref: requested_source_ref,
        artifact_id,
        project: requested_project,
        timeout,
    } = args;
    if forum.is_some() && curator_arg.is_some() {
        bail!("--forum cannot be combined with --curator");
    }
    let kind = input.kind();
    let participants = participant
        .iter()
        .map(|raw| parse_participant(raw))
        .collect::<Result<Vec<_>>>()?;
    validate_participants(&participants, fast)?;
    let curator = curator_arg
        .as_deref()
        .map(|raw| resolve_curator(&participants, raw))
        .transpose()?;
    if artifact_id
        .as_deref()
        .is_some_and(|id| !is_valid_greenfield_artifact_id(id))
    {
        bail!("--artifact-id must be ART- followed by five Crockford characters");
    }

    let ledger = manager::find_project_root()?;
    let project = manager::read_project_id(&ledger)?;
    if requested_project
        .as_deref()
        .is_some_and(|requested| requested != project)
    {
        bail!(
            "--project {} does not match current orgasmic project {project}",
            requested_project.unwrap()
        );
    }
    let default_branch = orgasmic_core::projects::read_board(home)?
        .into_iter()
        .find(|entry| entry.id == project)
        .map(|entry| entry.branch)
        .filter(|branch| !branch.is_empty())
        .unwrap_or_else(|| "main".to_string());
    let api = Api::new(home, project.clone(), kind)?;
    let round_started_at = chrono::Utc::now().to_rfc3339();
    let roster = participants
        .iter()
        .map(Participant::identity)
        .collect::<Vec<_>>()
        .join(" — ");
    let (parent, manifest_path, mut manifest, source_ref) = if let Some(forum) = forum {
        validate_forum_id(&forum)?;
        let path = forum_manifest_path(&ledger, &forum);
        let loaded = path.is_file().then(|| read_manifest(&path)).transpose()?;
        let parent_state = loaded
            .as_ref()
            .map(|_| api.task_state(&forum))
            .transpose()?;
        validate_join_request(
            &forum,
            &project,
            curator.is_some(),
            loaded.as_ref(),
            parent_state.as_deref(),
        )?;
        let manifest = loaded.unwrap();
        if requested_source_ref
            .as_deref()
            .is_some_and(|source| source != manifest.source_ref)
        {
            bail!("--from must match the forum's original source ref");
        }
        if artifact_id
            .as_deref()
            .is_some_and(|artifact| Some(artifact) != manifest.artifact_id.as_deref())
        {
            bail!("--artifact-id must match the forum's original override");
        }
        let source_ref = manifest.source_ref.clone();
        (forum, path, manifest, source_ref)
    } else {
        let source_ref = match requested_source_ref {
            Some(source_ref) => source_ref,
            None => {
                let branch = git_output(&["branch", "--show-current"])?;
                if branch == project {
                    default_branch
                } else {
                    git_output(&["rev-parse", "HEAD"])?
                }
            }
        };
        let parent = mint_node_id_for_class(NodeIdClass::Task);
        let (parent_description, parent_acceptance) = match &input {
            ForumInput::Ask { question } => (
                format!(
                    "Question: {}\n\nParticipants: {roster}",
                    question.split_whitespace().collect::<Vec<_>>().join(" ")
                ),
                if fast {
                    "All extraction reports are promoted and one curated artifact is submitted."
                } else {
                    "All extraction and blind-review reports are promoted and one curated artifact is submitted."
                },
            ),
            ForumInput::Critique {
                target,
                focus,
                basename,
            } => (
                format!(
                    "Target: {basename} ({} bytes)\nFocus: {}\n\nParticipants: {roster}",
                    target.len(),
                    focus.as_deref().unwrap_or("(none)")
                ),
                if fast {
                    "All critique reports are promoted and one curated verdict artifact is submitted."
                } else {
                    "All critique and blind-review reports are promoted and one curated verdict artifact is submitted."
                },
            ),
        };
        api.create_task(
            &parent,
            input.fallback_title(),
            &parent_description,
            parent_acceptance,
            "named promoted dispatch reports",
            "orgasmic tasks and artifact store via CLI only",
        )?;
        api.update_task_state(
            &parent,
            "in_progress",
            &format!("forum {} started", kind.slug()),
        )?;
        let path = forum_manifest_path(&ledger, &parent);
        let manifest = ForumManifest {
            version: 1,
            forum: parent.clone(),
            project: project.clone(),
            source_ref: source_ref.clone(),
            started_at: round_started_at.clone(),
            artifact_id: artifact_id.clone(),
            curation_mode: if curator.is_some() {
                CurationMode::Dispatched
            } else {
                CurationMode::SelfCurated
            },
            state: ForumState::Open,
            rounds: Vec::new(),
            curation_task: None,
            submitted_artifact: None,
        };
        write_manifest(&path, &manifest)?;
        eprintln!("parent_task={parent}");
        (parent, path, manifest, source_ref)
    };
    let round_number = manifest.rounds.len() + 1;
    let first_ordinal = next_task_ordinal(&manifest);

    let mut active = Vec::new();
    let mut extraction = Vec::new();
    let mut reviews = Vec::new();
    let result = (|| -> Result<RunResult> {
        let tmp = tempfile::Builder::new()
            .prefix(&format!("orgasmic-{}-", parent.to_ascii_lowercase()))
            .tempdir()
            .with_context(|| format!("create forum {} tempdir", kind.slug()))?;

        let extract_brief = tmp.path().join(format!("{}-stage-1.md", kind.slug()));
        std::fs::write(
            &extract_brief,
            api.compile_prompt(kind.first_stage_spec(), input.prompt_values())?,
        )?;
        let mut extract_dispatches = Vec::new();
        let (stage_title, stage_description, stage_acceptance, stage_read, stage_branch, stage_reason) =
            match kind {
                ForumKind::Ask => (
                    "Extract",
                    "Answer the parent run question independently. This is report-only; do not edit project source.",
                    "A standalone evidence-led extraction report is promoted.",
                    "question in dispatch brief; public or repository sources as needed",
                    "extract",
                    "independent multi-model extraction",
                ),
                ForumKind::Critique => (
                    "Critique",
                    "Critique the supplied target independently. This is report-only; do not edit project source.",
                    "A standalone evidence-anchored, severity-tagged critique report is promoted.",
                    "target and optional focus in dispatch brief",
                    "critic",
                    "independent multi-model critique",
                ),
            };
        for (index, participant) in participants.iter().enumerate() {
            let ordinal = first_ordinal + index;
            let task = format!("{parent}.{ordinal}");
            api.create_task(
                &task,
                format!("{stage_title} — {}", participant.identity()),
                stage_description,
                stage_acceptance,
                stage_read,
                "none; dispatch report only",
            )?;
            let dispatch = launch(
                home,
                &task,
                participant,
                &extract_brief,
                &source_ref,
                if curator.is_some() {
                    format!(
                        "mm-{}-{stage_branch}-{ordinal}",
                        parent.trim_start_matches("TASK-").to_ascii_lowercase()
                    )
                } else {
                    format!(
                        "mm-{}-r{round_number}-{stage_branch}-{ordinal}",
                        parent.trim_start_matches("TASK-").to_ascii_lowercase()
                    )
                },
                stage_reason,
            )?;
            active.push(dispatch.clone());
            extract_dispatches.push(dispatch);
        }
        wait_barrier(home, &extract_dispatches, timeout)?;
        for mut dispatch in extract_dispatches {
            let path = close_and_finish(home, &api, &ledger, &mut dispatch)?;
            mark_closed(&mut active, &dispatch);
            extraction.push(RunReport {
                participant: dispatch.participant.clone(),
                dispatch,
                path,
            });
        }

        let mut review_dispatches = Vec::new();
        for (index, participant) in participants.iter().enumerate().filter(|_| !fast) {
            let ordinal = first_ordinal + participants.len() + index;
            let task = format!("{parent}.{ordinal}");
            let report_manifest = extraction
                .iter()
                .enumerate()
                .filter(|(other_index, _)| *other_index != index)
                .map(|(_, report)| {
                    manifest_entry(
                        match kind {
                            ForumKind::Ask => "Extraction to review",
                            ForumKind::Critique => "Critique to review",
                        },
                        report,
                    )
                })
                .collect::<Vec<_>>()
                .join("\n\n");
            let review_brief = tmp.path().join(format!("cross-review-{ordinal}.md"));
            std::fs::write(&review_brief, {
                let mut values = input.prompt_values();
                values.insert("dispatch.brief".to_string(), report_manifest);
                api.compile_prompt(kind.cross_review_spec(), values)?
            })?;
            let (review_description, review_acceptance) = match kind {
                ForumKind::Ask => (
                    "Review only the other participants' promoted extraction reports. This is a fresh report-only dispatch.",
                    "A ? / + / = delta report is promoted without access to this participant's own extraction.",
                ),
                ForumKind::Critique => (
                    "Review only the other participants' promoted critique reports. This is a fresh report-only dispatch.",
                    "A ? / + / = delta report is promoted without access to this participant's own critique.",
                ),
            };
            api.create_task(
                &task,
                format!("Blind cross-review — {}", participant.identity()),
                review_description,
                review_acceptance,
                "other participants' report paths named in dispatch brief",
                "none; dispatch report only",
            )?;
            let dispatch = launch(
                home,
                &task,
                participant,
                &review_brief,
                &source_ref,
                if curator.is_some() {
                    format!(
                        "mm-{}-review-{ordinal}",
                        parent.trim_start_matches("TASK-").to_ascii_lowercase()
                    )
                } else {
                    format!(
                        "mm-{}-r{round_number}-review-{ordinal}",
                        parent.trim_start_matches("TASK-").to_ascii_lowercase()
                    )
                },
                "blind cross-review of other model reports",
            )?;
            active.push(dispatch.clone());
            review_dispatches.push(dispatch);
        }
        if !review_dispatches.is_empty() {
            wait_barrier(home, &review_dispatches, timeout)?;
        }
        for mut dispatch in review_dispatches {
            let path = close_and_finish(home, &api, &ledger, &mut dispatch)?;
            mark_closed(&mut active, &dispatch);
            reviews.push(RunReport {
                participant: dispatch.participant.clone(),
                dispatch,
                path,
            });
        }

        let extraction_tasks = extraction
            .iter()
            .map(|report| report.dispatch.task.clone())
            .collect::<Vec<_>>();
        let review_tasks = reviews
            .iter()
            .map(|report| report.dispatch.task.clone())
            .collect::<Vec<_>>();
        let promoted_report_paths = extraction
            .iter()
            .chain(&reviews)
            .map(|report| report.path.clone())
            .collect::<Vec<_>>();
        let contract_path = (curator.is_none()).then(|| {
            forum_dir(&ledger).join(format!(
                "{parent}-round-{round_number}-curation-contract.md"
            ))
        });
        manifest.rounds.push(ForumRound {
            round: round_number,
            kind,
            fast,
            input: input.clone(),
            panel: participants.iter().map(ManifestParticipant::from).collect(),
            first_stage_tasks: extraction_tasks.clone(),
            cross_review_tasks: review_tasks.clone(),
            promoted_report_paths: promoted_report_paths.clone(),
            started_at: round_started_at.clone(),
            completed_at: chrono::Utc::now().to_rfc3339(),
            contract_path: contract_path.clone(),
        });
        if let Some(contract_path) = &contract_path {
            std::fs::create_dir_all(forum_dir(&ledger))?;
            compile_self_contract(&api, &manifest, contract_path)?;
        }
        write_manifest(&manifest_path, &manifest)?;

        if let Some(contract_path) = contract_path {
            eprintln!("forum_manifest={}", manifest_path.display());
            eprintln!("curation_contract={}", contract_path.display());
            for path in &promoted_report_paths {
                eprintln!("promoted_report={}", path.display());
            }
            return Ok(RunResult::SelfCurated(SelfCuratedRoundResult {
                forum: parent.clone(),
                parent_task: parent.clone(),
                first_stage_tasks: extraction_tasks,
                cross_review_tasks: review_tasks,
                manifest_path: manifest_path.clone(),
                promoted_report_paths,
                contract_path,
            }));
        }

        let curator = curator.as_ref().expect("dispatched curator is present");
        let curator_task = format!("{parent}.{}", next_task_ordinal(&manifest));
        let mut manifest_segments = vec![
            format!(
                "Parent task: {parent}\nStarted UTC: {}\nParticipants ({}):\n{}\nCurator: {}",
                manifest.started_at,
                participants.len(),
                participants
                    .iter()
                    .map(|participant| format!("- {}", participant.identity()))
                    .collect::<Vec<_>>()
                    .join("\n"),
                curator.identity(),
            ),
            extraction
                .iter()
                .map(|report| {
                    manifest_entry(
                        match kind {
                            ForumKind::Ask => "Extraction",
                            ForumKind::Critique => "Critique",
                        },
                        report,
                    )
                })
                .collect::<Vec<_>>()
                .join("\n\n"),
        ];
        if !reviews.is_empty() {
            manifest_segments.push(
                reviews
                    .iter()
                    .map(|report| manifest_entry("Cross-review", report))
                    .collect::<Vec<_>>()
                    .join("\n\n"),
            );
        }
        manifest_segments.push(format!("Curation task: {curator_task}"));
        let run_manifest = manifest_segments.join("\n\n");
        let curator_brief = tmp.path().join("curator.md");
        std::fs::write(&curator_brief, {
            let mut values = input.prompt_values();
            values.insert("dispatch.brief".to_string(), run_manifest);
            values.insert("task.id".to_string(), curator_task.clone());
            let compiled = api.compile_prompt(kind.curator_spec(), values)?;
            if fast {
                strip_cross_review_output_line(&compiled)?
            } else {
                compiled
            }
        })?;
        let (curator_title, curator_description, curator_acceptance) = match kind {
            ForumKind::Ask => (
                "Curate answer",
                if fast {
                    "Read all promoted extraction reports, write the final prose draft and structured diagram fields, and report their paths."
                } else {
                    "Read all promoted extraction and cross-review reports, write the final prose draft and structured diagram fields, and report their paths."
                },
                "The prose draft matches the final-artifact contract, names every raw-report task, and contains only orchestrator placeholders for the run stats, Question, and diagram.",
            ),
            ForumKind::Critique => (
                "Curate verdict",
                if fast {
                    "Read all promoted critique reports, write the final verdict draft and structured diagram fields, and report their paths."
                } else {
                    "Read all promoted critique and cross-review reports, write the final verdict draft and structured diagram fields, and report their paths."
                },
                "The prose draft matches the final-artifact contract, names every raw-report task, and contains only orchestrator placeholders for the run stats, Target, and diagram.",
            ),
        };
        api.create_task(
            &curator_task,
            format!("{curator_title} — {}", curator.identity()),
            curator_description,
            curator_acceptance,
            "all promoted report paths named in dispatch brief and MDX block contract",
            "/tmp curation draft, diagram JSON, and dispatch report only",
        )?;
        let mut curator_dispatch = launch(
            home,
            &curator_task,
            curator,
            &curator_brief,
            &source_ref,
            format!(
                "mm-{}-curate",
                parent.trim_start_matches("TASK-").to_ascii_lowercase()
            ),
            "curate multi-model reports into final artifact",
        )?;
        active.push(curator_dispatch.clone());
        wait_barrier(home, std::slice::from_ref(&curator_dispatch), timeout)?;
        let curator_report_path = close_and_finish(home, &api, &ledger, &mut curator_dispatch)?;
        mark_closed(&mut active, &curator_dispatch);

        let draft_path = PathBuf::from(format!("/tmp/{curator_task}-curation.mdx"));
        let fields_path = PathBuf::from(format!("/tmp/{curator_task}-diagram.json"));
        if !draft_path.is_file() || !fields_path.is_file() {
            bail!(
                "curator outputs missing: draft={} fields={}",
                draft_path.is_file(),
                fields_path.is_file()
            );
        }
        let fields = load_diagram_fields(&fields_path, &extraction_tasks, &review_tasks)?;
        let svg = render_pipeline_svg(
            &input.diagram_prompt(),
            &extraction,
            &reviews,
            curator,
            &curator_task,
            &curator_report_path,
            &fields,
            fast,
        )?;
        let raw_tasks = extraction_tasks
            .iter()
            .chain(&review_tasks)
            .cloned()
            .chain(std::iter::once(curator_task.clone()))
            .collect::<Vec<_>>();
        let draft = std::fs::read_to_string(&draft_path)
            .with_context(|| format!("read {}", draft_path.display()))?;
        let about_run =
            render_about_run(kind, &extraction, &reviews, curator, &manifest.started_at);
        let mdx = assemble_artifact(&draft, &input, &svg, &about_run, &raw_tasks)?;
        let artifact = artifact_id
            .clone()
            .unwrap_or_else(|| mint_node_id_for_class(NodeIdClass::Artifact));
        api.submit_artifact(
            &artifact,
            mdx,
            input.artifact_title(&fields),
            &parent,
            input.content(),
        )?;
        std::fs::remove_file(&draft_path)
            .with_context(|| format!("remove {}", draft_path.display()))?;
        std::fs::remove_file(&fields_path)
            .with_context(|| format!("remove {}", fields_path.display()))?;

        let review_evidence = if review_tasks.is_empty() {
            String::new()
        } else {
            format!("- Cross-review tasks: {}\n", review_tasks.join(" "))
        };
        api.set_evidence(
            &parent,
            &format!(
                "- Artifact: {artifact}\n- {} tasks: {}\n{review_evidence}- Curation task: {curator_task}\n",
                match kind {
                    ForumKind::Ask => "Extraction",
                    ForumKind::Critique => "Critique",
                },
                extraction_tasks.join(" ")
            ),
        )?;
        api.finish_task(&parent)?;
        manifest.state = ForumState::Curated;
        manifest.curation_task = Some(curator_task.clone());
        manifest.submitted_artifact = Some(artifact.clone());
        write_manifest(&manifest_path, &manifest)?;
        Ok(RunResult::Dispatched(DispatchedRunResult {
            parent_task: parent.clone(),
            first_stage_tasks: extraction_tasks,
            cross_review_tasks: review_tasks,
            curation_task: curator_task,
            artifact_id: artifact,
        }))
    })();

    if let Err(error) = &result {
        if error.downcast_ref::<WaitUnknown>().is_none() {
            for dispatch in &mut active {
                best_effort_close(home, dispatch);
            }
        }
    }
    result
}

fn run_ask(home: &Home, args: AskArgs) -> Result<AskResult> {
    let question = match (args.question, args.file) {
        (Some(question), None) => question,
        (None, Some(path)) => {
            std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?
        }
        _ => bail!("one of --question or --file is required"),
    };
    let question = question.trim().to_string();
    validate_question(&question)?;
    let result = run_forum(home, ForumInput::Ask { question }, args.run)?;
    Ok(match result {
        RunResult::Dispatched(result) => AskResult::Dispatched(DispatchedAskResult {
            parent_task: result.parent_task,
            extraction_tasks: result.first_stage_tasks,
            cross_review_tasks: result.cross_review_tasks,
            curation_task: result.curation_task,
            artifact_id: result.artifact_id,
        }),
        RunResult::SelfCurated(result) => AskResult::SelfCurated(SelfCuratedAskResult {
            forum: result.forum,
            parent_task: result.parent_task,
            extraction_tasks: result.first_stage_tasks,
            cross_review_tasks: result.cross_review_tasks,
            manifest_path: result.manifest_path,
            promoted_report_paths: result.promoted_report_paths,
            contract_path: result.contract_path,
        }),
    })
}

fn run_critique(home: &Home, args: CritiqueArgs) -> Result<CritiqueResult> {
    let target = read_target(&args.file)?;
    let focus = match args.focus {
        Some(focus) => {
            let focus = focus.trim().to_string();
            validate_focus(&focus)?;
            Some(focus)
        }
        None => None,
    };
    let basename = args
        .file
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "target".to_string())
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let basename = if basename.is_empty() {
        "target".to_string()
    } else {
        basename
    };
    let result = run_forum(
        home,
        ForumInput::Critique {
            target,
            focus,
            basename,
        },
        args.run,
    )?;
    Ok(match result {
        RunResult::Dispatched(result) => CritiqueResult::Dispatched(DispatchedCritiqueResult {
            parent_task: result.parent_task,
            critique_tasks: result.first_stage_tasks,
            cross_review_tasks: result.cross_review_tasks,
            curation_task: result.curation_task,
            artifact_id: result.artifact_id,
        }),
        RunResult::SelfCurated(result) => CritiqueResult::SelfCurated(SelfCuratedCritiqueResult {
            forum: result.forum,
            parent_task: result.parent_task,
            critique_tasks: result.first_stage_tasks,
            cross_review_tasks: result.cross_review_tasks,
            manifest_path: result.manifest_path,
            promoted_report_paths: result.promoted_report_paths,
            contract_path: result.contract_path,
        }),
    })
}

fn run_curate(home: &Home, args: CurateArgs) -> Result<CurateResult> {
    validate_forum_id(&args.forum)?;
    let curator = parse_participant(&args.identity).context("--identity participant spec")?;
    let ledger = manager::find_project_root()?;
    let project = manager::read_project_id(&ledger)?;
    if args
        .project
        .as_deref()
        .is_some_and(|requested| requested != project)
    {
        bail!(
            "--project {} does not match current orgasmic project {project}",
            args.project.unwrap()
        );
    }
    let manifest_path = forum_manifest_path(&ledger, &args.forum);
    let manifest = manifest_path
        .is_file()
        .then(|| read_manifest(&manifest_path))
        .transpose()?;
    let kind = manifest
        .as_ref()
        .and_then(|manifest| manifest.rounds.first())
        .map(|round| round.kind)
        .unwrap_or(ForumKind::Ask);
    let api = Api::new(home, project.clone(), kind)?;
    let parent_state = manifest
        .as_ref()
        .map(|_| api.task_state(&args.forum))
        .transpose()?;
    validate_join_request(
        &args.forum,
        &project,
        false,
        manifest.as_ref(),
        parent_state.as_deref(),
    )?;
    let mut manifest = manifest.unwrap();
    if manifest.rounds.is_empty() {
        bail!("forum {} has no completed rounds", args.forum);
    }
    if !args.draft.is_file() || !args.diagram.is_file() {
        bail!(
            "curator outputs missing: draft={} diagram={}",
            args.draft.is_file(),
            args.diagram.is_file()
        );
    }

    let fields = load_multi_round_diagram_fields(&args.diagram, &manifest.rounds)?;
    // Reuse ids reserved by an earlier failed attempt so a retry cannot mint a
    // colliding curation task or orphan a second artifact.
    let curator_task = manifest
        .curation_task
        .clone()
        .unwrap_or_else(|| format!("{}.{}", manifest.forum, next_task_ordinal(&manifest)));
    let first = manifest.rounds.first().unwrap();
    // A session curation has no dispatch and no report file, so even a
    // one-round forum renders through the multi-round card, which does not
    // print a curator report path.
    let svg = render_multi_round_svg(
        &manifest.rounds,
        &curator,
        &curator_task,
        &args.draft,
        &fields,
    )?;
    let raw_tasks = manifest
        .rounds
        .iter()
        .flat_map(|round| {
            round
                .first_stage_tasks
                .iter()
                .chain(&round.cross_review_tasks)
        })
        .cloned()
        .collect::<Vec<_>>();
    let draft = std::fs::read_to_string(&args.draft)
        .with_context(|| format!("read {}", args.draft.display()))?;
    let about_run = render_forum_about_run(&manifest, &curator)?;
    let mdx = assemble_artifact(&draft, &first.input, &svg, &about_run, &raw_tasks)?;
    let artifact = manifest
        .artifact_id
        .clone()
        .or_else(|| manifest.submitted_artifact.clone())
        .unwrap_or_else(|| mint_node_id_for_class(NodeIdClass::Artifact));
    let title = fields
        .headline
        .clone()
        .unwrap_or_else(|| first.input.fallback_title());
    // Persist the reservation before the first daemon mutation; a retry after
    // any later failure replays the same task and artifact ids.
    let fresh_curation = manifest.curation_task.is_none();
    manifest.curation_task = Some(curator_task.clone());
    manifest.submitted_artifact = Some(artifact.clone());
    write_manifest(&manifest_path, &manifest)?;
    // The body carries no volatile file paths, so a retried create replays the
    // same request identically; the paths land in evidence below.
    let create_result = api.create_task(
        &curator_task,
        format!("Curate forum — {}", curator.identity()),
        &format!("Invoking session identity: {}", curator.identity()),
        "The session-authored draft and diagram pass the forum's curation gates and one artifact is submitted.",
        "all promoted reports named in the forum manifest",
        "session-authored draft and diagram; orgasmic task evidence and artifact store via CLI only",
    );
    match create_result {
        Ok(()) => {
            api.update_task_state(
                &curator_task,
                "in_progress",
                "invoking session began forum curation",
            )?;
        }
        Err(error) if !fresh_curation => {
            eprintln!("reusing curation task {curator_task} from an earlier attempt: {error:#}");
        }
        Err(error) => return Err(error),
    }
    api.submit_artifact(
        &artifact,
        mdx,
        title,
        &manifest.forum,
        first.input.content(),
    )?;

    api.set_evidence(
        &curator_task,
        &format!(
            "- Session curator: {}\n- Draft: {}\n- Diagram JSON: {}\n- Artifact: {artifact}\n",
            curator.identity(),
            args.draft.display(),
            args.diagram.display()
        ),
    )?;
    api.finish_task(&curator_task)?;
    api.set_evidence(
        &manifest.forum,
        &format!(
            "- Artifact: {artifact}\n- Forum rounds: {}\n- Raw report tasks: {}\n- Curation task: {curator_task}\n- Curator: {}\n",
            manifest.rounds.len(),
            raw_tasks.join(" "),
            curator.identity()
        ),
    )?;
    api.finish_task(&manifest.forum)?;
    manifest.state = ForumState::Curated;
    manifest.curation_task = Some(curator_task.clone());
    manifest.submitted_artifact = Some(artifact.clone());
    write_manifest(&manifest_path, &manifest)?;
    Ok(CurateResult {
        forum: manifest.forum.clone(),
        parent_task: manifest.forum,
        curation_task: curator_task,
        artifact_id: artifact,
        manifest_path,
    })
}

#[derive(Args, Debug)]
pub struct ForumArgs {
    #[command(subcommand)]
    mode: ForumMode,
}

#[derive(Subcommand, Debug)]
enum ForumMode {
    /// Ask a hard question through independent extraction, blind cross-review, and curation.
    Ask(AskArgs),
    /// Critique a target through independent analysis, blind cross-review, and curation.
    Critique(CritiqueArgs),
    /// Validate and submit an invoking session's self-curated forum artifact.
    Curate(CurateArgs),
}

pub fn run(home: &Home, args: ForumArgs) -> Result<()> {
    match args.mode {
        ForumMode::Ask(args) => {
            println!("{}", serde_json::to_string_pretty(&run_ask(home, args)?)?);
        }
        ForumMode::Critique(args) => println!(
            "{}",
            serde_json::to_string_pretty(&run_critique(home, args)?)?
        ),
        ForumMode::Curate(args) => println!(
            "{}",
            serde_json::to_string_pretty(&run_curate(home, args)?)?
        ),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reports(count: usize) -> (Vec<Participant>, Vec<RunReport>, Vec<RunReport>) {
        let participants = [
            parse_participant("stdio,codex,gpt-5.6-luna,low").unwrap(),
            parse_participant("stdio,hermes,google/gemini-3.7-flash,medium").unwrap(),
            parse_participant("stdio,claude,claude-haiku-4-5-20251001,low").unwrap(),
        ][..count]
            .to_vec();
        let extraction = participants
            .iter()
            .enumerate()
            .map(|(index, participant)| {
                let ordinal = index + 1;
                let task = format!("TASK-TESTX.{ordinal}");
                RunReport {
                    participant: participant.clone(),
                    dispatch: Dispatch {
                        task: task.clone(),
                        started_tx: format!("tx-extract-{ordinal}"),
                        participant: participant.clone(),
                        closed: false,
                    },
                    path: PathBuf::from(format!(
                        "/ledger/.orgasmic/tasks/{task}/dispatches/tx/report.md"
                    )),
                }
            })
            .collect();
        let reviews = participants
            .iter()
            .enumerate()
            .map(|(index, participant)| {
                let ordinal = index + 1;
                let task = format!("TASK-TESTX.{}", count + ordinal);
                RunReport {
                    participant: participant.clone(),
                    dispatch: Dispatch {
                        task: task.clone(),
                        started_tx: format!("tx-review-{ordinal}"),
                        participant: participant.clone(),
                        closed: false,
                    },
                    path: PathBuf::from(format!(
                        "/ledger/.orgasmic/tasks/{task}/dispatches/tx/report.md"
                    )),
                }
            })
            .collect();
        (participants, extraction, reviews)
    }

    fn fields(extraction: &[RunReport], reviews: &[RunReport]) -> DiagramFields {
        DiagramFields {
            extracts: extraction
                .iter()
                .map(|report| {
                    (
                        report.dispatch.task.clone(),
                        vec!["e".repeat(43), "Second short finding".to_string()],
                    )
                })
                .collect(),
            reviews: reviews
                .iter()
                .map(|report| {
                    (
                        report.dispatch.task.clone(),
                        vec![
                            DeltaBullet {
                                tag: "?".to_string(),
                                text: "r".repeat(43),
                            },
                            DeltaBullet {
                                tag: "+".to_string(),
                                text: "new evidence".to_string(),
                            },
                            DeltaBullet {
                                tag: "=".to_string(),
                                text: "shared conclusion".to_string(),
                            },
                        ],
                    )
                })
                .collect(),
            curator_summary: "reports deduplicated; disagreements remain explicit".to_string(),
            headline: None,
        }
    }

    fn manifest_round(round: usize, kind: ForumKind, first_ordinal: usize) -> ForumRound {
        let panel = [
            parse_participant("stdio,codex,gpt-5.6-luna,low").unwrap(),
            parse_participant("stdio,hermes,google/gemini-3.7-flash,medium").unwrap(),
        ];
        let input = match kind {
            ForumKind::Ask => ForumInput::Ask {
                question: format!("Question for round {round}?"),
            },
            ForumKind::Critique => ForumInput::Critique {
                target: format!("# Target for round {round}"),
                focus: Some("durability".to_string()),
                basename: "design.md".to_string(),
            },
        };
        let first_stage_tasks = (first_ordinal..first_ordinal + 2)
            .map(|ordinal| format!("TASK-TESTX.{ordinal}"))
            .collect::<Vec<_>>();
        let cross_review_tasks = (first_ordinal + 2..first_ordinal + 4)
            .map(|ordinal| format!("TASK-TESTX.{ordinal}"))
            .collect::<Vec<_>>();
        let promoted_report_paths = first_stage_tasks
            .iter()
            .chain(&cross_review_tasks)
            .map(|task| PathBuf::from(format!("/ledger/.orgasmic/tasks/{task}/report.md")))
            .collect();
        ForumRound {
            round,
            kind,
            fast: false,
            input,
            panel: panel.iter().map(ManifestParticipant::from).collect(),
            first_stage_tasks,
            cross_review_tasks,
            promoted_report_paths,
            started_at: format!("2026-08-30T0{round}:00:00+00:00"),
            completed_at: format!("2026-08-30T0{round}:10:00+00:00"),
            contract_path: Some(PathBuf::from(format!(
                "/ledger/.orgasmic/tmp/forum/TASK-TESTX-round-{round}-contract.md"
            ))),
        }
    }

    fn mixed_manifest() -> ForumManifest {
        ForumManifest {
            version: 1,
            forum: "TASK-TESTX".to_string(),
            project: "orgasmic".to_string(),
            source_ref: "deadbeef".to_string(),
            started_at: "2026-08-30T01:00:00+00:00".to_string(),
            artifact_id: None,
            curation_mode: CurationMode::SelfCurated,
            state: ForumState::Open,
            rounds: vec![
                manifest_round(1, ForumKind::Ask, 1),
                manifest_round(2, ForumKind::Critique, 5),
            ],
            curation_task: None,
            submitted_artifact: None,
        }
    }

    fn fast_round(round: usize, kind: ForumKind, first_ordinal: usize) -> ForumRound {
        let mut round = manifest_round(round, kind, first_ordinal);
        round.fast = true;
        round.panel.truncate(1);
        round.first_stage_tasks.truncate(1);
        round.cross_review_tasks.clear();
        round.promoted_report_paths.truncate(1);
        round
    }

    fn mixed_fast_manifest() -> ForumManifest {
        let mut manifest = mixed_manifest();
        manifest.rounds = vec![
            fast_round(1, ForumKind::Ask, 1),
            manifest_round(2, ForumKind::Critique, 2),
        ];
        manifest
    }

    fn multi_diagram_json(manifest: &ForumManifest) -> Value {
        let rounds = manifest
            .rounds
            .iter()
            .map(|round| {
                serde_json::json!({
                    "round": round.round,
                    "kind": round.kind.slug(),
                    "extracts": round.first_stage_tasks.iter().map(|task| serde_json::json!({
                        "task": task,
                        "excerpt_lines": [format!("finding from {task}")]
                    })).collect::<Vec<_>>(),
                    "reviews": round.cross_review_tasks.iter().map(|task| serde_json::json!({
                        "task": task,
                        "delta_bullets": [
                            {"tag": "?", "text": format!("challenge from {task}")},
                            {"tag": "+", "text": format!("addition from {task}")},
                            {"tag": "=", "text": format!("agreement from {task}")}
                        ]
                    })).collect::<Vec<_>>()
                })
            })
            .collect::<Vec<_>>();
        serde_json::json!({
            "rounds": rounds,
            "curator_summary": "two rounds synthesized",
            "headline": "Durable forum synthesis"
        })
    }

    #[test]
    fn participant_and_question_validation_match_the_script() {
        let codex = parse_participant("stdio,codex,gpt-5.6-luna,low").unwrap();
        let hermes = parse_participant("stdio,hermes,google/gemini-3.7-flash,medium").unwrap();
        assert_eq!(
            codex.identity(),
            "codex · openai · gpt-5.6-luna · effort low"
        );
        assert_eq!(
            (hermes.vendor.as_str(), hermes.model.as_str()),
            ("google", "gemini-3.7-flash")
        );
        let panel = [codex.clone(), hermes.clone()];
        assert_eq!(resolve_curator(&panel, "2").unwrap(), hermes);
        assert_eq!(
            resolve_curator(&panel, "stdio,claude,claude-fable-5,high")
                .unwrap()
                .identity(),
            "claude · anthropic · claude-fable-5 · effort high"
        );
        for rejected in ["0", "3", "not-a-number", ""] {
            assert!(resolve_curator(&panel, rejected).is_err());
        }
        assert!(resolve_curator(&panel, "stdio,claude").is_err());
        assert!(validate_participants(&[codex.clone(), codex], false).is_err());
        for rejected in [
            format!("contains {QUESTION_PLACEHOLDER}"),
            format!("contains {DIAGRAM_PLACEHOLDER}"),
            "-leading option-shaped question".to_string(),
        ] {
            assert!(
                validate_question(&rejected).is_err(),
                "question must be rejected up front: {rejected}"
            );
        }
    }

    #[test]
    fn panel_of_one_requires_fast_for_ask_and_critique_rounds() {
        let participant = parse_participant("stdio,codex,gpt-5.6-luna,low").unwrap();
        for kind in [ForumKind::Ask, ForumKind::Critique] {
            let error = validate_participants(std::slice::from_ref(&participant), false)
                .unwrap_err()
                .to_string();
            assert_eq!(
                error,
                "at least two participants are required",
                "normal {} round",
                kind.slug()
            );
            validate_participants(std::slice::from_ref(&participant), true)
                .unwrap_or_else(|error| panic!("fast {} round: {error:#}", kind.slug()));
        }
        assert_eq!(
            validate_participants(&[], true).unwrap_err().to_string(),
            "at least one participant is required with --fast"
        );
    }

    #[test]
    fn manifest_round_trip_preserves_mixed_rounds() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("forum/TASK-TESTX.json");
        let manifest = mixed_manifest();
        write_manifest(&path, &manifest).unwrap();
        assert_eq!(read_manifest(&path).unwrap(), manifest);
        assert_eq!(next_task_ordinal(&manifest), 9);
    }

    #[test]
    fn fast_manifest_round_trip_accepts_stage_one_only_and_rejects_reviews() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("forum/TASK-TESTX.json");
        let manifest = mixed_fast_manifest();
        write_manifest(&path, &manifest).unwrap();
        assert_eq!(read_manifest(&path).unwrap(), manifest);
        assert_eq!(manifest.rounds[0].panel.len(), 1);
        assert!(manifest.rounds[0].cross_review_tasks.is_empty());
        assert_eq!(manifest.rounds[0].promoted_report_paths.len(), 1);
        assert_eq!(next_task_ordinal(&manifest), 6);

        let mut invented_review = manifest.clone();
        invented_review.rounds[0]
            .cross_review_tasks
            .push("TASK-TESTX.6".to_string());
        assert!(validate_manifest(&invented_review)
            .unwrap_err()
            .to_string()
            .contains("mismatched panel, task, or report counts"));

        let mut malformed_normal = manifest_round(1, ForumKind::Ask, 1);
        malformed_normal.cross_review_tasks.clear();
        malformed_normal.promoted_report_paths.truncate(2);
        let mut normal_manifest = manifest;
        normal_manifest.rounds = vec![malformed_normal];
        assert!(validate_manifest(&normal_manifest).is_err());
    }

    #[test]
    fn manifest_validation_rejects_foreign_tasks_and_unvalidated_input() {
        let mut foreign = mixed_manifest();
        foreign.rounds[1].cross_review_tasks[1] = "TASK-OTHER.8".to_string();
        assert!(validate_manifest(&foreign)
            .unwrap_err()
            .to_string()
            .contains("does not belong to TASK-TESTX"));

        let mut junk = mixed_manifest();
        junk.rounds[0].first_stage_tasks[0] = "not-a-task-id".to_string();
        assert!(validate_manifest(&junk)
            .unwrap_err()
            .to_string()
            .contains("does not belong to TASK-TESTX"));

        let mut smuggled = mixed_manifest();
        smuggled.rounds[0].input = ForumInput::Ask {
            question: format!("hide {DIAGRAM_PLACEHOLDER} here"),
        };
        assert!(validate_manifest(&smuggled)
            .unwrap_err()
            .to_string()
            .contains("round 1 question"));

        let mut oversized = mixed_manifest();
        oversized.rounds[1].input = ForumInput::Critique {
            target: "x".repeat(MAX_TARGET_BYTES + 1),
            focus: None,
            basename: "design.md".to_string(),
        };
        assert!(validate_manifest(&oversized)
            .unwrap_err()
            .to_string()
            .contains("round 2 target"));
    }

    #[test]
    fn forum_join_refusal_matrix_is_explicit() {
        let mut manifest = mixed_manifest();
        assert!(
            validate_join_request("TASK-TESTX", "orgasmic", false, None, None)
                .unwrap_err()
                .to_string()
                .contains("unknown forum")
        );
        assert!(validate_join_request(
            "TASK-TESTX",
            "orgasmic",
            true,
            Some(&manifest),
            Some("in_progress")
        )
        .unwrap_err()
        .to_string()
        .contains("cannot be combined"));
        manifest.curation_mode = CurationMode::Dispatched;
        assert!(validate_join_request(
            "TASK-TESTX",
            "orgasmic",
            false,
            Some(&manifest),
            Some("in_progress")
        )
        .unwrap_err()
        .to_string()
        .contains("dispatched curator"));
        manifest.curation_mode = CurationMode::SelfCurated;
        manifest.state = ForumState::Curated;
        assert!(validate_join_request(
            "TASK-TESTX",
            "orgasmic",
            false,
            Some(&manifest),
            Some("done")
        )
        .unwrap_err()
        .to_string()
        .contains("already curated"));
        manifest.state = ForumState::Open;
        manifest.curation_task = Some("TASK-TESTX.9".to_string());
        assert!(validate_join_request(
            "TASK-TESTX",
            "orgasmic",
            false,
            Some(&manifest),
            Some("in_progress")
        )
        .unwrap_err()
        .to_string()
        .contains("already reserved its curation task"));
    }

    #[test]
    fn compiled_self_contract_drops_only_completion() {
        let compiled = "# Role\nKeep.\n\n# Completion\nDo not keep.\n\n# Security\nKeep secure.\n";
        let contract = without_completion_section(compiled).unwrap();
        assert!(contract.contains("# Role\nKeep."));
        assert!(!contract.contains("Do not keep"));
        assert!(contract.contains("# Security\nKeep secure."));
    }

    #[test]
    fn critique_target_and_focus_validation_rejects_unsafe_inputs() {
        assert!(validate_target("")
            .unwrap_err()
            .to_string()
            .contains("empty"));
        assert!(validate_target(" \n\t")
            .unwrap_err()
            .to_string()
            .contains("empty"));
        assert!(validate_target(&"x".repeat(MAX_TARGET_BYTES + 1))
            .unwrap_err()
            .to_string()
            .contains("exceeds 64 KiB"));
        for placeholder in [
            QUESTION_PLACEHOLDER,
            TARGET_PLACEHOLDER,
            DIAGRAM_PLACEHOLDER,
            RUN_STATS_PLACEHOLDER,
        ] {
            assert!(validate_target(&format!("hostile {placeholder}"))
                .unwrap_err()
                .to_string()
                .contains("orchestrator placeholders"));
        }
        for focus in ["", "two\nlines", "-option", TARGET_PLACEHOLDER] {
            assert!(
                validate_focus(focus).is_err(),
                "focus should fail: {focus:?}"
            );
        }
        assert!(validate_target("a valid target").is_ok());
        assert!(validate_target(&"x".repeat(MAX_TARGET_BYTES)).is_ok());
        assert!(validate_focus("security posture").is_ok());

        let tmp = tempfile::tempdir().unwrap();
        let invalid_utf8 = tmp.path().join("invalid.md");
        std::fs::write(&invalid_utf8, [0xff]).unwrap();
        assert!(read_target(&invalid_utf8)
            .unwrap_err()
            .to_string()
            .contains("as UTF-8"));
    }

    #[test]
    fn diagram_fields_clip_caps_and_reject_model_svg() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("diagram.json");
        let over = "x".repeat(44);
        std::fs::write(
            &path,
            serde_json::json!({
                "extracts": [{"task": "TASK-CAP.1", "excerpt_lines": [over]}],
                "reviews": [{"task": "TASK-CAP.2", "delta_bullets": [
                    {"tag": "?", "text": "x".repeat(44)},
                    {"tag": "+", "text": "x".repeat(44)},
                    {"tag": "=", "text": "x".repeat(44)}
                ]}],
                "curator_summary": "summary",
                "headline": format!(" {} ", "h".repeat(90))
            })
            .to_string(),
        )
        .unwrap();
        let fields = load_diagram_fields(
            &path,
            &["TASK-CAP.1".to_string()],
            &["TASK-CAP.2".to_string()],
        )
        .unwrap();
        let expected = format!("{}…", "x".repeat(42));
        assert_eq!(
            fields.extracts["TASK-CAP.1"],
            std::slice::from_ref(&expected)
        );
        assert_eq!(fields.reviews["TASK-CAP.2"][0].text, expected);
        assert_eq!(fields.headline, Some(format!("{}…", "h".repeat(79))));

        std::fs::write(
            &path,
            r#"{"extracts":[],"reviews":[],"curator_summary":"<svg/>"}"#,
        )
        .unwrap();
        assert!(load_diagram_fields(&path, &[], &[])
            .unwrap_err()
            .to_string()
            .contains("model-authored SVG"));
    }

    #[test]
    fn multi_round_diagram_covers_each_round_task_once() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("diagram.json");
        let manifest = mixed_manifest();
        let data = multi_diagram_json(&manifest);
        std::fs::write(&path, data.to_string()).unwrap();
        let fields = load_multi_round_diagram_fields(&path, &manifest.rounds).unwrap();
        assert_eq!(fields.rounds.len(), 2);
        assert_eq!(fields.rounds[0].kind, ForumKind::Ask);
        assert_eq!(fields.rounds[1].kind, ForumKind::Critique);

        let mut missing = data.clone();
        missing["rounds"][1]["reviews"]
            .as_array_mut()
            .unwrap()
            .pop();
        std::fs::write(&path, missing.to_string()).unwrap();
        assert!(load_multi_round_diagram_fields(&path, &manifest.rounds)
            .unwrap_err()
            .to_string()
            .contains("cover every review task once"));

        let mut duplicated = data;
        duplicated["rounds"][1]["round"] = serde_json::json!(1);
        std::fs::write(&path, duplicated.to_string()).unwrap();
        assert!(load_multi_round_diagram_fields(&path, &manifest.rounds)
            .unwrap_err()
            .to_string()
            .contains("unique positive round numbers"));
    }

    #[test]
    fn fast_diagram_omits_reviews_and_rejects_invented_review_provenance() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("diagram.json");
        let mut manifest = mixed_fast_manifest();
        manifest.rounds.truncate(1);
        let mut data = multi_diagram_json(&manifest);
        data["rounds"][0].as_object_mut().unwrap().remove("reviews");
        std::fs::write(&path, data.to_string()).unwrap();
        load_multi_round_diagram_fields(&path, &manifest.rounds).unwrap();

        data["rounds"][0]["reviews"] = serde_json::json!([{
            "task": "TASK-TESTX.2",
            "delta_bullets": [
                {"tag": "?", "text": "invented"},
                {"tag": "+", "text": "invented"},
                {"tag": "=", "text": "invented"}
            ]
        }]);
        std::fs::write(&path, data.to_string()).unwrap();
        assert!(load_multi_round_diagram_fields(&path, &manifest.rounds)
            .unwrap_err()
            .to_string()
            .contains("invalid review diagram entry"));
    }

    #[test]
    fn multi_round_renderer_has_one_curator_and_all_review_arrows() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("diagram.json");
        let manifest = mixed_manifest();
        std::fs::write(&path, multi_diagram_json(&manifest).to_string()).unwrap();
        let fields = load_multi_round_diagram_fields(&path, &manifest.rounds).unwrap();
        let curator = parse_participant("session,claude,claude-fable-5,interactive").unwrap();
        let svg = render_multi_round_svg(
            &manifest.rounds,
            &curator,
            "TASK-TESTX.9",
            Path::new("/tmp/TASK-TESTX-curation.mdx"),
            &fields,
        )
        .unwrap();
        assert!(!svg.contains("<style"));
        assert_eq!(svg.matches("data-card=\"prompt\"").count(), 2);
        assert_eq!(svg.matches("data-card=\"extract\"").count(), 4);
        assert_eq!(svg.matches("data-card=\"review\"").count(), 4);
        assert_eq!(svg.matches("data-card=\"curator\"").count(), 1);
        assert_eq!(svg.matches("data-arrow=\"review-curator\"").count(), 4);
        for task in manifest
            .rounds
            .iter()
            .flat_map(|round| &round.cross_review_tasks)
        {
            assert!(svg.contains(&format!("data-task=\"{task}\"")));
        }
    }

    #[test]
    fn single_normal_round_renders_through_the_multi_round_card() {
        // Latent-defect regression: before TASK-82KKQ this bailed on
        // rounds.len() < 2, which broke `forum curate` for every single-round
        // self-curated forum shipped in TASK-9TGQS.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("diagram.json");
        let mut manifest = mixed_manifest();
        manifest.rounds.truncate(1);
        std::fs::write(&path, multi_diagram_json(&manifest).to_string()).unwrap();
        let fields = load_multi_round_diagram_fields(&path, &manifest.rounds).unwrap();
        let curator = parse_participant("session,claude,claude-fable-5,interactive").unwrap();
        let svg = render_multi_round_svg(
            &manifest.rounds,
            &curator,
            "TASK-TESTX.5",
            Path::new("/tmp/TASK-TESTX-curation.mdx"),
            &fields,
        )
        .unwrap();
        assert_eq!(svg.matches("data-card=\"prompt\"").count(), 1);
        assert_eq!(svg.matches("data-card=\"extract\"").count(), 2);
        assert_eq!(svg.matches("data-card=\"review\"").count(), 2);
        assert_eq!(svg.matches("data-card=\"curator\"").count(), 1);
        assert_eq!(svg.matches("data-arrow=\"review-curator\"").count(), 2);
    }

    #[test]
    fn cross_review_contract_surgery_fails_closed_on_spec_drift() {
        let compiled = "# Output Contract\n- Draft MDX\n- Cross-review tasks\n- Curation task\n";
        let stripped = strip_cross_review_output_line(compiled).unwrap();
        assert!(!stripped.contains("Cross-review tasks"));
        assert!(stripped.contains("- Curation task"));
        assert!(
            strip_cross_review_output_line("# Output Contract\n- Draft MDX\n")
                .unwrap_err()
                .to_string()
                .contains("no longer carries")
        );
    }

    #[test]
    fn mixed_fast_and_normal_renderer_routes_each_row_to_the_curator() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("diagram.json");
        let manifest = mixed_fast_manifest();
        std::fs::write(&path, multi_diagram_json(&manifest).to_string()).unwrap();
        let fields = load_multi_round_diagram_fields(&path, &manifest.rounds).unwrap();
        let curator = parse_participant("session,claude,claude-fable-5,interactive").unwrap();
        let svg = render_multi_round_svg(
            &manifest.rounds,
            &curator,
            "TASK-TESTX.6",
            Path::new("/tmp/TASK-TESTX-curation.mdx"),
            &fields,
        )
        .unwrap();
        assert_eq!(svg.matches("data-card=\"prompt\"").count(), 2);
        assert_eq!(svg.matches("data-card=\"extract\"").count(), 3);
        assert_eq!(svg.matches("data-card=\"review\"").count(), 2);
        assert_eq!(svg.matches("data-card=\"curator\"").count(), 1);
        assert_eq!(svg.matches("data-arrow=\"extract-curator\"").count(), 1);
        assert_eq!(svg.matches("data-arrow=\"review-curator\"").count(), 2);
        assert!(!svg.contains("data-card=\"review\" data-round=\"1\""));
    }

    #[test]
    fn multi_round_curate_uses_the_existing_assembly_gates() {
        let mut manifest = mixed_manifest();
        manifest.rounds[0].input = ForumInput::Ask {
            question: "How should Vec<Section> and {braces} render?".to_string(),
        };
        let curator = parse_participant("session,claude,claude-fable-5,interactive").unwrap();
        let about = render_forum_about_run(&manifest, &curator).unwrap();
        assert!(about.contains("Round 1 · ask"));
        assert!(about.contains("Round 2 · critique"));
        // Round prompts are operator text; the footer must escape them like
        // the verbatim first section does.
        assert!(about.contains("Vec&lt;Section&gt; and &#123;braces&#125;"));
        assert!(!about.contains("Vec<Section>"));
        let raw_tasks = manifest
            .rounds
            .iter()
            .flat_map(|round| {
                round
                    .first_stage_tasks
                    .iter()
                    .chain(&round.cross_review_tasks)
            })
            .cloned()
            .collect::<Vec<_>>();
        let draft = format!(
            "{QUESTION_PLACEHOLDER}\n<Section title=\"Final answer\"><RichText>Answer.</RichText></Section>\n<Section title=\"From question to answer\">\n{DIAGRAM_PLACEHOLDER}\n<RichText>Raw reports: {}</RichText>\n</Section>\n<Section title=\"Knowledge map\"><RichText>Map.</RichText></Section>\n{RUN_STATS_PLACEHOLDER}",
            raw_tasks.join(" ")
        );
        let input = &manifest.rounds[0].input;
        assemble_artifact(&draft, input, "generated", &about, &raw_tasks).unwrap();

        let decoy = draft.replace(
            QUESTION_PLACEHOLDER,
            &format!(
                "<Section  title=\"Question\"><RichText>decoy</RichText></Section>\n{QUESTION_PLACEHOLDER}"
            ),
        );
        assert!(
            assemble_artifact(&decoy, input, "generated", &about, &raw_tasks)
                .unwrap_err()
                .to_string()
                .contains("does not match")
        );

        let missing = draft.replace("TASK-TESTX.8", "TASK-OTHER");
        assert!(
            assemble_artifact(&missing, input, "generated", &about, &raw_tasks)
                .unwrap_err()
                .to_string()
                .contains("omitted raw-report task ids")
        );

        let trailing = format!("{draft}\n<Section><RichText>late</RichText></Section>");
        assert!(
            assemble_artifact(&trailing, input, "generated", &about, &raw_tasks)
                .unwrap_err()
                .to_string()
                .contains("final block")
        );
    }

    #[test]
    fn fast_only_curate_uses_stage_one_reports_and_existing_assembly_gates() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("diagram.json");
        let mut manifest = mixed_fast_manifest();
        manifest.rounds.truncate(1);
        let mut diagram = multi_diagram_json(&manifest);
        diagram["rounds"][0]
            .as_object_mut()
            .unwrap()
            .remove("reviews");
        std::fs::write(&path, diagram.to_string()).unwrap();
        let fields = load_multi_round_diagram_fields(&path, &manifest.rounds).unwrap();
        let curator = parse_participant("session,claude,claude-fable-5,interactive").unwrap();
        let svg = render_multi_round_svg(
            &manifest.rounds,
            &curator,
            "TASK-TESTX.2",
            Path::new("/tmp/TASK-TESTX-curation.mdx"),
            &fields,
        )
        .unwrap();
        assert_eq!(svg.matches("data-card=\"extract\"").count(), 1);
        assert_eq!(svg.matches("data-card=\"review\"").count(), 0);
        assert_eq!(svg.matches("data-arrow=\"extract-curator\"").count(), 1);

        let raw_tasks = manifest.rounds[0].first_stage_tasks.clone();
        let draft = format!(
            "{QUESTION_PLACEHOLDER}\n<Section title=\"Final answer\"><RichText>Answer.</RichText></Section>\n<Section title=\"From question to answer\">\n{DIAGRAM_PLACEHOLDER}\n<RichText>Raw reports: {}</RichText>\n</Section>\n<Section title=\"Knowledge map\"><RichText>Map.</RichText></Section>\n{RUN_STATS_PLACEHOLDER}",
            raw_tasks.join(" ")
        );
        let about = render_forum_about_run(&manifest, &curator).unwrap();
        assemble_artifact(&draft, &manifest.rounds[0].input, &svg, &about, &raw_tasks).unwrap();
        assert!(assemble_artifact(
            &draft.replace(&raw_tasks[0], "TASK-OTHER"),
            &manifest.rounds[0].input,
            &svg,
            &about,
            &raw_tasks,
        )
        .unwrap_err()
        .to_string()
        .contains("omitted raw-report task ids"));
    }

    #[test]
    fn renderer_structure_scales_for_two_and_three_participants() {
        for count in [2, 3] {
            let (participants, extraction, reviews) = reports(count);
            let fields = fields(&extraction, &reviews);
            let curator_task = format!("TASK-TESTX.{}", 2 * count + 1);
            let svg = render_pipeline_svg(
                "When should append-only events be authoritative?",
                &extraction,
                &reviews,
                &participants[0],
                &curator_task,
                &PathBuf::from(format!(
                    "/ledger/.orgasmic/tasks/{curator_task}/dispatches/tx/report.md"
                )),
                &fields,
                false,
            )
            .unwrap();
            assert!(!svg.contains("<style"));
            assert_eq!(svg.matches("<g data-card=").count(), 2 * count + 2);
            assert_eq!(svg.matches("<g data-pill=").count(), 4);
            assert_eq!(svg.matches("<text ").count(), 12 + 18 * count);
            assert!(svg.contains(&format!(
                "width=\"{}\" height=\"1000\" viewBox=\"0 0 {} 1000\"",
                64 + count * 252 + (count - 1) * 30,
                64 + count * 252 + (count - 1) * 30
            )));
            assert!(svg.contains(&"e".repeat(43)) && svg.contains(&"r".repeat(43)));
            for participant in &participants {
                assert!(svg.contains(vendor_color(&participant.vendor)));
            }
            for label in [
                "1 · EXTRACT — PARALLEL · ISOLATED",
                "2 · CROSS-REVIEW — BLIND · NEVER SELF",
                "3 · CURATE",
                "FINAL ANSWER",
            ] {
                assert!(svg.contains(label));
            }
            for glyph in ["?", "+", "="] {
                assert_eq!(
                    svg.matches(&format!("data-delta=\"{glyph}\"")).count(),
                    count
                );
            }
            for tag in svg
                .split("<text ")
                .skip(1)
                .map(|tail| tail.split('>').next().unwrap())
            {
                assert!(tag.contains("style=\""));
                for forbidden in [
                    " font-family=",
                    " font-size=",
                    " font-weight=",
                    " fill=",
                    " text-anchor=",
                    " letter-spacing=",
                ] {
                    assert!(!tag.contains(forbidden));
                }
            }
        }
    }

    #[test]
    fn fast_dispatched_renderer_accepts_one_report_and_about_is_honest() {
        let (participants, extraction, _) = reports(1);
        let fields = fields(&extraction, &[]);
        assert!(render_pipeline_svg(
            "What is the cheapest useful critique?",
            &extraction,
            &[],
            &participants[0],
            "TASK-TESTX.2",
            Path::new("/ledger/.orgasmic/tasks/TASK-TESTX.2/dispatches/tx/report.md"),
            &fields,
            false,
        )
        .unwrap_err()
        .to_string()
        .contains("matching extraction and review rosters"));
        let svg = render_pipeline_svg(
            "What is the cheapest useful critique?",
            &extraction,
            &[],
            &participants[0],
            "TASK-TESTX.2",
            Path::new("/ledger/.orgasmic/tasks/TASK-TESTX.2/dispatches/tx/report.md"),
            &fields,
            true,
        )
        .unwrap();
        assert_eq!(svg.matches("data-card=\"extract\"").count(), 1);
        assert_eq!(svg.matches("data-card=\"review\"").count(), 0);
        assert_eq!(svg.matches("data-arrow=\"extract-curator\"").count(), 1);
        assert!(svg.contains("2 · CURATE"));
        assert!(!svg.contains("CROSS-REVIEW"));

        let about = render_about_run(
            ForumKind::Critique,
            &extraction,
            &[],
            &participants[0],
            "2026-08-30T08:00:00+00:00",
        );
        assert!(about.contains("1 critique reports · 0 cross-reviews"));
    }

    #[test]
    fn renderer_matches_stored_python_fixture() {
        let (participants, extraction, reviews) = reports(2);
        let svg = render_pipeline_svg(
            "When should append-only events be authoritative?",
            &extraction,
            &reviews,
            &participants[0],
            "TASK-TESTX.5",
            Path::new("/ledger/.orgasmic/tasks/TASK-TESTX.5/dispatches/tx/report.md"),
            &fields(&extraction, &reviews),
            false,
        )
        .unwrap();
        assert_eq!(
            svg,
            include_str!("../tests/fixtures/TASK-FBSZ2-pipeline.svg")
        );
    }

    #[test]
    fn base64_matches_known_vectors() {
        for (raw, encoded) in [
            (&b""[..], ""),
            (&b"f"[..], "Zg=="),
            (&b"fo"[..], "Zm8="),
            (&b"foo"[..], "Zm9v"),
        ] {
            assert_eq!(base64_encode(raw), encoded);
        }
    }

    #[test]
    fn assembly_preserves_hostile_question_and_enforces_task_boundaries() {
        let (participants, extraction, reviews) = reports(2);
        let about_run = render_about_run(
            ForumKind::Ask,
            &extraction,
            &reviews,
            &participants[0],
            "2026-08-29T21:07:20.123+00:00",
        );
        assert!(about_run.starts_with("<Section title=\"About this run\">"));
        assert!(about_run.contains("- **Curator:**"));
        assert!(about_run.contains("started 2026-08-29 21:07 UTC"));
        let raw_tasks = extraction
            .iter()
            .chain(&reviews)
            .map(|report| report.dispatch.task.clone())
            .chain(std::iter::once("TASK-TESTX.5".to_string()))
            .collect::<Vec<_>>();
        let draft = format!(
            "{QUESTION_PLACEHOLDER}\n<Section title=\"Final answer\"><RichText>Answer.</RichText></Section>\n<Section title=\"From question to answer\">\n{DIAGRAM_PLACEHOLDER}\n<RichText>Raw reports: {}</RichText>\n</Section>\n<Section title=\"Knowledge map\"><RichText>Map.</RichText></Section>\n<Section><RichText>Feedback.</RichText></Section>\n{RUN_STATS_PLACEHOLDER}",
            raw_tasks.join(" ")
        );
        let question = "Should <svg> and {braces} stay verbatim & safe?";
        let input = ForumInput::Ask {
            question: question.to_string(),
        };
        let assembled =
            assemble_artifact(&draft, &input, "<generated/>", &about_run, &raw_tasks).unwrap();
        assert!(
            assembled.find("title=\"Question\"").unwrap()
                < assembled.find("title=\"Final answer\"").unwrap()
        );
        assert_eq!(assembled.matches("data:image/svg+xml;base64,").count(), 1);
        assert!(assembled.trim_end().ends_with("</Section>"));
        assert!(assembled.contains("- **Curator:**"));
        assert!(assembled
            .contains("Should &lt;svg&gt; and &#123;braces&#125; stay verbatim &amp; safe?"));

        let cross_mode = draft.replace(
            "<RichText>Answer.</RichText>",
            &format!("<RichText>Answer. {TARGET_PLACEHOLDER}</RichText>"),
        );
        assert!(
            assemble_artifact(&cross_mode, &input, "generated", &about_run, &raw_tasks)
                .unwrap_err()
                .to_string()
                .contains("each orchestrator placeholder once")
        );

        let authored = draft.replace(DIAGRAM_PLACEHOLDER, "<svg/>");
        assert!(
            assemble_artifact(&authored, &input, "generated", &about_run, &raw_tasks)
                .unwrap_err()
                .to_string()
                .contains("model-authored SVG")
        );

        let header = draft.replace(RUN_STATS_PLACEHOLDER, "<RichText>Run header</RichText>");
        assert!(
            assemble_artifact(&header, &input, "generated", &about_run, &raw_tasks)
                .unwrap_err()
                .to_string()
                .contains("each orchestrator placeholder once")
        );

        let trailing = format!("{draft}\n<Section><RichText>PS.</RichText></Section>");
        assert!(
            assemble_artifact(&trailing, &input, "generated", &about_run, &raw_tasks)
                .unwrap_err()
                .to_string()
                .contains("must be the final block")
        );

        let boundary = draft.replace(&raw_tasks[0], &format!("{}1", raw_tasks[0]));
        assert!(
            assemble_artifact(&boundary, &input, "generated", &about_run, &raw_tasks)
                .unwrap_err()
                .to_string()
                .contains("omitted raw-report task ids")
        );
        assert!(!task_is_present("TASK-X.11", "TASK-X.1"));
        assert!(task_is_present("TASK-X.1 ", "TASK-X.1"));

        let decoy = draft.replace(
            QUESTION_PLACEHOLDER,
            &format!(
                "<Section  title=\"Question\">\n<RichText>\nfake question\n</RichText>\n</Section>\n{QUESTION_PLACEHOLDER}"
            ),
        );
        assert!(
            assemble_artifact(&decoy, &input, "generated", &about_run, &raw_tasks)
                .unwrap_err()
                .to_string()
                .contains("Question section does not match the input question verbatim")
        );
    }

    #[test]
    fn critique_assembly_preserves_hostile_target_and_rejects_a_decoy() {
        let (participants, extraction, reviews) = reports(2);
        let about_run = render_about_run(
            ForumKind::Critique,
            &extraction,
            &reviews,
            &participants[0],
            "2026-08-29T21:07:20.123+00:00",
        );
        assert!(about_run.contains("2 critique reports"));
        let raw_tasks = extraction
            .iter()
            .chain(&reviews)
            .map(|report| report.dispatch.task.clone())
            .chain(std::iter::once("TASK-TESTX.5".to_string()))
            .collect::<Vec<_>>();
        let draft = format!(
            "{TARGET_PLACEHOLDER}\n<Section title=\"Verdict\"><RichText>Reject.</RichText></Section>\n<Section title=\"Findings\"><Tabs><Tab label=\"Blocking\"><RichText>Finding.</RichText></Tab></Tabs></Section>\n<Section title=\"From target to verdict\">\n{DIAGRAM_PLACEHOLDER}\n<RichText>Raw reports: {}</RichText>\n</Section>\n<Section><QuestionForm questions={{[]}} /></Section>\n{RUN_STATS_PLACEHOLDER}",
            raw_tasks.join(" ")
        );
        let target = "# Hostile\r\n</RichText> <Section title=\"Target\">decoy</Section> & {rule}";
        let input = ForumInput::Critique {
            target: target.to_string(),
            focus: Some("security & boundaries".to_string()),
            basename: "design.md".to_string(),
        };
        let assembled =
            assemble_artifact(&draft, &input, "<generated/>", &about_run, &raw_tasks).unwrap();
        assert!(assembled.starts_with("<Section title=\"Target\">"));
        assert!(assembled.contains(&escape_rich_text(target)));
        assert!(assembled.contains("**Focus:** security &amp; boundaries"));
        let target_at = assembled.find("title=\"Target\"").unwrap();
        let verdict_at = assembled.find("title=\"Verdict\"").unwrap();
        let findings_at = assembled.find("title=\"Findings\"").unwrap();
        let diagram_at = assembled.find("title=\"From target to verdict\"").unwrap();
        assert!(target_at < verdict_at && verdict_at < findings_at && findings_at < diagram_at);
        assert!(assembled.trim_end().ends_with("</Section>"));

        let cross_mode = draft.replace(
            "<RichText>Reject.</RichText>",
            &format!("<RichText>Reject. {QUESTION_PLACEHOLDER}</RichText>"),
        );
        assert!(
            assemble_artifact(&cross_mode, &input, "generated", &about_run, &raw_tasks)
                .unwrap_err()
                .to_string()
                .contains("each orchestrator placeholder once")
        );

        let decoy = draft.replace(
            TARGET_PLACEHOLDER,
            &format!(
                "<Section  title=\"Target\"><RichText>fake target</RichText></Section>\n{TARGET_PLACEHOLDER}"
            ),
        );
        assert!(
            assemble_artifact(&decoy, &input, "generated", &about_run, &raw_tasks)
                .unwrap_err()
                .to_string()
                .contains("Target section does not match the input target verbatim")
        );

        let misplaced = draft.replacen(
            TARGET_PLACEHOLDER,
            "<Section title=\"Preface\"><RichText>decoy</RichText></Section>\n__ORGASMIC_TARGET_SECTION__",
            1,
        );
        assert!(
            assemble_artifact(&misplaced, &input, "generated", &about_run, &raw_tasks)
                .unwrap_err()
                .to_string()
                .contains("Target must be the first Section")
        );

        let verdict = "<Section title=\"Verdict\"><RichText>Reject.</RichText></Section>";
        let findings = "<Section title=\"Findings\"><Tabs><Tab label=\"Blocking\"><RichText>Finding.</RichText></Tab></Tabs></Section>";
        let reordered = draft.replace(
            &format!("{verdict}\n{findings}"),
            &format!("{findings}\n{verdict}"),
        );
        assert!(
            assemble_artifact(&reordered, &input, "generated", &about_run, &raw_tasks)
                .unwrap_err()
                .to_string()
                .contains("required sections are out of order")
        );
    }

    #[test]
    fn critique_title_uses_headline_then_focus_then_basename() {
        let (_, extraction, reviews) = reports(2);
        let mut fields = fields(&extraction, &reviews);
        let focused = ForumInput::Critique {
            target: "target".to_string(),
            focus: Some("security posture".to_string()),
            basename: "design.md".to_string(),
        };
        assert_eq!(
            focused.artifact_title(&fields),
            "Multi-model critique: security posture"
        );
        fields.headline = Some("Prioritized security verdict".to_string());
        assert_eq!(
            focused.artifact_title(&fields),
            "Prioritized security verdict"
        );
        fields.headline = None;
        let unfocused = ForumInput::Critique {
            target: "target".to_string(),
            focus: None,
            basename: "design.md".to_string(),
        };
        assert_eq!(
            unfocused.artifact_title(&fields),
            "Multi-model critique: design.md"
        );
        assert_eq!(unfocused.diagram_prompt(), "critique of design.md, 6 bytes");
    }
}

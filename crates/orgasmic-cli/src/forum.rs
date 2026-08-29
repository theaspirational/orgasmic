use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::{ArgAction, Args, Subcommand};
use orgasmic_core::{is_valid_greenfield_artifact_id, mint_node_id_for_class, NodeIdClass};
use orgasmic_drivers::catalog::transport_profiles;
use serde::Serialize;
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
    /// Participant as mode,harness,model,effort; repeat at least twice.
    #[arg(long, action = ArgAction::Append, required = true)]
    participant: Vec<String>,
    /// 1-based participant index that performs curation.
    #[arg(long, default_value_t = 1)]
    curator: usize,
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
  orgasmic forum ask --question-file /tmp/question.txt \\
    --participant 'stdio,hermes,openai/gpt-5.6-luna,low' \\
    --participant 'stdio,hermes,google/gemini-3.7-flash,low'

Participants are mode,harness,model,effort. Repeat --participant at least twice.")]
pub struct AskArgs {
    /// Question text. Mutually exclusive with --question-file.
    #[arg(long, conflicts_with = "question_file", allow_hyphen_values = true)]
    question: Option<String>,
    /// Read the question from this UTF-8 file.
    #[arg(long = "question-file", conflicts_with = "question")]
    question_file: Option<PathBuf>,
    #[command(flatten)]
    run: RunArgs,
}

#[derive(Args, Debug, Clone)]
#[command(after_help = "\
Example:
  orgasmic forum critique --target-file /tmp/design.md --focus 'security posture' \\
    --participant 'stdio,hermes,openai/gpt-5.6-luna,low' \\
    --participant 'stdio,hermes,google/gemini-3.7-flash,low'

Participants are mode,harness,model,effort. Repeat --participant at least twice.")]
pub struct CritiqueArgs {
    /// UTF-8 document to critique (non-empty, at most 64 KiB).
    #[arg(long = "target-file")]
    target_file: PathBuf,
    /// Optional one-line steer for the critique.
    #[arg(long, allow_hyphen_values = true)]
    focus: Option<String>,
    #[command(flatten)]
    run: RunArgs,
}

#[derive(Clone, Debug, Eq, PartialEq)]
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

#[derive(Debug)]
struct DiagramFields {
    extracts: BTreeMap<String, Vec<String>>,
    reviews: BTreeMap<String, Vec<DeltaBullet>>,
    curator_summary: String,
    headline: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
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

#[derive(Debug)]
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
struct AskResult {
    parent_task: String,
    extraction_tasks: Vec<String>,
    cross_review_tasks: Vec<String>,
    curation_task: String,
    artifact_id: String,
}

#[derive(Serialize)]
struct CritiqueResult {
    parent_task: String,
    critique_tasks: Vec<String>,
    cross_review_tasks: Vec<String>,
    curation_task: String,
    artifact_id: String,
}

struct RunResult {
    parent_task: String,
    first_stage_tasks: Vec<String>,
    cross_review_tasks: Vec<String>,
    curation_task: String,
    artifact_id: String,
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
) -> Result<String> {
    if extraction.len() < 2 || extraction.len() != reviews.len() {
        bail!("diagram requires matching extraction and review rosters");
    }
    if extraction
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
    let width = margin * 2 + count * card_width + (count - 1) * gap;
    let center = width / 2;
    let card_xs = (0..count)
        .map(|index| margin + index * (card_width + gap))
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
            if source_index == target_index {
                continue;
            }
            write!(
                out,
                "<path d=\"M{source},424 C{source},464 {destination},464 {destination},504\" fill=\"none\" stroke=\"#b9a998\" stroke-width=\"1.25\" opacity=\"0.55\" marker-end=\"url(#ah)\"/>"
            )?;
        }
    }
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

    for source in &card_centers {
        write!(
            out,
            "<path d=\"M{source},704 C{source},740 {center},740 {center},776\" fill=\"none\" stroke=\"#b9a998\" stroke-width=\"1.25\" opacity=\"0.55\" marker-end=\"url(#ah)\"/>"
        )?;
    }
    out.push_str("<g data-pill=\"curate\">");
    write!(
        out,
        "<rect x=\"{}\" y=\"729\" width=\"120\" height=\"22\" rx=\"11\" fill=\"#241a15\" stroke=\"{border}\"/>",
        center - 60
    )?;
    out.push_str(&svg_text(center, 743, "3 · CURATE", &stage_style, &[]));
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

fn assemble_artifact(
    draft: &str,
    input: &ForumInput,
    svg: &str,
    about_run: &str,
    raw_tasks: &[String],
) -> Result<String> {
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
                "Question flows through independent extraction and blind cross-review into curation",
                "From the verbatim question to the curated final answer.",
            ),
            ForumKind::Critique => (
                TARGET_PLACEHOLDER,
                QUESTION_PLACEHOLDER,
                "Target",
                &["Target", "Verdict", "Findings", "From target to verdict"][..],
                "Target flows through independent critique and blind cross-review into curation",
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
        .filter(|task| !task_is_present(&mdx, task))
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

fn validate_participants(participants: &[Participant]) -> Result<()> {
    if participants.len() < 2 {
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

fn read_target(path: &Path) -> Result<String> {
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
        curator: curator_index,
        source_ref,
        artifact_id,
        project: requested_project,
        timeout,
    } = args;
    let kind = input.kind();
    let participants = participant
        .iter()
        .map(|raw| parse_participant(raw))
        .collect::<Result<Vec<_>>>()?;
    validate_participants(&participants)?;
    if !(1..=participants.len()).contains(&curator_index) {
        bail!("--curator must select a 1-based participant entry");
    }
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
    let source_ref = match source_ref {
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
    let curator = participants[curator_index - 1].clone();
    let api = Api::new(home, project.clone(), kind)?;
    let started_at = chrono::Utc::now().to_rfc3339();
    let roster = participants
        .iter()
        .map(Participant::identity)
        .collect::<Vec<_>>()
        .join(" — ");
    let parent = mint_node_id_for_class(NodeIdClass::Task);
    let (parent_description, parent_acceptance) = match &input {
        ForumInput::Ask { question } => (
            format!(
                "Question: {}\n\nParticipants: {roster}",
                question.split_whitespace().collect::<Vec<_>>().join(" ")
            ),
            "All extraction and blind-review reports are promoted and one curated artifact is submitted.",
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
            "All critique and blind-review reports are promoted and one curated verdict artifact is submitted.",
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
    eprintln!("parent_task={parent}");

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
            let ordinal = index + 1;
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
                format!(
                    "mm-{}-{stage_branch}-{ordinal}",
                    parent.trim_start_matches("TASK-").to_ascii_lowercase()
                ),
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
        for (index, participant) in participants.iter().enumerate() {
            let ordinal = index + 1;
            let task = format!("{parent}.{}", participants.len() + ordinal);
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
                format!(
                    "mm-{}-review-{ordinal}",
                    parent.trim_start_matches("TASK-").to_ascii_lowercase()
                ),
                "blind cross-review of other model reports",
            )?;
            active.push(dispatch.clone());
            review_dispatches.push(dispatch);
        }
        wait_barrier(home, &review_dispatches, timeout)?;
        for mut dispatch in review_dispatches {
            let path = close_and_finish(home, &api, &ledger, &mut dispatch)?;
            mark_closed(&mut active, &dispatch);
            reviews.push(RunReport {
                participant: dispatch.participant.clone(),
                dispatch,
                path,
            });
        }

        let curator_task = format!("{parent}.{}", 2 * participants.len() + 1);
        let run_manifest = format!(
            "Parent task: {parent}\nStarted UTC: {started_at}\nParticipants ({}):\n{}\nCurator: {}\n\n{}\n\n{}\n\nCuration task: {curator_task}",
            participants.len(),
            participants
                .iter()
                .map(|participant| format!("- {}", participant.identity()))
                .collect::<Vec<_>>()
                .join("\n"),
            curator.identity(),
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
            reviews
                .iter()
                .map(|report| manifest_entry("Cross-review", report))
                .collect::<Vec<_>>()
                .join("\n\n"),
        );
        let curator_brief = tmp.path().join("curator.md");
        std::fs::write(&curator_brief, {
            let mut values = input.prompt_values();
            values.insert("dispatch.brief".to_string(), run_manifest);
            values.insert("task.id".to_string(), curator_task.clone());
            api.compile_prompt(kind.curator_spec(), values)?
        })?;
        let (curator_title, curator_description, curator_acceptance) = match kind {
            ForumKind::Ask => (
                "Curate answer",
                "Read all promoted extraction and cross-review reports, write the final prose draft and structured diagram fields, and report their paths.",
                "The prose draft matches the final-artifact contract, names every raw-report task, and contains only orchestrator placeholders for the run stats, Question, and diagram.",
            ),
            ForumKind::Critique => (
                "Curate verdict",
                "Read all promoted critique and cross-review reports, write the final verdict draft and structured diagram fields, and report their paths.",
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
            &curator,
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
        let extraction_tasks = extraction
            .iter()
            .map(|report| report.dispatch.task.clone())
            .collect::<Vec<_>>();
        let review_tasks = reviews
            .iter()
            .map(|report| report.dispatch.task.clone())
            .collect::<Vec<_>>();
        let fields = load_diagram_fields(&fields_path, &extraction_tasks, &review_tasks)?;
        let svg = render_pipeline_svg(
            &input.diagram_prompt(),
            &extraction,
            &reviews,
            &curator,
            &curator_task,
            &curator_report_path,
            &fields,
        )?;
        let raw_tasks = extraction_tasks
            .iter()
            .chain(&review_tasks)
            .cloned()
            .chain(std::iter::once(curator_task.clone()))
            .collect::<Vec<_>>();
        let draft = std::fs::read_to_string(&draft_path)
            .with_context(|| format!("read {}", draft_path.display()))?;
        let about_run = render_about_run(kind, &extraction, &reviews, &curator, &started_at);
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

        api.set_evidence(
            &parent,
            &format!(
                "- Artifact: {artifact}\n- {} tasks: {}\n- Cross-review tasks: {}\n- Curation task: {curator_task}\n",
                match kind {
                    ForumKind::Ask => "Extraction",
                    ForumKind::Critique => "Critique",
                },
                extraction_tasks.join(" "),
                review_tasks.join(" ")
            ),
        )?;
        api.finish_task(&parent)?;
        Ok(RunResult {
            parent_task: parent.clone(),
            first_stage_tasks: extraction_tasks,
            cross_review_tasks: review_tasks,
            curation_task: curator_task,
            artifact_id: artifact,
        })
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
    let question = match (args.question, args.question_file) {
        (Some(question), None) => question,
        (None, Some(path)) => {
            std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?
        }
        _ => bail!("one of --question or --question-file is required"),
    };
    let question = question.trim().to_string();
    validate_question(&question)?;
    let result = run_forum(home, ForumInput::Ask { question }, args.run)?;
    Ok(AskResult {
        parent_task: result.parent_task,
        extraction_tasks: result.first_stage_tasks,
        cross_review_tasks: result.cross_review_tasks,
        curation_task: result.curation_task,
        artifact_id: result.artifact_id,
    })
}

fn run_critique(home: &Home, args: CritiqueArgs) -> Result<CritiqueResult> {
    let target = read_target(&args.target_file)?;
    let focus = match args.focus {
        Some(focus) => {
            let focus = focus.trim().to_string();
            validate_focus(&focus)?;
            Some(focus)
        }
        None => None,
    };
    let basename = args
        .target_file
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
    Ok(CritiqueResult {
        parent_task: result.parent_task,
        critique_tasks: result.first_stage_tasks,
        cross_review_tasks: result.cross_review_tasks,
        curation_task: result.curation_task,
        artifact_id: result.artifact_id,
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
        assert!(validate_participants(&[codex.clone(), codex]).is_err());
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

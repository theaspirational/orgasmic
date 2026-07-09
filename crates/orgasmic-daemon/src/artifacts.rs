// arch: arch_045Q0.1
// orgasmic:arch_ARSPJ
//! Artifact store: on-disk layout, block registry, index projection helpers,
//! and read/write functions called from the API handlers.
//!
//! Layout per artifact:
//!   .orgasmic/artifacts/ART-<slug>/
//!     artifact.mdx   — opaque MDX; never parsed as Org
//!     artifact.org   — single heading with :ID: :TITLE: :SUBJECT_NODES: :PROMPT: :VERSION: :STATE:
//!     reviews.org    — append-only comment headings
//!     versions/      — vN.mdx archives (written at regeneration, TASK-EDQPG)
//!
//! artifact.org and reviews.org are read through the canonical
//! `orgasmic_core::OrgFile` parser (no hand-rolled second parser); writes
//! that mutate an existing heading go through `OrgRewriter` or a targeted
//! property-span splice computed from the canonical parse.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use orgasmic_core::{Heading, OrgFile, OrgRewriter};
use serde::{Deserialize, Serialize};

/// All Agent-Native block types that are valid in artifact.mdx.
/// Validated case-sensitively against PascalCase JSX component names.
pub const BLOCK_TYPES: &[&str] = &[
    "RichText",
    "Diagram",
    "Code",
    "AnnotatedCode",
    "Table",
    "Callout",
    "Checklist",
    "FileTree",
    "DataModel",
    "QuestionForm",
    "Wireframe",
    "Canvas",
    "Prototype",
    "Tabs",
    "Columns",
    "Section",
    "Image",
    "SequenceDiagram",
    "FlowChart",
    "Mermaid",
    "Timeline",
    "EntityRelationship",
];

/// Index-level summary written into `ProjectIndex.artifacts`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactSummary {
    pub id: String,
    pub title: String,
    pub subject_nodes: Vec<String>,
    pub version: u32,
    pub state: String,
    pub open_comment_count: usize,
}

/// Full artifact detail including MDX content, returned by GET /artifacts/:id.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactDetail {
    #[serde(flatten)]
    pub summary: ArtifactSummary,
    pub prompt: String,
    pub content: String,
    pub comments: Vec<CommentRecord>,
}

/// One entry from reviews.org.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommentRecord {
    pub cid: String,
    pub author: String,
    pub version: u32,
    pub anchor: String,
    pub resolution_target: String,
    pub resolved: bool,
    pub consumed: bool,
    pub message: String,
}

/// Why [`load_artifact_detail`] could not produce a detail view. Distinct
/// from a generic "not found" so the API can tell "no such artifact" apart
/// from "artifact exists but has no vN archive" (both are 404s, but with a
/// different message).
#[derive(Debug, thiserror::Error)]
pub enum ArtifactLoadError {
    #[error("artifact not found")]
    NotFound,
    #[error("artifact has no version {0}")]
    VersionNotFound(u32),
}

// ── path helpers ──────────────────────────────────────────────────────────────

pub fn artifacts_dir(project_root: &Path) -> PathBuf {
    project_root.join(".orgasmic").join("artifacts")
}

pub fn artifact_dir(project_root: &Path, art_id: &str) -> PathBuf {
    artifacts_dir(project_root).join(art_id)
}

fn artifact_org_path(art_dir: &Path) -> PathBuf {
    art_dir.join("artifact.org")
}

fn artifact_mdx_path(art_dir: &Path) -> PathBuf {
    art_dir.join("artifact.mdx")
}

fn reviews_org_path(art_dir: &Path) -> PathBuf {
    art_dir.join("reviews.org")
}

/// Directory holding archived `vN.mdx` snapshots. `pub` so callers folding
/// the archive write into a `writer.transaction` (rather than a bypass
/// `fs::copy`) can address the target path.
pub fn versions_dir(art_dir: &Path) -> PathBuf {
    art_dir.join("versions")
}

// ── MDX block-registry validation ────────────────────────────────────────────

/// Scan a top-level component tag's header, starting at `i` (the byte
/// position immediately after the tag name). Returns
/// `(self_closing, index_after_closing_angle_bracket)`, or `None` if the
/// header runs off the end of `bytes` before an unquoted `>` is found — a
/// malformed/unclosed top-level tag.
fn scan_tag_header(bytes: &[u8], mut i: usize) -> Option<(bool, usize)> {
    let mut quote: Option<u8> = None;
    while i < bytes.len() {
        let b = bytes[i];
        if let Some(q) = quote {
            if b == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        match b {
            b'"' | b'\'' => {
                quote = Some(b);
                i += 1;
            }
            b'>' => {
                let self_closing = i > 0 && bytes[i - 1] == b'/';
                return Some((self_closing, i + 1));
            }
            _ => i += 1,
        }
    }
    None
}

/// Validate the top-level structure of `content`.
///
/// The daemon has no real MDX parser, so this validates only what a byte
/// scan can see reliably: every top-level JSX component tag (PascalCase,
/// e.g. `<Code>`) must be a registered block type (`BLOCK_TYPES`), and must
/// be well-formed and balanced. Once a registered block's opening tag is
/// found, everything inside its body — up to the literal matching closing
/// tag — is opaque and never scanned for nested tags. That is where
/// generics (`Vec<Item>`, `f::<String>()`) and DSL-specific angle-bracket
/// content legitimately live. Deep/nested MDX validation is out of scope
/// (TASK-T25XQ owns the real JS/MDX parser); this is a structural gate only.
///
/// An artifact with zero registered top-level blocks is rejected — prose or
/// an empty payload alone is not a valid artifact body (MANAGER RULING,
/// TASK-Y2ZQJ). Unclosed or malformed top-level tags are rejected too.
pub fn validate_mdx(content: &str) -> Vec<String> {
    let known: HashSet<&str> = BLOCK_TYPES.iter().copied().collect();
    let mut seen_unknown: HashSet<String> = HashSet::new();
    let mut errors: Vec<String> = Vec::new();
    let mut found_registered_block = false;

    let bytes = content.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] != b'<' {
            i += 1;
            continue;
        }
        i += 1;
        // Skip closing tags `</`, comments `<!--`, and truncated `<` at EOF.
        if i >= bytes.len() || bytes[i] == b'/' || bytes[i] == b'!' {
            continue;
        }
        // Read component name (alphanumeric + hyphens, like MDX JSX).
        let name_start = i;
        while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'-') {
            i += 1;
        }
        if i == name_start {
            continue;
        }
        let name = &content[name_start..i];
        // Only PascalCase names are JSX components; lowercase = HTML tags,
        // passed through untouched (not validated, not tracked).
        if !name.as_bytes()[0].is_ascii_uppercase() {
            continue;
        }
        let Some((self_closing, header_end)) = scan_tag_header(bytes, i) else {
            errors.push(format!("malformed or unclosed top-level tag `<{name}`"));
            break;
        };
        if known.contains(name) {
            found_registered_block = true;
        } else if seen_unknown.insert(name.to_string()) {
            errors.push(format!("unknown block type `{name}`"));
        }
        if self_closing {
            i = header_end;
            continue;
        }
        // Opening tag: the body is opaque until the literal matching close.
        let closing = format!("</{name}>");
        match content[header_end..].find(closing.as_str()) {
            Some(rel) => i = header_end + rel + closing.len(),
            None => {
                errors.push(format!(
                    "unclosed top-level tag `<{name}>` (no matching `</{name}>`)"
                ));
                break;
            }
        }
    }

    if !found_registered_block {
        errors.push("artifact must contain at least one registered top-level block".to_string());
    }

    errors
}

// ── single-line property escaping ───────────────────────────────────────────

/// Collapse embedded newlines so a value stays a single physical line. A
/// value with a literal `\n` would prematurely terminate its
/// `:KEY: value` property-drawer line and corrupt everything after it
/// (tx.org's `:PROMPT:` contract already promises "escaped single-line";
/// artifact.org's `:TITLE:`/`:PROMPT:` follow the same discipline).
pub fn escape_single_line(value: &str) -> String {
    value.replace("\r\n", " ").replace(['\n', '\r'], " ")
}

// ── comment-body structural escaping (`*` headings + `#+begin_`/`#+end_` blocks) ─

/// True when `line`, after stripping any leading commas already present, would
/// be read by the canonical parser as structure rather than free text — either
/// a `* heading` or an org block marker (`#+begin_…` / `#+end_…`). Both are
/// injection vectors from a member-authored comment body:
/// - a column-0 `*` line forks reviews.org into a spurious extra heading
///   (`orgasmic_core::org::heading_level` matches `*` only at byte 0);
/// - an unbalanced `#+begin_`/`#+end_` line drives the file-global block mask
///   (`org::org_block_line_mask`), which lower-cases and trim-starts the line
///   before matching and never resets at EOF — so one stray marker can suppress
///   heading recognition for every LATER comment in the same reviews.org
///   (TASK-KBHAN).
///
/// A single leading comma neutralizes both: the parser's `trim_start()` strips
/// whitespace but not a comma, so `,#+begin_src` / `,* h` are inert. Mirrors the
/// comma-quoting convention `orgasmic_core::wrap_raw_body` uses for raw bodies.
/// The block check matches the parser's semantics exactly: case-insensitive and
/// after leading-whitespace trim (so `  #+BEGIN_SRC` is caught too).
fn comment_line_needs_escape(line: &str) -> bool {
    let bare = line.trim_start_matches(',');
    if bare.starts_with('*') {
        return true;
    }
    let directive = bare.trim_start().to_ascii_lowercase();
    directive.starts_with("#+begin_") || directive.starts_with("#+end_")
}

/// Escape a comment message so no line can be misread as a `* heading` or an
/// org block marker once appended to reviews.org. Reversed by the unescape step
/// in [`comment_record_from_heading`].
fn escape_comment_message(message: &str) -> String {
    message
        .split('\n')
        .map(|line| {
            if comment_line_needs_escape(line) {
                format!(",{line}")
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// ── artifact.org read/write ──────────────────────────────────────────────────

/// Content for a brand-new artifact.org file.
pub fn artifact_org_content(
    id: &str,
    title: &str,
    subject_nodes: &[String],
    prompt: &str,
    version: u32,
    state: &str,
) -> String {
    let title = escape_single_line(title);
    let prompt = escape_single_line(prompt);
    let subject_str = subject_nodes.join(" ");
    format!(
        "#+title: orgasmic artifact {id}\n\
         #+orgasmic_version: 1\n\
         \n\
         * {id} {title}\n\
         :PROPERTIES:\n\
         :ID:           {id}\n\
         :TITLE:        {title}\n\
         :SUBJECT_NODES: {subject_str}\n\
         :PROMPT:       {prompt}\n\
         :VERSION:      {version}\n\
         :STATE:        {state}\n\
         :END:\n"
    )
}

/// Rewrite artifact.org updating :VERSION: and :STATE: only, via the
/// canonical `OrgRewriter` (touches only those two property value spans;
/// every other byte, including drawer column alignment, is preserved).
pub fn update_artifact_org(current: &str, new_version: u32, new_state: &str) -> Result<Vec<u8>> {
    let file = OrgFile::parse(current, "artifact.org").context("parse artifact.org")?;
    let Some(id) = file.headings.first().and_then(|h| h.property("ID")) else {
        bail!("artifact.org has no heading with an :ID:");
    };
    let id = id.to_string();
    let mut rw = OrgRewriter::new(&file, "artifact.org");
    rw.set_property(&id, "VERSION", &new_version.to_string())
        .context("update :VERSION:")?;
    rw.set_property(&id, "STATE", new_state)
        .context("update :STATE:")?;
    Ok(rw.finish().into_bytes())
}

// ── reviews.org read/write ───────────────────────────────────────────────────

/// Initial content for a new reviews.org file (created before first comment).
pub fn reviews_org_header(art_id: &str) -> String {
    format!(
        "#+title: orgasmic artifact reviews {art_id}\n\
         #+orgasmic_version: 1\n"
    )
}

/// Fields for one new comment. Grouped into a struct (rather than passed
/// positionally) so [`comment_org_block`] and [`append_comment`] stay under
/// clippy's argument-count lint.
pub struct NewComment<'a> {
    pub cid: &'a str,
    pub author: &'a str,
    pub version: u32,
    pub anchor: &'a str,
    pub resolution_target: &'a str,
    pub message: &'a str,
}

/// Org heading block for one comment, ready to append to reviews.org. Lines
/// in the message that would be misread as a heading (leading `*`) are
/// comma-escaped first.
pub fn comment_org_block(comment: &NewComment<'_>) -> String {
    let NewComment {
        cid,
        author,
        version,
        anchor,
        resolution_target,
        message,
    } = comment;
    let message = escape_comment_message(message);
    format!(
        "\n* {cid}\n\
         :PROPERTIES:\n\
         :CID:              {cid}\n\
         :AUTHOR:           {author}\n\
         :VERSION:          {version}\n\
         :ANCHOR:           {anchor}\n\
         :RESOLUTION_TARGET: {resolution_target}\n\
         :RESOLVED:         false\n\
         :CONSUMED:         false\n\
         :END:\n\
         \n\
         {message}\n"
    )
}

/// Compute the new reviews.org content after appending one comment. Seeds
/// the `#+title`/`#+orgasmic_version` header when `current` is empty (the
/// file does not exist on disk yet). Pure — callers write the result
/// through `state.writer.transaction` alongside the tx entry so the file
/// rewrite and the tx append commit atomically.
pub fn append_comment(current: &str, art_id: &str, comment: &NewComment<'_>) -> String {
    let mut out = if current.is_empty() {
        reviews_org_header(art_id)
    } else {
        current.to_string()
    };
    out.push_str(&comment_org_block(comment));
    out
}

/// Rewrite reviews.org toggling `cid`'s `RESOLVED` flag only, leaving
/// `CONSUMED` untouched. Two-axis thread state (dec_V44E4 / dec_KF2MR):
/// open/resolved is people-facing and settable by any member with
/// `artifacts.comment`; consumed is agent-facing and set only by
/// regeneration ([`consume_all_open_comments`]). Locates the property value
/// span via the canonical parser and splices only that span, leaving every
/// other byte (including column alignment) untouched.
pub fn set_comment_resolved(current: &str, cid: &str, resolved: bool) -> Result<Vec<u8>> {
    let file = OrgFile::parse(current, "reviews.org").context("parse reviews.org")?;
    let Some(heading) = file
        .headings
        .iter()
        .find(|h| h.property("CID") == Some(cid))
    else {
        bail!("comment {cid} not found in reviews.org");
    };

    let Some(entry) = heading.property_entries().find(|e| e.key == "RESOLVED") else {
        bail!("comment {cid} has no RESOLVED property");
    };
    let range = entry.value_span.clone();
    let replacement = if resolved { "true" } else { "false" };

    let mut out = String::with_capacity(current.len());
    out.push_str(&current[..range.start]);
    out.push_str(replacement);
    out.push_str(&current[range.end..]);
    Ok(out.into_bytes())
}

// ── index projection ─────────────────────────────────────────────────────────

/// Build a [`CommentRecord`] from a parsed reviews.org heading, unescaping
/// the leading-`*` comma escape and trimming the template's blank-line
/// padding around the message body.
fn comment_record_from_heading(file: &OrgFile, heading: &Heading) -> Option<CommentRecord> {
    let cid = heading.property("CID")?.to_string();
    if cid.is_empty() {
        return None;
    }
    let version = heading
        .property("VERSION")
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(1);
    let resolved = heading
        .property("RESOLVED")
        .map(|v| v == "true")
        .unwrap_or(false);
    let consumed = heading
        .property("CONSUMED")
        .map(|v| v == "true")
        .unwrap_or(false);
    let message = trim_and_unescape_comment_body(file.slice(heading.body.clone()));
    Some(CommentRecord {
        cid,
        author: heading.property("AUTHOR").unwrap_or("").to_string(),
        version,
        anchor: heading
            .property("ANCHOR")
            .map(str::to_string)
            .unwrap_or_else(|| "{}".into()),
        resolution_target: heading
            .property("RESOLUTION_TARGET")
            .unwrap_or("")
            .to_string(),
        resolved,
        consumed,
        message,
    })
}

/// Trim leading/trailing blank lines from a raw heading body (the template
/// blank line right after `:END:`), then reverse the structural escape
/// (leading-`*` headings and `#+begin_`/`#+end_` block markers).
fn trim_and_unescape_comment_body(raw: &str) -> String {
    let mut lines: Vec<&str> = raw.lines().collect();
    while lines.first().map(|l| l.trim().is_empty()).unwrap_or(false) {
        lines.remove(0);
    }
    while lines.last().map(|l| l.trim().is_empty()).unwrap_or(false) {
        lines.pop();
    }
    lines
        .into_iter()
        .map(|line| {
            if comment_line_needs_escape(line) {
                line.strip_prefix(',').unwrap_or(line).to_string()
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Parse all comment records from reviews.org content via the canonical Org
/// parser. Each comment is a level-1 heading whose free body (everything
/// after its property drawer) is the message text.
pub fn parse_comments(content: &str) -> Vec<CommentRecord> {
    let Ok(file) = OrgFile::parse(content, "reviews.org") else {
        return Vec::new();
    };
    file.headings
        .iter()
        .filter_map(|heading| comment_record_from_heading(&file, heading))
        .collect()
}

/// Load an ArtifactSummary from an ART-* directory.
pub fn load_artifact(art_dir: &Path) -> Option<ArtifactSummary> {
    let org_path = artifact_org_path(art_dir);
    let content = fs::read_to_string(&org_path).ok()?;
    let file = OrgFile::parse(content, org_path.display().to_string()).ok()?;
    let heading = file.headings.first()?;

    let id = heading.property("ID")?.to_string();
    if id.is_empty() {
        return None;
    }
    let title = heading.property("TITLE").unwrap_or("").to_string();
    let subject_nodes: Vec<String> = heading
        .property("SUBJECT_NODES")
        .unwrap_or("")
        .split_whitespace()
        .map(str::to_string)
        .collect();
    let version = heading
        .property("VERSION")
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(1);
    let state = heading
        .property("STATE")
        .map(str::to_string)
        .unwrap_or_else(|| "submitted".to_string());

    // Count open comments (never resolved+consumed).
    let open_comment_count = {
        let reviews_path = reviews_org_path(art_dir);
        if let Ok(reviews_content) = fs::read_to_string(&reviews_path) {
            parse_comments(&reviews_content)
                .iter()
                .filter(|c| !c.resolved && !c.consumed)
                .count()
        } else {
            0
        }
    };

    Some(ArtifactSummary {
        id,
        title,
        subject_nodes,
        version,
        state,
        open_comment_count,
    })
}

/// Load full artifact detail including MDX and comments.
///
/// Comment scoping (TASK-EDQPG): when `version` names an *archived* version
/// (`Some(n)` where `n != summary.version`), `comments` is scoped to that
/// version's own thread (`c.version == n`) regardless of consumed state —
/// an archived view always shows the thread that was closed out with it,
/// consumed or not, and never another version's comments. Otherwise (no
/// version requested, or the requested version equals the current/live
/// version) behavior is unchanged from before archived-version support:
/// `include_consumed` alone decides whether consumed comments are stripped
/// (current-version reads default to excluding them via `include_consumed:
/// false`; callers that need the full current-version thread pass `true`).
pub fn load_artifact_detail(
    art_dir: &Path,
    version: Option<u32>,
    include_consumed: bool,
) -> Result<ArtifactDetail, ArtifactLoadError> {
    let summary = load_artifact(art_dir).ok_or(ArtifactLoadError::NotFound)?;

    let org_path = artifact_org_path(art_dir);
    let org_content = fs::read_to_string(&org_path).map_err(|_| ArtifactLoadError::NotFound)?;
    let prompt = OrgFile::parse(org_content, org_path.display().to_string())
        .ok()
        .and_then(|file| {
            file.headings
                .first()
                .and_then(|h| h.property("PROMPT").map(str::to_string))
        })
        .unwrap_or_default();

    // MDX content: either current or a versioned archive.
    let content = if let Some(v) = version {
        let versioned = versions_dir(art_dir).join(format!("v{v}.mdx"));
        fs::read_to_string(&versioned).map_err(|_| ArtifactLoadError::VersionNotFound(v))?
    } else {
        fs::read_to_string(artifact_mdx_path(art_dir)).unwrap_or_default()
    };

    let reviews_path = reviews_org_path(art_dir);
    let mut comments = if let Ok(reviews_content) = fs::read_to_string(&reviews_path) {
        parse_comments(&reviews_content)
    } else {
        Vec::new()
    };
    match version {
        Some(v) if v != summary.version => {
            // Archived view: that version's own thread, consumed or not.
            comments.retain(|c| c.version == v);
        }
        _ => {
            if !include_consumed {
                comments.retain(|c| !c.consumed);
            }
        }
    }

    Ok(ArtifactDetail {
        summary,
        prompt,
        content,
        comments,
    })
}

/// Load all artifact summaries for a project (for index projection).
pub fn load_project_artifacts(project_root: &Path) -> Vec<ArtifactSummary> {
    let dir = artifacts_dir(project_root);
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut summaries = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if !name.starts_with("ART-") {
            continue;
        }
        if let Some(summary) = load_artifact(&path) {
            summaries.push(summary);
        }
    }
    summaries
}

/// Generate a short unique comment ID.
pub fn new_cid() -> String {
    let id = uuid::Uuid::new_v4().to_string().replace('-', "");
    format!("CID-{}", &id[..8])
}

/// Mint a fresh artifact id: `ART-` plus a 5-char Crockford base32 stem.
/// Delegates to `orgasmic_core::mint_node_id` so the minter and
/// [`orgasmic_core::is_valid_greenfield_artifact_id`] validator share one
/// grammar definition (`NodeIdClass::Artifact`).
pub fn new_artifact_id() -> String {
    orgasmic_core::mint_node_id(orgasmic_core::NodeIdClass::Artifact)
}

/// Validate an incoming `art_id` path segment against the minted-artifact
/// grammar (`ART-<5-char-Crockford-stem>`) before it reaches [`artifact_dir`]
/// or any fs/index touch. Rejects traversal, wrong prefix, and malformed
/// stems in one choke point shared by every artifact route.
pub fn validate_art_id(art_id: &str) -> Result<(), String> {
    if orgasmic_core::is_valid_greenfield_artifact_id(art_id) {
        Ok(())
    } else {
        Err(format!(
            "invalid artifact id {art_id:?}: expected ART-<5-char-Crockford-stem> (e.g. ART-8KX2M); mint one with `orgasmic id mint --class artifact`"
        ))
    }
}

/// Traversal-safe validation for **read** routes that resolve an *already
/// existing* artifact directory (`GET /artifacts/:id`).
///
/// Unlike [`validate_art_id`] — which enforces the strict minted Crockford
/// grammar and is reserved for create/mint so fresh ids are always well-formed
/// — this only guarantees `art_id` is a single safe path segment under the
/// artifacts dir. It requires the `ART-` prefix and a non-empty stem drawn from
/// `[A-Za-z0-9_-]`, which cannot contain `/`, `\`, `.` (so no `..` traversal),
/// or NUL. That whitelist lets legacy / hand-authored semantic ids (e.g.
/// `ART-DEDUP`) that predate the grammar — and which the list route already
/// surfaces from disk — be opened, instead of 400-ing "listable but
/// unopenable". A genuinely missing id still 404s downstream via
/// [`ArtifactLoadError::NotFound`]; a traversal attempt is rejected here.
pub fn validate_art_id_readable(art_id: &str) -> Result<(), String> {
    let stem = art_id.strip_prefix("ART-");
    let ok = stem.is_some_and(|stem| {
        !stem.is_empty()
            && stem.len() <= 64
            && stem
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    });
    if ok {
        Ok(())
    } else {
        Err(format!(
            "invalid artifact id {art_id:?}: expected ART- followed by 1-64 characters (letters, digits, `-` or `_`)"
        ))
    }
}

/// Rewrite reviews.org marking every currently open (neither resolved nor
/// consumed) comment as resolved+consumed in one pass.
///
/// Used when a regenerate closes out the current version's comment surface
/// (dec_V44E4: "the fresh version starts with a clean comment surface").
/// Mirrors [`set_comment_resolved`]'s targeted property-span splice,
/// generalized to every open heading instead of one `cid`.
pub fn consume_all_open_comments(current: &str) -> Result<Vec<u8>> {
    if current.trim().is_empty() {
        return Ok(current.as_bytes().to_vec());
    }
    let file = OrgFile::parse(current, "reviews.org").context("parse reviews.org")?;

    let mut edits: Vec<_> = Vec::new();
    for heading in &file.headings {
        let resolved = heading.property("RESOLVED") == Some("true");
        let consumed = heading.property("CONSUMED") == Some("true");
        if resolved && consumed {
            continue;
        }
        edits.extend(
            heading
                .property_entries()
                .filter(|e| e.key == "RESOLVED" || e.key == "CONSUMED")
                .map(|e| (e.value_span.clone(), "true")),
        );
    }
    edits.sort_by_key(|(range, _)| range.start);

    let mut out = String::with_capacity(current.len());
    let mut cursor = 0usize;
    for (range, replacement) in edits {
        out.push_str(&current[cursor..range.start]);
        out.push_str(replacement);
        cursor = range.end;
    }
    out.push_str(&current[cursor..]);
    Ok(out.into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_registry_has_22_entries() {
        assert_eq!(BLOCK_TYPES.len(), 22);
    }

    /// The artifact-generator prompt spec's block-vocabulary reference is
    /// hand-authored prose (prompt specs are static org text, not
    /// template-injected from Rust consts), so it can silently drift from
    /// `BLOCK_TYPES`. This test is the single-source tripwire: every
    /// registered block name must appear in the shipped spec text.
    #[test]
    fn artifact_generator_spec_lists_every_block_type() {
        let mut here = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        loop {
            if here.join(".orgasmic").is_dir() && here.join("shipped").is_dir() {
                break;
            }
            assert!(
                here.pop(),
                "could not locate repo root from CARGO_MANIFEST_DIR"
            );
        }
        let spec_path = here.join("shipped/prompt-studio/prompt-specs/artifact-generator.org");
        let spec = fs::read_to_string(&spec_path)
            .unwrap_or_else(|e| panic!("read {}: {e}", spec_path.display()));
        for block in BLOCK_TYPES {
            assert!(
                spec.contains(block),
                "artifact-generator.org is missing block type {block}"
            );
        }
    }

    #[test]
    fn validate_mdx_accepts_known_types() {
        let mdx = "<RichText>hello</RichText>\n<Diagram type=\"flow\" />\n<Table />";
        assert!(validate_mdx(mdx).is_empty());
    }

    #[test]
    fn validate_mdx_rejects_unknown_type() {
        let mdx = "<RichText>ok</RichText>\n<GalacticWidget />";
        let errs = validate_mdx(mdx);
        assert_eq!(errs.len(), 1);
        assert!(errs[0].contains("GalacticWidget"));
    }

    #[test]
    fn validate_mdx_ignores_lowercase_html() {
        let mdx = "<div class=\"wrap\"><p>text</p></div><RichText />";
        assert!(validate_mdx(mdx).is_empty());
    }

    #[test]
    fn validate_mdx_deduplicates_unknown() {
        let mdx = "<RichText/><Unknown /><Unknown /><Unknown />";
        assert_eq!(validate_mdx(mdx).len(), 1);
    }

    // ── TASK-Y2ZQJ policy corpus (reviewer probe strings, run-20260703T194200) ──

    #[test]
    fn probe_code_block_with_generics_is_accepted() {
        let mdx = "<Code>let v: Vec<Item> = f::<String>();</Code>";
        assert!(
            validate_mdx(mdx).is_empty(),
            "generics inside a registered block body must stay opaque"
        );
    }

    #[test]
    fn probe_entity_relationship_body_is_opaque_and_accepted() {
        let mdx = "<EntityRelationship>A<Order>B</EntityRelationship>";
        assert!(
            validate_mdx(mdx).is_empty(),
            "nested-looking tags inside a registered block body must not be scanned"
        );
    }

    #[test]
    fn probe_bare_prose_is_rejected() {
        let mdx = "This is just some prose with no registered block at all.";
        assert!(!validate_mdx(mdx).is_empty());
    }

    #[test]
    fn probe_empty_content_is_rejected() {
        assert!(!validate_mdx("").is_empty());
    }

    #[test]
    fn probe_unclosed_tag_is_rejected() {
        assert!(!validate_mdx("<RichText").is_empty());
    }

    #[test]
    fn parse_comments_round_trip() {
        let header = "#+title: reviews\n#+orgasmic_version: 1\n";
        let block = comment_org_block(&NewComment {
            cid: "CID-abc12345",
            author: "user@test.com",
            version: 1,
            anchor: "{}",
            resolution_target: "",
            message: "Good work.",
        });
        let content = format!("{header}{block}");
        let records = parse_comments(&content);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].cid, "CID-abc12345");
        assert_eq!(records[0].author, "user@test.com");
        assert!(!records[0].resolved);
        assert_eq!(records[0].message, "Good work.");
    }

    #[test]
    fn comment_body_leading_star_escapes_and_round_trips() {
        let header = "#+title: reviews\n#+orgasmic_version: 1\n";
        let message = "* Bullet one\nSecond line\n* Bullet two";
        let block = comment_org_block(&NewComment {
            cid: "CID-star00001",
            author: "user@test.com",
            version: 1,
            anchor: "{}",
            resolution_target: "",
            message,
        });
        let content = format!("{header}{block}");

        // The stored bytes must not contain a column-0 `*` line — otherwise
        // the canonical parser would misread it as a second heading.
        let file = OrgFile::parse(content.clone(), "reviews.org").expect("parses cleanly");
        assert_eq!(
            file.headings.len(),
            1,
            "an escaped comment body must not fork into extra headings"
        );

        let records = parse_comments(&content);
        assert_eq!(records.len(), 1);
        assert_eq!(
            records[0].message, message,
            "leading-* escaping must round-trip losslessly"
        );
    }

    #[test]
    fn comment_body_preexisting_comma_star_round_trips() {
        // A message that already starts with a literal comma before the
        // star must still round-trip exactly (double-escape case).
        let header = "#+title: reviews\n#+orgasmic_version: 1\n";
        let message = ",* not actually a heading";
        let block = comment_org_block(&NewComment {
            cid: "CID-comma0001",
            author: "user@test.com",
            version: 1,
            anchor: "{}",
            resolution_target: "",
            message,
        });
        let content = format!("{header}{block}");
        let records = parse_comments(&content);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].message, message);
    }

    #[test]
    fn comment_body_block_marker_does_not_suppress_later_comments() {
        // TASK-KBHAN: a member-authored comment whose body carries an org block
        // marker must not drive the file-global block mask and swallow LATER
        // comments in the same reviews.org. Teeth: the first comment carries an
        // UNBALANCED `#+begin_src` (no matching `#+end_`) plus a lowercase-defeating
        // uppercase marker and an indented one; a second comment follows.
        let header = "#+title: reviews\n#+orgasmic_version: 1\n";
        let first_msg =
            "look here:\n#+begin_src rust\nlet x = 1;\n#+END_SRC\n  #+begin_example\nstill me";
        let second_msg = "I am the second comment and must survive";
        let block1 = comment_org_block(&NewComment {
            cid: "CID-block00001",
            author: "alice@test.com",
            version: 1,
            anchor: "{}",
            resolution_target: "",
            message: first_msg,
        });
        let block2 = comment_org_block(&NewComment {
            cid: "CID-block00002",
            author: "bob@test.com",
            version: 1,
            anchor: "{}",
            resolution_target: "",
            message: second_msg,
        });
        let content = format!("{header}{block1}{block2}");

        // Without the block-marker escape the stored `#+begin_src` opens a
        // never-closed block, masking every heading after it → the parser sees
        // only one comment. With it, both survive and round-trip losslessly.
        let records = parse_comments(&content);
        assert_eq!(
            records.len(),
            2,
            "a block marker in one comment body must not swallow later comments"
        );
        assert_eq!(
            records[0].message, first_msg,
            "block-marker body round-trips"
        );
        assert_eq!(records[1].message, second_msg);
    }

    #[test]
    fn set_comment_resolved_flips_only_the_resolved_axis() {
        let header = "#+title: reviews\n";
        let block = comment_org_block(&NewComment {
            cid: "CID-abc12345",
            author: "u@t.com",
            version: 1,
            anchor: "{}",
            resolution_target: "",
            message: "msg",
        });
        let content = format!("{header}{block}");
        let updated =
            String::from_utf8(set_comment_resolved(&content, "CID-abc12345", true).unwrap())
                .unwrap();
        let records = parse_comments(&updated);
        assert_eq!(records.len(), 1);
        assert!(records[0].resolved);
        assert!(
            !records[0].consumed,
            "resolved must not also flip consumed (two-axis thread state, dec_V44E4/dec_KF2MR)"
        );

        // Members can toggle back to open.
        let reopened =
            String::from_utf8(set_comment_resolved(&updated, "CID-abc12345", false).unwrap())
                .unwrap();
        let records = parse_comments(&reopened);
        assert!(!records[0].resolved);
        assert!(!records[0].consumed);
    }

    #[test]
    fn set_comment_resolved_errors_on_unknown_cid() {
        let content = "#+title: reviews\n";
        assert!(set_comment_resolved(content, "CID-notexist", true).is_err());
    }

    #[test]
    fn new_artifact_id_is_art_prefixed_five_char_crockford_stem() {
        for _ in 0..50 {
            let id = new_artifact_id();
            let stem = id.strip_prefix("ART-").expect("ART- prefix");
            assert_eq!(stem.len(), 5, "{id}");
            assert!(
                stem.chars().all(|c| orgasmic_core::CROCKFORD.contains(c)),
                "{id}"
            );
            assert!(stem.chars().any(|c| c.is_ascii_alphabetic()), "{id}");
            // Minter and validator must not diverge (one grammar, dec_073a).
            assert!(
                orgasmic_core::is_valid_greenfield_artifact_id(&id),
                "minted id failed the core validator: {id}"
            );
        }
    }

    #[test]
    fn validate_art_id_accepts_minted_and_rejects_traversal() {
        assert!(validate_art_id(&new_artifact_id()).is_ok());
        assert!(validate_art_id("ART-8KX2M").is_ok());
        for bad in [
            "../../etc/passwd",
            "ART-../..",
            "ART-/etc/passwd",
            "ART-AAAA/",
            "art-8kx2m",
            "",
        ] {
            let err = validate_art_id(bad).expect_err(bad);
            assert!(err.contains("ART-"), "{err}");
            assert!(err.contains("orgasmic id mint --class artifact"), "{err}");
        }
    }

    #[test]
    fn validate_art_id_readable_accepts_legacy_and_rejects_traversal() {
        // Minted ids and the strict grammar are of course still fine.
        assert!(validate_art_id_readable(&new_artifact_id()).is_ok());
        assert!(validate_art_id_readable("ART-8KX2M").is_ok());
        // Legacy / hand-authored semantic ids the strict grammar rejects but
        // that are perfectly safe path segments — now openable.
        for good in ["ART-DEDUP", "ART-config_dup", "ART-a", "ART-A1_b-2"] {
            assert!(validate_art_id_readable(good).is_ok(), "{good}");
            // These are exactly the ids the strict mint-time validator rejects.
            assert!(validate_art_id(good).is_err(), "{good}");
        }
        // Traversal / malformed must still be rejected before hitting the fs.
        for bad in [
            "../../etc/passwd",
            "ART-../..",
            "ART-/etc/passwd",
            "ART-AAAA/",
            "ART-a.b",
            "ART-",
            "art-8kx2m",
            "",
        ] {
            assert!(validate_art_id_readable(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn consume_all_open_comments_closes_every_open_heading_and_skips_closed_ones() {
        let header = reviews_org_header("ART-XYZAB");
        let open_a = comment_org_block(&NewComment {
            cid: "CID-open0001",
            author: "a@t.com",
            version: 1,
            anchor: "{}",
            resolution_target: "",
            message: "one",
        });
        let open_b = comment_org_block(&NewComment {
            cid: "CID-open0002",
            author: "b@t.com",
            version: 1,
            anchor: "{}",
            resolution_target: "",
            message: "two",
        });
        // Pre-close one of the two the production way (regeneration close-out):
        // `consume_all_open_comments` marks it resolved+consumed. Applying it to
        // open_a alone, then concatenating the still-open open_b, yields the
        // "one closed, one open" fixture. There is no single-cid resolve+consume
        // helper — resolve (people axis) and consume (agent axis) are separate.
        let closed_a =
            String::from_utf8(consume_all_open_comments(&format!("{header}{open_a}")).unwrap())
                .unwrap();
        let content = format!("{closed_a}{open_b}");

        let open_before: Vec<_> = parse_comments(&content)
            .into_iter()
            .filter(|c| !c.resolved && !c.consumed)
            .map(|c| c.cid)
            .collect();
        assert_eq!(open_before, vec!["CID-open0002".to_string()]);

        let closed = String::from_utf8(consume_all_open_comments(&content).unwrap()).unwrap();
        let records = parse_comments(&closed);
        assert_eq!(records.len(), 2);
        assert!(records.iter().all(|c| c.resolved && c.consumed));
    }

    #[test]
    fn consume_all_open_comments_is_noop_on_empty_reviews() {
        assert_eq!(consume_all_open_comments("").unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn update_artifact_org_changes_version_and_state() {
        let org = artifact_org_content("ART-ABC", "My Title", &[], "prompt", 1, "submitted");
        let updated =
            String::from_utf8(update_artifact_org(&org, 2, "regenerating").unwrap()).unwrap();
        let file = OrgFile::parse(updated, "artifact.org").unwrap();
        let heading = file.headings.first().unwrap();
        assert_eq!(heading.property("VERSION"), Some("2"));
        assert_eq!(heading.property("STATE"), Some("regenerating"));
    }

    #[test]
    fn artifact_org_content_escapes_multiline_title_and_prompt() {
        let org = artifact_org_content(
            "ART-ABC",
            "Multi\nline title",
            &[],
            "Multi\r\nline\nprompt",
            1,
            "submitted",
        );
        // Escaping must produce a file the canonical parser accepts as a
        // single well-formed heading with no injected properties.
        let file = OrgFile::parse(org, "artifact.org").unwrap();
        assert_eq!(file.headings.len(), 1);
        let heading = &file.headings[0];
        assert_eq!(heading.property("TITLE"), Some("Multi line title"));
        assert_eq!(heading.property("PROMPT"), Some("Multi line prompt"));
    }

    #[test]
    fn load_project_artifacts_empty_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let summaries = load_project_artifacts(tmp.path());
        assert!(summaries.is_empty());
    }

    #[test]
    fn load_artifact_from_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let art_dir = tmp.path().join("ART-XYZAB");
        fs::create_dir_all(&art_dir).unwrap();
        let org_content = artifact_org_content(
            "ART-XYZAB",
            "Test Artifact",
            &["arch_ARSPJ".to_string()],
            "A test prompt",
            1,
            "submitted",
        );
        fs::write(art_dir.join("artifact.org"), &org_content).unwrap();
        fs::write(art_dir.join("artifact.mdx"), "<RichText>hello</RichText>\n").unwrap();

        let summary = load_artifact(&art_dir).unwrap();
        assert_eq!(summary.id, "ART-XYZAB");
        assert_eq!(summary.title, "Test Artifact");
        assert_eq!(summary.version, 1);
        assert_eq!(summary.state, "submitted");
        assert_eq!(summary.subject_nodes, vec!["arch_ARSPJ"]);
        assert_eq!(summary.open_comment_count, 0);
    }

    #[test]
    fn load_artifact_detail_distinguishes_missing_artifact_from_missing_version() {
        let tmp = tempfile::tempdir().unwrap();
        let art_dir = tmp.path().join("ART-XYZAB");
        fs::create_dir_all(&art_dir).unwrap();
        let org_content = artifact_org_content("ART-XYZAB", "Title", &[], "prompt", 1, "submitted");
        fs::write(art_dir.join("artifact.org"), &org_content).unwrap();
        fs::write(art_dir.join("artifact.mdx"), "<RichText>hi</RichText>\n").unwrap();

        let missing_artifact = tmp.path().join("ART-NOPE");
        assert!(matches!(
            load_artifact_detail(&missing_artifact, None, true).unwrap_err(),
            ArtifactLoadError::NotFound
        ));

        assert!(matches!(
            load_artifact_detail(&art_dir, Some(7), true).unwrap_err(),
            ArtifactLoadError::VersionNotFound(7)
        ));

        assert!(load_artifact_detail(&art_dir, None, true).is_ok());
    }

    #[test]
    fn load_artifact_detail_excludes_consumed_comments_by_default() {
        let tmp = tempfile::tempdir().unwrap();
        let art_dir = tmp.path().join("ART-XYZAB");
        fs::create_dir_all(&art_dir).unwrap();
        let org_content = artifact_org_content("ART-XYZAB", "Title", &[], "prompt", 1, "submitted");
        fs::write(art_dir.join("artifact.org"), &org_content).unwrap();
        fs::write(art_dir.join("artifact.mdx"), "<RichText>hi</RichText>\n").unwrap();

        let block = comment_org_block(&NewComment {
            cid: "CID-consumed01",
            author: "u@t.com",
            version: 1,
            anchor: "{}",
            resolution_target: "",
            message: "old",
        });
        let reviews = format!("{}{}", reviews_org_header("ART-XYZAB"), block);
        // Default view excludes CONSUMED (agent axis), not merely resolved, so
        // the fixture must be consumed via the production close-out path.
        let consumed = consume_all_open_comments(&reviews).unwrap();
        fs::write(
            art_dir.join("reviews.org"),
            String::from_utf8(consumed).unwrap(),
        )
        .unwrap();

        let default_view = load_artifact_detail(&art_dir, None, false).unwrap();
        assert!(default_view.comments.is_empty());

        let full_view = load_artifact_detail(&art_dir, None, true).unwrap();
        assert_eq!(full_view.comments.len(), 1);
        assert!(full_view.comments[0].consumed);
    }

    #[test]
    fn load_artifact_detail_scopes_comments_to_archived_version() {
        let tmp = tempfile::tempdir().unwrap();
        let art_dir = tmp.path().join("ART-XYZAB");
        fs::create_dir_all(&art_dir).unwrap();
        fs::create_dir_all(versions_dir(&art_dir)).unwrap();

        // Current/live version is 2; v1 is archived.
        let org_content = artifact_org_content("ART-XYZAB", "Title", &[], "prompt", 2, "submitted");
        fs::write(art_dir.join("artifact.org"), &org_content).unwrap();
        fs::write(
            art_dir.join("artifact.mdx"),
            "<RichText>v2 body</RichText>\n",
        )
        .unwrap();
        fs::write(
            versions_dir(&art_dir).join("v1.mdx"),
            "<RichText>v1 body</RichText>\n",
        )
        .unwrap();

        // v1's thread was consumed at regenerate close-out.
        let v1_block = comment_org_block(&NewComment {
            cid: "CID-v1consumed",
            author: "viewer@test.com",
            version: 1,
            anchor: "{}",
            resolution_target: "",
            message: "v1 thread comment",
        });
        let v1_only = format!("{}{}", reviews_org_header("ART-XYZAB"), v1_block);
        let v1_consumed = String::from_utf8(consume_all_open_comments(&v1_only).unwrap()).unwrap();

        // v2 has one fresh, still-open comment.
        let v2_block = comment_org_block(&NewComment {
            cid: "CID-v2open",
            author: "editor@test.com",
            version: 2,
            anchor: "{}",
            resolution_target: "",
            message: "v2 thread comment",
        });
        let mut reviews = v1_consumed;
        reviews.push_str(&v2_block);
        fs::write(art_dir.join("reviews.org"), &reviews).unwrap();

        // Archived view (version=1): only v1's own comment, returned despite
        // being consumed and despite include_consumed=false.
        let archived = load_artifact_detail(&art_dir, Some(1), false).unwrap();
        assert_eq!(archived.content.trim(), "<RichText>v1 body</RichText>");
        assert_eq!(archived.comments.len(), 1);
        assert_eq!(archived.comments[0].cid, "CID-v1consumed");
        assert!(archived.comments[0].consumed);

        // Current/live view (criterion 5 invariant): only the open v2
        // comment, v1's consumed comment never leaks in.
        let live = load_artifact_detail(&art_dir, None, false).unwrap();
        assert_eq!(live.comments.len(), 1);
        assert_eq!(live.comments[0].cid, "CID-v2open");
    }
}

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

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
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

fn versions_dir(art_dir: &Path) -> PathBuf {
    art_dir.join("versions")
}

// ── MDX block-registry validation ────────────────────────────────────────────

/// Scan `content` for JSX component names (PascalCase `<Tag`) and return one
/// error string per unknown type.  No external parser — just a byte scan.
pub fn validate_mdx(content: &str) -> Vec<String> {
    let known: HashSet<&str> = BLOCK_TYPES.iter().copied().collect();
    let mut seen_errors: HashSet<String> = HashSet::new();
    let mut errors: Vec<String> = Vec::new();
    let bytes = content.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'<' {
            i += 1;
            continue;
        }
        i += 1;
        // Skip closing tags `</`, comments `<!--`, and self-close `/>`.
        if i >= bytes.len() || bytes[i] == b'/' || bytes[i] == b'!' {
            continue;
        }
        // Read component name (alphanumeric + hyphens, like MDX JSX).
        let start = i;
        while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'-') {
            i += 1;
        }
        if i == start {
            continue;
        }
        let name = &content[start..i];
        // Only PascalCase names are JSX components; lowercase = HTML tags.
        if !name.as_bytes()[0].is_ascii_uppercase() {
            continue;
        }
        if !known.contains(name) && seen_errors.insert(name.to_string()) {
            errors.push(format!("unknown block type `{name}`"));
        }
    }
    errors
}

// ── Org property parsing (minimal, for our predictable format) ───────────────

fn parse_org_properties(content: &str) -> std::collections::HashMap<String, String> {
    let mut props = std::collections::HashMap::new();
    let mut in_drawer = false;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.eq_ignore_ascii_case(":PROPERTIES:") {
            in_drawer = true;
            continue;
        }
        if trimmed.eq_ignore_ascii_case(":END:") {
            in_drawer = false;
            continue;
        }
        if in_drawer {
            if let Some(rest) = trimmed.strip_prefix(':') {
                if let Some(colon_pos) = rest.find(':') {
                    let key = rest[..colon_pos].trim().to_uppercase();
                    let val = rest[colon_pos + 1..].trim().to_string();
                    props.insert(key, val);
                }
            }
        }
    }
    props
}

/// Extract the body text of a comment heading (everything after :END:).
fn comment_body(content: &str) -> String {
    let mut after_end = false;
    let mut lines: Vec<&str> = Vec::new();
    for line in content.lines() {
        if line.trim().eq_ignore_ascii_case(":END:") {
            after_end = true;
            continue;
        }
        if after_end {
            lines.push(line);
        }
    }
    // Trim leading/trailing blank lines
    while lines.first().map(|l| l.trim().is_empty()).unwrap_or(false) {
        lines.remove(0);
    }
    while lines.last().map(|l| l.trim().is_empty()).unwrap_or(false) {
        lines.pop();
    }
    lines.join("\n")
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

/// Rewrite artifact.org updating :VERSION: and :STATE: only.
pub fn update_artifact_org(current: &str, new_version: u32, new_state: &str) -> Result<Vec<u8>> {
    let mut out = String::with_capacity(current.len());
    for line in current.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with(":VERSION:") {
            out.push_str(&format!(":VERSION:      {new_version}\n"));
        } else if trimmed.starts_with(":STATE:") {
            out.push_str(&format!(":STATE:        {new_state}\n"));
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    Ok(out.into_bytes())
}

// ── reviews.org read/write ───────────────────────────────────────────────────

/// Initial content for a new reviews.org file (created before first comment).
pub fn reviews_org_header(art_id: &str) -> String {
    format!(
        "#+title: orgasmic artifact reviews {art_id}\n\
         #+orgasmic_version: 1\n"
    )
}

/// Org heading block for one comment, ready to append to reviews.org.
pub fn comment_org_block(
    cid: &str,
    author: &str,
    version: u32,
    anchor: &str,
    resolution_target: &str,
    message: &str,
) -> String {
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

/// Rewrite reviews.org marking `cid` as resolved + consumed.
pub fn resolve_comment_in_reviews(current: &str, cid: &str) -> Result<Vec<u8>> {
    // Find the heading for this CID and flip its RESOLVED + CONSUMED flags.
    let mut out = String::with_capacity(current.len());
    let mut in_target = false;
    let mut found = false;
    for line in current.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("* ") {
            // Entering a new heading — check if it's the target CID.
            in_target = trimmed.contains(cid);
            if in_target {
                found = true;
            }
            out.push_str(line);
            out.push('\n');
            continue;
        }
        if in_target {
            if trimmed.starts_with(":RESOLVED:") {
                out.push_str(":RESOLVED:         true\n");
                continue;
            }
            if trimmed.starts_with(":CONSUMED:") {
                out.push_str(":CONSUMED:         true\n");
                continue;
            }
        }
        out.push_str(line);
        out.push('\n');
    }
    if !found {
        bail!("comment {cid} not found in reviews.org");
    }
    Ok(out.into_bytes())
}

// ── index projection ─────────────────────────────────────────────────────────

/// Parse one CID heading block from a slice of reviews.org lines.
fn parse_comment_block(lines: &[&str]) -> Option<CommentRecord> {
    if lines.is_empty() {
        return None;
    }
    let block = lines.join("\n");
    let props = parse_org_properties(&block);
    let cid = props.get("CID")?.clone();
    if cid.is_empty() {
        return None;
    }
    let version = props
        .get("VERSION")
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(1);
    let resolved = props.get("RESOLVED").map(|v| v == "true").unwrap_or(false);
    let consumed = props.get("CONSUMED").map(|v| v == "true").unwrap_or(false);
    let message = comment_body(&block);
    Some(CommentRecord {
        cid,
        author: props.get("AUTHOR").cloned().unwrap_or_default(),
        version,
        anchor: props.get("ANCHOR").cloned().unwrap_or_else(|| "{}".into()),
        resolution_target: props.get("RESOLUTION_TARGET").cloned().unwrap_or_default(),
        resolved,
        consumed,
        message,
    })
}

/// Parse all comment records from reviews.org content.
pub fn parse_comments(content: &str) -> Vec<CommentRecord> {
    let mut records = Vec::new();
    let mut current_block: Vec<&str> = Vec::new();
    for line in content.lines() {
        if line.starts_with("* ") && !current_block.is_empty() {
            if let Some(rec) = parse_comment_block(&current_block) {
                records.push(rec);
            }
            current_block.clear();
        }
        current_block.push(line);
    }
    if !current_block.is_empty() {
        if let Some(rec) = parse_comment_block(&current_block) {
            records.push(rec);
        }
    }
    records
}

/// Load an ArtifactSummary from an ART-* directory.
pub fn load_artifact(art_dir: &Path) -> Option<ArtifactSummary> {
    let org_path = artifact_org_path(art_dir);
    let content = fs::read_to_string(&org_path).ok()?;
    let props = parse_org_properties(&content);

    let id = props.get("ID")?.clone();
    if id.is_empty() {
        return None;
    }
    let title = props.get("TITLE").cloned().unwrap_or_default();
    let subject_nodes_str = props.get("SUBJECT_NODES").cloned().unwrap_or_default();
    let subject_nodes: Vec<String> = subject_nodes_str
        .split_whitespace()
        .map(str::to_string)
        .collect();
    let version = props
        .get("VERSION")
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(1);
    let state = props
        .get("STATE")
        .cloned()
        .unwrap_or_else(|| "submitted".to_string());

    // Count open comments
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
pub fn load_artifact_detail(art_dir: &Path, version: Option<u32>) -> Option<ArtifactDetail> {
    let summary = load_artifact(art_dir)?;

    let org_path = artifact_org_path(art_dir);
    let org_content = fs::read_to_string(&org_path).ok()?;
    let props = parse_org_properties(&org_content);
    let prompt = props.get("PROMPT").cloned().unwrap_or_default();

    // MDX content: either current or a versioned archive
    let content = if let Some(v) = version {
        let versioned = versions_dir(art_dir).join(format!("v{v}.mdx"));
        fs::read_to_string(&versioned).ok()?
    } else {
        let mdx_path = artifact_mdx_path(art_dir);
        fs::read_to_string(&mdx_path).unwrap_or_default()
    };

    let reviews_path = reviews_org_path(art_dir);
    let comments = if let Ok(reviews_content) = fs::read_to_string(&reviews_path) {
        parse_comments(&reviews_content)
    } else {
        Vec::new()
    };

    Some(ArtifactDetail {
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

// ── write helpers (called from API handlers) ──────────────────────────────────

/// Initialize the artifact folder structure for a new artifact.
/// Returns an error if the directory already exists (caller should check first).
pub fn init_artifact_dir(art_dir: &Path) -> Result<()> {
    fs::create_dir_all(art_dir).context("create artifact dir")?;
    fs::create_dir_all(versions_dir(art_dir)).context("create versions dir")?;
    Ok(())
}

/// Archive the current artifact.mdx to versions/vN.mdx before overwriting.
/// Returns `None` if there is no current mdx to archive.
pub fn archive_current_mdx(art_dir: &Path, current_version: u32) -> Result<()> {
    let mdx_path = artifact_mdx_path(art_dir);
    if !mdx_path.exists() {
        return Ok(());
    }
    let archive_path = versions_dir(art_dir).join(format!("v{current_version}.mdx"));
    fs::copy(&mdx_path, &archive_path).context("archive mdx version")?;
    Ok(())
}

/// Build the `FileMutate` transform for appending a comment to reviews.org.
/// Returns (new_content_bytes, needs_init) where needs_init means the header
/// must be written first (macOS-safe: read current before deciding).
pub fn append_comment_transform(
    cid: String,
    author: String,
    version: u32,
    anchor: String,
    resolution_target: String,
    message: String,
) -> impl FnOnce(&str) -> Result<Vec<u8>> + Send + 'static {
    move |current: &str| {
        let mut out = if current.is_empty() {
            // Infer art_id from context — reviews.org header is written separately
            // when the file doesn't exist, but we handle both paths here.
            String::new()
        } else {
            current.to_string()
        };
        let block = comment_org_block(
            &cid,
            &author,
            version,
            &anchor,
            &resolution_target,
            &message,
        );
        out.push_str(&block);
        Ok(out.into_bytes())
    }
}

/// Generate a short unique comment ID.
pub fn new_cid() -> String {
    let id = uuid::Uuid::new_v4().to_string().replace('-', "");
    format!("CID-{}", &id[..8])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_registry_has_22_entries() {
        assert_eq!(BLOCK_TYPES.len(), 22);
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
        let mdx = "<Unknown /><Unknown /><Unknown />";
        assert_eq!(validate_mdx(mdx).len(), 1);
    }

    #[test]
    fn parse_org_properties_reads_drawer() {
        let content = "* ART-ABC My Title\n:PROPERTIES:\n:ID:           ART-ABC\n:VERSION:      3\n:STATE:        submitted\n:END:\n";
        let props = parse_org_properties(content);
        assert_eq!(props.get("ID").map(String::as_str), Some("ART-ABC"));
        assert_eq!(props.get("VERSION").map(String::as_str), Some("3"));
        assert_eq!(props.get("STATE").map(String::as_str), Some("submitted"));
    }

    #[test]
    fn parse_comments_round_trip() {
        let header = "#+title: reviews\n#+orgasmic_version: 1\n";
        let block = comment_org_block("CID-abc12345", "user@test.com", 1, "{}", "", "Good work.");
        let content = format!("{header}{block}");
        let records = parse_comments(&content);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].cid, "CID-abc12345");
        assert_eq!(records[0].author, "user@test.com");
        assert!(!records[0].resolved);
        assert_eq!(records[0].message, "Good work.");
    }

    #[test]
    fn resolve_comment_flips_flags() {
        let header = "#+title: reviews\n";
        let block = comment_org_block("CID-abc12345", "u@t.com", 1, "{}", "", "msg");
        let content = format!("{header}{block}");
        let updated =
            String::from_utf8(resolve_comment_in_reviews(&content, "CID-abc12345").unwrap())
                .unwrap();
        let records = parse_comments(&updated);
        assert_eq!(records.len(), 1);
        assert!(records[0].resolved);
        assert!(records[0].consumed);
    }

    #[test]
    fn resolve_comment_errors_on_unknown_cid() {
        let content = "#+title: reviews\n";
        assert!(resolve_comment_in_reviews(content, "CID-notexist").is_err());
    }

    #[test]
    fn update_artifact_org_changes_version_and_state() {
        let org = artifact_org_content("ART-ABC", "My Title", &[], "prompt", 1, "submitted");
        let updated =
            String::from_utf8(update_artifact_org(&org, 2, "regenerating").unwrap()).unwrap();
        let props = parse_org_properties(&updated);
        assert_eq!(props.get("VERSION").map(String::as_str), Some("2"));
        assert_eq!(props.get("STATE").map(String::as_str), Some("regenerating"));
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
}

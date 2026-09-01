//! Type-agnostic node kernel (dec_E01MC).
//!
//! A node is a directory `<collection>/<ID>/` holding `node.org` (the current
//! content: one heading plus drawer and body) and `journal.org` (append-only
//! entries in the tx drawer grammar with real prose bodies — AP971.1). This
//! module is the boundary the artifact store refactors onto: everything here
//! is ignorant of node type; type-specific extras are more files in the dir.
//!
//! Pure functions take and return file contents so the daemon writer keeps
//! owning every byte that reaches disk; the only fs call here is the mkdir
//! that IS the id-collision check (AP971.3).

use std::collections::HashSet;
use std::ops::Range;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::org::OrgFile;
use crate::tx::TxEntry;

pub const NODE_FILE: &str = "node.org";
pub const JOURNAL_FILE: &str = "journal.org";
pub const ORGASMIC_VERSION: u32 = 2;
pub const JOURNAL_SIZE_LINT_BYTES: usize = 500 * 1024;

/// `<project>/.orgasmic/<collection>/<id>/` — the id is the dir name, verbatim.
pub fn node_dir(project_root: &Path, collection: &str, id: &str) -> PathBuf {
    project_root.join(".orgasmic").join(collection).join(id)
}

/// Create the node directory. `create_dir` (not `create_dir_all`) on the leaf
/// is atomic on every platform we run on, so a collision surfaces as
/// `AlreadyExists` and the caller re-mints — no id registry.
pub fn create_node_dir(dir: &Path) -> std::io::Result<()> {
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::create_dir(dir)
}

pub fn node_org_header(kind_label: &str, id: &str) -> String {
    format!("#+title: orgasmic {kind_label} {id}\n#+orgasmic_version: {ORGASMIC_VERSION}\n\n")
}

pub fn journal_header(id: &str) -> String {
    format!("#+title: orgasmic journal {id}\n#+orgasmic_version: {ORGASMIC_VERSION}\n")
}

/// The current node: its one heading, read back generically.
#[derive(Debug, Clone)]
pub struct Node {
    pub id: String,
    pub title: String,
    pub state: Option<String>,
    pub properties: Vec<(String, String)>,
    /// Free prose between the drawer and the first `**` section.
    pub body: String,
}

pub fn parse_node(content: &str, display: &str) -> Result<Node> {
    let file = OrgFile::parse(content, display).context("parse node.org")?;
    let Some(heading) = file.headings.first() else {
        bail!("{display}: node.org has no heading");
    };
    if file.headings.len() > 1 {
        bail!("{display}: node.org must hold exactly one top-level heading");
    }
    let Some(id) = heading.property("ID") else {
        bail!("{display}: heading has no :ID:");
    };
    Ok(Node {
        id: id.to_string(),
        title: heading.title.clone(),
        state: heading.todo.clone(),
        properties: heading
            .property_entries()
            .map(|e| (e.key.clone(), e.value.clone()))
            .collect(),
        body: content[heading.body.clone()].to_string(),
    })
}

/// One journal entry: the tx drawer grammar plus a prose body (AP971.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalEntry {
    pub entry_id: String,
    pub time: String,
    pub ty: String,
    pub actor: String,
    pub machine: String,
    /// Every drawer key beyond the five required ones, in file order.
    pub extras: Vec<(String, String)>,
    pub body: String,
}

impl JournalEntry {
    pub fn extra(&self, key: &str) -> Option<&str> {
        self.extras
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
    /// A comment is open while it is neither consumed nor tombstoned.
    pub fn is_open_comment(&self) -> bool {
        self.ty == "comment" && self.extra("CONSUMED") != Some("true")
    }

    pub fn validate(&self) -> Result<()> {
        let mut seen: HashSet<&str> = REQUIRED.into_iter().collect();
        for (key, _) in &self.extras {
            if !seen.insert(key) {
                bail!(
                    "journal entry {} has duplicate property :{key}:",
                    self.entry_id
                );
            }
        }
        let mut tx = TxEntry::new(
            &self.entry_id,
            &self.ty,
            &self.time,
            &self.actor,
            &self.machine,
        );
        tx.extra = self.extras.clone();
        tx.validate().context("validate journal entry drawer")?;
        Ok(())
    }
}

const REQUIRED: [&str; 5] = ["TX_ID", "TIME", "TYPE", "ACTOR", "MACHINE"];

pub fn parse_journal(content: &str, display: &str) -> Result<Vec<JournalEntry>> {
    if content.trim().is_empty() {
        return Ok(Vec::new());
    }
    let file = OrgFile::parse(content, display).context("parse journal.org")?;
    let mut out = Vec::with_capacity(file.headings.len());
    for h in &file.headings {
        let mut keys = HashSet::new();
        for property in h.property_entries() {
            if !keys.insert(property.key.as_str()) {
                bail!(
                    "{display}: journal entry `{}` has duplicate property :{}:",
                    h.title,
                    property.key
                );
            }
        }
        let get = |k: &str| h.property(k).map(str::to_string);
        let (Some(entry_id), Some(time), Some(ty), Some(actor), Some(machine)) = (
            get("TX_ID"),
            get("TIME"),
            get("TYPE"),
            get("ACTOR"),
            get("MACHINE"),
        ) else {
            bail!(
                "{display}: journal entry `{}` lacks a required key",
                h.title
            );
        };
        let entry = JournalEntry {
            entry_id,
            time,
            ty,
            actor,
            machine,
            extras: h
                .property_entries()
                .filter(|e| !REQUIRED.contains(&e.key.as_str()))
                .map(|e| (e.key.clone(), e.value.clone()))
                .collect(),
            body: content[h.body.start..h.span.end].trim().to_string(),
        };
        entry.validate()?;
        out.push(entry);
    }
    Ok(out)
}

/// Render one entry. Column-0 `* ` inside prose is refused upstream (AP971.1
/// item 11) — this function trusts its input.
pub fn journal_entry_block(e: &JournalEntry) -> String {
    let mut s = format!(
        "\n* {id} {ty}\n:PROPERTIES:\n:TX_ID:   {id}\n:TIME:    {time}\n:TYPE:    {ty}\n:ACTOR:   {actor}\n:MACHINE: {machine}\n",
        id = e.entry_id,
        ty = e.ty,
        time = e.time,
        actor = e.actor,
        machine = e.machine
    );
    for (k, v) in &e.extras {
        s.push_str(&format!(":{k}: {v}\n"));
    }
    s.push_str(":END:\n");
    if !e.body.is_empty() {
        s.push('\n');
        s.push_str(&e.body);
        s.push('\n');
    }
    s
}

/// Append-only: facts and comments alike enter through here.
pub fn append_entry(current: &str, id: &str, entry: &JournalEntry) -> String {
    let mut out = if current.is_empty() {
        journal_header(id)
    } else {
        current.to_string()
    };
    out.push_str(&journal_entry_block(entry));
    out
}

/// Journals are deliberately unrotated in v1. Callers surface this low-severity
/// lint so real data can force a rotation design if 500 KiB proves too large.
pub fn journal_size_lint(content: &str) -> bool {
    content.len() > JOURNAL_SIZE_LINT_BYTES
}

/// A journal entry's body span, plus one `(key, value span)` per property in its
/// drawer. Byte ranges into the `journal.org` they were parsed from, so an edit
/// can splice in place without reserializing the file.
type EntrySpans = (Range<usize>, Vec<(String, Range<usize>)>);

fn entry_spans(content: &str, entry_id: &str) -> Result<EntrySpans> {
    let file = OrgFile::parse(content, JOURNAL_FILE).context("parse journal.org")?;
    let Some(h) = file
        .headings
        .iter()
        .find(|h| h.property("TX_ID") == Some(entry_id))
    else {
        bail!("journal entry {entry_id} not found");
    };
    Ok((
        h.body.start..h.span.end,
        h.property_entries()
            .map(|e| (e.key.clone(), e.value_span.clone()))
            .collect(),
    ))
}

fn comment_spans(content: &str, entry_id: &str) -> Result<EntrySpans> {
    let spans = entry_spans(content, entry_id)?;
    let Some((_, ty)) = spans.1.iter().find(|(key, _)| key == "TYPE") else {
        bail!("journal entry {entry_id} has no TYPE");
    };
    if &content[ty.clone()] != "comment" {
        bail!("journal entry {entry_id} is not an editable comment");
    }
    Ok(spans)
}

fn upsert_comment_property(
    content: &str,
    entry_id: &str,
    key: &str,
    value: &str,
) -> Result<String> {
    let (body, props) = comment_spans(content, entry_id)?;
    if let Some((_, span)) = props.iter().find(|(property, _)| property == key) {
        let mut out = content.to_string();
        out.replace_range(span.clone(), value);
        return Ok(out);
    }
    let end_line = content[..body.start].rfind(":END:").context("drawer end")?;
    let mut out = content.to_string();
    out.insert_str(end_line, &format!(":{key}: {value}\n"));
    Ok(out)
}

/// Authored prose is edited IN PLACE (AP971.1 item 7): one copy ever exists,
/// git holds the previous text. The caller supplies the audit stamps and
/// performs the OCC check before calling.
pub fn edit_comment_body(
    content: &str,
    entry_id: &str,
    new_body: &str,
    edited_by: &str,
    edited_at: &str,
) -> Result<String> {
    let stamped = upsert_comment_property(content, entry_id, "EDITED_BY", edited_by)?;
    let stamped = upsert_comment_property(&stamped, entry_id, "EDITED_AT", edited_at)?;
    let (body, _) = comment_spans(&stamped, entry_id)?;
    let mut out = String::with_capacity(stamped.len() + new_body.len());
    out.push_str(&stamped[..body.start]);
    out.push('\n');
    out.push_str(new_body.trim());
    out.push('\n');
    out.push_str(&stamped[body.end..]);
    Ok(out)
}

/// Delete = drop the body, leave the one-line tombstone so reply chains never
/// dangle (AP971.1 item 7).
pub fn tombstone_comment(
    content: &str,
    entry_id: &str,
    deleted_by: &str,
    deleted_at: &str,
) -> Result<String> {
    let stamped = upsert_comment_property(content, entry_id, "DELETED_BY", deleted_by)?;
    let stamped = upsert_comment_property(&stamped, entry_id, "DELETED_AT", deleted_at)?;
    let (body, props) = comment_spans(&stamped, entry_id)?;
    let ty = &props
        .iter()
        .find(|(key, _)| key == "TYPE")
        .expect("comment_spans checked TYPE")
        .1;
    let mut out = String::with_capacity(stamped.len());
    out.push_str(&stamped[..ty.start]);
    out.push_str("comment.deleted");
    out.push_str(&stamped[ty.end..body.start]);
    out.push_str(&stamped[body.end..]);
    Ok(out)
}

/// The regenerate hook (AP971.10): every open comment becomes consumed, and
/// the caller records the returned ids on its `regenerated` entry.
pub fn consume_open_comments(content: &str) -> Result<(String, Vec<String>)> {
    if content.trim().is_empty() {
        return Ok((content.to_string(), Vec::new()));
    }
    let file = OrgFile::parse(content, JOURNAL_FILE).context("parse journal.org")?;
    let mut edits: Vec<(Range<usize>, String)> = Vec::new();
    let mut consumed = Vec::new();
    for h in &file.headings {
        if h.property("TYPE") != Some("comment") || h.property("CONSUMED") == Some("true") {
            continue;
        }
        consumed.push(h.property("TX_ID").unwrap_or_default().to_string());
        match h.property_entries().find(|e| e.key == "CONSUMED") {
            Some(e) => edits.push((e.value_span.clone(), "true".into())),
            None => {
                let end_line = content[..h.body.start]
                    .rfind(":END:")
                    .context("drawer end")?;
                edits.push((end_line..end_line, ":CONSUMED: true\n".into()));
            }
        }
    }
    edits.sort_by_key(|(r, _)| r.start);
    let mut out = String::with_capacity(content.len());
    let mut cursor = 0;
    for (r, rep) in edits {
        out.push_str(&content[cursor..r.start]);
        out.push_str(&rep);
        cursor = r.end;
    }
    out.push_str(&content[cursor..]);
    Ok((out, consumed))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn comment(id: &str, body: &str) -> JournalEntry {
        JournalEntry {
            entry_id: id.into(),
            time: "[2026-08-22 Sat 13:00:00]".into(),
            ty: "comment".into(),
            actor: "owner".into(),
            machine: "mac".into(),
            extras: vec![],
            body: body.into(),
        }
    }

    #[test]
    fn journal_round_trips_and_consumes() {
        let j = append_entry("", "TASK-X", &comment("tx-1", "first\n\nmulti-line"));
        let j = append_entry(&j, "TASK-X", &comment("tx-2", "second"));
        let parsed = parse_journal(&j, JOURNAL_FILE).unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].body, "first\n\nmulti-line");
        assert!(parsed.iter().all(JournalEntry::is_open_comment));

        let j = edit_comment_body(
            &j,
            "tx-2",
            "second, edited",
            "editor",
            "[2026-08-22 Sat 13:01:00]",
        )
        .unwrap();
        let j = edit_comment_body(
            &j,
            "tx-2",
            "second, edited again",
            "second editor",
            "[2026-08-22 Sat 13:01:30]",
        )
        .unwrap();
        let parsed = parse_journal(&j, JOURNAL_FILE).unwrap();
        assert_eq!(parsed[1].body, "second, edited again");
        assert_eq!(parsed[1].extra("EDITED_BY"), Some("second editor"));
        assert_eq!(
            parsed[1].extra("EDITED_AT"),
            Some("[2026-08-22 Sat 13:01:30]")
        );
        assert_eq!(
            parsed[1]
                .extras
                .iter()
                .filter(|(key, _)| key == "EDITED_BY")
                .count(),
            1
        );
        assert_eq!(
            parsed[1]
                .extras
                .iter()
                .filter(|(key, _)| key == "EDITED_AT")
                .count(),
            1
        );
        assert_eq!(parsed[0].body, "first\n\nmulti-line", "sibling untouched");

        let j = tombstone_comment(&j, "tx-1", "deleter", "[2026-08-22 Sat 13:02:00]").unwrap();
        let (j, consumed) = consume_open_comments(&j).unwrap();
        assert_eq!(consumed, vec!["tx-2"]);
        let parsed = parse_journal(&j, JOURNAL_FILE).unwrap();
        assert!(parsed.iter().all(|e| !e.is_open_comment()));
        assert_eq!(parsed[0].ty, "comment.deleted");
        assert_eq!(
            parsed[0].body, "",
            "tombstone keeps the entry, drops the prose"
        );
        assert_eq!(parsed[0].extra("DELETED_BY"), Some("deleter"));
        assert_eq!(
            parsed[0].extra("DELETED_AT"),
            Some("[2026-08-22 Sat 13:02:00]")
        );
        // second consume is a no-op
        let (again, none) = consume_open_comments(&j).unwrap();
        assert_eq!(again, j);
        assert!(none.is_empty());

        assert!(!journal_size_lint(&"x".repeat(JOURNAL_SIZE_LINT_BYTES)));
        assert!(journal_size_lint(&"x".repeat(JOURNAL_SIZE_LINT_BYTES + 1)));
    }

    #[test]
    fn node_parses_heading_state_and_body() {
        let src = "#+title: orgasmic task TASK-A\n#+orgasmic_version: 2\n\n* DONE TASK-A Title here :tag:\n:PROPERTIES:\n:ID: TASK-A\n:PRIORITY: P1\n:END:\nfree prose\n\n** Description\nnested\n";
        let n = parse_node(src, NODE_FILE).unwrap();
        assert_eq!(
            (n.id.as_str(), n.state.as_deref()),
            ("TASK-A", Some("DONE"))
        );
        assert_eq!(n.body.trim(), "free prose");
        assert!(n
            .properties
            .iter()
            .any(|(k, v)| k == "PRIORITY" && v == "P1"));
    }

    #[test]
    fn create_node_dir_is_the_collision_check() {
        let tmp = std::env::temp_dir().join(format!("nk-{}", std::process::id()));
        let dir = node_dir(&tmp, "tasks", "TASK-ZZZZZ");
        create_node_dir(&dir).unwrap();
        assert_eq!(
            create_node_dir(&dir).unwrap_err().kind(),
            std::io::ErrorKind::AlreadyExists
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }
}

// arch: arch_BVH7M.2, arch_R3EPE.2
// orgasmic:arch_BVH7M, dec_WH9PD, dec_R75SW
//! Property-drawer-only tx heading writer.
//!
//! Tx files are append-only audit artifacts. Every entry is an Org top-level
//! heading immediately followed by a property drawer and nothing else. There
//! is no free body, no `** Description`, no EDN payload — see
//! [`arch_003`](../../../../.orgasmic/architecture.org) and `dec_006`.
//!
//! The writer opens the file in append mode and holds a single file handle
//! per [`TxWriter`] instance. Callers serialize access externally (the
//! daemon owns the writer); we don't acquire any locks at this layer because
//! the daemon's serialization guarantee is the authoritative invariant.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::org::{OrgError, OrgFile};

#[derive(Debug, Error)]
pub enum TxError {
    #[error("tx io: {0}")]
    Io(#[from] std::io::Error),
    #[error("tx parse: {0}")]
    Parse(#[from] OrgError),
    #[error("tx entry missing required field: {0}")]
    MissingField(&'static str),
    #[error("tx entry has trailing content after property drawer; file: {file}")]
    NonPropertyOnly { file: String },
}

/// One tx record, serialized to a property-drawer-only Org heading.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TxEntry {
    pub tx_id: String,
    /// Org timestamp, e.g. `[2026-05-21 Thu 19:35:16]`.
    pub time: String,
    pub ty: String,
    pub actor: String,
    pub machine: String,
    pub project: Option<String>,
    pub task: Option<String>,
    pub target: Option<String>,
    pub reason: Option<String>,
    /// Additional `:KEY: value` properties not covered above. Stored as
    /// (key, value) tuples to preserve insertion order; keys are written
    /// in the order they appear here.
    pub extra: Vec<(String, String)>,
}

impl TxEntry {
    pub fn new(
        tx_id: impl Into<String>,
        ty: impl Into<String>,
        time: impl Into<String>,
        actor: impl Into<String>,
        machine: impl Into<String>,
    ) -> Self {
        Self {
            tx_id: tx_id.into(),
            time: time.into(),
            ty: ty.into(),
            actor: actor.into(),
            machine: machine.into(),
            project: None,
            task: None,
            target: None,
            reason: None,
            extra: Vec::new(),
        }
    }

    /// Render the entry to a property-drawer-only Org heading. The output
    /// matches the column-aligned style used by existing `.orgasmic/tx/*.org`
    /// files: property values start at column 16.
    pub fn render(&self) -> String {
        let title_summary = match (&self.project, &self.task, &self.target) {
            (_, Some(t), _) => t.as_str(),
            (Some(p), _, _) => p.as_str(),
            (_, _, Some(t)) => t.as_str(),
            _ => "",
        };
        let title_time = strip_brackets(&self.time);
        let mut out = String::new();
        out.push_str(&format!(
            "* TX {} {} {}\n",
            title_time, self.ty, title_summary
        ));
        out.push_str(":PROPERTIES:\n");
        for (k, v) in self.ordered_properties() {
            if v.trim().is_empty() {
                out.push_str(&format!(":{}:\n", k));
                continue;
            }
            // `:KEY:` + spaces such that the value starts at column 16
            // (i.e. 15 characters of prefix). For keys longer than 13, we
            // fall back to a single space.
            let prefix_len = 2 + k.len();
            let pad = if prefix_len < 15 { 15 - prefix_len } else { 1 };
            out.push_str(&format!(":{}:{}{}\n", k, " ".repeat(pad), v));
        }
        out.push_str(":END:\n");
        out
    }

    fn ordered_properties(&self) -> Vec<(String, String)> {
        let mut v = vec![
            ("TX_ID".into(), self.tx_id.clone()),
            ("TIME".into(), self.time.clone()),
            ("TYPE".into(), self.ty.clone()),
            ("ACTOR".into(), self.actor.clone()),
            ("MACHINE".into(), self.machine.clone()),
        ];
        if let Some(p) = &self.project {
            v.push(("PROJECT".into(), p.clone()));
        }
        if let Some(t) = &self.task {
            v.push(("TASK".into(), t.clone()));
        }
        if let Some(t) = &self.target {
            v.push(("TARGET".into(), t.clone()));
        }
        if let Some(r) = &self.reason {
            v.push(("REASON".into(), r.clone()));
        }
        for (k, val) in &self.extra {
            v.push((k.clone(), val.clone()));
        }
        v
    }
}

fn strip_brackets(time: &str) -> String {
    time.trim_start_matches('[')
        .trim_end_matches(']')
        .to_string()
}

/// Append-only file writer for one tx file.
///
/// The writer holds one open file handle for its lifetime in `O_APPEND`
/// mode, so every write is atomic with respect to other appenders. Callers
/// serialize tx writes externally (the daemon owns the writer); the OS
/// append semantics protect against torn entries.
pub struct TxWriter {
    path: PathBuf,
    file: File,
    needs_leading_blank: bool,
}

impl TxWriter {
    /// Open `path` in append mode. If the file did not exist, seeds it with
    /// `#+title:` / `#+orgasmic_version:` keywords.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, TxError> {
        let path = path.as_ref().to_path_buf();
        let prior_len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let existed = prior_len > 0;
        let needs_leading_blank = if existed {
            !file_ends_with_blank_line(&path)?
        } else {
            false
        };
        let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
        if !existed {
            let basename = path.file_stem().and_then(|s| s.to_str()).unwrap_or("tx");
            writeln!(file, "#+title: orgasmic tx {basename}")?;
            writeln!(file, "#+orgasmic_version: 1")?;
        }
        Ok(Self {
            path,
            file,
            needs_leading_blank,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one tx entry.
    pub fn append(&mut self, entry: &TxEntry) -> Result<(), TxError> {
        if self.needs_leading_blank {
            self.file.write_all(b"\n")?;
        }
        self.file.write_all(b"\n")?;
        self.file.write_all(entry.render().as_bytes())?;
        self.file.flush()?;
        self.needs_leading_blank = false;
        Ok(())
    }
}

fn file_ends_with_blank_line(path: &Path) -> Result<bool, TxError> {
    let bytes = std::fs::read(path)?;
    if bytes.is_empty() {
        return Ok(true);
    }
    let n = bytes.len();
    if n >= 2 && bytes[n - 1] == b'\n' && bytes[n - 2] == b'\n' {
        return Ok(true);
    }
    Ok(bytes[n - 1] == b'\n' && n == 1)
}

/// Parse a tx file's contents into a sequence of [`TxEntry`] structs. Rejects
/// any heading that has body content beyond the property drawer.
pub fn parse_tx_file(source: &str, display: &str) -> Result<Vec<TxEntry>, TxError> {
    let file = OrgFile::parse(source, display)?;
    let mut entries = Vec::new();
    for heading in &file.headings {
        // Property-drawer-only: body must be empty (whitespace only) and the
        // heading must have no nested sections.
        let body_text = file.slice(heading.body.clone());
        if !body_text.trim().is_empty() || !heading.sections.is_empty() {
            return Err(TxError::NonPropertyOnly {
                file: display.into(),
            });
        }
        let entry = TxEntry {
            tx_id: heading
                .property("TX_ID")
                .ok_or(TxError::MissingField("TX_ID"))?
                .to_string(),
            time: heading
                .property("TIME")
                .ok_or(TxError::MissingField("TIME"))?
                .to_string(),
            ty: heading
                .property("TYPE")
                .ok_or(TxError::MissingField("TYPE"))?
                .to_string(),
            actor: heading
                .property("ACTOR")
                .ok_or(TxError::MissingField("ACTOR"))?
                .to_string(),
            machine: heading
                .property("MACHINE")
                .ok_or(TxError::MissingField("MACHINE"))?
                .to_string(),
            project: heading.property("PROJECT").map(str::to_string),
            task: heading.property("TASK").map(str::to_string),
            target: heading.property("TARGET").map(str::to_string),
            reason: heading.property("REASON").map(str::to_string),
            extra: heading
                .property_entries()
                .filter(|e| {
                    !matches!(
                        e.key.as_str(),
                        "TX_ID"
                            | "TIME"
                            | "TYPE"
                            | "ACTOR"
                            | "MACHINE"
                            | "PROJECT"
                            | "TASK"
                            | "TARGET"
                            | "REASON"
                    )
                })
                .map(|e| (e.key.clone(), e.value.clone()))
                .collect(),
        };
        entries.push(entry);
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_entry() -> TxEntry {
        let mut e = TxEntry::new(
            "tx-20260521-proj-0099",
            "manager.action",
            "[2026-05-21 Thu 21:00:00]",
            "dev@example.com",
            "host.local",
        );
        e.project = Some("orgasmic".into());
        e.task = Some("TASK-003".into());
        e.target = Some(".orgasmic/tasks/backlog.org".into());
        e.reason = Some("Recorded implementer.done.".into());
        e
    }

    #[test]
    fn whitespace_only_property_value_renders_without_trailing_whitespace() {
        let mut entry = sample_entry();
        entry.reason = Some("   ".into());
        let rendered = entry.render();
        let reason_line = rendered
            .lines()
            .find(|line| line.starts_with(":REASON:"))
            .expect("REASON property line");
        assert!(
            !reason_line.ends_with(' ') && !reason_line.ends_with('\t'),
            "whitespace-only REASON must not pad with trailing whitespace: {reason_line:?}"
        );
        assert_eq!(reason_line, ":REASON:");
    }

    #[test]
    fn empty_property_value_emits_no_trailing_whitespace() {
        let mut entry = sample_entry();
        entry.reason = Some(String::new());
        let rendered = entry.render();
        let reason_line = rendered
            .lines()
            .find(|line| line.starts_with(":REASON:"))
            .expect("REASON property line");
        assert!(
            !reason_line.ends_with(' ') && !reason_line.ends_with('\t'),
            "empty REASON must not pad with trailing whitespace: {reason_line:?}"
        );
        assert_eq!(reason_line, ":REASON:");
    }

    #[test]
    fn renders_property_drawer_only_heading() {
        let rendered = sample_entry().render();
        assert!(rendered.starts_with("* TX 2026-05-21 Thu 21:00:00 manager.action TASK-003\n"));
        assert!(rendered.contains(":TX_ID:        tx-20260521-proj-0099\n"));
        assert!(rendered.contains(":END:\n"));
        // No body lines beyond drawer.
        let lines: Vec<&str> = rendered.lines().collect();
        let end_idx = lines.iter().position(|l| *l == ":END:").unwrap();
        assert_eq!(end_idx, lines.len() - 1, "no content after :END:");
    }

    #[test]
    fn append_and_reparse_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("2026-05.org");
        let mut writer = TxWriter::open(&path).unwrap();
        writer.append(&sample_entry()).unwrap();
        let mut second = TxEntry::new(
            "tx-20260521-proj-0100",
            "task.state_transitioned",
            "[2026-05-21 Thu 21:05:00]",
            "dev@example.com",
            "host.local",
        );
        second.task = Some("TASK-003".into());
        second.extra.push(("FROM_STATE".into(), "ready".into()));
        second.extra.push(("TO_STATE".into(), "done".into()));
        writer.append(&second).unwrap();
        drop(writer);

        let source = std::fs::read_to_string(&path).unwrap();
        let entries = parse_tx_file(&source, "2026-05.org").unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].tx_id, "tx-20260521-proj-0099");
        assert_eq!(entries[1].tx_id, "tx-20260521-proj-0100");
        assert_eq!(
            entries[1].extra,
            vec![
                ("FROM_STATE".into(), "ready".into()),
                ("TO_STATE".into(), "done".into())
            ]
        );
    }

    #[test]
    fn parse_rejects_non_property_only_heading() {
        let src = "#+title: x\n\n* TX 2026-05-21 21:00:00 x.y\n:PROPERTIES:\n:TX_ID: a\n:TIME: t\n:TYPE: x.y\n:ACTOR: a\n:MACHINE: m\n:END:\n\nfree prose here\n";
        let err = parse_tx_file(src, "x.org").unwrap_err();
        match err {
            TxError::NonPropertyOnly { .. } => {}
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn fresh_file_seeds_keywords() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("2026-06.org");
        {
            let mut writer = TxWriter::open(&path).unwrap();
            writer.append(&sample_entry()).unwrap();
        }
        let source = std::fs::read_to_string(&path).unwrap();
        assert!(source.starts_with("#+title: orgasmic tx 2026-06\n#+orgasmic_version: 1\n"));
    }
}

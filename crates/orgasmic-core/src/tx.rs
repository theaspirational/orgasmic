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
//!
// orgasmic:task_HQ970
//! # Single-line values, and why there is no body region (TASK-HQ970)
//!
//! Every property value is a single line. TASK-HQ970 asked whether the entry
//! format should instead grow a body region for long prose; it does not, for
//! three reasons visible in this file:
//!
//! 1. Drawer-only is the declared format, not an accident of the writer —
//!    `arch_003`/`dec_006`, and [`parse_tx_file`] already *rejects* an entry
//!    with content after `:END:`. A body region would be a format change to
//!    an append-only artifact whose historical files could not be re-read by
//!    older readers.
//! 2. The `tx-scannable-bodies` convention that appeared to require bodies
//!    does not: what it calls a "tx body" is the value the UI feed renders,
//!    which the daemon derives from the `REASON` (or `BODY`) *property*, not
//!    from an Org body.
//! 3. Prose that genuinely does not fit on one line already has two homes —
//!    the node body (`orgasmic node body set`) and the comment surface, which
//!    escapes newlines into its `BODY` property rather than emitting them.
//!
//! So a multi-line value is refused ([`TxEntry::validate`]) and the
//! constraint is documented in `tx record --help`. Anything validation does
//! not anticipate is caught by [`TxEntry::assert_round_trip`] before a byte
//! is written: this ledger only accepts what it can read back.

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
    // orgasmic:task_HQ970
    #[error("tx property :{key}: {detail}")]
    InvalidValue { key: String, detail: String },
    // orgasmic:task_HQ970
    #[error("tx entry would not read back as written ({detail}); refusing to append")]
    RoundTripLoss { detail: String },
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

    // orgasmic:task_HQ970
    /// Reject, at the API boundary, any value this format cannot carry.
    ///
    /// A tx entry is a property drawer and nothing else, so every property has
    /// to fit on one line. A newline written verbatim into the drawer ends it
    /// early: the remaining properties land outside, and the append-only
    /// ledger stops parsing for *every* reader — including the verbs an
    /// operator would use to repair it. That is TASK-HQ970's reproduced brick,
    /// and this is the check that makes it unreachable.
    pub fn validate(&self) -> Result<(), TxError> {
        for (key, value) in self.ordered_properties() {
            validate_property_key(&key)?;
            validate_property_value(&key, &value)?;
        }
        Ok(())
    }

    // orgasmic:task_HQ970
    /// Compose, re-parse, compare: a ledger that only accepts what it can read
    /// back cannot be bricked by its own writer.
    ///
    /// Runs on the rendered entry before a single byte reaches the file, so
    /// anything [`Self::validate`] did not anticipate (a key that collides
    /// with the drawer terminator, a duplicate property that the reader
    /// collapses) is refused instead of committed.
    ///
    /// The comparison is about *loss*, not layout. Values are compared trimmed,
    /// because the reader trims property values by contract; and the property
    /// list is compared as a set, because the reader canonicalizes position —
    /// a caller that passes `REASON` through `extra` gets it back in the
    /// `reason` field, having written and read the identical drawer line.
    ///
    /// Shares its shape with `OrgRewriter::assert_body_round_trip`
    /// (TASK-ZYWZD) — the same guarantee on a surface that has a body.
    pub fn assert_round_trip(&self) -> Result<(), TxError> {
        let rendered = self.render();
        // Path-free display: this error reaches an API client.
        let parsed = parse_tx_file(&rendered, TX_ENTRY_DISPLAY)?;
        let stored = match parsed.as_slice() {
            [only] => only,
            other => {
                return Err(TxError::RoundTripLoss {
                    detail: format!(
                        "composed entry parsed as {} entries, expected 1",
                        other.len()
                    ),
                })
            }
        };
        let submitted = trimmed_properties(self);
        let read_back = trimmed_properties(stored);
        if submitted == read_back {
            return Ok(());
        }
        Err(TxError::RoundTripLoss {
            detail: describe_property_loss(&submitted, &read_back),
        })
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

// orgasmic:task_HQ970
/// Display name used when the writer re-parses its own composed entry. Never a
/// path: this string can reach an API client inside a parse error.
const TX_ENTRY_DISPLAY: &str = "<tx entry>";

// orgasmic:task_HQ970
/// Keys the drawer syntax itself owns. `:END:` as a property key would
/// terminate the drawer and push every later property out of it.
const RESERVED_PROPERTY_KEYS: [&str; 2] = ["END", "PROPERTIES"];

// orgasmic:task_HQ970
/// Where prose that does not fit on one line actually belongs. Appended to the
/// refusal so the operator's next move is obvious rather than a retry.
const PROSE_HINT: &str = "keep tx properties to a single line and put long prose in the node body \
                          (`orgasmic node body set`)";

// orgasmic:task_HQ970
/// The entry's properties as the reader compares them: values trimmed (the
/// reader trims by contract) and sorted (the reader canonicalizes position),
/// so the round-trip check reports loss rather than reordering.
fn trimmed_properties(entry: &TxEntry) -> Vec<(String, String)> {
    let mut pairs: Vec<(String, String)> = entry
        .ordered_properties()
        .into_iter()
        .map(|(key, value)| (key, value.trim().to_string()))
        .collect();
    pairs.sort();
    pairs
}

// orgasmic:task_HQ970
/// Name what the reader would not have given back. The refusal has to say
/// which property was lost, not just that something was.
fn describe_property_loss(
    submitted: &[(String, String)],
    read_back: &[(String, String)],
) -> String {
    for (key, value) in submitted {
        if read_back
            .iter()
            .any(|pair| pair == &(key.clone(), value.clone()))
        {
            continue;
        }
        return match read_back.iter().find(|(got_key, _)| got_key == key) {
            Some((_, got_value)) => format!(
                ":{key}: submitted {} characters, reads back {}",
                value.chars().count(),
                got_value.chars().count()
            ),
            None => format!(":{key}: is dropped by the reader"),
        };
    }
    match read_back
        .iter()
        .find(|pair| !submitted.iter().any(|got| got == *pair))
    {
        Some((key, _)) => format!(":{key}: is invented by the reader"),
        None => format!(
            "submitted {} properties, reads back {}",
            submitted.len(),
            read_back.len()
        ),
    }
}

fn validate_property_key(key: &str) -> Result<(), TxError> {
    let invalid = |detail: &str| TxError::InvalidValue {
        key: key.to_string(),
        detail: detail.to_string(),
    };
    if key.is_empty() {
        return Err(invalid("key must not be empty"));
    }
    if RESERVED_PROPERTY_KEYS.contains(&key) {
        return Err(invalid("key is reserved by the property drawer syntax"));
    }
    if let Some(ch) = key
        .chars()
        .find(|ch| *ch == ':' || ch.is_whitespace() || ch.is_control())
    {
        return Err(invalid(&format!(
            "key must not contain {:?}; a drawer key is `:KEY:` on one line",
            ch
        )));
    }
    Ok(())
}

fn validate_property_value(key: &str, value: &str) -> Result<(), TxError> {
    for ch in value.chars() {
        if ch == '\n' || ch == '\r' {
            return Err(TxError::InvalidValue {
                key: key.to_string(),
                detail: format!(
                    "value must be a single line, but it contains a {}. A tx entry is a \
                     property drawer and nothing else, so a newline ends the drawer and \
                     leaves the append-only ledger unparseable for every reader; {PROSE_HINT}.",
                    if ch == '\n' {
                        "line break"
                    } else {
                        "carriage return"
                    }
                ),
            });
        }
        // Tabs survive a drawer line intact; every other control character
        // does not survive a round trip through a line-oriented reader.
        if ch.is_control() && ch != '\t' {
            return Err(TxError::InvalidValue {
                key: key.to_string(),
                detail: format!(
                    "value must be a single line of printable text, but it contains the \
                     control character U+{:04X}; {PROSE_HINT}.",
                    ch as u32
                ),
            });
        }
    }
    Ok(())
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

    /// Sync appended bytes through the descriptor retained by this writer.
    ///
    /// Keeping the sync on the append descriptor avoids reopening `path`
    /// between the append and its durability acknowledgement. The pathname
    /// may no longer name this file by then, but the bytes just appended still
    /// belong to this descriptor and must be synced before the caller replies.
    pub fn sync_data(&self) -> Result<(), TxError> {
        self.file.sync_data()?;
        Ok(())
    }

    /// Append one tx entry.
    ///
    // orgasmic:task_HQ970
    /// The entry is validated and round-tripped through the reader *before*
    /// any byte is written (TASK-HQ970). This is the last line of defence for
    /// every write path — the project ledger, the `$ORGASMIC_HOME/state/tx`
    /// home ledger, and every daemon-internal caller — so a write the reader
    /// would choke on is refused instead of bricking the file.
    pub fn append(&mut self, entry: &TxEntry) -> Result<(), TxError> {
        entry.validate()?;
        entry.assert_round_trip()?;
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
    fn retained_descriptor_syncs_after_path_is_renamed_away() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("2026-08.org");
        let renamed = dir.path().join("2026-08.renamed.org");
        let mut writer = TxWriter::open(&path).unwrap();
        writer.append(&sample_entry()).unwrap();

        std::fs::rename(&path, &renamed).unwrap();
        assert!(!path.exists(), "the original pathname must stay absent");

        writer
            .sync_data()
            .expect("the retained append descriptor remains syncable");
        drop(writer);
        let source = std::fs::read_to_string(renamed).unwrap();
        assert_eq!(parse_tx_file(&source, "2026-08.org").unwrap().len(), 1);
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

    // -----------------------------------------------------------------
    // orgasmic:task_HQ970 — the ledger only accepts what it can read back
    // -----------------------------------------------------------------

    #[test]
    fn multi_line_value_is_rejected_naming_the_property_and_the_constraint() {
        let mut entry = sample_entry();
        entry.reason = Some("Dispatched implementer.\n\nSecond paragraph.".into());
        let err = entry.validate().unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("REASON"), "must name the property: {msg}");
        assert!(msg.contains("single line"), "must state the rule: {msg}");
        assert!(
            msg.contains("node body set"),
            "must say where long prose goes: {msg}"
        );
        assert!(matches!(err, TxError::InvalidValue { .. }), "{err:?}");
    }

    #[test]
    fn carriage_return_and_control_characters_are_rejected_too() {
        let mut entry = sample_entry();
        entry.reason = Some("first\rsecond".into());
        assert!(matches!(
            entry.validate().unwrap_err(),
            TxError::InvalidValue { .. }
        ));

        let mut entry = sample_entry();
        entry.extra.push(("NOTE".into(), "bell\u{7}here".into()));
        let msg = entry.validate().unwrap_err().to_string();
        assert!(msg.contains("NOTE"), "{msg}");
        assert!(msg.contains("U+0007"), "{msg}");
    }

    #[test]
    fn tab_inside_a_value_is_allowed_and_round_trips() {
        let mut entry = sample_entry();
        entry.reason = Some("before\tafter".into());
        entry.validate().unwrap();
        entry.assert_round_trip().unwrap();
    }

    #[test]
    fn multi_line_extra_value_is_rejected_naming_its_key() {
        let mut entry = sample_entry();
        entry
            .extra
            .push(("ARTIFACTS".into(), "one.md\ntwo.md".into()));
        let msg = entry.validate().unwrap_err().to_string();
        assert!(msg.contains("ARTIFACTS"), "must name the extra key: {msg}");
        assert!(msg.contains("single line"), "{msg}");
    }

    #[test]
    fn drawer_terminator_as_a_key_is_rejected() {
        let mut entry = sample_entry();
        entry.extra.push(("END".into(), "x".into()));
        let msg = entry.validate().unwrap_err().to_string();
        assert!(msg.contains("END"), "{msg}");
        assert!(msg.contains("reserved"), "{msg}");
    }

    #[test]
    fn key_carrying_a_colon_is_rejected() {
        let mut entry = sample_entry();
        entry.extra.push(("A:B".into(), "value".into()));
        assert!(matches!(
            entry.validate().unwrap_err(),
            TxError::InvalidValue { .. }
        ));
    }

    #[test]
    fn a_canonical_key_carried_in_extra_still_round_trips() {
        // The daemon's artifact generation-state revert does exactly this: it
        // leaves `reason` unset and passes REASON through `extra`. The reader
        // gives it back in the `reason` field — the same drawer line, a
        // different slot on the struct. That is canonicalization, not loss,
        // and refusing it would break a live write path.
        let mut entry = TxEntry::new(
            "pending",
            "artifact.generation.failed",
            "[2026-05-21 Thu 21:00:00]",
            "dev@example.com",
            "host.local",
        );
        entry.project = Some("orgasmic".into());
        entry.reason = None;
        entry.extra = vec![
            ("ARTIFACT_ID".into(), "ART-1".into()),
            ("RUN_ID".into(), "run-1".into()),
            ("RESTORED_STATE".into(), "failed".into()),
            ("REASON".into(), "transport unsupported".into()),
        ];
        entry.validate().unwrap();
        entry.assert_round_trip().unwrap();
    }

    #[test]
    fn duplicate_property_that_the_reader_collapses_fails_the_round_trip() {
        // Validation has no reason to object — every value is one printable
        // line — but the reader keeps the first `:REASON:` and drops the
        // second, so the entry does not read back as written.
        let mut entry = sample_entry();
        entry
            .extra
            .push(("REASON".into(), "a second reason".into()));
        entry.validate().unwrap();
        let err = entry.assert_round_trip().unwrap_err();
        assert!(matches!(err, TxError::RoundTripLoss { .. }), "{err:?}");
        assert!(
            err.to_string().contains("REASON"),
            "must name the lossy property: {err}"
        );
    }

    #[test]
    fn round_trip_errors_never_leak_a_path() {
        let mut entry = sample_entry();
        entry.extra.push(("REASON".into(), "duplicate".into()));
        let msg = entry.assert_round_trip().unwrap_err().to_string();
        assert!(!msg.contains('/'), "refusal must stay path-free: {msg}");
    }

    #[test]
    fn writer_refuses_a_multi_line_value_and_leaves_the_ledger_byte_identical() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("2026-07.org");
        let mut writer = TxWriter::open(&path).unwrap();
        writer.append(&sample_entry()).unwrap();
        let before = std::fs::read_to_string(&path).unwrap();

        let mut bad = sample_entry();
        bad.tx_id = "tx-20260521-proj-0100".into();
        bad.reason = Some("line one\n\nline two".into());
        let err = writer.append(&bad).unwrap_err();
        assert!(matches!(err, TxError::InvalidValue { .. }), "{err:?}");

        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(before, after, "a refused append must write nothing");
        // And the ledger still reads, which is the whole point.
        let entries = parse_tx_file(&after, "2026-07.org").unwrap();
        assert_eq!(entries.len(), 1);

        // The writer is still usable afterwards: a refusal is not a poisoned
        // handle, and the next supported write lands and still parses.
        let mut good = sample_entry();
        good.tx_id = "tx-20260521-proj-0101".into();
        good.reason = Some("Line one; line two.".into());
        writer.append(&good).unwrap();
        drop(writer);
        let source = std::fs::read_to_string(&path).unwrap();
        assert_eq!(parse_tx_file(&source, "2026-07.org").unwrap().len(), 2);
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

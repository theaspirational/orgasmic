//! Durable node extras. Payloads live outside the ledger; these Org records do not.
use crate::OrgFile;
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MediaAnchor {
    pub attachment: String,
    pub revision: String,
    pub start_ms: u64,
    pub end_ms: Option<u64>,
    #[serde(default)]
    pub label: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LinkRecord {
    pub id: String,
    pub source: String,
    pub target: String,
    pub kind: String,
    pub revision: u64,
    pub deleted: bool,
    pub anchors: Vec<MediaAnchor>,
    pub actor: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AttachmentRecord {
    pub id: String,
    pub node: String,
    pub name: String,
    pub revision: String,
    pub size: u64,
    pub media_type: String,
    pub actor: String,
    pub created_at: String,
}

fn property<'a>(h: &'a crate::org::Heading, key: &str) -> Result<&'a str> {
    h.property(key)
        .with_context(|| format!("missing {key} in node service record"))
}

pub fn read_links(source: &str) -> Result<Vec<LinkRecord>> {
    let file = OrgFile::parse(source, "links.org")?;
    file.headings
        .iter()
        .map(|h| {
            ensure!(property(h, "SCHEMA")? == "1", "unsupported link schema");
            Ok(LinkRecord {
                id: property(h, "ID")?.into(),
                source: property(h, "SOURCE")?.into(),
                target: property(h, "TARGET")?.into(),
                kind: property(h, "KIND")?.into(),
                revision: property(h, "REVISION")?.parse()?,
                deleted: property(h, "DELETED")? == "true",
                actor: property(h, "ACTOR")?.into(),
                updated_at: property(h, "UPDATED_AT")?.into(),
                anchors: h
                    .sections
                    .iter()
                    .map(|a| {
                        Ok(MediaAnchor {
                            attachment: property(a, "ATTACHMENT")?.into(),
                            revision: property(a, "REVISION")?.into(),
                            start_ms: property(a, "START_MS")?.parse()?,
                            end_ms: a.property("END_MS").map(str::parse).transpose()?,
                            label: a.property("LABEL").unwrap_or_default().into(),
                        })
                    })
                    .collect::<Result<_>>()?,
            })
        })
        .collect()
}

pub fn render_links(records: &[LinkRecord]) -> String {
    let mut out = "#+title: Node links\n".to_owned();
    for r in records {
        out.push_str(&format!("* Link\n:PROPERTIES:\n:SCHEMA: 1\n:ID: {}\n:SOURCE: {}\n:TARGET: {}\n:KIND: {}\n:REVISION: {}\n:DELETED: {}\n:ACTOR: {}\n:UPDATED_AT: {}\n:END:\n", r.id, r.source, r.target, r.kind, r.revision, r.deleted, r.actor, r.updated_at));
        for a in &r.anchors {
            out.push_str(&format!(
                "** Media anchor\n:PROPERTIES:\n:ATTACHMENT: {}\n:REVISION: {}\n:START_MS: {}\n",
                a.attachment, a.revision, a.start_ms
            ));
            if let Some(end) = a.end_ms {
                out.push_str(&format!(":END_MS: {end}\n"));
            }
            if !a.label.is_empty() {
                out.push_str(&format!(":LABEL: {}\n", a.label));
            }
            out.push_str(":END:\n");
        }
    }
    out
}

pub fn read_attachments(source: &str) -> Result<Vec<AttachmentRecord>> {
    let file = OrgFile::parse(source, "attachments.org")?;
    file.headings
        .iter()
        .map(|h| {
            ensure!(
                property(h, "SCHEMA")? == "1",
                "unsupported attachment schema"
            );
            Ok(AttachmentRecord {
                id: property(h, "ID")?.into(),
                node: property(h, "NODE")?.into(),
                name: property(h, "NAME")?.into(),
                revision: property(h, "REVISION")?.into(),
                size: property(h, "SIZE")?.parse()?,
                media_type: property(h, "MEDIA_TYPE")?.into(),
                actor: property(h, "ACTOR")?.into(),
                created_at: property(h, "CREATED_AT")?.into(),
            })
        })
        .collect()
}

pub fn render_attachments(records: &[AttachmentRecord]) -> String {
    let mut out = "#+title: Node attachments\n".to_owned();
    for r in records {
        out.push_str(&format!("* Attachment\n:PROPERTIES:\n:SCHEMA: 1\n:ID: {}\n:NODE: {}\n:NAME: {}\n:REVISION: {}\n:SIZE: {}\n:MEDIA_TYPE: {}\n:ACTOR: {}\n:CREATED_AT: {}\n:END:\n", r.id, r.node, r.name, r.revision, r.size, r.media_type, r.actor, r.created_at));
    }
    out
}

/// One range, inclusive start / exclusive end; suffix and open-ended ranges work.
pub fn byte_range(header: Option<&str>, len: u64) -> Result<(u64, u64)> {
    let Some(header) = header else {
        return Ok((0, len));
    };
    let (start, end) = header
        .strip_prefix("bytes=")
        .context("invalid range unit")?
        .split_once('-')
        .context("invalid byte range")?;
    ensure!(len > 0 && !end.contains(','), "unsatisfiable range");
    if start.is_empty() {
        let suffix: u64 = end.parse()?;
        ensure!(suffix > 0, "empty suffix range");
        return Ok((len.saturating_sub(suffix), len));
    }
    let start: u64 = start.parse()?;
    let end = if end.is_empty() {
        len
    } else {
        end.parse::<u64>()?.saturating_add(1).min(len)
    };
    ensure!(start < len && start < end, "unsatisfiable range");
    Ok((start, end))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn records_round_trip_and_ranges_are_bounded() {
        let link = LinkRecord {
            id: "link-1".into(),
            source: "MEET-1".into(),
            target: "TASK-1".into(),
            kind: "RELATES_TO".into(),
            revision: 1,
            deleted: false,
            anchors: vec![MediaAnchor {
                attachment: "asset-1".into(),
                revision: "hash".into(),
                start_ms: 123,
                end_ms: Some(456),
                label: "Discussion".into(),
            }],
            actor: "plugin:meetings".into(),
            updated_at: "now".into(),
        };
        assert_eq!(
            read_links(&render_links(std::slice::from_ref(&link))).unwrap(),
            vec![link]
        );
        let asset = AttachmentRecord {
            id: "asset-1".into(),
            node: "MEET-1".into(),
            name: "notes.wav".into(),
            revision: "hash".into(),
            size: 42,
            media_type: "audio/wav".into(),
            actor: "admin".into(),
            created_at: "now".into(),
        };
        assert_eq!(
            read_attachments(&render_attachments(std::slice::from_ref(&asset))).unwrap(),
            vec![asset]
        );
        assert_eq!(byte_range(Some("bytes=-5"), 20).unwrap(), (15, 20));
        assert_eq!(byte_range(Some("bytes=5-"), 20).unwrap(), (5, 20));
        assert_eq!(byte_range(Some("bytes=5-999"), 20).unwrap(), (5, 20));
        assert!(byte_range(Some("bytes=20-"), 20).is_err());
        assert!(byte_range(Some("bytes=0-1,3-4"), 20).is_err());
    }
}

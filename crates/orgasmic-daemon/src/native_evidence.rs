//! Explicit, disposable native evidence (TASK-VBSG2 / dec_PC6T2).
//! This module has no writer, supervisor, recovery, or task mutation handle.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, ensure, Context, Result};
use orgasmic_core::session::bound_driver_event_payload;
use orgasmic_core::{SessionEnvelope, SessionEventKind};
use orgasmic_drivers::{
    find_native_transcript, lookup_from_envelopes, TranscriptConfidence, TranscriptFindResult,
    TranscriptRoots,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

pub(crate) const CONVERTER_VERSION: u32 = 1;
pub(crate) const SOURCE_LIMIT: usize = 64 * 1024 * 1024;
const LINE_LIMIT: usize = 2 * 1024 * 1024;
const EVENT_LIMIT: usize = 16 * 1024;
pub(crate) const CACHE_LIMIT: usize = 8 * 1024 * 1024;
const EVENT_COUNT_LIMIT: usize = 10_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceSummary {
    pub source_bytes: usize,
    pub source_records: usize,
    pub retained_records: usize,
    pub omitted_records: usize,
    pub bounded_payloads: u64,
    pub events: BTreeMap<String, usize>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DerivedRecord {
    source_line: usize,
    metadata: Value,
    events: Vec<Value>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DerivedEvidence {
    provenance: String,
    converter_version: u32,
    run_id: String,
    native_session_id: String,
    source_sha256: String,
    summary: EvidenceSummary,
    records: Vec<DerivedRecord>,
}

#[derive(Debug, Serialize)]
pub struct MaterializedEvidence {
    pub provenance: &'static str,
    pub run_id: String,
    pub converter_version: u32,
    pub source_sha256: String,
    pub reference: PathBuf,
    pub cache_hit: bool,
    pub summary: EvidenceSummary,
}

pub(crate) fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub(crate) fn read_bounded(path: &Path, limit: usize) -> Result<Vec<u8>> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options
        .open(path)
        .with_context(|| format!("open {}", path.display()))?;
    let before = file.metadata()?;
    ensure!(before.is_file(), "evidence source is not a regular file");
    ensure!(
        before.len() <= limit as u64,
        "evidence source exceeds {limit} bytes"
    );
    let mut bytes = Vec::new();
    (&file).take(limit as u64 + 1).read_to_end(&mut bytes)?;
    let after = file.metadata()?;
    ensure!(
        bytes.len() <= limit
            && before.len() == after.len()
            && before.modified()? == after.modified()?,
        "evidence source changed while reading or exceeded its byte limit"
    );
    Ok(bytes)
}

fn parse_lines(bytes: &[u8]) -> Result<impl Iterator<Item = Result<(usize, Value)>> + '_> {
    ensure!(
        bytes.is_empty() || bytes.ends_with(b"\n"),
        "incomplete final JSONL record; retry after the writer finishes"
    );
    Ok(bytes
        .split(|b| *b == b'\n')
        .enumerate()
        .filter(|(_, line)| !line.iter().all(u8::is_ascii_whitespace))
        .map(|(index, line)| {
            ensure!(
                line.len() <= LINE_LIMIT,
                "native record exceeds {LINE_LIMIT} bytes"
            );
            Ok((
                index + 1,
                serde_json::from_slice(line)
                    .with_context(|| format!("JSONL line {}", index + 1))?,
            ))
        }))
}

pub(crate) fn cache_dir(project_root: &Path, run_id: &str) -> Result<PathBuf> {
    let mut path = project_root.canonicalize()?;
    for part in [
        ".orgasmic",
        "tmp",
        "retro",
        "evidence",
        &digest(run_id.as_bytes()),
    ] {
        path.push(part);
        match std::fs::create_dir(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e.into()),
        }
        ensure!(
            std::fs::symlink_metadata(&path)?.is_dir(),
            "derived evidence directory must not be a symlink or file"
        );
    }
    Ok(path)
}

/// Materialize only this registered project's named run. The native source is
/// resolved from recorded lifecycle metadata, never from a caller-supplied path.
pub async fn materialize(
    project_root: &Path,
    run_id: &str,
    session_path: &Path,
    roots: &TranscriptRoots,
) -> Result<MaterializedEvidence> {
    let sessions = orgasmic_core::project_sessions_dir(project_root).canonicalize()?;
    ensure!(
        session_path.canonicalize()?.starts_with(sessions),
        "operational source is outside project sessions"
    );
    let (native_id, source) = verified_source(run_id, session_path, roots)?;
    materialize_source(project_root, run_id, native_id, source).await
}

pub(crate) fn verified_source(
    run_id: &str,
    session_path: &Path,
    roots: &TranscriptRoots,
) -> Result<(String, Vec<u8>)> {
    let session_bytes = read_bounded(session_path, SOURCE_LIMIT)?;
    let envelopes: Vec<SessionEnvelope> = parse_lines(&session_bytes)?
        .map(|line| Ok(serde_json::from_value(line?.1)?))
        .collect::<Result<_>>()?;
    let envelopes: Vec<_> = envelopes
        .into_iter()
        .filter(|e| e.run_id == run_id)
        .collect();
    let mut lookup =
        lookup_from_envelopes(&envelopes).context("run has no native correlation metadata")?;
    let mut identities = BTreeSet::new();
    let mut paths = BTreeSet::new();
    for e in &envelopes {
        if e.kind != SessionEventKind::Lifecycle || e.event["phase"] != "native_runtime" {
            continue;
        }
        let provider = e.event["provider"]
            .as_str()
            .context("native provider missing")?;
        let id = e.event["session_id"]
            .as_str()
            .context("native session id missing")?;
        ensure!(!id.is_empty(), "native session id is empty");
        identities.insert((provider.to_string(), id.to_string()));
        if let Some(path) = e.event["session_path"].as_str() {
            paths.insert(path.to_string());
        }
    }
    ensure!(
        identities.len() == 1 && paths.len() <= 1,
        "native correlation is missing or ambiguous"
    );
    let (provider, native_id) = identities.into_iter().next().unwrap();
    ensure!(
        provider == "claude",
        "native evidence converter supports Claude only"
    );
    lookup.harness = provider;
    lookup.session_id = Some(native_id.clone());
    let hit = match find_native_transcript(&lookup, roots) {
        TranscriptFindResult::Found(hit) if hit.confidence == TranscriptConfidence::High => hit,
        result => bail!("native transcript is not uniquely verified: {result:?}"),
    };
    let source = read_bounded(&hit.path, SOURCE_LIMIT)?;
    let mut correlated = false;
    for line in parse_lines(&source)? {
        let (_, raw) = line?;
        // Native JSONL's sessionId is distinct from its API session_id.
        if let Some(id) = raw.get("sessionId") {
            ensure!(
                id.as_str() == Some(native_id.as_str()),
                "native transcript contains a foreign or ambiguous sessionId"
            );
            correlated = true;
        }
    }
    ensure!(correlated, "native transcript has no matching sessionId");
    Ok((native_id, source))
}

pub(crate) async fn materialize_source(
    project_root: &Path,
    run_id: &str,
    native_id: String,
    source: Vec<u8>,
) -> Result<MaterializedEvidence> {
    let source_sha256 = digest(&source);
    let dir = cache_dir(project_root, run_id)?;
    let reference = dir.join(format!("claude-v{CONVERTER_VERSION}-{source_sha256}.json"));
    let finish = |cached: DerivedEvidence, cache_hit| -> Result<MaterializedEvidence> {
        ensure!(
            cached.provenance == "derived_native"
                && cached.converter_version == CONVERTER_VERSION
                && cached.run_id == run_id
                && cached.native_session_id == native_id
                && cached.source_sha256 == source_sha256,
            "derived cache identity mismatch"
        );
        Ok(MaterializedEvidence {
            provenance: "derived_native",
            run_id: run_id.into(),
            converter_version: CONVERTER_VERSION,
            source_sha256: source_sha256.clone(),
            reference: reference.clone(),
            cache_hit,
            summary: cached.summary,
        })
    };
    match read_bounded(&reference, CACHE_LIMIT) {
        Ok(bytes) => {
            return finish(
                serde_json::from_slice(&bytes).context("corrupt derived cache")?,
                true,
            )
        }
        Err(e)
            if e.downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) => {}
        Err(e) => return Err(e),
    }
    let mut summary = EvidenceSummary {
        source_bytes: source.len(),
        source_records: 0,
        retained_records: 0,
        omitted_records: 0,
        bounded_payloads: 0,
        events: BTreeMap::new(),
    };
    let mut adapter = orgasmic_drivers::adapters::ClaudeAdapter::new();
    let mut records = Vec::new();
    let mut retained_bytes = 0;
    let mut retained_events = 0;
    for raw in parse_lines(&source)? {
        let (source_line, raw) = raw?;
        summary.source_records += 1;
        if let Some(timestamp) = raw.get("timestamp").and_then(Value::as_str) {
            chrono::DateTime::parse_from_rfc3339(timestamp).context("invalid native timestamp")?;
        }
        let mut metadata = json!({});
        for key in [
            "timestamp",
            "uuid",
            "parentUuid",
            "isSidechain",
            "subtype",
            "durationMs",
            "compactMetadata",
            "stopReason",
            "preventedContinuation",
        ] {
            if let Some(v) = raw.get(key) {
                metadata[key] = v.clone();
            }
        }
        for key in [
            "id",
            "usage",
            "model",
            "stop_reason",
            "context_management",
            "context_window",
        ] {
            if let Some(v) = raw
                .pointer(&format!("/message/{key}"))
                .or_else(|| raw.get(key))
            {
                metadata[key] = v.clone();
            }
        }
        let bounded = bound_driver_event_payload(metadata, 2048);
        summary.bounded_payloads += bounded.bounded_payloads;
        let mut events = Vec::new();
        for event in adapter.native_evidence_events(&raw).await? {
            let bounded = bound_driver_event_payload(serde_json::to_value(event)?, 2048);
            ensure!(
                serde_json::to_vec(&bounded.value)?.len() <= EVENT_LIMIT,
                "derived event exceeds {EVENT_LIMIT} bytes"
            );
            summary.bounded_payloads += bounded.bounded_payloads;
            events.push(bounded.value);
        }
        let record = DerivedRecord {
            source_line,
            metadata: bounded.value,
            events,
        };
        let size = serde_json::to_vec(&record)?.len();
        if retained_bytes + size > CACHE_LIMIT / 2
            || retained_events + record.events.len() > EVENT_COUNT_LIMIT
        {
            summary.omitted_records += 1;
            continue;
        }
        for event in &record.events {
            *summary
                .events
                .entry(event["type"].as_str().unwrap_or("unknown").into())
                .or_default() += 1;
        }
        retained_bytes += size;
        retained_events += record.events.len();
        records.push(record);
    }
    summary.retained_records = records.len();
    let cache = DerivedEvidence {
        provenance: "derived_native".into(),
        converter_version: CONVERTER_VERSION,
        run_id: run_id.into(),
        native_session_id: native_id.clone(),
        source_sha256: source_sha256.clone(),
        summary,
        records,
    };
    let bytes = serde_json::to_vec(&cache)?;
    ensure!(
        bytes.len() <= CACHE_LIMIT,
        "derived cache exceeds {CACHE_LIMIT} bytes"
    );
    let temporary = dir.join(format!(".{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        match std::fs::hard_link(&temporary, &reference) {
            Ok(()) => {
                File::open(&dir)?.sync_all()?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                ensure!(
                    read_bounded(&reference, CACHE_LIMIT)? == bytes,
                    "concurrent derived cache differs"
                );
            }
            Err(e) => return Err(e.into()),
        }
        Ok(())
    })();
    let _ = std::fs::remove_file(temporary);
    result?;
    finish(cache, false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use orgasmic_core::{RuntimeIdentity, SessionWriter};

    fn fixture() -> (
        tempfile::TempDir,
        PathBuf,
        PathBuf,
        TranscriptRoots,
        PathBuf,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("project");
        let session = root.join(".orgasmic/tmp/sessions/run.jsonl");
        std::fs::create_dir_all(session.parent().unwrap()).unwrap();
        let roots = TranscriptRoots::from_home(dir.path().join("vendor-home"));
        std::fs::create_dir_all(&roots.claude_projects).unwrap();
        let native = roots.claude_projects.join("native.jsonl");
        let mut writer =
            SessionWriter::open(&session, RuntimeIdentity::new("run-evidence", "boot-test"))
                .unwrap();
        writer.append(SessionEventKind::Lifecycle, json!({"phase":"run_meta","transport":"stdio","harness":"claude","worktree":root,"driver_config":{}})).unwrap();
        writer.append(SessionEventKind::Lifecycle, json!({"phase":"native_runtime","provider":"claude","session_id":"native-id","session_path":native,"launch_argv":[],"resume_argv":[]})).unwrap();
        (dir, root, session, roots, native)
    }

    #[tokio::test]
    async fn materializer_is_explicit_bounded_idempotent_and_non_authoritative() {
        let (_dir, root, session, roots, native) = fixture();
        let before = std::fs::read(&session).unwrap();
        let assistant = json!({"type":"assistant","sessionId":"native-id","session_id":"different-api-id","uuid":"message-1","timestamp":"2026-09-07T12:00:00Z","message":{"id":"m1","usage":{"input_tokens":10,"output_tokens":4,"cache_read_input_tokens":3},"content":[{"type":"tool_use","id":"call-1","name":"Read","input":{"path":"src/lib.rs"}},{"type":"text","text":"x".repeat(100_000)}],"stop_reason":"tool_use"}});
        let result = json!({"type":"user","sessionId":"native-id","timestamp":"2026-09-07T12:00:02Z","message":{"content":[{"type":"tool_result","tool_use_id":"call-1","content":"ok"}]}});
        let terminal =
            json!({"type":"result","sessionId":"native-id","is_error":false,"result":"finished"});
        std::fs::write(&native, format!("{assistant}\n\n{result}\n{terminal}\n")).unwrap();
        let first = materialize(&root, "run-evidence", &session, &roots)
            .await
            .unwrap();
        assert!(!first.cache_hit);
        assert_eq!(first.summary.events["tool_call"], 1);
        assert_eq!(first.summary.events["tool_result"], 1);
        assert_eq!(first.summary.events["run_complete"], 1);
        assert!(first.summary.bounded_payloads > 0);
        let cache_bytes = std::fs::read(&first.reference).unwrap();
        let cache: DerivedEvidence = serde_json::from_slice(&cache_bytes).unwrap();
        assert_eq!(cache.records[1].source_line, 3);
        assert_eq!(cache.records[0].metadata["usage"]["input_tokens"], 10);
        assert_eq!(
            cache.records[1].metadata["timestamp"],
            "2026-09-07T12:00:02Z"
        );
        for record in &cache.records {
            assert!(serde_json::from_value::<SessionEnvelope>(
                serde_json::to_value(record).unwrap()
            )
            .is_err());
            for event in &record.events {
                assert!(serde_json::to_vec(event).unwrap().len() <= EVENT_LIMIT);
            }
        }
        let second = materialize(&root, "run-evidence", &session, &roots)
            .await
            .unwrap();
        assert!(second.cache_hit);
        assert_eq!(first.reference, second.reference);
        assert_eq!(std::fs::read(&first.reference).unwrap(), cache_bytes);
        OpenOptions::new()
            .append(true)
            .open(&native)
            .unwrap()
            .write_all(b"{\"type\":\"custom-title\",\"sessionId\":\"native-id\"}\n")
            .unwrap();
        let changed = materialize(&root, "run-evidence", &session, &roots)
            .await
            .unwrap();
        assert!(!changed.cache_hit);
        assert_ne!(changed.source_sha256, first.source_sha256);
        assert_ne!(changed.reference, first.reference);
        assert_eq!(std::fs::read(session).unwrap(), before);
        assert!(
            serde_json::to_vec(&first).unwrap().len() < 4096,
            "response is a summary, not transcript"
        );
    }

    #[tokio::test]
    async fn materializer_enforces_run_budget_and_rejects_cache_and_path_corruption() {
        let (_dir, root, session, roots, native) = fixture();
        let row = json!({"type":"assistant","sessionId":"native-id","message":{"content":[{"type":"tool_use","id":"call","name":"Read","input":{"path":"file"}}]}});
        std::fs::write(&native, format!("{row}\n").repeat(EVENT_COUNT_LIMIT + 1)).unwrap();
        let result = materialize(&root, "run-evidence", &session, &roots)
            .await
            .unwrap();
        assert_eq!(result.summary.retained_records, EVENT_COUNT_LIMIT);
        assert_eq!(result.summary.omitted_records, 1);
        assert!(std::fs::metadata(&result.reference).unwrap().len() <= CACHE_LIMIT as u64);
        std::fs::write(&result.reference, "{}\n").unwrap();
        assert!(materialize(&root, "run-evidence", &session, &roots)
            .await
            .unwrap_err()
            .to_string()
            .contains("corrupt derived cache"));
        std::fs::remove_file(&result.reference).unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&native, &result.reference).unwrap();
            assert!(materialize(&root, "run-evidence", &session, &roots)
                .await
                .is_err());
        }
        std::fs::OpenOptions::new()
            .write(true)
            .open(&native)
            .unwrap()
            .set_len(SOURCE_LIMIT as u64 + 1)
            .unwrap();
        assert!(materialize(&root, "run-evidence", &session, &roots)
            .await
            .unwrap_err()
            .to_string()
            .contains("exceeds"));
    }

    #[tokio::test]
    async fn materializer_refuses_uncorrelated_ambiguous_and_oversized_sources() {
        let (_dir, root, session, roots, native) = fixture();
        for body in [
            "{\"type\":\"user\",\"sessionId\":\"foreign\"}\n",
            "{\"type\":\"user\"}\n",
            "{\"sessionId\":\"native-id\"}",
            "{bad json}\n",
        ] {
            std::fs::write(&native, body).unwrap();
            assert!(materialize(&root, "run-evidence", &session, &roots)
                .await
                .is_err());
        }
        std::fs::write(&native, format!("{}\n", "x".repeat(LINE_LIMIT + 1))).unwrap();
        assert!(materialize(&root, "run-evidence", &session, &roots)
            .await
            .unwrap_err()
            .to_string()
            .contains("exceeds"));
        std::fs::write(&native, "{\"sessionId\":\"native-id\"}\n").unwrap();
        let mut writer =
            SessionWriter::open(&session, RuntimeIdentity::new("run-evidence", "boot-test"))
                .unwrap();
        writer.append(SessionEventKind::Lifecycle,json!({"phase":"native_runtime","provider":"claude","session_id":"different-native-id","launch_argv":[],"resume_argv":[]})).unwrap();
        assert!(materialize(&root, "run-evidence", &session, &roots)
            .await
            .unwrap_err()
            .to_string()
            .contains("ambiguous"));
    }
}

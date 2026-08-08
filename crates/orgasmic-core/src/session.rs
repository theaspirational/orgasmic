// orgasmic:arch_BVH7M, arch_A53QX, dec_R75SW
//! Append-only JSONL session writer.
//!
//! One file descriptor per run. Each line is a self-contained JSON object.
//! No compression in v0.0.1 (`dec_006`). Schema is intentionally loose at
//! this layer: the worker drivers serialize their own native events into
//! the `event` field; the daemon only enforces the envelope.
//!
//! Driver event vocabulary ([`DriverEvent`], [`Lifecycle`], [`BabysitterTool`])
//! is shared between `orgasmic-drivers` and `orgasmic-daemon::supervisor` so
//! the supervisor can persist driver-emitted events as well-typed payloads
//! without each driver duplicating the JSON envelope shape (`arch_004`).

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Error)]
pub enum SessionError {
    #[error("session io: {0}")]
    Io(#[from] std::io::Error),
    #[error("session serialize: {0}")]
    Serialize(#[from] serde_json::Error),
    #[error("invalid run sub-state {0}")]
    InvalidRunSubState(String),
    /// orgasmic:TASK-FZB6T.2 finding 5 — a pane transport offered rendered
    /// output to the session file. Refused, not bounded: a bound turns an
    /// 8 MiB repaint into a small line, and a million small lines is the same
    /// 2.239 GiB by another route.
    #[error(
        "refused a rendered pane payload: transport {transport} renders into a pane, so its \
         `text_chunk` is screen repaint, not evidence. A pane transport persists a \
         PaneActivity byte count and nothing else (dec_WDR5K item 7)."
    )]
    RenderedPanePayloadRefused { transport: String },
}

/// Whether a run's transport renders into a pane rather than streaming
/// structured turn events.
///
/// The whole difference between a `text_chunk` that is a screen repaint and a
/// `text_chunk` that is the assistant's actual words. A pane transport
/// (rmux/tmux) has no other channel, so its `text_chunk` is rendered TUI output
/// and forbidden storage; a structured transport's `text_chunk` is the model's or
/// a subprocess's content, which is evidence.
///
/// Lives here because [`SessionWriter`] is the choke point that has to act on
/// it; `orgasmic-daemon` re-exports rather than re-implements it.
pub fn transport_is_pane(transport: &str) -> bool {
    matches!(transport.trim(), "rmux" | "tmux" | "tmux-tui")
}

/// One session JSONL line.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionEnvelope {
    pub seq: u64,
    pub time: DateTime<Utc>,
    pub run_id: String,
    pub runtime_id: String,
    pub boot_id: String,
    pub kind: SessionEventKind,
    pub event: Value,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionEventKind {
    /// Driver-native event payload (Claude stream-json, Codex app-server, etc.).
    DriverEvent,
    /// Lifecycle event (acquire, attach, release, transition, etc.).
    Lifecycle,
    /// Babysitter summary chunk handed to a stall detector.
    BabysitterSummary,
    /// Free-form note written by a supervisor or recovery path.
    Note,
}

/// Identity tuple used to disambiguate cleanup from a replacement runtime
/// after restart (`arch_010`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RuntimeIdentity {
    pub run_id: String,
    pub runtime_id: String,
    pub boot_id: String,
}

impl RuntimeIdentity {
    pub fn new(run_id: impl Into<String>, boot_id: impl Into<String>) -> Self {
        Self {
            run_id: run_id.into(),
            runtime_id: Uuid::new_v4().to_string(),
            boot_id: boot_id.into(),
        }
    }

    /// Predeclared identity for crash-recoverable acquire (recovery claims).
    pub fn planned(
        run_id: impl Into<String>,
        runtime_id: impl Into<String>,
        boot_id: impl Into<String>,
    ) -> Self {
        Self {
            run_id: run_id.into(),
            runtime_id: runtime_id.into(),
            boot_id: boot_id.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunSubState(String);

impl RunSubState {
    /// Validate a performer-owned `<performer>.<verb>` sub-state.
    ///
    /// The namespace is the performer: any lowercase worker kind token,
    /// `human`, or reserved `ci`. The verb is a non-empty lowercase token
    /// using underscores for multi-word actions.
    pub fn new(value: impl Into<String>) -> Result<Self, SessionError> {
        let value = value.into();
        let Some((namespace, verb)) = value.split_once('.') else {
            return Err(SessionError::InvalidRunSubState(value));
        };
        if !is_valid_sub_state_namespace(namespace)
            || verb.is_empty()
            || !verb.chars().all(|c| c.is_ascii_lowercase() || c == '_')
        {
            return Err(SessionError::InvalidRunSubState(value));
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn is_valid_sub_state_namespace(namespace: &str) -> bool {
    let mut chars = namespace.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// Append-only writer for one session JSONL file.
pub struct SessionWriter {
    path: PathBuf,
    file: File,
    identity: RuntimeIdentity,
    seq: u64,
    /// The transport this run recorded, learned from the run's own `RunMeta`
    /// lifecycle line (orgasmic:TASK-FZB6T.2 finding 5).
    ///
    /// The writer had no transport context at all, so it could not tell a
    /// repaint from an assistant turn and treated both as payload to be
    /// bounded. It is learned from the bytes the run itself recorded rather
    /// than passed in by the caller, because every call site is a driver and a
    /// driver that believes it may send rendered output is exactly the
    /// regression this refusal exists to stop.
    transport: Option<String>,
}

impl SessionWriter {
    pub fn open(path: impl AsRef<Path>, identity: RuntimeIdentity) -> Result<Self, SessionError> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let transport = recorded_transport(&path);
        Ok(Self {
            path,
            file,
            identity,
            seq: 0,
            transport,
        })
    }

    /// Construct a writer from an already-authorized append handle. Callers
    /// use this when pathname re-resolution would discard retained file
    /// identity across a security-sensitive boundary.
    pub fn from_file(path: PathBuf, file: File, identity: RuntimeIdentity) -> Self {
        let transport = recorded_transport(&path);
        Self {
            path,
            file,
            identity,
            seq: 0,
            transport,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The transport this run recorded, once its `RunMeta` line has been seen.
    pub fn transport(&self) -> Option<&str> {
        self.transport.as_deref()
    }

    pub fn identity(&self) -> &RuntimeIdentity {
        &self.identity
    }

    pub fn next_seq(&self) -> u64 {
        self.seq
    }

    /// Append one envelope and return its sequence number.
    ///
    /// `driver_event` payloads pass through [`bound_driver_event_payload`]
    /// first: this is the single choke point every persisted harness payload
    /// goes through, so the cap cannot be bypassed by a new driver or a new
    /// call site (orgasmic:TASK-FZB6T item 3). Lifecycle, babysitter-summary,
    /// and note envelopes are supervisor-authored authority and are written
    /// verbatim.
    ///
    /// Ahead of the cap sits a REFUSAL (orgasmic:TASK-FZB6T.2 finding 5): a
    /// `text_chunk` from a run whose recorded transport renders into a pane is
    /// rendered screen output, and no amount of it may be persisted. The cap
    /// bounds one such event; it does not forbid a million of them, and the
    /// zero-persisted-redraw rule is a ban rather than a budget.
    pub fn append(&mut self, kind: SessionEventKind, event: Value) -> Result<u64, SessionError> {
        if kind == SessionEventKind::DriverEvent {
            if let Some(transport) = self.transport.as_deref() {
                if transport_is_pane(transport)
                    && event.get("type").and_then(Value::as_str) == Some("text_chunk")
                {
                    return Err(SessionError::RenderedPanePayloadRefused {
                        transport: transport.to_string(),
                    });
                }
            }
        }
        // A run states its transport in its own `RunMeta` line, so the writer
        // is fenced from that line onward — including a writer reopened by a
        // restarted daemon, which learns it from the file at `open`.
        if kind == SessionEventKind::Lifecycle {
            if let Some(transport) = run_meta_transport(&event) {
                self.transport = Some(transport);
            }
        }
        let event = match kind {
            SessionEventKind::DriverEvent => {
                bound_driver_event_payload(event, DRIVER_EVENT_PAYLOAD_CAP_BYTES).value
            }
            _ => event,
        };
        let envelope = SessionEnvelope {
            seq: self.seq,
            time: Utc::now(),
            run_id: self.identity.run_id.clone(),
            runtime_id: self.identity.runtime_id.clone(),
            boot_id: self.identity.boot_id.clone(),
            kind,
            event,
        };
        let line = serde_json::to_string(&envelope)?;
        self.file.write_all(line.as_bytes())?;
        self.file.write_all(b"\n")?;
        self.file.flush()?;
        self.file.sync_all()?;
        let seq = self.seq;
        self.seq += 1;
        Ok(seq)
    }
}

/// The `transport` a `RunMeta` lifecycle event states, or `None` for any other
/// lifecycle event.
fn run_meta_transport(event: &Value) -> Option<String> {
    if event.get("phase").and_then(Value::as_str) != Some("run_meta") {
        return None;
    }
    event
        .get("transport")
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// Bytes of a session file read to recover a run's recorded transport when a
/// writer is opened over history it did not write.
///
/// `RunMeta` is the second line of a run segment, immediately after `Acquire`,
/// so a small prefix window always contains it. A file whose window holds no
/// `RunMeta` leaves the transport unknown, and an unknown transport refuses
/// nothing: refusing on "not proven to be structured" would delete assistant turns and
/// tool results, which is the opposite failure.
const TRANSPORT_PROBE_BYTES: usize = 256 * 1024;

/// The transport recorded by the newest `RunMeta` in a session file's prefix
/// window. `None` for an absent, empty, or pre-`RunMeta` file.
///
/// orgasmic:TASK-FZB6T.2 finding 5 — without this, a daemon restart mid-run
/// reopens the session with no transport context and the pane refusal silently
/// stops applying for the rest of the run.
fn recorded_transport(path: &Path) -> Option<String> {
    use std::io::Read;
    let mut file = File::open(path).ok()?;
    let mut buffer = vec![0_u8; TRANSPORT_PROBE_BYTES];
    let mut filled = 0;
    while filled < buffer.len() {
        match file.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(read) => filled += read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return None,
        }
    }
    buffer.truncate(filled);
    let mut transport = None;
    for line in buffer.split(|byte| *byte == b'\n') {
        let Ok(envelope) = serde_json::from_slice::<SessionEnvelope>(line) else {
            continue;
        };
        if envelope.kind != SessionEventKind::Lifecycle {
            continue;
        }
        if let Some(recorded) = run_meta_transport(&envelope.event) {
            transport = Some(recorded);
        }
    }
    transport
}

// orgasmic:TASK-FZB6T
// ---------------------------------------------------------------------------
// Retention by authority, and the payload cap that enforces it.
// ---------------------------------------------------------------------------

/// Default per-payload byte cap for one string inside a persisted
/// `driver_event`.
///
/// Not a cap on the event, and not a cap on the file: it is a cap on any
/// SINGLE free-text payload (assistant text, tool result body, thinking
/// block). Events stay countable — `call_id`, `name`, `ok`, `seq`, and the
/// envelope `time` are structure, never payload, so a bounded stream still
/// answers how many tool calls ran, which failed, how many were retried, and
/// how long each took.
///
/// 16 KiB is chosen against the shapes that actually appear: it holds a whole
/// harness `ready` capabilities frame (the largest control frame observed on a real
/// 2.2 GiB board was 18 KiB, and control frames are not payload), an ordinary
/// assistant turn, and a normal tool result, while a `cargo test` log or a
/// pasted file — the payloads that produced the 2.239 GiB incident — is
/// digested instead of copied.
pub const DRIVER_EVENT_PAYLOAD_CAP_BYTES: usize = 16 * 1024;

/// Bytes of the original payload retained inline ahead of the digest marker.
/// Enough for a human skimming the JSONL to recognize what was cut.
const BOUNDED_PAYLOAD_HEAD_BYTES: usize = 2 * 1024;

/// Key suffix carrying the machine-readable digest for a bounded sibling.
/// `chunk` → `chunk_bounded`. A suffix rather than a nested rewrite because
/// `DriverEvent` deserialization must keep working on bounded lines: the tag
/// stays a string, and serde ignores the unknown sibling key.
const BOUNDED_SUFFIX: &str = "_bounded";

/// Where the bytes that were NOT copied still live.
///
/// orgasmic never copies harness-native JSONL into its own session file. When a
/// payload overflows the cap, the full text remains exactly where the harness
/// already wrote it, and the digest names that fact rather than a path orgasmic
/// would have to keep valid.
const BOUNDED_PAYLOAD_SOURCE: &str =
    "harness-native session transcript (vendor-owned; never copied by orgasmic)";

/// Default retention by authority (orgasmic:TASK-FZB6T item 5).
///
/// Four tiers, ordered by who owns the bytes. Anything a maintenance command
/// may reclaim must be justified against this table, and the two authoritative
/// tiers are never reclaimable:
///
/// | tier | authority | default retention |
/// | --- | --- | --- |
/// | lifecycle envelopes + run catalog | orgasmic (authoritative) | kept indefinitely; the catalog is derived and rebuildable, the lifecycle lines are not |
/// | compact semantic driver events | orgasmic (derived, budgeted) | kept within [`DRIVER_EVENT_PAYLOAD_CAP_BYTES`] per payload; overflow becomes a digest |
/// | derived retro/evidence caches | orgasmic (disposable) | reclaimable at any time; regenerable from the two tiers above |
/// | harness-native history | the vendor | never copied, never pruned, never counted against an orgasmic budget |
///
/// Rendered TUI redraw and scrollback are not a tier: they are forbidden
/// storage (dec_WDR5K item 7 / TASK-AFE5Q). A pane transport persists a
/// [`DriverEvent::PaneActivity`] byte COUNT and nothing else.
pub const RETENTION_TIERS: [(&str, &str, &str); 4] = [
    (
        "lifecycle",
        "orgasmic-authoritative",
        "kept: lifecycle envelopes and the run catalog decide recovery",
    ),
    (
        "semantic_driver_events",
        "orgasmic-derived",
        "kept within the documented per-payload byte budget; overflow digested",
    ),
    (
        "retro_evidence_cache",
        "orgasmic-disposable",
        "reclaimable: regenerable from lifecycle + semantic events",
    ),
    (
        "harness_native_history",
        "vendor-owned",
        "never copied by orgasmic and never pruned by orgasmic",
    ),
];

/// Outcome of bounding one `driver_event` payload tree.
#[derive(Debug, Clone)]
pub struct BoundedDriverEvent {
    /// The event with oversized payloads replaced by head + digest marker.
    pub value: Value,
    /// How many individual payloads were digested.
    pub bounded_payloads: u64,
    /// Total bytes of original payload that were NOT persisted.
    pub bytes_elided: u64,
}

/// Ceiling on the SERIALIZED size of one whole persisted `driver_event`,
/// derived from the per-payload cap.
///
/// orgasmic:TASK-FZB6T.1 finding 4 — a per-string cap alone bounds nothing. A
/// harness that emits ten thousand 1 KiB strings, or one that nests its content
/// deeper than the recursion ceiling, walked straight past a 16 KiB
/// per-payload limit and persisted megabytes per event. The budget has to be
/// stated over the thing that is actually written to disk: the serialized
/// event.
pub fn driver_event_total_cap(payload_cap: usize) -> usize {
    payload_cap.saturating_mul(4)
}

/// Structural keys, in the order they are given up if even they overflow.
///
/// These are what makes a bounded stream countable — how many tool calls ran,
/// which failed, in what order — so they are the LAST thing an over-budget
/// event loses, never the first.
const STRUCTURAL_KEYS: [&str; 8] = [
    "type",
    "call_id",
    "name",
    "ok",
    "seq",
    "stream",
    "phase",
    "protocol_version",
];

/// Bound one `driver_event` so both a single payload and the whole serialized
/// event stay inside a documented budget.
///
/// Three rules, applied in that order:
///
/// 1. **Per payload.** A string longer than `cap` is replaced by its first
///    [`BOUNDED_PAYLOAD_HEAD_BYTES`] plus an inline marker, and — when it sits
///    under a key in an object — a sibling `<key>_bounded` object carrying the
///    byte count, the SHA-256 of the full original, and the source reference.
/// 2. **Per subtree.** An object or array whose serialized form still exceeds
///    `cap` after its children were bounded is replaced wholesale by a digest.
///    This is what closes the many-small-payloads hole: ten thousand 1 KiB
///    strings are each individually legal and collectively are not.
/// 3. **Per event.** If the serialized event still exceeds
///    [`driver_event_total_cap`], its non-structural values are digested
///    largest-first until it fits, and only if that is still not enough are the
///    structural keys given up too.
///
/// A subtree deeper than [`BOUND_MAX_DEPTH`] is digested rather than passed
/// through: recursion has to stop somewhere, and stopping by *ignoring* the
/// rest of the tree is how an unbounded payload gets in under a depth ceiling.
///
/// Every replacement states the same three things — how many bytes were there,
/// their SHA-256, and where the bytes actually still live
/// ([`BOUNDED_PAYLOAD_SOURCE`]) — so the record is a truthful reference rather
/// than a silent truncation.
///
/// Idempotent: re-bounding an already-bounded event changes nothing, because
/// every retained head and every digest is smaller than the cap that produced
/// it.
pub fn bound_driver_event_payload(event: Value, cap: usize) -> BoundedDriverEvent {
    let mut value = event;
    let mut stats = BoundStats::default();
    let mut size = bound_value(&mut value, cap, 0, &mut stats);
    let total_cap = driver_event_total_cap(cap);
    if size > total_cap {
        size = bound_whole_event(&mut value, total_cap, &mut stats);
        debug_assert!(size <= total_cap || matches!(value, Value::Object(_)));
    }
    BoundedDriverEvent {
        value,
        bounded_payloads: stats.bounded_payloads,
        bytes_elided: stats.bytes_elided,
    }
}

#[derive(Default)]
struct BoundStats {
    bounded_payloads: u64,
    bytes_elided: u64,
}

/// Recursion depth ceiling. Harness content trees are shallow; a pathological
/// payload must cost bounded work rather than blow the stack.
const BOUND_MAX_DEPTH: usize = 16;

/// Bound one value in place and return its serialized JSON length.
///
/// The length is accumulated bottom-up so the whole tree costs one pass: a
/// re-serialization per container would be O(bytes × depth) on the write path.
fn bound_value(value: &mut Value, cap: usize, depth: usize, stats: &mut BoundStats) -> usize {
    match value {
        Value::Object(map) => {
            let keys: Vec<String> = map.keys().cloned().collect();
            let mut digests: Vec<(String, Value)> = Vec::new();
            let mut size = 2 + keys.len().saturating_sub(1);
            for key in &keys {
                let child = map.get_mut(key).expect("key taken from this map");
                let (child_size, digest) = bound_child(child, cap, depth + 1, stats);
                size += json_string_len(key) + 1 + child_size;
                if let Some(digest) = digest {
                    digests.push((format!("{key}{BOUNDED_SUFFIX}"), digest));
                }
            }
            for (key, digest) in digests {
                size += json_string_len(&key) + 2 + json_len(&digest);
                map.insert(key, digest);
            }
            size
        }
        Value::Array(items) => {
            let mut size = 2 + items.len().saturating_sub(1);
            for child in items.iter_mut() {
                let (child_size, _) = bound_child(child, cap, depth + 1, stats);
                size += child_size;
            }
            size
        }
        other => json_len(other),
    }
}

/// Bound one child of a container.
///
/// Returns `(serialized length after bounding, sibling digest to publish)`. The
/// sibling is only produced for an oversized *string*: an oversized subtree is
/// replaced in place by a digest that already says everything a sibling would.
fn bound_child(
    child: &mut Value,
    cap: usize,
    depth: usize,
    stats: &mut BoundStats,
) -> (usize, Option<Value>) {
    // A digest is already the bounded form of whatever used to be here.
    // Re-digesting it would make bounding non-idempotent, which matters
    // because a replayed or forwarded event can pass through the writer twice.
    if is_bounded_marker(child) {
        return (json_len(child), None);
    }
    if depth >= BOUND_MAX_DEPTH {
        return (digest_subtree(child, stats, "depth"), None);
    }
    if let Value::String(text) = child {
        if text.len() > cap {
            let (head, digest) = digest_payload(text);
            stats.bounded_payloads += 1;
            stats.bytes_elided += text.len().saturating_sub(head.len()) as u64;
            *text = head;
            return (json_string_len(text), Some(digest));
        }
        return (json_string_len(text), None);
    }
    if matches!(child, Value::Object(_) | Value::Array(_)) {
        let size = bound_value(child, cap, depth, stats);
        if size > cap {
            return (digest_subtree(child, stats, "subtree-size"), None);
        }
        return (size, None);
    }
    (json_len(child), None)
}

/// Last resort: the whole serialized event is over budget even after every
/// payload and subtree inside it was bounded.
///
/// Non-structural values go first, largest first, so an event gives up its
/// content before it gives up the facts that make it countable. Returns the
/// serialized length that remains.
fn bound_whole_event(value: &mut Value, total_cap: usize, stats: &mut BoundStats) -> usize {
    let Value::Object(map) = value else {
        return digest_subtree(value, stats, "event-size");
    };
    // The running size is tracked incrementally. Re-measuring the map after
    // every elision would be quadratic in the number of keys, and "thousands of
    // keys" is exactly the shape this function exists to survive.
    let mut current = json_len_of_object(map);
    let mut order: Vec<(String, usize)> = map
        .iter()
        .filter(|(key, _)| !STRUCTURAL_KEYS.contains(&key.as_str()))
        .map(|(key, child)| (key.clone(), json_len(child)))
        .collect();
    order.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    for (key, was) in order {
        if current <= total_cap {
            return current;
        }
        if let Some(child) = map.get_mut(&key) {
            let now = digest_subtree(child, stats, "event-size");
            current = current.saturating_sub(was).saturating_add(now);
        }
    }
    if current <= total_cap {
        return current;
    }
    // Structure alone overflows — a pathological event with thousands of keys.
    // Keep the type and digest everything else, so the record still says what
    // kind of event it was and how much was elided.
    let ty = map.get("type").cloned();
    let size = digest_subtree(value, stats, "event-size");
    if let (Some(ty), Value::Object(map)) = (ty, &mut *value) {
        map.insert("type".to_string(), ty.clone());
        return size + json_string_len("type") + 2 + json_len(&ty);
    }
    size
}

/// Whether `value` is already a digest this function produced.
fn is_bounded_marker(value: &Value) -> bool {
    value
        .as_object()
        .is_some_and(|map| map.len() == 1 && map.contains_key("orgasmic_bounded"))
}

/// Replace one subtree by a digest naming its size, hash and true home.
/// Returns the serialized length of the replacement.
fn digest_subtree(value: &mut Value, stats: &mut BoundStats, reason: &'static str) -> usize {
    let serialized = serde_json::to_string(value).unwrap_or_default();
    let bytes = serialized.len();
    let mut digest = Map::new();
    digest.insert("bytes".to_string(), Value::from(bytes as u64));
    digest.insert(
        "sha256".to_string(),
        Value::String(hex_sha256(serialized.as_bytes())),
    );
    digest.insert("retained_bytes".to_string(), Value::from(0_u64));
    digest.insert("reason".to_string(), Value::String(reason.to_string()));
    digest.insert(
        "source".to_string(),
        Value::String(BOUNDED_PAYLOAD_SOURCE.to_string()),
    );
    let mut replacement = Map::new();
    replacement.insert("orgasmic_bounded".to_string(), Value::Object(digest));
    stats.bounded_payloads += 1;
    stats.bytes_elided += bytes as u64;
    *value = Value::Object(replacement);
    json_len(value)
}

/// Serialized JSON length of `value`, without building the string.
fn json_len(value: &Value) -> usize {
    match value {
        Value::Null => 4,
        Value::Bool(true) => 4,
        Value::Bool(false) => 5,
        Value::Number(number) => number.to_string().len(),
        Value::String(text) => json_string_len(text),
        Value::Array(items) => {
            2 + items.len().saturating_sub(1) + items.iter().map(json_len).sum::<usize>()
        }
        Value::Object(map) => json_len_of_object(map),
    }
}

fn json_len_of_object(map: &Map<String, Value>) -> usize {
    2 + map.len().saturating_sub(1)
        + map
            .iter()
            .map(|(key, child)| json_string_len(key) + 1 + json_len(child))
            .sum::<usize>()
}

/// Serialized JSON length of one string, quotes and escapes included.
fn json_string_len(text: &str) -> usize {
    let mut len = 2;
    for ch in text.chars() {
        len += match ch {
            '"' | '\\' | '\n' | '\r' | '\t' | '\u{8}' | '\u{c}' => 2,
            ch if (ch as u32) < 0x20 => 6,
            ch => ch.len_utf8(),
        };
    }
    len
}

/// `(retained head + marker, machine-readable digest object)` for one
/// oversized payload.
fn digest_payload(text: &str) -> (String, Value) {
    let sha = hex_sha256(text.as_bytes());
    let bytes = text.len();
    let head_end = char_boundary_at_or_below(text, BOUNDED_PAYLOAD_HEAD_BYTES);
    let mut head = String::with_capacity(head_end + 160);
    head.push_str(&text[..head_end]);
    head.push_str(&format!(
        "…[orgasmic-bounded: {bytes} bytes, sha256 {sha}, retained {head_end}; \
         full payload stays in the {BOUNDED_PAYLOAD_SOURCE}]"
    ));
    let mut digest = Map::new();
    digest.insert("bytes".to_string(), Value::from(bytes as u64));
    digest.insert("sha256".to_string(), Value::String(sha));
    digest.insert("retained_bytes".to_string(), Value::from(head_end as u64));
    digest.insert(
        "source".to_string(),
        Value::String(BOUNDED_PAYLOAD_SOURCE.to_string()),
    );
    (head, Value::Object(digest))
}

fn char_boundary_at_or_below(text: &str, mut index: usize) -> usize {
    if index >= text.len() {
        return text.len();
    }
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn hex_sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest.iter() {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Driver-side events serialized into the per-run JSONL session.
///
/// The supervisor folds these into [`SessionEnvelope`] (kind = [`SessionEventKind::DriverEvent`])
/// without altering the payload shape, so the JSONL stream is the
/// authoritative source for replay, recovery, and UI rendering.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DriverEvent {
    /// Driver finished its startup handshake. Supervisor unblocks acquire.
    Ready {
        protocol_version: String,
        capabilities: Value,
    },
    /// Free-form text chunk (assistant reply, stdout, stderr).
    TextChunk {
        stream: TextStream,
        chunk: String,
        seq: u64,
    },
    /// Worker invoked a tool/transition. `name` matches a [`WorkerTool`]
    /// variant for implementer runs and a [`BabysitterTool`] variant for
    /// babysitter runs.
    ToolCall {
        call_id: String,
        name: String,
        args: Value,
        seq: u64,
    },
    /// Result of a previous [`DriverEvent::ToolCall`].
    ToolResult {
        call_id: String,
        ok: bool,
        output: Value,
        seq: u64,
    },
    /// Worker explicitly transitioned the task state machine.
    TransitionState {
        from: String,
        to: String,
        reason: String,
    },
    /// Worker reported run completion. Supervisor moves the lease to
    /// `Released`.
    RunComplete { summary: Option<String> },
    /// Worker failed. Supervisor records the error and releases the lease.
    RunFail {
        error_code: String,
        error_markdown: String,
    },
    /// Driver-internal error (process died, transport broke, etc.).
    DriverError { fatal: bool, message: String },
    /// Protocol signal that one agent/model turn finished at a native harness
    /// boundary (for example codex `turn/completed`, ACP `session/prompt`
    /// stop, or a synthesized single-turn subprocess completion).
    ///
    /// Carries no content. The supervisor counts only this variant toward
    /// `max_iterations`; substantive events within the same turn (text chunks,
    /// tool calls, heartbeats) do not advance the iteration counter.
    AgentTurnComplete { seq: u64 },
    /// Lightweight liveness signal emitted while a session is active but the
    /// underlying harness has produced no substantive output for a while (for
    /// example codex `app-server` buffering a long `cargo test` subprocess).
    ///
    /// Carries no content. Its sole purpose is to reset the supervisor's
    /// stall detector (`last_driver_event_at`) so an actively-working run that
    /// happens to be quiet is not mistaken for a stall. It is distinguished by
    /// its `type` (`heartbeat`) so substantive views (evidence distillation,
    /// babysitter summaries, UI transcripts) filter it out.
    Heartbeat { seq: u64 },
    /// A TUI pane wrote bytes to its terminal. Emitted only by the pane
    /// transports (rmux), coalesced to at most one event per fixed interval,
    /// and carrying no pane content — `bytes` is just how many raw pane output
    /// bytes were observed in the window (dec_WDR5K item 7 keeps rendered TUI
    /// output out of the JSONL; see TASK-AFE5Q).
    ///
    /// The unit is deliberately raw bytes, not lines: a full-screen harness
    /// repaints in place with ANSI/CR and can run for an entire stall window
    /// without ever emitting LF, so a line-terminated observation would go
    /// silent on exactly the runs this event exists to protect (TASK-RWCRN.1).
    /// A pane transport that cannot observe bytes must not emit this variant.
    ///
    /// What it proves: the harness process is still writing to its terminal.
    /// What it does NOT prove: that the worker made progress — a TUI that
    /// redraws a spinner while hung on the network keeps emitting these.
    ///
    /// It exists because a pane is a terminal, not an event source: without it
    /// `last_driver_event_at` freezes at `ready` and the supervisor's stall
    /// detector releases every healthy rmux dispatch at exactly
    /// `DEFAULT_STALL_TIMEOUT` (TASK-RWCRN). This is the *only* stall input an
    /// rmux run has, so anything that stops counting it against the stall
    /// clock — e.g. TASK-VZMZE moving stall onto a progress-only clock — must
    /// give the pane transports a replacement signal in the same change, or
    /// TASK-RWCRN regresses.
    PaneActivity { seq: u64, bytes: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TextStream {
    Stdout,
    Stderr,
    Assistant,
    User,
    System,
}

/// Lifecycle envelope (`kind = SessionEventKind::Lifecycle`).
///
/// Supervisor-side events the driver itself does not emit. Kept in
/// `orgasmic-core` so the on-disk JSONL is self-describing for boot
/// reconciliation in `arch_010`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum Lifecycle {
    Acquire {
        task_id: String,
        kind: String,
        worker_id: String,
    },
    /// Reattach metadata, written immediately after `Acquire` (boot
    /// auto-reattach). Carries enough to reconstruct the supervisor `reattach`
    /// call after a daemon restart: the driver transport/harness, project +
    /// worktree, and the exact `driver_config`. Kept a separate variant so
    /// pre-existing session JSONL (written before this event existed) still
    /// reconciles, and so the `Acquire` schema is untouched.
    RunMeta {
        transport: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        harness: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        project_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        worktree: Option<PathBuf>,
        /// Dispatch artifact paths (CLI-derived from the brief filename stem),
        /// so a boot reattach can reconstruct a `DispatchCompletion` and
        /// respawn the completion watcher that died with the old daemon
        /// process. `None` for non-dispatch runs (manager, recovery, stage
        /// launches) and for pre-upgrade session JSONL.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        last_path: Option<PathBuf>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stdout_path: Option<PathBuf>,
        /// Full UUID attempt token for CLI dispatch cleanup fencing (TASK-ZGT1X).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        dispatch_attempt_token: Option<String>,
        /// Worker role at acquire time (including `terminal` for custom bare
        /// terminals). Boot reattach restores this instead of inferring from
        /// `worker_id` alone (TASK-99W9C / dec_WDR5K item 6).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        role: Option<String>,
        /// Whether this run advertised the universal finalize contract when
        /// acquired. Boot reattach restores this instead of recomputing from
        /// artifact paths alone (TASK-99W9C).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        requires_worker_finalize: Option<bool>,
        /// How the harness authenticated this run, as a mode string only —
        /// `bare_api_key` or `native_login` for claude, `None` for every
        /// harness that resolves no credential mode and for session JSONL
        /// written before this field existed.
        ///
        /// Never credential material: this file is durable evidence and may be
        /// committed. The mode is recorded because it is the one input that
        /// decides whether a run could authenticate at all, and reading it back
        /// is how an operator learns which tier a finished run actually used
        /// (TASK-S0QRM).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        credential_mode: Option<String>,
        driver_config: Value,
    },
    /// The stage (`grill` / `plan` / `architect`) this run was launched as,
    /// written immediately after `RunMeta` by the stage launch path.
    ///
    /// A stage's completion ownership used to live only in the daemon's
    /// in-process watcher task, which dies with the daemon process. `RunMeta`
    /// cannot stand in for it: a stage run carries a `last_path` and never a
    /// `stdout_path`, so boot recovery read it as a half-recorded dispatch and
    /// respawned nothing at all — a stage live across a restart then emitted no
    /// terminal tx, ever (TASK-KPMFK).
    ///
    /// Only the stage name is durable. `target` is a static property of the
    /// stage (`api::stage_spec`), so persisting it would freeze a copy that can
    /// go stale against the one the live daemon uses; project and task are
    /// already on `Acquire`/`RunMeta`. A separate variant, rather than a
    /// `RunMeta` field, keeps every existing session JSONL and every existing
    /// `RunMeta` writer untouched.
    StageMeta {
        stage: String,
    },
    Attach,
    Release {
        reason: String,
        outcome: ReleaseOutcome,
        /// Set when the worker declared completion itself via
        /// `orgasmic dispatch finalize` (dec_3M7M0) before this release, over
        /// the same daemon channel used for every other write. The dispatch
        /// completion watcher treats this as authoritative and skips its
        /// scrollback-scrape fallback entirely. `#[serde(default)]` keeps
        /// pre-existing session JSONL (written before this field existed)
        /// parseable.
        #[serde(default)]
        finalized_by_worker: bool,
    },
    /// Historical auto-continuation envelope. No production path emits this
    /// after TASK-QPKCD; kept so older session JSONL still deserializes.
    Continuation {
        previous_run: String,
        previous_session_path: PathBuf,
        diff_summary: String,
        acceptance_criteria: Vec<String>,
    },
    BabysitterSpawned {
        target_run: String,
        babysitter_run: String,
    },
    /// A still-live runtime from a prior daemon boot was rehydrated into the
    /// current supervisor. The original `run_id`/`runtime_id` are preserved
    /// (carried by the envelope); this event records the *new* boot that
    /// reattached so the JSONL stays a complete per-run history.
    Reattach {
        reattached_boot: String,
        transport: String,
    },
    /// Harness-aware native runtime identity, captured at launch (or resume)
    /// time. For Claude, `session_id`/`session_path` are deterministic and
    /// `resume_argv` is the exact `claude --resume <id> --fork-session`
    /// command. Other harnesses populate only `launch_argv` until their
    /// native session semantics are known.
    NativeRuntime {
        provider: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_path: Option<PathBuf>,
        launch_argv: Vec<String>,
        #[serde(default)]
        resume_argv: Vec<String>,
    },
    /// Typed link from a replacement recovery run back to its Failed origin.
    /// Written into the replacement session after acquire succeeds so daemon
    /// session truth can verify committed recovery claims.
    RecoveryOrigin {
        project_id: String,
        origin_run_id: String,
        origin_session_path: PathBuf,
        request_id: String,
        replacement_run_id: String,
        replacement_session_path: PathBuf,
        action: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target: Option<String>,
        /// Complete immutable recovery claim snapshot. The daemon writes this
        /// only after the replacement exists and uses it to reconstruct a
        /// deleted claim without letting path-selected JSONL self-authenticate.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        claim: Option<Value>,
    },
    /// A recovery prompt staged for the operator. `sent = false` means the
    /// draft is pending an explicit composer send.
    PromptDraft {
        text: String,
        sent: bool,
    },
    /// A durable record of an operator composer send into a run.
    ComposerSend {
        text: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReleaseOutcome {
    Completed,
    Failed,
    Interrupted,
    Cancelled,
}

/// Worker-callable tools on implementer runs (arch_004).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerTool {
    TransitionState,
}

impl WorkerTool {
    pub fn as_str(self) -> &'static str {
        match self {
            WorkerTool::TransitionState => "transition_state",
        }
    }

    pub fn parse(s: &str) -> Option<WorkerTool> {
        match s {
            "transition_state" => Some(WorkerTool::TransitionState),
            _ => None,
        }
    }
}

/// Babysitter tool set per arch_004. Babysitters cannot edit code or invoke
/// arbitrary CLI commands; only these four actions are permitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BabysitterTool {
    Poke,
    Restart,
    Escalate,
    RecordFinding,
}

impl BabysitterTool {
    pub fn as_str(self) -> &'static str {
        match self {
            BabysitterTool::Poke => "poke_implementer",
            BabysitterTool::Restart => "restart_implementer",
            BabysitterTool::Escalate => "escalate_to_human",
            BabysitterTool::RecordFinding => "record_finding",
        }
    }

    pub fn parse(s: &str) -> Option<BabysitterTool> {
        match s {
            "poke" | "poke_implementer" => Some(BabysitterTool::Poke),
            "restart" | "restart_implementer" => Some(BabysitterTool::Restart),
            "escalate" | "escalate_to_human" => Some(BabysitterTool::Escalate),
            "record_finding" => Some(BabysitterTool::RecordFinding),
            _ => None,
        }
    }

    pub const ALL: [BabysitterTool; 4] = [
        BabysitterTool::Poke,
        BabysitterTool::Restart,
        BabysitterTool::Escalate,
        BabysitterTool::RecordFinding,
    ];
}

/// A summarized implementer event chunk fed to the babysitter (arch_004).
///
/// The supervisor coalesces driver events from the implementer's session into
/// a coarse summary before handing them to the babysitter, so the babysitter
/// reasons about stall/escalation signals instead of raw byte streams.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BabysitterSummaryChunk {
    pub window_start_seq: u64,
    pub window_end_seq: u64,
    pub event_count: usize,
    /// Highest-level event observed in the window. `RunFail` outranks
    /// `RunComplete` outranks `ToolCall`, etc.; see implementation in
    /// `orgasmic-daemon::supervisor`.
    pub headline: String,
    /// Last assistant text, truncated. Empty if no text in window.
    pub last_text: String,
    /// Tool call names observed.
    pub tool_calls: Vec<String>,
}

/// Read every envelope from a JSONL file. Skips empty lines but returns an
/// error on the first malformed line. Used by boot reconciliation and tests.
///
/// This reads and parses the whole transcript. Callers that only need
/// lifecycle facts (run enumeration, recovery classification, origin
/// indexing) must use [`scan_session_lifecycle`] instead: a single TUI run
/// can persist hundreds of megabytes of `text_chunk` driver events, and
/// answering "did this run release?" must not cost transcript bytes.
pub fn read_session_file(path: impl AsRef<Path>) -> Result<Vec<SessionEnvelope>, SessionError> {
    let contents = std::fs::read_to_string(path)?;
    let mut out = Vec::new();
    for line in contents.lines() {
        if line.trim().is_empty() {
            continue;
        }
        out.push(serde_json::from_str(line)?);
    }
    Ok(out)
}

/// Byte budget for a bounded lifecycle scan.
///
/// Lifecycle truth clusters at both ends of a session file: `acquire`,
/// `run_meta`, `native_runtime` and the first `ready` are written before any
/// work happens, and `release` / terminal driver events plus appended
/// `recovery_origin` links are written after it. Everything between is
/// transcript.
#[derive(Debug, Clone, Copy)]
pub struct SessionScanBudget {
    pub prefix_bytes: u64,
    pub tail_bytes: u64,
}

impl SessionScanBudget {
    /// Default inventory budget. Measured against a 198-file / 2.2 GiB real
    /// board, the furthest a lifecycle-deciding line ever sat from the start
    /// was 56 KiB and from the end 13 KiB, so these windows carry roughly 2x
    /// and 5x headroom while keeping a whole-board pass at tens of megabytes.
    pub const DEFAULT: Self = Self {
        prefix_bytes: 128 * 1024,
        tail_bytes: 64 * 1024,
    };

    fn window_bytes(&self) -> u64 {
        self.prefix_bytes.saturating_add(self.tail_bytes)
    }
}

impl Default for SessionScanBudget {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Result of a bounded lifecycle scan over one session JSONL.
#[derive(Debug, Clone, Default)]
pub struct SessionLifecycleScan {
    /// Retained lifecycle-relevant envelopes in file order. Transcript
    /// payloads are dropped without being parsed.
    pub envelopes: Vec<SessionEnvelope>,
    /// Size of the file on disk.
    pub file_bytes: u64,
    /// Bytes actually read from disk.
    pub bytes_inspected: u64,
    /// The middle of the file was skipped because it exceeded the budget.
    /// Callers must treat anything not provable from the retained envelopes
    /// as unknown rather than absent.
    pub truncated: bool,
    /// The file's final line was retained. When false, the last retained
    /// envelope is NOT the last event of the run, so terminal decisions that
    /// depend on "the last envelope" must not be made from it.
    pub final_envelope_retained: bool,
    /// `run_id` of the file's final line, read from that line's bounded
    /// envelope header even when the line itself was dropped as transcript.
    /// `None` only when there was no complete line to probe.
    ///
    /// This is the one fact a truncated scan can still state about the unread
    /// middle: whichever runs the gap holds, the run that owns the END of the
    /// file is this one. A caller pairing a retained prefix segment with the
    /// file's end (boot reattach does exactly that) must check it — the newest
    /// RETAINED segment is not provably the newest segment on disk, because the
    /// gap can hold a release and a later acquire (orgasmic:TASK-7QM8M).
    pub final_line_run_id: Option<String>,
    /// Lines dropped by the transcript filter, never parsed.
    pub skipped_transcript_lines: u64,
}

/// Driver events that carry lifecycle meaning. Everything else in the
/// `driver_event` stream is transcript and is dropped unparsed.
const LIFECYCLE_DRIVER_EVENT_TYPES: [&[u8]; 4] =
    [b"ready", b"run_complete", b"run_fail", b"run_error"];

/// How far into a line the envelope's own keys can be. `seq`, `time`,
/// `run_id`, `runtime_id`, `boot_id` and `kind` all precede `event`, and all
/// are short identifiers. Bounding the search keeps the filter O(1) per line
/// instead of O(transcript payload), which is the whole point.
const ENVELOPE_HEADER_PROBE_BYTES: usize = 1024;

/// Longest `driver_event` line that can still be a lifecycle-bearing control
/// frame. Driver events are stored as JSON objects with sorted keys, so the
/// `"type"` tag has no fixed offset and the line must be searched — bounding
/// that search is what keeps a transcript line cheap to reject. The largest
/// such frame observed on a real 2.2 GiB board was an 18 KiB `ready`; a run
/// whose control frame somehow exceeded this reads as non-terminal and
/// therefore stays visible as recoverable, never silently dropped.
const LIFECYCLE_DRIVER_EVENT_MAX_LINE_BYTES: usize = 64 * 1024;

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// `run_id` of the last non-blank line in `window`.
///
/// Every line carries the envelope header, transcript included, so this answers
/// "which run owns the end of this window" for the price of one bounded probe —
/// no parse, and no dependence on the line having been retained.
fn window_final_line_run_id(window: &[u8]) -> Option<String> {
    let line = window
        .split(|byte| *byte == b'\n')
        .rfind(|line| !line.iter().all(u8::is_ascii_whitespace))?;
    let header = &line[..line.len().min(ENVELOPE_HEADER_PROBE_BYTES)];
    let value = probe_string_value(header, b"\"run_id\":\"")?;
    std::str::from_utf8(value).ok().map(str::to_string)
}

/// Read the JSON string value following `key` inside `probe`.
fn probe_string_value<'a>(probe: &'a [u8], key: &[u8]) -> Option<&'a [u8]> {
    let start = find_bytes(probe, key)? + key.len();
    let rest = &probe[start..];
    let end = rest.iter().position(|byte| *byte == b'"')?;
    Some(&rest[..end])
}

/// True when a raw line must be parsed to answer lifecycle questions.
///
/// Conservative by construction: a line whose envelope shape cannot be read
/// from its bounded header probe is retained (and therefore surfaces as a
/// parse error, as before) rather than silently dropped. Because both probes
/// are anchored to envelope keys and length-bounded, transcript text that
/// merely *contains* `"kind":"lifecycle"` cannot forge retention.
fn line_is_lifecycle_relevant(line: &[u8]) -> bool {
    let header = &line[..line.len().min(ENVELOPE_HEADER_PROBE_BYTES)];
    if probe_string_value(header, b"\"kind\":\"") != Some(b"driver_event") {
        return true;
    }
    if line.len() > LIFECYCLE_DRIVER_EVENT_MAX_LINE_BYTES {
        return false;
    }
    // Every `"type":"…"` in the line is tested, not just the first: transcript
    // text can contain the same key, and the real tag may follow it. A false
    // positive only costs one retained envelope that no consumer matches.
    const TYPE_KEY: &[u8] = b"\"type\":\"";
    let mut cursor = 0;
    while let Some(found) = find_bytes(&line[cursor..], TYPE_KEY) {
        let start = cursor + found + TYPE_KEY.len();
        let rest = &line[start..];
        let Some(end) = rest.iter().position(|byte| *byte == b'"') else {
            return true;
        };
        if LIFECYCLE_DRIVER_EVENT_TYPES
            .iter()
            .any(|known| *known == &rest[..end])
        {
            return true;
        }
        cursor = start + end;
    }
    false
}

/// Read only the lifecycle-relevant envelopes of a session JSONL, inspecting
/// at most `budget` bytes.
///
/// Files within the budget are read whole and the result is exactly
/// [`read_session_file`] filtered down to lifecycle-relevant lines. Larger
/// files are read as a prefix window plus a tail window; partial lines at the
/// window edges are discarded, and [`SessionLifecycleScan::truncated`] is set
/// so callers can classify the gap conservatively.
///
/// Returns an error on the first malformed lifecycle-relevant line, matching
/// [`read_session_file`]'s strictness for the lines that decide recovery.
pub fn scan_session_lifecycle(
    path: impl AsRef<Path>,
    budget: SessionScanBudget,
) -> Result<SessionLifecycleScan, SessionError> {
    let mut file = File::open(path.as_ref())?;
    let file_bytes = file.metadata()?.len();
    scan_session_lifecycle_reader(&mut file, file_bytes, budget)
}

/// [`scan_session_lifecycle`] over an already-open handle.
///
/// Callers that must keep a retained, identity-validated file descriptor
/// (recovery claim reconciliation opens session files through a pinned
/// directory fd and re-checks device/inode) use this instead of reopening by
/// pathname. The handle is seeked; its cursor position on entry is ignored.
pub fn scan_session_lifecycle_reader<R: std::io::Read + std::io::Seek>(
    file: &mut R,
    file_bytes: u64,
    budget: SessionScanBudget,
) -> Result<SessionLifecycleScan, SessionError> {
    use std::io::SeekFrom;

    file.seek(SeekFrom::Start(0))?;
    let mut scan = SessionLifecycleScan {
        file_bytes,
        ..SessionLifecycleScan::default()
    };

    let (prefix, tail) = if file_bytes <= budget.window_bytes() {
        let mut whole = Vec::with_capacity(file_bytes as usize);
        file.read_to_end(&mut whole)?;
        scan.bytes_inspected = whole.len() as u64;
        (whole, Vec::new())
    } else {
        scan.truncated = true;
        let mut prefix = vec![0_u8; budget.prefix_bytes as usize];
        file.read_exact(&mut prefix)?;
        // Drop the partial line straddling the prefix boundary.
        match prefix.iter().rposition(|byte| *byte == b'\n') {
            Some(end) => prefix.truncate(end + 1),
            None => prefix.clear(),
        }

        let mut tail = vec![0_u8; budget.tail_bytes as usize];
        file.seek(SeekFrom::Start(file_bytes - budget.tail_bytes))?;
        file.read_exact(&mut tail)?;
        // Drop the partial line straddling the tail boundary.
        match tail.iter().position(|byte| *byte == b'\n') {
            Some(start) => {
                tail.drain(..=start);
            }
            None => tail.clear(),
        }

        scan.bytes_inspected = budget.window_bytes();
        (prefix, tail)
    };

    // Probed from the raw window, so a transcript last line — the normal shape
    // for a run that is still writing — still names its run.
    scan.final_line_run_id = window_final_line_run_id(if scan.truncated { &tail } else { &prefix });

    let mut final_line_retained = false;
    for window in [prefix.as_slice(), tail.as_slice()] {
        for line in window.split(|byte| *byte == b'\n') {
            match retain_lifecycle_line(&mut scan, line)? {
                LineOutcome::Blank => {}
                LineOutcome::Dropped => final_line_retained = false,
                LineOutcome::Retained => final_line_retained = true,
            }
        }
    }
    // A truncated scan whose tail window held no complete line proves nothing
    // about the file's final envelope, whatever the prefix ended with.
    scan.final_envelope_retained = final_line_retained && !(scan.truncated && tail.is_empty());

    Ok(scan)
}

/// What one raw line contributed to a scan.
enum LineOutcome {
    /// Whitespace only; it is not an event and says nothing about the last one.
    Blank,
    /// Transcript, rejected by the retention filter without being parsed.
    Dropped,
    /// Parsed and pushed onto [`SessionLifecycleScan::envelopes`].
    Retained,
}

/// Apply the retention filter to one raw line, shared by the bounded and the
/// complete scan so the two can never disagree about what a session file says.
fn retain_lifecycle_line(
    scan: &mut SessionLifecycleScan,
    line: &[u8],
) -> Result<LineOutcome, SessionError> {
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    if line.iter().all(u8::is_ascii_whitespace) {
        return Ok(LineOutcome::Blank);
    }
    if !line_is_lifecycle_relevant(line) {
        scan.skipped_transcript_lines += 1;
        return Ok(LineOutcome::Dropped);
    }
    scan.envelopes.push(serde_json::from_slice(line)?);
    Ok(LineOutcome::Retained)
}

/// Buffer size for the complete scan's line reader. Sized to the page-cache
/// read granularity rather than to any session fact.
const COMPLETE_SCAN_READ_BUFFER_BYTES: usize = 256 * 1024;

/// Full lifecycle enumeration of one session JSONL, streamed.
///
/// Same retention filter and the same strictness on malformed
/// lifecycle-relevant lines as [`scan_session_lifecycle`], but EVERY line is
/// examined, so [`SessionLifecycleScan::truncated`] is always false and the
/// result is a statement about the whole file rather than about two windows.
///
/// orgasmic:TASK-2QK4P.1.1 — this exists because a bounded scan's skipped
/// middle is UNKNOWN, and a caller that must not read unknown as absent needs
/// somewhere to escalate to. Streaming rather than a whole-file read is the
/// point: peak memory is one line plus the retained envelopes, never
/// `file_bytes`, so escalating on a multi-gigabyte transcript costs I/O and
/// not address space.
pub fn scan_session_lifecycle_complete(
    path: impl AsRef<Path>,
) -> Result<SessionLifecycleScan, SessionError> {
    let mut file = File::open(path.as_ref())?;
    let file_bytes = file.metadata()?.len();
    scan_session_lifecycle_complete_reader(&mut file, file_bytes)
}

/// [`scan_session_lifecycle_complete`] over an already-open handle, for callers
/// holding a pinned, identity-validated descriptor.
///
/// # A torn LAST line is not the same fact as a torn middle one
///
/// [`scan_session_lifecycle`] rejects the whole file on the first malformed
/// lifecycle-relevant line, which is right for it: it answers questions from
/// two windows and cannot tell where the damage sits. A complete scan can, and
/// the two shapes carry different information:
///
/// - A malformed line with BYTES AFTER IT aborts the read, so every line that
///   follows is unread. That is an incomplete observation and it is returned as
///   an error, because a `RecoveryOrigin` may sit behind it.
/// - A malformed line that is the file's LAST hides nothing. It is what a crash
///   mid-append leaves — the writer appends whole `sync_all`-ed lines, so the
///   only partial one is the one being written when the process died — and an
///   incomplete line cannot itself be a valid envelope. Every complete line in
///   the file WAS observed, so this returns `Ok`.
///
/// Both shapes are reachable: the daemon reopens a torn session and appends
/// after it, which puts yesterday's torn line in the middle of today's file.
/// orgasmic:TASK-2QK4P.1.1 — collapsing them would either freeze recovery on a
/// single junk `.jsonl` or hide a second authority behind one bad line.
pub fn scan_session_lifecycle_complete_reader<R: std::io::Read + std::io::Seek>(
    file: &mut R,
    file_bytes: u64,
) -> Result<SessionLifecycleScan, SessionError> {
    use std::io::{BufRead, BufReader, SeekFrom};

    file.seek(SeekFrom::Start(0))?;
    let mut scan = SessionLifecycleScan {
        file_bytes,
        ..SessionLifecycleScan::default()
    };
    let mut reader = BufReader::with_capacity(COMPLETE_SCAN_READ_BUFFER_BYTES, file);
    let mut line = Vec::new();
    // First bytes of the last non-blank line, kept so the file's final run id
    // can be probed without holding the line itself.
    let mut final_line_header = Vec::new();
    let mut final_line_retained = false;
    // Held rather than returned: it becomes an error only if a later line
    // proves it was not the tear at the end of the file.
    let mut deferred: Option<SessionError> = None;
    loop {
        line.clear();
        let read = reader.read_until(b'\n', &mut line)?;
        if read == 0 {
            break;
        }
        scan.bytes_inspected += read as u64;
        let raw = line.strip_suffix(b"\n").unwrap_or(&line);
        if raw.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        if let Some(err) = deferred.take() {
            return Err(err);
        }
        match retain_lifecycle_line(&mut scan, raw) {
            Ok(LineOutcome::Blank) => continue,
            Ok(LineOutcome::Dropped) => final_line_retained = false,
            Ok(LineOutcome::Retained) => final_line_retained = true,
            Err(err) => {
                deferred = Some(err);
                continue;
            }
        }
        final_line_header.clear();
        final_line_header.extend_from_slice(&raw[..raw.len().min(ENVELOPE_HEADER_PROBE_BYTES)]);
    }
    scan.final_line_run_id = window_final_line_run_id(&final_line_header);
    // A torn final line means the last retained envelope is not the file's last
    // event, exactly as a truncated tail window does.
    scan.final_envelope_retained = final_line_retained && deferred.is_none();
    Ok(scan)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Write a production-shaped session: lifecycle head, a bulky
    /// `text_chunk` transcript body, and a lifecycle/terminal tail.
    ///
    /// Written directly rather than through [`SessionWriter`], whose
    /// per-append `sync_all` makes megabyte fixtures take tens of seconds.
    fn write_bulky_session(path: &Path, run_id: &str, transcript_bytes: usize, released: bool) {
        let mut seq = 0;
        let mut out = String::new();
        let mut push = |kind: SessionEventKind, event: Value, out: &mut String| {
            let envelope = SessionEnvelope {
                seq,
                time: Utc::now(),
                run_id: run_id.to_string(),
                runtime_id: format!("runtime-{run_id}"),
                boot_id: "boot-scan".to_string(),
                kind,
                event,
            };
            out.push_str(&serde_json::to_string(&envelope).unwrap());
            out.push('\n');
            seq += 1;
        };

        push(
            SessionEventKind::Lifecycle,
            json!({"phase": "acquire", "kind": "worker", "task_id": "TASK-SCAN", "worker_id": "implementer-claude-rmux"}),
            &mut out,
        );
        push(
            SessionEventKind::DriverEvent,
            json!({"type": "ready", "protocol_version": "tmux-tui/1"}),
            &mut out,
        );
        let chunk = "x".repeat(4096);
        let mut written = 0;
        while written < transcript_bytes {
            push(
                SessionEventKind::DriverEvent,
                json!({"type": "text_chunk", "stream": "stdout", "text": chunk}),
                &mut out,
            );
            written += chunk.len();
        }
        if released {
            push(
                SessionEventKind::DriverEvent,
                json!({"type": "run_complete", "ok": true}),
                &mut out,
            );
            push(
                SessionEventKind::Lifecycle,
                json!({"phase": "release", "outcome": "completed", "reason": "done"}),
                &mut out,
            );
        }
        // Appends, so a caller can lay two run segments into one file the way
        // an older build's second-granularity manager file really did.
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        std::io::Write::write_all(&mut file, out.as_bytes()).unwrap();
    }

    #[test]
    fn lifecycle_scan_skips_transcript_bytes_on_a_huge_session() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run-bulky.jsonl");
        write_bulky_session(&path, "run-bulky", 8 * 1024 * 1024, true);

        let budget = SessionScanBudget::DEFAULT;
        let scan = scan_session_lifecycle(&path, budget).unwrap();

        assert!(scan.truncated, "an 8 MiB session must exceed the budget");
        assert!(scan.file_bytes > 8 * 1024 * 1024);
        assert_eq!(
            scan.bytes_inspected,
            budget.prefix_bytes + budget.tail_bytes
        );
        assert!(
            scan.envelopes.len() < 64,
            "only lifecycle-relevant envelopes are retained: {}",
            scan.envelopes.len()
        );
        // Only lines inside the two windows are even seen; the skipped middle
        // is never read, which is the point of the budget.
        assert!(
            scan.skipped_transcript_lines > 10,
            "window transcript lines must be rejected without parsing: {}",
            scan.skipped_transcript_lines
        );
        assert!(scan.final_envelope_retained);

        let first = scan.envelopes.first().unwrap();
        assert_eq!(first.seq, 0);
        assert_eq!(
            first.event.get("phase").and_then(|v| v.as_str()),
            Some("acquire")
        );
        let last = scan.envelopes.last().unwrap();
        assert_eq!(
            last.event.get("outcome").and_then(|v| v.as_str()),
            Some("completed")
        );
        assert!(scan
            .envelopes
            .iter()
            .any(|e| e.event.get("type").and_then(|v| v.as_str()) == Some("ready")));
        assert!(
            !scan
                .envelopes
                .iter()
                .any(|e| e.event.get("type").and_then(|v| v.as_str()) == Some("text_chunk")),
            "transcript payloads must never be retained"
        );
    }

    #[test]
    fn lifecycle_scan_reads_small_sessions_whole_without_truncation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run-small.jsonl");
        write_bulky_session(&path, "run-small", 4096, true);

        let scan = scan_session_lifecycle(&path, SessionScanBudget::DEFAULT).unwrap();
        assert!(!scan.truncated);
        assert_eq!(scan.bytes_inspected, scan.file_bytes);
        assert!(scan.final_envelope_retained);

        let full = read_session_file(&path).unwrap();
        let retained: Vec<u64> = scan.envelopes.iter().map(|e| e.seq).collect();
        let expected: Vec<u64> = full
            .iter()
            .filter(|e| {
                e.kind != SessionEventKind::DriverEvent
                    || e.event.get("type").and_then(|v| v.as_str()) != Some("text_chunk")
            })
            .map(|e| e.seq)
            .collect();
        assert_eq!(retained, expected);
    }

    /// orgasmic:TASK-7QM8M — a truncated scan cannot read the middle of a file,
    /// but it can still name the run that owns the END of it: `run_id` is read
    /// off the final line's bounded header even when that line was dropped as
    /// transcript, which is the normal shape for a run that is still writing.
    /// A caller pairing a retained prefix segment with the file's end needs
    /// exactly this to know the two belong to the same run.
    #[test]
    fn lifecycle_scan_names_the_run_that_owns_the_end_of_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("manager-two-runs.jsonl");
        write_bulky_session(&path, "run-first", 4 * 1024 * 1024, true);
        write_bulky_session(&path, "run-second", 4 * 1024 * 1024, false);

        let scan = scan_session_lifecycle(&path, SessionScanBudget::DEFAULT).unwrap();
        assert!(scan.truncated);
        assert!(
            !scan.final_envelope_retained,
            "the file ends in the second run's transcript"
        );
        assert!(
            !scan.envelopes.iter().any(|e| e.run_id == "run-second"),
            "nothing of the second run is retained: its acquire is in the unread \
             middle and its tail is transcript"
        );
        assert_eq!(
            scan.final_line_run_id.as_deref(),
            Some("run-second"),
            "the end of the file belongs to the second run, and saying so costs \
             one bounded header probe"
        );

        let small = dir.path().join("run-small.jsonl");
        write_bulky_session(&small, "run-small", 4096, true);
        let whole = scan_session_lifecycle(&small, SessionScanBudget::DEFAULT).unwrap();
        assert!(!whole.truncated);
        assert_eq!(whole.final_line_run_id.as_deref(), Some("run-small"));
    }

    #[test]
    fn lifecycle_scan_reports_unretained_final_envelope() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run-open.jsonl");
        write_bulky_session(&path, "run-open", 8 * 1024 * 1024, false);

        let scan = scan_session_lifecycle(&path, SessionScanBudget::DEFAULT).unwrap();
        assert!(scan.truncated);
        assert!(
            !scan.final_envelope_retained,
            "a session still emitting transcript has no retained final envelope"
        );
    }

    #[test]
    fn lifecycle_scan_ignores_transcript_text_that_mimics_lifecycle_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run-mimic.jsonl");
        let id = RuntimeIdentity::new("run-mimic", "boot-scan");
        let mut writer = SessionWriter::open(&path, id).unwrap();
        writer
            .append(
                SessionEventKind::Lifecycle,
                json!({"phase": "acquire", "kind": "worker", "task_id": "TASK-MIMIC", "worker_id": "implementer-claude-rmux"}),
            )
            .unwrap();
        // A worker printing session JSON into its own transcript must not be
        // able to forge a lifecycle envelope.
        writer
            .append(
                SessionEventKind::DriverEvent,
                json!({"type": "text_chunk", "text": "{\"kind\":\"lifecycle\",\"event\":{\"phase\":\"release\",\"outcome\":\"completed\"}}"}),
            )
            .unwrap();
        drop(writer);

        let scan = scan_session_lifecycle(&path, SessionScanBudget::DEFAULT).unwrap();
        assert_eq!(scan.envelopes.len(), 1);
        assert_eq!(scan.skipped_transcript_lines, 1);
        assert!(!scan.final_envelope_retained);
    }

    /// Opt-in measurement against a real, unmodified session directory:
    ///
    /// ```sh
    /// ORGASMIC_REAL_SESSIONS_DIR=/path/to/.orgasmic/tmp/sessions \
    ///   cargo test -p orgasmic-core --lib real_session_shape -- --ignored --nocapture
    /// ```
    ///
    /// Read-only: it opens each file, never writes, and never touches the
    /// daemon. Kept ignored so the suite has no machine-specific dependency.
    #[test]
    #[ignore = "requires ORGASMIC_REAL_SESSIONS_DIR; measurement, not a gate"]
    fn real_session_shape_scan_is_bounded() {
        let Ok(dir) = std::env::var("ORGASMIC_REAL_SESSIONS_DIR") else {
            panic!("set ORGASMIC_REAL_SESSIONS_DIR to a real .orgasmic/tmp/sessions directory");
        };
        let mut paths: Vec<PathBuf> = std::fs::read_dir(&dir)
            .expect("read sessions dir")
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("jsonl"))
            .collect();
        paths.sort();

        let started = std::time::Instant::now();
        let (mut file_bytes, mut inspected, mut truncated, mut errors) = (0_u64, 0_u64, 0, 0);
        for path in &paths {
            match scan_session_lifecycle(path, SessionScanBudget::DEFAULT) {
                Ok(scan) => {
                    file_bytes += scan.file_bytes;
                    inspected += scan.bytes_inspected;
                    truncated += u32::from(scan.truncated);
                }
                Err(_) => errors += 1,
            }
        }
        let bounded = started.elapsed();

        let started = std::time::Instant::now();
        let mut full_envelopes = 0_u64;
        for path in &paths {
            full_envelopes += read_session_file(path).map(|e| e.len() as u64).unwrap_or(0);
        }
        let full = started.elapsed();

        println!(
            "files={} on_disk={:.3} GiB inspected={:.3} MiB truncated={truncated} errors={errors}\n\
             bounded_scan={:?}  full_read={:?} ({full_envelopes} envelopes)",
            paths.len(),
            file_bytes as f64 / (1024.0 * 1024.0 * 1024.0),
            inspected as f64 / (1024.0 * 1024.0),
            bounded,
            full,
        );
        assert!(
            inspected * 20 < file_bytes,
            "bounded scan must read a small fraction of transcript bytes"
        );
    }

    #[test]
    fn lifecycle_scan_rejects_malformed_lifecycle_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run-torn.jsonl");
        std::fs::write(&path, "{\"seq\":0,\"kind\":\"lifecycle\"").unwrap();
        assert!(scan_session_lifecycle(&path, SessionScanBudget::DEFAULT).is_err());
    }

    // orgasmic:TASK-FZB6T — item 3: lock future storage.

    /// A long TUI session must not grow the orgasmic JSONL from screen repaint
    /// traffic.
    ///
    /// Stated as a CONSEQUENCE, not a wall time: the pane writes 8 MiB of
    /// full-screen ANSI redraws in 512 chunks, and the only thing that reaches
    /// the writer is the coalesced [`DriverEvent::PaneActivity`] byte count.
    /// The gate is that persisted bytes track the number of activity EVENTS,
    /// not the number of pane bytes — a driver that ever forwards rendered
    /// content (a `text_chunk` synthesized from the pane, a scrollback capture,
    /// a redraw payload) fails on file size and on the content assertion, in
    /// that order.
    ///
    /// orgasmic:TASK-FZB6T.2 finding 5 — the lock used to end at a byte
    /// ceiling: a 4 MiB repaint was asserted to grow the file by less than
    /// 8 KiB, which forbids one repaint and permits a million. It now asserts
    /// what the criterion actually says: ZERO persisted `text_chunk`
    /// envelopes, and zero repaint-derived growth.
    #[test]
    fn a_long_tui_session_persists_no_rendered_redraw_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run-tui.jsonl");
        let mut writer =
            SessionWriter::open(&path, RuntimeIdentity::new("run-tui", "boot-tui")).unwrap();
        writer
            .append(
                SessionEventKind::Lifecycle,
                json!({"phase": "acquire", "kind": "worker", "task_id": "TASK-TUI", "worker_id": "implementer-claude-rmux"}),
            )
            .unwrap();
        // The run states its own transport, exactly as a dispatched rmux run
        // does. This line is what fences the writer.
        writer
            .append(
                SessionEventKind::Lifecycle,
                json!({"phase": "run_meta", "transport": "rmux", "harness": "claude", "driver_config": {}}),
            )
            .unwrap();
        assert_eq!(writer.transport(), Some("rmux"));

        // One full-screen repaint: cursor home, clear, 40 rows of content.
        let repaint = format!(
            "\x1b[H\x1b[2J{}",
            "\x1b[K spinner ⠙ working…\r\n".repeat(40)
        );
        let mut pane_bytes_written = 0_u64;
        let mut activity_events = 0_u64;
        for seq in 0..512_u64 {
            for _ in 0..24 {
                pane_bytes_written += repaint.len() as u64;
            }
            // What the pane transport is allowed to persist: a count.
            let event = DriverEvent::PaneActivity {
                seq,
                bytes: repaint.len() as u64 * 24,
            };
            writer
                .append(
                    SessionEventKind::DriverEvent,
                    serde_json::to_value(&event).unwrap(),
                )
                .unwrap();
            activity_events += 1;
        }
        drop(writer);

        assert!(
            pane_bytes_written > 8 * 1024 * 1024,
            "the fixture must actually be a long TUI session: {pane_bytes_written} pane bytes"
        );
        let persisted = std::fs::metadata(&path).unwrap().len();
        // Each PaneActivity line is an envelope header plus two small numbers.
        // 512 bytes per event is generous headroom and still two orders of
        // magnitude below the pane traffic it observed.
        let ceiling = 512 * (activity_events + 1);
        assert!(
            persisted < ceiling,
            "rendered TUI output reached the JSONL: {persisted} bytes persisted for \
             {activity_events} activity events (ceiling {ceiling}), against \
             {pane_bytes_written} pane bytes observed"
        );

        let source = std::fs::read_to_string(&path).unwrap();
        assert!(
            !source.contains("\\u001b") && !source.contains("spinner"),
            "a redraw chunk persisted to JSONL: rendered pane content must never \
             be written to an orgasmic session file"
        );
        let envelopes = read_session_file(&path).unwrap();
        for envelope in &envelopes {
            if envelope.kind != SessionEventKind::DriverEvent {
                continue;
            }
            let parsed: DriverEvent = serde_json::from_value(envelope.event.clone()).unwrap();
            assert!(
                matches!(parsed, DriverEvent::PaneActivity { .. }),
                "a pane transport may persist only PaneActivity, got {parsed:?}"
            );
        }

        // THE LOCK, not the fixture. The assertions above hold because the
        // fixture emits only counts — which is what the current pane transports
        // do, and exactly what a future one could stop doing. So drive the case
        // that regression looks like: a driver that synthesizes a `text_chunk`
        // out of accumulated pane repaints and hands it to the writer. The
        // writer is the choke point, and it must REFUSE the rendered bytes
        // whatever the driver believes — not bound them, refuse them.
        //
        // A fresh writer, so this also proves the refusal survives the daemon
        // restart case: the reopened writer recovers the transport from the
        // run's own `RunMeta` line rather than from process memory.
        let scrollback = repaint.repeat(4096);
        assert!(scrollback.len() > 4 * 1024 * 1024);
        let before = std::fs::metadata(&path).unwrap().len();
        let mut writer =
            SessionWriter::open(&path, RuntimeIdentity::new("run-tui", "boot-tui")).unwrap();
        assert_eq!(
            writer.transport(),
            Some("rmux"),
            "a reopened writer must recover the run's recorded transport"
        );
        // Repeatedly, because a byte ceiling forbids one repaint and permits a
        // million: a linear-growth regression has to fail here too.
        for seq in 0..64_u64 {
            let refused = writer.append(
                SessionEventKind::DriverEvent,
                serde_json::to_value(&DriverEvent::TextChunk {
                    stream: TextStream::Stdout,
                    chunk: scrollback.clone(),
                    seq,
                })
                .unwrap(),
            );
            assert!(
                matches!(
                    refused,
                    Err(SessionError::RenderedPanePayloadRefused { .. })
                ),
                "the writer must refuse a pane transport's rendered payload, got {refused:?}"
            );
        }
        drop(writer);

        let grew = std::fs::metadata(&path).unwrap().len() - before;
        assert_eq!(
            grew,
            0,
            "a redraw chunk persisted to JSONL: 64 rendered repaints of {} bytes each grew \
             the session file by {grew} bytes; a pane transport must persist ZERO of them",
            scrollback.len()
        );
        let envelopes = read_session_file(&path).unwrap();
        let text_chunks = envelopes
            .iter()
            .filter(|e| e.event.get("type").and_then(|v| v.as_str()) == Some("text_chunk"))
            .count();
        assert_eq!(
            text_chunks, 0,
            "a redraw chunk persisted to JSONL: {text_chunks} text_chunk envelopes survived \
             in a pane transport's session file"
        );
        let source = std::fs::read_to_string(&path).unwrap();
        assert!(
            !source.contains("text_chunk") && !source.contains("orgasmic-bounded"),
            "a pane transport's rendered payload must leave no trace at all — not a \
             bounded digest, not an envelope"
        );
    }

    /// Oversized payloads are digested; the evidence needed for retrospective
    /// analysis survives.
    #[test]
    fn oversized_driver_payloads_are_digested_but_stay_countable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run-stdio.jsonl");
        let mut writer =
            SessionWriter::open(&path, RuntimeIdentity::new("run-stdio", "boot-stdio")).unwrap();

        let huge = "cargo test output line\n".repeat(200_000);
        assert!(huge.len() > 4 * 1024 * 1024);
        // Two calls, the first failing and retried, so the stream still has to
        // answer calls / results / retries / latencies after bounding.
        for (attempt, ok) in [("c1", false), ("c2", true)] {
            writer
                .append(
                    SessionEventKind::DriverEvent,
                    serde_json::to_value(&DriverEvent::ToolCall {
                        call_id: attempt.to_string(),
                        name: "shell".to_string(),
                        args: json!({"command": "cargo test"}),
                        seq: 0,
                    })
                    .unwrap(),
                )
                .unwrap();
            writer
                .append(
                    SessionEventKind::DriverEvent,
                    serde_json::to_value(&DriverEvent::ToolResult {
                        call_id: attempt.to_string(),
                        ok,
                        output: json!({"content": [{"type": "text", "text": huge}]}),
                        seq: 1,
                    })
                    .unwrap(),
                )
                .unwrap();
        }
        writer
            .append(
                SessionEventKind::DriverEvent,
                serde_json::to_value(&DriverEvent::TextChunk {
                    stream: TextStream::Assistant,
                    chunk: huge.clone(),
                    seq: 2,
                })
                .unwrap(),
            )
            .unwrap();
        drop(writer);

        let persisted = std::fs::metadata(&path).unwrap().len();
        assert!(
            persisted < 5 * DRIVER_EVENT_PAYLOAD_CAP_BYTES as u64,
            "three multi-megabyte payloads must not reach the file: {persisted} bytes"
        );

        let envelopes = read_session_file(&path).unwrap();
        // Calls, results, retries: countable.
        let calls = envelopes
            .iter()
            .filter(|e| e.event.get("type").and_then(|v| v.as_str()) == Some("tool_call"))
            .count();
        let results: Vec<&SessionEnvelope> = envelopes
            .iter()
            .filter(|e| e.event.get("type").and_then(|v| v.as_str()) == Some("tool_result"))
            .collect();
        assert_eq!(calls, 2);
        assert_eq!(results.len(), 2);
        assert_eq!(
            results
                .iter()
                .filter(|e| e.event.get("ok") == Some(&Value::Bool(false)))
                .count(),
            1,
            "a bounded result must still say whether it failed"
        );
        // Latency: envelope times survive, and call/result correlate by id.
        for envelope in &results {
            assert!(envelope
                .event
                .get("call_id")
                .and_then(|v| v.as_str())
                .is_some());
        }
        assert!(envelopes
            .windows(2)
            .all(|pair| pair[0].time <= pair[1].time));

        // The digest names bytes, hash, and where the bytes actually live.
        let chunk_envelope = envelopes
            .iter()
            .find(|e| e.event.get("type").and_then(|v| v.as_str()) == Some("text_chunk"))
            .unwrap();
        let digest = chunk_envelope.event.get("chunk_bounded").unwrap();
        assert_eq!(
            digest.get("bytes").and_then(|v| v.as_u64()),
            Some(huge.len() as u64)
        );
        assert_eq!(
            digest.get("sha256").and_then(|v| v.as_str()),
            Some(hex_sha256(huge.as_bytes()).as_str())
        );
        assert!(digest
            .get("source")
            .and_then(|v| v.as_str())
            .is_some_and(|s| s.contains("never copied by orgasmic")));

        // A bounded line still deserializes as its typed DriverEvent: the tag
        // stays a string and the digest is an ignored sibling key.
        let parsed: DriverEvent = serde_json::from_value(chunk_envelope.event.clone()).unwrap();
        let DriverEvent::TextChunk { chunk, .. } = parsed else {
            panic!("bounded text_chunk must still parse as TextChunk");
        };
        assert!(chunk.contains("orgasmic-bounded"));
        assert!(chunk.len() < DRIVER_EVENT_PAYLOAD_CAP_BYTES);
    }

    /// orgasmic:TASK-FZB6T.1 finding 4 — the budget is over the SERIALIZED
    /// event, not over one string at a time.
    ///
    /// Three shapes, each individually legal under a per-string cap and each
    /// unbounded in aggregate: many small strings in one array, many small
    /// strings under many keys, and a payload nested past the recursion
    /// ceiling. All three used to be persisted verbatim.
    #[test]
    fn a_composite_payload_is_bounded_as_a_whole_event() {
        let cap = DRIVER_EVENT_PAYLOAD_CAP_BYTES;
        let total_cap = driver_event_total_cap(cap);

        // 1. Ten thousand 1 KiB strings in one array: ~10 MB, no single string
        //    anywhere near the per-payload cap.
        let many: Vec<Value> = (0..10_000)
            .map(|index| Value::String(format!("{index:04}").repeat(256)))
            .collect();
        let event = json!({"type": "tool_result", "call_id": "c1", "ok": true, "output": many});
        let raw = serde_json::to_string(&event).unwrap().len();
        assert!(raw > 8 * 1024 * 1024, "the fixture must be large: {raw}");
        let bounded = bound_driver_event_payload(event, cap);
        let persisted = serde_json::to_string(&bounded.value).unwrap();
        assert!(
            persisted.len() <= total_cap,
            "a many-small-payloads event must be bounded as a whole: {} bytes persisted \
             against a {total_cap} byte ceiling",
            persisted.len()
        );
        // Structure survives, so the event is still countable.
        assert_eq!(
            bounded.value.get("type").and_then(Value::as_str),
            Some("tool_result")
        );
        assert_eq!(
            bounded.value.get("call_id").and_then(Value::as_str),
            Some("c1")
        );
        assert_eq!(bounded.value.get("ok"), Some(&Value::Bool(true)));
        // And the replacement is a truthful reference, not a silent truncation.
        let digest = bounded.value["output"]["orgasmic_bounded"].clone();
        assert!(digest["bytes"]
            .as_u64()
            .is_some_and(|bytes| bytes > 8 * 1024 * 1024));
        assert_eq!(digest["sha256"].as_str().map(str::len), Some(64));
        assert!(digest["source"]
            .as_str()
            .is_some_and(|source| source.contains("never copied by orgasmic")));
        assert!(bounded.bytes_elided > 8 * 1024 * 1024);

        // 2. The same volume spread across many KEYS rather than one array.
        let mut wide = Map::new();
        wide.insert("type".to_string(), Value::String("tool_result".to_string()));
        for index in 0..10_000 {
            wide.insert(format!("k{index}"), Value::String("payload".repeat(128)));
        }
        let event = Value::Object(wide);
        assert!(serde_json::to_string(&event).unwrap().len() > 8 * 1024 * 1024);
        let bounded = bound_driver_event_payload(event, cap);
        let persisted = serde_json::to_string(&bounded.value).unwrap();
        assert!(
            persisted.len() <= total_cap,
            "a wide event must be bounded too: {} bytes",
            persisted.len()
        );
        assert_eq!(
            bounded.value.get("type").and_then(Value::as_str),
            Some("tool_result")
        );

        // 3. A payload nested well past the recursion ceiling. The old rule
        //    stopped recursing at depth 16 and persisted everything below it.
        let mut deep = json!({"leaf": "z".repeat(4 * 1024 * 1024)});
        for _ in 0..64 {
            deep = json!({"next": deep});
        }
        let event = json!({"type": "text_chunk", "stream": "assistant", "chunk": deep});
        assert!(serde_json::to_string(&event).unwrap().len() > 4 * 1024 * 1024);
        let bounded = bound_driver_event_payload(event, cap);
        let persisted = serde_json::to_string(&bounded.value).unwrap();
        assert!(
            persisted.len() <= total_cap,
            "a deeply nested payload must be bounded, not skipped: {} bytes",
            persisted.len()
        );
        assert!(
            !persisted.contains("zzzzzzzzzz"),
            "content below the recursion ceiling reached the session file"
        );
        assert_eq!(
            bounded.value.get("type").and_then(Value::as_str),
            Some("text_chunk")
        );

        // Every bounded shape stays idempotent.
        let again = bound_driver_event_payload(bounded.value.clone(), cap);
        assert_eq!(again.bounded_payloads, 0, "re-bounding must be a no-op");
        assert_eq!(again.value, bounded.value);
    }

    /// The whole-event ceiling is enforced at the writer, not only in the
    /// helper: a driver that hands the writer a composite payload cannot route
    /// around it.
    #[test]
    fn the_writer_enforces_the_whole_event_ceiling() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run-wide.jsonl");
        let mut writer =
            SessionWriter::open(&path, RuntimeIdentity::new("run-wide", "boot-wide")).unwrap();
        let many: Vec<Value> = (0..10_000)
            .map(|index| Value::String(format!("{index:04}").repeat(256)))
            .collect();
        writer
            .append(
                SessionEventKind::DriverEvent,
                json!({"type": "tool_result", "call_id": "c1", "ok": false, "output": many}),
            )
            .unwrap();
        drop(writer);

        let persisted = std::fs::metadata(&path).unwrap().len();
        assert!(
            persisted < 2 * driver_event_total_cap(DRIVER_EVENT_PAYLOAD_CAP_BYTES) as u64,
            "a 10 MB composite payload reached the JSONL: {persisted} bytes"
        );
        let envelopes = read_session_file(&path).unwrap();
        assert_eq!(envelopes.len(), 1);
        assert_eq!(
            envelopes[0].event.get("ok"),
            Some(&Value::Bool(false)),
            "a bounded result must still say whether it failed"
        );
    }

    #[test]
    fn bounding_is_idempotent_and_leaves_small_payloads_untouched() {
        let small = json!({"type": "text_chunk", "chunk": "hello", "seq": 1});
        let once = bound_driver_event_payload(small.clone(), DRIVER_EVENT_PAYLOAD_CAP_BYTES);
        assert_eq!(once.bounded_payloads, 0);
        assert_eq!(once.value, small);

        let big = json!({"type": "text_chunk", "chunk": "x".repeat(1024 * 1024), "seq": 1});
        let first = bound_driver_event_payload(big, DRIVER_EVENT_PAYLOAD_CAP_BYTES);
        assert_eq!(first.bounded_payloads, 1);
        assert!(first.bytes_elided > 1_000_000);
        let second =
            bound_driver_event_payload(first.value.clone(), DRIVER_EVENT_PAYLOAD_CAP_BYTES);
        assert_eq!(second.bounded_payloads, 0, "re-bounding must be a no-op");
        assert_eq!(second.value, first.value);
    }

    /// Lifecycle envelopes are authority, not payload: the cap must not touch
    /// them, or a long recovery prompt draft would come back digested and the
    /// operator would lose the exact text that was staged.
    #[test]
    fn lifecycle_envelopes_are_never_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run-lifecycle.jsonl");
        let text = "recovery prompt ".repeat(4096);
        assert!(text.len() > DRIVER_EVENT_PAYLOAD_CAP_BYTES);
        let mut writer =
            SessionWriter::open(&path, RuntimeIdentity::new("run-lc", "boot-lc")).unwrap();
        writer
            .append(
                SessionEventKind::Lifecycle,
                serde_json::to_value(Lifecycle::PromptDraft {
                    text: text.clone(),
                    sent: false,
                })
                .unwrap(),
            )
            .unwrap();
        drop(writer);
        let envelopes = read_session_file(&path).unwrap();
        let Lifecycle::PromptDraft { text: back, .. } =
            serde_json::from_value(envelopes[0].event.clone()).unwrap()
        else {
            panic!("expected a prompt draft");
        };
        assert_eq!(back, text);
    }

    #[test]
    fn writer_appends_and_reads_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run-abc.jsonl");
        let id = RuntimeIdentity::new("run-abc", "boot-1");
        let mut writer = SessionWriter::open(&path, id).unwrap();
        writer
            .append(SessionEventKind::Lifecycle, json!({"type": "acquire"}))
            .unwrap();
        writer
            .append(
                SessionEventKind::DriverEvent,
                json!({"tool": "edit", "ok": true}),
            )
            .unwrap();
        drop(writer);

        let env = read_session_file(&path).unwrap();
        assert_eq!(env.len(), 2);
        assert_eq!(env[0].seq, 0);
        assert_eq!(env[1].seq, 1);
        assert_eq!(env[0].kind, SessionEventKind::Lifecycle);
    }

    #[test]
    fn driver_event_round_trips_through_envelope() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run-de.jsonl");
        let id = RuntimeIdentity::new("run-de", "boot-1");
        let mut writer = SessionWriter::open(&path, id).unwrap();
        let evt = DriverEvent::ToolCall {
            call_id: "c1".into(),
            name: WorkerTool::TransitionState.as_str().into(),
            args: json!({"to": "in_progress"}),
            seq: 0,
        };
        writer
            .append(
                SessionEventKind::DriverEvent,
                serde_json::to_value(&evt).unwrap(),
            )
            .unwrap();
        drop(writer);
        let env = read_session_file(&path).unwrap();
        let parsed: DriverEvent = serde_json::from_value(env[0].event.clone()).unwrap();
        assert_eq!(parsed, evt);
    }

    #[test]
    fn babysitter_tool_set_is_closed() {
        for t in BabysitterTool::ALL {
            assert_eq!(BabysitterTool::parse(t.as_str()), Some(t));
        }
        assert_eq!(
            BabysitterTool::parse("poke_implementer"),
            Some(BabysitterTool::Poke)
        );
        assert_eq!(
            BabysitterTool::parse("restart_implementer"),
            Some(BabysitterTool::Restart)
        );
        assert_eq!(
            BabysitterTool::parse("escalate_to_human"),
            Some(BabysitterTool::Escalate)
        );
        assert!(BabysitterTool::parse("edit_file").is_none());
        assert!(BabysitterTool::parse("shell").is_none());
    }

    #[test]
    fn lifecycle_round_trip() {
        let lc = Lifecycle::Acquire {
            task_id: "TASK-006".into(),
            kind: "implementer".into(),
            worker_id: "implementer-claude-stream-json".into(),
        };
        let v = serde_json::to_value(&lc).unwrap();
        let back: Lifecycle = serde_json::from_value(v).unwrap();
        assert_eq!(lc, back);
    }

    #[test]
    fn run_sub_state_validates_namespace_and_verb() {
        assert_eq!(
            RunSubState::new("implementer.working").unwrap().as_str(),
            "implementer.working"
        );
        assert_eq!(
            RunSubState::new("weird.thing").unwrap().as_str(),
            "weird.thing"
        );
        for invalid in [
            "Agent.working",
            "agent-x.working",
            ".working",
            "agent.",
            "agent.UPPER",
        ] {
            assert!(RunSubState::new(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn appends_preserve_prior_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run-xyz.jsonl");
        {
            let mut w =
                SessionWriter::open(&path, RuntimeIdentity::new("run-xyz", "boot-1")).unwrap();
            w.append(SessionEventKind::Note, json!({"msg": "one"}))
                .unwrap();
        }
        {
            let mut w =
                SessionWriter::open(&path, RuntimeIdentity::new("run-xyz", "boot-1")).unwrap();
            w.append(SessionEventKind::Note, json!({"msg": "two"}))
                .unwrap();
        }
        let env = read_session_file(&path).unwrap();
        assert_eq!(env.len(), 2);
        assert_eq!(env[0].event["msg"], "one");
        assert_eq!(env[1].event["msg"], "two");
    }
    /// orgasmic:TASK-2QK4P.1.1 — a torn LAST line and a torn MIDDLE line carry
    /// different information, and collapsing them costs something either way.
    ///
    /// Read as equally fatal, one junk `.jsonl` in a sessions directory freezes
    /// that project's recovery forever. Read as equally harmless, one bad line
    /// early in a file hides every `RecoveryOrigin` behind it and lets a second
    /// daemon-authenticated replacement go undiscovered. The distinction is
    /// mechanical: bytes after the tear were never read; bytes after nothing
    /// do not exist.
    #[test]
    fn a_torn_final_line_is_observed_and_a_torn_middle_line_is_not() {
        let dir = tempfile::tempdir().unwrap();
        let identity = |seq: u64, kind: &str| {
            format!(
                "{{\"seq\":{seq},\"time\":\"2026-08-08T00:00:00Z\",\"run_id\":\"run-tear\",\
                 \"runtime_id\":\"rt-tear\",\"boot_id\":\"boot-tear\",\"kind\":\"{kind}\",\
                 \"event\":{{\"phase\":\"acquire\",\"kind\":\"worker\",\
                 \"task_id\":\"TASK-TEAR\",\"worker_id\":\"implementer-claude-rmux\"}}}}"
            )
        };
        const TEAR: &str = "{\"seq\":9001,\"kind\":\"lifecycle\",\"event\":{\"phase\":";

        let trailing = dir.path().join("trailing.jsonl");
        std::fs::write(&trailing, format!("{}\n{TEAR}\n", identity(0, "lifecycle"))).unwrap();
        let scan = scan_session_lifecycle_complete(&trailing)
            .expect("a tear with nothing after it hides nothing");
        assert_eq!(scan.envelopes.len(), 1, "the complete line is still read");
        assert!(
            !scan.final_envelope_retained,
            "but the last retained envelope is not the file's last event"
        );

        let middle = dir.path().join("middle.jsonl");
        std::fs::write(
            &middle,
            format!(
                "{}\n{TEAR}\n{}\n",
                identity(0, "lifecycle"),
                identity(2, "lifecycle")
            ),
        )
        .unwrap();
        assert!(
            scan_session_lifecycle_complete(&middle).is_err(),
            "a tear with bytes behind it leaves those bytes unread, and unread is not absent"
        );
    }
}

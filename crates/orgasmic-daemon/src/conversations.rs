//! HTTP boundary for conversations (CHAT-SCOPE C1). A conversation is a
//! `CONV-` node whose agent runs lease on the conversation id. Continuing one
//! is `live` (a run is up), `resumed` (the harness reloads its own session on
//! this machine), or `cold` (a fresh run seeded with scope context and a
//! bounded transcript tail). Shares the node resolver, per-node locks, and
//! serialized ledger writer with the rest of the node surface.
use super::node_services::{actor_key, digest, node};
use super::*;
use crate::supervisor::DEFAULT_IDLE_TIMEOUT_SECS;
use orgasmic_core::node_services::{self as records, LinkRecord};
use std::collections::HashSet;

pub(super) const ROUTES: &[(&str, &str)] = &[
    ("POST", "/conversations"),
    ("POST", "/conversations/:id/input"),
];

pub(super) fn routes() -> Router<ApiState> {
    Router::new()
        .route("/conversations", post(post_conversation_create))
        .route("/conversations/:id/input", post(post_conversation_input))
}

pub(crate) const CONVERSATION_PREFIX: &str = "CONV-";
const COLLECTION: &str = "conversations";
const CONTEXT_OPEN: &str = "<<<orgasmic-context";
const CONTEXT_CLOSE: &str = ">>>";
const SELECTION_LIMIT_BYTES: usize = 4096;
const RELATES_TO: &str = "RELATES_TO";

/// Shared in-flight set: one launch per conversation at a time (409 otherwise).
pub type LaunchSet = Arc<std::sync::Mutex<HashSet<String>>>;

#[derive(Debug, Deserialize)]
pub(super) struct ConversationCreateRequest {
    #[serde(default)]
    pub project: Option<String>,
    pub purpose: String,
    #[serde(default)]
    pub node: Option<String>,
    pub provider: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
    #[serde(default)]
    pub access: Option<String>,
    #[serde(default)]
    pub service_tier: Option<String>,
    /// `chat` (ACP stdio, default) or `tmux` (PTY harness).
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub harness: Option<String>,
    #[serde(default)]
    pub harness_args: Vec<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
    pub request_id: String,
}

#[derive(Debug, Deserialize)]
struct ConversationInputRequest {
    #[serde(default)]
    project: Option<String>,
    message: String,
    #[serde(default)]
    context: Vec<ContextChip>,
    #[serde(default)]
    request_id: Option<String>,
}

/// Context chips ride with a send exactly as the contract names them; the raw
/// array is snapshotted on the composer_send lifecycle event.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum ContextChip {
    Node {
        id: String,
    },
    Attachment {
        node: String,
        id: String,
        revision: u64,
    },
    Range {
        node: String,
        attachment: String,
        revision: u64,
        start_ms: u64,
        end_ms: u64,
    },
    Selection {
        text: String,
    },
}

pub(super) struct Created {
    pub id: String,
    pub run_id: String,
}

/// The persisted conversation record (node.org drawer + links.org scope).
struct Conversation {
    id: String,
    dir: PathBuf,
    archived: bool,
    owner: String,
    purpose: String,
    provider: String,
    model: Option<String>,
    effort: Option<String>,
    access: Option<String>,
    mode: String,
    machine: String,
    runs: Vec<String>,
    scope: Option<String>,
}

struct RecordSpec {
    purpose: String,
    scope: Option<String>,
    title: String,
    provider: String,
    model: Option<String>,
    effort: Option<String>,
    access: Option<String>,
    mode: String,
    request_id: String,
}

struct LaunchPlan {
    driver: Box<dyn WorkerDriver>,
    config: DriverConfig,
}

struct PriorSession {
    run_id: String,
    path: PathBuf,
    envelopes: Vec<SessionEnvelope>,
}

fn bad(e: impl std::fmt::Display) -> ApiError {
    ApiError::bad_request(e.to_string())
}

// ---- routes ---------------------------------------------------------------

async fn post_conversation_create(
    State(state): State<ApiState>,
    Extension(identity): Extension<Identity>,
    Query(q): Query<GraphQuery>,
    Json(req): Json<ConversationCreateRequest>,
) -> Result<Json<Value>, ApiError> {
    // The query string wins over the body, per contract.
    let project = q.project.clone().or_else(|| req.project.clone());
    let created = create_authorized(&state, &identity, project.as_deref(), req).await?;
    Ok(Json(
        json!({"id": created.id, "run_id": created.run_id, "mode": "cold"}),
    ))
}

async fn post_conversation_input(
    State(state): State<ApiState>,
    Extension(identity): Extension<Identity>,
    Path(id): Path<String>,
    Query(q): Query<GraphQuery>,
    Json(req): Json<ConversationInputRequest>,
) -> Result<Json<Value>, ApiError> {
    let _plugin_guard = state.plugins.operations.clone().read_owned().await;
    let project = q.project.clone().or_else(|| req.project.clone());
    let (state, _plugins) =
        node_scope(state, &identity, project.as_deref(), Action::ChatWrite).await?;
    let (project_id, snapshot) =
        resolve_authorized_project(&state, &identity, project.as_deref(), Action::ChatExecute)
            .await?;
    let root = select_loaded_project(&snapshot, &project_id)?.root.clone();
    drop(snapshot);
    let conv = load_conversation(&state, &identity, &project_id, &id).await?;
    if !matches!(identity, Identity::Admin) && conv.owner != actor_key(&identity) {
        return Err(ApiError::forbidden(
            "conversation belongs to another principal",
        ));
    }
    if conv.archived {
        return Err(ApiError::bad_request("conversation is archived"));
    }
    let message = req.message.trim();
    if message.is_empty() {
        return Err(ApiError::bad_request("message must not be empty"));
    }
    validate_chips(&req.context)?;
    let context = (!req.context.is_empty()).then(|| json!(req.context));

    let live = state.supervisor.snapshot().await;
    if let Some(run) = live.runs.iter().find(|run| run.task_id == conv.id) {
        let text = compose(None, None, &req.context, message);
        send(&state, &run.run_id, &run.identity, text, context).await?;
        return Ok(Json(json!({"run_id": run.run_id, "mode": "live"})));
    }
    drop(live);
    let _launch = claim_launch(&state, &conv.id)?;
    let request_id = req
        .request_id
        .clone()
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let prior = prior_session(&root, &conv);

    if conv.machine == state.machine {
        if let Some(prior) = prior.as_ref() {
            let resumed = try_resume(
                &state,
                &project_id,
                &root,
                &conv,
                prior,
                &request_id,
                &req.context,
                message,
                context.clone(),
            )
            .await?;
            if let Some(run_id) = resumed {
                append_run(
                    &state,
                    &identity,
                    &project_id,
                    &conv.dir,
                    &conv.id,
                    &run_id,
                    ":resumed",
                    "conversation.run_resumed",
                    "run resumed",
                )
                .await?;
                return Ok(Json(json!({"run_id": run_id, "mode": "resumed"})));
            }
        }
    }

    let plan = plan_launch(
        &state,
        &root,
        &conv.mode,
        &conv.provider,
        conv.model.clone(),
        conv.effort.clone(),
        conv.access.clone(),
        None,
        &[],
        None,
    )?;
    let scope = scope_context(
        &state,
        &project_id,
        &conv.id,
        &conv.purpose,
        conv.scope.as_deref(),
    )
    .await?;
    let tail = prior
        .as_ref()
        .and_then(|prior| transcript_tail(&prior.envelopes));
    let (acquire, _) = acquire_run(&state, &project_id, &root, &conv.id, &conv.purpose, plan)
        .await
        .map_err(acquire_error)?;
    let text = compose(Some(&scope), tail.as_deref(), &req.context, message);
    send(&state, &acquire.run_id, &acquire.identity, text, context).await?;
    append_run(
        &state,
        &identity,
        &project_id,
        &conv.dir,
        &conv.id,
        &acquire.run_id,
        ":cold",
        "conversation.run_cold",
        "continued without native memory",
    )
    .await?;
    Ok(Json(json!({"run_id": acquire.run_id, "mode": "cold"})))
}

// ---- create -----------------------------------------------------------------

/// Create a conversation for a caller holding chat.write and chat.execute,
/// launch its first run, and send `message` when present. Shared by the
/// `/conversations` route and the `/manager/chat/launch` shim.
pub(super) async fn create_authorized(
    state: &ApiState,
    identity: &Identity,
    project: Option<&str>,
    req: ConversationCreateRequest,
) -> Result<Created, ApiError> {
    let _plugin_guard = state.plugins.operations.clone().read_owned().await;
    let (state, _plugins) = node_scope(state.clone(), identity, project, Action::ChatWrite).await?;
    let (project_id, snapshot) =
        resolve_authorized_project(&state, identity, project, Action::ChatExecute).await?;
    let root = select_loaded_project(&snapshot, &project_id)?.root.clone();
    drop(snapshot);

    let purpose = req.purpose.trim().to_string();
    validate_purpose(&purpose)?;
    let mode = req
        .mode
        .as_deref()
        .map(str::trim)
        .filter(|mode| !mode.is_empty())
        .unwrap_or("chat")
        .to_ascii_lowercase();
    let provider = if mode == "tmux" {
        req.harness.clone().unwrap_or_else(|| req.provider.clone())
    } else {
        req.provider.trim().to_ascii_lowercase()
    };

    let scope = match req.node.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
        Some(node_id) => {
            let target: super::node_services::Node = node(
                &state,
                identity,
                &project_id,
                node_id,
                Action::LinksRead,
                false,
            )
            .await
            .map_err(|error| {
                ApiError::bad_request(format!("conversation scope {node_id}: {}", error.message))
            })?;
            let layer = resolve_node_layer(&state.node_types, None, node_id)?;
            if layer.collection_name().is_none() {
                return Err(ApiError::bad_request(
                    "conversation scope must be a collection node",
                ));
            }
            let title = std::fs::read_to_string(&target.path)
                .ok()
                .and_then(|source| {
                    let file = OrgFile::parse(source, NODE_FILE).ok()?;
                    let heading = file.find_by_id(node_id)?;
                    Some(
                        heading
                            .title
                            .trim()
                            .strip_prefix(node_id)
                            .map(str::trim)
                            .filter(|rest| !rest.is_empty())
                            .unwrap_or(heading.title.trim())
                            .to_string(),
                    )
                })
                .unwrap_or_else(|| node_id.to_string());
            Some((node_id.to_string(), title))
        }
        None => None,
    };
    let title = req
        .title
        .as_deref()
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| match &scope {
            Some((_, title)) => format!("Chat about {title}"),
            None => "Chat".to_string(),
        });
    validate_node_title(&title)?;

    // Fail fast on the launch address before any record exists.
    let plan = plan_launch(
        &state,
        &root,
        &mode,
        &provider,
        req.model.clone(),
        req.effort.clone(),
        req.access.clone(),
        req.service_tier.as_deref(),
        &req.harness_args,
        None,
    )?;
    let access = req
        .access
        .clone()
        .or_else(|| (mode == "chat").then(|| "full-access".to_string()));

    let (id, dir) = write_record(
        &state,
        identity,
        &project_id,
        &root,
        RecordSpec {
            purpose: purpose.clone(),
            scope: scope.as_ref().map(|(id, _)| id.clone()),
            title,
            provider: provider.clone(),
            model: req.model.clone(),
            effort: req.effort.clone(),
            access,
            mode: mode.clone(),
            request_id: req.request_id.clone(),
        },
    )
    .await?;

    let live = state.supervisor.snapshot().await;
    if let Some(run) = live.runs.iter().find(|run| run.task_id == id) {
        // Idempotent replay of a create whose run is still up.
        return Ok(Created {
            id,
            run_id: run.run_id.clone(),
        });
    }
    drop(live);
    let _launch = claim_launch(&state, &id)?;
    let (acquire, _) = acquire_run(&state, &project_id, &root, &id, &purpose, plan)
        .await
        .map_err(acquire_error)?;
    append_run(
        &state,
        identity,
        &project_id,
        &dir,
        &id,
        &acquire.run_id,
        "",
        "conversation.run_started",
        "run started",
    )
    .await?;
    if let Some(message) = req
        .message
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty())
    {
        let chips: Vec<ContextChip> = scope
            .as_ref()
            .map(|(node, _)| vec![ContextChip::Node { id: node.clone() }])
            .unwrap_or_default();
        let context = scope_context(
            &state,
            &project_id,
            &id,
            &purpose,
            scope.as_ref().map(|(id, _)| id.as_str()),
        )
        .await?;
        let text = compose(Some(&context), None, &chips, message);
        send(
            &state,
            &acquire.run_id,
            &acquire.identity,
            text,
            (!chips.is_empty()).then(|| json!(chips)),
        )
        .await?;
    }
    Ok(Created {
        id,
        run_id: acquire.run_id,
    })
}

fn validate_purpose(purpose: &str) -> Result<(), ApiError> {
    if purpose.is_empty()
        || purpose.len() > 64
        || !purpose
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_' | '.'))
    {
        return Err(ApiError::bad_request(
            "purpose must be a short lowercase token (discuss, regenerate, implement, review, or a plugin purpose)",
        ));
    }
    Ok(())
}

/// Write the CONV- record (node.org plus a RELATES_TO link when scoped) as
/// one transaction. Performs no launch.
async fn write_record(
    state: &ApiState,
    identity: &Identity,
    project_id: &str,
    root: &FsPath,
    spec: RecordSpec,
) -> Result<(String, PathBuf), ApiError> {
    let descriptor = state
        .node_types
        .descriptor(COLLECTION)
        .ok_or_else(|| ApiError::internal("missing shipped descriptor for conversations"))?;
    let owner = actor_key(identity);
    let mutation = mutation_identity(
        "conversation.created",
        project_id,
        json!({
            "purpose": spec.purpose,
            "scope": spec.scope,
            "title": spec.title,
            "provider": spec.provider,
            "mode": spec.mode,
            "owner": owner,
        }),
    )?;
    if let Some(cached) = state
        .writer
        .cached_mutation(&spec.request_id, &mutation)
        .await
        .map_err(writer_transaction_error)?
    {
        let dir = orgasmic_core::node_kernel::node_dir(root, COLLECTION, &cached.mutation_id);
        return Ok((cached.mutation_id, dir));
    }
    let (id, dir) = orgasmic_core::create_node_dir(root, descriptor)
        .map_err(|error| ApiError::internal(format!("reserve conversation: {error}")))?;
    let path = dir.join(NODE_FILE);
    let created_at = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let mut source = orgasmic_core::node_kernel::node_org_header(&descriptor.label, &id);
    source.push_str("#+todo: OPEN | ARCHIVED\n\n");
    source.push_str(&format!(
        "* OPEN {}\n:PROPERTIES:\n:ID: {id}\n:PURPOSE: {}\n:OWNER: {owner}\n:PROVIDER: {}\n:MODEL: {}\n:EFFORT: {}\n:ACCESS: {}\n:MODE: {}\n:MACHINE: {}\n:WORKTREE: {}\n:RUNS: \n:CREATED_AT: {created_at}\n:END:\n",
        spec.title.trim(),
        spec.purpose,
        spec.provider.trim(),
        spec.model.as_deref().unwrap_or("").trim(),
        spec.effort.as_deref().unwrap_or("").trim(),
        spec.access.as_deref().unwrap_or("").trim(),
        spec.mode.trim(),
        state.machine,
        root.display(),
    ));
    let mut rewrites = vec![FileRewrite {
        path: path.clone(),
        new_contents: source.clone().into_bytes(),
    }];
    if let Some(target) = &spec.scope {
        let link = LinkRecord {
            id: digest(format!("{id}\0{target}").as_bytes()),
            source: id.clone(),
            target: target.clone(),
            kind: RELATES_TO.into(),
            revision: 1,
            deleted: false,
            anchors: Vec::new(),
            actor: identity
                .member_name()
                .unwrap_or_else(|| state.actor.clone()),
            updated_at: Utc::now().to_rfc3339(),
        };
        rewrites.push(FileRewrite {
            path: dir.join("links.org"),
            new_contents: records::render_links(&[link]).into_bytes(),
        });
    }
    let file = OrgFile::parse(&source, NODE_FILE).map_err(bad)?;
    let validation = state
        .node_types
        .validate_write(COLLECTION, &file, &file.headings[0])
        .and_then(|_| {
            state
                .node_types
                .validate_transition(COLLECTION, None, &file, &file.headings[0])
        })
        .map_err(bad);
    if let Err(error) = validation {
        let _ = std::fs::remove_dir(&dir);
        return Err(error);
    }
    let prepared = prepare_api_tx_as(
        state,
        identity,
        ApiTxRequest {
            ty: format!("graph.{COLLECTION}.created"),
            actor: None,
            project: Some(project_id.to_string()),
            task: None,
            target: Some(path.display().to_string()),
            reason: format!("created {} {id}", descriptor.label),
            request_id: Some(spec.request_id),
            extra: vec![("NODE_ID".to_string(), id.clone())],
        },
    )
    .await;
    let prepared = match prepared {
        Ok(prepared) => prepared,
        Err(error) => {
            let _ = std::fs::remove_dir(&dir);
            return Err(error);
        }
    };
    let result = state
        .writer
        .transaction_mutation(rewrites, prepared.tx, mutation, id.clone())
        .await
        .map_err(writer_transaction_error)?;
    if result.mutation_id != id {
        std::fs::remove_dir(&dir).map_err(|error| {
            ApiError::internal(format!("release unused conversation reservation: {error}"))
        })?;
    }
    let id = result.mutation_id;
    refresh_after_project_mutation(state, project_id, prepared.project_tx, &result.tx_id).await?;
    state.events.publish(
        Topic::Graph,
        EventPayload::GraphNodeCreated {
            project_id: project_id.to_string(),
            layer: COLLECTION.into(),
            node_id: id.clone(),
            tx_id: result.tx_id,
        },
    );
    let dir = orgasmic_core::node_kernel::node_dir(root, COLLECTION, &id);
    Ok((id, dir))
}

// ---- regenerate integration --------------------------------------------------

/// The OPEN `regenerate` conversation about `node_id`, when one exists.
pub(super) async fn find_regenerate_conversation(
    state: &ApiState,
    entry: &BoardEntry,
    node_id: &str,
) -> Result<Option<String>, ApiError> {
    let (_, snap) = ensure_loaded_snapshot(state, Some(&entry.id)).await?;
    let project = select_loaded_project(&snap, &entry.id)?;
    for link in &project.graph.links {
        if link.deleted
            || link.kind != RELATES_TO
            || link.target != node_id
            || !link.source.starts_with(CONVERSATION_PREFIX)
        {
            continue;
        }
        let dir = orgasmic_core::node_kernel::node_dir(&entry.path, COLLECTION, &link.source);
        if let Ok(conv) = parse_conversation(&dir, &link.source) {
            if !conv.archived && conv.purpose == "regenerate" {
                return Ok(Some(conv.id));
            }
        }
    }
    Ok(None)
}

/// Find or create the node's `regenerate` conversation. The caller's own
/// authority over the node (artifacts.generate / nodes.write) is the gate; no
/// run is launched here, the artifactor lease is keyed by the returned id.
pub(super) async fn regenerate_conversation(
    state: &ApiState,
    identity: &Identity,
    entry: &BoardEntry,
    node_id: &str,
) -> Result<String, ApiError> {
    if let Some(id) = find_regenerate_conversation(state, entry, node_id).await? {
        return Ok(id);
    }
    let (id, _) = write_record(
        state,
        identity,
        &entry.id,
        &entry.path,
        RecordSpec {
            purpose: "regenerate".into(),
            scope: Some(node_id.to_string()),
            title: format!("Regenerate {node_id}"),
            provider: String::new(),
            model: None,
            effort: None,
            access: None,
            mode: String::new(),
            request_id: uuid::Uuid::new_v4().to_string(),
        },
    )
    .await?;
    Ok(id)
}

/// Record an artifactor run on its conversation: first run plain, later fresh
/// runs `:cold`; a run already on file (a hot follow-up) is a no-op.
pub(super) async fn note_regenerate_run(
    state: &ApiState,
    identity: &Identity,
    entry: &BoardEntry,
    conversation_id: &str,
    run_id: &str,
) -> Result<(), ApiError> {
    let dir = orgasmic_core::node_kernel::node_dir(&entry.path, COLLECTION, conversation_id);
    let conv = parse_conversation(&dir, conversation_id)?;
    if conv.runs.iter().any(|run| run_base(run) == run_id) {
        return Ok(());
    }
    let (suffix, ty, reason) = if conv.runs.is_empty() {
        ("", "conversation.run_started", "run started")
    } else {
        ("cold", "conversation.run_cold", "regenerate run started")
    };
    let suffix = if suffix.is_empty() {
        String::new()
    } else {
        format!(":{suffix}")
    };
    append_run(
        state,
        identity,
        &entry.id,
        &dir,
        conversation_id,
        run_id,
        &suffix,
        ty,
        reason,
    )
    .await
}

// ---- record helpers ---------------------------------------------------------

fn run_base(entry: &str) -> &str {
    entry.split(':').next().unwrap_or(entry)
}

async fn load_conversation(
    state: &ApiState,
    identity: &Identity,
    project_id: &str,
    id: &str,
) -> Result<Conversation, ApiError> {
    if !id.starts_with(CONVERSATION_PREFIX) {
        return Err(ApiError::not_found(format!("conversation {id}")));
    }
    let resolved = node(state, identity, project_id, id, Action::ChatRead, false).await?;
    let dir = resolved
        .path
        .parent()
        .ok_or_else(|| ApiError::internal("conversation path has no parent"))?
        .to_path_buf();
    parse_conversation(&dir, id)
}

fn parse_conversation(dir: &FsPath, id: &str) -> Result<Conversation, ApiError> {
    let source = std::fs::read_to_string(dir.join(NODE_FILE))
        .map_err(|_| ApiError::not_found(format!("conversation {id}")))?;
    let file = OrgFile::parse(&source, NODE_FILE).map_err(bad)?;
    let heading = file
        .find_by_id(id)
        .ok_or_else(|| ApiError::not_found(format!("conversation {id}")))?;
    let prop = |key: &str| {
        heading
            .property(key)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    };
    let links = match std::fs::read_to_string(dir.join("links.org")) {
        Ok(source) => records::read_links(&source).map_err(bad)?,
        Err(_) => Vec::new(),
    };
    Ok(Conversation {
        id: id.to_string(),
        dir: dir.to_path_buf(),
        archived: heading
            .todo
            .as_deref()
            .is_some_and(|todo| todo.eq_ignore_ascii_case("archived")),
        owner: prop("OWNER").unwrap_or_default(),
        purpose: prop("PURPOSE").unwrap_or_default(),
        provider: prop("PROVIDER").unwrap_or_default(),
        model: prop("MODEL"),
        effort: prop("EFFORT"),
        access: prop("ACCESS"),
        mode: prop("MODE").unwrap_or_default(),
        machine: prop("MACHINE").unwrap_or_default(),
        runs: prop("RUNS")
            .unwrap_or_default()
            .split_whitespace()
            .map(str::to_string)
            .collect(),
        scope: links
            .iter()
            .find(|link| link.kind == RELATES_TO && !link.deleted)
            .map(|link| link.target.clone()),
    })
}

/// Daemon-owned RUNS append: a direct writer transaction under the per-node
/// lock, validated as a write (the transition hook is the user-edit fence).
#[allow(clippy::too_many_arguments)]
async fn append_run(
    state: &ApiState,
    identity: &Identity,
    project_id: &str,
    dir: &FsPath,
    id: &str,
    run_id: &str,
    suffix: &str,
    ty: &str,
    reason: &str,
) -> Result<(), ApiError> {
    let lock = state.node_write_lock(dir);
    let _guard = lock.lock().await;
    let path = dir.join(NODE_FILE);
    let source = std::fs::read_to_string(&path)
        .map_err(|error| ApiError::internal(format!("read conversation: {error}")))?;
    let file = OrgFile::parse(&source, NODE_FILE).map_err(bad)?;
    let heading = file
        .find_by_id(id)
        .ok_or_else(|| ApiError::not_found(format!("conversation {id}")))?;
    let mut runs: Vec<String> = heading
        .property("RUNS")
        .unwrap_or("")
        .split_whitespace()
        .map(str::to_string)
        .collect();
    if runs.iter().any(|run| run_base(run) == run_id) {
        return Ok(());
    }
    runs.push(format!("{run_id}{suffix}"));
    let mut rewriter = OrgRewriter::new(&file, NODE_FILE);
    rewriter
        .upsert_property(id, "RUNS", &runs.join(" "))
        .map_err(|error| org_rewriter_error("append conversation run", id, error))?;
    let rendered = rewriter.finish();
    let after = OrgFile::parse(&rendered, NODE_FILE).map_err(bad)?;
    let heading = after
        .find_by_id(id)
        .ok_or_else(|| ApiError::internal("conversation lost its heading"))?;
    state
        .node_types
        .validate_write(COLLECTION, &after, heading)
        .map_err(bad)?;
    let prepared = prepare_api_tx_as(
        state,
        identity,
        ApiTxRequest {
            ty: ty.to_string(),
            actor: None,
            project: Some(project_id.to_string()),
            task: None,
            target: Some(path.display().to_string()),
            reason: reason.to_string(),
            request_id: None,
            extra: vec![
                ("NODE_ID".to_string(), id.to_string()),
                ("RUN_ID".to_string(), run_id.to_string()),
            ],
        },
    )
    .await?;
    let tx_id = state
        .writer
        .transaction(
            vec![FileRewrite {
                path,
                new_contents: rendered.into_bytes(),
            }],
            prepared.tx,
        )
        .await
        .map_err(|error| ApiError::internal(format!("write conversation runs: {error}")))?;
    refresh_after_project_mutation(state, project_id, prepared.project_tx, &tx_id).await?;
    state.events.publish(
        Topic::Graph,
        EventPayload::GraphNodeRevised {
            project_id: project_id.to_string(),
            layer: COLLECTION.into(),
            node_id: id.to_string(),
            action: ty.to_string(),
            tx_id,
        },
    );
    Ok(())
}

// ---- launch ------------------------------------------------------------------

struct LaunchGuard {
    set: LaunchSet,
    id: String,
}

impl Drop for LaunchGuard {
    fn drop(&mut self) {
        if let Ok(mut set) = self.set.lock() {
            set.remove(&self.id);
        }
    }
}

fn claim_launch(state: &ApiState, id: &str) -> Result<LaunchGuard, ApiError> {
    let set = state.conversation_launches.clone();
    {
        let mut guard = set
            .lock()
            .map_err(|_| ApiError::internal("conversation launch set poisoned"))?;
        if !guard.insert(id.to_string()) {
            return Err(ApiError::conflict(format!(
                "conversation {id} launch already in flight"
            )));
        }
    }
    Ok(LaunchGuard {
        set,
        id: id.to_string(),
    })
}

/// Resolve the driver and config for one run of a conversation. `chat` reuses
/// the Chat launch address exactly; `tmux` mirrors `/manager/launch`.
#[allow(clippy::too_many_arguments)]
fn plan_launch(
    state: &ApiState,
    root: &FsPath,
    mode: &str,
    provider: &str,
    model: Option<String>,
    effort: Option<String>,
    access: Option<String>,
    service_tier: Option<&str>,
    harness_args: &[String],
    load_session: Option<&str>,
) -> Result<LaunchPlan, ApiError> {
    match mode {
        "chat" => {
            let provider = provider.trim().to_ascii_lowercase();
            let driver = crate::driver_resolution::resolve_chat_driver(&provider).ok_or_else(|| {
                ApiError::bad_request(format!(
                    "unsupported Chat provider '{provider}'; expected codex, claude, opencode, cursor-agent, or hermes"
                ))
            })?;
            let access = match access.as_deref().unwrap_or("full-access") {
                "supervised" | "auto-accept-edits" | "auto" | "full-access" => {
                    access.unwrap_or_else(|| "full-access".into())
                }
                value => {
                    return Err(ApiError::bad_request(format!(
                        "unsupported Chat access mode '{value}'"
                    )));
                }
            };
            if !chat_access_supported(&provider, &access) {
                return Err(ApiError::bad_request(format!(
                    "Chat access mode '{access}' is not supported by the {provider} runtime"
                )));
            }
            let service_tier = match service_tier {
                None | Some("") | Some("standard") => None,
                Some("fast") => Some("fast".to_string()),
                Some(value) => {
                    return Err(ApiError::bad_request(format!(
                        "unsupported Chat service tier '{value}'"
                    )));
                }
            };
            let sandbox_permissions = match access.as_str() {
                "supervised" => {
                    "allow_exec=false,allow_patch=false,allow_network=false,allow_writes_outside_cwd=false"
                }
                "auto-accept-edits" => {
                    "allow_exec=false,allow_patch=true,allow_network=false,allow_writes_outside_cwd=false"
                }
                "auto" => {
                    "allow_exec=true,allow_patch=true,allow_network=true,allow_writes_outside_cwd=false"
                }
                _ => "allow_exec=true,allow_patch=true,allow_network=true,allow_writes_outside_cwd=true",
            };
            let speed = service_tier.as_ref().map(|_| "fast");
            let mut config = json!({
                "cwd": root,
                "auto_start_turn": false,
                "model": verbatim_optional(model),
                "reasoning_effort": verbatim_optional(effort),
                "access": access,
                "service_tier": service_tier,
                "speed": speed,
                "sandbox_permissions": sandbox_permissions,
            });
            if let Some(session) = load_session {
                config["acp_load_session"] = json!(session);
            }
            let config = DriverConfig::from_value(config);
            driver
                .validate(&config)
                .map_err(|error| driver_validate_error("chat", &provider, error))?;
            Ok(LaunchPlan { driver, config })
        }
        "tmux" => {
            let harness = provider.trim().to_string();
            if harness.is_empty()
                || manager_terminal_harness(&harness)
                || manager_external_harness(&harness)
            {
                return Err(ApiError::bad_request(
                    "conversations run an agent harness; custom and external are not conversations",
                ));
            }
            validate_address_harness_args(&harness, harness_args).map_err(ApiError::bad_request)?;
            let driver = resolve_launch_driver("tmux", &harness).ok_or_else(|| {
                ApiError::bad_request(format!("unsupported conversation driver tmux/{harness}"))
            })?;
            let config = DriverConfig::from_value(json!({
                "cwd": root,
                "auto_start_turn": false,
                "model": model,
                "reasoning_effort": effort,
                "harness_args": harness_args,
            }));
            let config = apply_driver_defaults(config, "tmux", &harness, &state.driver_defaults);
            Ok(LaunchPlan { driver, config })
        }
        other => Err(ApiError::bad_request(format!(
            "unsupported conversation mode '{other}'; expected chat or tmux"
        ))),
    }
}

/// Acquire one run leased on the conversation id. The session file is
/// `conversation-<id>-<uuid>.jsonl` so a same-second restart never appends
/// to the previous run's transcript.
async fn acquire_run(
    state: &ApiState,
    project_id: &str,
    root: &FsPath,
    id: &str,
    purpose: &str,
    plan: LaunchPlan,
) -> Result<(AcquireResponse, PathBuf), SupervisorError> {
    let session_path = project_sessions_dir(root).join(format!(
        "conversation-{id}-{}.jsonl",
        uuid::Uuid::new_v4().simple()
    ));
    // Operator-paced runs idle at a prompt; only discuss/regenerate get an
    // idle release, implement/review run until the operator ends them.
    let idle_timeout_secs =
        matches!(purpose, "discuss" | "regenerate").then_some(DEFAULT_IDLE_TIMEOUT_SECS);
    let acquire = state
        .supervisor
        .acquire(
            plan.driver.as_ref(),
            AcquireRequest {
                task_id: id.to_string(),
                kind: RunKind::Worker,
                worker_id: "manager".into(),
                role: "manager".into(),
                project_id: Some(project_id.to_string()),
                worktree: Some(root.to_path_buf()),
                last_path: None,
                stdout_path: None,
                dispatch_attempt_token: None,
                session_path: session_path.clone(),
                driver_config: plan.config,
                stall_timeout_secs: Some(0),
                max_run_duration_secs: Some(0),
                idle_timeout_secs,
                applicable_states: Vec::new(),
                max_iterations: None,
                planned_identity: None,
            },
        )
        .await?;
    Ok((acquire, session_path))
}

fn acquire_error(error: SupervisorError) -> ApiError {
    match error {
        SupervisorError::LeaseHeld { run_id, .. } => {
            ApiError::conflict(format!("conversation already has a live run {run_id}"))
        }
        other => supervisor_acquire_error("conversation launch", other),
    }
}

async fn send(
    state: &ApiState,
    run_id: &str,
    identity: &RuntimeIdentity,
    text: String,
    context: Option<Value>,
) -> Result<(), ApiError> {
    let ack = state
        .supervisor
        .send_input_with_context(run_id, text, identity, context)
        .await
        .map_err(|error| supervisor_control_error("conversation input", error))?;
    if !ack.accepted {
        return Err(ApiError::conflict(
            ack.message.unwrap_or_else(|| "harness busy".to_string()),
        ));
    }
    Ok(())
}

async fn release_failed_resume(state: &ApiState, run_id: &str) {
    let _ = state
        .supervisor
        .release_with_finalization(
            run_id,
            "conversation_resume_failed",
            ReleaseOutcome::Failed,
            false,
            None,
        )
        .await;
}

// ---- resume ------------------------------------------------------------------

/// The last run's session, located by its first envelope's run id among the
/// conversation's own `conversation-<id>-*.jsonl` files.
// ponytail: scans the sessions dir per continue; index by run id if it grows.
fn prior_session(root: &FsPath, conv: &Conversation) -> Option<PriorSession> {
    let run_id = run_base(conv.runs.last()?).to_string();
    let prefix = format!("conversation-{}-", conv.id);
    for entry in std::fs::read_dir(project_sessions_dir(root))
        .ok()?
        .flatten()
    {
        let path = entry.path();
        let name = path.file_name()?.to_string_lossy().to_string();
        if !name.starts_with(&prefix) || !name.ends_with(".jsonl") {
            continue;
        }
        let first = {
            use std::io::BufRead as _;
            let file = std::fs::File::open(&path).ok()?;
            std::io::BufReader::new(file).lines().next()?.ok()?
        };
        let owned = serde_json::from_str::<Value>(&first)
            .ok()
            .is_some_and(|value| value["run_id"] == run_id);
        if !owned {
            continue;
        }
        let envelopes = read_session_file(&path).ok()?;
        return Some(PriorSession {
            run_id,
            path,
            envelopes,
        });
    }
    None
}

/// Try to give the new run the harness's own memory of the prior session.
/// `Ok(None)` means "no native memory available here; go cold".
#[allow(clippy::too_many_arguments)]
async fn try_resume(
    state: &ApiState,
    project_id: &str,
    root: &FsPath,
    conv: &Conversation,
    prior: &PriorSession,
    request_id: &str,
    chips: &[ContextChip],
    message: &str,
    context: Option<Value>,
) -> Result<Option<String>, ApiError> {
    let text = compose(None, None, chips, message);
    if let Some(native) = session_native_runtime(&prior.envelopes) {
        if let Some(session_id) =
            validated_claude_native_session_id(state.trusted_claude_binary.as_ref(), &native)
        {
            if let Some(plan) = claude_native_plan(state, root, &session_id)? {
                let (acquire, session_path) = match acquire_run(
                    state,
                    project_id,
                    root,
                    &conv.id,
                    &conv.purpose,
                    plan,
                )
                .await
                {
                    Ok(acquired) => acquired,
                    Err(error @ SupervisorError::LeaseHeld { .. }) => {
                        return Err(acquire_error(error))
                    }
                    Err(error) => {
                        tracing::warn!(conversation = %conv.id, error = %error, "claude native resume failed; continuing cold");
                        return Ok(None);
                    }
                };
                state
                    .supervisor
                    .write_recovery_origin(
                        &acquire.run_id,
                        &session_path,
                        &acquire.identity,
                        project_id,
                        &prior.run_id,
                        &prior.path,
                        request_id,
                        "conversation.resume",
                        &conv.id,
                        None,
                    )
                    .await
                    .map_err(|error| {
                        supervisor_control_error("conversation resume origin", error)
                    })?;
                return match send(state, &acquire.run_id, &acquire.identity, text, context).await {
                    Ok(()) => Ok(Some(acquire.run_id)),
                    Err(error) => {
                        tracing::warn!(conversation = %conv.id, error = %error.message, "claude native resume send failed; continuing cold");
                        release_failed_resume(state, &acquire.run_id).await;
                        Ok(None)
                    }
                };
            }
        }
    }
    let Some(resume) = orgasmic_drivers::acp_session_resume(&prior.envelopes) else {
        return Ok(None);
    };
    if !resume.load_session {
        return Ok(None);
    }
    let plan = match plan_launch(
        state,
        root,
        "chat",
        &resume.provider,
        conv.model.clone(),
        conv.effort.clone(),
        conv.access.clone(),
        None,
        &[],
        Some(&resume.session_id),
    ) {
        Ok(plan) => plan,
        Err(error) => {
            tracing::warn!(conversation = %conv.id, error = %error.message, "acp resume address rejected; continuing cold");
            return Ok(None);
        }
    };
    let (acquire, _) = match acquire_run(state, project_id, root, &conv.id, &conv.purpose, plan)
        .await
    {
        Ok(acquired) => acquired,
        Err(error @ SupervisorError::LeaseHeld { .. }) => return Err(acquire_error(error)),
        Err(error) => {
            tracing::warn!(conversation = %conv.id, error = %error, "acp session/load failed; continuing cold");
            return Ok(None);
        }
    };
    match send(state, &acquire.run_id, &acquire.identity, text, context).await {
        Ok(()) => Ok(Some(acquire.run_id)),
        Err(error) => {
            tracing::warn!(conversation = %conv.id, error = %error.message, "acp resume send failed; continuing cold");
            release_failed_resume(state, &acquire.run_id).await;
            Ok(None)
        }
    }
}

/// The daemon-owned Claude fork command, exactly as run recovery builds it.
fn claude_native_plan(
    state: &ApiState,
    root: &FsPath,
    session_id: &str,
) -> Result<Option<LaunchPlan>, ApiError> {
    let Some((command, args)) =
        reconstruct_claude_native_resume_command(state.trusted_claude_binary.as_ref(), session_id)
    else {
        return Ok(None);
    };
    let Some(pin) = state
        .trusted_claude_binary
        .as_ref()
        .filter(|pin| pin.revalidate())
    else {
        return Ok(None);
    };
    let execution =
        pinned_claude_execution_config(&state.home, pin, state.trusted_exec_wrapper.as_deref())?;
    let Some(driver) = resolve_driver("tmux", "claude") else {
        return Ok(None);
    };
    let config = DriverConfig::from_value(json!({
        "command": command.to_string_lossy(),
        "args": args,
        "harness": "claude",
        "cwd": root,
        "force_inert": false,
        "native_resume_mode": true,
        "trusted_provider_identity": execution["trusted_provider_identity"].clone(),
        "pinned_executable": execution["pinned_executable"].clone(),
        "provider_home": execution["provider_home"].clone(),
    }));
    Ok(Some(LaunchPlan { driver, config }))
}

// ---- message composition ------------------------------------------------------

fn validate_chips(chips: &[ContextChip]) -> Result<(), ApiError> {
    fn bounded(value: &str) -> Result<(), ApiError> {
        if value.is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
            return Err(ApiError::bad_request(
                "context chip ids must be bounded single lines",
            ));
        }
        Ok(())
    }
    for chip in chips {
        match chip {
            ContextChip::Node { id } => bounded(id)?,
            ContextChip::Attachment { node, id, .. } => {
                bounded(node)?;
                bounded(id)?;
            }
            ContextChip::Range {
                node,
                attachment,
                start_ms,
                end_ms,
                ..
            } => {
                bounded(node)?;
                bounded(attachment)?;
                if end_ms <= start_ms {
                    return Err(ApiError::bad_request("range chip end must follow start"));
                }
            }
            ContextChip::Selection { text } => {
                if text.len() > SELECTION_LIMIT_BYTES {
                    return Err(ApiError::bad_request(format!(
                        "selection chip exceeds {SELECTION_LIMIT_BYTES} bytes"
                    )));
                }
            }
        }
    }
    Ok(())
}

/// One JSON chip per line inside a delimited block; always present so the
/// operator's own text is the part after the block.
pub(crate) fn chips_block(chips: &[ContextChip]) -> String {
    let mut out = String::from(CONTEXT_OPEN);
    out.push('\n');
    for chip in chips {
        out.push_str(&serde_json::to_string(chip).unwrap_or_default());
        out.push('\n');
    }
    out.push_str(CONTEXT_CLOSE);
    out.push('\n');
    out
}

/// Scope context (first / cold sends), bounded tail (cold sends), chips
/// block, then the operator's message.
pub(crate) fn compose(
    scope: Option<&str>,
    tail: Option<&str>,
    chips: &[ContextChip],
    message: &str,
) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(scope) = scope.map(str::trim).filter(|scope| !scope.is_empty()) {
        parts.push(scope.to_string());
    }
    if let Some(tail) = tail.map(str::trim).filter(|tail| !tail.is_empty()) {
        parts.push(format!(
            "Earlier in this conversation (bounded tail, oldest first):\n{tail}"
        ));
    }
    parts.push(format!("{}{}", chips_block(chips), message.trim()));
    parts.join("\n\n")
}

/// The operator's own text out of a composed send.
fn operator_message(text: &str) -> &str {
    let close = format!("{CONTEXT_CLOSE}\n");
    text.rsplit_once(close.as_str())
        .map(|(_, message)| message)
        .unwrap_or(text)
}

/// Operator turns (from composer_send records) and assistant text (text
/// chunks and ACP agent message chunks), labelled, bounded to the last
/// [`DISPATCH_SUMMARY_TAIL_BYTES`].
pub(crate) fn transcript_tail(envelopes: &[SessionEnvelope]) -> Option<String> {
    let mut turns: Vec<(&'static str, String)> = Vec::new();
    let assistant = |turns: &mut Vec<(&'static str, String)>, text: &str| {
        if text.is_empty() {
            return;
        }
        match turns.last_mut() {
            Some((who, last)) if *who == "Assistant" => last.push_str(text),
            _ => turns.push(("Assistant", text.to_string())),
        }
    };
    for envelope in envelopes {
        match envelope.kind {
            SessionEventKind::Lifecycle => {
                if let Ok(Lifecycle::ComposerSend { text, .. }) =
                    serde_json::from_value::<Lifecycle>(envelope.event.clone())
                {
                    let message = operator_message(&text).trim();
                    if !message.is_empty() {
                        turns.push(("Operator", message.to_string()));
                    }
                }
            }
            SessionEventKind::DriverEvent => match envelope.event["type"].as_str() {
                Some("text_chunk") if envelope.event["stream"] == "assistant" => {
                    assistant(
                        &mut turns,
                        envelope.event["chunk"].as_str().unwrap_or_default(),
                    );
                }
                Some("acp") if envelope.event["message"]["method"] == "session/update" => {
                    let update = &envelope.event["message"]["params"]["update"];
                    if update["sessionUpdate"] == "agent_message_chunk"
                        && update["content"]["type"] == "text"
                    {
                        assistant(
                            &mut turns,
                            update["content"]["text"].as_str().unwrap_or_default(),
                        );
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }
    if turns.is_empty() {
        return None;
    }
    let joined = turns
        .iter()
        .map(|(who, text)| format!("{who}: {}", text.trim()))
        .collect::<Vec<_>>()
        .join("\n\n");
    tail_summary(&joined)
}

// ---- scope context ---------------------------------------------------------

/// Compile the conversation's scope context: the node's chat prompt spec
/// (or `project-chat` without a node) with the node, comments, links, and
/// conversation slots filled.
async fn scope_context(
    state: &ApiState,
    project_id: &str,
    conversation_id: &str,
    purpose: &str,
    scope: Option<&str>,
) -> Result<String, ApiError> {
    let (_, snap) = ensure_loaded_snapshot(state, Some(project_id)).await?;
    let project = select_loaded_project(&snap, project_id)?;
    let mut values = SlotValues::new();
    values.insert("conversation.purpose".into(), purpose.to_string());
    let spec = match scope {
        Some(node_id) => {
            let layer = resolve_node_layer(&state.node_types, None, node_id)?;
            let descriptor = layer
                .collection_name()
                .and_then(|collection| state.node_types.descriptor(collection));
            let (_, path, _) = org_node_path(state, Some(project_id), node_id, layer).await?;
            let content = if layer == NodeKind::Artifact {
                assemble_artifact_context(state, project_id, &[node_id.to_string()]).await
            } else {
                std::fs::read_to_string(&path).unwrap_or_default()
            };
            let journal = path
                .parent()
                .map(|dir| std::fs::read_to_string(dir.join(JOURNAL_FILE)).unwrap_or_default())
                .unwrap_or_default();
            values.insert("node.id".into(), node_id.to_string());
            values.insert(
                "node.type".into(),
                descriptor
                    .map(|d| d.label.clone())
                    .unwrap_or_else(|| layer.layer_name().to_string()),
            );
            values.insert("node.content".into(), prompt_value_or_not_set(content));
            values.insert("node.comments".into(), open_comment_context(&journal)?);
            values.insert("node.links".into(), links_summary(project, node_id));
            descriptor
                .and_then(|d| d.chat_prompt.clone())
                .unwrap_or_else(|| "node-chat".to_string())
        }
        None => "project-chat".to_string(),
    };
    let compiled = crate::prompt_compiler::compile_prompt_spec(
        &state.home,
        &spec,
        crate::prompt_compiler::PromptCompileRequest {
            project: Some(project_id.to_string()),
            mode: Some("conversation".to_string()),
            worker: Some("manager".to_string()),
            reason: Some(format!("conversation {conversation_id} ({purpose})")),
            values,
            ..Default::default()
        },
    )
    .map_err(|error| content_list_error(error, "conversation prompt spec"))?;
    if crate::prompt_compiler::has_error(&compiled.diagnostics) {
        let messages = compiled
            .diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.level == "error")
            .map(|diagnostic| diagnostic.message.as_str())
            .collect::<Vec<_>>()
            .join("; ");
        return Err(ApiError::internal(format!(
            "conversation prompt compile failed: {messages}"
        )));
    }
    Ok(format!(
        "orgasmic compiled prompt\ndispatch_kind: conversation\nconversation: {conversation_id}\nworker: manager\nprompt_spec: {}\n\n{}\n",
        compiled.spec.id,
        compiled.text.trim()
    ))
}

/// Linked ids and titles in both directions from the project's links index.
fn links_summary(project: &ProjectIndex, node_id: &str) -> String {
    let title = |id: &str| {
        project
            .graph
            .nodes
            .iter()
            .find(|node| node.id == id)
            .map(|node| node.title.clone())
            .or_else(|| {
                project
                    .tasks
                    .iter()
                    .find(|task| task.id == id)
                    .map(|task| task.title.clone())
            })
            .unwrap_or_default()
    };
    let mut lines = Vec::new();
    for link in project.graph.links.iter().filter(|link| !link.deleted) {
        if link.source == node_id {
            lines.push(format!(
                "- {} -> {} {}",
                link.kind,
                link.target,
                title(&link.target)
            ));
        } else if link.target == node_id {
            lines.push(format!(
                "- {} <- {} {}",
                link.kind,
                link.source,
                title(&link.source)
            ));
        }
    }
    if lines.is_empty() {
        "none".to_string()
    } else {
        lines.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn envelope(kind: SessionEventKind, event: Value) -> SessionEnvelope {
        SessionEnvelope {
            seq: 0,
            time: Utc::now(),
            run_id: "run-1".into(),
            runtime_id: "rt-1".into(),
            boot_id: "boot-1".into(),
            kind,
            event,
        }
    }

    #[test]
    fn chips_block_renders_one_json_line_per_chip_and_caps_selection() {
        let chips = vec![
            ContextChip::Node {
                id: "TASK-1".into(),
            },
            ContextChip::Range {
                node: "MTG-1".into(),
                attachment: "att".into(),
                revision: 2,
                start_ms: 10,
                end_ms: 20,
            },
            ContextChip::Selection {
                text: "two\nlines".into(),
            },
        ];
        let block = chips_block(&chips);
        let lines: Vec<&str> = block.lines().collect();
        assert_eq!(lines[0], CONTEXT_OPEN);
        assert_eq!(lines[1], r#"{"kind":"node","id":"TASK-1"}"#);
        assert!(lines[2].contains(r#""kind":"range""#) && lines[2].contains(r#""end_ms":20"#));
        assert_eq!(lines[3], r#"{"kind":"selection","text":"two\nlines"}"#);
        assert_eq!(lines[4], CONTEXT_CLOSE);
        assert_eq!(lines.len(), 5);
        assert!(validate_chips(&chips).is_ok());

        let composed = compose(Some("scope"), Some("tail"), &chips, " hello ");
        assert!(composed.starts_with("scope\n\nEarlier in this conversation"));
        assert!(composed.ends_with(">>>\nhello"));
        assert_eq!(operator_message(&composed), "hello");
        assert_eq!(operator_message("plain"), "plain");

        let big = ContextChip::Selection {
            text: "x".repeat(SELECTION_LIMIT_BYTES + 1),
        };
        assert!(validate_chips(&[big]).is_err());
        assert!(validate_chips(&[ContextChip::Selection {
            text: "x".repeat(SELECTION_LIMIT_BYTES)
        }])
        .is_ok());
        let err: Result<ContextChip, _> =
            serde_json::from_str(r#"{"kind":"node","id":"x","extra":1}"#);
        assert!(err.is_err(), "unknown chip fields are refused");
    }

    #[test]
    fn transcript_tail_keeps_operator_turns_and_is_bounded() {
        let mut envelopes = vec![
            envelope(
                SessionEventKind::Lifecycle,
                json!({"phase":"composer_send","text": compose(Some("SCOPE"), None, &[], "first question")}),
            ),
            envelope(
                SessionEventKind::DriverEvent,
                json!({"type":"acp","provider":"hermes","message":{"method":"session/update","params":{"update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"hel"}}}}}),
            ),
            envelope(
                SessionEventKind::DriverEvent,
                json!({"type":"acp","provider":"hermes","message":{"method":"session/update","params":{"update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"lo"}}}}}),
            ),
            envelope(
                SessionEventKind::DriverEvent,
                json!({"type":"acp","provider":"hermes","message":{"method":"session/update","params":{"update":{"sessionUpdate":"agent_thought_chunk","content":{"type":"text","text":"secret thinking"}}}}}),
            ),
            envelope(
                SessionEventKind::Lifecycle,
                json!({"phase":"composer_send","text": compose(None, None, &[ContextChip::Node { id: "TASK-1".into() }], "second question"), "context": [{"kind":"node","id":"TASK-1"}]}),
            ),
            envelope(
                SessionEventKind::DriverEvent,
                json!({"type":"text_chunk","stream":"assistant","chunk":"answer two","seq":1}),
            ),
            envelope(
                SessionEventKind::DriverEvent,
                json!({"type":"text_chunk","stream":"user","chunk":"raw echo","seq":2}),
            ),
        ];
        let tail = transcript_tail(&envelopes).unwrap();
        assert_eq!(
            tail,
            "Operator: first question\n\nAssistant: hello\n\nOperator: second question\n\nAssistant: answer two"
        );
        assert!(!tail.contains("SCOPE") && !tail.contains("secret") && !tail.contains("raw echo"));

        let huge = "y".repeat(DISPATCH_SUMMARY_TAIL_BYTES * 2);
        envelopes.push(envelope(
            SessionEventKind::DriverEvent,
            json!({"type":"text_chunk","stream":"assistant","chunk":huge,"seq":3}),
        ));
        envelopes.push(envelope(
            SessionEventKind::Lifecycle,
            json!({"phase":"composer_send","text":"last question"}),
        ));
        let bounded = transcript_tail(&envelopes).unwrap();
        assert!(bounded.starts_with("[…transcript truncated"));
        assert!(bounded.ends_with("Operator: last question"));
        assert!(bounded.len() <= DISPATCH_SUMMARY_TAIL_BYTES + 64);
        assert!(transcript_tail(&[]).is_none());
    }

    #[test]
    fn composer_send_context_round_trips() {
        let event = Lifecycle::ComposerSend {
            text: "hi".into(),
            context: Some(json!([{"kind":"selection","text":"quoted"}])),
        };
        let value = serde_json::to_value(&event).unwrap();
        assert_eq!(value["context"][0]["kind"], "selection");
        let back: Lifecycle = serde_json::from_value(value).unwrap();
        assert_eq!(back, event);
        let bare: Lifecycle =
            serde_json::from_value(json!({"phase":"composer_send","text":"old"})).unwrap();
        assert_eq!(
            bare,
            Lifecycle::ComposerSend {
                text: "old".into(),
                context: None
            }
        );
        assert!(!serde_json::to_string(&bare).unwrap().contains("context"));
    }

    #[test]
    fn purpose_and_run_entries_are_validated() {
        assert!(validate_purpose("discuss").is_ok());
        assert!(validate_purpose("meetings.summarize").is_ok());
        assert!(validate_purpose("").is_err());
        assert!(validate_purpose("Has Space").is_err());
        assert_eq!(run_base("run-1:cold"), "run-1");
        assert_eq!(run_base("run-1"), "run-1");
    }
}

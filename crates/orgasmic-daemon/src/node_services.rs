//! HTTP boundary for core.links and core.attachments. Shares the node resolver,
//! per-node locks, schema/ownership gate, and serialized ledger writer.
use super::*;
use axum::{
    body::{Body, Bytes},
    extract::DefaultBodyLimit,
};
use orgasmic_core::node_services::{self as records, AttachmentRecord, LinkRecord, MediaAnchor};
use orgasmic_core::schema::{AttachmentStorage, SchemaError};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

const CHUNK_LIMIT: usize = 4 * 1024 * 1024;
const FILE_LIMIT: u64 = 8 * 1024 * 1024 * 1024;
const STORE_LIMIT: u64 = 64 * 1024 * 1024 * 1024;
const MEDIA_TYPES: &[&str] = &[
    "audio/wav",
    "audio/mpeg",
    "audio/ogg",
    "video/ogg",
    "audio/mp4",
    "video/mp4",
    "audio/webm",
    "video/webm",
    "image/png",
    "image/jpeg",
    "application/pdf",
    "text/plain",
];

pub(super) const ROUTES: &[(&str, &str)] = &[
    ("GET", "/links"),
    ("POST", "/links"),
    ("GET", "/attachments"),
    ("POST", "/attachments/uploads"),
    ("GET", "/attachments/uploads/:id"),
    ("PUT", "/attachments/uploads/:id"),
    ("DELETE", "/attachments/uploads/:id"),
    ("POST", "/attachments/uploads/:id/finish"),
    ("GET", "/attachments/:node/:id/:revision/content"),
    ("HEAD", "/attachments/:node/:id/:revision/content"),
];

pub(super) fn routes() -> Router<ApiState> {
    Router::new()
        .route("/links", get(get_links).post(post_link))
        .route("/attachments", get(get_attachments))
        .route("/attachments/uploads", post(start_upload))
        .route(
            "/attachments/uploads/:id",
            get(get_upload).put(put_chunk).delete(cancel_upload),
        )
        .route("/attachments/uploads/:id/finish", post(finish_upload))
        .route("/attachments/:node/:id/:revision/content", get(get_content))
        .layer(DefaultBodyLimit::max(CHUNK_LIMIT))
}

#[derive(Deserialize)]
struct ServiceQuery {
    project: String,
    #[serde(default)]
    node: String,
    #[serde(default)]
    incoming: bool,
    #[serde(default)]
    include_deleted: bool,
    offset: Option<u64>,
}

pub(super) struct Node {
    pub(super) path: PathBuf,
    ledger: PathBuf,
}
pub(super) async fn node(
    state: &ApiState,
    identity: &Identity,
    project: &str,
    id: &str,
    action: Action,
    write: bool,
) -> Result<Node, ApiError> {
    let (state, plugins) = node_scope(state.clone(), identity, Some(project), action).await?;
    let layer = resolve_node_layer(&state.node_types, None, id)?;
    // Service permission does not grant access to an otherwise hidden node kind.
    authz::require(
        identity,
        Some(project),
        if layer == NodeKind::Artifact {
            Action::ArtifactsRead
        } else {
            Action::GraphRead
        },
    )?;
    let collection = layer.collection_name().map(str::to_string);
    let (_, path, _) = org_node_path(&state, Some(project), id, layer).await?;
    let (_, snap) = ensure_loaded_snapshot(&state, Some(project)).await?;
    let root = select_loaded_project(&snap, project)?.root.clone();
    drop(snap);
    guard_plugin_conversation(&state, identity, Some(project), &plugins, id).await?;
    let canonical = path
        .canonicalize()
        .map_err(|_| ApiError::not_found("node unavailable"))?;
    let ledger = root.join(".orgasmic").canonicalize().map_err(internal)?;
    if !canonical.starts_with(&ledger) {
        return Err(ApiError::not_found("node unavailable"));
    }
    let source = std::fs::read_to_string(&canonical).map_err(internal)?;
    let file = OrgFile::parse(&source, "node.org").map_err(bad)?;
    if file.find_by_id(id).is_none() {
        return Err(ApiError::not_found("node unavailable"));
    }
    if write {
        let collection = collection.as_deref().ok_or_else(|| {
            ApiError::bad_request("singleton nodes have no writable service records")
        })?;
        plugin_collection_write(identity, &plugins, Some(collection))?;
        plugins
            .check_write(collection, Some(&source))
            .map_err(bad)?;
    }
    Ok(Node { path, ledger })
}
fn bad(e: impl std::fmt::Display) -> ApiError {
    ApiError::bad_request(e.to_string())
}
fn internal(e: impl std::fmt::Display) -> ApiError {
    ApiError::internal(e.to_string())
}
fn optional_text(path: &FsPath) -> Result<String, ApiError> {
    if std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(bad("service records must not be symlinks"));
    }
    match std::fs::read_to_string(path) {
        Ok(s) => Ok(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(internal(e)),
    }
}
fn single_line(s: &str, max: usize) -> Result<(), ApiError> {
    if s.len() > max || s.chars().any(char::is_control) {
        return Err(ApiError::bad_request("value must be a bounded single line"));
    }
    Ok(())
}
pub(super) fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}
fn uuid(value: &str) -> Result<(), ApiError> {
    if uuid::Uuid::parse_str(value).is_err() || value.len() != 36 {
        return Err(ApiError::bad_request("invalid upload/attachment id"));
    }
    Ok(())
}
pub(super) fn actor_key(identity: &Identity) -> String {
    match identity {
        Identity::Admin => "admin".into(),
        Identity::Member { name, .. } => json!(["member", name]).to_string(),
        Identity::Plugin { id, caller, .. } => json!(["plugin", id, actor_key(caller)]).to_string(),
    }
}

pub(super) async fn commit_extra(
    state: &ApiState,
    identity: &Identity,
    path: PathBuf,
    mut request: ApiTxRequest,
    payload: Value,
    transform: impl FnOnce(&str) -> anyhow::Result<String> + Send + 'static,
) -> Result<String, ApiError> {
    // The writer owns mutation ordering; reject local symlink escapes before it opens an extra.
    if std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(bad("service records must not be symlinks"));
    }
    let project = request
        .project
        .clone()
        .ok_or_else(|| internal("service project required"))?;
    let node_id = request
        .target
        .clone()
        .ok_or_else(|| internal("service node required"))?;
    let ty = request.ty.clone();
    request.extra.push(("NODE_ID".into(), node_id.clone()));
    let mutation = mutation_identity(&ty, &project, payload)?;
    let prepared = prepare_api_tx_as(state, identity, request).await?;
    let result = state
        .writer
        .transaction_mutate_file_mutation(
            FileMutate {
                path,
                transform: Box::new(move |bytes| Ok(transform(bytes)?.into_bytes())),
            },
            prepared.tx,
            mutation,
            node_id.clone(),
        )
        .await
        .map_err(|error| {
            if error.downcast_ref::<LinkConflict>().is_some() {
                ApiError::conflict("link changed; reload before editing")
            } else {
                writer_transaction_error(error)
            }
        })?;
    refresh_after_project_mutation(state, &project, prepared.project_tx, &result.tx_id).await?;
    state.events.publish(
        Topic::Graph,
        EventPayload::GraphNodeRevised {
            project_id: project,
            layer: "node-services".into(),
            node_id,
            action: ty,
            tx_id: result.tx_id.clone(),
        },
    );
    Ok(result.tx_id)
}

async fn get_links(
    State(state): State<ApiState>,
    Extension(identity): Extension<Identity>,
    Query(q): Query<ServiceQuery>,
) -> Result<Json<Vec<LinkRecord>>, ApiError> {
    node(
        &state,
        &identity,
        &q.project,
        &q.node,
        Action::LinksRead,
        false,
    )
    .await?;
    let (_, snap) = ensure_loaded_snapshot(&state, Some(&q.project)).await?;
    let project = select_loaded_project(&snap, &q.project)?;
    Ok(Json(
        project
            .graph
            .links
            .iter()
            .filter(|r| {
                (!r.deleted || (q.include_deleted && !q.incoming))
                    && if q.incoming {
                        r.target == q.node
                    } else {
                        r.source == q.node
                    }
            })
            .cloned()
            .collect(),
    ))
}

#[derive(Debug, thiserror::Error)]
#[error("link changed; reload before editing")]
struct LinkConflict;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct LinkWrite {
    project: String,
    source: String,
    target: String,
    kind: String,
    #[serde(default)]
    anchors: Vec<MediaAnchor>,
    base_revision: u64,
    #[serde(default)]
    deleted: bool,
    request_id: String,
}
async fn post_link(
    State(state): State<ApiState>,
    Extension(identity): Extension<Identity>,
    Json(req): Json<LinkWrite>,
) -> Result<Json<Value>, ApiError> {
    let _guard = state.plugins.operations.clone().read_owned().await;
    let source = node(
        &state,
        &identity,
        &req.project,
        &req.source,
        Action::LinksWrite,
        true,
    )
    .await?;
    let target = node(
        &state,
        &identity,
        &req.project,
        &req.target,
        Action::LinksRead,
        false,
    )
    .await?;
    if req.source == req.target
        || !["RELATES_TO", "PRODUCES"].contains(&req.kind.as_str())
        || req.anchors.len() > 256
    {
        return Err(ApiError::bad_request(
            "links require distinct nodes, RELATES_TO/PRODUCES, and at most 256 anchors",
        ));
    }
    let dir = source.path.parent().unwrap();
    let lock = state.node_write_lock(dir);
    let _lock = lock.lock().await;
    // Re-resolve under the same node lock used by edit/delete.
    node(
        &state,
        &identity,
        &req.project,
        &req.source,
        Action::LinksWrite,
        true,
    )
    .await?;
    // An anchor may point into media on either end of the link (C2): a
    // conversation anchors moments in the meeting it is about.
    let mut assets =
        records::read_attachments(&optional_text(&dir.join("attachments.org"))?).map_err(bad)?;
    assets.extend(
        records::read_attachments(&optional_text(
            &target.path.parent().unwrap().join("attachments.org"),
        )?)
        .map_err(bad)?,
    );
    for anchor in &req.anchors {
        single_line(&anchor.label, 512)?;
        if anchor.start_ms > 9_007_199_254_740_991
            || anchor
                .end_ms
                .is_some_and(|end| end <= anchor.start_ms || end > 9_007_199_254_740_991)
        {
            return Err(ApiError::bad_request("invalid media anchor range"));
        }
        if !media_revision_owned(&assets, &anchor.attachment, &anchor.revision) {
            return Err(ApiError::bad_request(
                "anchor must reference an immutable media revision owned by the source or target node",
            ));
        }
    }
    let id = digest(format!("{}\0{}", req.source, req.target).as_bytes());
    let next = LinkRecord {
        id: id.clone(),
        source: req.source.clone(),
        target: req.target.clone(),
        kind: req.kind.clone(),
        revision: req
            .base_revision
            .checked_add(1)
            .ok_or_else(|| bad("revision overflow"))?,
        deleted: req.deleted,
        anchors: req.anchors.clone(),
        actor: identity
            .member_name()
            .unwrap_or_else(|| state.actor.clone()),
        updated_at: Utc::now().to_rfc3339(),
    };
    let expected = req.base_revision;
    let tx = commit_extra(
        &state,
        &identity,
        dir.join("links.org"),
        ApiTxRequest {
            ty: "link.updated".into(),
            actor: None,
            project: Some(req.project.clone()),
            task: None,
            target: Some(req.source.clone()),
            reason: "link.updated".into(),
            request_id: Some(req.request_id.clone()),
            extra: vec![],
        },
        json!(req),
        move |source| {
            let mut links = records::read_links(source)?;
            let old = links.iter().find(|r| r.id == next.id);
            if old.map_or(0, |r| r.revision) != expected {
                return Err(LinkConflict.into());
            }
            links.retain(|r| r.id != next.id);
            links.push(next);
            Ok(records::render_links(&links))
        },
    )
    .await?;
    Ok(Json(json!({"id": id, "tx_id": tx})))
}

/// The exact-revision, audio/video rule every media anchor must satisfy.
pub(super) fn media_revision_owned(
    assets: &[records::AttachmentRecord],
    attachment: &str,
    revision: &str,
) -> bool {
    assets.iter().any(|a| {
        a.id == attachment
            && a.revision == revision
            && (a.media_type.starts_with("audio/") || a.media_type.starts_with("video/"))
    })
}

/// Append `anchors` to the live `source -> target` link beside `source`,
/// deduped on attachment+revision+range, through the `POST /links` write
/// path (revision bump, index refresh, graph event). `Ok(false)` when every
/// anchor was already on file and nothing was written.
pub(super) async fn append_link_anchors(
    state: &ApiState,
    identity: &Identity,
    project: &str,
    source_dir: &FsPath,
    source: &str,
    target: &str,
    anchors: Vec<MediaAnchor>,
) -> Result<bool, ApiError> {
    let same = |a: &MediaAnchor, b: &MediaAnchor| {
        a.attachment == b.attachment
            && a.revision == b.revision
            && a.start_ms == b.start_ms
            && a.end_ms == b.end_ms
    };
    let id = digest(format!("{source}\0{target}").as_bytes());
    let lock = state.node_write_lock(source_dir);
    let _lock = lock.lock().await;
    let path = source_dir.join("links.org");
    let links = records::read_links(&optional_text(&path)?).map_err(bad)?;
    let Some(link) = links.iter().find(|r| r.id == id && !r.deleted) else {
        return Err(bad("scope link is missing"));
    };
    let fresh: Vec<MediaAnchor> = anchors.into_iter().fold(Vec::new(), |mut fresh, anchor| {
        if !link
            .anchors
            .iter()
            .chain(fresh.iter())
            .any(|b| same(b, &anchor))
        {
            fresh.push(anchor);
        }
        fresh
    });
    if fresh.is_empty() {
        return Ok(false);
    }
    let actor = identity
        .member_name()
        .unwrap_or_else(|| state.actor.clone());
    let payload = json!({"source": source, "target": target, "anchors": fresh});
    commit_extra(
        state,
        identity,
        path,
        ApiTxRequest {
            ty: "link.updated".into(),
            actor: None,
            project: Some(project.to_string()),
            task: None,
            target: Some(source.to_string()),
            reason: "link.updated".into(),
            request_id: None,
            extra: vec![],
        },
        payload,
        move |current| {
            let mut links = records::read_links(current)?;
            let link = links
                .iter_mut()
                .find(|r| r.id == id && !r.deleted)
                .ok_or_else(|| anyhow::anyhow!("scope link is missing"))?;
            for anchor in fresh {
                if !link.anchors.iter().any(|b| same(b, &anchor)) {
                    link.anchors.push(anchor);
                }
            }
            link.revision += 1;
            link.actor = actor;
            link.updated_at = Utc::now().to_rfc3339();
            Ok(records::render_links(&links))
        },
    )
    .await?;
    Ok(true)
}

fn store_root(state: &ApiState, project: &str) -> PathBuf {
    state
        .home
        .root
        .join("assets")
        .join(digest(project.as_bytes()))
}
#[derive(Deserialize, Serialize, Clone)]
struct Upload {
    id: String,
    node: String,
    actor: String,
    name: String,
    size: u64,
    media_type: String,
    complete: Option<AttachmentRecord>,
}
fn upload_dir(base: &FsPath, id: &str) -> PathBuf {
    base.join("uploads").join(id)
}
fn load_upload(base: &FsPath, id: &str, identity: &Identity) -> Result<Upload, ApiError> {
    uuid(id)?;
    let source = optional_text(&upload_dir(base, id).join("state.json"))?;
    let upload: Upload =
        serde_json::from_str(&source).map_err(|_| ApiError::not_found("upload unavailable"))?;
    if upload.actor != actor_key(identity) {
        return Err(ApiError::forbidden("upload belongs to another principal"));
    }
    Ok(upload)
}
fn save_upload(base: &FsPath, upload: &Upload) -> Result<(), ApiError> {
    let dir = upload_dir(base, &upload.id);
    let temporary = dir.join("state.next");
    let mut file = std::fs::File::create(&temporary).map_err(internal)?;
    std::io::Write::write_all(&mut file, &serde_json::to_vec(upload).map_err(internal)?)
        .map_err(internal)?;
    file.sync_all().map_err(internal)?;
    std::fs::rename(temporary, dir.join("state.json")).map_err(internal)?;
    Ok(())
}
fn upload_view(base: &FsPath, upload: &Upload) -> Result<Value, ApiError> {
    let offset = if upload.complete.is_some() {
        upload.size
    } else {
        std::fs::metadata(upload_dir(base, &upload.id).join("payload"))
            .map_err(internal)?
            .len()
    };
    Ok(
        json!({"id": upload.id, "node": upload.node, "size": upload.size, "offset": offset, "chunk_limit": CHUNK_LIMIT, "complete": upload.complete}),
    )
}
#[derive(Deserialize)]
struct UploadStart {
    project: String,
    node: String,
    name: String,
    size: u64,
    media_type: String,
    request_id: String,
}
async fn start_upload(
    State(state): State<ApiState>,
    Extension(identity): Extension<Identity>,
    Json(req): Json<UploadStart>,
) -> Result<Json<Value>, ApiError> {
    let _guard = state.plugins.operations.clone().read_owned().await;
    let owner = node(
        &state,
        &identity,
        &req.project,
        &req.node,
        Action::AttachmentsWrite,
        true,
    )
    .await?;
    uuid(&req.request_id)?;
    single_line(&req.name, 255)?;
    if req.name.trim().is_empty()
        || req.size == 0
        || req.size > FILE_LIMIT
        || !MEDIA_TYPES.contains(&req.media_type.as_str())
    {
        return Err(bad("unsupported media type or size (maximum 8 GiB)"));
    }
    let base = store_root(&state, &req.project);
    let lock = state.node_write_lock(&base);
    let _lock = lock.lock().await;
    let dir = upload_dir(&base, &req.request_id);
    if dir.join("state.json").exists() {
        let upload = load_upload(&base, &req.request_id, &identity)?;
        if upload.node != req.node
            || upload.name != req.name
            || upload.size != req.size
            || upload.media_type != req.media_type
        {
            return Err(ApiError::conflict("upload request id was reused"));
        }
        return Ok(Json(upload_view(&base, &upload)?));
    }
    let mut reserved = 0u64;
    // ponytail: quota is a metadata scan on upload creation only; index it if upload volume warrants it.
    // The ledger is a live worktree the sync loop rewrites; an entry that
    // vanishes mid-walk is skipped, not a failed upload.
    let dirs = |path: PathBuf| std::fs::read_dir(path).into_iter().flatten().flatten();
    for collection in dirs(owner.ledger.clone()) {
        for node in dirs(collection.path()) {
            for attachment in dirs(node.path().join("attachments")) {
                if attachment.file_name().to_str().is_some_and(valid_digest) {
                    if let Ok(meta) = attachment.metadata() {
                        if meta.is_file() {
                            reserved = reserved.saturating_add(meta.len());
                        }
                    }
                }
            }
        }
    }
    let mut sessions = 0;
    if let Ok(entries) = std::fs::read_dir(base.join("uploads")) {
        for entry in entries {
            let path = entry.map_err(internal)?.path().join("state.json");
            if let Ok(source) = std::fs::read_to_string(path) {
                let upload: Upload = serde_json::from_str(&source).map_err(internal)?;
                if upload.complete.is_none() {
                    sessions += 1;
                    reserved = reserved.saturating_add(upload.size);
                }
            }
        }
    }
    if sessions >= 4096 {
        return Err(bad(
            "project unfinished upload limit reached (4096); resume or cancel pending uploads",
        ));
    }
    if reserved.saturating_add(req.size) > STORE_LIMIT {
        return Err(bad(
            "project asset quota exceeded (64 GiB including reserved uploads)",
        ));
    }
    std::fs::create_dir_all(&dir).map_err(internal)?;
    tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dir.join("payload"))
        .await
        .map_err(internal)?;
    let upload = Upload {
        id: req.request_id,
        node: req.node,
        actor: actor_key(&identity),
        name: req.name,
        size: req.size,
        media_type: req.media_type,
        complete: None,
    };
    if let Err(error) = save_upload(&base, &upload) {
        let _ = std::fs::remove_dir_all(&dir);
        return Err(error);
    }
    Ok(Json(upload_view(&base, &upload)?))
}
async fn upload_scope(
    state: &ApiState,
    identity: &Identity,
    q: &ServiceQuery,
    id: &str,
) -> Result<(PathBuf, Upload), ApiError> {
    let (_, snap) =
        resolve_authorized_project(state, identity, Some(&q.project), Action::AttachmentsWrite)
            .await?;
    select_loaded_project(&snap, &q.project)?;
    let base = store_root(state, &q.project);
    let upload = load_upload(&base, id, identity)?;
    node(
        state,
        identity,
        &q.project,
        &upload.node,
        Action::AttachmentsWrite,
        true,
    )
    .await?;
    Ok((base, upload))
}
async fn get_upload(
    State(state): State<ApiState>,
    Extension(identity): Extension<Identity>,
    Path(id): Path<String>,
    Query(q): Query<ServiceQuery>,
) -> Result<Json<Value>, ApiError> {
    let (base, _) = upload_scope(&state, &identity, &q, &id).await?;
    let lock = state.node_write_lock(&upload_dir(&base, &id));
    let _lock = lock.lock().await;
    let upload = load_upload(&base, &id, &identity)?;
    Ok(Json(upload_view(&base, &upload)?))
}
async fn put_chunk(
    State(state): State<ApiState>,
    Extension(identity): Extension<Identity>,
    Path(id): Path<String>,
    Query(q): Query<ServiceQuery>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Result<Json<Value>, ApiError> {
    let _guard = state.plugins.operations.clone().read_owned().await;
    let (base, _) = upload_scope(&state, &identity, &q, &id).await?;
    let lock = state.node_write_lock(&upload_dir(&base, &id));
    let _lock = lock.lock().await;
    let upload = load_upload(&base, &id, &identity)?;
    if upload.complete.is_some() {
        return Err(ApiError::conflict("upload already finalized"));
    }
    let offset = q.offset.ok_or_else(|| bad("offset required"))?;
    if bytes.is_empty()
        || bytes.len() > CHUNK_LIMIT
        || offset
            .checked_add(bytes.len() as u64)
            .is_none_or(|end| end > upload.size)
    {
        return Err(bad("chunk exceeds declared total or chunk limit"));
    }
    if let Some(hash) = headers.get("x-chunk-sha256") {
        if hash.to_str().ok() != Some(digest(&bytes).as_str()) {
            return Err(bad("chunk checksum mismatch"));
        }
    }
    let mut file = tokio::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(upload_dir(&base, &id).join("payload"))
        .await
        .map_err(internal)?;
    if file.metadata().await.map_err(internal)?.len() != offset {
        return Err(ApiError::conflict(
            "upload offset changed; query the confirmed offset",
        ));
    }
    file.seek(std::io::SeekFrom::Start(offset))
        .await
        .map_err(internal)?;
    if let Err(error) = file.write_all(&bytes).await {
        let _ = file.set_len(offset).await;
        return Err(internal(error));
    }
    if let Err(error) = file.sync_data().await {
        let _ = file.set_len(offset).await;
        return Err(internal(error));
    }
    Ok(Json(
        json!({"id": id, "offset": offset + bytes.len() as u64}),
    ))
}
async fn cancel_upload(
    State(state): State<ApiState>,
    Extension(identity): Extension<Identity>,
    Path(id): Path<String>,
    Query(q): Query<ServiceQuery>,
) -> Result<Json<Value>, ApiError> {
    let _guard = state.plugins.operations.clone().read_owned().await;
    let (base, _) = upload_scope(&state, &identity, &q, &id).await?;
    let lock = state.node_write_lock(&upload_dir(&base, &id));
    let _lock = lock.lock().await;
    let upload = load_upload(&base, &id, &identity)?;
    if upload.complete.is_some() {
        return Err(ApiError::conflict("completed attachments are retained"));
    }
    tokio::fs::remove_dir_all(upload_dir(&base, &id))
        .await
        .map_err(internal)?;
    Ok(Json(json!({"cancelled": true})))
}

fn matches_media(prefix: &[u8], media: &str) -> bool {
    match media {
        "audio/wav" => prefix.starts_with(b"RIFF") && prefix.get(8..12) == Some(b"WAVE"),
        "audio/mpeg" => {
            prefix.starts_with(b"ID3")
                || (prefix.len() >= 2 && prefix[0] == 0xff && prefix[1] & 0xe0 == 0xe0)
        }
        "audio/ogg" | "video/ogg" => prefix.starts_with(b"OggS"),
        "audio/mp4" | "video/mp4" => prefix.get(4..8) == Some(b"ftyp"),
        "audio/webm" | "video/webm" => prefix.starts_with(&[0x1a, 0x45, 0xdf, 0xa3]),
        "image/png" => prefix.starts_with(b"\x89PNG\r\n\x1a\n"),
        "image/jpeg" => prefix.starts_with(&[0xff, 0xd8, 0xff]),
        "application/pdf" => prefix.starts_with(b"%PDF-"),
        "text/plain" => !prefix.contains(&0),
        _ => false,
    }
}
#[derive(Deserialize)]
struct Finish {
    sha256: Option<String>,
}
async fn finish_upload(
    State(state): State<ApiState>,
    Extension(identity): Extension<Identity>,
    Path(id): Path<String>,
    Query(q): Query<ServiceQuery>,
    Json(req): Json<Finish>,
) -> Result<Json<AttachmentRecord>, ApiError> {
    let _guard = state.plugins.operations.clone().read_owned().await;
    let (base, _) = upload_scope(&state, &identity, &q, &id).await?;
    let lock = state.node_write_lock(&upload_dir(&base, &id));
    let _lock = lock.lock().await;
    let mut upload = load_upload(&base, &id, &identity)?;
    if let Some(done) = upload.complete {
        return Ok(Json(done));
    }
    let owner = node(
        &state,
        &identity,
        &q.project,
        &upload.node,
        Action::AttachmentsWrite,
        true,
    )
    .await?;
    let node_lock = state.node_write_lock(owner.path.parent().unwrap());
    let _node_lock = node_lock.lock().await;
    node(
        &state,
        &identity,
        &q.project,
        &upload.node,
        Action::AttachmentsWrite,
        true,
    )
    .await?;
    let payload = upload_dir(&base, &id).join("payload");
    let mut file = tokio::fs::File::open(&payload).await.map_err(internal)?;
    if file.metadata().await.map_err(internal)?.len() != upload.size {
        return Err(ApiError::conflict("upload is incomplete"));
    }
    let mut hash = Sha256::new();
    let mut buffer = vec![0u8; 64 * 1024];
    let mut first = true;
    loop {
        let n = file.read(&mut buffer).await.map_err(internal)?;
        if n == 0 {
            break;
        }
        if first && !matches_media(&buffer[..n], &upload.media_type) {
            return Err(bad("file signature does not match the declared media type"));
        }
        first = false;
        hash.update(&buffer[..n]);
    }
    let revision = format!("{:x}", hash.finalize());
    if req
        .sha256
        .as_ref()
        .is_some_and(|expected| expected != &revision)
    {
        return Err(bad("attachment checksum mismatch"));
    }
    let blob = owner
        .path
        .parent()
        .unwrap()
        .join("attachments")
        .join(&revision);
    let source = payload.clone();
    let repo = owner
        .ledger
        .parent()
        .ok_or_else(|| internal("ledger has no parent; repair the project registration and retry"))?
        .to_path_buf();
    let storage = match crate::ledger_sync::attachment_storage(&repo) {
        Ok(storage) => storage,
        Err(error)
            if matches!(
                error.downcast_ref::<SchemaError>(),
                Some(SchemaError::UnknownAttachmentStorage { .. })
            ) =>
        {
            return Err(bad(error));
        }
        Err(error) => {
            tracing::warn!(project = %q.project, %error, "attachment storage setting could not be read; defaulting to lfs for upload");
            AttachmentStorage::Lfs
        }
    };
    let published = tokio::task::spawn_blocking(move || {
        if storage == AttachmentStorage::Lfs {
            match crate::ledger_sync::attachment_lfs_ready(&repo) {
                Ok(false) => return Ok(None),
                Err(e) => return Err(std::io::Error::other(e.to_string())),
                Ok(true) => {}
            }
        }
        crate::publish_blob(&source, &blob).map(Some)
    })
    .await
    .map_err(internal)?
    .map_err(internal)?;
    if published.is_none() {
        return Err(ApiError::unavailable(
            format!(
                "attachment bytes cannot be published to this project's Git remote because git-lfs is not installed; install git-lfs, or run `orgasmic node prop set {} ATTACHMENT_STORAGE local --kind project --project {}` to set `:ATTACHMENT_STORAGE: local` on the project",
                q.project, q.project
            ),
        ));
    }
    let record = AttachmentRecord {
        id: id.clone(),
        node: upload.node.clone(),
        name: upload.name.clone(),
        revision,
        size: upload.size,
        media_type: upload.media_type.clone(),
        actor: identity
            .member_name()
            .unwrap_or_else(|| state.actor.clone()),
        created_at: Utc::now().to_rfc3339(),
        machine: Some(state.machine.clone()),
    };
    let next = record.clone();
    // Stable request identity makes finish recoverable after blob publication or lost replies.
    commit_extra(
        &state,
        &identity,
        owner.path.parent().unwrap().join("attachments.org"),
        ApiTxRequest {
            ty: "attachment.created".into(),
            actor: None,
            project: Some(q.project.clone()),
            task: None,
            target: Some(upload.node.clone()),
            reason: "attachment.created".into(),
            request_id: Some(format!("attachment-finish-{id}")),
            extra: vec![],
        },
        json!({"id": id, "revision": record.revision}),
        move |source| {
            let mut assets = records::read_attachments(source)?;
            if let Some(old) = assets.iter().find(|a| a.id == next.id) {
                anyhow::ensure!(
                    old.revision == next.revision,
                    "conflict: immutable attachment changed"
                );
            } else {
                assets.push(next);
            }
            Ok(records::render_attachments(&assets))
        },
    )
    .await?;
    let record = records::read_attachments(&optional_text(
        &owner.path.parent().unwrap().join("attachments.org"),
    )?)
    .map_err(internal)?
    .into_iter()
    .find(|a| a.id == id)
    .ok_or_else(|| internal("committed attachment missing"))?;
    upload.complete = Some(record.clone());
    save_upload(&base, &upload)?;
    tokio::fs::remove_file(payload).await.map_err(internal)?;
    Ok(Json(record))
}
async fn get_attachments(
    State(state): State<ApiState>,
    Extension(identity): Extension<Identity>,
    Query(q): Query<ServiceQuery>,
) -> Result<Json<Vec<AttachmentRecord>>, ApiError> {
    let owner = node(
        &state,
        &identity,
        &q.project,
        &q.node,
        Action::AttachmentsRead,
        false,
    )
    .await?;
    Ok(Json(
        records::read_attachments(&optional_text(
            &owner.path.parent().unwrap().join("attachments.org"),
        )?)
        .map_err(bad)?,
    ))
}
async fn attachment(
    state: &ApiState,
    identity: &Identity,
    project: &str,
    node_id: &str,
    id: &str,
    revision: &str,
) -> Result<(PathBuf, AttachmentRecord, PathBuf), ApiError> {
    uuid(id)?;
    if !valid_digest(revision) {
        return Err(bad("invalid attachment revision"));
    }
    let owner = node(
        state,
        identity,
        project,
        node_id,
        Action::AttachmentsRead,
        false,
    )
    .await?;
    let assets = records::read_attachments(&optional_text(
        &owner.path.parent().unwrap().join("attachments.org"),
    )?)
    .map_err(bad)?;
    let record = assets
        .into_iter()
        .find(|a| a.id == id && a.node == node_id && a.revision == revision)
        .ok_or_else(|| ApiError::not_found("attachment unavailable"))?;
    if !MEDIA_TYPES.contains(&record.media_type.as_str()) {
        return Err(bad("unsupported attachment media type"));
    }
    let repo = owner.ledger.parent().ok_or_else(|| {
        internal("ledger has no parent; repair the project registration and retry")
    })?;
    let node_dir = owner
        .path
        .parent()
        .ok_or_else(|| internal("node has no directory; repair the node record and retry"))?
        .canonicalize()
        .map_err(|_| internal("node directory is unavailable; restore it and retry"))?;
    Ok((
        node_dir.join("attachments").join(revision),
        record,
        repo.to_path_buf(),
    ))
}
async fn get_content(
    State(state): State<ApiState>,
    Extension(identity): Extension<Identity>,
    Path((node_id, id, revision)): Path<(String, String, String)>,
    Query(q): Query<ServiceQuery>,
    method: Method,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let (path, record, repo) =
        attachment(&state, &identity, &q.project, &node_id, &id, &revision).await?;
    let mut file = tokio::fs::File::open(&path)
        .await
        .map_err(|_| {
            // Only the error text needs the ledger-relative path; never an absolute one.
            let relative = path
                .strip_prefix(&repo)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| format!("<node>/attachments/{revision}"));
            let storage = crate::ledger_sync::attachment_storage(&repo).unwrap_or_default();
            ApiError::not_found(match storage {
                AttachmentStorage::Local => match record.machine.as_deref() {
                    Some(machine) if machine == state.machine.as_str() => format!(
                        "attachment payload is missing from this machine's ledger at {relative}; this project stores attachments locally, not in git; restore that file from backup or upload it again"
                    ),
                    Some(machine) => format!(
                        "attachment payload is not on this machine (uploaded on machine {machine}); this project stores attachments locally, not in git; copy the payload from that machine to {relative}"
                    ),
                    None => format!(
                        "attachment payload is not on this machine; its record predates machine tracking, and this project stores attachments locally, not in git; locate the original upload machine and copy the payload to {relative}"
                    ),
                },
                AttachmentStorage::Lfs => {
                    "attachment payload missing; run git lfs pull in the ledger".to_string()
                }
            })
        })?;
    if file.metadata().await.map_err(internal)?.len() != record.size {
        return Err(internal("attachment payload size mismatch"));
    }
    let etag = format!("\"{revision}\"");
    let range = if headers
        .get(header::IF_RANGE)
        .is_some_and(|h| h.as_bytes() != etag.as_bytes())
    {
        None
    } else {
        headers.get(header::RANGE).and_then(|h| h.to_str().ok())
    };
    let (start, end) = match records::byte_range(range, record.size) {
        Ok(value) => value,
        Err(_) => {
            return Ok((
                StatusCode::RANGE_NOT_SATISFIABLE,
                [(header::CONTENT_RANGE, format!("bytes */{}", record.size))],
            )
                .into_response())
        }
    };
    file.seek(std::io::SeekFrom::Start(start))
        .await
        .map_err(internal)?;
    let body = if method == Method::HEAD {
        Body::empty()
    } else {
        let stream =
            futures::stream::try_unfold((file, end - start), |(mut file, remaining)| async move {
                if remaining == 0 {
                    return Ok::<_, std::io::Error>(None);
                }
                let mut bytes = vec![0; remaining.min(64 * 1024) as usize];
                let n = file.read(&mut bytes).await?;
                if n == 0 {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "short attachment",
                    ));
                }
                bytes.truncate(n);
                Ok(Some((bytes, (file, remaining - n as u64))))
            });
        Body::from_stream(stream)
    };
    let mut response = Response::builder()
        .status(if range.is_some() {
            StatusCode::PARTIAL_CONTENT
        } else {
            StatusCode::OK
        })
        .header(header::CONTENT_TYPE, &record.media_type)
        .header(header::CONTENT_LENGTH, end - start)
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::ETAG, etag)
        .header(header::CACHE_CONTROL, "private, no-store")
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff")
        .header(header::CONTENT_DISPOSITION, "attachment")
        .header(
            header::CONTENT_SECURITY_POLICY,
            "default-src 'none'; sandbox",
        )
        .header(header::REFERRER_POLICY, "no-referrer");
    if range.is_some() {
        response = response.header(
            header::CONTENT_RANGE,
            format!("bytes {start}-{}/{}", end - 1, record.size),
        );
    }
    response.body(body).map_err(internal)
}

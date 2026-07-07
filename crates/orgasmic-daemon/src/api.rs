// arch: arch_045Q0.1, arch_C87Z9.1, arch_MPAQT.1, arch_MPAQT.2, arch_PCSQE.1, arch_PCSQE.2, arch_QFQTD.1, arch_QFQTD.3, arch_QXS5W.2, arch_R3EPE.1, arch_R3EPE.2, arch_Z3Z3V.2
// orgasmic:arch_R3EPE, arch_C87Z9, arch_QXS5W, dec_XV9AK, dec_N17XX
//! Axum REST routes mirroring the arch_006 inventory.
//!
//! Real handlers (v0.0.1 priority): /board, /projects, /projects/{id},
//! /projects/{id}/tasks, /projects/{id}/tasks/{task_id}, /tx (GET + POST),
//! /daemon/status, /recovery/status, /auth/whoami, /graph/parse-errors.
//!
//! The rest of the inventory is wired as 501 stubs carrying the tracking
//! task ID, matching how the CLI returned stubs in TASK-004. The routes
//! must exist so the CLI/manager/UI can discover them; the handlers fill
//! in over TASK-006..TASK-010.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path as FsPath, PathBuf};
use std::process::Command;
use std::str::FromStr;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Extension, MatchedPath, Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode, Uri};
use axum::middleware::Next;
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use axum::Router;
use chrono::Utc;
use include_dir::{include_dir, Dir};
use orgasmic_core::projects::{init_project, register_project, ScaffoldInputs};
use orgasmic_core::tx::TxEntry;
use orgasmic_core::{
    goal_file_path, goal_file_rel, handoff_file_path, lifecycle_stage_file_name,
    project_sessions_dir, read_session_file, resolve_loader, task_file_path, task_file_rel,
    DriverEvent, Heading, Home, Lifecycle, LifecycleStage, OrgFile, OrgRewriter, ProjectConfig,
    ProjectFile, ReleaseOutcome, RuntimeIdentity, SandboxAllowlist, SessionEnvelope,
    SessionEventKind, SlotValues, Worker, WorkerKind, DEFAULT_TASK_FILE, DEFAULT_TASK_FILE_REL,
};
use orgasmic_drivers::r#trait::AttachOutcome;
use orgasmic_drivers::{
    driver_for, driver_for_mode_harness, DriverConfig, DriverContext, RunKind,
    RuntimeOptionsCatalog, RuntimeOptionsRequest, WorkerDriver,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tower_http::cors::{Any, CorsLayer};

use crate::artifacts::{
    self, append_comment, artifact_dir, artifact_org_content, load_artifact, load_artifact_detail,
    new_cid, reviews_org_header, update_artifact_org, validate_art_id, validate_mdx, versions_dir,
    ArtifactDetail, ArtifactLoadError, ArtifactSummary, NewComment,
};
use crate::auth::AuthState;
use crate::authz::{self, Action, Identity};
use crate::config::DriverDefaults;
use crate::content::{self, ContentLoadError};
use crate::events::{EventBus, EventPayload, Topic};
use crate::index::{BoardEntry, Index, IndexSnapshot, ProjectIndex, TaskOwner};
use crate::runtime::BootIdentity;
use crate::supervisor::{
    resolve_dispatch_watch_pid, supervisor_metrics, AcquireRequest, AcquireResponse,
    BabysitterAutoSpawn, DiffSummarizer, Supervisor, DEFAULT_IDLE_TIMEOUT_SECS,
};
use crate::writer::{FileRewrite, TxAppend, TxIdPolicy, WriterHandle};
use crate::ws;

static UI_DIST: Dir<'_> = include_dir!("$ORGASMIC_UI_DIST_DIR");

#[derive(Clone)]
pub struct ApiState {
    pub home: Home,
    pub index: Index,
    pub writer: WriterHandle,
    pub supervisor: Supervisor,
    pub manager_driver: Arc<dyn WorkerDriver>,
    pub events: EventBus,
    pub boot: Arc<BootIdentity>,
    pub auth: AuthState,
    pub default_tx_path: PathBuf,
    pub tx_commit_to_project: bool,
    pub manager_actor: Option<String>,
    pub auto_commit_signal: bool,
    pub driver_defaults: DriverDefaults,
    pub actor: String,
    pub machine: String,
    pub bind_host: String,
    pub bind_port: u16,
    pub ui_asset_hash: String,
    /// Flips to `true` when graceful shutdown begins. Long-lived connection
    /// tasks (websockets, PTY bridges) select on this so axum's drain phase
    /// can't deadlock behind a client that never disconnects.
    pub shutdown: tokio::sync::watch::Receiver<bool>,
    /// Post-release settle window for the dispatch completion watcher.
    pub dispatch_watcher_grace: std::time::Duration,
    /// Optional override (whole seconds) for the tmux/claude driver's
    /// input-ready detection timeout. `None` keeps the driver's 10s default.
    pub tmux_input_ready_timeout_secs: Option<u64>,
    /// Artificial delay before the dispatch HTTP handler returns (tests only).
    pub dispatch_response_delay: Option<std::time::Duration>,
    /// Per-artifact locks serializing reviews.org read-modify-write across the
    /// two feedback handlers. See [`ApiState::artifact_write_lock`].
    pub artifact_write_locks:
        Arc<std::sync::Mutex<std::collections::HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>>,
}

impl ApiState {
    /// Serialize the read-modify-write of a single artifact's on-disk state
    /// (`reviews.org` and `artifact.org`) across every handler that mutates it.
    ///
    /// The atomic-transaction refactor (TASK-Y2ZQJ finding #4) replaced
    /// `writer.mutate_file` — which read the file under the writer's exclusive
    /// flock and applied its transform there, serializing concurrent mutations
    /// — with a read-outside-the-lock then blind `FileRewrite`. That
    /// reintroduced a lost-update race (finding M1): two concurrent ops on the
    /// same artifact both read the same base and the second clobbers the first.
    ///
    /// Every handler with a read→transaction window on the same artifact MUST
    /// hold this per-artifact lock across that window so they serialize again
    /// while keeping finding #4's file+tx atomicity:
    /// - `post_artifact_add_comment` / `post_artifact_comment_resolve` (reviews.org)
    /// - `post_artifact_regenerate` (reviews.org consume + artifact.org state flip)
    /// - `post_artifact_submit` (artifact.org version read→bump; TASK-2ZQSB M1)
    /// - `revert_artifact_generation_state` (the release-watcher's state restore)
    fn artifact_write_lock(&self, art_dir: &FsPath) -> Arc<tokio::sync::Mutex<()>> {
        let mut map = self.artifact_write_locks.lock().unwrap();
        map.entry(art_dir.to_path_buf())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }
}

pub fn router(state: ApiState) -> Router {
    let identity_state = state.clone();
    let protected = Router::new()
        // v0.0.1 priority: real handlers
        .route("/board", get(get_board))
        .route("/me", get(get_me))
        .route("/projects", post(add_project).get(get_projects))
        .route("/projects/:id", get(get_project))
        .route(
            "/projects/:id/tasks",
            get(get_project_tasks).post(post_task_create),
        )
        .route(
            "/projects/:id/tasks/:task_id",
            get(get_task).post(post_task_update),
        )
        .route("/projects/:id/goal/set", post(post_goal_set))
        .route("/projects/:id/goal/clear", post(post_goal_clear))
        .route("/projects/:id/goal/supersede", post(post_goal_supersede))
        .route(
            "/projects/:id/tasks/:task_id/dispatch",
            post(post_task_dispatch),
        )
        .route(
            "/projects/:id/tasks/:task_id/lease/release",
            post(post_task_lease_release),
        )
        .route(
            "/projects/:id/tasks/:task_id/dispatch/cleanup",
            post(post_task_dispatch_cleanup),
        )
        .route("/tasks/:id/subtasks", post(post_task_subtask))
        .route("/tasks/:id/comments", post(post_task_comment))
        .route("/tasks/:id/activity", get(get_task_activity))
        .route("/tx", get(get_tx).post(post_tx))
        .route("/daemon/status", get(get_status))
        .route("/daemon/restart", post(post_daemon_restart))
        .route("/recovery/status", get(get_recovery))
        .route("/auth/whoami", get(get_whoami))
        .route("/filesystem/roots", get(get_filesystem_roots))
        .route("/filesystem/entries", get(get_filesystem_entries))
        .route(
            "/filesystem/validate-project",
            post(post_filesystem_validate_project),
        )
        .route("/graph/parse-errors", get(get_parse_errors))
        .route("/reindex", post(post_reindex))
        .route("/reindex/:project", post(post_reindex_project))
        .route("/graph/edges", get(get_graph_edges))
        .route("/org/file", get(get_org_file).post(post_org_file))
        .route("/org/node", get(get_org_node))
        .route("/org/node/:id/edit", post(post_org_node_edit))
        .route("/ws", get(ws::handler))
        .route("/ws/tmux/:run_id", get(ws::tmux_handler))
        // Stubs — wired so callers can discover them; tracked tasks owned elsewhere.
        .route("/runs", get(get_runs).post(stub("TASK-006")))
        .route("/runs/:id", get(get_run).post(stub("TASK-006")))
        .route("/runs/:id/recover", post(post_run_recover))
        .route("/runs/:id/input", post(post_run_input))
        .route(
            "/runs/:id/runtime-options",
            get(get_run_runtime_options).post(post_run_runtime_options),
        )
        .route("/runs/:id/release", post(post_run_release))
        .route("/workers", get(get_workers))
        .route(
            "/workers/validate",
            get(get_workers_validate).post(post_workers_validate),
        )
        .route("/workers/:id", get(get_worker))
        .route("/prompt-specs", get(get_prompt_specs))
        .route("/prompt-specs/parts", get(get_prompt_parts))
        .route(
            "/prompt-specs/parts/:id",
            get(get_prompt_part).post(post_prompt_part_save),
        )
        .route("/prompt-specs/context-packs", get(get_context_packs))
        .route(
            "/prompt-specs/:id",
            get(get_prompt_spec).post(post_prompt_spec_save),
        )
        .route("/prompt-specs/:id/compile", post(post_prompt_spec_compile))
        .route("/prompt-specs/:id/lint", post(post_prompt_spec_lint))
        .route("/prompt-specs/:id/fork", post(post_prompt_spec_fork))
        .route("/skills", get(get_skills))
        .route("/skills/:id", get(get_skill))
        .route("/manager/launch", post(post_manager_launch))
        .route("/manager/action", post(post_manager_action))
        .route("/manager/state", get(get_manager_state))
        .route("/managers/drivers", get(get_manager_drivers))
        .route("/tmux/:run_id/attach", get(stub("TASK-007")))
        .route("/question", post(post_question))
        .route("/question/:id/answer", post(post_question_answer))
        .route("/glossary", get(get_glossary).post(post_glossary_create))
        .route(
            "/glossary/:id",
            get(get_glossary_term).post(post_glossary_action),
        )
        .route("/decisions", get(get_decisions).post(post_decision_create))
        .route(
            "/decisions/:id",
            get(get_decision).post(post_decision_action),
        )
        .route(
            "/architecture",
            get(get_architecture).post(post_architecture_create),
        )
        .route("/architecture/nodes", get(get_architecture_nodes))
        .route(
            "/architecture/:id",
            get(get_architecture_node).post(post_architecture_action),
        )
        .route("/grill", post(post_grill))
        .route("/architect", post(post_architect))
        .route("/plan", post(post_plan))
        .route("/graph/markers/:node_id", get(get_graph_markers))
        .route("/graph/nodes", get(get_graph_nodes).post(stub("TASK-008")))
        // Artifact store (arch_ARSPJ / TASK-ZEFEY)
        .route("/artifacts", get(get_artifacts))
        .route("/artifacts/generate", post(post_artifact_generate))
        .route("/artifacts/:id", get(get_artifact))
        .route("/artifacts/:id/submit", post(post_artifact_submit))
        .route("/artifacts/:id/regenerate", post(post_artifact_regenerate))
        .route("/artifacts/:id/comments", post(post_artifact_add_comment))
        .route(
            "/artifacts/:id/comments/:cid/resolve",
            post(post_artifact_comment_resolve),
        )
        .layer(axum::middleware::from_fn_with_state(
            identity_state,
            identity_middleware,
        ));

    let api = Router::new()
        .route("/healthz", get(healthz))
        .route(
            "/auth/ui-session",
            post(post_ui_session).get(get_app_session),
        )
        .route("/login", post(post_login))
        .merge(protected)
        .fallback(api_not_found)
        .with_state(state);

    Router::new()
        .route("/", get(get_app_index))
        .nest("/api", api)
        .fallback(get(get_spa_asset))
        .layer(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_methods(Any)
                .allow_headers(Any),
        )
}

// ---- authorization seam wiring (arch_Z8CW2 / dec_KF2MR) --------------------
//
// Every request through `protected` resolves an `Identity` (admin, unchanged
// full access; or a named member with grants read fresh from members.org)
// and stores it in request extensions. For a member identity, the route
// itself must additionally appear in `MEMBER_ALLOWED_ROUTES` below — the
// route->action map spanning the whole surface: everything NOT listed here
// is implicitly admin-only. Routes that ARE listed still gate their specific
// per-project action through the one `authz::require` seam inside the
// handler; this table only decides whether a member's request is even
// allowed to reach that handler at all.

/// `(HTTP method, axum route template)` pairs a member identity may reach.
/// Every other route in `protected` is admin-only by omission. `/board` and
/// `/me` are listed but gate nothing here — their handlers filter results per
/// identity rather than rejecting the whole request.
const MEMBER_ALLOWED_ROUTES: &[(&str, &str)] = &[
    ("GET", "/board"),
    ("GET", "/me"),
    ("GET", "/projects/:id"),
    ("GET", "/projects/:id/tasks"),
    ("GET", "/projects/:id/tasks/:task_id"),
    ("GET", "/tasks/:id/activity"),
    ("GET", "/graph/nodes"),
    ("GET", "/graph/edges"),
    ("GET", "/decisions"),
    ("GET", "/decisions/:id"),
    ("GET", "/architecture"),
    ("GET", "/architecture/nodes"),
    ("GET", "/architecture/:id"),
    ("GET", "/glossary"),
    ("GET", "/glossary/:id"),
    ("GET", "/artifacts"),
    ("GET", "/artifacts/:id"),
    ("POST", "/artifacts/generate"),
    ("POST", "/artifacts/:id/regenerate"),
    ("POST", "/artifacts/:id/comments"),
    ("POST", "/artifacts/:id/comments/:cid/resolve"),
    ("GET", "/ws"),
    ("GET", "/ws/tmux/:run_id"),
];

fn member_route_allowed(method: &Method, pattern: &str) -> bool {
    let method = method.as_str();
    MEMBER_ALLOWED_ROUTES
        .iter()
        .any(|(m, p)| *m == method && *p == pattern)
}

/// Resolves identity for every request under `protected` and, for a member
/// identity, checks the route against `MEMBER_ALLOWED_ROUTES` before the
/// handler ever runs. Supersedes the old plain bearer-only `bearer_middleware`
/// — admin auth (bearer token or admin UI session, plus the WS query-token
/// escape hatch) is unchanged; member session-cookie resolution is additive.
pub async fn identity_middleware(
    State(state): State<ApiState>,
    matched_path: Option<MatchedPath>,
    mut request: axum::extract::Request,
    next: Next,
) -> Response {
    let path = request.uri().path().to_string();
    let query_token_allowed = path == "/api/ws"
        || path.starts_with("/api/ws/")
        || path == "/ws"
        || path.starts_with("/ws/");

    let identity = match state.auth.resolve_identity(request.headers(), &state.home) {
        Some(identity) => identity,
        None if query_token_allowed && state.auth.check_query_token(request.uri().query()) => {
            Identity::Admin
        }
        None => {
            return (
                StatusCode::UNAUTHORIZED,
                [(header::WWW_AUTHENTICATE, "Bearer")],
                "missing or invalid bearer token",
            )
                .into_response();
        }
    };

    if matches!(identity, Identity::Member { .. }) {
        // `MatchedPath` reports the route template *including* the `/api` nest
        // prefix (e.g. `/api/ws`, `/api/projects/:id`), while
        // `MEMBER_ALLOWED_ROUTES` lists app-relative templates (`/ws`). Strip
        // the prefix so the coarse allow-list matches — otherwise every member
        // request is rejected here before the per-capability gate ever runs.
        let raw = matched_path.as_ref().map(|m| m.as_str()).unwrap_or(&path);
        let pattern = raw
            .strip_prefix("/api")
            .filter(|rest| rest.starts_with('/'))
            .unwrap_or(raw);
        if !member_route_allowed(request.method(), pattern) {
            return ApiError {
                status: StatusCode::FORBIDDEN,
                message: "forbidden for this member role".into(),
                body: None,
            }
            .into_response();
        }
    }

    request.extensions_mut().insert(identity);
    next.run(request).await
}

#[derive(Debug, Deserialize)]
struct LoginRequest {
    token: String,
}

/// `POST /login` — paste a member token, get a long-lived named session
/// cookie. Public (no prior auth): this is how a member acquires one.
async fn post_login(State(state): State<ApiState>, Json(body): Json<LoginRequest>) -> Response {
    let Ok(Some(entry)) = orgasmic_core::find_member_by_token(&state.home, &body.token) else {
        return (StatusCode::UNAUTHORIZED, "invalid or unknown token").into_response();
    };
    let (session, expires_at) = state.auth.create_member_session(&entry.name);
    let cookie = format!(
        "{}={}; Path=/; HttpOnly; SameSite=Lax; Max-Age={}",
        AuthState::member_session_cookie_name(),
        session,
        AuthState::member_session_max_age_seconds()
    );
    let mut response = Json(json!({
        "name": entry.name,
        "expires_at": expires_at.to_rfc3339(),
    }))
    .into_response();
    if let Ok(value) = HeaderValue::from_str(&cookie) {
        response.headers_mut().insert(header::SET_COOKIE, value);
    }
    response
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct MeProjectCapabilities {
    project_id: String,
    role: String,
    capabilities: Vec<&'static str>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct MeResponse {
    identity: &'static str,
    name: Option<String>,
    projects: Vec<MeProjectCapabilities>,
}

/// `GET /me` — capability snapshot the UI renders nav/affordances from.
/// Convenience only (per arch_Z8CW2: the server is the sole real gate);
/// admin sees every project with every capability, a member sees only their
/// granted projects with that grant's resolved role and capability set.
async fn get_me(
    State(state): State<ApiState>,
    Extension(identity): Extension<Identity>,
) -> Json<MeResponse> {
    let snap = state.index.snapshot().await;
    let all_ids: Vec<&str> = snap.board.iter().map(|e| e.id.as_str()).collect();
    let visible = authz::visible_project_ids(&identity, all_ids.iter().copied());

    let projects = visible
        .into_iter()
        .map(|project_id| {
            let role = match &identity {
                Identity::Admin => "admin".to_string(),
                Identity::Member { .. } => identity.role_for(project_id).unwrap_or("").to_string(),
            };
            let capabilities = match &identity {
                Identity::Admin => Action::ALL
                    .iter()
                    .copied()
                    .map(authz::action_name)
                    .collect(),
                Identity::Member { .. } => authz::role_capabilities(&role)
                    .iter()
                    .copied()
                    .map(authz::action_name)
                    .collect(),
            };
            MeProjectCapabilities {
                project_id: project_id.to_string(),
                role,
                capabilities,
            }
        })
        .collect();

    Json(MeResponse {
        identity: match identity {
            Identity::Admin => "admin",
            Identity::Member { .. } => "member",
        },
        name: identity.member_name().map(str::to_string),
        projects,
    })
}

// ---- real handlers ---------------------------------------------------------

async fn healthz() -> Json<Value> {
    Json(json!({
        "status": "ok",
        "supervisor": supervisor_metrics(),
    }))
}

async fn get_app_index() -> Response {
    app_asset_response("index.html", false)
}

async fn get_spa_asset(uri: Uri, headers: HeaderMap) -> Response {
    let path = uri.path();
    if path == "/api" || path.starts_with("/api/") || path == "/app" || path.starts_with("/app/") {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }
    app_asset_response(path.trim_start_matches('/'), accepts_html(&headers))
}

async fn api_not_found() -> Response {
    (StatusCode::NOT_FOUND, "api route not found").into_response()
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct UiSessionResponse {
    path: String,
    expires_at: String,
}

async fn post_ui_session(State(state): State<ApiState>, headers: HeaderMap) -> Response {
    if !state.auth.check(&headers) {
        return (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"))],
            "missing or invalid bearer token",
        )
            .into_response();
    }
    let ticket = state.auth.create_ui_ticket();
    Json(UiSessionResponse {
        path: format!("/api/auth/ui-session?ticket={}", ticket.ticket),
        expires_at: ticket.expires_at.to_rfc3339(),
    })
    .into_response()
}

#[derive(Debug, Deserialize)]
struct UiSessionQuery {
    ticket: String,
}

async fn get_app_session(
    State(state): State<ApiState>,
    Query(query): Query<UiSessionQuery>,
) -> Response {
    let Some(session) = state.auth.redeem_ui_ticket(&query.ticket) else {
        return (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"))],
            "invalid or expired UI session ticket",
        )
            .into_response();
    };
    let cookie = format!(
        "{}={}; Path=/; HttpOnly; SameSite=Lax; Max-Age={}",
        AuthState::ui_session_cookie_name(),
        session,
        AuthState::ui_session_max_age_seconds()
    );
    let mut response = redirect_response("/");
    if let Ok(value) = HeaderValue::from_str(&cookie) {
        response.headers_mut().insert(header::SET_COOKIE, value);
    }
    response
}

fn redirect_response(location: &str) -> Response {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::SEE_OTHER;
    response.headers_mut().insert(
        header::LOCATION,
        HeaderValue::from_str(location).unwrap_or_else(|_| HeaderValue::from_static("/")),
    );
    response
}

fn app_asset_response(path: &str, allow_spa_fallback: bool) -> Response {
    let Some(safe_path) = safe_app_asset_path(path) else {
        return (StatusCode::BAD_REQUEST, "invalid app asset path").into_response();
    };
    let file = UI_DIST.get_file(&safe_path).or_else(|| {
        if allow_spa_fallback {
            UI_DIST.get_file("index.html")
        } else {
            None
        }
    });
    let Some(file) = file else {
        return (StatusCode::NOT_FOUND, "app asset not found").into_response();
    };
    let response_path = if UI_DIST.get_file(&safe_path).is_some() {
        safe_path.as_str()
    } else {
        "index.html"
    };
    let mut response = Response::new(Body::from(file.contents().to_vec()));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(content_type_for_path(response_path)),
    );
    response
}

fn safe_app_asset_path(path: &str) -> Option<String> {
    let trimmed = path.trim_start_matches('/');
    if trimmed.is_empty() || trimmed.ends_with('/') {
        return Some("index.html".to_string());
    }
    if trimmed
        .split('/')
        .any(|segment| segment.is_empty() || segment == "." || segment == "..")
    {
        return None;
    }
    Some(trimmed.to_string())
}

fn accepts_html(headers: &HeaderMap) -> bool {
    headers
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.contains("text/html"))
        .unwrap_or(false)
}

fn content_type_for_path(path: &str) -> &'static str {
    match FsPath::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("")
    {
        "css" => "text/css; charset=utf-8",
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" => "application/json; charset=utf-8",
        "png" => "image/png",
        "svg" => "image/svg+xml",
        "wasm" => "application/wasm",
        "woff2" => "font/woff2",
        _ => "application/octet-stream",
    }
}

pub fn embedded_ui_asset_hash() -> String {
    let mut files = Vec::new();
    collect_ui_files(&UI_DIST, &mut files);
    files.sort_by(|a, b| a.0.cmp(&b.0));
    let mut hasher = Sha256::new();
    for (path, bytes) in files {
        hasher.update(path.as_bytes());
        hasher.update([0]);
        hasher.update(bytes);
        hasher.update([0]);
    }
    format!("{:x}", hasher.finalize())
}

fn collect_ui_files<'a>(dir: &'a Dir<'a>, out: &mut Vec<(String, &'a [u8])>) {
    for file in dir.files() {
        out.push((file.path().to_string_lossy().to_string(), file.contents()));
    }
    for child in dir.dirs() {
        collect_ui_files(child, out);
    }
}

async fn get_board(
    State(state): State<ApiState>,
    Extension(identity): Extension<Identity>,
) -> Json<Vec<crate::index::BoardEntry>> {
    let snap = state.index.snapshot().await;
    // Cross-project list: never 403 — filter to the projects this identity may
    // see (admin sees all; a member sees only granted projects). Mirrors `get_me`.
    let visible: std::collections::HashSet<String> =
        authz::visible_project_ids(&identity, snap.board.iter().map(|e| e.id.as_str()))
            .into_iter()
            .map(String::from)
            .collect();
    Json(
        snap.board
            .into_iter()
            .filter(|e| visible.contains(&e.id))
            .collect(),
    )
}

async fn get_projects(
    State(state): State<ApiState>,
    Extension(identity): Extension<Identity>,
) -> Json<Vec<crate::index::ProjectIndex>> {
    let snap = state.index.snapshot().await;
    let supervisor = state.supervisor.snapshot().await;
    // Cross-project list: filter to visible projects rather than 403 (admin
    // sees all; a member sees only granted projects).
    let visible: std::collections::HashSet<String> =
        authz::visible_project_ids(&identity, snap.projects.keys().map(String::as_str))
            .into_iter()
            .map(String::from)
            .collect();
    Json(
        snap.projects
            .into_values()
            .filter(|project| visible.contains(&project.project_id))
            .map(|project| apply_task_owners(project, &supervisor.runs))
            .collect(),
    )
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AddProjectRequest {
    path: PathBuf,
    #[serde(default)]
    scaffold: bool,
}

#[derive(Debug)]
struct ExistingProjectConfig {
    project_id: String,
    default_branch: Option<String>,
}

async fn add_project(
    State(state): State<ApiState>,
    Json(req): Json<AddProjectRequest>,
) -> Result<(StatusCode, Json<ProjectIndex>), ApiError> {
    if !req.path.is_absolute() {
        return Err(ApiError::bad_request("path must be absolute"));
    }
    if !req.path.is_dir() {
        return Err(ApiError::bad_request(
            "path does not exist or is not a directory",
        ));
    }
    let project_root = std::fs::canonicalize(&req.path).map_err(|e| {
        tracing::warn!(path = %req.path.display(), error = %e, "project path canonicalize failed");
        ApiError::bad_request("failed to resolve project path")
    })?;
    let project_org = project_root.join(".orgasmic").join("project.org");
    let has_project_org = project_org.exists();

    if !has_project_org && !req.scaffold {
        return Err(ApiError::bad_request(
            "path is not an orgasmic project; pass scaffold=true to initialize",
        ));
    }

    let mut inputs = ScaffoldInputs::derive(&project_root, None);

    if has_project_org {
        let existing = read_existing_project_config(&project_org)?;
        inputs.project_id = existing.project_id;
        inputs.default_branch = existing
            .default_branch
            .unwrap_or_else(|| "main".to_string());
    } else {
        init_project(&state.home, &project_root, &inputs, false).map_err(init_project_error)?;
    }

    register_project(
        &state.home,
        &project_root,
        &inputs.project_id,
        &inputs.default_branch,
    )
    .map_err(register_project_error)?;

    state.index.refresh_board().await;
    state
        .index
        .refresh_project(&inputs.project_id)
        .await
        .map_err(|e| {
            tracing::warn!(project_id = %inputs.project_id, error = %e, "project refresh after registration failed");
            ApiError::internal("failed to refresh project index")
        })?;
    state
        .events
        .publish(Topic::Board, EventPayload::BoardRefreshed);
    state.events.publish(
        Topic::Board,
        EventPayload::ProjectIndexed {
            project_id: inputs.project_id.clone(),
        },
    );

    let snap = state.index.snapshot().await;
    let supervisor = state.supervisor.snapshot().await;
    let project = snap
        .projects
        .get(&inputs.project_id)
        .cloned()
        .map(|project| apply_task_owners(project, &supervisor.runs))
        .ok_or_else(|| ApiError::internal("project registered but missing from snapshot"))?;
    Ok((StatusCode::CREATED, Json(project)))
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
struct FilesystemRoot {
    path: PathBuf,
    display_name: String,
    kind: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
struct FilesystemEntry {
    path: PathBuf,
    display_name: String,
    kind: String,
    accessible: bool,
    orgasmic_project: bool,
    project_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct FilesystemEntriesQuery {
    path: PathBuf,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FilesystemValidateProjectRequest {
    path: PathBuf,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
struct FilesystemValidateProjectResponse {
    path: PathBuf,
    exists: bool,
    is_directory: bool,
    orgasmic_project: bool,
    project_id: Option<String>,
    default_branch: Option<String>,
}

async fn get_filesystem_roots(
    State(state): State<ApiState>,
) -> Result<Json<Vec<FilesystemRoot>>, ApiError> {
    let snap = state.index.snapshot().await;
    let mut roots = Vec::new();
    push_filesystem_root(&mut roots, state.home.root.clone(), "orgasmic home", "home");
    if state.home.source().is_dir() {
        push_filesystem_root(&mut roots, state.home.source(), "orgasmic source", "source");
    }
    if let Ok(cwd) = std::env::current_dir() {
        push_filesystem_root(&mut roots, cwd, "current directory", "cwd");
    }
    if let Some(home_dir) = user_home_dir() {
        push_filesystem_root(&mut roots, home_dir, "user home", "user_home");
    }
    for project in snap.board {
        push_filesystem_root(
            &mut roots,
            project.path,
            &format!("project {}", project.id),
            "project",
        );
    }
    push_platform_roots(&mut roots);
    Ok(Json(roots))
}

async fn get_filesystem_entries(
    Query(query): Query<FilesystemEntriesQuery>,
) -> Result<Json<Vec<FilesystemEntry>>, ApiError> {
    let path = canonical_directory(&query.path)?;
    let mut entries = Vec::new();
    let read_dir = std::fs::read_dir(&path).map_err(|error| {
        tracing::warn!(path = %path.display(), error = %error, "filesystem directory read failed");
        ApiError::bad_request("directory is not readable")
    })?;
    for entry in read_dir {
        match entry {
            Ok(entry) => entries.push(filesystem_entry(entry.path())),
            Err(error) => {
                tracing::warn!(path = %path.display(), error = %error, "filesystem entry read failed");
            }
        }
    }
    entries.sort_by(|a, b| {
        let a_dir = a.kind == "directory";
        let b_dir = b.kind == "directory";
        b_dir.cmp(&a_dir).then_with(|| {
            a.display_name
                .to_lowercase()
                .cmp(&b.display_name.to_lowercase())
        })
    });
    Ok(Json(entries))
}

async fn post_filesystem_validate_project(
    Json(req): Json<FilesystemValidateProjectRequest>,
) -> Result<Json<FilesystemValidateProjectResponse>, ApiError> {
    let exists = req.path.exists();
    let is_directory = req.path.is_dir();
    let mut response = FilesystemValidateProjectResponse {
        path: req.path.clone(),
        exists,
        is_directory,
        orgasmic_project: false,
        project_id: None,
        default_branch: None,
    };
    if is_directory {
        let root = canonical_directory(&req.path)?;
        response.path = root.clone();
        if let Some(project) = probe_orgasmic_project(&root) {
            response.orgasmic_project = true;
            response.project_id = Some(project.project_id);
            response.default_branch = project.default_branch;
        }
    }
    Ok(Json(response))
}

fn push_filesystem_root(
    roots: &mut Vec<FilesystemRoot>,
    path: PathBuf,
    display_name: &str,
    kind: &str,
) {
    let path = std::fs::canonicalize(&path).unwrap_or(path);
    if !path.is_dir() || roots.iter().any(|root| root.path == path) {
        return;
    }
    roots.push(FilesystemRoot {
        path,
        display_name: display_name.to_string(),
        kind: kind.to_string(),
    });
}

fn push_platform_roots(roots: &mut Vec<FilesystemRoot>) {
    #[cfg(windows)]
    {
        for drive in b'A'..=b'Z' {
            let path = PathBuf::from(format!("{}:\\", drive as char));
            push_filesystem_root(roots, path, "drive", "drive");
        }
    }
    #[cfg(not(windows))]
    {
        push_filesystem_root(roots, PathBuf::from("/"), "filesystem root", "root");
    }
}

fn user_home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("USERPROFILE").map(PathBuf::from))
}

fn canonical_directory(path: &FsPath) -> Result<PathBuf, ApiError> {
    if !path.is_dir() {
        return Err(ApiError::bad_request(
            "path does not exist or is not a directory",
        ));
    }
    std::fs::canonicalize(path).map_err(|error| {
        tracing::warn!(path = %path.display(), error = %error, "filesystem path canonicalize failed");
        ApiError::bad_request("failed to resolve directory path")
    })
}

fn filesystem_entry(path: PathBuf) -> FilesystemEntry {
    let display_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_string)
        .unwrap_or_else(|| path.to_string_lossy().to_string());
    let metadata = std::fs::symlink_metadata(&path);
    let (kind, accessible) = match metadata {
        Ok(metadata) if metadata.file_type().is_symlink() => ("symlink".to_string(), true),
        Ok(metadata) if metadata.is_dir() => ("directory".to_string(), true),
        Ok(metadata) if metadata.is_file() => ("file".to_string(), true),
        Ok(_) => ("other".to_string(), true),
        Err(_) => ("unknown".to_string(), false),
    };
    let project = if kind == "directory" {
        probe_orgasmic_project(&path)
    } else {
        None
    };
    FilesystemEntry {
        path,
        display_name,
        kind,
        accessible,
        orgasmic_project: project.is_some(),
        project_id: project.map(|project| project.project_id),
    }
}

fn probe_orgasmic_project(root: &FsPath) -> Option<ExistingProjectConfig> {
    read_existing_project_config(&root.join(".orgasmic/project.org")).ok()
}

fn non_empty_field(value: Option<String>) -> Option<String> {
    value
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn read_artifact(path: &FsPath, artifact: &'static str) -> Result<String, ApiError> {
    std::fs::read_to_string(path).map_err(|e| file_read_error(path, artifact, e))
}

fn file_read_error(path: &FsPath, artifact: &'static str, error: std::io::Error) -> ApiError {
    if error.kind() == std::io::ErrorKind::NotFound {
        tracing::warn!(path = %path.display(), error = %error, "{artifact} not found");
        ApiError::not_found(format!("{artifact} not found"))
    } else {
        tracing::error!(path = %path.display(), error = %error, "{artifact} read failed");
        ApiError::internal(format!("failed to read {artifact}"))
    }
}

fn org_parse_bad_request(
    path: &FsPath,
    artifact: &'static str,
    error: impl std::fmt::Display,
) -> ApiError {
    tracing::warn!(path = %path.display(), error = %error, "{artifact} parse failed");
    ApiError::bad_request(format!("{artifact} is not valid orgasmic markup"))
}

fn org_parse_internal(
    path: &FsPath,
    artifact: &'static str,
    error: impl std::fmt::Display,
) -> ApiError {
    tracing::error!(path = %path.display(), error = %error, "{artifact} parse failed");
    ApiError::internal(format!("failed to parse {artifact}"))
}

fn content_list_error(error: ContentLoadError, artifact: &'static str) -> ApiError {
    content_error(error, artifact)
}

fn content_load_error(error: ContentLoadError, artifact: &'static str) -> ApiError {
    content_error(error, artifact)
}

fn content_error(error: ContentLoadError, artifact: &'static str) -> ApiError {
    match &error {
        ContentLoadError::NotFoundByRule { .. } => {
            tracing::warn!(error = %error, "{artifact} not found");
            ApiError::not_found(format!("{artifact} not found"))
        }
        ContentLoadError::NotFoundOnDisk { path, source } => {
            tracing::warn!(path = %path.display(), error = %source, "{artifact} not found");
            ApiError::not_found(format!("{artifact} not found"))
        }
        ContentLoadError::Read { path, source } => {
            tracing::error!(path = %path.display(), error = %source, "{artifact} load failed");
            ApiError::internal(format!("failed to load {artifact}"))
        }
        ContentLoadError::Parse { path, source } => {
            tracing::error!(path = %path.display(), error = %source, "{artifact} parse failed");
            ApiError::internal(format!("failed to load {artifact}"))
        }
        ContentLoadError::Other { source } => {
            if let Some(path) = error.path() {
                tracing::error!(path = %path.display(), error = %source, "{artifact} load failed");
            } else {
                tracing::error!(error = %source, "{artifact} load failed");
            }
            ApiError::internal(format!("failed to load {artifact}"))
        }
    }
}

fn writer_transaction_error(error: impl std::fmt::Display) -> ApiError {
    tracing::error!(error = %error, "writer transaction failed");
    ApiError::internal("failed to apply changes")
}

fn writer_append_error(error: impl std::fmt::Display) -> ApiError {
    tracing::error!(error = %error, "writer append failed");
    ApiError::internal("failed to record transaction")
}

fn writer_rewrite_error(path: &FsPath, error: impl std::fmt::Display) -> ApiError {
    tracing::error!(path = %path.display(), error = %error, "writer rewrite failed");
    ApiError::internal("failed to rewrite org file")
}

fn writer_drain_error(context: &str, error: impl std::fmt::Display) -> ApiError {
    tracing::error!(context = context, error = %error, "writer drain failed");
    ApiError::internal("failed to drain writer before restart")
}

fn supervisor_acquire_error(context: &str, error: impl std::fmt::Display) -> ApiError {
    tracing::error!(context = context, error = %error, "supervisor acquire failed");
    ApiError::internal("failed to acquire worker run")
}

fn supervisor_release_error(run_id: &str, error: impl std::fmt::Display) -> ApiError {
    tracing::error!(run_id = run_id, error = %error, "supervisor release failed");
    ApiError::internal("failed to release run")
}

fn supervisor_control_error(context: &str, error: impl std::fmt::Display) -> ApiError {
    tracing::error!(context = context, error = %error, "supervisor control failed");
    ApiError::internal("failed to control run")
}

fn supervisor_recover_error(error: crate::supervisor::SupervisorError) -> ApiError {
    use crate::supervisor::SupervisorError;
    match error {
        SupervisorError::LeaseHeld {
            task_id,
            kind,
            run_id,
        } => ApiError::conflict_json(json!({
            "error": "recovery blocked by an active lease",
            "task_id": task_id,
            "kind": format!("{kind:?}").to_lowercase(),
            "active_run_id": run_id,
            "target": if task_id.starts_with("manager.") || task_id.starts_with("manager:") {
                "manager"
            } else {
                "worker"
            },
            "choices": ["open_active_run", "stop_active_run", "start_recovery_run"],
        })),
        SupervisorError::ReattachLeaseConflict {
            task_id,
            kind,
            active_run_id,
        } => ApiError::conflict_json(json!({
            "error": "reattach blocked by an active lease",
            "task_id": task_id,
            "kind": format!("{kind:?}").to_lowercase(),
            "active_run_id": active_run_id,
            "target": if task_id.starts_with("manager.") || task_id.starts_with("manager:") {
                "manager"
            } else {
                "worker"
            },
            "choices": ["open_active_run", "stop_active_run", "start_recovery_run"],
        })),
        SupervisorError::NotReattachable { run_id, reason } => {
            tracing::warn!(run_id = %run_id, reason = %reason, "reattach not possible");
            ApiError::conflict(format!("run {run_id} is no longer reattachable: {reason}"))
        }
        other => {
            tracing::error!(error = %other, "run recovery failed");
            ApiError::internal("failed to recover run")
        }
    }
}

fn driver_validate_error(driver: &str, error: impl std::fmt::Display) -> ApiError {
    tracing::warn!(driver = driver, error = %error, "driver validation failed");
    ApiError::bad_request("driver configuration is invalid")
}

fn org_input_parse_error(path: &FsPath, error: impl std::fmt::Display) -> ApiError {
    tracing::warn!(path = %path.display(), error = %error, "org input parse failed");
    ApiError::bad_request("org file is not valid orgasmic markup")
}

fn org_rewriter_error(context: &str, node_id: &str, error: impl std::fmt::Display) -> ApiError {
    tracing::warn!(context = context, node_id = node_id, error = %error, "org rewriter failed");
    ApiError::bad_request("org file update failed")
}

fn read_existing_project_config(project_org: &FsPath) -> Result<ExistingProjectConfig, ApiError> {
    let source = std::fs::read_to_string(project_org).map_err(|e| {
        tracing::warn!(path = %project_org.display(), error = %e, "project.org read failed");
        ApiError::bad_request("failed to read project.org")
    })?;
    let file = OrgFile::parse(source, project_org.to_string_lossy()).map_err(|e| {
        tracing::warn!(path = %project_org.display(), error = %e, "project.org parse failed");
        ApiError::bad_request("project file is not valid orgasmic markup")
    })?;
    let project = ProjectFile::from_org(&file, project_org.to_string_lossy().as_ref()).map_err(|e| {
        tracing::warn!(path = %project_org.display(), error = %e, "project.org schema validation failed");
        ApiError::bad_request("project file is not valid orgasmic markup")
    })?;
    Ok(ExistingProjectConfig {
        project_id: project.id.to_string(),
        default_branch: read_config_default_branch(&project_org.with_file_name("config.org")),
    })
}

/// Read `:DEFAULT_BRANCH:` from a project's `config.org`, returning `None` when
/// the file is absent, unparseable, or carries no non-empty branch (dec_051).
fn read_config_default_branch(config_org: &FsPath) -> Option<String> {
    let source = std::fs::read_to_string(config_org).ok()?;
    let file = OrgFile::parse(source, config_org.to_string_lossy()).ok()?;
    let config = ProjectConfig::from_org(&file, config_org.to_string_lossy().as_ref()).ok()?;
    config
        .default_branch
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn init_project_error(error: anyhow::Error) -> ApiError {
    let message = error.to_string();
    if is_project_input_error(&message) {
        return ApiError::bad_request(message);
    }
    tracing::warn!(error = %error, "project scaffold failed");
    if message.contains("did not parse as an app-owned Org file") {
        ApiError::bad_request("project file is not valid orgasmic markup")
    } else {
        ApiError::bad_request("failed to initialize project scaffold")
    }
}

fn register_project_error(error: anyhow::Error) -> ApiError {
    let message = error.to_string();
    if message.contains("already on the board") {
        ApiError::conflict(message)
    } else if is_project_input_error(&message) {
        ApiError::bad_request(message)
    } else if message.contains("parse") || message.contains("valid orgasmic markup") {
        tracing::warn!(error = %error, "project board parse failed during registration");
        ApiError::internal("project board is not valid orgasmic markup")
    } else {
        tracing::warn!(error = %error, "project board update failed");
        ApiError::internal("failed to update project board")
    }
}

fn is_project_input_error(message: &str) -> bool {
    message.starts_with("project_id must ")
        || message.starts_with("default_branch ")
        || message.starts_with("project_root ")
        || message.starts_with("project_name ")
}

async fn get_project(
    State(state): State<ApiState>,
    Extension(identity): Extension<Identity>,
    Path(id): Path<String>,
) -> Result<Json<crate::index::ProjectIndex>, ApiError> {
    let snap = state.index.snapshot().await;
    let supervisor = state.supervisor.snapshot().await;
    // Resolve existence first (404), then gate the capability (403) — mirrors
    // the artifact-handler pattern (`resolve_artifact_project` + `require`).
    let project = snap
        .project(&id)
        .cloned()
        .ok_or_else(|| ApiError::not_found(format!("project {id}")))?;
    authz::require(&identity, Some(&id), Action::ProjectRead)?;
    Ok(Json(apply_task_owners(project, &supervisor.runs)))
}

async fn get_project_tasks(
    State(state): State<ApiState>,
    Extension(identity): Extension<Identity>,
    Path(id): Path<String>,
) -> Result<Json<Vec<crate::index::TaskSummary>>, ApiError> {
    let snap = state.index.snapshot().await;
    let supervisor = state.supervisor.snapshot().await;
    let project = snap
        .project(&id)
        .ok_or_else(|| ApiError::not_found(format!("project {id}")))?;
    authz::require(&identity, Some(&id), Action::TasksRead)?;
    Ok(Json(
        apply_task_owners(project.clone(), &supervisor.runs).tasks,
    ))
}

async fn get_task(
    State(state): State<ApiState>,
    Extension(identity): Extension<Identity>,
    Path((project_id, task_id)): Path<(String, String)>,
) -> Result<Json<crate::index::TaskDetail>, ApiError> {
    let snap = state.index.snapshot().await;
    let project = snap
        .project(&project_id)
        .ok_or_else(|| project_not_found_error(&snap, &project_id))?;
    authz::require(&identity, Some(&project_id), Action::TasksRead)?;
    if !project.tasks.iter().any(|task| task.id == task_id) {
        tracing::warn!(project_id = %project_id, task_id = %task_id, "task not found");
        return Err(ApiError::not_found("task not found"));
    }
    let supervisor = state.supervisor.snapshot().await;
    let project = apply_task_owners(project.clone(), &supervisor.runs);
    let task = project
        .tasks
        .iter()
        .find(|t| t.id == task_id)
        .cloned()
        .expect("task index membership checked above");
    let body = project.task_bodies.get(&task_id).cloned();
    Ok(Json(crate::index::TaskDetail::from_indexed_body(
        task, body,
    )))
}

fn apply_task_owners(
    mut project: crate::index::ProjectIndex,
    runs: &[crate::supervisor::RunSummary],
) -> crate::index::ProjectIndex {
    let mut owners = BTreeMap::<String, (String, String)>::new();
    for run in runs {
        if run
            .project_id
            .as_deref()
            .map(|id| id == project.project_id)
            .unwrap_or(true)
        {
            // A babysitter only watches the performer; it never headlines the
            // task. Prefer any non-babysitter run as the owner regardless of
            // iteration order — a babysitter owns the task only when no
            // performer run exists for it.
            let new_owner = run.role.clone();
            let replace = match owners.get(&run.task_id) {
                None => true,
                Some((existing, _)) => existing == "babysitter" && new_owner != "babysitter",
            };
            if replace {
                owners.insert(run.task_id.clone(), (new_owner, run.run_id.clone()));
            }
        }
    }
    for task in &mut project.tasks {
        if let Some((owner, run_id)) = owners.get(&task.id) {
            task.owner = TaskOwner::Agent(owner.clone());
            task.run_id = Some(run_id.clone());
        } else {
            task.owner = TaskOwner::Human;
            task.run_id = None;
        }
    }
    project
}

/// Resolve a run's role — the worker kind name shown as "who is working" —
/// from its recorded worker id. Babysitters and the manager are not registry
/// workers; unknown/deleted workers fall back to the bare run surface.
fn resolve_run_role(home: &Home, worker_id: &str, kind: RunKind) -> String {
    match kind {
        RunKind::Babysitter => "babysitter".to_string(),
        RunKind::Worker => {
            if worker_id == "manager" {
                "manager".to_string()
            } else {
                load_stage_worker(home, worker_id)
                    .map(|worker| worker_kind_name(worker.kind).to_string())
                    .unwrap_or_else(|_| "worker".to_string())
            }
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct TaskCommentRequest {
    pub actor: String,
    pub body: String,
    #[serde(default)]
    pub run_id: Option<String>,
    #[serde(default)]
    pub artifacts: Vec<String>,
    #[serde(default)]
    pub in_reply_to: Option<String>,
    #[serde(default)]
    pub request_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct TaskCommentResponse {
    pub tx_id: String,
}

async fn post_task_comment(
    State(state): State<ApiState>,
    Path(task_id): Path<String>,
    Json(req): Json<TaskCommentRequest>,
) -> Result<Json<TaskCommentResponse>, ApiError> {
    if req.actor.trim().is_empty() {
        return Err(ApiError::bad_request("comment actor is required"));
    }
    if req.body.trim().is_empty() {
        return Err(ApiError::bad_request("comment body is required"));
    }
    let located = locate_task(&state.index.snapshot().await, &task_id)?;
    let mut extra = vec![("BODY".to_string(), escape_property_value(&req.body))];
    if let Some(run_id) = req.run_id.filter(|value| !value.trim().is_empty()) {
        extra.push(("RUN_ID".to_string(), run_id));
    }
    if !req.artifacts.is_empty() {
        extra.push(("ARTIFACTS".to_string(), req.artifacts.join(" ")));
    }
    if let Some(in_reply_to) = req.in_reply_to.filter(|value| !value.trim().is_empty()) {
        extra.push(("IN_REPLY_TO".to_string(), in_reply_to));
    }
    let tx_id = record_api_tx(
        &state,
        ApiTxRequest {
            ty: "comment".to_string(),
            actor: Some(req.actor),
            project: Some(located.project_id),
            task: Some(task_id),
            target: None,
            reason: "comment".to_string(),
            request_id: req.request_id,
            extra,
        },
    )
    .await?;
    Ok(Json(TaskCommentResponse { tx_id }))
}

async fn get_task_activity(
    State(state): State<ApiState>,
    Extension(identity): Extension<Identity>,
    Path(task_id): Path<String>,
) -> Result<Json<Vec<crate::index::ActivityEntry>>, ApiError> {
    let snap = state.index.snapshot().await;
    let located = locate_task(&snap, &task_id)?;
    authz::require(&identity, Some(&located.project_id), Action::TasksRead)?;
    let entries = snap
        .projects
        .get(&located.project_id)
        .and_then(|project| project.activity_index.get(&task_id))
        .cloned()
        .unwrap_or_default();
    Ok(Json(entries))
}

#[derive(Debug, Deserialize)]
pub struct CreateSubtaskRequest {
    pub title: String,
    /// Worker id to perform the subtask (`:WORKER:`). Omitted → routed to
    /// the pipeline implementer like any worker-less task.
    #[serde(default)]
    pub worker: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub request_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct CreateSubtaskResponse {
    pub id: String,
    pub heading: String,
    pub tx_id: String,
}

async fn post_task_subtask(
    State(state): State<ApiState>,
    Path(parent_id): Path<String>,
    Json(req): Json<CreateSubtaskRequest>,
) -> Result<Json<CreateSubtaskResponse>, ApiError> {
    if !orgasmic_core::is_valid_task_path_id(&parent_id) {
        return Err(ApiError::bad_request(format!(
            "parent task id must match TASK-<stem>(.<n>)* with legacy numeric or minted stem: {parent_id}"
        )));
    }
    if req.title.trim().is_empty() {
        return Err(ApiError::bad_request("subtask title is required"));
    }
    let snap = state.index.snapshot().await;
    let located = locate_task(&snap, &parent_id)?;
    let project = snap
        .projects
        .get(&located.project_id)
        .ok_or_else(|| ApiError::not_found(format!("project {}", located.project_id)))?;
    let worker = req
        .worker
        .as_deref()
        .map(str::trim)
        .filter(|worker| !worker.is_empty());
    if let Some(worker) = worker {
        load_stage_worker(&state.home, worker)
            .map_err(|_| ApiError::bad_request(format!("unknown worker {worker}")))?;
    }
    if !project.tasks.iter().any(|task| task.id == parent_id) {
        return Err(ApiError::not_found(format!("parent task {parent_id}")));
    }
    let subtask_id = next_subtask_id(project, &parent_id);
    let default_worker =
        default_dispatch_worker_id(&state.home, project, DispatchEndpointKind::Implementer);
    let heading = render_subtask_heading(
        &subtask_id,
        &parent_id,
        &req.title,
        worker,
        default_worker.as_deref(),
        req.description.as_deref(),
    );
    // New subtasks are born BACKLOG, and file membership is canonical state
    // (dec_QQYXM) — they land in backlog.org regardless of where the parent
    // currently lives; the file and the heading keyword may never disagree.
    let target_path = task_create_target_path(&project.root);
    let target_rel = task_file_rel(task_create_target_file_name());
    let source = if target_path.exists() {
        read_artifact(&target_path, "task file")?
    } else {
        empty_task_file_header(task_create_target_file_name())
    };
    let new_contents = append_heading_to_task_file(&source, &heading);

    let prepared = prepare_api_tx(
        &state,
        ApiTxRequest {
            ty: "task.created".to_string(),
            actor: None,
            project: Some(located.project_id.clone()),
            task: Some(subtask_id.clone()),
            target: Some(target_rel.clone()),
            reason: format!("created subtask {subtask_id} under {parent_id}"),
            request_id: req.request_id.clone().map(|id| format!("{id}/tx")),
            extra: {
                let mut extra = Vec::new();
                if let Some(worker) = worker {
                    extra.push(("WORKER".to_string(), worker.to_string()));
                }
                extra
            },
        },
    )
    .await?;
    let project_tx = prepared.project_tx;
    let destination_project_id = prepared.destination_project_id.clone();
    let tx_id = state
        .writer
        .transaction(
            vec![FileRewrite {
                path: target_path,
                new_contents: new_contents.into_bytes(),
            }],
            prepared.tx,
        )
        .await
        .map_err(writer_transaction_error)?;
    refresh_after_tx(&state, project_tx, destination_project_id).await;
    Ok(Json(CreateSubtaskResponse {
        id: subtask_id,
        heading,
        tx_id,
    }))
}

#[derive(Debug, Clone)]
struct LocatedTask {
    project_id: String,
}

fn locate_task(snap: &IndexSnapshot, task_id: &str) -> Result<LocatedTask, ApiError> {
    let mut matches = snap
        .projects
        .iter()
        .filter(|(_, project)| project.tasks.iter().any(|task| task.id == task_id))
        .map(|(project_id, _)| project_id.clone())
        .collect::<Vec<_>>();
    matches.sort();
    match matches.len() {
        0 => {
            tracing::warn!(task_id = %task_id, "task not found");
            Err(ApiError::not_found("task not found"))
        }
        1 => Ok(LocatedTask {
            project_id: matches.remove(0),
        }),
        _ => Err(ApiError::bad_request(format!(
            "task {task_id} exists in multiple projects; use a project-scoped route"
        ))),
    }
}

fn next_subtask_id(project: &crate::index::ProjectIndex, parent_id: &str) -> String {
    let max_suffix = project
        .subtasks
        .get(parent_id)
        .into_iter()
        .flat_map(|children| children.iter())
        .filter_map(|child| child.strip_prefix(parent_id))
        .filter_map(|suffix| suffix.strip_prefix('.'))
        .filter_map(|suffix| suffix.split('.').next())
        .filter_map(|suffix| suffix.parse::<u32>().ok())
        .max()
        .unwrap_or(0);
    format!("{}.{}", parent_id, max_suffix + 1)
}

fn render_subtask_heading(
    id: &str,
    _parent_id: &str,
    title: &str,
    worker: Option<&str>,
    default_worker: Option<&str>,
    description: Option<&str>,
) -> String {
    let clean_title = title
        .chars()
        .map(|ch| if ch == '\n' || ch == '\r' { ' ' } else { ch })
        .collect::<String>();
    let worker_line = worker
        .map(str::trim)
        .filter(|worker| !worker.is_empty())
        .filter(|worker| default_worker.map(str::trim) != Some(*worker))
        .map(|worker| format!(":WORKER:           {worker}\n"))
        .unwrap_or_default();
    let mut heading = format!(
        "* BACKLOG {id} {}\n:PROPERTIES:\n:ID:               {id}\n{worker_line}:END:\n",
        clean_title.trim(),
    );
    if let Some(description) = description {
        if !description.trim().is_empty() {
            heading.push_str("\n** Description\n");
            heading.push_str(description.trim_end());
            heading.push('\n');
        }
    }
    heading
}

fn worker_kind_name(kind: WorkerKind) -> &'static str {
    match kind {
        WorkerKind::Implementer => "implementer",
        WorkerKind::Reviewer => "reviewer",
        WorkerKind::Planner => "planner",
        WorkerKind::Analyzer => "analyzer",
        WorkerKind::Architector => "architector",
        WorkerKind::Griller => "griller",
        WorkerKind::Glossarist => "glossarist",
        WorkerKind::Babysitter => "babysitter",
        WorkerKind::Manager => "manager",
        WorkerKind::Artifactor => "artifactor",
    }
}

fn escape_property_value(value: &str) -> String {
    value.replace('\\', "\\\\").replace('\n', "\\n")
}

#[derive(Debug, Deserialize)]
pub struct TxQuery {
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub struct GraphQuery {
    #[serde(default)]
    pub project: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct GraphEdgesQuery {
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub node: Option<String>,
    #[serde(default)]
    pub dir: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub relation: Option<String>,
}

async fn get_tx(
    State(state): State<ApiState>,
    Query(q): Query<TxQuery>,
) -> Json<Vec<crate::index::TxRecord>> {
    let snap = state.index.snapshot().await;
    let mut items: Vec<_> = snap
        .tx
        .into_iter()
        .filter(|r| {
            q.project
                .as_deref()
                .is_none_or(|p| r.project_id.as_deref() == Some(p))
        })
        .collect();
    items.sort_by(|a, b| a.entry.time.cmp(&b.entry.time));
    if let Some(limit) = q.limit {
        if items.len() > limit {
            let skip = items.len() - limit;
            items = items.into_iter().skip(skip).collect();
        }
    }
    Json(items)
}

#[derive(Debug, Deserialize)]
pub struct TxAppendRequest {
    /// Optional stable request id for retry idempotency (AC #4).
    #[serde(default)]
    pub request_id: Option<String>,
    pub r#type: String,
    #[serde(default)]
    pub actor: Option<String>,
    #[serde(default)]
    pub machine: Option<String>,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub task: Option<String>,
    #[serde(default)]
    pub target: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub extra: Vec<(String, String)>,
    /// Override the writer's choice of target file. Defaults to the
    /// daemon's home tx file `$ORGASMIC_HOME/state/tx/YYYY-MM.org`.
    #[serde(default)]
    pub tx_path: Option<PathBuf>,
}

#[derive(Debug, Serialize)]
pub struct TxAppendResponse {
    pub tx_id: String,
    pub tx_path: PathBuf,
    pub time: String,
}

fn tx_time_string_utc(now: &chrono::DateTime<Utc>) -> String {
    now.format("[%Y-%m-%d %a %H:%M:%S]").to_string()
}

fn tx_preserve_id_timestamp_utc(now: &chrono::DateTime<Utc>) -> String {
    now.format("%Y%m%d%H%M%S").to_string()
}

fn tx_project_date_utc(now: &chrono::DateTime<Utc>) -> String {
    now.format("%Y%m%d").to_string()
}

fn tx_file_month_utc(now: &chrono::DateTime<Utc>) -> String {
    now.format("%Y-%m").to_string()
}

async fn post_tx(
    State(state): State<ApiState>,
    Json(req): Json<TxAppendRequest>,
) -> Result<Json<TxAppendResponse>, ApiError> {
    let now = Utc::now();
    let snap = state.index.snapshot().await;
    let project_entry = req
        .project
        .as_deref()
        .and_then(|id| snap.board.iter().find(|entry| entry.id == id))
        .cloned();
    let destination = tx_destination(&state, &req, project_entry.as_ref(), &now)?;
    let tx_id = match &destination.tx_id_policy {
        TxIdPolicy::Preserve => format!(
            "tx-{}-{}",
            tx_preserve_id_timestamp_utc(&now),
            &uuid::Uuid::new_v4().to_string()[..8]
        ),
        TxIdPolicy::ProjectSequence { .. } => "pending-project-sequence".to_string(),
    };
    let time_str = tx_time_string_utc(&now);
    let mut entry = TxEntry::new(
        &tx_id,
        &req.r#type,
        &time_str,
        choose_actor(&req, project_entry.as_ref(), &state),
        req.machine.clone().unwrap_or_else(|| state.machine.clone()),
    );
    entry.project = req.project.clone();
    entry.task = req.task;
    entry.target = req.target;
    entry.reason = req.reason;
    entry.extra = req.extra;

    let project_tx = destination.project_tx;
    let destination_project_id = destination.project_id.clone();
    let res = state
        .writer
        .append_tx(
            TxAppend {
                tx_path: destination.tx_path.clone(),
                entry,
                project_id: destination.project_id.clone(),
                tx_id_policy: destination.tx_id_policy.clone(),
                request_id: None,
            },
            req.request_id,
        )
        .await
        .map_err(writer_append_error)?;
    if project_tx {
        if let Some(project_id) = destination_project_id {
            let _ = state.index.refresh_project(&project_id).await;
        }
    } else {
        state.index.refresh_home_tx().await;
    }
    Ok(Json(TxAppendResponse {
        tx_id: res.tx_id,
        tx_path: res.tx_path,
        time: time_str,
    }))
}

async fn get_workers(
    State(state): State<ApiState>,
) -> Result<Json<Vec<crate::content::WorkerView>>, ApiError> {
    content::list_workers(&state.home)
        .map(Json)
        .map_err(|e| content_list_error(e, "workers"))
}

async fn get_worker(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<Json<crate::content::WorkerView>, ApiError> {
    content::load_worker(&state.home, &id)
        .map(Json)
        .map_err(|e| content_load_error(e, "worker"))
}

async fn get_workers_validate(
    State(state): State<ApiState>,
) -> Result<Json<Vec<crate::content::WorkerValidationResult>>, ApiError> {
    let mut results =
        content::validate_workers(&state.home).map_err(|e| content_list_error(e, "workers"))?;
    for result in &mut results {
        validate_worker_driver_config(&state, result);
    }
    Ok(Json(results))
}

#[derive(Debug, Deserialize)]
struct WorkerValidateRequest {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    source: Option<String>,
}

async fn post_workers_validate(
    State(state): State<ApiState>,
    Json(req): Json<WorkerValidateRequest>,
) -> Result<Json<crate::content::WorkerValidationResult>, ApiError> {
    let id = req.id.as_deref().map(str::trim).filter(|id| !id.is_empty());
    let mut result = if let Some(source) = req.source {
        content::validate_worker_source(&state.home, id, source)
    } else if let Some(id) = id {
        content::validate_worker_by_id(&state.home, id)
    } else {
        return Err(ApiError::bad_request(
            "worker validation needs id or source",
        ));
    };
    validate_worker_driver_config(&state, &mut result);
    Ok(Json(result))
}

async fn get_prompt_specs(
    State(state): State<ApiState>,
) -> Result<Json<Vec<crate::prompt_compiler::PromptSpecView>>, ApiError> {
    crate::prompt_compiler::list_prompt_specs(&state.home)
        .map(Json)
        .map_err(|e| content_list_error(e, "prompt specs"))
}

async fn get_prompt_spec(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<Json<crate::prompt_compiler::PromptSpecView>, ApiError> {
    crate::prompt_compiler::load_prompt_spec(&state.home, &id)
        .map(Json)
        .map_err(|e| content_load_error(e, "prompt spec"))
}

async fn get_prompt_parts(
    State(state): State<ApiState>,
) -> Result<Json<Vec<crate::prompt_compiler::PromptPartView>>, ApiError> {
    crate::prompt_compiler::list_prompt_parts(&state.home)
        .map(Json)
        .map_err(|e| content_list_error(e, "prompt parts"))
}

async fn get_prompt_part(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<Json<crate::prompt_compiler::PromptPartView>, ApiError> {
    crate::prompt_compiler::load_prompt_part_view(&state.home, &id)
        .map(Json)
        .map_err(|e| content_load_error(e, "prompt part"))
}

async fn get_context_packs(
    State(state): State<ApiState>,
) -> Result<Json<Vec<crate::prompt_compiler::ContextPackView>>, ApiError> {
    crate::prompt_compiler::list_context_packs(&state.home)
        .map(Json)
        .map_err(|e| content_list_error(e, "context packs"))
}

async fn post_prompt_spec_compile(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(req): Json<crate::prompt_compiler::PromptCompileRequest>,
) -> Result<Json<crate::prompt_compiler::CompiledPrompt>, ApiError> {
    let req = enrich_prompt_compile_request(&state, req).await?;
    crate::prompt_compiler::compile_prompt_spec(&state.home, &id, req)
        .map(Json)
        .map_err(|e| content_list_error(e, "prompt spec compile"))
}

async fn post_prompt_spec_lint(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(req): Json<crate::prompt_compiler::PromptCompileRequest>,
) -> Result<Json<crate::prompt_compiler::CompiledPrompt>, ApiError> {
    let req = enrich_prompt_compile_request(&state, req).await?;
    crate::prompt_compiler::lint_prompt_spec(&state.home, &id, req)
        .map(Json)
        .map_err(|e| content_list_error(e, "prompt spec lint"))
}

async fn post_prompt_spec_fork(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<Json<crate::prompt_compiler::PromptSpecView>, ApiError> {
    crate::prompt_compiler::fork_prompt_spec(&state.home, &id)
        .map(Json)
        .map_err(|e| content_list_error(e, "prompt spec fork"))
}

async fn post_prompt_spec_save(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(req): Json<crate::prompt_compiler::PromptSpecSaveRequest>,
) -> Result<Json<crate::prompt_compiler::PromptSpecView>, ApiError> {
    crate::prompt_compiler::save_prompt_spec(&state.home, &id, req)
        .map(Json)
        .map_err(|e| content_list_error(e, "prompt spec save"))
}

async fn post_prompt_part_save(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(req): Json<crate::prompt_compiler::PromptPartSaveRequest>,
) -> Result<Json<crate::prompt_compiler::PromptPartView>, ApiError> {
    crate::prompt_compiler::save_prompt_part(&state.home, &id, req)
        .map(Json)
        .map_err(|e| content_list_error(e, "prompt part save"))
}

async fn enrich_prompt_compile_request(
    state: &ApiState,
    mut req: crate::prompt_compiler::PromptCompileRequest,
) -> Result<crate::prompt_compiler::PromptCompileRequest, ApiError> {
    let project_id = req
        .project
        .clone()
        .unwrap_or_else(|| "orgasmic".to_string());
    let snap = state.index.snapshot().await;
    if let Some(project) = snap.projects.get(&project_id) {
        req.project = Some(project_id.clone());
        req.context_overrides
            .entry("project.id".to_string())
            .or_insert_with(|| project_id.clone());
        req.context_overrides
            .entry("project.name".to_string())
            .or_insert_with(|| project_id.clone());
        req.context_overrides
            .entry("project.path".to_string())
            .or_insert_with(|| project.root.display().to_string());
        req.context_overrides
            .entry("project.default_branch".to_string())
            .or_insert_with(|| project.branch.clone());
    }
    Ok(req)
}

async fn get_skills(
    State(state): State<ApiState>,
) -> Result<Json<Vec<crate::content::SkillView>>, ApiError> {
    content::list_skills(&state.home)
        .map(Json)
        .map_err(|e| content_list_error(e, "skills"))
}

async fn get_skill(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<Json<crate::content::SkillView>, ApiError> {
    content::load_skill(&state.home, &id)
        .map(Json)
        .map_err(|e| content_load_error(e, "skill"))
}

#[derive(Debug, Deserialize)]
pub struct ManagerLaunchRequest {
    pub project_id: String,
    pub mode: String,
    pub harness: String,
    /// Optional launch-time model override, threaded into the harness CLI argv
    /// (e.g. `claude --model <m>`). Session-pinned: unlike an in-session
    /// `/model`, it never rewrites the operator's saved harness default.
    #[serde(default)]
    pub model: Option<String>,
    /// Optional reasoning-effort override for harnesses that support it.
    #[serde(default)]
    pub effort: Option<String>,
    /// Extra argv appended verbatim to the harness CLI by the PTY modes —
    /// the launcher's escape hatch for harnesses without typed options.
    #[serde(default)]
    pub harness_args: Vec<String>,
    /// When true (and the driver is rmux), spawn the session "system-wide" so
    /// it survives a daemon restart/rebuild. Defaults ON for the manager in the
    /// UI. Ignored by non-rmux drivers.
    #[serde(default)]
    pub system_wide: bool,
}

#[derive(Debug, Serialize)]
pub struct ManagerLaunchResponse {
    pub run_id: String,
}

#[derive(Debug, Deserialize)]
pub struct ManagerActionRequest {
    #[serde(default)]
    pub ty: Option<String>,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub task: Option<String>,
    #[serde(default)]
    pub target: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub request_id: Option<String>,
    #[serde(default)]
    pub extra: Vec<(String, String)>,
}

#[derive(Debug, Serialize)]
pub struct ManagerActionResponse {
    pub status: String,
    pub tx_id: String,
}

async fn post_manager_launch(
    State(state): State<ApiState>,
    Json(req): Json<ManagerLaunchRequest>,
) -> Result<Json<ManagerLaunchResponse>, ApiError> {
    let snap = state.index.snapshot().await;
    let project = snap
        .projects
        .get(&req.project_id)
        .cloned()
        .ok_or_else(|| ApiError::not_found(format!("project {}", req.project_id)))?;
    drop(snap);

    let driver = driver_for_mode_harness(&req.mode, &req.harness).ok_or_else(|| {
        ApiError::bad_request(format!(
            "unsupported manager driver {}/{}",
            req.mode, req.harness
        ))
    })?;
    let driver_config = DriverConfig::from_value(json!({
        "cwd": project.root.clone(),
        "auto_start_turn": false,
        // Launch-time runtime overrides: the PTY modes append `--model` to the
        // harness argv, so the choice stays pinned to this session instead of
        // rewriting the operator's saved harness default (an in-session
        // `/model` does the latter).
        "model": req.model,
        "effort": req.effort,
        "reasoning_effort": req.effort,
        "harness_args": req.harness_args,
        // Threaded through to the rmux driver so the session is detached from
        // the daemon's lifecycle and survives a restart (no-op for tmux).
        "system_wide": req.system_wide,
    }));
    let driver_config = apply_driver_defaults(
        driver_config,
        &req.mode,
        &req.harness,
        &state.driver_defaults,
    );

    let now = Utc::now();
    let session_path = project_sessions_dir(&project.root).join(format!(
        "manager-{}-{}.jsonl",
        req.project_id,
        now.format("%Y%m%dT%H%M%S")
    ));
    let task_id = format!("manager.launch:{}", req.project_id);
    let acquire = state
        .supervisor
        .acquire(
            driver.as_ref(),
            AcquireRequest {
                task_id,
                kind: RunKind::Worker,
                worker_id: "manager".into(),
                role: "manager".into(),
                project_id: Some(req.project_id.clone()),
                worktree: Some(project.root),
                last_path: None,
                stdout_path: None,
                session_path,
                driver_config,
                babysitter_target: None,
                // The manager is interactive and operator-paced: it idles at a
                // prompt waiting for a human, which must never read as a
                // stalled worker. 0 disables both detectors.
                stall_timeout_secs: Some(0),
                max_run_duration_secs: Some(0),
                idle_timeout_secs: None,
                babysitter: None,
            },
        )
        .await
        .map_err(|e| supervisor_acquire_error("manager launch", e))?;

    Ok(Json(ManagerLaunchResponse {
        run_id: acquire.run_id,
    }))
}

async fn post_manager_action(
    State(state): State<ApiState>,
    Json(req): Json<ManagerActionRequest>,
) -> Result<Json<ManagerActionResponse>, ApiError> {
    let mut extra = req.extra;
    if let Some(ty) = req.ty {
        extra.push(("ACTION_TYPE".to_string(), ty));
    }
    let tx_id = record_api_tx(
        &state,
        ApiTxRequest {
            ty: "manager.action".to_string(),
            actor: None,
            project: req.project.or_else(|| Some("orgasmic".to_string())),
            task: req.task,
            target: req
                .target
                .or_else(|| Some(DEFAULT_TASK_FILE_REL.to_string())),
            reason: req
                .reason
                .unwrap_or_else(|| "manager action recorded".to_string()),
            request_id: req.request_id,
            extra,
        },
    )
    .await?;
    Ok(Json(ManagerActionResponse {
        status: "recorded".to_string(),
        tx_id,
    }))
}

async fn get_manager_state(
    State(state): State<ApiState>,
) -> Json<crate::supervisor::SupervisorSnapshot> {
    Json(state.supervisor.snapshot().await)
}

#[derive(Debug, Serialize)]
pub struct ManagerDriverProfile {
    pub mode: String,
    pub harness: String,
    pub binary: String,
    pub display_name: String,
    /// Standalone transport label, e.g. "tmux" / "acp" — for grouping the UI by mode.
    pub mode_label: String,
    /// Standalone provider label, e.g. "Claude" / "Codex" — the leaf choice within a mode.
    pub harness_label: String,
    pub installed: bool,
    /// Mode-level binary requirement, when the mode itself needs a separately
    /// provisioned binary on top of the harness CLI. `rmux` (TASK-104) needs a
    /// real `rmux` daemon binary; the catalog checks it independently of the
    /// harness binary so a missing prerequisite is reported honestly.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode_binary: Option<String>,
    /// Whether [`Self::mode_binary`] resolves on PATH (or via
    /// `RMUX_SDK_DAEMON_BINARY` for rmux). `None` when the mode has no extra
    /// binary requirement.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode_installed: Option<bool>,
}

#[derive(Debug, Serialize)]
pub struct ManagerDriversResponse {
    pub drivers: Vec<ManagerDriverProfile>,
}

/// CLI binary expected on PATH for a given harness.
fn harness_binary(harness: &str) -> &str {
    match harness {
        "claude" => "claude",
        "codex" => "codex",
        "cursor-agent" => "cursor-agent",
        "hermes" => "hermes",
        // Bare terminal pseudo-harness: the shell is always present.
        "custom" => "sh",
        other => other,
    }
}

/// Standalone provider label for a harness, e.g. "Claude". The leaf choice once
/// a transport mode is picked.
fn harness_label(harness: &str) -> &str {
    match harness {
        "claude" => "Claude",
        "codex" => "Codex",
        "cursor-agent" => "Cursor",
        "hermes" => "Hermes",
        "custom" => "Custom",
        other => other,
    }
}

/// Standalone transport label for a mode, e.g. "tmux" / "acp". The first choice
/// the UI groups drivers by.
fn mode_label(mode: &str) -> &str {
    match mode {
        "tmux" => "tmux",
        "rmux" => "rmux",
        "acp-stdio" => "acp",
        "acp-ws" => "acp-ws",
        "subprocess-stream-json" => "stream-json",
        other => other,
    }
}

/// Human-facing label for a `(mode, harness)` driver, e.g. "Claude (tmux)".
fn driver_display_name(mode: &str, harness: &str) -> String {
    format!("{} ({})", harness_label(harness), mode_label(mode))
}

/// True when `binary` resolves to a file on the current PATH.
fn binary_on_path(binary: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join(binary).is_file()))
        .unwrap_or(false)
}

/// Mode-level binary a driver needs *in addition to* its harness CLI, plus
/// whether it currently resolves. Returns `None` for modes with no extra
/// binary requirement. `rmux` (TASK-104) needs a separately provisioned `rmux`
/// daemon binary, discovered via `RMUX_SDK_DAEMON_BINARY` or PATH — checked
/// independently of the harness binary so a missing prerequisite is honest.
fn mode_binary_status(mode: &str) -> Option<(String, bool)> {
    match mode {
        "rmux" => {
            let probe = orgasmic_drivers::probe_rmux_binary();
            let display = probe.path.clone().unwrap_or_else(|| "rmux".to_string());
            Some((display, probe.found))
        }
        _ => None,
    }
}

/// Driver-adaptive launcher catalog (dec_029): every supported `(mode, harness)`
/// pair plus whether its CLI binary is installed, so the UI can offer the same
/// "choose driver, then start" affordance HAR exposes in its board-manager drawer.
async fn get_manager_drivers() -> Json<ManagerDriversResponse> {
    let drivers = orgasmic_drivers::SUPPORTED
        .iter()
        .map(|&(mode, harness)| {
            let binary = harness_binary(harness);
            let mode_status = mode_binary_status(mode);
            ManagerDriverProfile {
                mode: mode.to_string(),
                harness: harness.to_string(),
                binary: binary.to_string(),
                display_name: driver_display_name(mode, harness),
                mode_label: mode_label(mode).to_string(),
                harness_label: harness_label(harness).to_string(),
                installed: binary_on_path(binary),
                mode_binary: mode_status.as_ref().map(|(b, _)| b.clone()),
                mode_installed: mode_status.as_ref().map(|(_, ok)| *ok),
            }
        })
        .collect();
    Json(ManagerDriversResponse { drivers })
}

async fn post_question(Json(body): Json<Value>) -> Json<Value> {
    Json(json!({
        "kind": "question.recorded",
        "tracked_by": "TASK-007",
        "status": "recorded",
        "body": body,
    }))
}

async fn post_question_answer(Path(id): Path<String>, Json(body): Json<Value>) -> Json<Value> {
    Json(json!({
        "kind": "question.answered",
        "tracked_by": "TASK-007",
        "question_id": id,
        "body": body,
    }))
}

#[derive(Debug, Clone, Deserialize)]
pub struct StageRequest {
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub task_id: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub context: Option<String>,
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub request_id: Option<String>,
    #[serde(default)]
    pub values: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy)]
struct StageSpec {
    stage: &'static str,
    requested_tx: &'static str,
    worker_id: &'static str,
    prompt_kind: &'static str,
    target: &'static str,
    default_reason: &'static str,
}

async fn post_grill(
    State(state): State<ApiState>,
    Json(req): Json<StageRequest>,
) -> Result<Json<Value>, ApiError> {
    post_stage(&state, stage_spec("grill"), req, None).await
}

async fn post_architect(
    State(state): State<ApiState>,
    Json(req): Json<StageRequest>,
) -> Result<Json<Value>, ApiError> {
    post_stage(&state, stage_spec("architect"), req, None).await
}

async fn post_plan(
    State(state): State<ApiState>,
    Json(req): Json<StageRequest>,
) -> Result<Json<Value>, ApiError> {
    post_stage(&state, stage_spec("plan"), req, None).await
}

fn stage_spec(stage: &str) -> StageSpec {
    match stage {
        "grill" => StageSpec {
            stage: "grill",
            requested_tx: "grill.requested",
            worker_id: "griller",
            prompt_kind: "griller",
            target: DEFAULT_TASK_FILE_REL,
            default_reason: "grill stage requested",
        },
        "architect" => StageSpec {
            stage: "architect",
            requested_tx: "architect.requested",
            worker_id: "architector",
            prompt_kind: "architector",
            target: ".orgasmic/architecture.org",
            default_reason: "architect stage requested",
        },
        "plan" => StageSpec {
            stage: "plan",
            requested_tx: "plan.requested",
            worker_id: "planner",
            prompt_kind: "planner",
            target: DEFAULT_TASK_FILE_REL,
            default_reason: "plan stage requested",
        },
        _ => unreachable!("unknown stage spec"),
    }
}

async fn post_stage(
    state: &ApiState,
    spec: StageSpec,
    req: StageRequest,
    snapshot_id: Option<String>,
) -> Result<Json<Value>, ApiError> {
    let project_id = req
        .project
        .clone()
        .unwrap_or_else(|| "orgasmic".to_string());
    let requested_task_id = req.task_id.clone();
    let snap = state.index.snapshot().await;
    let project = snap
        .projects
        .get(&project_id)
        .cloned()
        .ok_or_else(|| ApiError::not_found(format!("project {project_id}")))?;
    drop(snap);

    let worker = load_stage_worker(&state.home, spec.worker_id)?;
    if let Some(skill) = worker.missing_skills.first() {
        return Err(ApiError::bad_request(format!(
            "unresolved slot skills.{skill}"
        )));
    }

    let reason = stage_reason(spec, &req);
    let task_id = req
        .task_id
        .clone()
        .unwrap_or_else(|| format!("stage:{}", spec.stage));
    let mut values = req.values.clone();
    fill_stage_slot_values(
        state,
        spec,
        &project,
        &worker,
        &task_id,
        &reason,
        &mut values,
    )?;

    let prompt = compile_stage_prompt(&state.home, spec, &mut values)?;
    let bundle = stage_prompt_bundle(spec, &worker, &prompt.id, &prompt.compiled);
    let driver = driver_for_mode_harness(&worker.driver, &worker.harness).ok_or_else(|| {
        ApiError::internal(format!(
            "driver registry missing {}/{}",
            worker.driver, worker.harness
        ))
    })?;
    let driver_config = stage_driver_config(
        &worker,
        &project.root,
        &project.root,
        &bundle,
        &state.driver_defaults,
        state.tmux_input_ready_timeout_secs,
    );
    driver
        .validate(&driver_config)
        .map_err(|e| driver_validate_error(&worker.driver, e))?;

    let now = Utc::now();
    let session_path = project_sessions_dir(&project.root).join(format!(
        "{}-{}.jsonl",
        spec.stage,
        now.format("%Y%m%dT%H%M%S")
    ));
    let acquire = state
        .supervisor
        .acquire(
            driver.as_ref(),
            AcquireRequest {
                task_id: task_id.clone(),
                kind: RunKind::Worker,
                worker_id: worker.id.clone(),
                role: worker_kind_name(worker.kind).to_string(),
                project_id: Some(project_id.clone()),
                worktree: Some(project.root.clone()),
                last_path: None,
                stdout_path: None,
                session_path: session_path.clone(),
                driver_config,
                babysitter_target: None,
                stall_timeout_secs: worker.stall_timeout_secs,
                max_run_duration_secs: worker.max_run_duration_secs,
                idle_timeout_secs: None,
                babysitter: None,
            },
        )
        .await
        .map_err(|e| supervisor_acquire_error(spec.stage, e))?;

    let mut extra = vec![
        ("RUN_ID".to_string(), acquire.run_id.clone()),
        ("WORKER".to_string(), worker.id.clone()),
        ("PROMPT".to_string(), prompt.id.clone()),
        ("DRIVER".to_string(), worker.driver.clone()),
        ("HARNESS".to_string(), worker.harness.clone()),
    ];
    if let Some(snapshot_id) = snapshot_id.as_deref() {
        extra.push(("SNAPSHOT_ID".to_string(), snapshot_id.to_string()));
    }

    let tx_id = record_api_tx(
        state,
        ApiTxRequest {
            ty: spec.requested_tx.to_string(),
            actor: None,
            project: Some(project_id.clone()),
            task: Some(task_id.clone()),
            target: Some(spec.target.to_string()),
            reason: reason.clone(),
            request_id: req.request_id,
            extra,
        },
    )
    .await?;

    state.events.publish(
        Topic::Run,
        EventPayload::StageRequested {
            stage: spec.stage.to_string(),
            project_id: project_id.clone(),
            task_id: requested_task_id,
            run_id: acquire.run_id.clone(),
            tx_id: tx_id.clone(),
            snapshot_id: snapshot_id.clone(),
        },
    );

    spawn_stage_completion_watcher(
        state.clone(),
        StageCompletion {
            stage: spec.stage.to_string(),
            project_id: project_id.clone(),
            task_id,
            target: spec.target.to_string(),
            run_id: acquire.run_id.clone(),
            session_path: session_path.clone(),
        },
    );

    Ok(Json(json!({
        "stage": spec.stage,
        "status": "acquired",
        "project": project_id,
        "run_id": acquire.run_id,
        "runtime_id": acquire.identity.runtime_id,
        "boot_id": acquire.identity.boot_id,
        "session_path": session_path,
        "tx_id": tx_id,
        "worker_id": worker.id,
        "prompt_id": prompt.id,
        "driver": worker.driver,
        "harness": worker.harness,
    })))
}

fn stage_reason(spec: StageSpec, req: &StageRequest) -> String {
    req.reason
        .clone()
        .or_else(|| req.context.clone())
        .or_else(|| req.scope.clone())
        .unwrap_or_else(|| spec.default_reason.to_string())
}

#[derive(Debug, Clone)]
struct StageWorker {
    id: String,
    kind: WorkerKind,
    driver: String,
    harness: String,
    models: Vec<String>,
    reasoning_efforts: Vec<String>,
    default_provider: Option<String>,
    default_model: Option<String>,
    default_effort: Option<String>,
    linked_skills: Vec<String>,
    persona: Option<String>,
    operating_rules: Option<String>,
    missing_skills: Vec<String>,
    babysitter_worker: Option<String>,
    stall_timeout_secs: Option<u32>,
    max_run_duration_secs: Option<u32>,
    sandbox_permissions: Option<SandboxAllowlist>,
    /// Extra argv appended verbatim to the harness CLI (`:HARNESS_ARGS:`).
    harness_args: Vec<String>,
}

#[derive(Debug, Clone, Default)]
struct DriverOverrides {
    provider: Option<String>,
    model: Option<String>,
    effort: Option<String>,
}

#[derive(Debug, Clone)]
struct CompiledStagePrompt {
    id: String,
    compiled: String,
}

fn load_stage_worker(home: &Home, id: &str) -> Result<StageWorker, ApiError> {
    match content::load_worker(home, id) {
        Ok(worker) => Ok(stage_worker_from_view(worker)),
        Err(ContentLoadError::NotFoundByRule { .. }) => {
            let rel = PathBuf::from("workers").join(format!("{id}.org"));
            let Some(path) = resolve_stage_content_path(home, &rel) else {
                tracing::warn!(worker_id = %id, "worker not found");
                return Err(ApiError::not_found("worker not found"));
            };
            load_stage_worker_path(home, &path, id)
        }
        Err(loader_error) => Err(content_load_error(loader_error, "worker")),
    }
}

fn stage_worker_from_view(worker: crate::content::WorkerView) -> StageWorker {
    StageWorker {
        id: worker.id,
        kind: worker.kind,
        driver: worker.driver,
        harness: worker.harness,
        models: worker.models,
        reasoning_efforts: worker.reasoning_efforts,
        default_provider: worker.default_provider,
        default_model: worker.default_model,
        default_effort: worker.default_effort,
        linked_skills: worker.linked_skills,
        persona: worker.persona,
        operating_rules: worker.operating_rules,
        missing_skills: worker.missing_skills,
        babysitter_worker: worker.babysitter_worker,
        stall_timeout_secs: worker.stall_timeout_secs,
        max_run_duration_secs: worker.max_run_duration_secs,
        sandbox_permissions: worker.sandbox_permissions,
        harness_args: worker.harness_args,
    }
}

fn validate_worker_driver_config(
    state: &ApiState,
    result: &mut crate::content::WorkerValidationResult,
) {
    if !result.errors.is_empty() {
        result.ok = false;
        return;
    }
    let Some(worker_view) = result.worker.clone() else {
        result.ok = false;
        return;
    };
    let worker = stage_worker_from_view(worker_view);
    let Some(driver) = driver_for_mode_harness(&worker.driver, &worker.harness) else {
        result
            .errors
            .push(crate::content::WorkerValidationDiagnostic {
                code: "unsupported_driver_harness".to_string(),
                message: format!(
                    "unsupported driver/harness pair {}/{}",
                    worker.driver, worker.harness
                ),
            });
        result.ok = false;
        return;
    };
    let source = state.home.source();
    let worktree = if source.exists() {
        source
    } else {
        state.home.root.clone()
    };
    let driver_config = stage_driver_config_with_overrides(
        &worker,
        &worktree,
        &worktree,
        "",
        DriverOverrides::default(),
        None,
        &state.driver_defaults,
        state.tmux_input_ready_timeout_secs,
    );
    if let Err(error) = driver.validate(&driver_config) {
        result
            .errors
            .push(crate::content::WorkerValidationDiagnostic {
                code: "driver_config".to_string(),
                message: format!("driver configuration is invalid: {error}"),
            });
        result.ok = false;
    }
}

fn load_stage_worker_path(home: &Home, path: &FsPath, id: &str) -> Result<StageWorker, ApiError> {
    let source = read_artifact(path, "worker")?;
    let file = OrgFile::parse(source, path.to_string_lossy())
        .map_err(|e| org_parse_internal(path, "worker", e))?;
    let worker = Worker::from_org(&file, path.to_string_lossy().as_ref()).map_err(|e| {
        tracing::error!(path = %path.display(), error = %e, worker_id = %id, "worker schema validation failed");
        ApiError::internal("failed to parse worker")
    })?;
    let linked_skills = worker
        .linked_skills
        .iter()
        .map(|skill| (*skill).to_string())
        .collect::<Vec<_>>();
    let missing_skills = linked_skills
        .iter()
        .filter(|skill| {
            resolve_stage_content_path(home, &PathBuf::from("skills").join(format!("{skill}.org")))
                .is_none()
        })
        .cloned()
        .collect::<Vec<_>>();
    Ok(StageWorker {
        id: worker.id.to_string(),
        kind: worker.kind,
        driver: worker.driver.to_string(),
        harness: worker.harness.to_string(),
        models: worker.models,
        reasoning_efforts: worker.reasoning_efforts,
        default_provider: worker.default_provider,
        default_model: worker.default_model,
        default_effort: worker.default_effort,
        linked_skills,
        persona: worker.persona,
        operating_rules: worker.operating_rules,
        missing_skills,
        babysitter_worker: worker.babysitter_worker.map(str::to_string),
        harness_args: worker.harness_args.clone(),
        stall_timeout_secs: worker.stall_timeout_secs,
        max_run_duration_secs: worker.max_run_duration_secs,
        sandbox_permissions: worker.sandbox_permissions.clone(),
    })
}

fn compile_stage_prompt(
    home: &Home,
    spec: StageSpec,
    values: &mut SlotValues,
) -> Result<CompiledStagePrompt, ApiError> {
    let req = crate::prompt_compiler::PromptCompileRequest {
        project: values.get("project.id").cloned(),
        mode: Some(spec.stage.to_string()),
        worker: values.get("worker.id").cloned(),
        harness: None,
        renderer: None,
        reason: values.get("task.description").cloned(),
        context_overrides: BTreeMap::new(),
        values: values.clone(),
    };
    let compiled_prompt = crate::prompt_compiler::compile_prompt_spec(home, spec.prompt_kind, req)
        .map_err(|e| content_list_error(e, "stage prompt spec"))?;
    if crate::prompt_compiler::has_error(&compiled_prompt.diagnostics) {
        let messages = compiled_prompt
            .diagnostics
            .iter()
            .filter(|diag| diag.level == "error")
            .map(|diag| diag.message.as_str())
            .collect::<Vec<_>>()
            .join("; ");
        return Err(ApiError::bad_request(format!(
            "stage prompt compile failed: {messages}"
        )));
    }
    Ok(CompiledStagePrompt {
        id: compiled_prompt.spec.id,
        compiled: compiled_prompt.text,
    })
}

fn fill_stage_slot_values(
    state: &ApiState,
    spec: StageSpec,
    project: &crate::index::ProjectIndex,
    worker: &StageWorker,
    task_id: &str,
    reason: &str,
    values: &mut SlotValues,
) -> Result<(), ApiError> {
    let project_defaults = project_prompt_defaults(project);
    insert_slot(values, "task.id", task_id.to_string());
    insert_slot(
        values,
        "task.title",
        format!("{} pipeline stage", spec.stage),
    );
    insert_slot(values, "task.description", reason.to_string());
    insert_slot(values, "task.priority", "P2".to_string());
    insert_slot(values, "task.tags", format!("pipeline,{}", spec.stage));
    insert_slot(values, "task.acceptance", stage_acceptance(spec, reason));
    insert_slot(
        values,
        "task.write_scope",
        prompt_lines_or_not_set(&project_defaults.write_scope),
    );
    insert_slot(
        values,
        "task.read_scope",
        ".orgasmic/**\nshipped/**\ncrates/**".to_string(),
    );
    insert_slot(values, "task.depends_on", String::new());
    insert_slot(
        values,
        "task.activity",
        task_activity_slot(project, task_id),
    );
    insert_slot(values, "project.id", project.project_id.clone());
    insert_slot(values, "project.name", project.project_id.clone());
    insert_slot(values, "project.path", project.root.display().to_string());
    insert_slot(values, "project.test_cmd", project_defaults.test_cmd);
    insert_slot(values, "project.lint_cmd", project_defaults.lint_cmd);
    insert_slot(values, "project.build_cmd", project_defaults.build_cmd);
    insert_slot(
        values,
        "project.default_branch",
        project_defaults.default_branch,
    );
    insert_slot(values, "worker.id", worker.id.clone());
    insert_slot(values, "worker.kind", spec.prompt_kind.to_string());
    insert_slot(
        values,
        "worker.persona",
        worker.persona.clone().unwrap_or_default(),
    );
    insert_slot(
        values,
        "worker.operating_rules",
        worker.operating_rules.clone().unwrap_or_default(),
    );
    insert_slot(values, "skills.all", skills_manifest(&state.home)?);
    insert_slot(values, "run.id", "pending".to_string());
    insert_slot(values, "run.iteration_count", "0".to_string());
    insert_slot(values, "run.previous_state", "none".to_string());
    insert_slot(values, "evidence.so_far", graph_evidence(project));
    insert_slot(values, "worklog.tail", String::new());
    hydrate_skill_slots(state, worker, values)?;
    Ok(())
}

fn insert_slot(values: &mut SlotValues, key: &str, value: String) {
    values.entry(key.to_string()).or_insert(value);
}

fn task_activity_slot(project: &crate::index::ProjectIndex, task_id: &str) -> String {
    project
        .activity_index
        .get(task_id)
        .map(|entries| {
            entries
                .iter()
                .map(|entry| {
                    format!(
                        "{} · {} · {:?} · {}",
                        entry.time, entry.actor, entry.kind, entry.body
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

fn prompt_value_or_not_set(value: impl AsRef<str>) -> String {
    let trimmed = value.as_ref().trim();
    if trimmed.is_empty() {
        "not set".to_string()
    } else {
        trimmed.to_string()
    }
}

fn prompt_lines_or_not_set(values: &[String]) -> String {
    if values.is_empty() {
        "not set".to_string()
    } else {
        values.join("\n")
    }
}

fn acceptance_prompt_value(body: Option<&crate::index::TaskBody>) -> String {
    let Some(body) = body else {
        return "not set".to_string();
    };
    if body.acceptance_criteria.is_empty() {
        return "not set".to_string();
    }
    body.acceptance_criteria
        .iter()
        .map(|item| {
            let marker = match item.state {
                crate::index::AcceptanceState::Checked => "X",
                crate::index::AcceptanceState::Partial => "-",
                crate::index::AcceptanceState::Unchecked => " ",
            };
            format!("- [{marker}] {}", item.text)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn resolve_dispatch_task_summary<'a>(
    project: &'a crate::index::ProjectIndex,
    task_id: &str,
) -> Result<&'a crate::index::TaskSummary, ApiError> {
    if let Some(task) = project.tasks.iter().find(|task| task.id == task_id) {
        return Ok(task);
    }
    let primary = task_id
        .split_whitespace()
        .next()
        .filter(|id| !id.is_empty())
        .unwrap_or(task_id);
    project
        .tasks
        .iter()
        .find(|task| task.id == primary)
        .ok_or_else(|| ApiError::not_found("task not found"))
}

fn hydrate_dispatch_task_slots(
    values: &mut SlotValues,
    project: &crate::index::ProjectIndex,
    task_id: &str,
) -> Result<(), ApiError> {
    let summary = resolve_dispatch_task_summary(project, task_id)?;
    let project_defaults = project_prompt_defaults(project);
    let lookup_id = summary.id.as_str();
    let body = project.task_bodies.get(lookup_id);
    values.insert("task.id".to_string(), task_id.to_string());
    values.insert(
        "task.title".to_string(),
        prompt_value_or_not_set(&summary.title),
    );
    values.insert(
        "task.description".to_string(),
        prompt_value_or_not_set(body.map(|body| body.description.as_str()).unwrap_or("")),
    );
    values.insert("task.acceptance".to_string(), acceptance_prompt_value(body));
    values.insert(
        "task.read_scope".to_string(),
        prompt_lines_or_not_set(&summary.read_scope),
    );
    let write_scope = if summary.write_scope.is_empty() {
        &project_defaults.write_scope
    } else {
        &summary.write_scope
    };
    values.insert(
        "task.write_scope".to_string(),
        prompt_lines_or_not_set(write_scope),
    );
    let test_cmd = summary
        .test_cmd
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(project_defaults.test_cmd.as_str());
    values.insert(
        "task.test_cmd".to_string(),
        prompt_value_or_not_set(test_cmd),
    );
    values.insert(
        "task.priority".to_string(),
        prompt_value_or_not_set(summary.priority.as_deref().unwrap_or("")),
    );
    values.insert(
        "task.tags".to_string(),
        prompt_lines_or_not_set(&summary.tags),
    );
    values.insert(
        "task.depends_on".to_string(),
        prompt_lines_or_not_set(&summary.depends_on),
    );
    values.insert(
        "task.activity".to_string(),
        prompt_value_or_not_set(task_activity_slot(project, lookup_id)),
    );
    Ok(())
}

fn hydrate_dispatch_project_slots(values: &mut SlotValues, project: &crate::index::ProjectIndex) {
    let project_defaults = project_prompt_defaults(project);
    values.insert("project.id".to_string(), project.project_id.clone());
    values.insert("project.name".to_string(), project.project_id.clone());
    values.insert(
        "project.path".to_string(),
        project.root.display().to_string(),
    );
    values.insert(
        "project.default_branch".to_string(),
        project_defaults.default_branch,
    );
    values.insert("project.test_cmd".to_string(), project_defaults.test_cmd);
    values.insert("project.lint_cmd".to_string(), project_defaults.lint_cmd);
    values.insert("project.build_cmd".to_string(), project_defaults.build_cmd);
}

fn hydrate_dispatch_worker_slots(
    values: &mut SlotValues,
    worker: &StageWorker,
    kind: DispatchEndpointKind,
) {
    values.insert("worker.id".to_string(), worker.id.clone());
    values.insert("worker.kind".to_string(), kind.as_str().to_string());
    values.insert(
        "worker.persona".to_string(),
        worker.persona.clone().unwrap_or_default(),
    );
    values.insert(
        "worker.operating_rules".to_string(),
        worker.operating_rules.clone().unwrap_or_default(),
    );
}

#[derive(Default)]
struct ProjectPromptDefaults {
    default_branch: String,
    test_cmd: String,
    lint_cmd: String,
    build_cmd: String,
    write_scope: Vec<String>,
}

fn project_prompt_defaults(project: &crate::index::ProjectIndex) -> ProjectPromptDefaults {
    let mut defaults = ProjectPromptDefaults {
        default_branch: project.branch.clone(),
        test_cmd: project.default_test_cmd.clone().unwrap_or_default(),
        write_scope: project.default_write_scope.clone(),
        ..ProjectPromptDefaults::default()
    };
    let path = project.root.join(".orgasmic/config.org");
    let Ok(source) = std::fs::read_to_string(&path) else {
        return defaults;
    };
    let Ok(file) = OrgFile::parse(source, path.to_string_lossy()) else {
        return defaults;
    };
    let Ok(config) = ProjectConfig::from_org(&file, path.to_string_lossy().as_ref()) else {
        return defaults;
    };
    if let Some(value) = config.default_branch {
        defaults.default_branch = value.to_string();
    }
    if let Some(value) = config.test_cmd {
        defaults.test_cmd = value.to_string();
    }
    if let Some(value) = config.lint_cmd {
        defaults.lint_cmd = value.to_string();
    }
    if let Some(value) = config.build_cmd {
        defaults.build_cmd = value.to_string();
    }
    if !config.write_scope.is_empty() {
        defaults.write_scope = config.write_scope.into_iter().map(str::to_string).collect();
    }
    defaults
}

fn stage_acceptance(spec: StageSpec, reason: &str) -> String {
    match spec.stage {
        "grill" => format!("- [ ] Record findings or questions for: {reason}"),
        "architect" => format!("- [ ] Draft or update architecture for: {reason}"),
        "plan" => format!("- [ ] Draft implementation-ready tasks for: {reason}"),
        _ => format!("- [ ] Complete stage: {reason}"),
    }
}

fn skills_manifest(home: &Home) -> Result<String, ApiError> {
    let skills = content::list_skills(home).map_err(|e| content_list_error(e, "skills"))?;
    if skills.is_empty() {
        return Ok(String::new());
    }
    Ok(skills
        .into_iter()
        .map(|skill| {
            format!(
                "- {}: {} [{}]",
                skill.id,
                skill.description.unwrap_or_default(),
                skill
                    .absolute_path
                    .unwrap_or_else(|| skill.source_path.display().to_string())
            )
        })
        .collect::<Vec<_>>()
        .join("\n"))
}

fn graph_evidence(project: &crate::index::ProjectIndex) -> String {
    if project.graph.nodes.is_empty() {
        return "No indexed graph nodes.".to_string();
    }
    project
        .graph
        .nodes
        .iter()
        .take(40)
        .map(|node| format!("- {} {}", node.layer, node.id))
        .collect::<Vec<_>>()
        .join("\n")
}

fn hydrate_skill_slots(
    state: &ApiState,
    worker: &StageWorker,
    values: &mut SlotValues,
) -> Result<(), ApiError> {
    for skill in &worker.linked_skills {
        fill_skill_slot(&state.home, &format!("skills.{skill}"), values)?;
        fill_skill_slot(&state.home, &format!("skill_path.{skill}"), values)?;
    }
    Ok(())
}

fn fill_skill_slot(home: &Home, slot: &str, values: &mut SlotValues) -> Result<(), ApiError> {
    if values.contains_key(slot) {
        return Ok(());
    }
    let (prefix, skill_id) = slot
        .split_once('.')
        .ok_or_else(|| ApiError::bad_request(format!("unresolved slot {slot}")))?;
    let rel = PathBuf::from("skills").join(format!("{skill_id}.org"));
    let Some(path) = resolve_stage_content_path(home, &rel) else {
        return Err(ApiError::bad_request(format!("unresolved slot {slot}")));
    };
    let value = if prefix == "skill_path" {
        path.display().to_string()
    } else {
        read_artifact(&path, "skill file")?
    };
    values.insert(slot.to_string(), value);
    Ok(())
}

fn resolve_stage_content_path(home: &Home, relative: &FsPath) -> Option<PathBuf> {
    resolve_loader(home, relative).or_else(|| {
        resolve_stage_repo_root()
            .map(|root| root.join("shipped").join(relative))
            .filter(|path| path.exists())
    })
}

fn resolve_stage_repo_root() -> Option<PathBuf> {
    if let Ok(cwd) = std::env::current_dir() {
        if cwd.join("shipped").is_dir() {
            return Some(cwd);
        }
    }
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .and_then(FsPath::parent)
        .map(FsPath::to_path_buf)
        .filter(|path| path.join("shipped").is_dir())
}

fn stage_prompt_bundle(
    spec: StageSpec,
    worker: &StageWorker,
    prompt_id: &str,
    compiled_prompt: &str,
) -> String {
    format!(
        "orgasmic compiled prompt\nstage: {}\nworker: {}\nprompt_spec: {}\n\n{}\n",
        spec.stage,
        worker.id,
        prompt_id,
        compiled_prompt.trim()
    )
}

fn sandbox_allowlist_to_csv(list: &SandboxAllowlist) -> String {
    format!(
        "allow_exec={},allow_patch={},allow_network={},allow_writes_outside_cwd={}",
        list.allow_exec, list.allow_patch, list.allow_network, list.allow_writes_outside_cwd
    )
}

fn stage_driver_config(
    worker: &StageWorker,
    project_root: &FsPath,
    worktree: &FsPath,
    bundle: &str,
    driver_defaults: &DriverDefaults,
    tmux_input_ready_timeout_secs: Option<u64>,
) -> DriverConfig {
    stage_driver_config_with_overrides(
        worker,
        project_root,
        worktree,
        bundle,
        DriverOverrides::default(),
        None,
        driver_defaults,
        tmux_input_ready_timeout_secs,
    )
}

#[allow(clippy::too_many_arguments)]
fn stage_driver_config_with_overrides(
    worker: &StageWorker,
    project_root: &FsPath,
    worktree: &FsPath,
    bundle: &str,
    overrides: DriverOverrides,
    task_sandbox_permissions: Option<&SandboxAllowlist>,
    driver_defaults: &DriverDefaults,
    tmux_input_ready_timeout_secs: Option<u64>,
) -> DriverConfig {
    let endpoint = if worker.harness == "cursor-agent" {
        "stdio"
    } else {
        ""
    };
    let provider = overrides
        .provider
        .or_else(|| worker.default_provider.clone());
    let model = overrides.model.or_else(|| worker.default_model.clone());
    let effort = overrides.effort.or_else(|| worker.default_effort.clone());
    let reasoning_effort = effort.clone();
    let resolved_sandbox = SandboxAllowlist::resolve(
        task_sandbox_permissions,
        worker.sandbox_permissions.as_ref(),
    );
    let mut config = json!({
        "transport": worker.driver,
        "harness": worker.harness,
        "endpoint": endpoint,
        "provider": provider,
        "model": model,
        "effort": effort,
        "reasoning_effort": reasoning_effort,
        "harness_args": worker.harness_args,
        "command": "sh",
        "args": ["-lc", "echo orgasmic pipeline stage acquired; exec sh"],
        "cwd": worktree,
        "project_root": project_root,
        "prompt_bundle_text": bundle,
        "sandbox_permissions": sandbox_allowlist_to_csv(&resolved_sandbox),
    });
    if let Some(secs) = tmux_input_ready_timeout_secs {
        if let Some(map) = config.as_object_mut() {
            map.insert("input_ready_timeout".to_string(), json!(secs));
        }
    }
    if worker.kind == WorkerKind::Artifactor && worker.driver == "rmux" {
        if let Some(map) = config.as_object_mut() {
            // orgasmic:dec_S02E2 — hot-session artifactor: persist across grilling
            // rounds; not system_wide so daemon shutdown reaps the pane cleanly.
            map.insert("persistent".to_string(), json!(true));
        }
    }
    apply_driver_defaults(
        DriverConfig::from_value(config),
        &worker.driver,
        &worker.harness,
        driver_defaults,
    )
}

fn apply_driver_defaults(
    mut config: DriverConfig,
    mode: &str,
    harness: &str,
    defaults: &DriverDefaults,
) -> DriverConfig {
    if mode != "acp-ws" || harness != "hermes" {
        return config;
    }
    let Some(endpoint) = defaults.hermes.acp_ws.endpoint.as_deref() else {
        return config;
    };
    let Some(map) = config.0.as_object_mut() else {
        return config;
    };
    let endpoint_is_empty = map
        .get("endpoint")
        .and_then(Value::as_str)
        .map(str::is_empty)
        .unwrap_or(true);
    if endpoint_is_empty {
        map.insert("endpoint".to_string(), json!(endpoint));
    }
    if let Some(token_env) = defaults.hermes.acp_ws.session_token_env.as_deref() {
        if let Ok(token) = std::env::var(token_env) {
            let token = token.trim();
            if !token.is_empty() {
                map.entry("session_token".to_string())
                    .or_insert_with(|| json!(token));
            }
        }
    }
    config
}

/// Compose a self-contained prompt from a worker's persona and operating rules,
/// for workers (like the babysitter) that are spawned without compiling a
/// prompt-spec.
fn compose_worker_prompt(worker: &StageWorker) -> String {
    let mut out = String::new();
    if let Some(persona) = worker
        .persona
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        out.push_str(persona);
    }
    if let Some(rules) = worker
        .operating_rules
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str("Operating rules:\n");
        out.push_str(rules);
    }
    out
}

fn build_babysitter_auto_spawn(
    home: &Home,
    implementer: &StageWorker,
    worktree: &FsPath,
    driver_defaults: &DriverDefaults,
) -> Result<Option<BabysitterAutoSpawn>, ApiError> {
    let Some(babysitter_id) = implementer.babysitter_worker.as_deref() else {
        return Ok(None);
    };
    let babysitter = load_stage_worker(home, babysitter_id)?;
    if let Some(skill) = babysitter.missing_skills.first() {
        return Err(ApiError::bad_request(format!(
            "unresolved slot skills.{skill} on babysitter worker {babysitter_id}"
        )));
    }
    let Some(_driver) = driver_for_mode_harness(&babysitter.driver, &babysitter.harness) else {
        return Err(ApiError::bad_request(format!(
            "unsupported babysitter driver/harness pair {}/{}",
            babysitter.driver, babysitter.harness
        )));
    };
    let bundle = compose_worker_prompt(&babysitter);
    let driver_config = stage_driver_config_with_overrides(
        &babysitter,
        worktree,
        worktree,
        &bundle,
        DriverOverrides::default(),
        None,
        driver_defaults,
        None,
    );
    Ok(Some(BabysitterAutoSpawn {
        worker_id: babysitter.id,
        mode: babysitter.driver,
        harness: babysitter.harness,
        driver_config,
        stall_timeout_secs: babysitter.stall_timeout_secs,
        max_run_duration_secs: babysitter.max_run_duration_secs,
    }))
}

#[derive(Debug, Deserialize)]
struct DispatchRequest {
    pub kind: String,
    pub brief_path: PathBuf,
    pub worktree_path: PathBuf,
    pub last_path: PathBuf,
    pub stdout_path: PathBuf,
    #[serde(default)]
    pub worker_id: Option<String>,
    #[serde(default)]
    pub model_override: Option<String>,
    #[serde(default)]
    pub effort_override: Option<String>,
    #[serde(default)]
    pub provider_override: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub liveness: Option<String>,
    #[serde(default)]
    pub goal_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct DispatchResponse {
    pub run_id: String,
    pub session_path: PathBuf,
    pub pid: u32,
    pub worker_id: String,
    pub driver: String,
    pub harness: String,
    pub dispatch_tx_id: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DispatchEndpointKind {
    Implementer,
    Reviewer,
    Architector,
}

impl DispatchEndpointKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Implementer => "implementer",
            Self::Reviewer => "reviewer",
            Self::Architector => "architector",
        }
    }

    fn worker_kind(self) -> WorkerKind {
        match self {
            Self::Implementer => WorkerKind::Implementer,
            Self::Reviewer => WorkerKind::Reviewer,
            Self::Architector => WorkerKind::Architector,
        }
    }

    fn run_kind(self) -> RunKind {
        // Reviewer + Architector dispatches are normal worker runs in the
        // supervisor. The dispatched role is passed separately to the run
        // summary; the driver runtime kind stays non-babysitter so they have
        // the same tool surface as implementers.
        RunKind::Worker
    }
}

impl FromStr for DispatchEndpointKind {
    type Err = ApiError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "implementer" => Ok(Self::Implementer),
            "reviewer" => Ok(Self::Reviewer),
            "architector" => Ok(Self::Architector),
            other => Err(ApiError::bad_request(format!(
                "unknown dispatch kind {other}"
            ))),
        }
    }
}

/// Shared worker spawn pipeline for CLI dispatch.
struct SpawnWorkerRequest<'a> {
    project_id: &'a str,
    task_id: &'a str,
    worker_id: &'a str,
    run_kind: RunKind,
    bundle: &'a str,
    overrides: DriverOverrides,
    project_root_path: &'a FsPath,
    worktree_path: &'a FsPath,
    /// Dispatch artifact paths (CLI dispatch only). `None` for
    /// non-dispatch spawns (artifact generation); threaded into `RunMeta` so
    /// a boot reattach can respawn the dispatch completion watcher.
    last_path: Option<&'a FsPath>,
    stdout_path: Option<&'a FsPath>,
    origin: &'static str,
    /// Session-path fragment for CLI dispatch (`implementer` / `reviewer`).
    dispatch_kind: Option<&'a str>,
    /// When set, skips a second org read for the same worker id in one request.
    preloaded_worker: Option<StageWorker>,
    /// Task-heading sandbox override, when known for this spawn.
    task_sandbox_permissions: Option<SandboxAllowlist>,
}

struct SpawnWorkerResult {
    acquire: AcquireResponse,
    worker: StageWorker,
    session_path: PathBuf,
    pid: u32,
}

struct SpawnWorkerFailure {
    error: ApiError,
}

async fn spawn_worker_run(
    state: &ApiState,
    req: SpawnWorkerRequest<'_>,
) -> Result<SpawnWorkerResult, SpawnWorkerFailure> {
    crate::supervisor::record_spawn_pipeline_poll();
    let worker = match req.preloaded_worker {
        Some(worker) => worker,
        None => match load_stage_worker(&state.home, req.worker_id) {
            Ok(worker) => worker,
            Err(error) => {
                return Err(SpawnWorkerFailure { error });
            }
        },
    };
    if let Some(skill) = worker.missing_skills.first() {
        return Err(SpawnWorkerFailure {
            error: ApiError::bad_request(format!("unresolved slot skills.{skill}")),
        });
    }

    let Some(driver) = driver_for_mode_harness(&worker.driver, &worker.harness) else {
        return Err(SpawnWorkerFailure {
            error: ApiError::bad_request(format!(
                "unsupported driver/harness pair {}/{}",
                worker.driver, worker.harness
            )),
        });
    };

    let driver_config = stage_driver_config_with_overrides(
        &worker,
        req.project_root_path,
        req.worktree_path,
        req.bundle,
        req.overrides,
        req.task_sandbox_permissions.as_ref(),
        &state.driver_defaults,
        state.tmux_input_ready_timeout_secs,
    );
    if let Err(error) = driver.validate(&driver_config) {
        return Err(SpawnWorkerFailure {
            error: driver_validate_error(&worker.driver, &error),
        });
    }

    let now = Utc::now();
    let fragment = safe_session_fragment(req.task_id);
    let timestamp = now.format("%Y%m%dT%H%M%S");
    let kind = req.dispatch_kind.ok_or_else(|| SpawnWorkerFailure {
        error: ApiError::internal("dispatch spawn missing dispatch_kind fragment"),
    })?;
    let session_path = project_sessions_dir(req.project_root_path)
        .join(format!("dispatch-{fragment}-{kind}-{timestamp}.jsonl"));

    let babysitter = if req.run_kind == RunKind::Worker {
        match build_babysitter_auto_spawn(
            &state.home,
            &worker,
            req.worktree_path,
            &state.driver_defaults,
        ) {
            Ok(babysitter) => babysitter,
            Err(error) => {
                return Err(SpawnWorkerFailure { error });
            }
        }
    } else {
        None
    };

    let acquire = match state
        .supervisor
        .acquire(
            driver.as_ref(),
            AcquireRequest {
                task_id: req.task_id.to_string(),
                kind: req.run_kind,
                worker_id: worker.id.clone(),
                role: kind.to_string(),
                project_id: Some(req.project_id.to_string()),
                worktree: Some(req.worktree_path.to_path_buf()),
                last_path: req.last_path.map(|p| p.to_path_buf()),
                stdout_path: req.stdout_path.map(|p| p.to_path_buf()),
                session_path: session_path.clone(),
                driver_config,
                babysitter_target: None,
                // Only the persistent hot-session artifactor (rmux) spawn
                // opts into idle release; every other dispatch (one-shot
                // implementer/reviewer/etc.) stays exempt. Mirrors the same
                // `persistent` condition `stage_driver_config_with_overrides`
                // uses to flag the rmux driver_config as persistent. That
                // same condition must disable stall for these runs too, or
                // the 600s stall pre-empts the 900s idle window and the run
                // is still released before idle ever gets a chance to fire.
                stall_timeout_secs: (worker.kind == WorkerKind::Artifactor
                    && worker.driver == "rmux")
                    .then_some(0)
                    .or(worker.stall_timeout_secs),
                max_run_duration_secs: worker.max_run_duration_secs,
                idle_timeout_secs: (worker.kind == WorkerKind::Artifactor
                    && worker.driver == "rmux")
                    .then_some(DEFAULT_IDLE_TIMEOUT_SECS),
                babysitter,
            },
        )
        .await
    {
        Ok(acquire) => acquire,
        Err(crate::supervisor::SupervisorError::LeaseHeld { run_id, .. }) => {
            return Err(SpawnWorkerFailure {
                error: ApiError::conflict(format!("another dispatch is already active: {run_id}")),
            });
        }
        Err(error) => {
            tracing::error!(
                task_id = %req.task_id,
                origin = req.origin,
                error = %error,
                "spawn_worker_run acquire failed"
            );
            return Err(SpawnWorkerFailure {
                error: supervisor_acquire_error("dispatch", error),
            });
        }
    };

    // Return immediately with the wrapper pid. The precise worker-child
    // ("watch") pid used to be resolved here for cli dispatches, blocking the
    // HTTP handler up to 5s (`poll_direct_child_pid`) — combined with the
    // driver's input-ready wait that blew past the CLI's 10s timeout and left a
    // zombie lease. The wrapper pid already serves the operator's `ps -p` watch
    // hint (the wrapper outlives the worker), and the precise child pid is
    // resolved off the hot path below for observability.
    let pid = acquire.pid.unwrap_or(0);
    if req.origin == "cli_dispatch" {
        if let Some(wrapper_pid) = acquire.pid.filter(|p| *p != 0) {
            let run_id = acquire.run_id.clone();
            tokio::spawn(async move {
                if let Some(watch_pid) = resolve_dispatch_watch_pid(Some(wrapper_pid)).await {
                    tracing::info!(
                        %run_id,
                        wrapper_pid,
                        watch_pid,
                        "resolved dispatch worker watch pid"
                    );
                }
            });
        }
    }

    Ok(SpawnWorkerResult {
        pid,
        acquire,
        worker,
        session_path,
    })
}

/// Dispatch a manager-driven implementer/reviewer run through the supervisor.
///
/// `--model` / `--effort` / `--provider` overrides in [`DispatchRequest`] are
/// trusted manager input: they bypass worker eligibility checks and are applied
/// directly to the staged driver config after schema validation only.
async fn post_task_dispatch(
    State(state): State<ApiState>,
    Path((project_id, task_id)): Path<(String, String)>,
    Json(req): Json<DispatchRequest>,
) -> Result<Json<DispatchResponse>, ApiError> {
    let kind = DispatchEndpointKind::from_str(&req.kind)?;
    validate_dispatch_path("brief_path", &req.brief_path)?;
    validate_dispatch_path("worktree_path", &req.worktree_path)?;
    validate_dispatch_path("last_path", &req.last_path)?;
    validate_dispatch_path("stdout_path", &req.stdout_path)?;

    let snap = state.index.snapshot().await;
    let project = snap
        .projects
        .get(&project_id)
        .cloned()
        .ok_or_else(|| ApiError::not_found(format!("project {project_id}")))?;
    let task_summary = project.tasks.iter().find(|task| task.id == task_id);
    let task_sandbox_permissions = task_summary.and_then(|task| task.sandbox_permissions.clone());
    let task_worker = task_summary.and_then(|task| task.worker.clone());
    drop(snap);

    let worker_id = resolve_dispatch_worker_id(
        &state.home,
        &project,
        kind,
        req.worker_id.as_deref(),
        task_worker.as_deref(),
    )?;
    let worker_for_bundle = load_stage_worker(&state.home, &worker_id)?;
    if let Some(skill) = worker_for_bundle.missing_skills.first() {
        return Err(ApiError::bad_request(format!(
            "unresolved slot skills.{skill}"
        )));
    }
    let brief = read_artifact(&req.brief_path, "dispatch brief")?;
    let bundle = compile_dispatch_prompt_bundle(
        &state.home,
        &project,
        kind,
        &task_id,
        &worker_for_bundle,
        &brief,
    )?;
    let overrides = DriverOverrides {
        provider: non_empty_field(req.provider_override.clone()),
        model: non_empty_field(req.model_override.clone()),
        effort: non_empty_field(req.effort_override.clone()),
    };
    warn_dispatch_overrides(&worker_for_bundle, &overrides);
    let spawn = spawn_worker_run(
        &state,
        SpawnWorkerRequest {
            project_id: &project_id,
            task_id: &task_id,
            worker_id: &worker_id,
            run_kind: kind.run_kind(),
            bundle: &bundle,
            overrides,
            project_root_path: &project.root,
            worktree_path: &req.worktree_path,
            last_path: Some(&req.last_path),
            stdout_path: Some(&req.stdout_path),
            origin: "cli_dispatch",
            dispatch_kind: Some(kind.as_str()),
            preloaded_worker: Some(worker_for_bundle),
            task_sandbox_permissions,
        },
    )
    .await
    .map_err(|failure| failure.error)?;
    let SpawnWorkerResult {
        acquire,
        worker,
        session_path,
        pid,
    } = spawn;

    let dispatch_tx_id = record_dispatch_started(
        &state,
        DispatchStartedRecord {
            project_id: &project_id,
            project_root: &project.root,
            task_id: &task_id,
            kind,
            req: &req,
            run_id: &acquire.run_id,
        },
    )
    .await?;

    record_dispatch_created(
        &state,
        DispatchCreatedRecord {
            project_id: &project_id,
            project_root: &project.root,
            task_id: &task_id,
            kind,
            req: &req,
            acquire: &acquire,
            session_path: &session_path,
            worker: &worker,
            dispatch_tx_id: &dispatch_tx_id,
        },
    )
    .await?;

    spawn_dispatch_completion_watcher(
        state.clone(),
        DispatchCompletion {
            project_id: project_id.clone(),
            task_id: task_id.clone(),
            run_id: acquire.run_id.clone(),
            session_path: session_path.clone(),
            last_path: req.last_path.clone(),
            stdout_path: req.stdout_path.clone(),
            worktree_path: req.worktree_path.clone(),
        },
    );

    if let Some(delay) = state.dispatch_response_delay {
        tokio::time::sleep(delay).await;
    }

    Ok(Json(DispatchResponse {
        run_id: acquire.run_id,
        session_path,
        pid,
        worker_id: worker.id,
        driver: worker.driver,
        harness: worker.harness,
        dispatch_tx_id,
    }))
}

#[derive(Debug, Default, Deserialize)]
pub struct LeaseReleaseRequest {
    /// Lease kind to clear; `implementer` (default, covers all dispatch
    /// kinds — see `DispatchEndpointKind::run_kind`) or `babysitter`.
    #[serde(default)]
    pub kind: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct LeaseReleaseResponse {
    pub status: String,
    pub run_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct DispatchCleanupRequest {
    pub kind: String,
    pub worktree_path: PathBuf,
    pub branch: String,
}

#[derive(Debug, Serialize)]
pub struct DispatchCleanupResponse {
    pub status: String,
    pub released_run_id: Option<String>,
    pub worktree_removed: bool,
    pub branch_deleted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Daemon-executed rollback for a failed or timed-out CLI dispatch. Per the
/// fencing invariant: kill any live worker (driver release / PGID reap), wait
/// for confirmed death, then delete worktree + branch. The CLI only requests
/// this endpoint and reports the result.
async fn post_task_dispatch_cleanup(
    State(state): State<ApiState>,
    Path((project_id, task_id)): Path<(String, String)>,
    Json(req): Json<DispatchCleanupRequest>,
) -> Result<Json<DispatchCleanupResponse>, ApiError> {
    let kind = DispatchEndpointKind::from_str(&req.kind)?;
    validate_dispatch_path("worktree_path", &req.worktree_path)?;
    if req.branch.trim().is_empty() {
        return Err(ApiError::bad_request("branch must be non-empty"));
    }

    let snap = state.index.snapshot().await;
    let project = snap
        .projects
        .get(&project_id)
        .cloned()
        .ok_or_else(|| ApiError::not_found(format!("project {project_id}")))?;
    drop(snap);

    let released_run_id = state
        .supervisor
        .release_dispatch_worker_for_cleanup(&task_id, kind.run_kind())
        .await
        .map_err(|error| ApiError::internal(error.to_string()))?;

    let mut errors = Vec::new();
    let worktree_removed = match remove_dispatch_worktree(&project.root, &req.worktree_path) {
        Ok(removed) => removed,
        Err(err) => {
            errors.push(format!("worktree: {err}"));
            false
        }
    };
    let branch_deleted = match delete_dispatch_branch(&project.root, &req.branch) {
        Ok(deleted) => deleted,
        Err(err) => {
            errors.push(format!("branch: {err}"));
            false
        }
    };

    let status = match (worktree_removed, branch_deleted, errors.is_empty()) {
        (true, true, true) => "ok",
        (false, false, true) => "noop",
        (true, false, _) | (false, true, _) => "partial",
        _ => "failed",
    };

    Ok(Json(DispatchCleanupResponse {
        status: status.into(),
        released_run_id,
        worktree_removed,
        branch_deleted,
        error: if errors.is_empty() {
            None
        } else {
            Some(errors.join("; "))
        },
    }))
}

fn remove_dispatch_worktree(project_root: &FsPath, path: &FsPath) -> Result<bool, String> {
    if !path.exists() {
        return Ok(false);
    }
    let output = Command::new("git")
        .args(["worktree", "remove", "--force"])
        .arg(path)
        .current_dir(project_root)
        .output()
        .map_err(|err| err.to_string())?;
    if output.status.success() {
        orgasmic_core::prune_dispatch_stem_after_worktree(path);
        Ok(true)
    } else {
        Err(format!(
            "{}{}",
            String::from_utf8_lossy(&output.stderr),
            String::from_utf8_lossy(&output.stdout)
        ))
    }
}

fn delete_dispatch_branch(project_root: &FsPath, branch: &str) -> Result<bool, String> {
    let output = Command::new("git")
        .args(["branch", "-D", branch])
        .current_dir(project_root)
        .output()
        .map_err(|err| err.to_string())?;
    if output.status.success() {
        Ok(true)
    } else {
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stderr),
            String::from_utf8_lossy(&output.stdout)
        );
        if combined.contains("not found") {
            Ok(false)
        } else {
            Err(combined)
        }
    }
}

/// Clear an orphaned dispatch lease — one whose run record is gone (e.g. the
/// dispatching CLI timed out and gave up while the daemon held the lease).
/// Refuses (409) when a live run still holds the lease: release the run
/// instead. This is the manager's lever for a stuck lease, replacing the
/// "restart the daemon" anti-pattern (which the restart guard now refuses).
async fn post_task_lease_release(
    State(state): State<ApiState>,
    Path((_project_id, task_id)): Path<(String, String)>,
    Json(req): Json<LeaseReleaseRequest>,
) -> Result<Json<LeaseReleaseResponse>, ApiError> {
    let kind = match req.kind.as_deref().unwrap_or("implementer") {
        "implementer" | "reviewer" | "architector" => RunKind::Worker,
        "babysitter" => RunKind::Babysitter,
        other => return Err(ApiError::bad_request(format!("unknown lease kind {other}"))),
    };
    match state
        .supervisor
        .release_orphaned_lease(&task_id, kind)
        .await
    {
        crate::supervisor::OrphanedLeaseOutcome::Released { run_id } => {
            tracing::info!(%task_id, %run_id, "cleared orphaned lease");
            Ok(Json(LeaseReleaseResponse {
                status: "released".into(),
                run_id: Some(run_id),
            }))
        }
        crate::supervisor::OrphanedLeaseOutcome::NoLease => Ok(Json(LeaseReleaseResponse {
            status: "no_lease".into(),
            run_id: None,
        })),
        crate::supervisor::OrphanedLeaseOutcome::HeldByLiveRun { run_id } => {
            Err(ApiError::conflict(format!(
                "lease for {task_id} is held by live run {run_id}; release the run instead"
            )))
        }
    }
}

fn validate_dispatch_path(field: &str, path: &FsPath) -> Result<(), ApiError> {
    if !path.is_absolute() {
        return Err(ApiError::bad_request(format!("{field} must be absolute")));
    }
    if field == "worktree_path" {
        validate_dispatch_worktree_dir(path)?;
    }
    Ok(())
}

fn validate_dispatch_worktree_dir(path: &FsPath) -> Result<(), ApiError> {
    match std::fs::metadata(path) {
        Ok(meta) if meta.is_dir() => Ok(()),
        _ => Err(ApiError::bad_request(
            "worktree path does not exist or is not a directory",
        )),
    }
}

fn warn_dispatch_overrides(worker: &StageWorker, overrides: &DriverOverrides) {
    if let Some(model) = overrides.model.as_deref() {
        if !worker.models.iter().any(|allowed| allowed == model) {
            tracing::warn!(
                worker_id = %worker.id,
                model = %model,
                allowed_models = ?worker.models,
                "dispatch model override is not listed in worker :MODELS:"
            );
        }
    }
    if let Some(effort) = overrides.effort.as_deref() {
        if !worker
            .reasoning_efforts
            .iter()
            .any(|allowed| allowed == effort)
        {
            tracing::warn!(
                worker_id = %worker.id,
                effort = %effort,
                allowed_efforts = ?worker.reasoning_efforts,
                "dispatch effort override is not listed in worker :REASONING_EFFORTS:"
            );
        }
    }
}

fn resolve_dispatch_worker_id(
    home: &Home,
    project: &ProjectIndex,
    kind: DispatchEndpointKind,
    explicit: Option<&str>,
    task_worker: Option<&str>,
) -> Result<String, ApiError> {
    if let Some(worker) = explicit.map(str::trim).filter(|worker| !worker.is_empty()) {
        return Ok(worker.to_string());
    }
    if let Some(worker) = task_worker
        .map(str::trim)
        .filter(|worker| !worker.is_empty())
    {
        return Ok(worker.to_string());
    }
    let worker_kind = kind.worker_kind();
    first_pipeline_worker_for_kind(home, project, worker_kind)?.ok_or_else(|| {
        ApiError::bad_request(format!(
            "no worker specified and no pipeline worker for {}",
            worker_kind_name(worker_kind)
        ))
    })
}

fn first_pipeline_worker_for_kind(
    home: &Home,
    project: &ProjectIndex,
    kind: WorkerKind,
) -> Result<Option<String>, ApiError> {
    for worker_id in &project.worker_pipeline {
        let worker = load_stage_worker(home, worker_id)?;
        if worker.kind == kind {
            return Ok(Some(worker.id));
        }
    }
    Ok(None)
}

fn compile_dispatch_prompt_bundle(
    home: &Home,
    project: &ProjectIndex,
    kind: DispatchEndpointKind,
    task_id: &str,
    worker: &StageWorker,
    brief: &str,
) -> Result<String, ApiError> {
    let mut values = SlotValues::new();
    values.insert("task.id".to_string(), task_id.to_string());
    values.insert("dispatch.brief".to_string(), prompt_value_or_not_set(brief));
    hydrate_dispatch_task_slots(&mut values, project, task_id)?;
    hydrate_dispatch_project_slots(&mut values, project);
    hydrate_dispatch_worker_slots(&mut values, worker, kind);
    let req = crate::prompt_compiler::PromptCompileRequest {
        project: Some(project.project_id.clone()),
        mode: Some("dispatch".to_string()),
        worker: Some(worker.id.clone()),
        harness: Some(worker.harness.clone()),
        renderer: None,
        reason: Some(format!("{} dispatch for {task_id}", kind.as_str())),
        context_overrides: BTreeMap::new(),
        values,
    };
    let compiled = crate::prompt_compiler::compile_prompt_spec(home, kind.as_str(), req)
        .map_err(|e| content_list_error(e, "dispatch prompt spec"))?;
    if crate::prompt_compiler::has_error(&compiled.diagnostics) {
        let messages = compiled
            .diagnostics
            .iter()
            .filter(|diag| diag.level == "error")
            .map(|diag| diag.message.as_str())
            .collect::<Vec<_>>()
            .join("; ");
        return Err(ApiError::bad_request(format!(
            "dispatch prompt compile failed: {messages}"
        )));
    }
    Ok(format!(
        "orgasmic compiled prompt\ndispatch_kind: {}\ntask: {}\nworker: {}\nprompt_spec: {}\n\n{}\n",
        kind.as_str(),
        task_id,
        worker.id,
        compiled.spec.id,
        compiled.text.trim()
    ))
}

struct DispatchStartedRecord<'a> {
    project_id: &'a str,
    project_root: &'a FsPath,
    task_id: &'a str,
    kind: DispatchEndpointKind,
    req: &'a DispatchRequest,
    run_id: &'a str,
}

fn dispatch_expected_next(kind: DispatchEndpointKind) -> &'static str {
    match kind {
        DispatchEndpointKind::Implementer => "fix-implementer-watch-then-integrate",
        DispatchEndpointKind::Reviewer => "reviewer-watch-then-fix-or-close",
        DispatchEndpointKind::Architector => "architector-watch-then-integrate",
    }
}

/// Render a path for tx records without leaking the local directory layout
/// into the project tx log (committed to git, possibly public). Paths under
/// the project root become project-relative; paths under the orgasmic home
/// (sessions, state) become home-relative. Anything else (e.g. `/tmp`
/// worktrees) is recorded verbatim.
fn tx_safe_path(path: &FsPath, project_root: Option<&FsPath>, home: &Home) -> String {
    if let Some(root) = project_root {
        if let Ok(rel) = path.strip_prefix(root) {
            return rel.display().to_string();
        }
    }
    if let Ok(rel) = path.strip_prefix(&home.root) {
        return rel.display().to_string();
    }
    path.display().to_string()
}

async fn record_dispatch_started(
    state: &ApiState,
    record: DispatchStartedRecord<'_>,
) -> Result<String, ApiError> {
    let started_at = Utc::now().format("[%Y-%m-%d %a %H:%M:%S]").to_string();
    let mut extra = vec![
        ("KIND".to_string(), record.kind.as_str().to_string()),
        (
            "WORKTREE".to_string(),
            record.req.worktree_path.display().to_string(),
        ),
        (
            "BRANCH".to_string(),
            non_empty_field(record.req.branch.clone()).unwrap_or_default(),
        ),
        (
            "BRIEF_PATH".to_string(),
            tx_safe_path(
                &record.req.brief_path,
                Some(record.project_root),
                &state.home,
            ),
        ),
        ("STARTED_AT".to_string(), started_at),
        (
            "LIVENESS".to_string(),
            non_empty_field(record.req.liveness.clone()).unwrap_or_default(),
        ),
        (
            "NEXT".to_string(),
            dispatch_expected_next(record.kind).to_string(),
        ),
    ];
    if let Some(model) = non_empty_field(record.req.model_override.clone()) {
        extra.push(("MODEL".to_string(), model));
    }
    if let Some(effort) = non_empty_field(record.req.effort_override.clone()) {
        extra.push(("EFFORT".to_string(), effort));
    }
    if let Some(goal_id) = non_empty_field(record.req.goal_id.clone()) {
        extra.push(("GOAL_ID".to_string(), goal_id));
    }
    if let Some(reason) = non_empty_field(record.req.reason.clone()) {
        extra.push(("REASON_INITIAL".to_string(), reason));
    }
    record_api_tx(
        state,
        ApiTxRequest {
            ty: "manager.dispatch_started".to_string(),
            actor: None,
            project: Some(record.project_id.to_string()),
            task: Some(record.task_id.to_string()),
            target: None,
            reason: String::new(),
            request_id: Some(format!("dispatch-started-{}", record.run_id)),
            extra,
        },
    )
    .await
}

struct DispatchCreatedRecord<'a> {
    project_id: &'a str,
    project_root: &'a FsPath,
    task_id: &'a str,
    kind: DispatchEndpointKind,
    req: &'a DispatchRequest,
    acquire: &'a crate::supervisor::AcquireResponse,
    session_path: &'a FsPath,
    worker: &'a StageWorker,
    dispatch_tx_id: &'a str,
}

async fn record_dispatch_created(
    state: &ApiState,
    record: DispatchCreatedRecord<'_>,
) -> Result<String, ApiError> {
    let mut extra = vec![
        ("RUN_ID".to_string(), record.acquire.run_id.clone()),
        ("ORIGIN".to_string(), "cli_dispatch".to_string()),
        ("WORKER".to_string(), record.worker.id.clone()),
        ("KIND".to_string(), record.kind.as_str().to_string()),
        ("DRIVER".to_string(), record.worker.driver.clone()),
        ("HARNESS".to_string(), record.worker.harness.clone()),
        (
            "WORKTREE".to_string(),
            record.req.worktree_path.display().to_string(),
        ),
        (
            "BRIEF_PATH".to_string(),
            tx_safe_path(
                &record.req.brief_path,
                Some(record.project_root),
                &state.home,
            ),
        ),
        (
            "LAST_PATH".to_string(),
            tx_safe_path(
                &record.req.last_path,
                Some(record.project_root),
                &state.home,
            ),
        ),
        (
            "STDOUT_PATH".to_string(),
            tx_safe_path(
                &record.req.stdout_path,
                Some(record.project_root),
                &state.home,
            ),
        ),
    ];
    if let Some(pid) = record.acquire.pid {
        extra.push(("PID".to_string(), pid.to_string()));
    }
    extra.push(("DISPATCH_TX".to_string(), record.dispatch_tx_id.to_string()));
    if let Some(model) = non_empty_field(record.req.model_override.clone()) {
        extra.push(("MODEL_OVERRIDE".to_string(), model));
    }
    if let Some(effort) = non_empty_field(record.req.effort_override.clone()) {
        extra.push(("EFFORT_OVERRIDE".to_string(), effort));
    }
    if let Some(provider) = non_empty_field(record.req.provider_override.clone()) {
        extra.push(("PROVIDER_OVERRIDE".to_string(), provider));
    }
    record_api_tx(
        state,
        ApiTxRequest {
            ty: "run.created".to_string(),
            actor: None,
            project: Some(record.project_id.to_string()),
            task: Some(record.task_id.to_string()),
            target: Some(tx_safe_path(
                record.session_path,
                Some(record.project_root),
                &state.home,
            )),
            reason: record.req.reason.clone().unwrap_or_else(|| {
                format!(
                    "cli dispatch {} with {}",
                    record.kind.as_str(),
                    record.worker.id
                )
            }),
            request_id: None,
            extra,
        },
    )
    .await
}

#[derive(Clone)]
struct DispatchCompletion {
    project_id: String,
    task_id: String,
    run_id: String,
    session_path: PathBuf,
    last_path: PathBuf,
    stdout_path: PathBuf,
    worktree_path: PathBuf,
}

/// Follow the run while the supervisor holds it live, then flush dispatch
/// artifact files. Deliberately anchors NO wall clock at dispatch start: a
/// run may live arbitrarily long and still get artifacts on release. (The
/// original watcher used a fixed 30s clock from dispatch start, TASK-060.6 /
/// TASK-066, which silently dropped artifacts for long runs.) The only timer
/// is the post-release grace (`dispatch_watcher_grace`) that lets the session
/// file settle before finalizing without a terminal marker.
fn spawn_dispatch_completion_watcher(state: ApiState, completion: DispatchCompletion) {
    tokio::spawn(async move {
        let grace = state.dispatch_watcher_grace;
        let poll = std::time::Duration::from_millis(250);
        let mut finalize_deadline: Option<std::time::Instant> = None;
        loop {
            let live = state
                .supervisor
                .snapshot()
                .await
                .runs
                .iter()
                .any(|run| run.run_id == completion.run_id);
            if live {
                finalize_deadline = None;
                tokio::time::sleep(poll).await;
                continue;
            }
            let deadline =
                *finalize_deadline.get_or_insert_with(|| std::time::Instant::now() + grace);
            let grace_elapsed = std::time::Instant::now() >= deadline;
            let Ok(envelopes) = read_session_file(&completion.session_path) else {
                if grace_elapsed {
                    tracing::warn!(
                        run_id = %completion.run_id,
                        path = %completion.session_path.display(),
                        "dispatch completion watcher gave up: session file unreadable after post-release grace"
                    );
                    return;
                }
                tokio::time::sleep(poll).await;
                continue;
            };
            // dec_3M7M0: a worker-declared finalize (`orgasmic dispatch
            // finalize`) is the PRIMARY completion signal. If it landed, the
            // worker already wrote last.txt/stdout.log verbatim and emitted
            // its own terminal tx — this watcher's scrollback-scrape is a
            // FALLBACK and must never scrape or overwrite that report.
            if dispatch_release_finalized_by_worker(&envelopes) {
                tracing::info!(
                    run_id = %completion.run_id,
                    "dispatch completion watcher: worker finalized via `orgasmic dispatch finalize`, skipping scrape"
                );
                return;
            }
            // The worker never finalized and the run was killed by a timeout:
            // do NOT synthesize a last.txt that would make this look like a
            // normal completion. Flag it orphan instead so the manager can
            // rescue it (acceptance #5, dec_3M7M0). This check must run BEFORE
            // `dispatch_terminal_reached`: a timeout release carries
            // `ReleaseOutcome::Failed`, which `dispatch_terminal_reached`
            // treats as terminal, so nesting this inside the
            // `!dispatch_terminal_reached` branch (as originally written)
            // made it unreachable — the timeout release always short-circuited
            // straight to `write_dispatch_completion_artifacts` below.
            //
            // Key on ANY timeout reason, not just the stall string:
            // `timed_out_run` also emits `max_run_duration_exceeded` and
            // `idle_timeout_exceeded` (both `ReleaseOutcome::Failed`), and the
            // persistent artifactor-rmux path disables stall entirely and uses
            // idle — so matching only `stall_timeout_exceeded` would silently
            // scrape those into a fabricated "done" report.
            if dispatch_release_reason(&envelopes)
                .as_deref()
                .is_some_and(is_timeout_release_reason)
            {
                tracing::warn!(
                    run_id = %completion.run_id,
                    reason = dispatch_release_reason(&envelopes).as_deref().unwrap_or(""),
                    "dispatch completion watcher: timeout with no worker finalize, flagging orphan"
                );
                record_dispatch_orphaned(&state, &completion).await;
                return;
            }
            if !dispatch_terminal_reached(&envelopes) {
                if grace_elapsed {
                    tracing::info!(
                        run_id = %completion.run_id,
                        "dispatch completion watcher writing artifacts after post-release grace without terminal session marker"
                    );
                    write_dispatch_completion_artifacts(&state, &completion, &envelopes).await;
                    return;
                }
                tokio::time::sleep(poll).await;
                continue;
            }
            write_dispatch_completion_artifacts(&state, &completion, &envelopes).await;
            return;
        }
    });
}

/// True for every `timed_out_run` release reason (supervisor.rs): the run was
/// killed by a deadline, not by the worker completing. All three are released
/// as `ReleaseOutcome::Failed`, so the completion watcher must treat any of
/// them (absent a worker finalize) as orphan rather than scrape a fake report.
fn is_timeout_release_reason(reason: &str) -> bool {
    matches!(
        reason,
        "stall_timeout_exceeded" | "max_run_duration_exceeded" | "idle_timeout_exceeded"
    )
}

/// Last `Lifecycle::Release` event's `reason`, if any — used to tell a timeout
/// release apart from every other release path.
fn dispatch_release_reason(envelopes: &[SessionEnvelope]) -> Option<String> {
    envelopes.iter().rev().find_map(|envelope| {
        if envelope.kind != SessionEventKind::Lifecycle {
            return None;
        }
        match serde_json::from_value::<Lifecycle>(envelope.event.clone()) {
            Ok(Lifecycle::Release { reason, .. }) => Some(reason),
            _ => None,
        }
    })
}

/// Whether the run's `Lifecycle::Release` event carries `finalized_by_worker`
/// (set by `orgasmic dispatch finalize`, dec_3M7M0).
fn dispatch_release_finalized_by_worker(envelopes: &[SessionEnvelope]) -> bool {
    envelopes.iter().rev().any(|envelope| {
        envelope.kind == SessionEventKind::Lifecycle
            && matches!(
                serde_json::from_value::<Lifecycle>(envelope.event.clone()),
                Ok(Lifecycle::Release {
                    finalized_by_worker: true,
                    ..
                })
            )
    })
}

/// Flag a dispatch run that stalled without the worker ever calling
/// `orgasmic dispatch finalize` (dec_3M7M0): distinct from a normal
/// completion tx so the manager can rescue it rather than mistake it for
/// silent success.
async fn record_dispatch_orphaned(state: &ApiState, completion: &DispatchCompletion) {
    let extra = vec![
        ("RUN_ID".to_string(), completion.run_id.clone()),
        (
            "WORKTREE".to_string(),
            completion.worktree_path.display().to_string(),
        ),
    ];
    if let Err(e) = record_api_tx(
        state,
        ApiTxRequest {
            ty: "manager.dispatch_orphaned".to_string(),
            actor: None,
            project: Some(completion.project_id.clone()),
            task: Some(completion.task_id.clone()),
            target: Some(tx_safe_path(&completion.session_path, None, &state.home)),
            reason:
                "worker never called `orgasmic dispatch finalize` before the stall window elapsed"
                    .to_string(),
            request_id: None,
            extra,
        },
    )
    .await
    {
        tracing::warn!(
            run_id = %completion.run_id,
            error = ?e,
            "manager.dispatch_orphaned tx failed"
        );
    }
}

async fn write_dispatch_completion_artifacts(
    state: &ApiState,
    completion: &DispatchCompletion,
    envelopes: &[SessionEnvelope],
) {
    // Belt-and-suspenders: the watcher loop already returns before calling
    // this function when the worker finalized, but callers that reach here
    // directly (e.g. the post-release grace path) must not overwrite an
    // authoritative worker-written report either (regression, dec_3M7M0).
    if dispatch_release_finalized_by_worker(envelopes) {
        tracing::info!(
            run_id = %completion.run_id,
            "dispatch completion artifact write skipped: worker finalized via `orgasmic dispatch finalize`"
        );
    } else {
        if let Err(e) = write_dispatch_last_path(&completion.last_path, envelopes) {
            tracing::warn!(
                run_id = %completion.run_id,
                path = %completion.last_path.display(),
                error = %e,
                "dispatch last_path write failed"
            );
        }
        if let Err(e) = write_dispatch_stdout_path(&completion.stdout_path, envelopes) {
            tracing::warn!(
                run_id = %completion.run_id,
                path = %completion.stdout_path.display(),
                error = %e,
                "dispatch stdout_path write failed"
            );
        }
    }
    if state.auto_commit_signal {
        let dirty = worktree_has_uncommitted_changes(&completion.worktree_path);
        if dirty {
            let extra = vec![
                ("RUN_ID".to_string(), completion.run_id.clone()),
                (
                    "WORKTREE".to_string(),
                    completion.worktree_path.display().to_string(),
                ),
                ("HAS_UNCOMMITTED".to_string(), "yes".to_string()),
            ];
            if let Err(e) = record_api_tx(
                state,
                ApiTxRequest {
                    ty: "implementer.commit_pending".to_string(),
                    actor: None,
                    project: Some(completion.project_id.clone()),
                    task: Some(completion.task_id.clone()),
                    target: Some(tx_safe_path(&completion.session_path, None, &state.home)),
                    reason: "implementer dispatch completed with uncommitted worktree".to_string(),
                    request_id: None,
                    extra,
                },
            )
            .await
            {
                tracing::warn!(
                    run_id = %completion.run_id,
                    error = ?e,
                    "implementer.commit_pending tx failed"
                );
            }
        }
    }
}

fn write_dispatch_last_path(
    last_path: &FsPath,
    envelopes: &[SessionEnvelope],
) -> std::io::Result<()> {
    let summary = dispatch_last_summary_from_session(envelopes);
    if summary.is_empty() {
        return Ok(());
    }
    if let Some(parent) = last_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(last_path, summary)
}

fn write_dispatch_stdout_path(
    stdout_path: &FsPath,
    envelopes: &[SessionEnvelope],
) -> std::io::Result<()> {
    let body = dispatch_stdout_from_session(envelopes);
    if body.is_empty() {
        return Ok(());
    }
    if let Some(parent) = stdout_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(stdout_path, body)
}

fn dispatch_stdout_from_session(envelopes: &[SessionEnvelope]) -> String {
    let mut text_lines = Vec::new();
    let mut json_lines = Vec::new();
    for envelope in envelopes {
        if envelope.kind != SessionEventKind::DriverEvent {
            continue;
        }
        if let Ok(line) = serde_json::to_string(&envelope.event) {
            json_lines.push(line);
        }
        if envelope.event.get("type").and_then(Value::as_str) != Some("text_chunk") {
            continue;
        }
        let Some(chunk) = envelope.event.get("chunk").and_then(Value::as_str) else {
            continue;
        };
        if chunk.is_empty() {
            continue;
        }
        let stream = envelope
            .event
            .get("stream")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        text_lines.push(format!("[{stream}] {chunk}"));
    }
    if !text_lines.is_empty() {
        return text_lines.join("\n");
    }
    json_lines.join("\n")
}

fn dispatch_terminal_reached(envelopes: &[SessionEnvelope]) -> bool {
    session_driver_terminal_event(envelopes).is_some()
        || envelopes.iter().rev().any(|envelope| {
            release_outcome(envelope)
                .map(|outcome| {
                    matches!(
                        outcome,
                        ReleaseOutcome::Completed
                            | ReleaseOutcome::Failed
                            | ReleaseOutcome::Cancelled
                    )
                })
                .unwrap_or(false)
        })
}

/// Bare status markers the codex ACP adapter fabricated before TASK-YHA6V
/// ("codex turn completed", "thread closed"). Sessions recorded by the old
/// adapter still carry them; they must never shadow the worker's real report.
fn is_generic_completion_marker(summary: &str) -> bool {
    summary == "thread closed"
        || summary
            .strip_prefix("codex turn ")
            .is_some_and(|status| !status.is_empty() && !status.contains(char::is_whitespace))
}

fn dispatch_last_summary_from_session(envelopes: &[SessionEnvelope]) -> String {
    let mut run_complete_summary = None;
    let mut marker_summary = None;
    let mut assistant_text = String::new();
    let mut system_text = String::new();
    let mut release_reason = None;
    for envelope in envelopes {
        if envelope.kind == SessionEventKind::Lifecycle {
            if release_reason.is_none() {
                if let Ok(Lifecycle::Release { reason, .. }) =
                    serde_json::from_value::<Lifecycle>(envelope.event.clone())
                {
                    let trimmed = reason.trim();
                    if !trimmed.is_empty() {
                        release_reason = Some(trimmed.to_string());
                    }
                }
            }
            continue;
        }
        if envelope.kind != SessionEventKind::DriverEvent {
            continue;
        }
        match envelope.event.get("type").and_then(Value::as_str) {
            Some("run_complete") if run_complete_summary.is_none() => {
                if let Some(summary) = envelope
                    .event
                    .get("summary")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                {
                    if is_generic_completion_marker(summary) {
                        if marker_summary.is_none() {
                            marker_summary = Some(summary.to_string());
                        }
                    } else {
                        run_complete_summary = Some(summary.to_string());
                    }
                }
            }
            Some("text_chunk") => {
                let Some(chunk) = envelope.event.get("chunk").and_then(Value::as_str) else {
                    continue;
                };
                if chunk.is_empty() {
                    continue;
                }
                let stream = envelope.event.get("stream").and_then(Value::as_str);
                let assistant = envelope.event.get("role").and_then(Value::as_str)
                    == Some("assistant")
                    || stream == Some("assistant");
                if assistant {
                    assistant_text.push_str(chunk);
                } else if stream == Some("system") {
                    system_text.push_str(chunk);
                }
            }
            _ => {}
        }
    }
    // rmux/tmux `release(reason)` synthesize a `RunComplete { summary: reason }`
    // when they tear down a run that never emitted its own terminal event — e.g.
    // the worker printed its report and the `[orgasmic-eot]` marker but the
    // render stream missed the marker, so the stall detector fired and released
    // the run (TASK-B05AM). That sentinel is not a worker report: when the
    // run_complete summary merely echoes the release reason, drop it so the
    // actual transcript tail is preserved in last.txt instead of being shadowed
    // by "stall_timeout_exceeded".
    let run_complete_echoes_release = matches!(
        (run_complete_summary.as_deref(), release_reason.as_deref()),
        (Some(rc), Some(reason)) if rc == reason
    );
    if run_complete_echoes_release {
        run_complete_summary = None;
    }
    let assistant_summary = tail_summary(&assistant_text);
    let system_summary = tail_summary(&system_text);
    run_complete_summary
        .or(assistant_summary)
        .or(system_summary)
        .or(release_reason)
        .or(marker_summary)
        .unwrap_or_default()
}

/// Streaming transcripts (notably rmux, which accumulates the whole rendered
/// scrollback here) can be large, and the worker's report is always at the
/// tail. Cap the fallback summary to its last [`DISPATCH_SUMMARY_TAIL_BYTES`] on
/// a char boundary so last.txt stays bounded while still carrying the report.
const DISPATCH_SUMMARY_TAIL_BYTES: usize = 32 * 1024;

fn tail_summary(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.len() <= DISPATCH_SUMMARY_TAIL_BYTES {
        return Some(trimmed.to_string());
    }
    let mut start = trimmed.len() - DISPATCH_SUMMARY_TAIL_BYTES;
    while !trimmed.is_char_boundary(start) {
        start += 1;
    }
    Some(format!(
        "[…transcript truncated to last {} KiB…]\n{}",
        DISPATCH_SUMMARY_TAIL_BYTES / 1024,
        &trimmed[start..]
    ))
}

fn worktree_has_uncommitted_changes(worktree: &FsPath) -> bool {
    Command::new("git")
        .args([
            "-C",
            &worktree.display().to_string(),
            "status",
            "--porcelain",
        ])
        .output()
        .map(|output| output.status.success() && !output.stdout.is_empty())
        .unwrap_or(false)
}

#[derive(Clone)]
struct StageCompletion {
    stage: String,
    project_id: String,
    task_id: String,
    target: String,
    run_id: String,
    session_path: PathBuf,
}

enum StageOutcome {
    Completed,
    Failed { reason: Option<String> },
}

fn spawn_stage_completion_watcher(state: ApiState, completion: StageCompletion) {
    tokio::spawn(async move {
        loop {
            let live = state
                .supervisor
                .snapshot()
                .await
                .runs
                .iter()
                .any(|run| run.run_id == completion.run_id);
            if live {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                continue;
            }
            let outcome = stage_outcome_from_session(&completion.session_path);
            let tx_type = match &outcome {
                StageOutcome::Completed => format!("{}.completed", completion.stage),
                StageOutcome::Failed { .. } => format!("{}.failed", completion.stage),
            };
            let reason = match &outcome {
                StageOutcome::Completed => format!("{} stage completed", completion.stage),
                StageOutcome::Failed { reason } => reason
                    .clone()
                    .unwrap_or_else(|| format!("{} stage failed", completion.stage)),
            };
            match record_api_tx(
                &state,
                ApiTxRequest {
                    ty: tx_type.clone(),
                    actor: None,
                    project: Some(completion.project_id.clone()),
                    task: Some(completion.task_id.clone()),
                    target: Some(completion.target.clone()),
                    reason,
                    request_id: None,
                    extra: vec![
                        ("RUN_ID".to_string(), completion.run_id.clone()),
                        (
                            "SESSION_PATH".to_string(),
                            completion.session_path.display().to_string(),
                        ),
                    ],
                },
            )
            .await
            {
                Ok(tx_id) => {
                    let payload = match &outcome {
                        StageOutcome::Completed => EventPayload::StageCompleted {
                            stage: completion.stage.clone(),
                            project_id: completion.project_id.clone(),
                            task_id: completion.task_id.clone(),
                            run_id: completion.run_id.clone(),
                            tx_id: tx_id.clone(),
                        },
                        StageOutcome::Failed { .. } => EventPayload::StageFailed {
                            stage: completion.stage.clone(),
                            project_id: completion.project_id.clone(),
                            task_id: completion.task_id.clone(),
                            run_id: completion.run_id.clone(),
                            tx_id: tx_id.clone(),
                        },
                    };
                    state.events.publish(Topic::Run, payload);
                    state.events.publish(
                        Topic::Manager,
                        EventPayload::ManagerNotice {
                            message: format!(
                                "{} for run {} recorded as {}",
                                tx_type, completion.run_id, tx_id
                            ),
                        },
                    );
                    state
                        .events
                        .publish(Topic::Board, EventPayload::BoardRefreshed);
                }
                Err(e) => {
                    tracing::warn!(
                        run_id = %completion.run_id,
                        stage = %completion.stage,
                        error = ?e,
                        "stage completion tx failed"
                    );
                }
            }
            break;
        }
    });
}

fn stage_outcome_from_session(session_path: &FsPath) -> StageOutcome {
    let envelopes = match read_session_file(session_path) {
        Ok(envelopes) => envelopes,
        Err(e) => {
            tracing::warn!(path = %session_path.display(), error = %e, "stage session read failed");
            return StageOutcome::Failed { reason: None };
        }
    };
    let mut completed = false;
    let mut driver_error_reason = None;
    for envelope in envelopes {
        match envelope.kind {
            SessionEventKind::DriverEvent => {
                match envelope.event.get("type").and_then(Value::as_str) {
                    Some("run_fail") => return StageOutcome::Failed { reason: None },
                    Some("driver_error") => {
                        if driver_error_reason.is_none() {
                            driver_error_reason = envelope
                                .event
                                .get("message")
                                .and_then(Value::as_str)
                                .filter(|value| !value.trim().is_empty())
                                .map(|message| format!("driver error: {message}"));
                        }
                        if envelope
                            .event
                            .get("fatal")
                            .and_then(Value::as_bool)
                            .unwrap_or(false)
                        {
                            return StageOutcome::Failed {
                                reason: Some(
                                    driver_error_reason
                                        .unwrap_or_else(|| "fatal driver error".to_string()),
                                ),
                            };
                        }
                    }
                    Some("run_complete") => completed = true,
                    _ => {}
                }
            }
            SessionEventKind::Lifecycle => {
                if envelope.event.get("phase").and_then(Value::as_str) == Some("release") {
                    match envelope.event.get("outcome").and_then(Value::as_str) {
                        Some("completed") => completed = true,
                        Some("failed") | Some("interrupted") | Some("cancelled") => {
                            return StageOutcome::Failed {
                                reason: driver_error_reason,
                            };
                        }
                        _ => {}
                    }
                }
            }
            SessionEventKind::BabysitterSummary | SessionEventKind::Note => {}
        }
    }
    if completed {
        StageOutcome::Completed
    } else {
        StageOutcome::Failed { reason: None }
    }
}

struct ApiTxRequest {
    ty: String,
    actor: Option<String>,
    project: Option<String>,
    task: Option<String>,
    target: Option<String>,
    reason: String,
    request_id: Option<String>,
    extra: Vec<(String, String)>,
}

async fn record_api_tx(state: &ApiState, req: ApiTxRequest) -> Result<String, ApiError> {
    let prepared = prepare_api_tx(state, req).await?;
    let res = state
        .writer
        .append_tx(prepared.tx, None)
        .await
        .map_err(writer_append_error)?;
    refresh_after_tx(state, prepared.project_tx, prepared.destination_project_id).await;
    Ok(res.tx_id)
}

struct PreparedApiTx {
    tx: TxAppend,
    project_tx: bool,
    destination_project_id: Option<String>,
}

async fn prepare_api_tx(state: &ApiState, req: ApiTxRequest) -> Result<PreparedApiTx, ApiError> {
    let now = Utc::now();
    let snap = state.index.snapshot().await;
    let project_entry = req
        .project
        .as_deref()
        .and_then(|id| snap.board.iter().find(|entry| entry.id == id))
        .cloned();
    let pseudo_req = TxAppendRequest {
        request_id: req.request_id.clone(),
        r#type: req.ty.clone(),
        actor: req.actor,
        machine: None,
        project: req.project,
        task: req.task,
        target: req.target,
        reason: Some(req.reason),
        extra: req.extra,
        tx_path: None,
    };
    let destination = tx_destination(state, &pseudo_req, project_entry.as_ref(), &now)?;
    let tx_id = match &destination.tx_id_policy {
        TxIdPolicy::Preserve => format!(
            "tx-{}-{}",
            tx_preserve_id_timestamp_utc(&now),
            &uuid::Uuid::new_v4().to_string()[..8]
        ),
        TxIdPolicy::ProjectSequence { .. } => "pending-project-sequence".to_string(),
    };
    let time_str = tx_time_string_utc(&now);
    let mut entry = TxEntry::new(
        tx_id,
        &req.ty,
        time_str,
        choose_actor(&pseudo_req, project_entry.as_ref(), state),
        state.machine.clone(),
    );
    entry.project = pseudo_req.project;
    entry.task = pseudo_req.task;
    entry.target = pseudo_req.target;
    entry.reason = pseudo_req.reason;
    entry.extra = pseudo_req.extra;
    Ok(PreparedApiTx {
        tx: TxAppend {
            tx_path: destination.tx_path,
            entry,
            project_id: destination.project_id.clone(),
            tx_id_policy: destination.tx_id_policy,
            request_id: req.request_id,
        },
        project_tx: destination.project_tx,
        destination_project_id: destination.project_id,
    })
}

async fn refresh_after_tx(
    state: &ApiState,
    project_tx: bool,
    destination_project_id: Option<String>,
) {
    if project_tx {
        if let Some(project_id) = destination_project_id {
            let _ = state.index.refresh_project(&project_id).await;
        }
    } else {
        state.index.refresh_home_tx().await;
    }
}

fn safe_session_fragment(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.' {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

struct TxDestination {
    tx_path: PathBuf,
    project_id: Option<String>,
    project_tx: bool,
    tx_id_policy: TxIdPolicy,
}

fn tx_destination(
    state: &ApiState,
    req: &TxAppendRequest,
    project_entry: Option<&BoardEntry>,
    now: &chrono::DateTime<Utc>,
) -> Result<TxDestination, ApiError> {
    if let Some(tx_path) = req.tx_path.clone() {
        return Ok(TxDestination {
            tx_path,
            project_id: req.project.clone(),
            project_tx: false,
            tx_id_policy: TxIdPolicy::Preserve,
        });
    }
    if state.tx_commit_to_project {
        if let Some(project_id) = req
            .project
            .as_deref()
            .map(str::trim)
            .filter(|project_id| !project_id.is_empty())
        {
            let Some(entry) = project_entry else {
                tracing::warn!(
                    project_id = %project_id,
                    "tx project not found on board"
                );
                return Err(ApiError::bad_request("project could not be resolved"));
            };
            if !valid_project_root(&entry.path) {
                tracing::warn!(
                    project_id = %project_id,
                    path = %entry.path.display(),
                    "tx project root is invalid"
                );
                return Err(ApiError::bad_request("project could not be resolved"));
            }
            let tx_path = entry
                .path
                .join(".orgasmic")
                .join("tx")
                .join(format!("{}.org", tx_file_month_utc(now)));
            return Ok(TxDestination {
                tx_path,
                project_id: Some(entry.id.clone()),
                project_tx: true,
                tx_id_policy: TxIdPolicy::ProjectSequence {
                    project_id: entry.id.clone(),
                    date: tx_project_date_utc(now),
                },
            });
        }
    }
    Ok(TxDestination {
        tx_path: state
            .default_tx_path
            .with_file_name(format!("{}.org", tx_file_month_utc(now))),
        project_id: req.project.clone(),
        project_tx: false,
        tx_id_policy: TxIdPolicy::Preserve,
    })
}

fn valid_project_root(path: &FsPath) -> bool {
    !path.as_os_str().is_empty() && path.join(".orgasmic").is_dir()
}

fn choose_actor(
    req: &TxAppendRequest,
    project_entry: Option<&BoardEntry>,
    state: &ApiState,
) -> String {
    req.actor
        .clone()
        .and_then(non_empty)
        .or_else(|| state.manager_actor.clone())
        .or_else(|| project_entry.and_then(|entry| git_user_email(&entry.path)))
        .unwrap_or_else(|| state.actor.clone())
}

fn non_empty(value: String) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn git_user_email(project_root: &FsPath) -> Option<String> {
    let output = Command::new("git")
        .args(["config", "user.email"])
        .current_dir(project_root)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let email = String::from_utf8(output.stdout).ok()?;
    non_empty(email)
}

#[derive(Debug, Serialize)]
pub struct StatusResponse {
    pub name: &'static str,
    pub version: String,
    pub runtime_version: String,
    pub boot_id: String,
    pub pid: u32,
    pub started_at: String,
    pub home: PathBuf,
    pub machine: String,
    pub bind_host: String,
    pub bind_port: u16,
    pub local_only: bool,
    pub ui_asset_hash: String,
    pub projects: usize,
    pub parse_errors: usize,
    pub tx_count: usize,
    pub rebuilt_at: Option<String>,
}

async fn get_status(State(state): State<ApiState>) -> Json<StatusResponse> {
    let snap: IndexSnapshot = state.index.snapshot().await;
    Json(StatusResponse {
        name: "orgasmic",
        version: state.boot.version.clone(),
        runtime_version: state.boot.version.clone(),
        boot_id: state.boot.boot_id.clone(),
        pid: state.boot.pid,
        started_at: state.boot.started_at.to_rfc3339(),
        home: state.index.home_root().await.to_path_buf(),
        machine: state.machine,
        local_only: state.bind_host == "127.0.0.1" || state.bind_host == "::1",
        bind_host: state.bind_host,
        bind_port: state.bind_port,
        ui_asset_hash: state.ui_asset_hash,
        projects: snap.projects.len(),
        parse_errors: snap.parse_errors.len(),
        tx_count: snap.tx.len(),
        rebuilt_at: snap.rebuilt_at.map(|t| t.to_rfc3339()),
    })
}

#[derive(Debug, Serialize)]
pub struct RecoveryResponse {
    pub boot_id: String,
    pub acquisition_paused: bool,
    pub live_runs: Vec<crate::supervisor::RunSummary>,
    pub interrupted_runs: Vec<RecoveredRun>,
    pub reattached_runs: Vec<RecoveredRun>,
    pub terminal_noop_runs: Vec<RecoveredRun>,
    pub ambiguous_runs: Vec<RecoveredRun>,
    pub note: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct RecoveredRun {
    pub run_id: String,
    pub runtime_id: String,
    pub boot_id: String,
    pub session_path: PathBuf,
    pub classification: String,
    pub reason: String,
    /// Backend-owned, ordered, harness-aware recovery actions (dec_052). The
    /// first entry is the preferred action; the UI renders its `label` and
    /// executes through `POST /runs/:id/recover` with `action.kind`.
    #[serde(default)]
    pub recovery_actions: Vec<RecoveryAction>,
}

/// A concrete recovery action the backend will take through `/recover`.
#[derive(Debug, Clone, Serialize)]
pub struct RecoveryAction {
    /// Stable machine identifier: `reattach_tmux`, `resume_native_fork`, or
    /// `start_recovery_run`.
    pub kind: String,
    /// Human-facing label rendered by the notification bell / Run Dock.
    pub label: String,
    /// Where recovery opens once executed: `manager` or `worker`.
    pub target: String,
}

/// Recovery surface a successful recover opens.
fn recovery_target_for(run_kind: Option<RunKind>, worker_id: &str) -> &'static str {
    if worker_id == "manager" || worker_id.starts_with("manager") {
        "manager"
    } else {
        match run_kind {
            Some(RunKind::Babysitter) => "worker",
            _ => "worker",
        }
    }
}

async fn get_recovery(State(state): State<ApiState>) -> Json<RecoveryResponse> {
    Json(recovery_status(&state).await)
}

async fn get_runs(State(state): State<ApiState>) -> Json<Value> {
    let recovery = recovery_status(&state).await;
    Json(json!({
        "live": recovery.live_runs,
        "interrupted": recovery.interrupted_runs,
        "reattached": recovery.reattached_runs,
        "terminal_noop": recovery.terminal_noop_runs,
        "ambiguous": recovery.ambiguous_runs,
    }))
}

async fn get_run(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let recovery = recovery_status(&state).await;
    if let Some(run) = recovery.live_runs.iter().find(|run| run.run_id == id) {
        let source = read_artifact(&run.session_path, "run session")?;
        return Ok(Json(json!({"source": source, "run": run})));
    }
    for (classification, runs) in [
        ("interrupted", &recovery.interrupted_runs),
        ("reattached", &recovery.reattached_runs),
        ("terminal_noop", &recovery.terminal_noop_runs),
        ("ambiguous", &recovery.ambiguous_runs),
    ] {
        if let Some(run) = runs.iter().find(|run| run.run_id == id) {
            let source = read_artifact(&run.session_path, "run session")?;
            return Ok(Json(
                json!({"classification": classification, "source": source, "run": run}),
            ));
        }
    }
    Err(ApiError::not_found(format!("run {id}")))
}

#[derive(Debug, Deserialize)]
pub struct RunReleaseRequest {
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub request_id: Option<String>,
    /// Set by `orgasmic dispatch finalize` (dec_3M7M0): the worker itself
    /// declared completion over this same authenticated call, so the
    /// dispatch completion watcher must not scrape/overwrite the report it
    /// already wrote verbatim. `false` for every other caller (manager
    /// dispatch-close, manual cancel, etc.).
    #[serde(default)]
    pub finalized_by_worker: bool,
    /// Set by `orgasmic dispatch finalize` (TASK-DWJVH, review #4): the
    /// run's own `RuntimeIdentity` as the worker resolved it, so the release
    /// can be rejected if a different run has since reclaimed this run_id or
    /// the identity is otherwise stale (same self-consistency guard
    /// `send_input`/`transition_state` already apply). `None` for every
    /// other caller — preserves today's unauthenticated release for the
    /// human manager path (dispatch-close, lease-release).
    #[serde(default)]
    pub caller_identity: Option<RuntimeIdentity>,
}

#[derive(Debug, Serialize)]
pub struct RunReleaseResponse {
    pub run_id: String,
    pub task_id: String,
    pub owner: TaskOwner,
}

#[derive(Debug, Deserialize)]
pub struct RunInputRequest {
    pub input: String,
}

#[derive(Debug, Serialize)]
pub struct RunInputResponse {
    pub run_id: String,
    pub accepted: bool,
    pub message: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct RunRuntimeOptionsResponse {
    pub run_id: String,
    pub accepted: bool,
    pub message: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct RunRuntimeOptionsCatalogResponse {
    pub run_id: String,
    pub catalog: RuntimeOptionsCatalog,
}

async fn post_run_input(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(req): Json<RunInputRequest>,
) -> Result<Json<RunInputResponse>, ApiError> {
    let input = req.input.trim().to_string();
    if input.is_empty() {
        return Err(ApiError::bad_request("run input must not be empty"));
    }
    let live = state.supervisor.snapshot().await;
    let run = live
        .runs
        .iter()
        .find(|run| run.run_id == id)
        .cloned()
        .ok_or_else(|| ApiError::not_found(format!("active run {id}")))?;
    let ack = state
        .supervisor
        .send_input(&id, input, &run.identity)
        .await
        .map_err(|e| match e {
            crate::supervisor::SupervisorError::RunNotFound(_) => {
                ApiError::not_found(format!("active run {id}"))
            }
            other => supervisor_control_error("run input", other),
        })?;
    Ok(Json(RunInputResponse {
        run_id: id,
        accepted: ack.accepted,
        message: ack.message,
    }))
}

async fn post_run_runtime_options(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(req): Json<RuntimeOptionsRequest>,
) -> Result<Json<RunRuntimeOptionsResponse>, ApiError> {
    let req = req.normalized().map_err(ApiError::bad_request)?;
    if req.is_empty() {
        return Err(ApiError::bad_request(
            "runtime options request must not be empty",
        ));
    }
    let live = state.supervisor.snapshot().await;
    let run = live
        .runs
        .iter()
        .find(|run| run.run_id == id)
        .cloned()
        .ok_or_else(|| ApiError::not_found(format!("active run {id}")))?;
    let ack = state
        .supervisor
        .switch_runtime_options(&id, req, &run.identity)
        .await
        .map_err(|e| match e {
            crate::supervisor::SupervisorError::RunNotFound(_) => {
                ApiError::not_found(format!("active run {id}"))
            }
            other => supervisor_control_error("runtime options", other),
        })?;
    Ok(Json(RunRuntimeOptionsResponse {
        run_id: id,
        accepted: ack.accepted,
        message: ack.message,
    }))
}

async fn get_run_runtime_options(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<Json<RunRuntimeOptionsCatalogResponse>, ApiError> {
    let live = state.supervisor.snapshot().await;
    let run = live
        .runs
        .iter()
        .find(|run| run.run_id == id)
        .cloned()
        .ok_or_else(|| ApiError::not_found(format!("active run {id}")))?;
    let catalog = state
        .supervisor
        .runtime_options_catalog(&id, &run.identity)
        .await
        .map_err(|e| match e {
            crate::supervisor::SupervisorError::RunNotFound(_) => {
                ApiError::not_found(format!("active run {id}"))
            }
            other => supervisor_control_error("runtime options catalog", other),
        })?;
    Ok(Json(RunRuntimeOptionsCatalogResponse {
        run_id: id,
        catalog,
    }))
}

async fn post_run_release(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(req): Json<RunReleaseRequest>,
) -> Result<Json<RunReleaseResponse>, ApiError> {
    let live = state.supervisor.snapshot().await;
    let run = live
        .runs
        .iter()
        .find(|run| run.run_id == id)
        .cloned()
        .ok_or_else(|| ApiError::not_found(format!("active run {id}")))?;
    let reason = req.reason.unwrap_or_else(|| "run released".to_string());
    state
        .supervisor
        .release_with_finalization(
            &id,
            &reason,
            ReleaseOutcome::Cancelled,
            req.finalized_by_worker,
            req.caller_identity.as_ref(),
        )
        .await
        .map_err(|e| match e {
            crate::supervisor::SupervisorError::RunNotFound(_) => {
                ApiError::not_found(format!("active run {id}"))
            }
            crate::supervisor::SupervisorError::OwnershipMismatch {
                run_id,
                field,
                expected,
                got,
            } => ApiError::conflict_json(json!({
                "error": "runtime ownership mismatch",
                "run_id": run_id,
                "field": field,
                "expected": expected,
                "got": got,
            })),
            other => supervisor_release_error(&id, other),
        })?;
    Ok(Json(RunReleaseResponse {
        run_id: id,
        task_id: run.task_id,
        owner: TaskOwner::Human,
    }))
}

#[derive(Debug, Deserialize)]
pub struct RunRecoverRequest {
    /// Explicit recovery action: `reattach_tmux`, `resume_native_fork`, or
    /// `start_recovery_run`. When omitted, the backend executes the run's sole
    /// valid action or returns a 409 conflict listing the choices.
    #[serde(default)]
    pub action: Option<String>,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub request_id: Option<String>,
    #[serde(default)]
    pub force_inert: Option<bool>,
}

#[derive(Debug, Serialize)]
pub struct RunRecoverResponse {
    pub run_id: String,
    pub runtime_id: String,
    pub boot_id: String,
    pub session_path: PathBuf,
    /// Machine identifier of the executed action.
    pub action: String,
    /// UI surface to open: `manager` or `worker`.
    pub target: String,
    /// Staged recovery prompt for the operator to send manually. `None` for
    /// reattach, which leaves the composer empty.
    pub draft_prompt: Option<String>,
}

/// Resolve the harness driver for a recovered run from its session JSONL.
fn recovery_driver(
    home: &Home,
    envelopes: &[SessionEnvelope],
) -> Option<(Box<dyn WorkerDriver>, SessionAcquireMeta)> {
    let meta = session_acquire_meta(envelopes)?;
    let (driver, _id) = session_driver(home, envelopes, &meta)?;
    Some((driver, meta))
}

/// Best-effort harness name for a recorded worker id (e.g. `claude` for
/// `implementer-claude-tmux`). Returns `None` when the worker cannot be loaded.
fn recovery_harness_for_worker(home: &Home, worker_id: &str) -> Option<String> {
    load_stage_worker(home, worker_id)
        .ok()
        .map(|worker| worker.harness)
}

/// Mechanical recovery prompt (dec_052): prior run id, absolute prior session
/// JSONL path, a mechanical summary from the JSONL, the current diff stat, and
/// inspect-before-acting instructions. No model-generated summary.
fn build_recovery_prompt(
    prior_run_id: &str,
    prior_session_path: &std::path::Path,
    envelopes: &[SessionEnvelope],
    diff_stat: &str,
) -> String {
    let mut driver_events = 0usize;
    let mut tool_calls = 0usize;
    let mut last_assistant = String::new();
    for env in envelopes {
        if env.kind == SessionEventKind::DriverEvent {
            let ty = env.event.get("type").and_then(Value::as_str);
            // Heartbeats are liveness pings, not work; counting them inflates the
            // "driver events recorded" figure on long quiet runs.
            if ty == Some("heartbeat") {
                continue;
            }
            driver_events += 1;
            if let Some(ty) = ty {
                if ty == "tool_call" {
                    tool_calls += 1;
                }
                if ty == "text_chunk" {
                    if let Some(chunk) = env.event.get("chunk").and_then(Value::as_str) {
                        last_assistant = chunk.to_string();
                    }
                }
            }
        }
    }
    let last_excerpt: String = last_assistant.chars().take(280).collect();
    let diff_block = if diff_stat.trim().is_empty() {
        "(no uncommitted changes)".to_string()
    } else {
        diff_stat.trim().to_string()
    };
    format!(
        "You are recovering an interrupted orgasmic run. Inspect before acting; do not assume the prior run finished cleanly.\n\n\
Prior run id: {prior_run_id}\n\
Prior session JSONL (absolute): {prior_path}\n\n\
Mechanical summary (no model interpretation):\n\
- driver events recorded: {driver_events}\n\
- tool calls recorded: {tool_calls}\n\
- last assistant text excerpt: {last_excerpt}\n\n\
Current worktree diff stat:\n{diff_block}\n\n\
Instructions:\n\
1. Read the prior session JSONL at the absolute path above.\n\
2. Review the worktree diff before making any change.\n\
3. Only then continue the work; re-verify gates before declaring done.\n\
This prompt is staged in the composer and has NOT been sent. Send it yourself when ready.",
        prior_run_id = prior_run_id,
        prior_path = prior_session_path.display(),
        driver_events = driver_events,
        tool_calls = tool_calls,
        last_excerpt = last_excerpt,
        diff_block = diff_block,
    )
}

async fn post_run_recover(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(req): Json<RunRecoverRequest>,
) -> Result<Json<RunRecoverResponse>, ApiError> {
    let recovery = recovery_status(&state).await;
    let prior = recovery
        .interrupted_runs
        .iter()
        .chain(recovery.reattached_runs.iter())
        .chain(recovery.ambiguous_runs.iter())
        .find(|run| run.run_id == id)
        .cloned()
        .ok_or_else(|| ApiError::not_found(format!("recoverable run {id}")))?;

    // Resolve the explicit action, or auto-pick when exactly one exists.
    let action = match req.action.clone() {
        Some(a) => {
            if !prior.recovery_actions.iter().any(|act| act.kind == a) {
                return Err(ApiError::bad_request(format!(
                    "recovery action {a} is not valid for run {id}"
                )));
            }
            a
        }
        None => match prior.recovery_actions.as_slice() {
            [single] => single.kind.clone(),
            [] => {
                return Err(ApiError::not_found(format!(
                    "no recovery actions available for run {id}"
                )))
            }
            _ => {
                return Err(ApiError::conflict_json(json!({
                    "error": "multiple recovery actions; specify action",
                    "run_id": id,
                    "recovery_actions": prior.recovery_actions,
                })))
            }
        },
    };
    let target = prior
        .recovery_actions
        .iter()
        .find(|act| act.kind == action)
        .map(|act| act.target.clone())
        .unwrap_or_else(|| "worker".to_string());

    let envelopes = read_session_file(&prior.session_path).unwrap_or_default();
    let meta = session_acquire_meta(&envelopes);
    let worker_id = meta
        .as_ref()
        .map(|m| m.worker_id.clone())
        .unwrap_or_else(|| "manager".to_string());
    let run_kind = meta.as_ref().map(|m| m.kind).unwrap_or(RunKind::Worker);
    let worktree = Some(state.home.source());

    match action.as_str() {
        "reattach_tmux" => {
            let (driver, _meta) = recovery_driver(&state.home, &envelopes).ok_or_else(|| {
                ApiError::internal("recovered run has no recoverable driver identity")
            })?;
            let identity = RuntimeIdentity {
                run_id: prior.run_id.clone(),
                runtime_id: prior.runtime_id.clone(),
                boot_id: prior.boot_id.clone(),
            };
            let acquire = state
                .supervisor
                .reattach(
                    driver.as_ref(),
                    identity,
                    run_kind,
                    meta.as_ref()
                        .map(|m| m.task_id.clone())
                        .unwrap_or_else(|| format!("recover:{id}")),
                    worker_id.clone(),
                    resolve_run_role(&state.home, &worker_id, run_kind),
                    req.project.clone(),
                    worktree,
                    prior.session_path.clone(),
                    DriverConfig::from_value(json!({
                        "force_inert": req.force_inert.unwrap_or(false),
                    })),
                )
                .await
                .map_err(supervisor_recover_error)?;
            // Reattach attaches as-is and leaves the composer empty (dec_052).
            Ok(Json(RunRecoverResponse {
                run_id: acquire.run_id,
                runtime_id: acquire.identity.runtime_id,
                boot_id: acquire.identity.boot_id,
                session_path: prior.session_path,
                action,
                target,
                draft_prompt: None,
            }))
        }
        "resume_native_fork" | "start_recovery_run" => {
            let native = session_native_runtime(&envelopes);
            let diff_stat = crate::supervisor::GitDiffSummarizer
                .summarize(worktree.as_deref())
                .to_string();
            let prompt = build_recovery_prompt(&id, &prior.session_path, &envelopes, &diff_stat);

            let now = Utc::now();
            // Write the recovery transcript next to the run it recovers so it
            // lands in the same per-project sessions dir and is rediscovered by
            // the per-project boot scan; fall back to home only if the prior
            // path has no parent.
            let recover_name = format!("recover-{}.jsonl", now.format("%Y%m%dT%H%M%S"));
            let session_path = prior
                .session_path
                .parent()
                .map(|dir| dir.join(&recover_name))
                .unwrap_or_else(|| state.home.sessions().join(&recover_name));

            // start_recovery_run uses the original harness; resume_native_fork
            // resumes the same native Claude session via the recorded argv.
            let (driver, driver_config) = if action == "resume_native_fork" {
                let native = native.ok_or_else(|| {
                    ApiError::bad_request("resume_native_fork requires recorded native metadata")
                })?;
                let resume_argv = native.resume_argv.clone();
                if resume_argv.len() < 2 {
                    return Err(ApiError::bad_request(
                        "recorded native metadata has no resume argv",
                    ));
                }
                let command = resume_argv[0].clone();
                let args: Vec<String> = resume_argv[1..].to_vec();
                let driver = driver_for_mode_harness("tmux", "claude").ok_or_else(|| {
                    ApiError::internal("tmux/claude driver unavailable for native resume")
                })?;
                let cfg = DriverConfig::from_value(json!({
                    "command": command,
                    "args": args,
                    "harness": "claude",
                    "cwd": state.home.source(),
                    "force_inert": req.force_inert.unwrap_or(false),
                    // Idle launch: do NOT auto-send the recovery prompt.
                }));
                (driver, cfg)
            } else {
                // start_recovery_run launches a REAL tmux session for the
                // original harness (dec_052) — never the placeholder shell and
                // never the deferred acp driver. The harness comes from native
                // metadata or the recorded worker; default to claude.
                let harness = native
                    .as_ref()
                    .map(|n| n.provider.clone())
                    .or_else(|| {
                        meta.as_ref()
                            .and_then(|m| recovery_harness_for_worker(&state.home, &m.worker_id))
                    })
                    .unwrap_or_else(|| "claude".to_string());
                let driver = driver_for_mode_harness("tmux", &harness)
                    .or_else(|| driver_for_mode_harness("tmux", "claude"))
                    .ok_or_else(|| {
                        ApiError::internal("tmux driver unavailable for recovery run")
                    })?;
                let cfg = DriverConfig::from_value(json!({
                    "harness": harness,
                    "cwd": state.home.source(),
                    "force_inert": req.force_inert.unwrap_or(false),
                }));
                (driver, cfg)
            };

            let acquire = state
                .supervisor
                .acquire(
                    driver.as_ref(),
                    AcquireRequest {
                        task_id: meta
                            .as_ref()
                            .map(|m| m.task_id.clone())
                            .unwrap_or_else(|| format!("recover:{id}")),
                        kind: run_kind,
                        role: resolve_run_role(&state.home, &worker_id, run_kind),
                        worker_id,
                        project_id: req.project.clone(),
                        worktree,
                        last_path: None,
                        stdout_path: None,
                        session_path: session_path.clone(),
                        driver_config,
                        babysitter_target: None,
                        stall_timeout_secs: None,
                        max_run_duration_secs: None,
                        idle_timeout_secs: None,
                        babysitter: None,
                    },
                )
                .await
                .map_err(supervisor_recover_error)?;

            // Persist the staged recovery prompt draft (pending send).
            state
                .supervisor
                .append_prompt_draft(&acquire.run_id, &session_path, &acquire.identity, &prompt)
                .await
                .map_err(supervisor_recover_error)?;

            Ok(Json(RunRecoverResponse {
                run_id: acquire.run_id,
                runtime_id: acquire.identity.runtime_id,
                boot_id: acquire.identity.boot_id,
                session_path,
                action,
                target,
                draft_prompt: Some(prompt),
            }))
        }
        other => Err(ApiError::bad_request(format!(
            "unknown recovery action {other}"
        ))),
    }
}

async fn recovery_status(state: &ApiState) -> RecoveryResponse {
    let live = state.supervisor.snapshot().await;
    let live_ids: BTreeSet<_> = live.runs.iter().map(|run| run.run_id.as_str()).collect();
    let project_roots: Vec<PathBuf> = state
        .index
        .snapshot()
        .await
        .board
        .iter()
        .map(|entry| entry.path.clone())
        .collect();
    let recovered =
        classify_session_files(&state.home, &state.boot.boot_id, &live.runs, &project_roots).await;
    let mut interrupted_runs = Vec::new();
    let mut reattached_runs = Vec::new();
    let mut terminal_noop_runs = Vec::new();
    let mut ambiguous_runs = Vec::new();
    for run in recovered {
        if live_ids.contains(run.run_id.as_str()) {
            continue;
        }
        match run.classification.as_str() {
            "interrupted" => interrupted_runs.push(run),
            "reattached" => reattached_runs.push(run),
            "terminal_noop" => terminal_noop_runs.push(run),
            _ => ambiguous_runs.push(run),
        }
    }
    RecoveryResponse {
        boot_id: state.boot.boot_id.clone(),
        acquisition_paused: live.acquisition_paused,
        live_runs: live.runs,
        interrupted_runs,
        reattached_runs,
        terminal_noop_runs,
        ambiguous_runs,
        note: "boot reconciliation classifies durable session JSONL without restoring snapshots or mutating graph state",
    }
}

/// Reattach material reconstructed from a non-terminal run's session JSONL.
struct BootReattachCandidate {
    run_id: String,
    runtime_id: String,
    boot_id: String,
    task_id: String,
    kind: RunKind,
    worker_id: String,
    transport: String,
    harness: Option<String>,
    project_id: Option<String>,
    worktree: Option<PathBuf>,
    /// Dispatch artifact paths, when the reattached run was a CLI dispatch —
    /// enables respawning its completion watcher (TASK-567JG).
    last_path: Option<PathBuf>,
    stdout_path: Option<PathBuf>,
    driver_config: serde_json::Value,
    session_path: PathBuf,
}

/// `(transport, harness, project_id, worktree, last_path, stdout_path,
/// driver_config)` from a `RunMeta` lifecycle event.
type RunMetaFields = (
    String,
    Option<String>,
    Option<String>,
    Option<PathBuf>,
    Option<PathBuf>,
    Option<PathBuf>,
    serde_json::Value,
);

/// Extract a [`BootReattachCandidate`] from a session JSONL if the run is
/// non-terminal and carries both an `Acquire` and a `RunMeta` lifecycle event.
/// Returns `None` for terminal runs, babysitters (derived, not reattached), or
/// runs missing the metadata (pre-`RunMeta` sessions).
fn boot_reattach_candidate(
    envelopes: &[SessionEnvelope],
    session_path: &FsPath,
) -> Option<BootReattachCandidate> {
    let first = envelopes.first()?;
    let last = envelopes.last().unwrap_or(first);
    // Skip terminal runs (mirrors the recovery classifier).
    let terminal = match release_outcome(last) {
        Some(ReleaseOutcome::Completed)
        | Some(ReleaseOutcome::Failed)
        | Some(ReleaseOutcome::Cancelled) => true,
        Some(ReleaseOutcome::Interrupted) | None => {
            session_driver_terminal_event(envelopes).is_some()
        }
    };
    if terminal {
        return None;
    }

    let mut acquire: Option<(String, String, String)> = None;
    let mut meta: Option<RunMetaFields> = None;
    for env in envelopes {
        if env.kind != SessionEventKind::Lifecycle {
            continue;
        }
        match serde_json::from_value::<Lifecycle>(env.event.clone()) {
            Ok(Lifecycle::Acquire {
                task_id,
                kind,
                worker_id,
            }) => acquire = Some((task_id, kind, worker_id)),
            Ok(Lifecycle::RunMeta {
                transport,
                harness,
                project_id,
                worktree,
                last_path,
                stdout_path,
                driver_config,
            }) => {
                meta = Some((
                    transport,
                    harness,
                    project_id,
                    worktree,
                    last_path,
                    stdout_path,
                    driver_config,
                ))
            }
            _ => {}
        }
    }

    let (task_id, kind_str, worker_id) = acquire?;
    let (transport, harness, project_id, worktree, last_path, stdout_path, driver_config) = meta?;
    let kind = match kind_str.as_str() {
        // Babysitters watch another run; they are re-derived, not reattached.
        "babysitter" => return None,
        _ => RunKind::Worker,
    };

    Some(BootReattachCandidate {
        run_id: first.run_id.clone(),
        runtime_id: first.runtime_id.clone(),
        boot_id: first.boot_id.clone(),
        task_id,
        kind,
        worker_id,
        transport,
        harness,
        project_id,
        worktree,
        last_path,
        stdout_path,
        driver_config,
        session_path: session_path.to_path_buf(),
    })
}

/// Boot auto-reattach: rehydrate still-live runs into the freshly built
/// supervisor so a daemon restart/rebuild is transparent to the operator. For
/// each project's session JSONL that is non-terminal and carries reattach
/// metadata, build the recorded driver and call [`Supervisor::reattach`] — the
/// driver's `attach()` proves the mux session is still live (otherwise the run
/// is skipped, not interrupted). Manager runs (`manager.launch:` task ids) are
/// reattached first so the operator's terminal reconnects promptly.
///
/// A reattached run that carries dispatch artifact paths (`last_path` /
/// `stdout_path`, set only for CLI dispatch runs) gets its dispatch
/// completion watcher respawned (TASK-567JG): that watcher is a tokio task
/// spawned only at dispatch time, so it dies with the old daemon process on a
/// mid-run restart, and without a respawn `last.txt`/`stdout.log` are never
/// written on release. Runs missing either path (manager, recovery, stage
/// launch, or pre-upgrade session JSONL) are reattached with no watcher, same
/// as before this fix.
pub async fn reattach_live_runs_on_boot(state: &ApiState, project_roots: &[PathBuf]) {
    let home = &state.home;
    let supervisor = &state.supervisor;
    let mut candidates: Vec<BootReattachCandidate> = Vec::new();
    let mut seen_dirs: std::collections::BTreeSet<PathBuf> = std::collections::BTreeSet::new();
    for root in project_roots {
        let dir = project_sessions_dir(root);
        if !seen_dirs.insert(dir.clone()) {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let Ok(envelopes) = read_session_file(&path) else {
                continue;
            };
            if let Some(candidate) = boot_reattach_candidate(&envelopes, &path) {
                candidates.push(candidate);
            }
        }
    }
    // Managers first (false sorts before true).
    candidates.sort_by_key(|c| !c.task_id.starts_with("manager.launch:"));

    let (mut reattached, mut skipped) = (0usize, 0usize);
    for c in candidates {
        let Some(driver) =
            driver_for_mode_harness(&c.transport, c.harness.as_deref().unwrap_or_default())
        else {
            skipped += 1;
            tracing::info!(
                run_id = %c.run_id,
                transport = %c.transport,
                "boot reattach: no driver for recorded transport/harness; skipped"
            );
            continue;
        };
        let identity = RuntimeIdentity {
            run_id: c.run_id.clone(),
            runtime_id: c.runtime_id.clone(),
            boot_id: c.boot_id.clone(),
        };
        match supervisor
            .reattach(
                driver.as_ref(),
                identity,
                c.kind,
                c.task_id.clone(),
                c.worker_id.clone(),
                resolve_run_role(home, &c.worker_id, c.kind),
                c.project_id.clone(),
                c.worktree.clone(),
                c.session_path.clone(),
                DriverConfig::from_value(c.driver_config.clone()),
            )
            .await
        {
            Ok(_) => {
                reattached += 1;
                tracing::info!(
                    run_id = %c.run_id,
                    task_id = %c.task_id,
                    "boot reattach: rehydrated live run"
                );
                match (
                    c.last_path.clone(),
                    c.stdout_path.clone(),
                    c.project_id.clone(),
                    c.worktree.clone(),
                ) {
                    (Some(last_path), Some(stdout_path), Some(project_id), Some(worktree_path)) => {
                        tracing::info!(
                            run_id = %c.run_id,
                            task_id = %c.task_id,
                            "boot reattach: respawning dispatch completion watcher"
                        );
                        // Backfill RunRecord so `orgasmic dispatch finalize`
                        // can resolve this reattached run's artifact paths
                        // via `/runs`, same as a freshly acquired dispatch.
                        supervisor
                            .set_dispatch_artifact_paths(
                                &c.run_id,
                                last_path.clone(),
                                stdout_path.clone(),
                            )
                            .await;
                        spawn_dispatch_completion_watcher(
                            state.clone(),
                            DispatchCompletion {
                                project_id,
                                task_id: c.task_id.clone(),
                                run_id: c.run_id.clone(),
                                session_path: c.session_path.clone(),
                                last_path,
                                stdout_path,
                                worktree_path,
                            },
                        );
                    }
                    (None, None, _, _) => {
                        // Non-dispatch run (manager, recovery, stage launch) or
                        // pre-upgrade RunMeta: no watcher, as before this fix.
                    }
                    _ => {
                        tracing::warn!(
                            run_id = %c.run_id,
                            task_id = %c.task_id,
                            "boot reattach: dispatch artifact paths present without project_id/worktree; skipping completion watcher"
                        );
                    }
                }
            }
            Err(e) => {
                skipped += 1;
                tracing::info!(
                    run_id = %c.run_id,
                    task_id = %c.task_id,
                    error = %e,
                    "boot reattach: skipped (mux session not live / not reattachable)"
                );
            }
        }
    }
    if reattached > 0 || skipped > 0 {
        tracing::info!(reattached, skipped, "boot auto-reattach pass complete");
    }
}

async fn classify_session_files(
    home: &Home,
    _current_boot_id: &str,
    live_runs: &[crate::supervisor::RunSummary],
    project_roots: &[PathBuf],
) -> Vec<RecoveredRun> {
    let live_ids: std::collections::BTreeSet<_> =
        live_runs.iter().map(|run| run.run_id.as_str()).collect();
    let mut runs = Vec::new();
    // Per-run transcripts live per project under `.orgasmic/tmp/sessions/`
    // (dec: per-project tmp). Enumerate each registered project's sessions dir;
    // de-dup roots so a project listed twice on the board isn't scanned twice.
    let mut seen_dirs: std::collections::BTreeSet<PathBuf> = std::collections::BTreeSet::new();
    for root in project_roots {
        let dir = project_sessions_dir(root);
        if !seen_dirs.insert(dir.clone()) {
            continue;
        }
        classify_session_dir(home, &dir, &live_ids, &mut runs).await;
    }
    runs.sort_by(|a, b| a.run_id.cmp(&b.run_id));
    runs
}

/// Classify every session JSONL in a single directory, appending to `runs`.
/// Shared by the per-project boot scan; keys everything off the file path and
/// parsed envelopes, so it is location-agnostic.
async fn classify_session_dir(
    home: &Home,
    dir: &FsPath,
    live_ids: &std::collections::BTreeSet<&str>,
    runs: &mut Vec<RecoveredRun>,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
            continue;
        }
        let Ok(envelopes) = read_session_file(&path) else {
            runs.push(RecoveredRun {
                run_id: path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("unknown")
                    .to_string(),
                runtime_id: String::new(),
                boot_id: String::new(),
                session_path: path,
                classification: "ambiguous".to_string(),
                reason: "session JSONL could not be parsed".to_string(),
                recovery_actions: Vec::new(),
            });
            continue;
        };
        let Some(first) = envelopes.first() else {
            continue;
        };
        let last = envelopes.last().unwrap_or(first);
        let terminal = if let Some(outcome) = release_outcome(last) {
            match outcome {
                ReleaseOutcome::Completed | ReleaseOutcome::Failed | ReleaseOutcome::Cancelled => {
                    Some("session ended with terminal release".to_string())
                }
                ReleaseOutcome::Interrupted => None,
            }
        } else if session_driver_terminal_event(&envelopes).is_some() {
            Some("session ended with terminal driver event".to_string())
        } else {
            None
        };

        let (classification, reason, recovery_actions) = if let Some(reason) = terminal {
            ("terminal_noop".to_string(), reason, Vec::new())
        } else {
            // Non-terminal: prove liveness (live tmux from any boot reattaches
            // before being marked interrupted, dec_052) and compute the
            // ordered, harness-aware recovery action list.
            let attach = classify_current_boot_session(home, &envelopes, first, live_ids).await;
            let actions = resolve_recovery_actions(home, &envelopes, attach.classification).await;
            (attach.classification.to_string(), attach.reason, actions)
        };
        runs.push(RecoveredRun {
            run_id: first.run_id.clone(),
            runtime_id: first.runtime_id.clone(),
            boot_id: first.boot_id.clone(),
            session_path: path,
            classification,
            reason,
            recovery_actions,
        });
    }
}

/// One-shot, idempotent, non-fatal relocation of legacy home-level session
/// transcripts (`$ORGASMIC_HOME/sessions/*.jsonl`) into their project's
/// per-project `.orgasmic/tmp/sessions/` dir. Runs at boot before recovery is
/// served.
///
/// Mapping: a `manager-<project_id>-…jsonl` file goes to the board project with
/// that id; any other file goes to the sole board project when there is exactly
/// one (covers the common single-project case, incl. stage/dispatch/recover);
/// otherwise it is left in place with a warning (a handful of unmappable
/// historical files simply won't appear in per-project recovery). Never deletes
/// on failure; skips when the destination already exists.
pub(crate) fn migrate_legacy_home_sessions(home: &Home, projects: &[(String, PathBuf)]) {
    let legacy_dir = home.sessions();
    let Ok(entries) = std::fs::read_dir(&legacy_dir) else {
        return; // no legacy dir → nothing to migrate
    };
    let by_id: std::collections::HashMap<&str, &PathBuf> = projects
        .iter()
        .map(|(id, root)| (id.as_str(), root))
        .collect();
    let sole_root = if projects.len() == 1 {
        Some(&projects[0].1)
    } else {
        None
    };
    let (mut moved, mut left) = (0u32, 0u32);
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let Some(file_name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        let target_root = file_name
            .strip_prefix("manager-")
            .and_then(|rest| {
                // `manager-<project_id>-<timestamp>.jsonl`: the id is everything
                // before the final `-<timestamp>` segment.
                rest.rsplit_once('-').map(|(id, _)| id)
            })
            .and_then(|id| by_id.get(id).copied())
            .or(sole_root);
        let Some(root) = target_root else {
            tracing::warn!(session = %file_name, "legacy session could not be mapped to a project; leaving in place");
            left += 1;
            continue;
        };
        let dest_dir = project_sessions_dir(root);
        let dest = dest_dir.join(file_name);
        if dest.exists() {
            continue; // already migrated (idempotent)
        }
        if let Err(e) = std::fs::create_dir_all(&dest_dir) {
            tracing::warn!(session = %file_name, error = %e, "create per-project sessions dir failed");
            left += 1;
            continue;
        }
        if let Err(e) = std::fs::rename(&path, &dest) {
            tracing::warn!(session = %file_name, error = %e, "move legacy session failed; leaving in place");
            left += 1;
            continue;
        }
        moved += 1;
    }
    if moved > 0 || left > 0 {
        tracing::info!(
            moved,
            left,
            "migrated legacy home session transcripts to per-project tmp"
        );
    }
}

struct AttachClassification {
    classification: &'static str,
    reason: String,
}

#[derive(Debug, Clone)]
struct SessionAcquireMeta {
    task_id: String,
    kind: RunKind,
    worker_id: String,
}

async fn classify_current_boot_session(
    home: &Home,
    envelopes: &[SessionEnvelope],
    first: &SessionEnvelope,
    live_ids: &std::collections::BTreeSet<&str>,
) -> AttachClassification {
    if live_ids.contains(first.run_id.as_str()) {
        return AttachClassification {
            classification: "reattached",
            reason: "supervisor-live run; attach proof skipped for active lease".to_string(),
        };
    }
    let Some(meta) = session_acquire_meta(envelopes) else {
        return AttachClassification {
            classification: "ambiguous",
            reason: "current-boot session has no acquire lifecycle metadata".to_string(),
        };
    };
    let Some((driver, driver_id)) = session_driver(home, envelopes, &meta) else {
        return AttachClassification {
            classification: "ambiguous",
            reason: "current-boot session has no recoverable driver identity".to_string(),
        };
    };
    let identity = RuntimeIdentity {
        run_id: first.run_id.clone(),
        runtime_id: first.runtime_id.clone(),
        boot_id: first.boot_id.clone(),
    };
    let ctx = DriverContext {
        identity,
        run_kind: meta.kind,
        task_id: meta.task_id,
        worker_id: meta.worker_id,
        project_id: None,
        worktree: None,
        babysitter_target: None,
        continuation: None,
    };
    match driver.attach(ctx, DriverConfig::empty()).await {
        Ok(AttachOutcome::Attached(_attached)) => AttachClassification {
            classification: "reattached",
            reason: if live_ids.contains(first.run_id.as_str()) {
                format!("driver {driver_id} proved live runtime handle for supervisor-live run")
            } else {
                format!("driver {driver_id} proved live runtime handle")
            },
        },
        Ok(AttachOutcome::NotReattachable) => AttachClassification {
            classification: "interrupted",
            reason: format!("driver {driver_id} could not prove a live runtime handle"),
        },
        Err(e) => AttachClassification {
            classification: "interrupted",
            reason: format!("driver {driver_id} attach proof failed: {e}"),
        },
    }
}

fn session_acquire_meta(envelopes: &[SessionEnvelope]) -> Option<SessionAcquireMeta> {
    envelopes.iter().find_map(|envelope| {
        if envelope.kind != SessionEventKind::Lifecycle {
            return None;
        }
        match serde_json::from_value::<Lifecycle>(envelope.event.clone()).ok()? {
            Lifecycle::Acquire {
                task_id,
                kind,
                worker_id,
            } => Some(SessionAcquireMeta {
                task_id,
                kind: run_kind_from_lifecycle(&kind)?,
                worker_id,
            }),
            _ => None,
        }
    })
}

/// Native runtime identity recorded into the session JSONL at launch time.
#[derive(Debug, Clone)]
struct SessionNativeRuntime {
    provider: String,
    session_id: Option<String>,
    resume_argv: Vec<String>,
}

fn session_native_runtime(envelopes: &[SessionEnvelope]) -> Option<SessionNativeRuntime> {
    envelopes.iter().rev().find_map(|envelope| {
        if envelope.kind != SessionEventKind::Lifecycle {
            return None;
        }
        match serde_json::from_value::<Lifecycle>(envelope.event.clone()).ok()? {
            Lifecycle::NativeRuntime {
                provider,
                session_id,
                resume_argv,
                ..
            } => Some(SessionNativeRuntime {
                provider,
                session_id,
                resume_argv,
            }),
            _ => None,
        }
    })
}

/// Backend-owned, ordered, harness-aware recovery action resolver (dec_052).
/// Priority: live tmux reattach, native session resume/fork, then a fresh
/// recovery run. `classification` is the attach-proof result for this run.
async fn resolve_recovery_actions(
    _home: &Home,
    envelopes: &[SessionEnvelope],
    classification: &str,
) -> Vec<RecoveryAction> {
    let meta = session_acquire_meta(envelopes);
    let worker_id = meta.as_ref().map(|m| m.worker_id.as_str()).unwrap_or("");
    let run_kind = meta.as_ref().map(|m| m.kind);
    let target = recovery_target_for(run_kind, worker_id).to_string();

    let mut actions = Vec::new();

    // 1. Live tmux reattach proven by the classifier.
    if classification == "reattached" {
        actions.push(RecoveryAction {
            kind: "reattach_tmux".to_string(),
            label: "Reattach".to_string(),
            target: target.clone(),
        });
    }

    // 2. Native session resume/fork, only when recorded metadata supports it.
    if let Some(native) = session_native_runtime(envelopes) {
        if native.provider == "claude"
            && native.session_id.is_some()
            && !native.resume_argv.is_empty()
        {
            actions.push(RecoveryAction {
                kind: "resume_native_fork".to_string(),
                label: "Resume Claude (fork)".to_string(),
                target: target.clone(),
            });
        }
    }

    // 3. Fresh recovery run is always available as the last resort. No
    //    ~/.claude/projects best-effort discovery for legacy runs (dec_052).
    actions.push(RecoveryAction {
        kind: "start_recovery_run".to_string(),
        label: "Start Recovery Run".to_string(),
        target,
    });

    actions
}

fn run_kind_from_lifecycle(kind: &str) -> Option<RunKind> {
    match kind {
        // "implementer" is the pre-rename spelling still present in old
        // persisted session JSONLs.
        "worker" | "implementer" => Some(RunKind::Worker),
        "babysitter" => Some(RunKind::Babysitter),
        _ => None,
    }
}

fn session_driver(
    home: &Home,
    envelopes: &[SessionEnvelope],
    meta: &SessionAcquireMeta,
) -> Option<(Box<dyn WorkerDriver>, String)> {
    if let Ok(worker) = load_stage_worker(home, &meta.worker_id) {
        let driver = driver_for_mode_harness(&worker.driver, &worker.harness)?;
        let driver_id = format!("{}/{}", worker.driver, worker.harness);
        return Some((driver, driver_id));
    }
    let driver_id = session_driver_id_from_ready(envelopes)?;
    driver_for(&driver_id).map(|driver| (driver, driver_id))
}

fn session_driver_id_from_ready(envelopes: &[SessionEnvelope]) -> Option<String> {
    envelopes.iter().find_map(|envelope| {
        if envelope.kind != SessionEventKind::DriverEvent {
            return None;
        }
        let DriverEvent::Ready {
            protocol_version, ..
        } = serde_json::from_value::<DriverEvent>(envelope.event.clone()).ok()?
        else {
            return None;
        };
        driver_id_from_protocol(&protocol_version).map(str::to_string)
    })
}

fn driver_id_from_protocol(protocol_version: &str) -> Option<&'static str> {
    if protocol_version.starts_with("tmux-tui/") {
        Some("tmux-tui")
    } else if protocol_version.starts_with("codex-appserver/") {
        Some("codex-appserver")
    } else if protocol_version.starts_with("hermes/") {
        Some("hermes")
    } else if protocol_version.starts_with("acp/")
        || protocol_version.starts_with("claude-code-stream-json/")
    {
        Some("claude-acp")
    } else {
        None
    }
}

fn session_driver_terminal_event(envelopes: &[SessionEnvelope]) -> Option<&'static str> {
    for envelope in envelopes.iter().rev() {
        if envelope.kind != SessionEventKind::DriverEvent {
            continue;
        }
        match envelope.event.get("type").and_then(Value::as_str) {
            Some("run_complete") => return Some("run_complete"),
            Some("run_fail") | Some("run_error") => return Some("run_fail"),
            _ => {}
        }
    }
    None
}

fn release_outcome(envelope: &orgasmic_core::SessionEnvelope) -> Option<ReleaseOutcome> {
    if envelope.kind != SessionEventKind::Lifecycle {
        return None;
    }
    let lifecycle: Lifecycle = serde_json::from_value(envelope.event.clone()).ok()?;
    match lifecycle {
        Lifecycle::Release { outcome, .. } => Some(outcome),
        _ => None,
    }
}

#[derive(Debug, Deserialize)]
pub struct RestartRequest {
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub request_id: Option<String>,
    /// Retained for old clients. Controlled restart is recovery-aware and no
    /// longer refuses live manager runs; stale leases are still cleared through
    /// the lease-release endpoint, not by restart.
    #[serde(default)]
    pub force: bool,
}

#[derive(Debug, Serialize)]
pub struct RestartResponse {
    pub status: String,
    pub boot_id: String,
    pub acquisition_paused: bool,
    pub warnings: Vec<RestartWarning>,
}

#[derive(Debug, Serialize)]
pub struct RestartWarning {
    pub kind: String,
    pub pending_writes: usize,
    pub message: String,
}

const RESTART_WRITER_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

async fn post_daemon_restart(
    State(state): State<ApiState>,
    Json(_req): Json<RestartRequest>,
) -> Result<Json<RestartResponse>, ApiError> {
    let live_managers: Vec<String> = state
        .supervisor
        .snapshot()
        .await
        .runs
        .iter()
        .filter(|run| crate::supervisor::is_interactive_manager_task(&run.task_id))
        .map(|run| run.run_id.clone())
        .collect();
    state.supervisor.pause_acquisition().await;
    let mut warnings = Vec::new();
    if !live_managers.is_empty() {
        warnings.push(RestartWarning {
            kind: "live_manager_recovery".to_string(),
            pending_writes: 0,
            message: format!(
                "live manager run(s) {} will be reattached on next boot when the driver can prove the runtime is still live; otherwise they remain visible through run recovery",
                live_managers.join(", ")
            ),
        });
    }
    warnings.extend(drain_writer_before_restart(&state).await?);
    Ok(Json(RestartResponse {
        status: "restart_requested".to_string(),
        boot_id: state.boot.boot_id.clone(),
        acquisition_paused: state.supervisor.snapshot().await.acquisition_paused,
        warnings,
    }))
}

async fn drain_writer_before_restart(state: &ApiState) -> Result<Vec<RestartWarning>, ApiError> {
    let marker = state
        .home
        .state()
        .join("writer-drain")
        .join(format!("restart-{}.marker", uuid::Uuid::new_v4().simple()));
    let request_id = format!("daemon-restart-drain/{}", uuid::Uuid::new_v4());
    let writer = state.writer.clone();
    let marker_for_task = marker.clone();
    let mut drain = tokio::spawn(async move {
        let result = writer
            .rewrite_file(
                FileRewrite {
                    path: marker_for_task.clone(),
                    new_contents: b"restart drain barrier\n".to_vec(),
                },
                Some(request_id),
            )
            .await;
        if result.is_ok() {
            let _ = tokio::fs::remove_file(&marker_for_task).await;
        }
        result
    });

    match tokio::time::timeout(RESTART_WRITER_DRAIN_TIMEOUT, &mut drain).await {
        Ok(Ok(Ok(()))) => Ok(Vec::new()),
        Ok(Ok(Err(e))) => Err(writer_drain_error("rewrite", e)),
        Ok(Err(e)) => Err(writer_drain_error("task", e)),
        Err(_) => {
            tokio::spawn(async move {
                let _ = drain.await;
            });
            Ok(vec![RestartWarning {
                kind: "writer_drain_timeout".to_string(),
                pending_writes: 1,
                message: "writer drain timed out; pending_writes is a proven lower bound"
                    .to_string(),
            }])
        }
    }
}

async fn get_whoami(State(state): State<ApiState>) -> Json<Value> {
    Json(json!({
        "authenticated": true,
        "boot_id": state.boot.boot_id,
    }))
}

/// `/graph/parse-errors` view: the raw `ParseError` plus the project it was
/// attributed to (TASK-V8WY9), so a caller never has to grep the daemon log
/// to learn which project owns a dangling reference or parse failure.
#[derive(Debug, Serialize)]
struct ParseErrorView {
    #[serde(skip_serializing_if = "Option::is_none")]
    project_id: Option<String>,
    path: PathBuf,
    kind: crate::index::ParseErrorKind,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    line: Option<usize>,
    at: chrono::DateTime<Utc>,
}

fn parse_error_views(snap: &IndexSnapshot) -> Vec<ParseErrorView> {
    snap.parse_errors
        .iter()
        .map(|error| ParseErrorView {
            project_id: snap.parse_error_project_id(error).map(str::to_string),
            path: error.path.clone(),
            kind: error.kind.clone(),
            message: error.message.clone(),
            line: error.line,
            at: error.at,
        })
        .collect()
}

async fn get_parse_errors(State(state): State<ApiState>) -> Json<Vec<ParseErrorView>> {
    let snap = state.index.snapshot().await;
    Json(parse_error_views(&snap))
}

/// `orgasmic reindex [--project <id>]` response (TASK-V8WY9): fresh
/// per-project parse-error counts after forcing a rebuild, so a battle-tester
/// can confirm a fix without a daemon restart.
#[derive(Debug, Serialize)]
struct ReindexResponse {
    projects: BTreeMap<String, usize>,
    total_parse_errors: usize,
}

fn reindex_response(snap: &IndexSnapshot) -> ReindexResponse {
    ReindexResponse {
        projects: snap.parse_error_counts_by_project(),
        total_parse_errors: snap.parse_errors.len(),
    }
}

async fn post_reindex(State(state): State<ApiState>) -> Json<ReindexResponse> {
    state.index.rebuild().await;
    let snap = state.index.snapshot().await;
    Json(reindex_response(&snap))
}

async fn post_reindex_project(
    State(state): State<ApiState>,
    Path(project_id): Path<String>,
) -> Result<Json<ReindexResponse>, ApiError> {
    state
        .index
        .refresh_project(&project_id)
        .await
        .map_err(ApiError::not_found)?;
    let snap = state.index.snapshot().await;
    Ok(Json(reindex_response(&snap)))
}

#[derive(Debug, Deserialize)]
pub struct OrgFileQuery {
    #[serde(default)]
    pub project: Option<String>,
    pub path: String,
}

#[derive(Debug, Deserialize)]
pub struct OrgFileWriteRequest {
    #[serde(default)]
    pub project: Option<String>,
    pub path: String,
    pub contents: String,
    #[serde(default)]
    pub request_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct OrgFileResponse {
    pub project: String,
    pub path: String,
    pub contents: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tx_id: Option<String>,
}

async fn get_org_file(
    State(state): State<ApiState>,
    Query(q): Query<OrgFileQuery>,
) -> Result<Json<OrgFileResponse>, ApiError> {
    let rel = validate_org_edit_path(&q.path)?;
    let rel_str = rel.to_string_lossy().to_string();
    let snap = state.index.snapshot().await;
    let project = select_project(&snap, q.project.as_deref())?;
    let path = project.root.join(&rel);
    let label = org_file_artifact_label(&rel);
    let contents = read_artifact(&path, label)?;
    Ok(Json(OrgFileResponse {
        project: project.project_id.clone(),
        path: rel_str,
        contents,
        tx_id: None,
    }))
}

async fn post_org_file(
    State(state): State<ApiState>,
    Json(req): Json<OrgFileWriteRequest>,
) -> Result<Json<OrgFileResponse>, ApiError> {
    let rel = validate_org_edit_path(&req.path)?;
    let rel_str = rel.to_string_lossy().to_string();
    OrgFile::parse(req.contents.clone(), rel_str.clone())
        .map_err(|e| org_input_parse_error(FsPath::new(&rel_str), e))?;
    let snap = state.index.snapshot().await;
    let project = select_project(&snap, req.project.as_deref())?;
    let project_id = project.project_id.clone();
    let path = project.root.join(&rel);
    drop(snap);
    state
        .writer
        .rewrite_file(
            FileRewrite {
                path: path.clone(),
                new_contents: req.contents.clone().into_bytes(),
            },
            req.request_id.as_ref().map(|id| format!("{id}/rewrite")),
        )
        .await
        .map_err(|e| writer_rewrite_error(&path, e))?;
    let tx_id = record_api_tx(
        &state,
        ApiTxRequest {
            ty: "org.file_rewritten".to_string(),
            actor: None,
            project: Some(project_id.clone()),
            task: None,
            target: Some(rel_str.clone()),
            reason: "org file rewrite requested".to_string(),
            request_id: req.request_id.map(|id| format!("{id}/tx")),
            extra: Vec::new(),
        },
    )
    .await?;
    let _ = state.index.refresh_project(&project_id).await;
    Ok(Json(OrgFileResponse {
        project: project_id,
        path: rel_str,
        contents: req.contents,
        tx_id: Some(tx_id),
    }))
}

fn validate_org_edit_path(path: &str) -> Result<PathBuf, ApiError> {
    let rel = PathBuf::from(path);
    if rel.is_absolute()
        || !rel
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
    {
        return Err(ApiError::bad_request("org file path must be relative"));
    }
    if rel.extension().and_then(|ext| ext.to_str()) != Some("org") {
        return Err(ApiError::bad_request("org file path must end in .org"));
    }
    if !(rel.starts_with(".orgasmic") || rel.starts_with("docs/adr")) {
        return Err(ApiError::bad_request(
            "org file path must be under .orgasmic/ or docs/adr/",
        ));
    }
    Ok(rel)
}

fn org_file_artifact_label(relative_path: &FsPath) -> &'static str {
    match relative_path.file_name().and_then(|s| s.to_str()) {
        Some("decisions.org") => "decisions file",
        Some("architecture.org") => "architecture file",
        Some("glossary.org") => "glossary file",
        Some(name) if relative_path.starts_with("docs/adr") && name.ends_with(".org") => "ADR file",
        Some(name) if name.starts_with("adr-") && name.ends_with(".org") => "ADR file",
        _ => "org file",
    }
}

async fn get_graph_nodes(
    State(state): State<ApiState>,
    Extension(identity): Extension<Identity>,
    Query(q): Query<GraphQuery>,
) -> Result<Json<Vec<crate::index::GraphNodeSummary>>, ApiError> {
    let snap = state.index.snapshot().await;
    let project = select_project(&snap, q.project.as_deref())?;
    authz::require(&identity, Some(&project.project_id), Action::GraphRead)?;
    Ok(Json(project.graph.nodes.clone()))
}

async fn get_graph_edges(
    State(state): State<ApiState>,
    Extension(identity): Extension<Identity>,
    Query(q): Query<GraphEdgesQuery>,
) -> Result<Json<Vec<crate::index::GraphEdgeSummary>>, ApiError> {
    let snap = state.index.snapshot().await;
    let project = select_project(&snap, q.project.as_deref())?;
    authz::require(&identity, Some(&project.project_id), Action::GraphRead)?;
    let graph = &project.graph;
    let (relation_kind, dir_from_relation) = graph_relation_filter(q.relation.as_deref())?;
    // A relation alias already fixes both kind and direction; reject a
    // conflicting explicit kind/dir rather than silently overriding the
    // alias's intent (VJXXC reviewer NIT). A redundant matching value is fine.
    if q.relation.is_some() {
        if let (Some(explicit), Some(want)) = (q.kind.as_deref(), relation_kind) {
            if explicit != want {
                return Err(ApiError::bad_request(format!(
                    "relation implies kind {want}; drop --kind or make it match"
                )));
            }
        }
        if let (Some(explicit), Some(want)) = (q.dir.as_deref(), dir_from_relation) {
            if explicit != want {
                return Err(ApiError::bad_request(format!(
                    "relation implies dir {want}; drop --dir or make it match"
                )));
            }
        }
    }
    let kind = q.kind.as_deref().or(relation_kind);
    let dir = q.dir.as_deref().or(dir_from_relation).unwrap_or("out");
    if dir != "out" && dir != "in" && dir != "both" {
        return Err(ApiError::bad_request(
            "graph edge dir must be one of out, in, both",
        ));
    }
    let edges = graph
        .edges
        .iter()
        .filter(|edge| kind.is_none_or(|want| edge.kind == want))
        .filter(|edge| {
            q.node.as_deref().is_none_or(|node| match dir {
                "out" => edge.from == node,
                "in" => edge.to == node,
                "both" => edge.from == node || edge.to == node,
                _ => false,
            })
        })
        .cloned()
        .collect();
    Ok(Json(edges))
}

fn graph_relation_filter(
    relation: Option<&str>,
) -> Result<(Option<&'static str>, Option<&'static str>), ApiError> {
    let Some(relation) = relation else {
        return Ok((None, None));
    };
    match relation {
        "depends_on" | "blocked_by" => Ok((Some("depends_on"), Some("out"))),
        "blocks" => Ok((Some("depends_on"), Some("in"))),
        "implements" => Ok((Some("implements"), Some("out"))),
        "implemented_by" => Ok((Some("implements"), Some("in"))),
        "produces" => Ok((Some("produces"), Some("out"))),
        "produced_by" => Ok((Some("produces"), Some("in"))),
        "motivated_by" => Ok((Some("motivated_by"), Some("out"))),
        "motivates" => Ok((Some("motivated_by"), Some("in"))),
        other => Err(ApiError::bad_request(format!(
            "unknown graph edge relation {other:?}"
        ))),
    }
}

#[derive(Debug, Serialize)]
pub struct MarkerFilesResponse {
    pub node_id: String,
    pub files: Vec<PathBuf>,
}

async fn get_graph_markers(
    State(state): State<ApiState>,
    Path(node_id): Path<String>,
) -> Result<Json<MarkerFilesResponse>, ApiError> {
    let snap = state.index.snapshot().await;
    Ok(Json(MarkerFilesResponse {
        files: snap.marker_files(&node_id),
        node_id,
    }))
}

async fn get_decisions(
    State(state): State<ApiState>,
    Extension(identity): Extension<Identity>,
    Query(q): Query<GraphQuery>,
) -> Result<Json<Vec<crate::index::DecisionSummary>>, ApiError> {
    let snap = state.index.snapshot().await;
    let project = select_project(&snap, q.project.as_deref())?;
    authz::require(&identity, Some(&project.project_id), Action::GraphRead)?;
    Ok(Json(project.graph.decisions.clone()))
}

async fn get_decision(
    State(state): State<ApiState>,
    Extension(identity): Extension<Identity>,
    Path(id): Path<String>,
    Query(q): Query<GraphQuery>,
) -> Result<Json<crate::index::DecisionSummary>, ApiError> {
    let snap = state.index.snapshot().await;
    let project = select_project(&snap, q.project.as_deref())?;
    authz::require(&identity, Some(&project.project_id), Action::GraphRead)?;
    project
        .graph
        .decisions
        .iter()
        .find(|node| node.id == id)
        .cloned()
        .map(Json)
        .ok_or_else(|| ApiError::not_found(format!("decision {id}")))
}

async fn get_architecture(
    State(state): State<ApiState>,
    Extension(identity): Extension<Identity>,
    Query(q): Query<GraphQuery>,
) -> Result<Json<Vec<crate::index::ArchitectureSummary>>, ApiError> {
    let snap = state.index.snapshot().await;
    let project = select_project(&snap, q.project.as_deref())?;
    authz::require(&identity, Some(&project.project_id), Action::GraphRead)?;
    Ok(Json(project.graph.architecture.clone()))
}

async fn get_architecture_nodes(
    State(state): State<ApiState>,
    Extension(identity): Extension<Identity>,
    Query(q): Query<GraphQuery>,
) -> Result<Json<crate::index::ArchitectureNodesResponse>, ApiError> {
    let snap = state.index.snapshot().await;
    let project = select_project(&snap, q.project.as_deref())?;
    authz::require(&identity, Some(&project.project_id), Action::GraphRead)?;
    Ok(Json(project.graph.architecture_nodes_response()))
}

async fn get_architecture_node(
    State(state): State<ApiState>,
    Extension(identity): Extension<Identity>,
    Path(id): Path<String>,
    Query(q): Query<GraphQuery>,
) -> Result<Json<crate::index::ArchitectureSummary>, ApiError> {
    let snap = state.index.snapshot().await;
    let project = select_project(&snap, q.project.as_deref())?;
    authz::require(&identity, Some(&project.project_id), Action::GraphRead)?;
    project
        .graph
        .architecture
        .iter()
        .find(|node| node.id == id)
        .cloned()
        .map(Json)
        .ok_or_else(|| ApiError::not_found(format!("architecture node {id}")))
}

async fn get_glossary(
    State(state): State<ApiState>,
    Extension(identity): Extension<Identity>,
    Query(q): Query<GraphQuery>,
) -> Result<Json<Vec<crate::index::GlossarySummary>>, ApiError> {
    let snap = state.index.snapshot().await;
    let project = select_project(&snap, q.project.as_deref())?;
    authz::require(&identity, Some(&project.project_id), Action::GraphRead)?;
    Ok(Json(project.graph.glossary.clone()))
}

async fn get_glossary_term(
    State(state): State<ApiState>,
    Extension(identity): Extension<Identity>,
    Path(id): Path<String>,
    Query(q): Query<GraphQuery>,
) -> Result<Json<crate::index::GlossarySummary>, ApiError> {
    let snap = state.index.snapshot().await;
    let project = select_project(&snap, q.project.as_deref())?;
    authz::require(&identity, Some(&project.project_id), Action::GraphRead)?;
    project
        .graph
        .glossary
        .iter()
        .find(|term| term.id == id)
        .cloned()
        .map(Json)
        .ok_or_else(|| ApiError::not_found(format!("glossary term {id}")))
}

#[derive(Debug, Deserialize)]
pub struct GraphCreateRequest {
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub request_id: Option<String>,
    /// Omitted or empty → daemon mints a short-random id for the layer.
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub properties: BTreeMap<String, String>,
    #[serde(default)]
    pub body: Option<String>,
    /// Skip the write-time reference-token check (dec_4T80X) for intentional
    /// forward references. The index-time dangling lint still catches it.
    #[serde(default)]
    pub force: bool,
    /// Skip the glossary implementation-detail marker guard
    /// (`reject_glossary_implementation_detail`) for a glossary term that
    /// legitimately defines a language construct (e.g. a "struct" or "trait"
    /// entry in a language glossary).
    #[serde(default)]
    pub allow_marker: bool,
}

#[derive(Debug, Deserialize)]
pub struct GraphActionRequest {
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub request_id: Option<String>,
    #[serde(default)]
    pub action: Option<String>,
    #[serde(default)]
    pub properties: BTreeMap<String, String>,
    #[serde(default)]
    pub reason: Option<String>,
    /// Skip the write-time reference-token check (dec_4T80X / TASK-KSD6D)
    /// for intentional forward references. The index-time dangling lint
    /// still catches it.
    #[serde(default)]
    pub force: bool,
}

#[derive(Debug, Serialize)]
pub struct GraphMutationResponse {
    pub id: String,
    pub action: String,
    pub tx_id: String,
}

/// Default output shape for mutations that would otherwise echo an entire
/// ~1.5KB node/task body back on every state flip or property edit
/// (F12, TASK-MTB56): just what changed, keyed by field name, plus the tx
/// that recorded it. Pass `?json=true` to get the full node/`TaskDetail`
/// back instead.
#[derive(Debug, Serialize)]
pub struct CompactMutationResponse {
    pub id: String,
    pub changed: BTreeMap<String, String>,
    pub tx_id: String,
}

/// Query flag shared by mutation endpoints that default to
/// [`CompactMutationResponse`]: `?json=true` restores the full node/TaskDetail.
#[derive(Debug, Deserialize)]
pub struct MutationOutputQuery {
    #[serde(default)]
    pub json: bool,
}

fn compact_or_full_response<T: Serialize>(
    want_full: bool,
    compact: CompactMutationResponse,
    full: T,
) -> Result<Json<serde_json::Value>, ApiError> {
    if want_full {
        Ok(Json(
            serde_json::to_value(full).map_err(|e| ApiError::internal(e.to_string()))?,
        ))
    } else {
        Ok(Json(
            serde_json::to_value(compact).map_err(|e| ApiError::internal(e.to_string()))?,
        ))
    }
}

async fn post_decision_create(
    State(state): State<ApiState>,
    Json(req): Json<GraphCreateRequest>,
) -> Result<Json<GraphMutationResponse>, ApiError> {
    create_graph_heading(&state, GraphLayer::Decision, req).await
}

async fn post_architecture_create(
    State(state): State<ApiState>,
    Json(req): Json<GraphCreateRequest>,
) -> Result<Json<GraphMutationResponse>, ApiError> {
    create_graph_heading(&state, GraphLayer::Architecture, req).await
}

async fn post_glossary_create(
    State(state): State<ApiState>,
    Json(req): Json<GraphCreateRequest>,
) -> Result<Json<GraphMutationResponse>, ApiError> {
    create_graph_heading(&state, GraphLayer::Glossary, req).await
}

async fn post_decision_action(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(req): Json<GraphActionRequest>,
) -> Result<Json<GraphMutationResponse>, ApiError> {
    mutate_graph_heading(&state, GraphLayer::Decision, id, req).await
}

async fn post_architecture_action(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(req): Json<GraphActionRequest>,
) -> Result<Json<GraphMutationResponse>, ApiError> {
    mutate_graph_heading(&state, GraphLayer::Architecture, id, req).await
}

async fn post_glossary_action(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(req): Json<GraphActionRequest>,
) -> Result<Json<GraphMutationResponse>, ApiError> {
    mutate_graph_heading(&state, GraphLayer::Glossary, id, req).await
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GraphLayer {
    Decision,
    Architecture,
    Glossary,
}

impl GraphLayer {
    fn file_name(self) -> &'static str {
        match self {
            Self::Decision => "decisions.org",
            Self::Architecture => "architecture.org",
            Self::Glossary => "glossary.org",
        }
    }

    fn layer_name(self) -> &'static str {
        match self {
            Self::Decision => "decision",
            Self::Architecture => "architecture",
            Self::Glossary => "glossary",
        }
    }

    fn artifact_name(self) -> &'static str {
        match self {
            Self::Decision => "decisions file",
            Self::Architecture => "architecture file",
            Self::Glossary => "glossary file",
        }
    }

    fn title_prefix(self) -> &'static str {
        match self {
            Self::Decision => "",
            Self::Architecture => "",
            Self::Glossary => "term:",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NodeLayer {
    Decision,
    Architecture,
    Glossary,
    Project,
    Task,
    Goal,
    Handoff,
    Config,
}

/// Drawer properties owned by the config layer. Setting one of these under any
/// other `--kind` used to silently write to the wrong file (e.g. `project.org`
/// instead of `config.org`, since both scaffold the same `:ID:`); rejected in
/// [`reject_misrouted_config_key`].
const CONFIG_RESERVED_KEYS: &[&str] = &["TEST_CMD", "LINT_CMD", "BUILD_CMD", "PIPELINE"];

impl NodeLayer {
    fn file_name(self) -> Option<&'static str> {
        match self {
            Self::Decision => Some("decisions.org"),
            Self::Architecture => Some("architecture.org"),
            Self::Glossary => Some("glossary.org"),
            Self::Project => Some("project.org"),
            Self::Config => Some("config.org"),
            Self::Task => None,
            Self::Goal | Self::Handoff => None,
        }
    }

    fn layer_name(self) -> &'static str {
        orgasmic_core::NodeKind::from(self).as_str()
    }

    fn artifact_name(self) -> &'static str {
        match self {
            Self::Decision => "decisions file",
            Self::Architecture => "architecture file",
            Self::Glossary => "glossary file",
            Self::Project => "project file",
            Self::Config => "config file",
            Self::Task => "task file",
            Self::Goal => "goal file",
            Self::Handoff => "handoff file",
        }
    }

    /// Infer the org-node layer that owns a node id: `TASK-*` -> task,
    /// `dec_*` -> decisions, `arch_*` -> architecture, `handoff-current` ->
    /// handoff, `goal-*` -> goal, anything else -> glossary. The project and
    /// config layers have no distinctive id prefix (and share the same
    /// `:ID:`), so they can only be selected explicitly via
    /// [`NodeLayer::from_kind`].
    fn for_id(id: &str) -> Self {
        if id.starts_with("TASK-") {
            Self::Task
        } else if id.starts_with("dec_") {
            Self::Decision
        } else if id.starts_with("arch_") {
            Self::Architecture
        } else if id == "handoff-current" {
            Self::Handoff
        } else if id.starts_with("goal-") {
            Self::Goal
        } else {
            Self::Glossary
        }
    }

    /// Resolve an explicit `kind` selector back to a layer. Used when the node
    /// id alone cannot identify the owning file. The accepted set is the
    /// single-sourced [`orgasmic_core::NodeKind`] registry so the CLI's
    /// `--kind` enum can be parity-tested against it.
    fn from_kind(kind: &str) -> Option<Self> {
        orgasmic_core::NodeKind::parse(kind).map(NodeLayer::from)
    }
}

impl From<orgasmic_core::NodeKind> for NodeLayer {
    fn from(kind: orgasmic_core::NodeKind) -> Self {
        match kind {
            orgasmic_core::NodeKind::Decision => Self::Decision,
            orgasmic_core::NodeKind::Architecture => Self::Architecture,
            orgasmic_core::NodeKind::Glossary => Self::Glossary,
            orgasmic_core::NodeKind::Project => Self::Project,
            orgasmic_core::NodeKind::Task => Self::Task,
            orgasmic_core::NodeKind::Goal => Self::Goal,
            orgasmic_core::NodeKind::Handoff => Self::Handoff,
            orgasmic_core::NodeKind::Config => Self::Config,
        }
    }
}

impl From<NodeLayer> for orgasmic_core::NodeKind {
    fn from(layer: NodeLayer) -> Self {
        match layer {
            NodeLayer::Decision => Self::Decision,
            NodeLayer::Architecture => Self::Architecture,
            NodeLayer::Glossary => Self::Glossary,
            NodeLayer::Project => Self::Project,
            NodeLayer::Task => Self::Task,
            NodeLayer::Goal => Self::Goal,
            NodeLayer::Handoff => Self::Handoff,
            NodeLayer::Config => Self::Config,
        }
    }
}

/// Kinds this daemon accepts for `--kind` on `node prop/body get/set`. Exposed
/// so the CLI can parity-test its `--kind` enum against the daemon's actual
/// acceptance set instead of duplicating the list by hand (TASK-JJ9RD).
pub fn accepted_node_kinds() -> &'static [orgasmic_core::NodeKind] {
    &orgasmic_core::NodeKind::ALL
}

/// Reject a `set_property`/`SetProperty` op that would write a CONFIG-owned
/// key to a non-config layer — the bug that made `node prop set <project-id>
/// TEST_CMD ... --kind project` silently succeed against `project.org` instead
/// of `config.org` (both share the project's `:ID:`).
fn reject_misrouted_config_key(layer: NodeLayer, key: &str) -> Result<(), ApiError> {
    if layer != NodeLayer::Config && CONFIG_RESERVED_KEYS.contains(&key) {
        return Err(ApiError::bad_request(format!(
            "{key} is a config-file property; use --kind config to write it to config.org (got --kind {})",
            layer.layer_name()
        )));
    }
    Ok(())
}

/// The known node id universe for a project, mirroring the id set
/// `lint_dangling_graph_edges` (index.rs) resolves edges against. Used by the
/// write-time reference-property guard below so a token either resolves here
/// or would also dangle at index time — the two never disagree.
async fn known_reference_ids(state: &ApiState, project_id: &str) -> BTreeSet<String> {
    let snap = state.index.snapshot().await;
    snap.project(project_id)
        .map(|project| {
            project
                .graph
                .nodes
                .iter()
                .map(|node| node.id.clone())
                .collect()
        })
        .unwrap_or_default()
}

/// Reject a reference-valued property (dec_HJENQ vocabulary) whose value
/// names a token that isn't a known node id — front-running the index-time
/// dangling-reference lint that previously only surfaced this as an opaque
/// global parse-error count (TASK-4T80X battle-test F4: prose accepted as
/// `--relates-to` at write time poisoned the whole index).
fn reject_unresolved_reference_token(
    known_ids: &BTreeSet<String>,
    key: &str,
    value: &str,
) -> Result<(), ApiError> {
    let unresolved = orgasmic_core::unresolved_reference_tokens(key, value, known_ids);
    if let Some(token) = unresolved.first() {
        return Err(ApiError::bad_request(format!(
            "{key} references unresolvable id `{token}`; expected space-separated node ids (use --force to write anyway)"
        )));
    }
    Ok(())
}

async fn create_graph_heading(
    state: &ApiState,
    layer: GraphLayer,
    req: GraphCreateRequest,
) -> Result<Json<GraphMutationResponse>, ApiError> {
    let (project_id, path) = graph_path(state, req.project.as_deref(), layer).await?;
    if !req.force {
        let known_ids = known_reference_ids(state, &project_id).await;
        for (key, value) in &req.properties {
            // Decision PARENT already has dedicated existence/class/cycle
            // validation below (validate_decision_create_parent); don't
            // double-reject it here with a weaker generic check.
            if layer == GraphLayer::Decision && key == "PARENT" {
                continue;
            }
            reject_unresolved_reference_token(&known_ids, key, value)?;
        }
    }
    let node_id = resolve_graph_create_id(layer, &req)?;
    let mut source = read_or_seed_graph_file(&path, layer)?;
    if layer == GraphLayer::Decision {
        let file = OrgFile::parse(source.clone(), path.to_string_lossy())
            .map_err(|e| org_parse_bad_request(&path, layer.artifact_name(), e))?;
        validate_decision_create_parent(&path, &file, &node_id, req.properties.get("PARENT"))?;
    }
    let heading = render_graph_heading(layer, &req, &node_id)?;
    if !source.ends_with('\n') {
        source.push('\n');
    }
    source.push('\n');
    source.push_str(&heading);
    OrgFile::parse(source.clone(), path.to_string_lossy())
        .map_err(|e| org_parse_bad_request(&path, layer.artifact_name(), e))?;
    write_graph_and_record(GraphWriteRequest {
        state,
        layer,
        project_id,
        path,
        source,
        request_id: req.request_id,
        tx_type: format!("graph.{}.created", layer.layer_name()),
        node_id,
        action: "created",
    })
    .await
}

fn graph_layer_class(layer: GraphLayer) -> orgasmic_core::NodeIdClass {
    match layer {
        GraphLayer::Decision => orgasmic_core::NodeIdClass::Decision,
        GraphLayer::Architecture => orgasmic_core::NodeIdClass::Architecture,
        GraphLayer::Glossary => orgasmic_core::NodeIdClass::Term,
    }
}

fn resolve_graph_create_id(
    layer: GraphLayer,
    req: &GraphCreateRequest,
) -> Result<String, ApiError> {
    let class = graph_layer_class(layer);
    match req.id.as_deref().map(str::trim).filter(|id| !id.is_empty()) {
        None => Ok(orgasmic_core::mint_node_id(class)),
        Some(id) => {
            if orgasmic_core::is_legacy_sequential_create_id(class, id) {
                return Err(ApiError::bad_request(format!(
                    "legacy sequential id {id} cannot be assigned on create; omit id to auto-mint or use `orgasmic id mint`"
                )));
            }
            Ok(id.to_string())
        }
    }
}

async fn mutate_graph_heading(
    state: &ApiState,
    layer: GraphLayer,
    id: String,
    req: GraphActionRequest,
) -> Result<Json<GraphMutationResponse>, ApiError> {
    let (project_id, path) = graph_path(state, req.project.as_deref(), layer).await?;
    if !req.force {
        let known_ids = known_reference_ids(state, &project_id).await;
        for (key, value) in &req.properties {
            // Decision PARENT already has dedicated existence/class/cycle
            // validation below (validate_decision_parent_property_update);
            // don't double-reject it here with a weaker generic check.
            if layer == GraphLayer::Decision && key == "PARENT" {
                continue;
            }
            reject_unresolved_reference_token(&known_ids, key, value)?;
        }
    }
    let source = read_artifact(&path, layer.artifact_name())?;
    let file = OrgFile::parse(source, path.to_string_lossy())
        .map_err(|e| org_parse_bad_request(&path, layer.artifact_name(), e))?;
    let mut rw = OrgRewriter::new(&file, path.to_string_lossy());
    let mut action = req.action.unwrap_or_else(|| "revise".to_string());
    if layer == GraphLayer::Decision && action == "delete" {
        return delete_decision_heading(state, project_id, path, file, id, req.request_id).await;
    }
    let parent_changed = if layer == GraphLayer::Decision {
        validate_decision_parent_property_update(&path, &file, &id, &req.properties)?
    } else {
        false
    };
    if parent_changed {
        action = "reparent".to_string();
    }
    for (key, value) in req.properties {
        if layer == GraphLayer::Decision && key == "PARENT" && value.trim().is_empty() {
            if file
                .find_by_id(&id)
                .and_then(|heading| heading.property("PARENT"))
                .is_some()
            {
                rw.remove_property(&id, "PARENT")
                    .map_err(|e| org_rewriter_error("remove property", &id, e))?;
            }
        } else {
            rw.upsert_property(&id, &key, &value)
                .map_err(|e| org_rewriter_error("set property", &id, e))?;
        }
    }
    let updated = rw.finish();
    OrgFile::parse(updated.clone(), path.to_string_lossy())
        .map_err(|e| org_parse_bad_request(&path, layer.artifact_name(), e))?;
    write_graph_and_record(GraphWriteRequest {
        state,
        layer,
        project_id,
        path,
        source: updated,
        request_id: req.request_id,
        tx_type: format!("graph.{}.{}", layer.layer_name(), action),
        node_id: id,
        action: &action,
    })
    .await
}

fn decision_parent_error(err: orgasmic_core::ParentTreeError) -> ApiError {
    ApiError::bad_request(format!("invalid decision :PARENT:: {err}"))
}

fn decision_parent_from_heading(heading: &Heading, id: &str) -> Result<Option<String>, ApiError> {
    orgasmic_core::parse_parent_value(
        orgasmic_core::NodeIdClass::Decision,
        id,
        heading.property("PARENT"),
    )
    .map_err(decision_parent_error)
}

fn decision_parent_nodes_with_override(
    file: &OrgFile,
    target_id: &str,
    override_parent: Option<Option<String>>,
) -> Result<Vec<orgasmic_core::ParentTreeNode>, ApiError> {
    let mut found = false;
    let mut nodes = Vec::new();
    for heading in &file.headings {
        let Some(id) = heading.property("ID") else {
            continue;
        };
        if !id.starts_with("dec_") {
            continue;
        }
        let parent = if id == target_id {
            found = true;
            override_parent
                .clone()
                .unwrap_or(decision_parent_from_heading(heading, id)?)
        } else {
            decision_parent_from_heading(heading, id)?
        };
        nodes.push(orgasmic_core::ParentTreeNode {
            id: id.to_string(),
            parent,
        });
    }
    if !found {
        return Err(ApiError::not_found(format!("decision {target_id}")));
    }
    Ok(nodes)
}

fn validate_decision_create_parent(
    _path: &FsPath,
    file: &OrgFile,
    id: &str,
    parent_raw: Option<&String>,
) -> Result<(), ApiError> {
    let parent = orgasmic_core::parse_parent_value(
        orgasmic_core::NodeIdClass::Decision,
        id,
        parent_raw.map(String::as_str),
    )
    .map_err(decision_parent_error)?;
    let ids = file
        .headings
        .iter()
        .filter_map(|heading| heading.property("ID"))
        .filter(|candidate| candidate.starts_with("dec_"))
        .collect::<Vec<_>>();
    orgasmic_core::validate_parent_exists(id, parent.as_deref(), ids).map_err(decision_parent_error)
}

fn validate_decision_parent_property_update(
    _path: &FsPath,
    file: &OrgFile,
    id: &str,
    properties: &BTreeMap<String, String>,
) -> Result<bool, ApiError> {
    let Some(next_raw) = properties.get("PARENT") else {
        return Ok(false);
    };
    let heading = file
        .find_by_id(id)
        .ok_or_else(|| ApiError::not_found(format!("decision {id}")))?;
    let current = decision_parent_from_heading(heading, id)?;
    let next = orgasmic_core::parse_parent_value(
        orgasmic_core::NodeIdClass::Decision,
        id,
        Some(next_raw.as_str()),
    )
    .map_err(decision_parent_error)?;
    if next == current {
        return Ok(false);
    }
    let nodes = decision_parent_nodes_with_override(file, id, Some(next))?;
    orgasmic_core::validate_parent_tree(orgasmic_core::NodeIdClass::Decision, nodes)
        .map_err(decision_parent_error)?;
    Ok(true)
}

fn decision_has_children(file: &OrgFile, id: &str) -> Result<bool, ApiError> {
    for heading in &file.headings {
        let Some(child_id) = heading.property("ID") else {
            continue;
        };
        if !child_id.starts_with("dec_") || child_id == id {
            continue;
        }
        if decision_parent_from_heading(heading, child_id)?.as_deref() == Some(id) {
            return Ok(true);
        }
    }
    Ok(false)
}

async fn delete_decision_heading(
    state: &ApiState,
    project_id: String,
    path: PathBuf,
    file: OrgFile,
    id: String,
    request_id: Option<String>,
) -> Result<Json<GraphMutationResponse>, ApiError> {
    file.find_by_id(&id)
        .ok_or_else(|| ApiError::not_found(format!("decision {id}")))?;
    if decision_has_children(&file, &id)? {
        return Err(ApiError::bad_request(format!(
            "decision {id} still has child decisions; re-parent or delete children first"
        )));
    }
    let mut rw = OrgRewriter::new(&file, path.to_string_lossy());
    rw.remove_heading(&id)
        .map_err(|e| org_rewriter_error("remove decision heading", &id, e))?;
    let updated = rw.finish();
    OrgFile::parse(updated.clone(), path.to_string_lossy())
        .map_err(|e| org_parse_bad_request(&path, "decisions file", e))?;
    write_graph_and_record(GraphWriteRequest {
        state,
        layer: GraphLayer::Decision,
        project_id,
        path,
        source: updated,
        request_id,
        tx_type: "graph.decision.deleted".to_string(),
        node_id: id,
        action: "deleted",
    })
    .await
}

// --- generic org-node read/edit (editor surface) ------------------------

#[derive(Debug, Deserialize)]
pub struct NodeQuery {
    #[serde(default)]
    pub project: Option<String>,
    pub id: String,
    /// Explicit layer selector (`decision`/`architecture`/`glossary`/`project`/`task`).
    /// Required for ids whose owning file cannot be inferred from the id prefix
    /// (e.g. the `project` node). When absent, the layer is inferred from `id`.
    #[serde(default)]
    pub kind: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct NodeProperty {
    pub key: String,
    pub value: String,
}

#[derive(Debug, Serialize)]
pub struct NodeSection {
    pub title: String,
    pub body: String,
}

#[derive(Debug, Serialize)]
pub struct NodeSource {
    pub file: String,
    /// Content hash of the node's heading span — the optimistic-concurrency
    /// token an edit must echo back.
    pub base_version: String,
}

/// A generic structured projection of a single org node: the read counterpart
/// to the edit-op endpoint. Replaces client-side `.org` parsing for detail
/// views.
#[derive(Debug, Serialize)]
pub struct NodeDoc {
    pub id: String,
    pub kind: String,
    pub title: String,
    pub todo: Option<String>,
    pub tags: Vec<String>,
    /// The heading's own free prose (between its drawer and first nested
    /// heading). Leaf architecture nodes keep their description here rather
    /// than in a named `**` section.
    pub body: String,
    pub properties: Vec<NodeProperty>,
    pub sections: Vec<NodeSection>,
    pub source: NodeSource,
}

#[derive(Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BodyFormat {
    #[default]
    Default,
    Raw,
}

fn prepare_body_edit(body: &str, body_format: BodyFormat) -> String {
    match body_format {
        BodyFormat::Default => body.to_string(),
        BodyFormat::Raw => orgasmic_core::wrap_raw_body(body),
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum NodeEditOp {
    /// Replace the heading's own free prose (leaf-node descriptions).
    SetBody {
        #[serde(default)]
        body: String,
        #[serde(default)]
        body_format: BodyFormat,
    },
    SetSectionBody {
        title: String,
        #[serde(default)]
        body: String,
        #[serde(default)]
        body_format: BodyFormat,
    },
    AddSection {
        title: String,
        #[serde(default)]
        body: String,
        #[serde(default)]
        body_format: BodyFormat,
    },
    RemoveSection {
        title: String,
    },
    SetProperty {
        key: String,
        value: String,
    },
    RemoveProperty {
        key: String,
    },
    SetTitle {
        title: String,
    },
    SetTags {
        #[serde(default)]
        tags: Vec<String>,
    },
}

#[derive(Debug, Deserialize)]
pub struct NodeEditRequest {
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub request_id: Option<String>,
    /// Explicit layer selector; see [`NodeQuery::kind`].
    #[serde(default)]
    pub kind: Option<String>,
    pub base_version: String,
    pub ops: Vec<NodeEditOp>,
    /// Skip the write-time reference-token check (dec_4T80X) for intentional
    /// forward references. The index-time dangling lint still catches it.
    #[serde(default)]
    pub force: bool,
}

/// FNV-1a/64 over the bytes, as 16-char lowercase hex. Dependency-free and
/// MSRV-safe; used only as an opaque optimistic-concurrency token.
fn content_hash(bytes: &[u8]) -> String {
    let mut hash: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
    }
    format!("{hash:016x}")
}

/// Display title with the leading id token stripped (`dec_001 Foo` → `Foo`)
/// when the heading actually has that token. Tokenless drifted headings still
/// display their full title so a repair edit does not hide the bad state.
fn node_display_title(heading: &Heading) -> String {
    let Some(id) = heading.property("ID") else {
        return heading.title.trim().to_string();
    };
    heading
        .title
        .strip_prefix(id)
        .map(str::trim)
        .unwrap_or_else(|| heading.title.trim())
        .to_string()
}

/// Recompose a node's title line from its immutable parts (level, TODO, id
/// token) plus optional new display title / tags, ready for `set_title_line`.
fn recompose_title_line(
    heading: &Heading,
    new_title: Option<&str>,
    new_tags: Option<&[String]>,
) -> String {
    let stars = "*".repeat(heading.level);
    let todo = heading
        .todo
        .as_deref()
        .map(|t| format!("{t} "))
        .unwrap_or_default();
    let drawer_id = heading.property("ID").unwrap_or(heading.title.as_str());
    let (id_token, current_display) = if let Some(rest) = heading.title.strip_prefix(drawer_id) {
        (drawer_id, rest.trim())
    } else {
        (drawer_id, heading.title.trim())
    };
    let display = new_title.map(str::trim).unwrap_or(current_display);
    let tags: Vec<String> = new_tags
        .map(|t| t.to_vec())
        .unwrap_or_else(|| heading.tags.clone());
    let mut line = format!("{stars} {todo}{id_token}");
    if !display.is_empty() {
        line.push(' ');
        line.push_str(display);
    }
    if !tags.is_empty() {
        line.push_str("    :");
        line.push_str(&tags.join(":"));
        line.push(':');
    }
    line
}

fn org_node_doc(
    file: &OrgFile,
    heading: &Heading,
    layer: NodeLayer,
    source_file: String,
) -> NodeDoc {
    let properties = heading
        .property_entries()
        .map(|e| NodeProperty {
            key: e.key.clone(),
            value: e.value.clone(),
        })
        .collect();
    let sections = heading
        .sections
        .iter()
        .map(|s| NodeSection {
            title: s.title.clone(),
            body: file.slice(s.body.clone()).trim_end().to_string(),
        })
        .collect();
    NodeDoc {
        id: heading.property("ID").unwrap_or_default().to_string(),
        kind: layer.layer_name().to_string(),
        title: node_display_title(heading),
        todo: heading.todo.clone(),
        tags: heading.tags.clone(),
        body: file.slice(heading.body.clone()).trim().to_string(),
        properties,
        sections,
        source: NodeSource {
            file: source_file,
            base_version: content_hash(file.slice(heading.span.clone()).as_bytes()),
        },
    }
}

/// Pick the layer that owns a node: an explicit `kind` selector when present,
/// otherwise inferred from the id prefix.
fn resolve_node_layer(kind: Option<&str>, id: &str) -> Result<NodeLayer, ApiError> {
    match kind {
        Some(kind) => NodeLayer::from_kind(kind)
            .ok_or_else(|| ApiError::bad_request(format!("unknown node kind {kind}"))),
        None => Ok(NodeLayer::for_id(id)),
    }
}

fn display_node_source_path(project_root: &FsPath, path: &FsPath) -> String {
    path.strip_prefix(project_root)
        .unwrap_or(path)
        .to_string_lossy()
        .to_string()
}

async fn org_node_path(
    state: &ApiState,
    project: Option<&str>,
    id: &str,
    layer: NodeLayer,
) -> Result<(String, PathBuf, String), ApiError> {
    let snap = state.index.snapshot().await;
    let project = select_project(&snap, project)?;
    match layer {
        NodeLayer::Task => {
            let task = project
                .tasks
                .iter()
                .find(|task| task.id == id)
                .ok_or_else(|| ApiError::not_found(format!("node {id}")))?;
            let path = task.source_file.clone();
            let source_file = display_node_source_path(&project.root, &path);
            Ok((project.project_id.clone(), path, source_file))
        }
        NodeLayer::Goal => {
            let path = goal_file_path(&project.root);
            let source_file = display_node_source_path(&project.root, &path);
            Ok((project.project_id.clone(), path, source_file))
        }
        NodeLayer::Handoff => {
            let path = handoff_file_path(&project.root);
            let source_file = display_node_source_path(&project.root, &path);
            Ok((project.project_id.clone(), path, source_file))
        }
        _ => {
            let file_name = layer
                .file_name()
                .expect("non-task node layers have fixed .org files");
            Ok((
                project.project_id.clone(),
                project.root.join(".orgasmic").join(file_name),
                format!(".orgasmic/{file_name}"),
            ))
        }
    }
}

async fn get_org_node(
    State(state): State<ApiState>,
    Query(q): Query<NodeQuery>,
) -> Result<Json<NodeDoc>, ApiError> {
    let layer = resolve_node_layer(q.kind.as_deref(), &q.id)?;
    let (_project_id, path, source_file) =
        org_node_path(&state, q.project.as_deref(), &q.id, layer).await?;
    let source = read_artifact(&path, layer.artifact_name())?;
    let file = OrgFile::parse(source, path.to_string_lossy())
        .map_err(|e| org_parse_bad_request(&path, layer.artifact_name(), e))?;
    let heading = file
        .find_by_id(&q.id)
        .ok_or_else(|| ApiError::not_found(format!("node {}", q.id)))?;
    Ok(Json(org_node_doc(&file, heading, layer, source_file)))
}

async fn post_org_node_edit(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Query(q): Query<MutationOutputQuery>,
    Json(req): Json<NodeEditRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let layer = resolve_node_layer(req.kind.as_deref(), &id)?;
    if layer == NodeLayer::Goal {
        return Err(ApiError::bad_request(
            "goal nodes are read-only through /org/node; use goal set/clear/supersede",
        ));
    }
    for op in &req.ops {
        if let NodeEditOp::SetProperty { key, .. } = op {
            reject_misrouted_config_key(layer, key)?;
        }
    }
    let (project_id, path, source_file) =
        org_node_path(&state, req.project.as_deref(), &id, layer).await?;
    if !req.force {
        let known_ids = known_reference_ids(&state, &project_id).await;
        for op in &req.ops {
            if let NodeEditOp::SetProperty { key, value } = op {
                // Decision PARENT already has dedicated existence/class/cycle
                // validation below (validate_decision_parent_ops); don't
                // double-reject it here with a weaker generic check.
                if layer == NodeLayer::Decision && key == "PARENT" {
                    continue;
                }
                reject_unresolved_reference_token(&known_ids, key, value)?;
            }
        }
    }
    let source = read_artifact(&path, layer.artifact_name())?;
    let file = OrgFile::parse(source, path.to_string_lossy())
        .map_err(|e| org_parse_bad_request(&path, layer.artifact_name(), e))?;
    let heading = file
        .find_by_id(&id)
        .ok_or_else(|| ApiError::not_found(format!("node {id}")))?;

    // Optimistic concurrency: reject if the on-disk node drifted since read.
    let current_version = content_hash(file.slice(heading.span.clone()).as_bytes());
    if current_version != req.base_version {
        return Err(ApiError::conflict(format!(
            "node {id} changed on disk; reload before editing"
        )));
    }

    let parent_changed = if layer == NodeLayer::Decision {
        validate_decision_parent_ops(&file, &id, &req.ops)?
    } else {
        false
    };

    let mut rw = OrgRewriter::new(&file, path.to_string_lossy());
    let mut title_override: Option<String> = None;
    let mut tags_override: Option<Vec<String>> = None;
    let mut changed: BTreeMap<String, String> = BTreeMap::new();
    for op in req.ops {
        match op {
            NodeEditOp::SetBody { body, body_format } => {
                let body = prepare_body_edit(&body, body_format);
                rw.set_node_body(&id, &body)
                    .map_err(|e| org_rewriter_error("set body", &id, e))?;
                changed.insert("body".to_string(), body);
            }
            NodeEditOp::SetSectionBody {
                title,
                body,
                body_format,
            }
            | NodeEditOp::AddSection {
                title,
                body,
                body_format,
            } => {
                let body = prepare_body_edit(&body, body_format);
                rw.upsert_section_text(&id, &title, &body)
                    .map_err(|e| org_rewriter_error("edit section", &id, e))?;
                changed.insert(format!("section:{title}"), body);
            }
            NodeEditOp::RemoveSection { title } => {
                rw.remove_section(&id, &title)
                    .map_err(|e| org_rewriter_error("remove section", &id, e))?;
                changed.insert(format!("section:{title}"), "<removed>".to_string());
            }
            NodeEditOp::SetProperty { key, value } => {
                rw.upsert_property(&id, &key, &value)
                    .map_err(|e| org_rewriter_error("set property", &id, e))?;
                changed.insert(key, value);
            }
            NodeEditOp::RemoveProperty { key } => {
                rw.remove_property(&id, &key)
                    .map_err(|e| org_rewriter_error("remove property", &id, e))?;
                changed.insert(key, "<removed>".to_string());
            }
            NodeEditOp::SetTitle { title } => {
                changed.insert("title".to_string(), title.clone());
                title_override = Some(title);
            }
            NodeEditOp::SetTags { tags } => {
                changed.insert("tags".to_string(), tags.join(" "));
                tags_override = Some(tags);
            }
        }
    }
    if title_override.is_some() || tags_override.is_some() {
        let line =
            recompose_title_line(heading, title_override.as_deref(), tags_override.as_deref());
        rw.set_title_line(&id, &line)
            .map_err(|e| org_rewriter_error("set title line", &id, e))?;
    }

    let updated = rw.finish();
    OrgFile::parse(updated.clone(), path.to_string_lossy())
        .map_err(|e| org_parse_bad_request(&path, layer.artifact_name(), e))?;
    let tx_id = write_org_node_edit_and_record(NodeEditWriteRequest {
        state: &state,
        layer,
        project_id: project_id.clone(),
        path: path.clone(),
        source: updated,
        request_id: req.request_id,
        node_id: id.clone(),
        action: if parent_changed { "reparent" } else { "edited" },
    })
    .await?;

    // Re-read so the response carries the fresh base_version.
    let fresh = read_artifact(&path, layer.artifact_name())?;
    let file = OrgFile::parse(fresh, path.to_string_lossy())
        .map_err(|e| org_parse_internal(&path, layer.artifact_name(), e))?;
    let heading = file
        .find_by_id(&id)
        .ok_or_else(|| ApiError::not_found(format!("node {id}")))?;
    let doc = org_node_doc(&file, heading, layer, source_file);
    let compact = CompactMutationResponse {
        id: id.clone(),
        changed,
        tx_id,
    };
    compact_or_full_response(q.json, compact, doc)
}

fn validate_decision_parent_ops(
    file: &OrgFile,
    id: &str,
    ops: &[NodeEditOp],
) -> Result<bool, ApiError> {
    let heading = file
        .find_by_id(id)
        .ok_or_else(|| ApiError::not_found(format!("decision {id}")))?;
    let current = decision_parent_from_heading(heading, id)?;
    let mut next = current.clone();
    let mut touched = false;
    for op in ops {
        match op {
            NodeEditOp::SetProperty { key, value } if key == "PARENT" => {
                next = orgasmic_core::parse_parent_value(
                    orgasmic_core::NodeIdClass::Decision,
                    id,
                    Some(value.as_str()),
                )
                .map_err(decision_parent_error)?;
                touched = true;
            }
            NodeEditOp::RemoveProperty { key } if key == "PARENT" => {
                next = None;
                touched = true;
            }
            _ => {}
        }
    }
    if !touched || next == current {
        return Ok(false);
    }
    let nodes = decision_parent_nodes_with_override(file, id, Some(next))?;
    orgasmic_core::validate_parent_tree(orgasmic_core::NodeIdClass::Decision, nodes)
        .map_err(decision_parent_error)?;
    Ok(true)
}

struct NodeEditWriteRequest<'a> {
    state: &'a ApiState,
    layer: NodeLayer,
    project_id: String,
    path: PathBuf,
    source: String,
    request_id: Option<String>,
    node_id: String,
    action: &'a str,
}

async fn write_org_node_edit_and_record(req: NodeEditWriteRequest<'_>) -> Result<String, ApiError> {
    let primary_rewrite = FileRewrite {
        path: req.path.clone(),
        new_contents: req.source.into_bytes(),
    };
    let project_tx_type = if req.layer == NodeLayer::Task {
        "task.edited".to_string()
    } else {
        format!("graph.{}.{}", req.layer.layer_name(), req.action)
    };
    let prepared_tx = prepare_api_tx(
        req.state,
        ApiTxRequest {
            ty: project_tx_type,
            actor: None,
            project: Some(req.project_id.clone()),
            task: if req.layer == NodeLayer::Task {
                Some(req.node_id.clone())
            } else {
                None
            },
            target: Some(req.path.display().to_string()),
            reason: format!(
                "{} {} node {}",
                req.action,
                req.layer.layer_name(),
                req.node_id
            ),
            request_id: req.request_id.map(|id| format!("{id}/tx")),
            extra: vec![("NODE_ID".to_string(), req.node_id.clone())],
        },
    )
    .await?;
    let tx_id = req
        .state
        .writer
        .transaction(vec![primary_rewrite], prepared_tx.tx)
        .await
        .map_err(writer_transaction_error)?;
    refresh_after_tx(
        req.state,
        prepared_tx.project_tx,
        prepared_tx.destination_project_id,
    )
    .await;
    if req.layer == NodeLayer::Task {
        req.state.events.publish(
            Topic::Task,
            EventPayload::TaskUpdated {
                project_id: req.project_id.clone(),
                task_id: req.node_id.clone(),
            },
        );
    } else {
        req.state.events.publish(
            Topic::Graph,
            EventPayload::GraphNodeRevised {
                project_id: req.project_id.clone(),
                layer: req.layer.layer_name().to_string(),
                node_id: req.node_id.clone(),
                action: req.action.to_string(),
                tx_id: tx_id.clone(),
            },
        );
    }
    let _ = req.state.index.refresh_project(&req.project_id).await;
    Ok(tx_id)
}

async fn graph_path(
    state: &ApiState,
    project: Option<&str>,
    layer: GraphLayer,
) -> Result<(String, PathBuf), ApiError> {
    let snap = state.index.snapshot().await;
    let project = select_project(&snap, project)?;
    Ok((
        project.project_id.clone(),
        project.root.join(".orgasmic").join(layer.file_name()),
    ))
}

struct GraphWriteRequest<'a> {
    state: &'a ApiState,
    layer: GraphLayer,
    project_id: String,
    path: PathBuf,
    source: String,
    request_id: Option<String>,
    tx_type: String,
    node_id: String,
    action: &'a str,
}

async fn write_graph_and_record(
    req: GraphWriteRequest<'_>,
) -> Result<Json<GraphMutationResponse>, ApiError> {
    let primary_rewrite = FileRewrite {
        path: req.path.clone(),
        new_contents: req.source.into_bytes(),
    };
    let rewrites = vec![primary_rewrite];
    let prepared_tx = prepare_api_tx(
        req.state,
        ApiTxRequest {
            ty: req.tx_type,
            actor: None,
            project: Some(req.project_id.clone()),
            task: None,
            target: Some(req.path.display().to_string()),
            reason: format!("{} graph node {}", req.action, req.node_id),
            request_id: req.request_id.map(|id| format!("{id}/tx")),
            extra: vec![("NODE_ID".to_string(), req.node_id.clone())],
        },
    )
    .await?;
    let tx_id = req
        .state
        .writer
        .transaction(rewrites, prepared_tx.tx)
        .await
        .map_err(writer_transaction_error)?;
    refresh_after_tx(
        req.state,
        prepared_tx.project_tx,
        prepared_tx.destination_project_id,
    )
    .await;
    let layer = req.layer.layer_name().to_string();
    if req.action == "created" {
        req.state.events.publish(
            Topic::Graph,
            EventPayload::GraphNodeCreated {
                project_id: req.project_id.clone(),
                layer: layer.clone(),
                node_id: req.node_id.clone(),
                tx_id: tx_id.clone(),
            },
        );
    } else {
        req.state.events.publish(
            Topic::Graph,
            EventPayload::GraphNodeRevised {
                project_id: req.project_id.clone(),
                layer: layer.clone(),
                node_id: req.node_id.clone(),
                action: req.action.to_string(),
                tx_id: tx_id.clone(),
            },
        );
    }
    let _ = req.state.index.refresh_project(&req.project_id).await;
    Ok(Json(GraphMutationResponse {
        id: req.node_id,
        action: req.action.to_string(),
        tx_id,
    }))
}

fn read_or_seed_graph_file(path: &FsPath, layer: GraphLayer) -> Result<String, ApiError> {
    if path.exists() {
        return read_artifact(path, layer.artifact_name());
    }
    Ok(format!(
        "#+title: orgasmic {}\n#+orgasmic_version: 1\n",
        layer.file_name()
    ))
}

fn render_graph_heading(
    layer: GraphLayer,
    req: &GraphCreateRequest,
    id: &str,
) -> Result<String, ApiError> {
    if id.trim().is_empty() {
        return Err(ApiError::bad_request("graph node id is required"));
    }
    if matches!(layer, GraphLayer::Glossary) {
        reject_glossary_implementation_detail(req)?;
    }
    let title = req.title.as_deref().unwrap_or(id).trim();
    let mut properties = req.properties.clone();
    properties.insert("ID".to_string(), id.to_string());
    let heading_title = match layer {
        GraphLayer::Decision | GraphLayer::Architecture if title == id => id.to_string(),
        GraphLayer::Decision | GraphLayer::Architecture => format!("{id} {title}"),
        GraphLayer::Glossary => {
            if id.starts_with("term_") {
                format!("{id} {title}")
            } else if title.starts_with("term:") {
                title.to_string()
            } else {
                format!("{}{} {}", layer.title_prefix(), id, title)
            }
        }
    };
    let mut out = format!("* {heading_title}\n:PROPERTIES:\n");
    for (key, value) in properties {
        out.push_str(&format!(":{key}: {value}\n"));
    }
    out.push_str(":END:\n");
    if let Some(body) = req.body.as_deref() {
        out.push('\n');
        out.push_str(body.trim_end());
        out.push('\n');
    }
    Ok(out)
}

/// Path/symbol markers: unambiguous in prose, matched as plain substrings.
const GLOSSARY_MARKER_SUBSTRINGS: &[&str] = &["crates/", "src/", ".rs", "::"];

/// Code-keyword markers: also legitimate English words/word-fragments
/// ("construct", "reconstruction", "implement"), so these are matched as
/// whole word tokens only.
const GLOSSARY_MARKER_WORDS: &[&str] = &["fn", "struct", "impl", "enum", "trait"];

/// True if `text` contains `word` as a standalone token: bounded on both
/// sides by the start/end of the string or a non-alphanumeric, non-`_`
/// character. Prevents `struct` from matching inside `construct` or
/// `reconstruction` while still catching `struct Foo {` or `a struct`.
fn contains_word(text: &str, word: &str) -> bool {
    let is_boundary = |c: Option<char>| !matches!(c, Some(c) if c.is_alphanumeric() || c == '_');
    let bytes = text.as_bytes();
    let word_len = word.len();
    text.match_indices(word).any(|(start, _)| {
        let before = text[..start].chars().next_back();
        let after_idx = start + word_len;
        let after = if after_idx <= bytes.len() {
            text[after_idx..].chars().next()
        } else {
            None
        };
        is_boundary(before) && is_boundary(after)
    })
}

fn reject_glossary_implementation_detail(req: &GraphCreateRequest) -> Result<(), ApiError> {
    if req.allow_marker {
        return Ok(());
    }
    let mut text = String::new();
    if let Some(definition) = req.properties.get("DEFINITION") {
        text.push_str(definition);
    }
    if let Some(body) = req.body.as_deref() {
        text.push('\n');
        text.push_str(body);
    }
    if let Some(marker) = GLOSSARY_MARKER_SUBSTRINGS
        .iter()
        .find(|marker| text.contains(**marker))
    {
        return Err(ApiError::bad_request(format!(
            "glossary definition contains implementation detail marker {marker:?} \
             (use --allow-marker if this glossary legitimately defines language constructs)"
        )));
    }
    if let Some(marker) = GLOSSARY_MARKER_WORDS
        .iter()
        .find(|marker| contains_word(&text, marker))
    {
        return Err(ApiError::bad_request(format!(
            "glossary definition contains implementation detail marker {marker:?} \
             (use --allow-marker if this glossary legitimately defines language constructs)"
        )));
    }
    Ok(())
}

/// 404 for a project id absent from the loaded index. Distinguishes the
/// residual window right after a project appears on the board (registered
/// but not yet loaded/watched) from a genuinely unknown id, so operators
/// don't chase a stale-daemon hypothesis on the former (TASK-GERBB).
fn project_not_found_error(snap: &IndexSnapshot, project_id: &str) -> ApiError {
    if snap.board.iter().any(|entry| entry.id == project_id) {
        tracing::warn!(
            project_id = %project_id,
            "project registered on board but not yet loaded"
        );
        return ApiError::not_found(format!(
            "project {project_id} is registered on the board but not yet loaded \
             (registered after daemon boot); it will load on the next reindex, \
             or run `orgasmic restart`"
        ));
    }
    tracing::warn!(project_id = %project_id, "project not found");
    ApiError::not_found("project not found")
}

fn select_graph<'a>(
    snap: &'a IndexSnapshot,
    project: Option<&str>,
) -> Result<&'a crate::index::GraphIndex, ApiError> {
    Ok(&select_project(snap, project)?.graph)
}

fn select_project<'a>(
    snap: &'a IndexSnapshot,
    project: Option<&str>,
) -> Result<&'a crate::index::ProjectIndex, ApiError> {
    if let Some(id) = project {
        return snap
            .projects
            .get(id)
            .ok_or_else(|| ApiError::not_found(format!("project {id}")));
    }
    if snap.projects.len() == 1 {
        return snap.projects.values().next().ok_or_else(|| {
            ApiError::not_found("no project available for graph route".to_string())
        });
    }
    Err(ApiError::not_found(
        "graph route requires ?project= when board has zero or multiple projects",
    ))
}

fn default_dispatch_worker_id(
    home: &Home,
    project: &ProjectIndex,
    kind: DispatchEndpointKind,
) -> Option<String> {
    first_pipeline_worker_for_kind(home, project, kind.worker_kind())
        .ok()
        .flatten()
}

fn split_dispatch_scope(value: &str) -> Vec<&str> {
    value.split_whitespace().collect()
}

fn filter_default_dispatch_properties(
    properties: &BTreeMap<String, String>,
    default_worker: Option<&str>,
    default_test_cmd: &str,
    default_write_scope: &[String],
) -> BTreeMap<String, String> {
    properties
        .iter()
        .filter_map(|(key, value)| {
            let trimmed = value.trim();
            if matches!(key.as_str(), "WORKER" | "TEST_CMD" | "WRITE_SCOPE") && trimmed.is_empty() {
                return None;
            }
            if key == "WORKER" && default_worker.map(str::trim) == Some(trimmed) {
                return None;
            }
            if key == "TEST_CMD" && default_test_cmd.trim() == trimmed {
                return None;
            }
            if key == "WRITE_SCOPE"
                && split_dispatch_scope(trimmed)
                    == default_write_scope
                        .iter()
                        .map(String::as_str)
                        .collect::<Vec<_>>()
            {
                return None;
            }
            Some((key.clone(), value.clone()))
        })
        .collect()
}

// ---- task create + property update (TASK-NP5H6) ----------------------------

/// Target file for new top-level tasks (dec_QQYXM: always backlog.org).
fn task_create_target_file_name() -> &'static str {
    DEFAULT_TASK_FILE
}

fn task_create_target_path(project_root: &FsPath) -> PathBuf {
    task_file_path(project_root, task_create_target_file_name())
}

#[derive(Debug, Deserialize)]
struct TaskCreateRequest {
    #[serde(default)]
    id: Option<String>,
    title: String,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    properties: BTreeMap<String, String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    request_id: Option<String>,
    /// Skip the write-time reference-token check (dec_4T80X / TASK-KSD6D)
    /// for intentional forward references. The index-time dangling lint
    /// still catches it.
    #[serde(default)]
    force: bool,
}

#[derive(Debug, Serialize)]
struct TaskCreateResponse {
    id: String,
    tx_id: String,
}

fn resolve_task_create_id(req: &TaskCreateRequest) -> Result<String, ApiError> {
    match req.id.as_deref().map(str::trim).filter(|id| !id.is_empty()) {
        None => Ok(orgasmic_core::mint_node_id(
            orgasmic_core::NodeIdClass::Task,
        )),
        Some(id) => {
            if orgasmic_core::is_legacy_sequential_create_id(orgasmic_core::NodeIdClass::Task, id) {
                return Err(ApiError::bad_request(format!(
                    "legacy sequential id {id} cannot be assigned on create; omit id to auto-mint or use `orgasmic id mint`"
                )));
            }
            if !orgasmic_core::is_valid_task_path_id(id) {
                return Err(ApiError::bad_request(format!("invalid task id: {id}")));
            }
            Ok(id.to_string())
        }
    }
}

fn task_create_lifecycle_stage(req: &TaskCreateRequest) -> Result<LifecycleStage, ApiError> {
    if req.properties.contains_key("STATE") {
        return Err(ApiError::bad_request(
            "STATE is owned by the lifecycle state machine; new tasks land in BACKLOG",
        ));
    }
    Ok(LifecycleStage::Backlog)
}

fn render_new_task_heading(
    id: &str,
    title: &str,
    tags: &[String],
    stage: LifecycleStage,
    properties: &BTreeMap<String, String>,
    body: Option<&str>,
) -> String {
    let clean_title = title
        .chars()
        .map(|ch| if ch == '\n' || ch == '\r' { ' ' } else { ch })
        .collect::<String>();
    let mut line = format!("* {} {} {}", stage.todo_keyword(), id, clean_title.trim());
    if !tags.is_empty() {
        line.push_str("    :");
        line.push_str(&tags.join(":"));
        line.push(':');
    }
    line.push('\n');
    line.push_str(":PROPERTIES:\n");
    let mut props = properties.clone();
    props
        .entry("ID".to_string())
        .or_insert_with(|| id.to_string());
    const DROPPED: &[&str] = &[
        "STATE",
        "KIND",
        "PARENT_TASK",
        "FIX_SUBTASK",
        "BLOCKED_BY",
        "LAST_UPDATED",
    ];
    for (key, value) in props {
        if DROPPED
            .iter()
            .any(|dropped| key.eq_ignore_ascii_case(dropped))
        {
            continue;
        }
        line.push_str(&format!(":{key}: {value}\n"));
    }
    line.push_str(":END:\n");
    if let Some(body) = body.filter(|value| !value.trim().is_empty()) {
        line.push('\n');
        line.push_str(body.trim_end());
        line.push('\n');
    }
    line
}

/// Task bodies are spliced into the task file verbatim; nested `**` sections
/// are legitimate structure, but a column-0 level-1 heading would escape the
/// task subtree and land as a sibling top-level task (the TASK-HC7PW
/// phantom-heading class). Block-aware so `#+begin_example` payloads keep
/// their literal star lines.
fn reject_structural_task_body(body: &str) -> Result<(), ApiError> {
    let mut block_depth = 0usize;
    for line in body.lines() {
        let directive = line.trim_start().to_ascii_lowercase();
        if directive.starts_with("#+begin_") {
            block_depth += 1;
            continue;
        }
        if directive.starts_with("#+end_") {
            block_depth = block_depth.saturating_sub(1);
            continue;
        }
        if block_depth == 0 && line.starts_with("* ") {
            return Err(ApiError::bad_request(format!(
                "task body must not contain column-0 level-1 `* ` headings (would escape the task subtree); use `**` sections or comma-escape inside a block: {line:?}"
            )));
        }
    }
    Ok(())
}

async fn post_task_create(
    State(state): State<ApiState>,
    Path(project_id): Path<String>,
    Json(req): Json<TaskCreateRequest>,
) -> Result<Json<TaskCreateResponse>, ApiError> {
    if req.title.trim().is_empty() {
        return Err(ApiError::bad_request("task title is required"));
    }
    if let Some(body) = req.body.as_deref() {
        reject_structural_task_body(body)?;
    }
    if req.properties.contains_key("ID") {
        return Err(ApiError::bad_request(
            "do not pass ID as a property; the drawer :ID: is derived from the task id (use `id` to pin one)",
        ));
    }
    let snap = state.index.snapshot().await;
    let project = snap
        .project(&project_id)
        .ok_or_else(|| project_not_found_error(&snap, &project_id))?;
    if !req.force {
        let known_ids = known_reference_ids(&state, &project_id).await;
        for (key, value) in &req.properties {
            reject_unresolved_reference_token(&known_ids, key, value)?;
        }
    }
    let task_id = resolve_task_create_id(&req)?;
    let stage = task_create_lifecycle_stage(&req)?;
    let path = task_create_target_path(&project.root);
    let mut source = if path.exists() {
        read_artifact(&path, "task file")?
    } else {
        format!(
            "#+title: orgasmic {}\n#+orgasmic_version: 1\n",
            task_create_target_file_name().trim_end_matches(".org")
        )
    };
    let project_defaults = project_prompt_defaults(project);
    let default_worker =
        default_dispatch_worker_id(&state.home, project, DispatchEndpointKind::Implementer);
    let properties = filter_default_dispatch_properties(
        &req.properties,
        default_worker.as_deref(),
        project_defaults.test_cmd.as_str(),
        &project_defaults.write_scope,
    );
    let heading = render_new_task_heading(
        &task_id,
        &req.title,
        &req.tags,
        stage,
        &properties,
        req.body.as_deref(),
    );
    if !source.ends_with('\n') {
        source.push('\n');
    }
    if !source.ends_with("\n\n") {
        source.push('\n');
    }
    source.push_str(&heading);
    OrgFile::parse(source.clone(), path.to_string_lossy())
        .map_err(|e| org_parse_bad_request(&path, "task file", e))?;
    let target = task_file_rel(task_create_target_file_name());
    let reason = req
        .reason
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| format!("created task {task_id}"));
    let prepared = prepare_api_tx(
        &state,
        ApiTxRequest {
            ty: "task.created".to_string(),
            actor: None,
            project: Some(project_id.clone()),
            task: Some(task_id.clone()),
            target: Some(target),
            reason,
            request_id: req.request_id.clone().map(|id| format!("{id}/tx")),
            extra: Vec::new(),
        },
    )
    .await?;
    let tx_id = state
        .writer
        .transaction(
            vec![FileRewrite {
                path: path.clone(),
                new_contents: source.into_bytes(),
            }],
            prepared.tx,
        )
        .await
        .map_err(writer_transaction_error)?;
    refresh_after_tx(&state, prepared.project_tx, prepared.destination_project_id).await;
    state.events.publish(
        Topic::Task,
        EventPayload::TaskUpdated {
            project_id: project_id.clone(),
            task_id: task_id.clone(),
        },
    );
    let _ = state.index.refresh_project(&project_id).await;
    Ok(Json(TaskCreateResponse { id: task_id, tx_id }))
}

// ---- goal lifecycle + task state flip (TASK-G9KCN) -------------------------

#[derive(Debug, Deserialize)]
struct TaskUpdateRequest {
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    priority: Option<String>,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    request_id: Option<String>,
    #[serde(default)]
    properties: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct GoalSetRequest {
    #[serde(default)]
    id: Option<String>,
    title: String,
    statement: String,
    #[serde(default)]
    reached_when: Option<String>,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    request_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GoalMutationRequest {
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    request_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct GoalMutationResponse {
    goal_id: String,
    tx_id: String,
    tx_path: String,
}

fn org_goal_date(now: &chrono::DateTime<Utc>) -> String {
    now.format("[%Y-%m-%d %a]").to_string()
}

/// Goal statement / reached-when bodies are spliced into goal.org verbatim;
/// reject payloads that would change the heading structure (the TASK-HC7PW
/// invariant — this write path must not become a phantom-heading injector).
fn reject_structural_goal_body(field: &str, value: &str) -> Result<(), ApiError> {
    for line in value.lines() {
        if line.starts_with('*') || line.starts_with("#+") {
            return Err(ApiError::bad_request(format!(
                "{field} must not contain column-0 `*` or `#+` lines (would alter goal.org structure); indent or rephrase: {line:?}"
            )));
        }
    }
    Ok(())
}

fn find_active_goal(file: &OrgFile) -> Option<&Heading> {
    file.headings.iter().find(|heading| {
        heading.todo.as_deref() == Some("GOAL") || heading.property("STATUS") == Some("active")
    })
}

fn goal_keyword_title_line(heading: &Heading, keyword: &str) -> String {
    let mut line = format!(
        "{} {} {}",
        "*".repeat(heading.level),
        keyword,
        heading.title
    );
    if !heading.tags.is_empty() {
        line.push(' ');
        line.push(':');
        line.push_str(&heading.tags.join(":"));
        line.push(':');
    }
    line
}

fn task_lifecycle_title_line(heading: &Heading, stage: LifecycleStage) -> String {
    let mut line = format!(
        "{} {} {}",
        "*".repeat(heading.level),
        stage.todo_keyword(),
        heading.title
    );
    if !heading.tags.is_empty() {
        line.push(' ');
        line.push(':');
        line.push_str(&heading.tags.join(":"));
        line.push(':');
    }
    line
}

fn prepend_goal_heading(source: &str, block: &str) -> String {
    let trimmed = block.trim_end();
    if let Some(idx) = source.find("\n* ") {
        let (head, tail) = source.split_at(idx + 1);
        format!("{head}{trimmed}\n\n{tail}")
    } else if source.ends_with('\n') {
        format!("{source}{trimmed}\n")
    } else {
        format!("{source}\n\n{trimmed}\n")
    }
}

fn render_new_goal_heading(
    id: &str,
    title: &str,
    actor: &str,
    set_at: &str,
    replaces: &str,
    statement: &str,
    reached_when: Option<&str>,
) -> String {
    let clean_title = title
        .chars()
        .map(|ch| if ch == '\n' || ch == '\r' { ' ' } else { ch })
        .collect::<String>();
    let mut heading = format!(
        "* GOAL {} :goal:\n:PROPERTIES:\n:ID:               {id}\n:SET_AT:           {set_at}\n:SET_BY:           {actor}\n:REPLACES:         {replaces}\n:END:\n\n** Statement\n{statement}\n",
        clean_title.trim()
    );
    if let Some(reached) = reached_when.filter(|value| !value.trim().is_empty()) {
        heading.push('\n');
        heading.push_str("** Reached When\n");
        heading.push_str(reached.trim_end());
        heading.push('\n');
    }
    heading
}

fn supersede_goal_in_rewrite(
    rw: &mut OrgRewriter,
    heading: &Heading,
    superseded_at: &str,
) -> Result<(), ApiError> {
    let id = heading
        .property("ID")
        .ok_or_else(|| ApiError::bad_request("active goal is missing :ID:"))?;
    rw.set_title_line(id, &goal_keyword_title_line(heading, "SUPERSEDED"))
        .map_err(|e| org_rewriter_error("supersede goal title", id, e))?;
    rw.upsert_property(id, "STATUS", "superseded")
        .map_err(|e| org_rewriter_error("supersede goal status", id, e))?;
    rw.upsert_property(id, "SUPERSEDED_AT", superseded_at)
        .map_err(|e| org_rewriter_error("supersede goal timestamp", id, e))?;
    Ok(())
}

fn clear_goal_in_rewrite(
    rw: &mut OrgRewriter,
    heading: &Heading,
    cleared_at: &str,
    actor: &str,
) -> Result<(), ApiError> {
    let id = heading
        .property("ID")
        .ok_or_else(|| ApiError::bad_request("active goal is missing :ID:"))?;
    rw.set_title_line(id, &goal_keyword_title_line(heading, "CLEARED"))
        .map_err(|e| org_rewriter_error("clear goal title", id, e))?;
    rw.upsert_property(id, "STATUS", "cleared")
        .map_err(|e| org_rewriter_error("clear goal status", id, e))?;
    rw.upsert_property(id, "CLEARED_AT", cleared_at)
        .map_err(|e| org_rewriter_error("clear goal timestamp", id, e))?;
    rw.upsert_property(id, "CLEARED_BY", actor)
        .map_err(|e| org_rewriter_error("clear goal actor", id, e))?;
    Ok(())
}

async fn project_board_entry(
    state: &ApiState,
    project_id: &str,
) -> Result<crate::index::BoardEntry, ApiError> {
    let snap = state.index.snapshot().await;
    snap.board
        .iter()
        .find(|entry| entry.id == project_id)
        .cloned()
        .ok_or_else(|| ApiError::not_found(format!("project {project_id}")))
}

struct GoalWriteRequest<'a> {
    state: &'a ApiState,
    project_id: &'a str,
    path: PathBuf,
    source: String,
    tx_type: &'a str,
    goal_id: &'a str,
    replaces: Option<&'a str>,
    reason: String,
    request_id: Option<String>,
}

async fn write_goal_file_and_record(
    req: GoalWriteRequest<'_>,
) -> Result<(String, String), ApiError> {
    OrgFile::parse(req.source.clone(), req.path.to_string_lossy())
        .map_err(|e| org_parse_bad_request(&req.path, "goal file", e))?;
    let mut extra = vec![("GOAL_ID".to_string(), req.goal_id.to_string())];
    if let Some(replaces) = req.replaces {
        extra.push(("REPLACES".to_string(), replaces.to_string()));
    }
    let prepared_tx = prepare_api_tx(
        req.state,
        ApiTxRequest {
            ty: req.tx_type.to_string(),
            actor: None,
            project: Some(req.project_id.to_string()),
            task: None,
            target: Some(goal_file_rel().to_string()),
            reason: req.reason,
            request_id: req.request_id,
            extra,
        },
    )
    .await?;
    let tx_path = prepared_tx.tx.tx_path.display().to_string();
    let tx_id = req
        .state
        .writer
        .transaction(
            vec![FileRewrite {
                path: req.path.clone(),
                new_contents: req.source.into_bytes(),
            }],
            prepared_tx.tx,
        )
        .await
        .map_err(writer_transaction_error)?;
    refresh_after_tx(
        req.state,
        prepared_tx.project_tx,
        prepared_tx.destination_project_id,
    )
    .await;
    let _ = req.state.index.refresh_project(req.project_id).await;
    Ok((tx_id, tx_path))
}

async fn post_goal_set(
    State(state): State<ApiState>,
    Path(project_id): Path<String>,
    Json(req): Json<GoalSetRequest>,
) -> Result<Json<GoalMutationResponse>, ApiError> {
    if req.title.trim().is_empty() {
        return Err(ApiError::bad_request("goal title is required"));
    }
    if req.statement.trim().is_empty() {
        return Err(ApiError::bad_request("goal statement is required"));
    }
    reject_structural_goal_body("statement", &req.statement)?;
    if let Some(reached) = req.reached_when.as_deref() {
        reject_structural_goal_body("reached_when", reached)?;
    }
    let entry = project_board_entry(&state, &project_id).await?;
    let path = goal_file_path(&entry.path);
    let source = if path.exists() {
        read_artifact(&path, "goal file")?
    } else {
        format!("#+title: {project_id} active goal\n#+orgasmic_version: 1\n#+scope: project\n\n")
    };
    let file = OrgFile::parse(source.clone(), path.to_string_lossy())
        .map_err(|e| org_parse_bad_request(&path, "goal file", e))?;
    let now = Utc::now();
    let set_at = org_goal_date(&now);
    let actor = choose_actor(
        &TxAppendRequest {
            request_id: None,
            r#type: "manager.set_goal".into(),
            actor: None,
            machine: None,
            project: Some(project_id.clone()),
            task: None,
            target: None,
            reason: None,
            extra: Vec::new(),
            tx_path: None,
        },
        Some(&entry),
        &state,
    );
    let previous = find_active_goal(&file).and_then(|heading| heading.property("ID"));
    let goal_id = req
        .id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| {
            format!(
                "goal-{}-{}",
                now.format("%Y%m%d"),
                req.title
                    .split_whitespace()
                    .take(3)
                    .collect::<Vec<_>>()
                    .join("-")
                    .to_ascii_lowercase()
                    .chars()
                    .map(|ch| {
                        if ch.is_ascii_alphanumeric() || ch == '-' {
                            ch
                        } else {
                            '-'
                        }
                    })
                    .collect::<String>()
            )
        });
    let replaces = previous.unwrap_or("—");
    let mut rw = OrgRewriter::new(&file, path.to_string_lossy());
    if let Some(active) = find_active_goal(&file) {
        supersede_goal_in_rewrite(&mut rw, active, &set_at)?;
    }
    let mut updated = rw.finish();
    let block = render_new_goal_heading(
        &goal_id,
        &req.title,
        &actor,
        &set_at,
        replaces,
        &req.statement,
        req.reached_when.as_deref(),
    );
    updated = prepend_goal_heading(&updated, &block);
    let reason = req
        .reason
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| format!("set {goal_id}"));
    let handoff_request_id = req.request_id.clone();
    let (tx_id, tx_path) = write_goal_file_and_record(GoalWriteRequest {
        state: &state,
        project_id: &project_id,
        path,
        source: updated,
        tx_type: "manager.set_goal",
        goal_id: &goal_id,
        replaces: Some(replaces),
        reason,
        request_id: req.request_id,
    })
    .await?;
    // F11 (TASK-MTB56): `goal set` used to leave handoff-current.GOAL_ID
    // pointing at the superseded goal. Sync it here, in the same operation,
    // so the two never drift.
    sync_handoff_goal_id(
        &state,
        &project_id,
        &entry.path,
        &goal_id,
        handoff_request_id,
    )
    .await?;
    Ok(Json(GoalMutationResponse {
        goal_id,
        tx_id,
        tx_path,
    }))
}

/// Keep `handoff-current`'s `:GOAL_ID:` property pointing at the currently
/// active goal after `goal set` mints a new one (F11, TASK-MTB56). A no-op
/// when there is no handoff file/node yet, or it already matches — this is
/// best-effort continuity metadata, not load-bearing state, so a missing
/// handoff surface never fails the goal-set call itself.
async fn sync_handoff_goal_id(
    state: &ApiState,
    project_id: &str,
    project_root: &FsPath,
    goal_id: &str,
    request_id: Option<String>,
) -> Result<(), ApiError> {
    let path = handoff_file_path(project_root);
    if !path.exists() {
        return Ok(());
    }
    let source = read_artifact(&path, "handoff file")?;
    let file = OrgFile::parse(source, path.to_string_lossy())
        .map_err(|e| org_parse_bad_request(&path, "handoff file", e))?;
    let Some(heading) = file.find_by_id("handoff-current") else {
        return Ok(());
    };
    if heading.property("GOAL_ID") == Some(goal_id) {
        return Ok(());
    }
    let mut rw = OrgRewriter::new(&file, path.to_string_lossy());
    rw.upsert_property("handoff-current", "GOAL_ID", goal_id)
        .map_err(|e| org_rewriter_error("sync handoff GOAL_ID", "handoff-current", e))?;
    let updated = rw.finish();
    write_org_node_edit_and_record(NodeEditWriteRequest {
        state,
        layer: NodeLayer::Handoff,
        project_id: project_id.to_string(),
        path,
        source: updated,
        request_id,
        node_id: "handoff-current".to_string(),
        action: "edited",
    })
    .await?;
    Ok(())
}

async fn post_goal_clear(
    State(state): State<ApiState>,
    Path(project_id): Path<String>,
    Json(req): Json<GoalMutationRequest>,
) -> Result<Json<GoalMutationResponse>, ApiError> {
    let entry = project_board_entry(&state, &project_id).await?;
    let path = goal_file_path(&entry.path);
    let source = read_artifact(&path, "goal file")?;
    let file = OrgFile::parse(source, path.to_string_lossy())
        .map_err(|e| org_parse_bad_request(&path, "goal file", e))?;
    let active =
        find_active_goal(&file).ok_or_else(|| ApiError::not_found("no active goal to clear"))?;
    let goal_id = active
        .property("ID")
        .ok_or_else(|| ApiError::bad_request("active goal is missing :ID:"))?
        .to_string();
    let now = Utc::now();
    let cleared_at = org_goal_date(&now);
    let actor = choose_actor(
        &TxAppendRequest {
            request_id: None,
            r#type: "manager.clear_goal".into(),
            actor: None,
            machine: None,
            project: Some(project_id.clone()),
            task: None,
            target: None,
            reason: None,
            extra: Vec::new(),
            tx_path: None,
        },
        Some(&entry),
        &state,
    );
    let mut rw = OrgRewriter::new(&file, path.to_string_lossy());
    clear_goal_in_rewrite(&mut rw, active, &cleared_at, &actor)?;
    let updated = rw.finish();
    let reason = req
        .reason
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| format!("clear {goal_id}"));
    let (tx_id, tx_path) = write_goal_file_and_record(GoalWriteRequest {
        state: &state,
        project_id: &project_id,
        path,
        source: updated,
        tx_type: "manager.clear_goal",
        goal_id: &goal_id,
        replaces: None,
        reason,
        request_id: req.request_id,
    })
    .await?;
    Ok(Json(GoalMutationResponse {
        goal_id,
        tx_id,
        tx_path,
    }))
}

async fn post_goal_supersede(
    State(state): State<ApiState>,
    Path(project_id): Path<String>,
    Json(req): Json<GoalMutationRequest>,
) -> Result<Json<GoalMutationResponse>, ApiError> {
    let entry = project_board_entry(&state, &project_id).await?;
    let path = goal_file_path(&entry.path);
    let source = read_artifact(&path, "goal file")?;
    let file = OrgFile::parse(source, path.to_string_lossy())
        .map_err(|e| org_parse_bad_request(&path, "goal file", e))?;
    let active = find_active_goal(&file)
        .ok_or_else(|| ApiError::not_found("no active goal to supersede"))?;
    let goal_id = active
        .property("ID")
        .ok_or_else(|| ApiError::bad_request("active goal is missing :ID:"))?
        .to_string();
    let superseded_at = org_goal_date(&Utc::now());
    let mut rw = OrgRewriter::new(&file, path.to_string_lossy());
    supersede_goal_in_rewrite(&mut rw, active, &superseded_at)?;
    let updated = rw.finish();
    let reason = req
        .reason
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| format!("supersede {goal_id}"));
    let (tx_id, tx_path) = write_goal_file_and_record(GoalWriteRequest {
        state: &state,
        project_id: &project_id,
        path,
        source: updated,
        tx_type: "manager.supersede_goal",
        goal_id: &goal_id,
        replaces: None,
        reason,
        request_id: req.request_id,
    })
    .await?;
    Ok(Json(GoalMutationResponse {
        goal_id,
        tx_id,
        tx_path,
    }))
}

struct TaskLifecycleWriteRequest<'a> {
    state: &'a ApiState,
    project_id: &'a str,
    task_id: &'a str,
    rewrites: Vec<(PathBuf, String)>,
    target_rel: String,
    from_state: LifecycleStage,
    to_state: LifecycleStage,
    reason: String,
    request_id: Option<String>,
}

async fn write_task_lifecycle_and_record(
    req: TaskLifecycleWriteRequest<'_>,
) -> Result<String, ApiError> {
    for (path, source) in &req.rewrites {
        OrgFile::parse(source.clone(), path.to_string_lossy())
            .map_err(|e| org_parse_bad_request(path, "task file", e))?;
    }
    let prepared_tx = prepare_api_tx(
        req.state,
        ApiTxRequest {
            ty: "task.state_transitioned".to_string(),
            actor: None,
            project: Some(req.project_id.to_string()),
            task: Some(req.task_id.to_string()),
            target: Some(req.target_rel),
            reason: req.reason,
            request_id: req.request_id,
            extra: vec![
                (
                    "FROM_STATE".to_string(),
                    req.from_state.as_str().to_string(),
                ),
                ("TO_STATE".to_string(), req.to_state.as_str().to_string()),
            ],
        },
    )
    .await?;
    let file_rewrites = req
        .rewrites
        .into_iter()
        .map(|(path, source)| FileRewrite {
            path,
            new_contents: source.into_bytes(),
        })
        .collect();
    let tx_id = req
        .state
        .writer
        .transaction(file_rewrites, prepared_tx.tx)
        .await
        .map_err(writer_transaction_error)?;
    refresh_after_tx(
        req.state,
        prepared_tx.project_tx,
        prepared_tx.destination_project_id,
    )
    .await;
    let _ = req.state.index.refresh_project(req.project_id).await;
    Ok(tx_id)
}

fn empty_task_file_header(file_name: &str) -> String {
    format!(
        "#+title: orgasmic {}\n#+orgasmic_version: 1\n",
        file_name.trim_end_matches(".org")
    )
}

fn append_heading_to_task_file(existing: &str, heading: &str) -> String {
    let mut out = existing.to_string();
    if !out.ends_with('\n') {
        out.push('\n');
    }
    if !out.ends_with("\n\n") {
        out.push('\n');
    }
    out.push_str(heading.trim_start_matches('\n'));
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

fn rewrite_heading_subtree_for_stage(
    subtree: &str,
    task_id: &str,
    stage: LifecycleStage,
) -> Result<String, ApiError> {
    let file = OrgFile::parse(subtree, "heading.org")
        .map_err(|e| ApiError::bad_request(format!("task subtree parse failed: {e}")))?;
    let heading = file
        .find_by_id(task_id)
        .ok_or_else(|| ApiError::not_found(format!("task {task_id} in subtree")))?;
    let mut rw = OrgRewriter::new(&file, "heading.org");
    rw.set_title_line(task_id, &task_lifecycle_title_line(heading, stage))
        .map_err(|e| org_rewriter_error("set task lifecycle title", task_id, e))?;
    Ok(rw.finish())
}

fn prepare_cross_file_lifecycle_move(
    source_text: &str,
    source_display: &str,
    task_id: &str,
    stage: LifecycleStage,
) -> Result<(String, String), ApiError> {
    let file = OrgFile::parse(source_text, source_display)
        .map_err(|e| org_parse_bad_request(&PathBuf::from(source_display), "task file", e))?;
    let heading = file
        .find_by_id(task_id)
        .ok_or_else(|| ApiError::not_found(format!("task {task_id}")))?;
    let subtree = file.slice(heading.span.clone());
    let moved = rewrite_heading_subtree_for_stage(subtree, task_id, stage)?;
    let mut rw = OrgRewriter::new(&file, source_display);
    rw.remove_heading(task_id)
        .map_err(|e| org_rewriter_error("remove task heading", task_id, e))?;
    Ok((rw.finish(), moved))
}

async fn post_task_update(
    State(state): State<ApiState>,
    Path((project_id, task_id)): Path<(String, String)>,
    Query(q): Query<MutationOutputQuery>,
    Json(req): Json<TaskUpdateRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if req.state.is_some() {
        return update_task_state(&state, &project_id, &task_id, q.json, req).await;
    }
    if req.priority.is_some() || !req.properties.is_empty() {
        return update_task_properties(&state, &project_id, &task_id, q.json, req).await;
    }
    Err(ApiError::bad_request(
        "task update requires at least one field",
    ))
}

async fn update_task_state(
    state: &ApiState,
    project_id: &str,
    task_id: &str,
    want_full: bool,
    req: TaskUpdateRequest,
) -> Result<Json<serde_json::Value>, ApiError> {
    let to_state = LifecycleStage::from_str(req.state.as_deref().unwrap_or(""))
        .map_err(|_| ApiError::bad_request("unknown task state"))?;
    let snap = state.index.snapshot().await;
    let project = snap
        .project(project_id)
        .ok_or_else(|| project_not_found_error(&snap, project_id))?;
    let task = project
        .tasks
        .iter()
        .find(|task| task.id == task_id)
        .ok_or_else(|| {
            tracing::warn!(project_id = %project_id, task_id = %task_id, "task not found");
            ApiError::not_found("task not found")
        })?;
    let from_state = task.lifecycle_stage;
    if from_state == to_state {
        let supervisor = state.supervisor.snapshot().await;
        let project = apply_task_owners(project.clone(), &supervisor.runs);
        let task = project
            .tasks
            .iter()
            .find(|task| task.id == task_id)
            .cloned()
            .expect("task index membership checked above");
        let body = project.task_bodies.get(task_id).cloned();
        let detail = crate::index::TaskDetail::from_indexed_body(task, body);
        let compact = CompactMutationResponse {
            id: task_id.to_string(),
            changed: BTreeMap::new(),
            tx_id: String::new(),
        };
        return compact_or_full_response(want_full, compact, detail);
    }
    let from_path = task.source_file.clone();
    let to_file_name = lifecycle_stage_file_name(to_state);
    let to_path = task_file_path(&project.root, to_file_name);
    let source_display = from_path.to_string_lossy().to_string();
    let source = read_artifact(&from_path, "task file")?;
    let rewrites = if from_path == to_path {
        let file = OrgFile::parse(source.clone(), &source_display)
            .map_err(|e| org_parse_bad_request(&from_path, "task file", e))?;
        let mut rw = OrgRewriter::new(&file, &source_display);
        let heading = file
            .find_by_id(task_id)
            .ok_or_else(|| ApiError::not_found(format!("task {task_id}")))?;
        rw.set_title_line(task_id, &task_lifecycle_title_line(heading, to_state))
            .map_err(|e| org_rewriter_error("set task lifecycle title", task_id, e))?;
        vec![(from_path, rw.finish())]
    } else {
        let (source_updated, moved_heading) =
            prepare_cross_file_lifecycle_move(&source, &source_display, task_id, to_state)?;
        let dest_existing = if to_path.exists() {
            read_artifact(&to_path, "task file")?
        } else {
            empty_task_file_header(to_file_name)
        };
        let dest_updated = append_heading_to_task_file(&dest_existing, &moved_heading);
        vec![(from_path, source_updated), (to_path, dest_updated)]
    };
    let reason = req
        .reason
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| format!("transition {task_id} to {}", to_state.as_str()));
    let tx_id = write_task_lifecycle_and_record(TaskLifecycleWriteRequest {
        state,
        project_id,
        task_id,
        rewrites,
        target_rel: task_file_rel(to_file_name),
        from_state,
        to_state,
        reason,
        request_id: req.request_id,
    })
    .await?;
    state.events.publish(
        Topic::Task,
        EventPayload::TaskUpdated {
            project_id: project_id.to_string(),
            task_id: task_id.to_string(),
        },
    );
    let snap = state.index.snapshot().await;
    let project = snap
        .project(project_id)
        .ok_or_else(|| ApiError::internal("project index unavailable after state flip"))?;
    let supervisor = state.supervisor.snapshot().await;
    let project = apply_task_owners(project.clone(), &supervisor.runs);
    let task = project
        .tasks
        .iter()
        .find(|task| task.id == task_id)
        .cloned()
        .ok_or_else(|| ApiError::internal("task missing from index after state flip"))?;
    let body = project.task_bodies.get(task_id).cloned();
    let detail = crate::index::TaskDetail::from_indexed_body(task, body);
    let mut changed = BTreeMap::new();
    changed.insert("STATE".to_string(), to_state.as_str().to_string());
    let compact = CompactMutationResponse {
        id: task_id.to_string(),
        changed,
        tx_id,
    };
    compact_or_full_response(want_full, compact, detail)
}

struct TaskPropertyWriteRequest<'a> {
    state: &'a ApiState,
    project_id: &'a str,
    project_root: &'a FsPath,
    task_id: &'a str,
    path: PathBuf,
    source: String,
    reason: String,
    request_id: Option<String>,
    changed: Vec<(String, String)>,
}

async fn write_task_property_and_record(
    req: TaskPropertyWriteRequest<'_>,
) -> Result<String, ApiError> {
    OrgFile::parse(req.source.clone(), req.path.to_string_lossy())
        .map_err(|e| org_parse_bad_request(&req.path, "task file", e))?;
    let target = req
        .path
        .strip_prefix(req.project_root)
        .unwrap_or(req.path.as_path())
        .to_string_lossy()
        .to_string();
    let prepared_tx = prepare_api_tx(
        req.state,
        ApiTxRequest {
            ty: "task.property_updated".to_string(),
            actor: None,
            project: Some(req.project_id.to_string()),
            task: Some(req.task_id.to_string()),
            target: Some(target),
            reason: req.reason,
            request_id: req.request_id,
            extra: req.changed,
        },
    )
    .await?;
    let tx_id = req
        .state
        .writer
        .transaction(
            vec![FileRewrite {
                path: req.path.clone(),
                new_contents: req.source.into_bytes(),
            }],
            prepared_tx.tx,
        )
        .await
        .map_err(writer_transaction_error)?;
    refresh_after_tx(
        req.state,
        prepared_tx.project_tx,
        prepared_tx.destination_project_id,
    )
    .await;
    Ok(tx_id)
}

async fn update_task_properties(
    state: &ApiState,
    project_id: &str,
    task_id: &str,
    want_full: bool,
    req: TaskUpdateRequest,
) -> Result<Json<serde_json::Value>, ApiError> {
    let snap = state.index.snapshot().await;
    let project = snap
        .project(project_id)
        .ok_or_else(|| project_not_found_error(&snap, project_id))?;
    let task = project
        .tasks
        .iter()
        .find(|task| task.id == task_id)
        .ok_or_else(|| {
            tracing::warn!(project_id = %project_id, task_id = %task_id, "task not found");
            ApiError::not_found("task not found")
        })?;
    let path = task.source_file.clone();
    let source = read_artifact(&path, "task file")?;
    let file = OrgFile::parse(source, path.to_string_lossy())
        .map_err(|e| org_parse_bad_request(&path, "task file", e))?;
    let mut rw = OrgRewriter::new(&file, path.to_string_lossy());
    let mut changed = Vec::new();
    if let Some(priority) = req
        .priority
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        rw.upsert_property(task_id, "PRIORITY", priority)
            .map_err(|e| org_rewriter_error("set task priority", task_id, e))?;
        changed.push(("PRIORITY".to_string(), priority.to_string()));
    }
    for (key, value) in req.properties {
        let key = key.trim();
        if key.is_empty() {
            continue;
        }
        if key.eq_ignore_ascii_case("STATE") {
            return Err(ApiError::bad_request(
                "STATE is owned by the lifecycle state machine; use `--state` (the heading keyword and task.state_transitioned tx must move with it)",
            ));
        }
        if key.eq_ignore_ascii_case("ID") {
            return Err(ApiError::bad_request("task identity is immutable"));
        }
        rw.upsert_property(task_id, key, &value)
            .map_err(|e| org_rewriter_error("set task property", task_id, e))?;
        changed.push((key.to_string(), value));
    }
    if changed.is_empty() {
        return Err(ApiError::bad_request(
            "task update requires at least one property",
        ));
    }
    let updated = rw.finish();
    let reason = req
        .reason
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| format!("update properties on {task_id}"));
    let changed_map: BTreeMap<String, String> = changed.iter().cloned().collect();
    let tx_id = write_task_property_and_record(TaskPropertyWriteRequest {
        state,
        project_id,
        project_root: &project.root,
        task_id,
        path,
        source: updated,
        reason,
        request_id: req.request_id,
        changed,
    })
    .await?;
    state.events.publish(
        Topic::Task,
        EventPayload::TaskUpdated {
            project_id: project_id.to_string(),
            task_id: task_id.to_string(),
        },
    );
    let snap = state.index.snapshot().await;
    let project = snap
        .project(project_id)
        .ok_or_else(|| ApiError::internal("project index unavailable after property update"))?;
    let supervisor = state.supervisor.snapshot().await;
    let project = apply_task_owners(project.clone(), &supervisor.runs);
    let task = project
        .tasks
        .iter()
        .find(|task| task.id == task_id)
        .cloned()
        .ok_or_else(|| ApiError::internal("task missing from index after property update"))?;
    let body = project.task_bodies.get(task_id).cloned();
    let detail = crate::index::TaskDetail::from_indexed_body(task, body);
    let compact = CompactMutationResponse {
        id: task_id.to_string(),
        changed: changed_map,
        tx_id,
    };
    compact_or_full_response(want_full, compact, detail)
}

// ---- stubs -----------------------------------------------------------------

fn stub(
    task: &'static str,
) -> impl Fn() -> std::pin::Pin<Box<dyn std::future::Future<Output = Response> + Send>> + Clone {
    move || {
        Box::pin(async move {
            (
                StatusCode::NOT_IMPLEMENTED,
                Json(json!({
                    "error": "not implemented",
                    "tracked_by": task,
                })),
            )
                .into_response()
        })
    }
}

// ---- artifact handlers (arch_ARSPJ / TASK-ZEFEY) ---------------------------

#[derive(Debug, Deserialize)]
struct ArtifactQuery {
    project: Option<String>,
    version: Option<u32>,
    /// Current-version comment lists exclude consumed comments by default;
    /// archived-version views (TASK-EDQPG) pass this to see them all.
    #[serde(default)]
    include_consumed: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct ArtifactSubmitRequest {
    content: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    subject_nodes: Option<Vec<String>>,
    #[serde(default)]
    prompt: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ArtifactCommentRequest {
    message: String,
    /// Opaque JSON anchor (selection-to-pin capture: `PlanCommentAnchor`
    /// shape — textQuote, targetNodeId/path, canvas coords — is a UI-side
    /// concern; the daemon stores and round-trips it verbatim).
    #[serde(default)]
    anchor: Option<String>,
    #[serde(default)]
    resolution_target: Option<String>,
    /// Admin-only override (a member's comment is always stamped with their
    /// session identity's name, never this field — see `post_artifact_add_comment`).
    #[serde(default)]
    author: Option<String>,
}

fn resolve_artifact_project<'a>(
    snap: &'a IndexSnapshot,
    project: Option<&str>,
) -> Result<&'a BoardEntry, ApiError> {
    let id = project
        .and_then(|s| if s.is_empty() { None } else { Some(s) })
        .ok_or_else(|| ApiError::bad_request("missing required query param: project"))?;
    snap.board
        .iter()
        .find(|e| e.id == id)
        .ok_or_else(|| ApiError::not_found("project not found"))
}

/// Shared choke point for every artifact route taking `Path(art_id)`: rejects
/// traversal/malformed ids with a house-style 400 before the id reaches
/// `artifact_dir()` or any fs/index touch (TASK-3K9ZG).
fn require_valid_art_id(art_id: &str) -> Result<(), ApiError> {
    validate_art_id(art_id).map_err(ApiError::bad_request)
}

async fn get_artifacts(
    State(state): State<ApiState>,
    Extension(identity): Extension<Identity>,
    Query(q): Query<ArtifactQuery>,
) -> Result<Json<Vec<ArtifactSummary>>, ApiError> {
    let snap = state.index.snapshot().await;
    let entry = resolve_artifact_project(&snap, q.project.as_deref())?;
    authz::require(&identity, Some(&entry.id), Action::ArtifactsRead)?;
    let project = snap
        .projects
        .get(&entry.id)
        .ok_or_else(|| ApiError::not_found("project index unavailable"))?;
    Ok(Json(project.artifacts.clone()))
}

async fn get_artifact(
    State(state): State<ApiState>,
    Extension(identity): Extension<Identity>,
    Path(id): Path<String>,
    Query(q): Query<ArtifactQuery>,
) -> Result<Json<ArtifactDetail>, ApiError> {
    require_valid_art_id(&id)?;
    let snap = state.index.snapshot().await;
    let entry = resolve_artifact_project(&snap, q.project.as_deref())?;
    authz::require(&identity, Some(&entry.id), Action::ArtifactsRead)?;
    let art_dir = artifact_dir(&entry.path, &id);
    let detail = load_artifact_detail(&art_dir, q.version, q.include_consumed.unwrap_or(false))
        .map_err(|e| match e {
            ArtifactLoadError::NotFound => ApiError::not_found("artifact not found"),
            ArtifactLoadError::VersionNotFound(v) => {
                ApiError::not_found(format!("artifact {id} has no version {v}"))
            }
        })?;
    Ok(Json(detail))
}

async fn post_artifact_submit(
    State(state): State<ApiState>,
    Path(art_id): Path<String>,
    Query(q): Query<ArtifactQuery>,
    Json(body): Json<ArtifactSubmitRequest>,
) -> Result<Json<Value>, ApiError> {
    require_valid_art_id(&art_id)?;
    // Validate block registry before any write.
    let errs = validate_mdx(&body.content);
    if !errs.is_empty() {
        return Err(ApiError {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            message: "MDX validation failed".into(),
            body: Some(json!({ "error": "MDX validation failed", "block_errors": errs })),
        });
    }

    let snap = state.index.snapshot().await;
    let entry = resolve_artifact_project(&snap, q.project.as_deref())?.clone();
    drop(snap);

    let art_dir = artifact_dir(&entry.path, &art_id);

    // Serialize the whole submit (create or version-bump) against a concurrent
    // submit and against feedback/consume/regenerate on the same artifact
    // (TASK-2ZQSB reviewer M1). Without it the existing-artifact branch below
    // reads the current version outside any lock and then blind-writes
    // version+1, so two concurrent submits lose an update and the vN archive
    // races; a submit could also interleave with regenerate's state read/flip.
    // Same per-artifact lock as the feedback handlers (Y2ZQJ M1); held across
    // the existence check so create/version-bump can't race either.
    let write_lock = state.artifact_write_lock(&art_dir);
    let _write_guard = write_lock.lock().await;

    let is_new = !art_dir.join("artifact.org").exists();

    if is_new {
        let title = body
            .title
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(artifacts::escape_single_line)
            .ok_or_else(|| ApiError::bad_request("title is required for new artifact"))?;
        let subject_nodes = body.subject_nodes.clone().unwrap_or_default();
        let prompt = artifacts::escape_single_line(body.prompt.as_deref().unwrap_or(""));

        // Write initial artifact.org.
        let org_content =
            artifact_org_content(&art_id, &title, &subject_nodes, &prompt, 1, "submitted");

        // Write reviews.org header.
        let reviews_header = reviews_org_header(&art_id);

        let now = Utc::now();
        let time_str = now.format("[%Y-%m-%d %a %H:%M:%S]").to_string();
        let project_date = now.format("%Y%m%d").to_string();
        let month_str = now.format("%Y-%m").to_string();
        let tx_path = entry
            .path
            .join(".orgasmic")
            .join("tx")
            .join(format!("{month_str}.org"));

        // artifact.created tx. Uses a request_id stable across retries for
        // the same (project, artifact) pair so a client retry (or a retry
        // after the files transaction below fails) does not double-append
        // this marker while the writer process is still alive.
        let created_tx_id = format!(
            "tx-{}-{}",
            now.format("%Y%m%d%H%M%S"),
            &uuid::Uuid::new_v4().to_string()[..8]
        );
        let mut created_entry = orgasmic_core::tx::TxEntry::new(
            &created_tx_id,
            "artifact.created",
            &time_str,
            &state.actor,
            &state.machine,
        );
        created_entry.project = Some(entry.id.clone());
        created_entry.extra = vec![
            ("ARTIFACT_ID".into(), art_id.clone()),
            ("TITLE".into(), title.clone()),
            ("SUBJECT_NODES".into(), subject_nodes.join(" ")),
            ("PROMPT".into(), prompt.clone()),
        ];
        state
            .writer
            .append_tx(
                TxAppend {
                    tx_path: tx_path.clone(),
                    entry: created_entry,
                    project_id: Some(entry.id.clone()),
                    tx_id_policy: TxIdPolicy::ProjectSequence {
                        project_id: entry.id.clone(),
                        date: project_date.clone(),
                    },
                    request_id: Some(format!("artifact-created-{}-{art_id}", entry.id)),
                },
                None,
            )
            .await
            .map_err(|e| ApiError::internal(format!("write artifact.created tx: {e}")))?;

        // Write files + artifact.submitted tx atomically.
        let mut submitted_entry = orgasmic_core::tx::TxEntry::new(
            "pending",
            "artifact.submitted",
            &time_str,
            &state.actor,
            &state.machine,
        );
        submitted_entry.project = Some(entry.id.clone());
        submitted_entry.extra = vec![
            ("ARTIFACT_ID".into(), art_id.clone()),
            ("VERSION".into(), "1".into()),
        ];

        state
            .writer
            .transaction(
                vec![
                    FileRewrite {
                        path: art_dir.join("artifact.org"),
                        new_contents: org_content.into_bytes(),
                    },
                    FileRewrite {
                        path: art_dir.join("artifact.mdx"),
                        new_contents: body.content.clone().into_bytes(),
                    },
                    FileRewrite {
                        path: art_dir.join("reviews.org"),
                        new_contents: reviews_header.into_bytes(),
                    },
                ],
                TxAppend {
                    tx_path,
                    entry: submitted_entry,
                    project_id: Some(entry.id.clone()),
                    tx_id_policy: TxIdPolicy::ProjectSequence {
                        project_id: entry.id.clone(),
                        date: project_date,
                    },
                    request_id: None,
                },
            )
            .await
            .map_err(|e| ApiError::internal(format!("write artifact files: {e}")))?;

        let _ = state.index.refresh_project(&entry.id).await;
        state.events.publish(
            Topic::Artifact,
            EventPayload::ArtifactChanged {
                project_id: entry.id.clone(),
                artifact_id: art_id.clone(),
                state: "submitted".into(),
            },
        );
        return Ok(Json(json!({ "artifact_id": art_id, "version": 1 })));
    }

    // Existing artifact: read current version, bump version, archive the
    // outgoing mdx as part of the same writer transaction as the new
    // artifact.org/artifact.mdx (no bypass fs writes, no partial state if
    // the transaction fails partway).
    let current_org = std::fs::read_to_string(art_dir.join("artifact.org"))
        .map_err(|e| ApiError::internal(format!("read artifact.org: {e}")))?;
    let current_version = load_artifact(&art_dir).map(|s| s.version).unwrap_or(1);
    let new_version = current_version + 1;
    let new_state = "submitted";

    let now = Utc::now();
    let time_str = now.format("[%Y-%m-%d %a %H:%M:%S]").to_string();
    let project_date = now.format("%Y%m%d").to_string();
    let month_str = now.format("%Y-%m").to_string();
    let tx_path = entry
        .path
        .join(".orgasmic")
        .join("tx")
        .join(format!("{month_str}.org"));

    let new_org = update_artifact_org(&current_org, new_version, new_state)
        .map_err(|e| ApiError::internal(format!("update artifact.org: {e}")))?;

    let mut submitted_entry = orgasmic_core::tx::TxEntry::new(
        "pending",
        "artifact.submitted",
        &time_str,
        &state.actor,
        &state.machine,
    );
    submitted_entry.project = Some(entry.id.clone());
    submitted_entry.extra = vec![
        ("ARTIFACT_ID".into(), art_id.clone()),
        ("VERSION".into(), new_version.to_string()),
    ];

    let mut rewrites = vec![
        FileRewrite {
            path: art_dir.join("artifact.org"),
            new_contents: new_org,
        },
        FileRewrite {
            path: art_dir.join("artifact.mdx"),
            new_contents: body.content.into_bytes(),
        },
    ];
    if let Ok(outgoing_mdx) = std::fs::read(art_dir.join("artifact.mdx")) {
        rewrites.push(FileRewrite {
            path: versions_dir(&art_dir).join(format!("v{current_version}.mdx")),
            new_contents: outgoing_mdx,
        });
    }

    state
        .writer
        .transaction(
            rewrites,
            TxAppend {
                tx_path,
                entry: submitted_entry,
                project_id: Some(entry.id.clone()),
                tx_id_policy: TxIdPolicy::ProjectSequence {
                    project_id: entry.id.clone(),
                    date: project_date,
                },
                request_id: None,
            },
        )
        .await
        .map_err(|e| ApiError::internal(format!("write artifact files: {e}")))?;

    let _ = state.index.refresh_project(&entry.id).await;
    state.events.publish(
        Topic::Artifact,
        EventPayload::ArtifactChanged {
            project_id: entry.id.clone(),
            artifact_id: art_id.clone(),
            state: new_state.into(),
        },
    );
    Ok(Json(
        json!({ "artifact_id": art_id, "version": new_version }),
    ))
}

/// `POST /artifacts/:id/comments` — pin a named comment (selection-to-pin
/// capture, arch_A3NSW). A member's comment is always attributed to their own
/// session identity; the request body's `author` field is honored only for
/// the admin identity (backward-compatible scripting override), never for a
/// member — closing the client-supplied-author spoofing hole the old
/// `/feedback` route had.
async fn post_artifact_add_comment(
    State(state): State<ApiState>,
    Extension(identity): Extension<Identity>,
    Path(art_id): Path<String>,
    Query(q): Query<ArtifactQuery>,
    Json(body): Json<ArtifactCommentRequest>,
) -> Result<Json<Value>, ApiError> {
    require_valid_art_id(&art_id)?;
    let snap = state.index.snapshot().await;
    let entry = resolve_artifact_project(&snap, q.project.as_deref())?.clone();
    authz::require(&identity, Some(&entry.id), Action::ArtifactsComment)?;
    let project_id = entry.id.clone();
    let project_root = entry.path.clone();
    drop(snap);

    let art_dir = artifact_dir(&project_root, &art_id);
    if !art_dir.join("artifact.org").exists() {
        return Err(ApiError::not_found("artifact not found"));
    }

    // Serialize the reviews.org read→transaction window against a concurrent
    // add/consume on the same artifact (finding M1). Held for the rest of the
    // handler; released on return.
    let write_lock = state.artifact_write_lock(&art_dir);
    let _write_guard = write_lock.lock().await;

    // One canonical read of artifact.org for both the regenerating guard and
    // the current version (finding L1: was two ad-hoc line scrapers).
    let summary = load_artifact(&art_dir)
        .ok_or_else(|| ApiError::internal("read artifact.org: unreadable or unparseable"))?;
    if summary.state == "regenerating" {
        return Err(ApiError::conflict(
            "artifact is regenerating; comments cannot be added now",
        ));
    }
    let version = summary.version;

    let cid = new_cid();
    let author = match identity.member_name() {
        Some(name) => name.to_string(),
        None => body
            .author
            .as_deref()
            .and_then(|s| if s.is_empty() { None } else { Some(s) })
            .unwrap_or(&state.actor)
            .to_string(),
    };
    let anchor = body.anchor.unwrap_or_else(|| "{}".into());
    let resolution_target = body.resolution_target.unwrap_or_default();

    let reviews_path = art_dir.join("reviews.org");
    let current_reviews = std::fs::read_to_string(&reviews_path).unwrap_or_default();
    let new_reviews = append_comment(
        &current_reviews,
        &art_id,
        &NewComment {
            cid: &cid,
            author: &author,
            version,
            anchor: &anchor,
            resolution_target: &resolution_target,
            message: &body.message,
        },
    );

    let now = Utc::now();
    let time_str = now.format("[%Y-%m-%d %a %H:%M:%S]").to_string();
    let project_date = now.format("%Y%m%d").to_string();
    let month_str = now.format("%Y-%m").to_string();
    let tx_path = project_root
        .join(".orgasmic")
        .join("tx")
        .join(format!("{month_str}.org"));

    let mut tx_entry = orgasmic_core::tx::TxEntry::new(
        "pending",
        "artifact.comment.added",
        &time_str,
        &state.actor,
        &state.machine,
    );
    tx_entry.project = Some(project_id.clone());
    tx_entry.extra = vec![
        ("ARTIFACT_ID".into(), art_id.clone()),
        ("CID".into(), cid.clone()),
        ("AUTHOR".into(), author),
        ("VERSION".into(), version.to_string()),
        ("ANCHOR".into(), anchor),
        ("RESOLUTION_TARGET".into(), resolution_target),
    ];

    // Single writer.transaction bundling the reviews.org rewrite + the tx
    // append (matching submit): either both land or neither does.
    state
        .writer
        .transaction(
            vec![FileRewrite {
                path: reviews_path,
                new_contents: new_reviews.into_bytes(),
            }],
            TxAppend {
                tx_path,
                entry: tx_entry,
                project_id: Some(project_id.clone()),
                tx_id_policy: TxIdPolicy::ProjectSequence {
                    project_id: project_id.clone(),
                    date: project_date,
                },
                request_id: None,
            },
        )
        .await
        .map_err(|e| ApiError::internal(format!("write artifact comment: {e}")))?;

    let _ = state.index.refresh_project(&project_id).await;
    state.events.publish(
        Topic::Artifact,
        EventPayload::ArtifactCommentAdded {
            project_id,
            artifact_id: art_id,
            cid: cid.clone(),
        },
    );
    Ok(Json(json!({ "cid": cid })))
}

#[derive(Debug, Deserialize)]
struct ArtifactCommentResolveRequest {
    /// Members toggle both ways (open<->resolved); omitted defaults to the
    /// common "mark resolved" case.
    #[serde(default = "default_true")]
    resolved: bool,
}

fn default_true() -> bool {
    true
}

/// `POST /artifacts/:id/comments/:cid/resolve` — the people-facing axis of
/// two-axis thread state (dec_V44E4/dec_KF2MR): any member with
/// `artifacts.comment` may open/resolve a thread. Consumed is a separate
/// agent-facing axis, touched only by regeneration close-out
/// ([`consume_all_open_comments`]), never by this route.
async fn post_artifact_comment_resolve(
    State(state): State<ApiState>,
    Extension(identity): Extension<Identity>,
    Path((art_id, cid)): Path<(String, String)>,
    Query(q): Query<ArtifactQuery>,
    Json(body): Json<ArtifactCommentResolveRequest>,
) -> Result<Json<Value>, ApiError> {
    require_valid_art_id(&art_id)?;
    let snap = state.index.snapshot().await;
    let entry = resolve_artifact_project(&snap, q.project.as_deref())?.clone();
    authz::require(&identity, Some(&entry.id), Action::ArtifactsComment)?;
    let project_id = entry.id.clone();
    let project_root = entry.path.clone();
    drop(snap);

    let art_dir = artifact_dir(&project_root, &art_id);
    if !art_dir.join("artifact.org").exists() {
        return Err(ApiError::not_found("artifact not found"));
    }

    // Serialize the reviews.org read→transaction window against a concurrent
    // add/consume on the same artifact (finding M1). Held until return.
    let write_lock = state.artifact_write_lock(&art_dir);
    let _write_guard = write_lock.lock().await;

    let reviews_path = art_dir.join("reviews.org");
    let current_reviews = std::fs::read_to_string(&reviews_path).unwrap_or_default();
    let new_reviews = artifacts::set_comment_resolved(&current_reviews, &cid, body.resolved)
        .map_err(|e| {
            if e.to_string().contains("not found") {
                ApiError::not_found(format!("comment {cid} not found"))
            } else {
                ApiError::internal(format!("resolve comment: {e}"))
            }
        })?;

    // The WS event should carry the artifact's actual current state:
    // resolving only touches reviews.org, never artifact.org's :STATE:.
    let actual_state = load_artifact(&art_dir)
        .map(|s| s.state)
        .unwrap_or_else(|| "submitted".to_string());

    let now = Utc::now();
    let time_str = now.format("[%Y-%m-%d %a %H:%M:%S]").to_string();
    let project_date = now.format("%Y%m%d").to_string();
    let month_str = now.format("%Y-%m").to_string();
    let tx_path = project_root
        .join(".orgasmic")
        .join("tx")
        .join(format!("{month_str}.org"));

    let resolved_by = identity.member_name().unwrap_or(&state.actor).to_string();
    let mut tx_entry = orgasmic_core::tx::TxEntry::new(
        "pending",
        "artifact.comment.resolved",
        &time_str,
        &state.actor,
        &state.machine,
    );
    tx_entry.project = Some(project_id.clone());
    tx_entry.extra = vec![
        ("ARTIFACT_ID".into(), art_id.clone()),
        ("CID".into(), cid.clone()),
        ("RESOLVED".into(), body.resolved.to_string()),
        ("RESOLVED_BY".into(), resolved_by),
    ];

    // Single writer.transaction bundling the reviews.org rewrite + the tx
    // append (matching submit): either both land or neither does.
    state
        .writer
        .transaction(
            vec![FileRewrite {
                path: reviews_path,
                new_contents: new_reviews,
            }],
            TxAppend {
                tx_path,
                entry: tx_entry,
                project_id: Some(project_id.clone()),
                tx_id_policy: TxIdPolicy::ProjectSequence {
                    project_id: project_id.clone(),
                    date: project_date,
                },
                request_id: None,
            },
        )
        .await
        .map_err(|e| ApiError::internal(format!("write comment resolution: {e}")))?;

    let _ = state.index.refresh_project(&project_id).await;
    state.events.publish(
        Topic::Artifact,
        EventPayload::ArtifactChanged {
            project_id,
            artifact_id: art_id,
            state: actual_state,
        },
    );
    Ok(Json(json!({ "cid": cid, "resolved": body.resolved })))
}

// ---- artifact generation launch (arch_045Q0 / dec_JBWB9 / dec_V44E4) -------

#[derive(Debug, Deserialize)]
struct ArtifactGenerateRequest {
    nodes: Vec<String>,
    prompt: String,
}

#[derive(Debug, Deserialize)]
struct ArtifactRegenerateRequest {
    #[serde(default, rename = "extraPrompt")]
    extra_prompt: Option<String>,
}

#[derive(Debug, Serialize)]
struct ArtifactGenerateResponse {
    artifact_id: String,
    run_id: String,
}

/// Truncate a free-form prompt down to a single-line default title. Generate
/// has no dedicated title field (nodes + prompt only, per the route
/// contract); the artifact's own :TITLE: is cosmetic index metadata, so a
/// clipped first line of the prompt is a reasonable default.
fn derive_artifact_title(prompt: &str) -> String {
    const MAX_CHARS: usize = 80;
    let collapsed: String = prompt.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= MAX_CHARS {
        return collapsed;
    }
    let truncated: String = collapsed.chars().take(MAX_CHARS).collect();
    format!("{}…", truncated.trim_end())
}

/// Subject nodes' Org content plus their graph neighborhood — the launch-time
/// context every artifact-generator run receives regardless of generate vs
/// regenerate (dec_JBWB9).
async fn assemble_artifact_context(
    state: &ApiState,
    project_id: &str,
    node_ids: &[String],
) -> String {
    let mut out = String::new();
    for id in node_ids {
        let layer = NodeLayer::for_id(id);
        out.push_str(&format!("### {id}\n"));
        match org_node_path(state, Some(project_id), id, layer).await {
            Ok((_, path, source_file)) => match read_artifact(&path, layer.artifact_name()) {
                Ok(source) => match OrgFile::parse(source, path.to_string_lossy()) {
                    Ok(file) => match file.find_by_id(id) {
                        Some(heading) => {
                            let doc = org_node_doc(&file, heading, layer, source_file);
                            out.push_str(&format!("[{}] {}\n", doc.kind, doc.title));
                            if !doc.body.trim().is_empty() {
                                out.push_str(doc.body.trim());
                                out.push('\n');
                            }
                            for section in &doc.sections {
                                if section.body.trim().is_empty() {
                                    continue;
                                }
                                out.push_str(&format!(
                                    "**{}**\n{}\n",
                                    section.title,
                                    section.body.trim()
                                ));
                            }
                        }
                        None => out.push_str(&format!("(not found in {source_file})\n")),
                    },
                    Err(e) => out.push_str(&format!("(parse error reading {source_file}: {e})\n")),
                },
                Err(_) => out.push_str(&format!("(unreadable: {source_file})\n")),
            },
            Err(_) => out.push_str("(node lookup failed)\n"),
        }
        out.push('\n');
    }

    let snap = state.index.snapshot().await;
    if let Ok(graph) = select_graph(&snap, Some(project_id)) {
        let mut lines: Vec<String> = graph
            .edges
            .iter()
            .filter(|e| node_ids.iter().any(|id| id == &e.from || id == &e.to))
            .map(|e| format!("- {} --{}--> {}", e.from, e.kind, e.to))
            .collect();
        lines.sort();
        lines.dedup();
        if !lines.is_empty() {
            out.push_str("### Graph neighborhood\n");
            out.push_str(&lines.join("\n"));
            out.push('\n');
        }
    }
    out
}

/// Regenerate-only context: the prior artifact.mdx, every current-version
/// comment (author, resolution target, anchor) that this regenerate closes
/// out, and the optional extra prompt from the Regenerate dialog (dec_V44E4).
fn assemble_regen_context(
    prior_mdx: &str,
    comments: &[artifacts::CommentRecord],
    extra_prompt: &str,
) -> String {
    let mut out = String::new();
    out.push_str("### Prior artifact.mdx\n");
    if prior_mdx.trim().is_empty() {
        out.push_str("(none)\n");
    } else {
        out.push_str(prior_mdx.trim());
        out.push('\n');
    }
    out.push_str("\n### Current-version comments (closed out by this regenerate)\n");
    if comments.is_empty() {
        out.push_str("(none)\n");
    } else {
        for c in comments {
            let resolves = if c.resolution_target.trim().is_empty() {
                "-"
            } else {
                c.resolution_target.as_str()
            };
            out.push_str(&format!(
                "- [{}] {} (anchor: {}; resolves: {}; resolved: {}): {}\n",
                c.cid, c.author, c.anchor, resolves, c.resolved, c.message
            ));
        }
    }
    out.push_str("\n### Extra prompt\n");
    out.push_str(&prompt_value_or_not_set(extra_prompt));
    out.push('\n');
    out
}

/// Compile the artifact-generator prompt spec into the worker's initial
/// prompt bundle. Standalone from `compile_dispatch_prompt_bundle`: this run
/// has no task heading, so only the `artifact.*` slot namespace is filled.
fn compile_artifact_generate_prompt_bundle(
    home: &Home,
    project_id: &str,
    art_id: &str,
    subject_context: &str,
    user_prompt: &str,
    regen_context: &str,
) -> Result<String, ApiError> {
    let mut values = SlotValues::new();
    values.insert(
        "artifact.subject_nodes".to_string(),
        prompt_value_or_not_set(subject_context),
    );
    values.insert(
        "artifact.user_prompt".to_string(),
        prompt_value_or_not_set(user_prompt),
    );
    values.insert(
        "artifact.regen_context".to_string(),
        prompt_value_or_not_set(regen_context),
    );
    let req = crate::prompt_compiler::PromptCompileRequest {
        project: Some(project_id.to_string()),
        mode: Some("artifact_generate".to_string()),
        worker: Some("artifactor".to_string()),
        harness: None,
        renderer: None,
        reason: Some(format!("artifact generation for {art_id}")),
        context_overrides: BTreeMap::new(),
        values,
    };
    let compiled = crate::prompt_compiler::compile_prompt_spec(home, "artifact-generator", req)
        .map_err(|e| content_list_error(e, "artifact-generator prompt spec"))?;
    if crate::prompt_compiler::has_error(&compiled.diagnostics) {
        let messages = compiled
            .diagnostics
            .iter()
            .filter(|diag| diag.level == "error")
            .map(|diag| diag.message.as_str())
            .collect::<Vec<_>>()
            .join("; ");
        return Err(ApiError::internal(format!(
            "artifact-generator prompt compile failed: {messages}"
        )));
    }
    Ok(format!(
        "orgasmic compiled prompt\ndispatch_kind: artifactor\nartifact: {art_id}\nworker: artifactor\nprompt_spec: {}\n\n{}\n",
        compiled.spec.id,
        compiled.text.trim()
    ))
}

/// Acquire the per-artifact lease `artifact.generate:{art_id}` and launch the
/// artifactor worker, modeled on `post_manager_launch`'s direct
/// `supervisor.acquire` pattern (synthetic task_id, `RunKind::Worker`, no
/// task heading) but reusing `spawn_worker_run`'s shared plumbing so the run
/// gets a real compiled-prompt bundle, session file, and (if configured)
/// babysitter, exactly like a CLI dispatch.
///
/// A second generate/regenerate on the same artifact while one runs is
/// refused by the lease itself (`spawn_worker_run` maps
/// `SupervisorError::LeaseHeld` to 409).
///
/// Performs no durable mutation and no revert on failure — every caller must
/// either (a) mutate the artifact only *after* this returns `Ok` (regenerate:
/// nothing to undo on failure, since nothing changed yet), or (b) already
/// hold a durable record it is prepared to revert itself on a non-409 error
/// (generate: a fresh record with no other caller possibly racing it). A
/// live LLM-driven probe against a real daemon (TASK-2ZQSB) caught the
/// opposite ordering appending a spurious `artifact.regenerated` tx entry —
/// and silently consuming real comments — for a regenerate call that the
/// lease went on to refuse.
async fn launch_artifact_generation(
    state: &ApiState,
    entry: &BoardEntry,
    art_id: &str,
    art_dir: &FsPath,
    restore_state: &'static str,
    restore_version: u32,
    bundle: String,
) -> Result<String, ApiError> {
    let worker = load_stage_worker(&state.home, "artifactor")?;

    let task_id = format!("artifact.generate:{art_id}");
    let spawn = spawn_worker_run(
        state,
        SpawnWorkerRequest {
            project_id: &entry.id,
            task_id: &task_id,
            worker_id: "artifactor",
            run_kind: RunKind::Worker,
            bundle: &bundle,
            overrides: DriverOverrides::default(),
            project_root_path: &entry.path,
            worktree_path: &entry.path,
            last_path: None,
            stdout_path: None,
            origin: "artifact_generate",
            dispatch_kind: Some("artifactor"),
            preloaded_worker: Some(worker),
            task_sandbox_permissions: None,
        },
    )
    .await
    .map_err(|failure| failure.error)?;

    spawn_artifact_release_watcher(
        state.clone(),
        ArtifactReleaseWatch {
            entry: entry.clone(),
            task_id,
            art_dir: art_dir.to_path_buf(),
            art_id: art_id.to_string(),
            run_id: spawn.acquire.run_id.clone(),
            restore_state,
            restore_version,
        },
    );
    Ok(spawn.acquire.run_id)
}

/// Best-effort recovery: restore the artifact out of `regenerating` when a
/// launch never happened or the run ended without ever calling `orgasmic
/// artifact submit`. Only acts if the artifact is still `regenerating` —
/// a completed submit, or a newer run already holding the lease, means
/// someone else already resolved it, and clobbering that would be the bug
/// this function exists to prevent.
struct RevertArtifactState<'a> {
    entry: &'a BoardEntry,
    art_dir: &'a FsPath,
    art_id: &'a str,
    run_id: &'a str,
    reason: &'a str,
    restore_state: &'a str,
    restore_version: u32,
}

async fn revert_artifact_generation_state(state: &ApiState, revert: RevertArtifactState<'_>) {
    let RevertArtifactState {
        entry,
        art_dir,
        art_id,
        run_id,
        reason,
        restore_state,
        restore_version,
    } = revert;
    let write_lock = state.artifact_write_lock(art_dir);
    let _guard = write_lock.lock().await;

    let Ok(current_org) = std::fs::read_to_string(art_dir.join("artifact.org")) else {
        return;
    };
    let Some(summary) = load_artifact(art_dir) else {
        return;
    };
    if summary.state != "regenerating" {
        return;
    }
    // For established hot sessions, revert to the last submitted version — not
    // the frozen (failed, 0) target captured at first generate time.
    let (target_state, target_version) = if summary.version > 0 {
        ("submitted", summary.version)
    } else {
        (restore_state, restore_version)
    };
    let Ok(new_org) = update_artifact_org(&current_org, target_version, target_state) else {
        return;
    };

    let now = Utc::now();
    let time_str = now.format("[%Y-%m-%d %a %H:%M:%S]").to_string();
    let project_date = now.format("%Y%m%d").to_string();
    let month_str = now.format("%Y-%m").to_string();
    let tx_path = entry
        .path
        .join(".orgasmic")
        .join("tx")
        .join(format!("{month_str}.org"));

    let mut tx_entry = orgasmic_core::tx::TxEntry::new(
        "pending",
        "artifact.generation.failed",
        &time_str,
        &state.actor,
        &state.machine,
    );
    tx_entry.project = Some(entry.id.clone());
    tx_entry.extra = vec![
        ("ARTIFACT_ID".into(), art_id.to_string()),
        ("RUN_ID".into(), run_id.to_string()),
        ("RESTORED_STATE".into(), target_state.to_string()),
        ("REASON".into(), reason.to_string()),
    ];

    let result = state
        .writer
        .transaction(
            vec![FileRewrite {
                path: art_dir.join("artifact.org"),
                new_contents: new_org,
            }],
            TxAppend {
                tx_path,
                entry: tx_entry,
                project_id: Some(entry.id.clone()),
                tx_id_policy: TxIdPolicy::ProjectSequence {
                    project_id: entry.id.clone(),
                    date: project_date,
                },
                request_id: None,
            },
        )
        .await;

    if let Err(e) = result {
        tracing::error!(art_id, run_id, error = %e, "artifact generation-state revert failed");
        return;
    }

    let _ = state.index.refresh_project(&entry.id).await;
    state.events.publish(
        Topic::Artifact,
        EventPayload::ArtifactChanged {
            project_id: entry.id.clone(),
            artifact_id: art_id.to_string(),
            state: target_state.to_string(),
        },
    );
}

struct ArtifactRegenerateCloseOut<'a> {
    state: &'a ApiState,
    entry: &'a BoardEntry,
    art_dir: &'a FsPath,
    art_id: &'a str,
    run_id: &'a str,
    version: u32,
    extra_prompt: &'a str,
}

/// Version-N close-out shared by cold and hot regenerate paths: consume open
/// comments, flip artifact.org to `regenerating`, append `artifact.regenerated`.
async fn close_out_artifact_regenerate_round(
    close: ArtifactRegenerateCloseOut<'_>,
) -> Result<(), ApiError> {
    let ArtifactRegenerateCloseOut {
        state,
        entry,
        art_dir,
        art_id,
        run_id,
        version,
        extra_prompt,
    } = close;
    let write_lock = state.artifact_write_lock(art_dir);
    let _write_guard = write_lock.lock().await;

    let reviews_path = art_dir.join("reviews.org");
    let current_reviews = std::fs::read_to_string(&reviews_path).unwrap_or_default();
    let consumed_cids: Vec<String> = artifacts::parse_comments(&current_reviews)
        .into_iter()
        .filter(|c| !(c.resolved && c.consumed))
        .map(|c| c.cid)
        .collect();
    let new_reviews = artifacts::consume_all_open_comments(&current_reviews)
        .map_err(|e| ApiError::internal(format!("consume open comments: {e}")))?;

    let current_org = std::fs::read_to_string(art_dir.join("artifact.org"))
        .map_err(|e| ApiError::internal(format!("read artifact.org: {e}")))?;
    let new_org = update_artifact_org(&current_org, version, "regenerating")
        .map_err(|e| ApiError::internal(format!("update artifact.org: {e}")))?;

    let now = Utc::now();
    let time_str = now.format("[%Y-%m-%d %a %H:%M:%S]").to_string();
    let project_date = now.format("%Y%m%d").to_string();
    let month_str = now.format("%Y-%m").to_string();
    let tx_path = entry
        .path
        .join(".orgasmic")
        .join("tx")
        .join(format!("{month_str}.org"));

    let mut regen_entry = orgasmic_core::tx::TxEntry::new(
        "pending",
        "artifact.regenerated",
        &time_str,
        &state.actor,
        &state.machine,
    );
    regen_entry.project = Some(entry.id.clone());
    regen_entry.extra = vec![
        ("ARTIFACT_ID".into(), art_id.to_string()),
        ("RUN_ID".into(), run_id.to_string()),
        ("VERSION".into(), version.to_string()),
        ("CONSUMED_CIDS".into(), consumed_cids.join(" ")),
        (
            "EXTRA_PROMPT".into(),
            artifacts::escape_single_line(extra_prompt),
        ),
    ];

    state
        .writer
        .transaction(
            vec![
                FileRewrite {
                    path: art_dir.join("artifact.org"),
                    new_contents: new_org,
                },
                FileRewrite {
                    path: reviews_path,
                    new_contents: new_reviews,
                },
            ],
            TxAppend {
                tx_path,
                entry: regen_entry,
                project_id: Some(entry.id.clone()),
                tx_id_policy: TxIdPolicy::ProjectSequence {
                    project_id: entry.id.clone(),
                    date: project_date,
                },
                request_id: None,
            },
        )
        .await
        .map_err(|e| ApiError::internal(format!("write regenerate state: {e}")))?;

    let _ = state.index.refresh_project(&entry.id).await;
    state.events.publish(
        Topic::Artifact,
        EventPayload::ArtifactChanged {
            project_id: entry.id.clone(),
            artifact_id: art_id.to_string(),
            state: "regenerating".into(),
        },
    );
    Ok(())
}

struct ArtifactReleaseWatch {
    entry: BoardEntry,
    /// The supervisor lease key (`artifact.generate:{art_id}`); used to
    /// detect whether a newer run already took over the artifact by the time
    /// this run ends.
    task_id: String,
    art_dir: PathBuf,
    art_id: String,
    run_id: String,
    restore_state: &'static str,
    restore_version: u32,
}

/// Poll until the launched run leaves the supervisor's live set, then revert
/// a still-`regenerating` artifact. Mirrors `spawn_dispatch_completion_watcher`'s
/// polling shape; simpler, since there is no CLI-side last_path/stdout_path
/// to flush here — only the store's own state needs a safety net.
fn spawn_artifact_release_watcher(state: ApiState, watch: ArtifactReleaseWatch) {
    tokio::spawn(async move {
        let poll = std::time::Duration::from_millis(250);
        loop {
            let snapshot = state.supervisor.snapshot().await;
            let still_live = snapshot.runs.iter().any(|run| run.run_id == watch.run_id);
            if still_live {
                tokio::time::sleep(poll).await;
                continue;
            }
            // A fresh run already holds this artifact's lease (a new
            // generate/regenerate started after this one released) — leave
            // its regenerating state alone.
            let superseded = snapshot.runs.iter().any(|run| run.task_id == watch.task_id);
            if !superseded {
                revert_artifact_generation_state(
                    &state,
                    RevertArtifactState {
                        entry: &watch.entry,
                        art_dir: &watch.art_dir,
                        art_id: &watch.art_id,
                        run_id: &watch.run_id,
                        reason: "run ended without a submit",
                        restore_state: watch.restore_state,
                        restore_version: watch.restore_version,
                    },
                )
                .await;
            }
            return;
        }
    });
}

async fn post_artifact_generate(
    State(state): State<ApiState>,
    Extension(identity): Extension<Identity>,
    Query(q): Query<ArtifactQuery>,
    Json(body): Json<ArtifactGenerateRequest>,
) -> Result<Json<ArtifactGenerateResponse>, ApiError> {
    let nodes: Vec<String> = body
        .nodes
        .into_iter()
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty())
        .collect();
    let prompt = body.prompt.trim().to_string();
    if prompt.is_empty() {
        return Err(ApiError::bad_request("prompt is required"));
    }

    let snap = state.index.snapshot().await;
    let entry = resolve_artifact_project(&snap, q.project.as_deref())?.clone();
    authz::require(&identity, Some(&entry.id), Action::ArtifactsGenerate)?;
    drop(snap);

    let art_id = artifacts::new_artifact_id();
    let art_dir = artifact_dir(&entry.path, &art_id);
    let title = artifacts::escape_single_line(&derive_artifact_title(&prompt));
    let prompt_escaped = artifacts::escape_single_line(&prompt);

    let org_content =
        artifact_org_content(&art_id, &title, &nodes, &prompt_escaped, 0, "regenerating");
    let reviews_header = reviews_org_header(&art_id);

    let now = Utc::now();
    let time_str = now.format("[%Y-%m-%d %a %H:%M:%S]").to_string();
    let project_date = now.format("%Y%m%d").to_string();
    let month_str = now.format("%Y-%m").to_string();
    let tx_path = entry
        .path
        .join(".orgasmic")
        .join("tx")
        .join(format!("{month_str}.org"));

    let mut created_entry = orgasmic_core::tx::TxEntry::new(
        "pending",
        "artifact.created",
        &time_str,
        &state.actor,
        &state.machine,
    );
    created_entry.project = Some(entry.id.clone());
    created_entry.extra = vec![
        ("ARTIFACT_ID".into(), art_id.clone()),
        ("TITLE".into(), title.clone()),
        ("SUBJECT_NODES".into(), nodes.join(" ")),
        ("PROMPT".into(), prompt_escaped.clone()),
    ];

    state
        .writer
        .transaction(
            vec![
                FileRewrite {
                    path: art_dir.join("artifact.org"),
                    new_contents: org_content.into_bytes(),
                },
                FileRewrite {
                    path: art_dir.join("reviews.org"),
                    new_contents: reviews_header.into_bytes(),
                },
            ],
            TxAppend {
                tx_path,
                entry: created_entry,
                project_id: Some(entry.id.clone()),
                tx_id_policy: TxIdPolicy::ProjectSequence {
                    project_id: entry.id.clone(),
                    date: project_date,
                },
                request_id: Some(format!("artifact-created-{}-{art_id}", entry.id)),
            },
        )
        .await
        .map_err(|e| ApiError::internal(format!("write artifact record: {e}")))?;

    let _ = state.index.refresh_project(&entry.id).await;
    state.events.publish(
        Topic::Artifact,
        EventPayload::ArtifactChanged {
            project_id: entry.id.clone(),
            artifact_id: art_id.clone(),
            state: "regenerating".into(),
        },
    );

    let context = assemble_artifact_context(&state, &entry.id, &nodes).await;
    let bundle = compile_artifact_generate_prompt_bundle(
        &state.home,
        &entry.id,
        &art_id,
        &context,
        &prompt,
        "",
    )?;

    // The durable record above always exists before this call and no other
    // caller can reference this freshly minted id, so any launch failure
    // here (there is no 409 case: the lease key is brand new) must revert it
    // — unlike regenerate, which mutates only after a successful launch.
    let run_id =
        match launch_artifact_generation(&state, &entry, &art_id, &art_dir, "failed", 0, bundle)
            .await
        {
            Ok(run_id) => run_id,
            Err(error) => {
                revert_artifact_generation_state(
                    &state,
                    RevertArtifactState {
                        entry: &entry,
                        art_dir: &art_dir,
                        art_id: &art_id,
                        run_id: "no-run",
                        reason: "worker launch failed",
                        restore_state: "failed",
                        restore_version: 0,
                    },
                )
                .await;
                return Err(error);
            }
        };

    Ok(Json(ArtifactGenerateResponse {
        artifact_id: art_id,
        run_id,
    }))
}

async fn post_artifact_regenerate(
    State(state): State<ApiState>,
    Extension(identity): Extension<Identity>,
    Path(art_id): Path<String>,
    Query(q): Query<ArtifactQuery>,
    Json(body): Json<ArtifactRegenerateRequest>,
) -> Result<Json<ArtifactGenerateResponse>, ApiError> {
    require_valid_art_id(&art_id)?;
    let snap = state.index.snapshot().await;
    let entry = resolve_artifact_project(&snap, q.project.as_deref())?.clone();
    authz::require(&identity, Some(&entry.id), Action::ArtifactsGenerate)?;
    drop(snap);

    let art_dir = artifact_dir(&entry.path, &art_id);
    if !art_dir.join("artifact.org").exists() {
        return Err(ApiError::not_found("artifact not found"));
    }

    let extra_prompt = body.extra_prompt.unwrap_or_default();

    // Read-only: feeds the launch/followup prompt. This is NOT yet the final
    // "what gets closed out" comment set — that is re-read and applied only
    // after the round is committed (lease acquire or accepted send_input).
    let detail = load_artifact_detail(&art_dir, None, false).map_err(|e| match e {
        ArtifactLoadError::NotFound => ApiError::not_found("artifact not found"),
        ArtifactLoadError::VersionNotFound(v) => {
            ApiError::not_found(format!("artifact {art_id} has no version {v}"))
        }
    })?;
    let version = detail.summary.version;
    let nodes = detail.summary.subject_nodes.clone();

    let artifact_task_id = format!("artifact.generate:{art_id}");
    let snapshot = state.supervisor.snapshot().await;
    let live = snapshot.runs.iter().find(|r| r.task_id == artifact_task_id);

    if let Some(live_run) = live {
        // HOT PATH: route the followup into the live session — no new lease.
        let followup_payload =
            assemble_regen_context(&detail.content, &detail.comments, &extra_prompt);
        let ack = state
            .supervisor
            .send_input(&live_run.run_id, followup_payload, &live_run.identity)
            .await
            .map_err(|e| match e {
                crate::supervisor::SupervisorError::RunNotFound(_) => {
                    ApiError::not_found(format!("active run {}", live_run.run_id))
                }
                other => supervisor_control_error("artifact regenerate followup", other),
            })?;
        if !ack.accepted {
            return Err(ApiError::conflict(
                ack.message.unwrap_or_else(|| "harness busy".to_string()),
            ));
        }
        close_out_artifact_regenerate_round(ArtifactRegenerateCloseOut {
            state: &state,
            entry: &entry,
            art_dir: &art_dir,
            art_id: &art_id,
            run_id: &live_run.run_id,
            version,
            extra_prompt: &extra_prompt,
        })
        .await?;
        return Ok(Json(ArtifactGenerateResponse {
            artifact_id: art_id,
            run_id: live_run.run_id.clone(),
        }));
    }

    // COLD PATH: no live holder — spawn a fresh run (also post-restart).
    let subject_context = assemble_artifact_context(&state, &entry.id, &nodes).await;
    let regen_context = assemble_regen_context(&detail.content, &detail.comments, &extra_prompt);
    let bundle = compile_artifact_generate_prompt_bundle(
        &state.home,
        &entry.id,
        &art_id,
        &subject_context,
        &detail.prompt,
        &regen_context,
    )?;

    let run_id = launch_artifact_generation(
        &state,
        &entry,
        &art_id,
        &art_dir,
        "submitted",
        version,
        bundle,
    )
    .await?;

    close_out_artifact_regenerate_round(ArtifactRegenerateCloseOut {
        state: &state,
        entry: &entry,
        art_dir: &art_dir,
        art_id: &art_id,
        run_id: &run_id,
        version,
        extra_prompt: &extra_prompt,
    })
    .await?;

    Ok(Json(ArtifactGenerateResponse {
        artifact_id: art_id,
        run_id,
    }))
}

// ---- errors ----------------------------------------------------------------

/// HTTP error returned by daemon routes.
///
/// Wire envelope (stable; consumed by `ui/src/lib/transport.ts`): a JSON object
/// with a single string field `error` holding the public message, e.g.
/// `{"error":"task not found"}`. No other top-level fields are emitted (`code`,
/// `details`, `trace_id`, etc.). Unsafe details (paths, I/O errors, driver
/// internals) are logged via `tracing` and never included in the body.
#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    message: String,
    /// Optional structured body. When present it replaces the default
    /// `{"error": message}` shape (used for recovery lease conflicts that
    /// carry actionable data, dec_052).
    body: Option<Value>,
}

impl ApiError {
    fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: message.into(),
            body: None,
        }
    }
    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
            body: None,
        }
    }
    fn conflict(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::CONFLICT,
            message: message.into(),
            body: None,
        }
    }
    /// Conflict with a structured body. The body should carry enough data for
    /// the UI to render follow-up actions (e.g. active run id, choices).
    fn conflict_json(body: Value) -> Self {
        let message = body
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("conflict")
            .to_string();
        Self {
            status: StatusCode::CONFLICT,
            message,
            body: Some(body),
        }
    }
    fn internal(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: message.into(),
            body: None,
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = self.body.unwrap_or_else(|| json!({"error": self.message}));
        (self.status, Json(body)).into_response()
    }
}

/// The one authorization seam's error converts to a plain 403 — route code
/// never builds its own forbidden response, it just propagates `?` through
/// `authz::require`.
impl From<authz::Forbidden> for ApiError {
    fn from(err: authz::Forbidden) -> Self {
        ApiError {
            status: StatusCode::FORBIDDEN,
            message: err.0,
            body: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use futures::{SinkExt as _, StreamExt as _};
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::tungstenite::Message;

    type TestWs = tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >;

    /// Serialize real-tmux/rmux tests across ALL test binaries: they spawn real
    /// mux daemons and contend under `cargo test --workspace` (TASK-X0ZVE). An
    /// advisory flock on a shared temp path lets at most one run at a time,
    /// cross-process. Held for the whole test via the returned guard.
    fn live_session_guard() -> LiveSessionGuard {
        let path = std::env::temp_dir().join("orgasmic-live-session-tests.lock");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .expect("open live-session lock file");
        // MSRV 1.87: call fs2 explicitly — std's File::lock_exclusive (1.89) shadows it.
        fs2::FileExt::lock_exclusive(&file).expect("flock live-session lock");
        LiveSessionGuard(file)
    }

    struct LiveSessionGuard(std::fs::File);
    impl Drop for LiveSessionGuard {
        fn drop(&mut self) {
            let _ = fs2::FileExt::unlock(&self.0);
        }
    }

    fn write(path: PathBuf, contents: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, contents).unwrap();
    }

    fn glossary_create_request(definition: &str, allow_marker: bool) -> GraphCreateRequest {
        let mut properties = BTreeMap::new();
        properties.insert("DEFINITION".to_string(), definition.to_string());
        GraphCreateRequest {
            project: None,
            request_id: None,
            id: None,
            title: None,
            properties,
            body: None,
            force: false,
            allow_marker,
        }
    }

    #[test]
    fn glossary_marker_guard_allows_ordinary_english_word_fragments() {
        let ok = [
            "A construct holding related fields.",
            "The reconstruction of the index happens on boot.",
            "Enumerate every project before syncing.",
            "This is a simple implementation of the pattern.",
        ];
        for definition in ok {
            assert!(
                reject_glossary_implementation_detail(&glossary_create_request(definition, false))
                    .is_ok(),
                "expected {definition:?} to pass the marker guard"
            );
        }
    }

    #[test]
    fn glossary_marker_guard_rejects_real_markers() {
        let bad = [
            "struct Foo { field: bool }",
            "impl Foo for Bar {}",
            "enum Color { Red, Blue }",
            "trait Greeter { fn greet(&self); }",
            "Calls crates/orgasmic-core/src/schema.rs directly.",
            "See orgasmic_core::schema for the type.",
        ];
        for definition in bad {
            assert!(
                reject_glossary_implementation_detail(&glossary_create_request(definition, false))
                    .is_err(),
                "expected {definition:?} to be rejected by the marker guard"
            );
        }
    }

    #[test]
    fn glossary_marker_guard_allow_marker_overrides() {
        let req = glossary_create_request("struct Foo { field: bool }", true);
        assert!(reject_glossary_implementation_detail(&req).is_ok());
    }

    #[test]
    fn app_spa_fallback_accepts_html_navigation_only() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::ACCEPT,
            HeaderValue::from_static("text/html,application/xhtml+xml,*/*;q=0.8"),
        );
        assert!(accepts_html(&headers));

        headers.insert(header::ACCEPT, HeaderValue::from_static("*/*"));
        assert!(!accepts_html(&headers));
    }

    #[tokio::test]
    async fn root_spa_fallback_serves_deep_links() {
        let mut headers = HeaderMap::new();
        headers.insert(header::ACCEPT, HeaderValue::from_static("text/html"));

        let response = get_spa_asset(
            Uri::from_static("/projects/orgasmic/graph"),
            headers.clone(),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/html; charset=utf-8"
        );
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = std::str::from_utf8(&body).unwrap();
        assert!(
            body.contains("<div id=\"root\"></div>") || body.contains("placeholder UI"),
            "{body}"
        );

        let app_response = get_spa_asset(Uri::from_static("/app/"), headers.clone()).await;
        assert_eq!(app_response.status(), StatusCode::NOT_FOUND);

        headers.insert(header::ACCEPT, HeaderValue::from_static("application/json"));
        let api_shape_response = get_spa_asset(Uri::from_static("/projects"), headers).await;
        assert_eq!(api_shape_response.status(), StatusCode::NOT_FOUND);
    }

    fn dispatch_prompt_test_worker(renderer: &str) -> StageWorker {
        StageWorker {
            id: format!("implementer-{renderer}"),
            kind: WorkerKind::Implementer,
            driver: "subprocess-stream-json".to_string(),
            harness: renderer.to_string(),
            models: Vec::new(),
            reasoning_efforts: Vec::new(),
            default_provider: None,
            default_model: None,
            default_effort: None,
            linked_skills: Vec::new(),
            persona: None,
            operating_rules: None,
            missing_skills: Vec::new(),
            babysitter_worker: None,
            stall_timeout_secs: None,
            max_run_duration_secs: None,
            sandbox_permissions: None,
            harness_args: Vec::new(),
        }
    }

    fn count_occurrences(haystack: &str, needle: &str) -> usize {
        haystack.matches(needle).count()
    }

    #[test]
    fn tx_timestamp_helpers_use_utc_date_at_local_midnight_boundary() {
        let before_utc_midnight = "2026-06-10T21:10:00Z"
            .parse::<chrono::DateTime<Utc>>()
            .unwrap();

        assert_eq!(tx_project_date_utc(&before_utc_midnight), "20260610");
        assert_eq!(
            tx_preserve_id_timestamp_utc(&before_utc_midnight),
            "20260610211000"
        );
        assert_eq!(
            tx_time_string_utc(&before_utc_midnight),
            "[2026-06-10 Wed 21:10:00]"
        );
        assert_eq!(tx_file_month_utc(&before_utc_midnight), "2026-06");

        let after_utc_midnight = "2026-06-11T00:00:01Z"
            .parse::<chrono::DateTime<Utc>>()
            .unwrap();

        assert_eq!(tx_project_date_utc(&after_utc_midnight), "20260611");
        assert_eq!(
            tx_time_string_utc(&after_utc_midnight),
            "[2026-06-11 Thu 00:00:01]"
        );
    }

    #[test]
    fn dispatch_endpoint_kind_supports_architector() {
        let kind = DispatchEndpointKind::Architector;

        assert_eq!(kind.as_str(), "architector");
        assert_eq!(kind.worker_kind(), WorkerKind::Architector);
        assert_eq!(kind.run_kind(), RunKind::Worker);
        assert_eq!(
            DispatchEndpointKind::from_str("architector").unwrap(),
            DispatchEndpointKind::Architector
        );
        assert!(DispatchEndpointKind::from_str("nonexistent").is_err());
        assert_eq!(
            dispatch_expected_next(kind),
            "architector-watch-then-integrate"
        );
    }

    #[test]
    fn dispatch_prompt_compiler_hydrates_task_slots_and_renders_brief_once() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        symlink_repo_source(&home);
        let mut project = test_project(tmp.path(), &[], &[], &[]);
        let task_id = "TASK-FIX";
        let task_title = "Canonical fixture task title";
        let task_description = "CANONICAL-DESCRIPTION-ONLY";
        let acceptance_text = "CANONICAL-ACCEPTANCE-ONLY";
        let read_scope = "CANONICAL-READ-SCOPE-ONLY";
        let write_scope = "CANONICAL-WRITE-SCOPE-ONLY";
        let brief = "DISPATCH-BRIEF-ONLY";
        project.tasks.push(crate::index::TaskSummary {
            id: task_id.to_string(),
            title: task_title.to_string(),
            lifecycle_stage: LifecycleStage::InProgress,
            parent_task: None,
            depends_on: Vec::new(),
            implements: Vec::new(),
            produces: Vec::new(),
            read_scope: vec![read_scope.to_string()],
            write_scope: vec![write_scope.to_string()],
            owner: TaskOwner::Human,
            run_id: None,
            priority: Some("P1".to_string()),
            worker: None,
            provider: None,
            model: None,
            reasoning_effort: None,
            test_cmd: None,
            tags: Vec::new(),
            source_file: tmp.path().join(".orgasmic/tasks/in_progress.org"),
            sandbox_permissions: None,
        });
        project.task_bodies.insert(
            task_id.to_string(),
            crate::index::TaskBody {
                description: task_description.to_string(),
                acceptance_criteria: vec![crate::index::AcceptanceItem {
                    state: crate::index::AcceptanceState::Unchecked,
                    text: acceptance_text.to_string(),
                }],
                evidence: Vec::new(),
                notes: String::new(),
                worklog: Vec::new(),
                reviewer_pass: Vec::new(),
            },
        );
        project.activity_index.insert(
            task_id.to_string(),
            vec![crate::index::ActivityEntry {
                tx_id: "tx-dispatch-fixture".to_string(),
                time: "2026-06-12T00:00:00Z".to_string(),
                kind: crate::index::ActivityKind::Comment,
                actor: "tester".to_string(),
                body: "CANONICAL-ACTIVITY-ONLY".to_string(),
                artifacts: Vec::new(),
                in_reply_to: None,
            }],
        );

        for renderer in ["markdown", "xml"] {
            let worker = dispatch_prompt_test_worker(renderer);
            let compiled = compile_dispatch_prompt_bundle(
                &home,
                &project,
                DispatchEndpointKind::Implementer,
                task_id,
                &worker,
                brief,
            )
            .unwrap();

            assert!(
                compiled.contains(task_title),
                "{renderer} prompt should include canonical title"
            );
            assert!(
                compiled.contains(task_description),
                "{renderer} prompt should include canonical description"
            );
            assert!(
                compiled.contains(acceptance_text),
                "{renderer} prompt should include canonical acceptance"
            );
            assert!(
                compiled.contains(read_scope),
                "{renderer} prompt should include canonical read scope"
            );
            assert!(
                compiled.contains(write_scope),
                "{renderer} prompt should include canonical write scope"
            );
            assert!(
                compiled.contains("CANONICAL-ACTIVITY-ONLY"),
                "{renderer} prompt should include indexed activity"
            );
            assert_eq!(
                count_occurrences(&compiled, brief),
                1,
                "{renderer} prompt should render the manager dispatch brief exactly once"
            );
            assert!(
                compiled.contains("Dispatch Brief"),
                "{renderer} prompt should label the manager handoff section"
            );
            assert!(
                !compiled.contains("See dispatch brief."),
                "{renderer} prompt must not use scope placeholders"
            );
            assert!(
                !compiled.contains(task_description) || !compiled.contains("The task brief is"),
                "{renderer} prompt goal must not embed task.description"
            );
        }
    }

    fn test_project(
        project_root: &FsPath,
        decisions: &[&str],
        architecture: &[(&str, &str)],
        glossary: &[&str],
    ) -> crate::index::ProjectIndex {
        let mut graph = crate::index::GraphIndex::default();
        let decisions_file = project_root.join(".orgasmic/decisions.org");
        for id in decisions {
            graph.decisions.push(crate::index::DecisionSummary {
                id: (*id).to_string(),
                title: String::new(),
                tags: Vec::new(),
                parent: None,
                children: Vec::new(),
                depth: None,
                path: None,
                glossary_refs: Vec::new(),
                decided_at: None,
                preview: None,
                source_file: decisions_file.clone(),
                superseded: false,
            });
            graph.nodes.push(crate::index::GraphNodeSummary {
                id: (*id).to_string(),
                layer: "decision".to_string(),
                outgoing: Vec::new(),
                source_file: decisions_file.clone(),
                superseded: false,
            });
        }
        let architecture_file = project_root.join(".orgasmic/architecture.org");
        for (id, _status) in architecture {
            graph.architecture.push(crate::index::ArchitectureSummary {
                id: (*id).to_string(),
                label: String::new(),
                motivated_by: Vec::new(),
                glossary_refs: Vec::new(),
                interface: Vec::new(),
                constraints: Vec::new(),
                depends_on: Vec::new(),
                source_paths: Vec::new(),
                tests: Vec::new(),
                parent_id: None,
                description: None,
                source_file: architecture_file.clone(),
            });
            graph.nodes.push(crate::index::GraphNodeSummary {
                id: (*id).to_string(),
                layer: "architecture".to_string(),
                outgoing: Vec::new(),
                source_file: architecture_file.clone(),
                superseded: false,
            });
        }
        let glossary_file = project_root.join(".orgasmic/glossary.org");
        for id in glossary {
            graph.glossary.push(crate::index::GlossarySummary {
                id: (*id).to_string(),
                canonical: None,
                avoid: None,
                relates_to: Vec::new(),
                definition: None,
                source_file: glossary_file.clone(),
            });
            graph.nodes.push(crate::index::GraphNodeSummary {
                id: (*id).to_string(),
                layer: "glossary".to_string(),
                outgoing: Vec::new(),
                source_file: glossary_file.clone(),
                superseded: false,
            });
        }
        crate::index::ProjectIndex {
            project_id: "orgasmic".to_string(),
            root: project_root.to_path_buf(),
            repo_url: String::new(),
            branch: "main".to_string(),
            status: "active".to_string(),
            tasks: Vec::new(),
            task_bodies: BTreeMap::new(),
            subtasks: BTreeMap::new(),
            activity_index: BTreeMap::new(),
            worker_pipeline: Vec::new(),
            default_test_cmd: None,
            default_write_scope: Vec::new(),
            graph,
            markers: BTreeMap::new(),
            last_loaded_at: None,
            artifacts: Vec::new(),
        }
    }

    #[test]
    fn content_load_not_found_on_disk_maps_to_404() {
        let path = PathBuf::from("/tmp/orgasmic-race/workers/racy.org");
        let error = ContentLoadError::NotFoundOnDisk {
            path,
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "gone"),
        };

        let api_error = content_load_error(error, "worker");

        assert_eq!(api_error.status, StatusCode::NOT_FOUND);
        assert_eq!(api_error.message, "worker not found");
    }

    #[test]
    fn org_file_artifact_label_matches_allowed_artifacts() {
        assert_eq!(
            org_file_artifact_label(FsPath::new(".orgasmic/decisions.org")),
            "decisions file"
        );
        assert_eq!(
            org_file_artifact_label(FsPath::new(".orgasmic/architecture.org")),
            "architecture file"
        );
        assert_eq!(
            org_file_artifact_label(FsPath::new(".orgasmic/glossary.org")),
            "glossary file"
        );
        assert_eq!(
            org_file_artifact_label(FsPath::new("docs/adr/0001-choice.org")),
            "ADR file"
        );
        assert_eq!(
            org_file_artifact_label(FsPath::new(task_file_rel("backlog.org").as_str())),
            "org file"
        );
    }

    fn repo_root() -> PathBuf {
        let mut here = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        loop {
            if here.join(".orgasmic").is_dir() && here.join("shipped").is_dir() {
                return here;
            }
            if !here.pop() {
                panic!("could not locate orgasmic repo root from CARGO_MANIFEST_DIR");
            }
        }
    }

    fn symlink_repo_source(home: &Home) {
        if home.source().exists() {
            return;
        }
        std::os::unix::fs::symlink(repo_root(), home.source()).unwrap();
    }

    fn seed_project(home: &Home, project_root: &FsPath, project_id: &str) {
        symlink_repo_source(home);
        write(
            project_root.join(".orgasmic/project.org"),
            &format!(
                "#+title: {project_id}\n#+orgasmic_version: 1\n\n* PROJECT {project_id}\n:PROPERTIES:\n:ID:               {project_id}\n:END:\n"
            ),
        );
        write(
            task_file_path(project_root, "backlog.org"),
            "#+title: sprint\n#+orgasmic_version: 1\n\n* BACKLOG TASK-PRE Pre-boot task :work:\n:PROPERTIES:\n:ID:               TASK-PRE\n:END:\n",
        );
        write(
            home.board(),
            &format!(
                "#+title: orgasmic board\n#+orgasmic_version: 1\n\n* PROJECT {project_id}\n:PROPERTIES:\n:ID:               {project_id}\n:PATH:             {}\n:BRANCH:           main\n:STATUS:           active\n:END:\n",
                project_root.display()
            ),
        );
    }

    fn test_options() -> crate::DaemonOptions {
        crate::DaemonOptions {
            bind_override: Some("127.0.0.1".parse().unwrap()),
            port_override: Some(0),
            // None of the API unit tests assert on the `notify` watcher;
            // disabling it avoids the ~0.8s-per-watch macOS FSEvents
            // registration latency at every boot.
            fs_watcher_enabled: false,
            ..crate::DaemonOptions::default()
        }
    }

    fn read_token(home: &Home) -> String {
        std::fs::read_to_string(home.auth_token())
            .expect("token file")
            .trim()
            .to_string()
    }

    async fn connect_ws(addr: std::net::SocketAddr, token: &str, topic: &str) -> TestWs {
        let url = format!("ws://{addr}/api/ws?topic={topic}");
        let mut req = url.into_client_request().unwrap();
        req.headers_mut()
            .insert("authorization", format!("Bearer {token}").parse().unwrap());
        let (ws, _resp) = tokio_tungstenite::connect_async(req).await.unwrap();
        ws
    }

    async fn connect_tmux_ws(addr: std::net::SocketAddr, token: &str, run_id: &str) -> TestWs {
        let url = format!("ws://{addr}/api/ws/tmux/{run_id}?token={token}");
        let req = url.into_client_request().unwrap();
        let (ws, _resp) = tokio_tungstenite::connect_async(req).await.unwrap();
        ws
    }

    async fn next_event_kind(ws: &mut TestWs, kind: &str) -> Value {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let msg = ws.next().await.unwrap().unwrap();
                match msg {
                    Message::Text(text) => {
                        let value: Value = serde_json::from_str(&text).unwrap();
                        if value["payload"]["kind"] == kind {
                            return value;
                        }
                    }
                    Message::Close(frame) => {
                        panic!("websocket closed while waiting for {kind}: {frame:?}")
                    }
                    _ => {}
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {kind} event"))
    }

    async fn next_ws_text_containing(ws: &mut TestWs, needle: &str) -> String {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let msg = ws.next().await.unwrap().unwrap();
                match msg {
                    Message::Text(text) if text.contains(needle) => return text,
                    Message::Close(frame) => {
                        panic!("websocket closed while waiting for {needle}: {frame:?}")
                    }
                    _ => {}
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for websocket text containing {needle}"))
    }

    async fn next_bus_event_kind(
        rx: &mut tokio::sync::broadcast::Receiver<crate::events::Event>,
        topic: Topic,
        kind: &str,
    ) -> crate::events::Event {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let event = rx.recv().await.unwrap();
                let value = serde_json::to_value(&event).unwrap();
                if event.topic == topic && value["payload"]["kind"] == kind {
                    return event;
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {kind} bus event"))
    }

    fn seed_minimal_graph(project_root: &FsPath) {
        write(
            project_root.join(".orgasmic/decisions.org"),
            "#+title: decisions\n#+orgasmic_version: 1\n\n* dec_001 Choice :topic:\n:PROPERTIES:\n:ID:                 dec_001\n:DECIDES:\n:END:\n",
        );
        write(
            project_root.join(".orgasmic/architecture.org"),
            "#+title: architecture\n#+orgasmic_version: 1\n\n* arch_001 Component\n:PROPERTIES:\n:ID:                 arch_001\n:DEPENDS_ON:\n:MOTIVATED_BY:\n:END:\n",
        );
        write(
            project_root.join(".orgasmic/glossary.org"),
            "#+title: glossary\n#+orgasmic_version: 1\n\n* term:one One\n:PROPERTIES:\n:ID:                 one\n:CANONICAL:          one\n:OVERRIDES:\n:REFERENCES:\n:END:\n",
        );
    }

    fn write_nonterminal_session(
        project_root: &FsPath,
        identity: RuntimeIdentity,
        protocol_version: &str,
        worker_id: &str,
    ) -> PathBuf {
        let path = project_sessions_dir(project_root).join(format!("{}.jsonl", identity.run_id));
        let mut writer = orgasmic_core::SessionWriter::open(&path, identity).unwrap();
        let acquire = Lifecycle::Acquire {
            task_id: "TASK-038".into(),
            kind: "implementer".into(),
            worker_id: worker_id.into(),
        };
        writer
            .append(
                SessionEventKind::Lifecycle,
                serde_json::to_value(acquire).unwrap(),
            )
            .unwrap();
        let ready = DriverEvent::Ready {
            protocol_version: protocol_version.into(),
            capabilities: json!({}),
        };
        writer
            .append(
                SessionEventKind::DriverEvent,
                serde_json::to_value(ready).unwrap(),
            )
            .unwrap();
        path
    }

    fn write_stage_session(path: &FsPath, identity: RuntimeIdentity, event: DriverEvent) {
        let mut writer = orgasmic_core::SessionWriter::open(path, identity).unwrap();
        writer
            .append(
                SessionEventKind::DriverEvent,
                serde_json::to_value(event).unwrap(),
            )
            .unwrap();
    }

    async fn direct_stage_test_state(home: Home) -> ApiState {
        let events = EventBus::new();
        let writer = crate::spawn_writer(events.clone());
        let boot = Arc::new(BootIdentity::new());
        let index = Index::new(home.clone());
        index.rebuild().await;
        let supervisor = Supervisor::new(writer.clone(), boot.clone());
        ApiState {
            home: home.clone(),
            index,
            writer,
            supervisor,
            manager_driver: Arc::new(orgasmic_drivers::modes::tmux::driver()),
            events,
            boot,
            auth: AuthState::new("test-token".to_string()),
            default_tx_path: crate::default_home_tx_path(&home),
            tx_commit_to_project: false,
            manager_actor: None,
            auto_commit_signal: true,
            driver_defaults: DriverDefaults::default(),
            actor: "test".to_string(),
            machine: "test-machine".to_string(),
            bind_host: "127.0.0.1".to_string(),
            bind_port: 4848,
            ui_asset_hash: embedded_ui_asset_hash(),
            shutdown: tokio::sync::watch::channel(false).1,
            dispatch_watcher_grace: std::time::Duration::from_secs(30),
            tmux_input_ready_timeout_secs: Some(1),
            dispatch_response_delay: None,
            artifact_write_locks: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        }
    }

    #[tokio::test]
    async fn get_run_exposes_driver_field_immediately_after_acquire() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let state = direct_stage_test_state(home.clone()).await;
        let driver = orgasmic_drivers::TmuxTuiDriver;
        let acquire = state
            .supervisor
            .acquire(
                &driver,
                AcquireRequest {
                    task_id: "manager.launch:orgasmic".into(),
                    kind: RunKind::Worker,
                    worker_id: "manager".into(),
                    role: "manager".into(),
                    project_id: Some("orgasmic".into()),
                    worktree: Some(home.source()),
                    last_path: None,
                    stdout_path: None,
                    session_path: home.sessions().join("manager-test.jsonl"),
                    driver_config: orgasmic_drivers::modes::tmux::inert_config(),
                    babysitter_target: None,
                    stall_timeout_secs: None,
                    max_run_duration_secs: None,
                    idle_timeout_secs: None,
                    babysitter: None,
                },
            )
            .await
            .unwrap();

        let Json(detail) = get_run(State(state), Path(acquire.run_id)).await.unwrap();
        let source = detail["source"].as_str().unwrap();
        assert!(source.contains("\"kind\":\"lifecycle\""));
        assert!(source.contains("\"task_id\":\"manager.launch:orgasmic\""));
        assert_eq!(detail["run"]["driver"], "tmux-tui");
    }

    #[tokio::test]
    async fn get_run_exposes_dispatched_role_as_kind_and_initial_sub_state() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let state = direct_stage_test_state(home.clone()).await;
        let driver = orgasmic_drivers::TmuxTuiDriver;
        let acquire = state
            .supervisor
            .acquire(
                &driver,
                AcquireRequest {
                    task_id: "TASK-REVIEW-RUN".into(),
                    kind: RunKind::Worker,
                    worker_id: "reviewer-claude-rmux".into(),
                    role: "reviewer".into(),
                    project_id: Some("orgasmic".into()),
                    worktree: Some(home.source()),
                    last_path: None,
                    stdout_path: None,
                    session_path: home.sessions().join("reviewer-test.jsonl"),
                    driver_config: orgasmic_drivers::modes::tmux::inert_config(),
                    babysitter_target: None,
                    stall_timeout_secs: None,
                    max_run_duration_secs: None,
                    idle_timeout_secs: None,
                    babysitter: None,
                },
            )
            .await
            .unwrap();

        let Json(detail) = get_run(State(state), Path(acquire.run_id)).await.unwrap();
        assert_eq!(detail["run"]["kind"], "reviewer");
        assert_eq!(detail["run"]["role"], "reviewer");
        assert_eq!(detail["run"]["run_kind"], "worker");
        assert_eq!(detail["run"]["sub_state"], "reviewer.working");
    }

    #[tokio::test]
    async fn daemon_restart_with_live_manager_warns_and_pauses() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let state = direct_stage_test_state(home.clone()).await;
        let driver = orgasmic_drivers::TmuxTuiDriver;
        let acquire = state
            .supervisor
            .acquire(
                &driver,
                AcquireRequest {
                    task_id: "manager.launch:orgasmic".into(),
                    kind: RunKind::Worker,
                    worker_id: "manager".into(),
                    role: "manager".into(),
                    project_id: Some("orgasmic".into()),
                    worktree: Some(home.source()),
                    last_path: None,
                    stdout_path: None,
                    session_path: home.sessions().join("manager-restart-guard.jsonl"),
                    driver_config: orgasmic_drivers::modes::tmux::inert_config(),
                    babysitter_target: None,
                    stall_timeout_secs: None,
                    max_run_duration_secs: None,
                    idle_timeout_secs: None,
                    babysitter: None,
                },
            )
            .await
            .unwrap();

        let Json(response) = post_daemon_restart(
            State(state.clone()),
            Json(RestartRequest {
                reason: None,
                request_id: None,
                force: false,
            }),
        )
        .await
        .expect("restart proceeds while recovery can preserve manager run state");
        assert_eq!(response.status, "restart_requested");
        assert!(
            response.acquisition_paused,
            "restart drain should pause new acquisitions"
        );
        assert!(
            response
                .warnings
                .iter()
                .any(|warning| warning.kind == "live_manager_recovery"
                    && warning.message.contains(&acquire.run_id)),
            "restart should document manager reattach/recovery: {:?}",
            response.warnings
        );
        assert!(
            state
                .supervisor
                .snapshot()
                .await
                .runs
                .iter()
                .any(|run| run.run_id == acquire.run_id),
            "the current daemon keeps the run record until the process exits"
        );
    }

    #[test]
    fn boot_reattach_candidate_requires_nonterminal_acquire_and_run_meta() {
        let env = |kind: SessionEventKind, event: serde_json::Value| SessionEnvelope {
            seq: 0,
            time: chrono::Utc::now(),
            run_id: "run-reattach".into(),
            runtime_id: "rt-reattach".into(),
            boot_id: "boot-old".into(),
            kind,
            event,
        };
        let acquire = env(
            SessionEventKind::Lifecycle,
            serde_json::to_value(Lifecycle::Acquire {
                task_id: "manager.launch:orgasmic".into(),
                kind: "implementer".into(),
                worker_id: "manager".into(),
            })
            .unwrap(),
        );
        let meta = env(
            SessionEventKind::Lifecycle,
            serde_json::to_value(Lifecycle::RunMeta {
                transport: "rmux".into(),
                harness: Some("claude".into()),
                project_id: Some("orgasmic".into()),
                worktree: Some(PathBuf::from("/tmp/orgasmic")),
                last_path: None,
                stdout_path: None,
                driver_config: json!({"system_wide": true}),
            })
            .unwrap(),
        );
        let release = env(
            SessionEventKind::Lifecycle,
            serde_json::to_value(Lifecycle::Release {
                reason: "done".into(),
                outcome: ReleaseOutcome::Completed,
                finalized_by_worker: false,
            })
            .unwrap(),
        );
        let path = PathBuf::from("/tmp/run-reattach.jsonl");

        // Acquire + RunMeta, non-terminal → a full candidate.
        let candidate =
            boot_reattach_candidate(&[acquire.clone(), meta.clone()], &path).expect("candidate");
        assert_eq!(candidate.run_id, "run-reattach");
        assert_eq!(candidate.task_id, "manager.launch:orgasmic");
        assert_eq!(candidate.transport, "rmux");
        assert_eq!(candidate.harness.as_deref(), Some("claude"));
        assert_eq!(candidate.driver_config["system_wide"], json!(true));
        // Pre-upgrade / non-dispatch RunMeta carries no artifact paths — boot
        // reattach must still succeed, just without a completion watcher.
        assert!(candidate.last_path.is_none());
        assert!(candidate.stdout_path.is_none());

        // Terminal release → not a candidate.
        assert!(
            boot_reattach_candidate(&[acquire.clone(), meta.clone(), release], &path).is_none()
        );
        // Missing RunMeta (pre-upgrade session) → not a candidate.
        assert!(boot_reattach_candidate(&[acquire], &path).is_none());
    }

    /// TASK-567JG: a daemon restart mid-run must still write `last.txt` on
    /// release. `spawn_dispatch_completion_watcher` is a tokio task spawned
    /// only at dispatch time, so it dies with the old daemon process; without
    /// respawning it on boot reattach, `write_dispatch_completion_artifacts`
    /// never runs. This drives the real `reattach_live_runs_on_boot` path
    /// (not just `boot_reattach_candidate`) against a genuinely live tmux
    /// session — simulating the restart with a second, independent
    /// `ApiState`/`Supervisor` that never acquired the run itself — then
    /// releases the run and asserts the artifact lands.
    #[tokio::test]
    async fn boot_reattach_respawns_dispatch_completion_watcher_and_writes_last_txt() {
        let _live_guard = live_session_guard();
        if !tmux_on_path() {
            eprintln!(
                "skipping boot_reattach_respawns_dispatch_completion_watcher_and_writes_last_txt: tmux not on PATH"
            );
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let project_root = tmp.path().join("proj");
        std::fs::create_dir_all(project_sessions_dir(&project_root)).unwrap();

        let suffix = uuid::Uuid::new_v4().simple().to_string();
        let identity = RuntimeIdentity {
            run_id: format!("run-reattach-watcher-{suffix}"),
            runtime_id: format!("rt-{suffix}"),
            boot_id: "boot-before-restart".into(),
        };
        let session_name = orgasmic_drivers::modes::tmux::tmux_session_name(&identity);
        let status = Command::new("tmux")
            .args(["new-session", "-d", "-s", &session_name, "sleep", "60"])
            .status()
            .unwrap();
        assert!(status.success(), "tmux test session should start");
        let _guard = TmuxSessionGuard(session_name);

        let session_path =
            project_sessions_dir(&project_root).join(format!("{}.jsonl", identity.run_id));
        let last_path = tmp.path().join("last.txt");
        let stdout_path = tmp.path().join("stdout.log");
        let mut writer = orgasmic_core::SessionWriter::open(&session_path, identity.clone())
            .expect("open session writer");
        writer
            .append(
                SessionEventKind::Lifecycle,
                serde_json::to_value(Lifecycle::Acquire {
                    task_id: "TASK-567JG-REATTACH".into(),
                    kind: "implementer".into(),
                    worker_id: "implementer-claude-rmux".into(),
                })
                .unwrap(),
            )
            .unwrap();
        writer
            .append(
                SessionEventKind::Lifecycle,
                serde_json::to_value(Lifecycle::RunMeta {
                    transport: "tmux".into(),
                    harness: Some("claude".into()),
                    project_id: Some("orgasmic".into()),
                    worktree: Some(project_root.clone()),
                    last_path: Some(last_path.clone()),
                    stdout_path: Some(stdout_path.clone()),
                    driver_config: json!({}),
                })
                .unwrap(),
            )
            .unwrap();
        writer
            .append(
                SessionEventKind::DriverEvent,
                serde_json::to_value(DriverEvent::TextChunk {
                    stream: orgasmic_core::TextStream::Assistant,
                    chunk: "## Report\nboot reattach watcher smoke".into(),
                    seq: 0,
                })
                .unwrap(),
            )
            .unwrap();
        drop(writer);

        // A fresh Supervisor/ApiState never acquired this run — standing in
        // for the post-restart daemon boot.
        let state = direct_stage_test_state(home).await;
        reattach_live_runs_on_boot(&state, std::slice::from_ref(&project_root)).await;
        assert!(
            state
                .supervisor
                .snapshot()
                .await
                .runs
                .iter()
                .any(|run| run.run_id == identity.run_id),
            "run should be rehydrated into the post-restart supervisor"
        );

        state
            .supervisor
            .release(
                &identity.run_id,
                "test cleanup",
                orgasmic_core::ReleaseOutcome::Completed,
            )
            .await
            .expect("release reattached run");

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if last_path.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let last_body = std::fs::read_to_string(&last_path)
            .unwrap_or_else(|e| panic!("last.txt was never written by the respawned watcher: {e}"));
        assert!(
            last_body.contains("boot reattach watcher smoke"),
            "last.txt should carry the worker report: {last_body}"
        );
    }

    fn dispatch_summary_env(kind: SessionEventKind, event: serde_json::Value) -> SessionEnvelope {
        SessionEnvelope {
            seq: 0,
            time: chrono::Utc::now(),
            run_id: "run-b05am".into(),
            runtime_id: "rt-b05am".into(),
            boot_id: "boot".into(),
            kind,
            event,
        }
    }

    #[test]
    fn stall_release_synthetic_run_complete_does_not_shadow_worker_report() {
        // A completed rmux run whose eot marker was missed: the worker printed
        // its report as assistant chunks, then release("stall_timeout_exceeded")
        // synthesized RunComplete{summary: reason} and the stall Release landed.
        // last.txt must carry the report, not the stall sentinel (TASK-B05AM).
        let envelopes = vec![
            dispatch_summary_env(
                SessionEventKind::DriverEvent,
                json!({"type":"text_chunk","stream":"assistant","chunk":"## Report\nAll gates green; commit abc123.","seq":1}),
            ),
            dispatch_summary_env(
                SessionEventKind::DriverEvent,
                json!({"type":"run_complete","summary":"stall_timeout_exceeded"}),
            ),
            dispatch_summary_env(
                SessionEventKind::Lifecycle,
                serde_json::to_value(Lifecycle::Release {
                    reason: "stall_timeout_exceeded".into(),
                    outcome: ReleaseOutcome::Failed,
                    finalized_by_worker: false,
                })
                .unwrap(),
            ),
        ];
        let summary = dispatch_last_summary_from_session(&envelopes);
        assert!(
            summary.contains("All gates green"),
            "worker report must be preserved, got: {summary:?}"
        );
        assert!(
            !summary.contains("stall_timeout_exceeded"),
            "stall sentinel must not surface as the report, got: {summary:?}"
        );
    }

    #[test]
    fn genuine_run_complete_summary_is_still_preferred() {
        // A clean eot completion: run_complete carries the real report tail and
        // the release reason differs, so the run_complete summary is used as-is
        // (the B05AM fix must not disturb the normal path).
        let envelopes = vec![
            dispatch_summary_env(
                SessionEventKind::DriverEvent,
                json!({"type":"text_chunk","stream":"assistant","chunk":"working...","seq":1}),
            ),
            dispatch_summary_env(
                SessionEventKind::DriverEvent,
                json!({"type":"run_complete","summary":"## Report\nDone: 42 tests pass."}),
            ),
            dispatch_summary_env(
                SessionEventKind::Lifecycle,
                serde_json::to_value(Lifecycle::Release {
                    reason: "driver terminal event".into(),
                    outcome: ReleaseOutcome::Completed,
                    finalized_by_worker: false,
                })
                .unwrap(),
            ),
        ];
        let summary = dispatch_last_summary_from_session(&envelopes);
        assert_eq!(summary, "## Report\nDone: 42 tests pass.");
    }

    #[tokio::test]
    async fn manager_session_after_restart_is_recovered_not_lost() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let project_root = tmp.path().join("project");
        std::fs::create_dir_all(project_root.join(".orgasmic/tmp/sessions")).unwrap();
        let identity = RuntimeIdentity {
            run_id: "run-manager-recovery".into(),
            runtime_id: "rt-manager-recovery".into(),
            boot_id: "boot-before-restart".into(),
        };
        let session_path = project_sessions_dir(&project_root).join("run-manager-recovery.jsonl");
        let mut writer = orgasmic_core::SessionWriter::open(&session_path, identity).unwrap();
        writer
            .append(
                SessionEventKind::Lifecycle,
                serde_json::to_value(Lifecycle::Acquire {
                    task_id: "manager.launch:orgasmic".into(),
                    kind: "worker".into(),
                    worker_id: "manager".into(),
                })
                .unwrap(),
            )
            .unwrap();
        writer
            .append(
                SessionEventKind::Lifecycle,
                serde_json::to_value(Lifecycle::RunMeta {
                    transport: "acp-stdio".into(),
                    harness: Some("claude".into()),
                    project_id: Some("orgasmic".into()),
                    worktree: Some(project_root.clone()),
                    last_path: None,
                    stdout_path: None,
                    driver_config: json!({}),
                })
                .unwrap(),
            )
            .unwrap();
        writer
            .append(
                SessionEventKind::DriverEvent,
                serde_json::to_value(DriverEvent::Ready {
                    protocol_version: "acp/1".into(),
                    capabilities: json!({}),
                })
                .unwrap(),
            )
            .unwrap();

        let recovered = classify_session_files(
            &home,
            "boot-after-restart",
            &[],
            std::slice::from_ref(&project_root),
        )
        .await;
        let run = recovered
            .iter()
            .find(|run| run.run_id == "run-manager-recovery")
            .expect("manager run should remain visible after restart");
        assert_eq!(run.classification, "interrupted");
        assert!(
            run.reason.contains("could not prove a live runtime handle"),
            "{}",
            run.reason
        );
        assert!(
            run.recovery_actions.iter().any(|action| {
                action.kind == "start_recovery_run" && action.target == "manager"
            }),
            "manager run should offer a manager-targeted recovery action: {:?}",
            run.recovery_actions
        );
    }

    #[test]
    fn dispatch_last_summary_accumulates_assistant_deltas_instead_of_last_fragment() {
        let envelopes = vec![
            SessionEnvelope {
                seq: 0,
                time: chrono::Utc::now(),
                run_id: "run-test".into(),
                runtime_id: "rt-test".into(),
                boot_id: "boot-test".into(),
                kind: SessionEventKind::DriverEvent,
                event: json!({"type": "text_chunk", "stream": "assistant", "chunk": "Invest"}),
            },
            SessionEnvelope {
                seq: 1,
                time: chrono::Utc::now(),
                run_id: "run-test".into(),
                runtime_id: "rt-test".into(),
                boot_id: "boot-test".into(),
                kind: SessionEventKind::DriverEvent,
                event: json!({"type": "text_chunk", "stream": "assistant", "chunk": "igating the code"}),
            },
            SessionEnvelope {
                seq: 2,
                time: chrono::Utc::now(),
                run_id: "run-test".into(),
                runtime_id: "rt-test".into(),
                boot_id: "boot-test".into(),
                kind: SessionEventKind::DriverEvent,
                event: json!({"type": "text_chunk", "stream": "assistant", "chunk": " paths."}),
            },
            SessionEnvelope {
                seq: 3,
                time: chrono::Utc::now(),
                run_id: "run-test".into(),
                runtime_id: "rt-test".into(),
                boot_id: "boot-test".into(),
                kind: SessionEventKind::Lifecycle,
                event: json!({"phase": "release", "reason": "driver stream closed", "outcome": "interrupted"}),
            },
        ];
        let summary = dispatch_last_summary_from_session(&envelopes);
        assert_eq!(summary, "Investigating the code paths.");
    }

    #[test]
    fn dispatch_last_summary_prefers_assistant_report_over_generic_completion_marker() {
        // Pre-TASK-YHA6V codex sessions: run_complete carries a fabricated
        // "codex turn completed" while the real report sits in assistant chunks.
        let envelopes = vec![
            SessionEnvelope {
                seq: 0,
                time: chrono::Utc::now(),
                run_id: "run-test".into(),
                runtime_id: "rt-test".into(),
                boot_id: "boot-test".into(),
                kind: SessionEventKind::DriverEvent,
                event: json!({"type": "text_chunk", "stream": "assistant", "chunk": "## Report\nAll gates green."}),
            },
            SessionEnvelope {
                seq: 1,
                time: chrono::Utc::now(),
                run_id: "run-test".into(),
                runtime_id: "rt-test".into(),
                boot_id: "boot-test".into(),
                kind: SessionEventKind::DriverEvent,
                event: json!({"type": "run_complete", "summary": "codex turn completed"}),
            },
        ];
        let summary = dispatch_last_summary_from_session(&envelopes);
        assert_eq!(summary, "## Report\nAll gates green.");
    }

    #[test]
    fn dispatch_last_summary_uses_marker_when_nothing_else_exists() {
        let envelopes = vec![SessionEnvelope {
            seq: 0,
            time: chrono::Utc::now(),
            run_id: "run-test".into(),
            runtime_id: "rt-test".into(),
            boot_id: "boot-test".into(),
            kind: SessionEventKind::DriverEvent,
            event: json!({"type": "run_complete", "summary": "thread closed"}),
        }];
        let summary = dispatch_last_summary_from_session(&envelopes);
        assert_eq!(summary, "thread closed");
    }

    #[test]
    fn generic_completion_marker_detection_is_narrow() {
        assert!(is_generic_completion_marker("codex turn completed"));
        assert!(is_generic_completion_marker("codex turn interrupted"));
        assert!(is_generic_completion_marker("thread closed"));
        // Real content that merely starts with the prefix must NOT be demoted.
        assert!(!is_generic_completion_marker(
            "codex turn completed — fixed the parser, all tests green"
        ));
        assert!(!is_generic_completion_marker("codex turn "));
        assert!(!is_generic_completion_marker("## Report\nAll gates green."));
    }

    #[test]
    fn dispatch_last_summary_falls_back_to_accumulated_system_text_without_assistant() {
        let envelopes = vec![
            SessionEnvelope {
                seq: 0,
                time: chrono::Utc::now(),
                run_id: "run-test".into(),
                runtime_id: "rt-test".into(),
                boot_id: "boot-test".into(),
                kind: SessionEventKind::DriverEvent,
                event: json!({"type": "text_chunk", "stream": "system", "chunk": "thinking "}),
            },
            SessionEnvelope {
                seq: 1,
                time: chrono::Utc::now(),
                run_id: "run-test".into(),
                runtime_id: "rt-test".into(),
                boot_id: "boot-test".into(),
                kind: SessionEventKind::DriverEvent,
                event: json!({"type": "text_chunk", "stream": "system", "chunk": "summary text"}),
            },
            SessionEnvelope {
                seq: 2,
                time: chrono::Utc::now(),
                run_id: "run-test".into(),
                runtime_id: "rt-test".into(),
                boot_id: "boot-test".into(),
                kind: SessionEventKind::Lifecycle,
                event: json!({"phase": "release", "reason": "driver stream closed", "outcome": "interrupted"}),
            },
        ];
        let summary = dispatch_last_summary_from_session(&envelopes);
        assert_eq!(summary, "thinking summary text");
    }

    #[test]
    fn dispatch_terminal_reached_ignores_interrupted_release_without_driver_terminal() {
        let envelopes = vec![SessionEnvelope {
            seq: 0,
            time: chrono::Utc::now(),
            run_id: "run-test".into(),
            runtime_id: "rt-test".into(),
            boot_id: "boot-test".into(),
            kind: SessionEventKind::Lifecycle,
            event: json!({"phase": "release", "reason": "driver stream closed", "outcome": "interrupted"}),
        }];
        assert!(
            !dispatch_terminal_reached(&envelopes),
            "Interrupted lifecycle alone must not satisfy dispatch terminal gate"
        );
    }

    #[tokio::test]
    async fn dispatch_completion_artifacts_write_system_only_session_fixture() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let state = direct_stage_test_state(home).await;
        let session_path = tmp.path().join("session.jsonl");
        let last_path = tmp.path().join("last.txt");
        let stdout_path = tmp.path().join("stdout.log");
        let worktree = tmp.path().join("worktree");
        std::fs::create_dir_all(&worktree).unwrap();
        let envelopes = vec![
            SessionEnvelope {
                seq: 0,
                time: chrono::Utc::now(),
                run_id: "run-test".into(),
                runtime_id: "rt-test".into(),
                boot_id: "boot-test".into(),
                kind: SessionEventKind::DriverEvent,
                event: json!({"type": "ready", "protocol_version": "codex-appserver/1"}),
            },
            SessionEnvelope {
                seq: 1,
                time: chrono::Utc::now(),
                run_id: "run-test".into(),
                runtime_id: "rt-test".into(),
                boot_id: "boot-test".into(),
                kind: SessionEventKind::DriverEvent,
                event: json!({"type": "text_chunk", "stream": "system", "chunk": "cursor-agent thinking delta"}),
            },
            SessionEnvelope {
                seq: 2,
                time: chrono::Utc::now(),
                run_id: "run-test".into(),
                runtime_id: "rt-test".into(),
                boot_id: "boot-test".into(),
                kind: SessionEventKind::DriverEvent,
                event: json!({"type": "text_chunk", "stream": "system", "chunk": " meaningful work summary"}),
            },
            SessionEnvelope {
                seq: 3,
                time: chrono::Utc::now(),
                run_id: "run-test".into(),
                runtime_id: "rt-test".into(),
                boot_id: "boot-test".into(),
                kind: SessionEventKind::Lifecycle,
                event: json!({"phase": "release", "reason": "driver stream closed", "outcome": "interrupted"}),
            },
        ];
        assert!(!dispatch_terminal_reached(&envelopes));
        let completion = DispatchCompletion {
            project_id: "proj-dispatch".into(),
            task_id: "TASK-072".into(),
            run_id: "run-test".into(),
            session_path: session_path.clone(),
            last_path: last_path.clone(),
            stdout_path: stdout_path.clone(),
            worktree_path: worktree.clone(),
        };
        write_dispatch_completion_artifacts(&state, &completion, &envelopes).await;
        let last_body = std::fs::read_to_string(&last_path).expect("last_path");
        assert!(
            last_body.contains("meaningful work summary"),
            "grace-path artifact write should carry accumulated system text: {last_body}"
        );
        let stdout_body = std::fs::read_to_string(&stdout_path).expect("stdout_path");
        assert!(
            stdout_body.contains("[system]"),
            "stdout_path should include system chunks: {stdout_body}"
        );
    }

    /// Acceptance #4 (TASK-WFW1N, regression, dec_3M7M0): once a worker has
    /// called `orgasmic dispatch finalize` — recorded as
    /// `Lifecycle::Release { finalized_by_worker: true, .. }` — the
    /// completion path must never scrape scrollback and overwrite the
    /// worker-authored `last.txt`/`stdout.log`, even when reached directly
    /// (e.g. the post-release grace path, not just the watcher's early
    /// return).
    #[tokio::test]
    async fn finalized_release_suppresses_completion_artifact_write() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let state = direct_stage_test_state(home).await;
        let session_path = tmp.path().join("session.jsonl");
        let last_path = tmp.path().join("last.txt");
        let stdout_path = tmp.path().join("stdout.log");
        let worktree = tmp.path().join("worktree");
        std::fs::create_dir_all(&worktree).unwrap();

        let worker_last = "## Report\nfinalized by worker, verbatim.";
        let worker_stdout = "worker stdout, verbatim.";
        std::fs::write(&last_path, worker_last).unwrap();
        std::fs::write(&stdout_path, worker_stdout).unwrap();

        let envelopes = vec![
            SessionEnvelope {
                seq: 0,
                time: chrono::Utc::now(),
                run_id: "run-test".into(),
                runtime_id: "rt-test".into(),
                boot_id: "boot-test".into(),
                kind: SessionEventKind::DriverEvent,
                event: json!({"type": "text_chunk", "stream": "assistant", "chunk": "scrollback scrape candidate — must NOT land in last.txt"}),
            },
            SessionEnvelope {
                seq: 1,
                time: chrono::Utc::now(),
                run_id: "run-test".into(),
                runtime_id: "rt-test".into(),
                boot_id: "boot-test".into(),
                kind: SessionEventKind::Lifecycle,
                event: serde_json::to_value(Lifecycle::Release {
                    reason: "worker finalize for TASK-B05AM".into(),
                    outcome: ReleaseOutcome::Completed,
                    finalized_by_worker: true,
                })
                .unwrap(),
            },
        ];
        assert!(dispatch_release_finalized_by_worker(&envelopes));

        let completion = DispatchCompletion {
            project_id: "proj-dispatch".into(),
            task_id: "TASK-B05AM".into(),
            run_id: "run-test".into(),
            session_path: session_path.clone(),
            last_path: last_path.clone(),
            stdout_path: stdout_path.clone(),
            worktree_path: worktree.clone(),
        };
        write_dispatch_completion_artifacts(&state, &completion, &envelopes).await;

        let last_body = std::fs::read_to_string(&last_path).unwrap();
        assert_eq!(
            last_body, worker_last,
            "finalized release must not overwrite the worker-authored last.txt"
        );
        let stdout_body = std::fs::read_to_string(&stdout_path).unwrap();
        assert_eq!(
            stdout_body, worker_stdout,
            "finalized release must not overwrite the worker-authored stdout.log"
        );
    }

    /// Acceptance #5 (TASK-WFW1N, dec_3M7M0): if the worker never calls
    /// `orgasmic dispatch finalize` and the run stalls, the completion
    /// watcher must flag the run `manager.dispatch_orphaned` rather than
    /// silently synthesizing a `last.txt` that would read as a normal
    /// completion. Drives the real `spawn_dispatch_completion_watcher`
    /// end-to-end against a session file recorded exactly as
    /// `Supervisor::release_first_timed_out_run_after_candidate` would
    /// write one (`reason: "stall_timeout_exceeded"`,
    /// `finalized_by_worker: false`).
    #[tokio::test]
    async fn stall_without_worker_finalize_flags_orphan_not_done() {
        assert_timeout_without_finalize_flags_orphan("stall_timeout_exceeded").await;
    }

    /// Reviewer #1 regression: the orphan branch keyed on the literal
    /// `"stall_timeout_exceeded"`, but `timed_out_run` also emits
    /// `idle_timeout_exceeded` (the ONLY timeout the persistent artifactor-rmux
    /// path can hit — it disables stall) and `max_run_duration_exceeded`. Those
    /// were falling through to a synthesized last.txt (silently "done"). Assert
    /// a non-stall timeout without finalize is also flagged orphan.
    #[tokio::test]
    async fn idle_timeout_without_worker_finalize_flags_orphan_not_done() {
        assert_timeout_without_finalize_flags_orphan("idle_timeout_exceeded").await;
    }

    #[tokio::test]
    async fn max_run_duration_without_worker_finalize_flags_orphan_not_done() {
        assert_timeout_without_finalize_flags_orphan("max_run_duration_exceeded").await;
    }

    async fn assert_timeout_without_finalize_flags_orphan(release_reason: &str) {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let mut state = direct_stage_test_state(home).await;
        state.dispatch_watcher_grace = Duration::from_millis(50);

        let session_path = tmp.path().join("session.jsonl");
        let last_path = tmp.path().join("last.txt");
        let stdout_path = tmp.path().join("stdout.log");
        let worktree = tmp.path().join("worktree");
        std::fs::create_dir_all(&worktree).unwrap();

        let identity = RuntimeIdentity {
            run_id: format!("run-orphan-{release_reason}"),
            runtime_id: "rt-orphan-test".into(),
            boot_id: "boot-orphan-test".into(),
        };
        let mut writer =
            orgasmic_core::SessionWriter::open(&session_path, identity.clone()).unwrap();
        writer
            .append(
                SessionEventKind::Lifecycle,
                serde_json::to_value(Lifecycle::Acquire {
                    task_id: "TASK-ORPHAN".into(),
                    kind: "implementer".into(),
                    worker_id: "implementer-claude-rmux".into(),
                })
                .unwrap(),
            )
            .unwrap();
        writer
            .append(
                SessionEventKind::DriverEvent,
                serde_json::to_value(DriverEvent::TextChunk {
                    stream: orgasmic_core::TextStream::Assistant,
                    chunk: "still working, worker never finalized".into(),
                    seq: 0,
                })
                .unwrap(),
            )
            .unwrap();
        // No `Lifecycle::Release { finalized_by_worker: true, .. }` ever
        // lands — the worker never called `orgasmic dispatch finalize`.
        // Instead a timeout releases the run itself, exactly as
        // `Supervisor::release_first_timed_out_run_after_candidate` does.
        writer
            .append(
                SessionEventKind::Lifecycle,
                serde_json::to_value(Lifecycle::Release {
                    reason: release_reason.into(),
                    outcome: ReleaseOutcome::Failed,
                    finalized_by_worker: false,
                })
                .unwrap(),
            )
            .unwrap();
        drop(writer);

        let completion = DispatchCompletion {
            project_id: "proj-dispatch".into(),
            task_id: "TASK-ORPHAN".into(),
            run_id: identity.run_id.clone(),
            session_path: session_path.clone(),
            last_path: last_path.clone(),
            stdout_path: stdout_path.clone(),
            worktree_path: worktree.clone(),
        };
        // This run was never acquired via `state.supervisor`, so
        // `snapshot().runs` never contains it — the watcher sees it as
        // already released on its very first poll, matching the real
        // post-release state the production code observes.
        spawn_dispatch_completion_watcher(state.clone(), completion);

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut tx_raw = String::new();
        while std::time::Instant::now() < deadline {
            if let Ok(body) = std::fs::read_to_string(&state.default_tx_path) {
                if body.contains("manager.dispatch_orphaned") {
                    tx_raw = body;
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(
            tx_raw.contains("manager.dispatch_orphaned"),
            "{release_reason} without worker finalize must record manager.dispatch_orphaned, got tx log: {tx_raw:?}"
        );
        assert!(
            tx_raw.contains(":TASK:         TASK-ORPHAN"),
            "orphan tx should carry the task id: {tx_raw}"
        );
        assert!(
            !last_path.exists(),
            "{release_reason} without worker finalize must never synthesize a last.txt (not silently marked done)"
        );
        assert!(
            !stdout_path.exists(),
            "{release_reason} without worker finalize must never synthesize a stdout.log"
        );
    }

    #[test]
    fn is_timeout_release_reason_covers_all_timed_out_run_reasons() {
        // Must stay in sync with `timed_out_run` (supervisor.rs).
        assert!(is_timeout_release_reason("stall_timeout_exceeded"));
        assert!(is_timeout_release_reason("idle_timeout_exceeded"));
        assert!(is_timeout_release_reason("max_run_duration_exceeded"));
        // A clean worker finalize / dispatch-close release is NOT a timeout.
        assert!(!is_timeout_release_reason("worker finalize for TASK-ABC"));
        assert!(!is_timeout_release_reason(""));
    }

    #[test]
    fn dispatch_last_summary_prefers_run_complete_over_dispatch_close_release() {
        let envelopes = vec![
            SessionEnvelope {
                seq: 0,
                time: chrono::Utc::now(),
                run_id: "run-test".into(),
                runtime_id: "rt-test".into(),
                boot_id: "boot-test".into(),
                kind: SessionEventKind::DriverEvent,
                event: json!({"type": "text_chunk", "stream": "assistant", "chunk": "hello world"}),
            },
            SessionEnvelope {
                seq: 1,
                time: chrono::Utc::now(),
                run_id: "run-test".into(),
                runtime_id: "rt-test".into(),
                boot_id: "boot-test".into(),
                kind: SessionEventKind::DriverEvent,
                event: json!({"type": "run_complete", "summary": "hello world cursor agent summary"}),
            },
            SessionEnvelope {
                seq: 2,
                time: chrono::Utc::now(),
                run_id: "run-test".into(),
                runtime_id: "rt-test".into(),
                boot_id: "boot-test".into(),
                kind: SessionEventKind::Lifecycle,
                event: json!({"phase": "release", "reason": "dispatch close for TASK-067", "outcome": "completed"}),
            },
        ];
        let summary = dispatch_last_summary_from_session(&envelopes);
        assert_eq!(summary, "hello world cursor agent summary");
    }

    #[test]
    fn dispatch_last_summary_falls_back_to_assistant_text_chunk_without_run_complete_summary() {
        let envelopes = vec![
            SessionEnvelope {
                seq: 0,
                time: chrono::Utc::now(),
                run_id: "run-test".into(),
                runtime_id: "rt-test".into(),
                boot_id: "boot-test".into(),
                kind: SessionEventKind::DriverEvent,
                event: json!({"type": "text_chunk", "stream": "assistant", "chunk": "hello world"}),
            },
            SessionEnvelope {
                seq: 1,
                time: chrono::Utc::now(),
                run_id: "run-test".into(),
                runtime_id: "rt-test".into(),
                boot_id: "boot-test".into(),
                kind: SessionEventKind::DriverEvent,
                event: json!({"type": "run_complete"}),
            },
            SessionEnvelope {
                seq: 2,
                time: chrono::Utc::now(),
                run_id: "run-test".into(),
                runtime_id: "rt-test".into(),
                boot_id: "boot-test".into(),
                kind: SessionEventKind::Lifecycle,
                event: json!({"phase": "release", "reason": "dispatch close for TASK-067", "outcome": "completed"}),
            },
        ];
        let summary = dispatch_last_summary_from_session(&envelopes);
        assert_eq!(summary, "hello world");
    }

    #[test]
    fn dispatch_worker_resolution_uses_pipeline_without_explicit_worker() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        write(
            home.user().join("workers/stage-implementer.org"),
            "* WORKER stage-implementer
:PROPERTIES:
:ID: stage-implementer
:KIND: implementer
:DRIVER: acp-ws
:HARNESS: codex
:DEFAULT_PROVIDER: openai
:DEFAULT_MODEL: gpt-5.5
:LINKED_SKILLS:
:END:
",
        );
        let mut project = test_project(tmp.path(), &[], &[], &[]);
        project.worker_pipeline = vec!["stage-implementer".to_string()];

        let worker = resolve_dispatch_worker_id(
            &home,
            &project,
            DispatchEndpointKind::Implementer,
            None,
            None,
        )
        .unwrap();

        assert_eq!(worker, "stage-implementer");
    }

    #[test]
    fn dispatch_worker_resolution_explicit_worker_overrides_pipeline() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let mut project = test_project(tmp.path(), &[], &[], &[]);
        project.worker_pipeline = vec!["stage-implementer".to_string()];

        let worker = resolve_dispatch_worker_id(
            &home,
            &project,
            DispatchEndpointKind::Implementer,
            Some("explicit-implementer"),
            None,
        )
        .unwrap();

        assert_eq!(worker, "explicit-implementer");
    }

    #[test]
    fn dispatch_worker_resolution_prefers_task_heading_over_pipeline_default() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        write(
            home.user().join("workers/pipeline-implementer.org"),
            "* WORKER pipeline-implementer\n:PROPERTIES:\n:ID: pipeline-implementer\n:KIND: implementer\n:DRIVER: acp-ws\n:HARNESS: codex\n:END:\n",
        );
        let mut project = test_project(tmp.path(), &[], &[], &[]);
        project.worker_pipeline = vec!["pipeline-implementer".to_string()];

        let worker = resolve_dispatch_worker_id(
            &home,
            &project,
            DispatchEndpointKind::Implementer,
            None,
            Some("task-implementer"),
        )
        .unwrap();

        assert_eq!(worker, "task-implementer");
    }

    #[test]
    fn dispatch_task_slots_fall_back_to_config_defaults_and_task_overrides_win() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".orgasmic")).unwrap();
        std::fs::write(
            root.join(".orgasmic/config.org"),
            "#+title: config\n#+orgasmic_version: 1\n\n* CONFIG orgasmic\n:PROPERTIES:\n:ID: orgasmic\n:TEST_CMD: cargo test --config-default\n:WRITE_SCOPE: crates/default src/default\n:PIPELINE: pipeline-implementer\n:END:\n",
        )
        .unwrap();
        let mut project = test_project(root, &[], &[], &[]);
        project.default_test_cmd = Some("cargo test --indexed-default".to_string());
        project.default_write_scope = vec!["indexed/default".to_string()];
        project.tasks.push(crate::index::TaskSummary {
            id: "TASK-DEFAULT".to_string(),
            title: "defaulted".to_string(),
            lifecycle_stage: LifecycleStage::InProgress,
            parent_task: None,
            depends_on: Vec::new(),
            implements: Vec::new(),
            produces: Vec::new(),
            read_scope: Vec::new(),
            write_scope: Vec::new(),
            owner: TaskOwner::Human,
            run_id: None,
            priority: None,
            worker: None,
            provider: None,
            model: None,
            reasoning_effort: None,
            test_cmd: None,
            tags: Vec::new(),
            source_file: root.join(".orgasmic/tasks/in_progress.org"),
            sandbox_permissions: None,
        });
        project.tasks.push(crate::index::TaskSummary {
            id: "TASK-OVERRIDE".to_string(),
            title: "overridden".to_string(),
            lifecycle_stage: LifecycleStage::InProgress,
            parent_task: None,
            depends_on: Vec::new(),
            implements: Vec::new(),
            produces: Vec::new(),
            read_scope: Vec::new(),
            write_scope: vec!["crates/override".to_string()],
            owner: TaskOwner::Human,
            run_id: None,
            priority: None,
            worker: None,
            provider: None,
            model: None,
            reasoning_effort: None,
            test_cmd: Some("cargo test --task-override".to_string()),
            tags: Vec::new(),
            source_file: root.join(".orgasmic/tasks/in_progress.org"),
            sandbox_permissions: None,
        });

        let mut values = SlotValues::new();
        hydrate_dispatch_task_slots(&mut values, &project, "TASK-DEFAULT").unwrap();
        assert_eq!(values["task.write_scope"], "crates/default\nsrc/default");
        assert_eq!(values["task.test_cmd"], "cargo test --config-default");

        let mut values = SlotValues::new();
        hydrate_dispatch_task_slots(&mut values, &project, "TASK-OVERRIDE").unwrap();
        assert_eq!(values["task.write_scope"], "crates/override");
        assert_eq!(values["task.test_cmd"], "cargo test --task-override");
    }

    #[test]
    fn task_create_filters_blank_and_default_dispatch_params() {
        let mut properties = BTreeMap::new();
        properties.insert("WORKER".to_string(), "implementer-default".to_string());
        properties.insert("TEST_CMD".to_string(), "cargo test".to_string());
        properties.insert(
            "WRITE_SCOPE".to_string(),
            "crates/default src/default".to_string(),
        );
        properties.insert("PROVIDER".to_string(), "".to_string());
        properties.insert("MODEL".to_string(), "gpt-5.5".to_string());

        let filtered = filter_default_dispatch_properties(
            &properties,
            Some("implementer-default"),
            "cargo test",
            &["crates/default".to_string(), "src/default".to_string()],
        );

        assert!(!filtered.contains_key("WORKER"));
        assert!(!filtered.contains_key("TEST_CMD"));
        assert!(!filtered.contains_key("WRITE_SCOPE"));
        assert_eq!(filtered.get("MODEL").map(String::as_str), Some("gpt-5.5"));
        assert_eq!(filtered.get("PROVIDER").map(String::as_str), Some(""));
    }

    #[test]
    fn dispatch_override_warns_off_list_model_without_blocking() {
        let worker = StageWorker {
            id: "worker-a".to_string(),
            kind: WorkerKind::Implementer,
            driver: "acp-ws".to_string(),
            harness: "codex".to_string(),
            models: vec!["gpt-5.5".to_string()],
            reasoning_efforts: vec!["high".to_string()],
            default_provider: None,
            default_model: None,
            default_effort: None,
            linked_skills: Vec::new(),
            persona: None,
            operating_rules: None,
            missing_skills: Vec::new(),
            babysitter_worker: None,
            stall_timeout_secs: None,
            max_run_duration_secs: None,
            sandbox_permissions: None,
            harness_args: Vec::new(),
        };
        warn_dispatch_overrides(
            &worker,
            &DriverOverrides {
                provider: None,
                model: Some("gpt-99".to_string()),
                effort: Some("xhigh".to_string()),
            },
        );
        let cfg = stage_driver_config_with_overrides(
            &worker,
            FsPath::new("/tmp/project"),
            FsPath::new("/tmp/worktree"),
            "brief",
            DriverOverrides {
                provider: None,
                model: Some("gpt-99".to_string()),
                effort: None,
            },
            None,
            &DriverDefaults::default(),
            None,
        );
        assert_eq!(cfg.0["model"], "gpt-99");
    }

    #[test]
    fn dispatch_driver_config_applies_model_and_effort_overrides() {
        let worker = StageWorker {
            id: "worker-a".to_string(),
            kind: WorkerKind::Implementer,
            driver: "subprocess-stream-json".to_string(),
            harness: "cursor-agent".to_string(),
            models: vec!["composer-2.5-fast".to_string()],
            reasoning_efforts: vec!["high".to_string()],
            default_provider: Some("cursor".to_string()),
            default_model: Some("composer-2.5-fast".to_string()),
            default_effort: Some("high".to_string()),
            linked_skills: Vec::new(),
            persona: None,
            operating_rules: None,
            missing_skills: Vec::new(),
            babysitter_worker: None,
            stall_timeout_secs: None,
            max_run_duration_secs: None,
            sandbox_permissions: None,
            harness_args: Vec::new(),
        };

        let cfg = stage_driver_config_with_overrides(
            &worker,
            FsPath::new("/tmp/project"),
            FsPath::new("/tmp/worktree"),
            "brief",
            DriverOverrides {
                provider: Some("openai".to_string()),
                model: Some("gpt-5.5".to_string()),
                effort: Some("xhigh".to_string()),
            },
            None,
            &DriverDefaults::default(),
            None,
        );

        assert_eq!(cfg.0["provider"], "openai");
        assert_eq!(cfg.0["model"], "gpt-5.5");
        assert_eq!(cfg.0["effort"], "xhigh");
        assert_eq!(cfg.0["reasoning_effort"], "xhigh");
        assert_eq!(cfg.0["prompt_bundle_text"], "brief");
    }

    #[test]
    fn dispatch_driver_config_applies_hermes_acp_ws_defaults() {
        let token_env = "ORGASMIC_TEST_HERMES_TOKEN_APPLY_DEFAULTS";
        std::env::set_var(token_env, "fixture-token");
        let worker = StageWorker {
            id: "worker-hermes".to_string(),
            kind: WorkerKind::Implementer,
            driver: "acp-ws".to_string(),
            harness: "hermes".to_string(),
            models: Vec::new(),
            reasoning_efforts: Vec::new(),
            default_provider: None,
            default_model: None,
            default_effort: None,
            linked_skills: Vec::new(),
            persona: None,
            operating_rules: None,
            missing_skills: Vec::new(),
            babysitter_worker: None,
            stall_timeout_secs: None,
            max_run_duration_secs: None,
            sandbox_permissions: None,
            harness_args: Vec::new(),
        };
        let defaults = DriverDefaults {
            hermes: crate::config::HermesDriverDefaults {
                acp_ws: crate::config::AcpWsDriverDefaults {
                    endpoint: Some("ws://127.0.0.1:9090/acp".to_string()),
                    session_token_env: Some(token_env.to_string()),
                },
            },
        };

        let cfg = stage_driver_config_with_overrides(
            &worker,
            FsPath::new("/tmp/project"),
            FsPath::new("/tmp/worktree"),
            "brief",
            DriverOverrides::default(),
            None,
            &defaults,
            None,
        );

        assert_eq!(cfg.0["endpoint"], "ws://127.0.0.1:9090/acp");
        assert_eq!(cfg.0["session_token"], "fixture-token");
    }

    #[test]
    fn driver_defaults_only_apply_to_hermes_acp_ws() {
        let defaults = DriverDefaults {
            hermes: crate::config::HermesDriverDefaults {
                acp_ws: crate::config::AcpWsDriverDefaults {
                    endpoint: Some("ws://127.0.0.1:9090/acp".to_string()),
                    session_token_env: None,
                },
            },
        };
        let cfg = apply_driver_defaults(
            DriverConfig::from_value(json!({"endpoint": ""})),
            "acp-stdio",
            "codex",
            &defaults,
        );

        assert_eq!(cfg.0["endpoint"], "");
        assert!(cfg.0.get("session_token").is_none());
    }

    #[test]
    fn explicit_hermes_driver_config_wins_over_defaults() {
        let defaults = DriverDefaults {
            hermes: crate::config::HermesDriverDefaults {
                acp_ws: crate::config::AcpWsDriverDefaults {
                    endpoint: Some("ws://127.0.0.1:9090/acp".to_string()),
                    session_token_env: None,
                },
            },
        };
        let cfg = apply_driver_defaults(
            DriverConfig::from_value(json!({"endpoint": "ws://explicit/acp"})),
            "acp-ws",
            "hermes",
            &defaults,
        );

        assert_eq!(cfg.0["endpoint"], "ws://explicit/acp");
    }

    #[test]
    fn dispatch_driver_config_resolves_task_sandbox_over_worker() {
        use orgasmic_drivers::allowlist_from_driver_config;

        let worker = StageWorker {
            id: "worker-a".to_string(),
            kind: WorkerKind::Implementer,
            driver: "acp-stdio".to_string(),
            harness: "codex".to_string(),
            models: vec!["gpt-5.5".to_string()],
            reasoning_efforts: vec!["high".to_string()],
            default_provider: None,
            default_model: None,
            default_effort: None,
            linked_skills: Vec::new(),
            persona: None,
            operating_rules: None,
            missing_skills: Vec::new(),
            babysitter_worker: None,
            stall_timeout_secs: None,
            max_run_duration_secs: None,
            sandbox_permissions: Some(SandboxAllowlist {
                allow_exec: true,
                allow_patch: true,
                allow_network: true,
                allow_writes_outside_cwd: true,
            }),
            harness_args: Vec::new(),
        };
        let task_override = SandboxAllowlist {
            allow_exec: false,
            allow_patch: true,
            allow_network: true,
            allow_writes_outside_cwd: false,
        };
        let cfg = stage_driver_config_with_overrides(
            &worker,
            FsPath::new("/tmp/project"),
            FsPath::new("/tmp/worktree"),
            "brief",
            DriverOverrides::default(),
            Some(&task_override),
            &DriverDefaults::default(),
            None,
        );
        let allowlist = allowlist_from_driver_config(&cfg).expect("resolved sandbox csv");
        assert!(!allowlist.allow_exec);
        assert!(allowlist.allow_patch);
        assert!(!allowlist.allow_writes_outside_cwd);
    }

    #[tokio::test]
    async fn get_task_uses_last_good_indexed_body_after_source_parse_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let project_root = tmp.path().join("proj");
        seed_project(&home, &project_root, "orgasmic");
        write(
            task_file_path(&project_root, "backlog.org"),
            "#+title: sprint\n#+orgasmic_version: 1\n\n* BACKLOG TASK-PRE Pre-boot task :work:\n:PROPERTIES:\n:ID:               TASK-PRE\n:END:\n\n** Description\nLast-good detail.\n\n** Acceptance Criteria\n- [ ] Opens from the indexed snapshot.\n",
        );
        let state = direct_stage_test_state(home).await;
        write(
            task_file_path(&project_root, "backlog.org"),
            "#+title: broken\n#+orgasmic_version: 1\n\n* BACKLOG TASK-PRE Broken\n:PROPERTIES:\n:ID:               TASK-PRE\n",
        );
        state.index.refresh_project("orgasmic").await.unwrap();

        let Json(detail) = get_task(
            State(state),
            Extension(Identity::Admin),
            Path(("orgasmic".to_string(), "TASK-PRE".to_string())),
        )
        .await
        .unwrap();

        assert_eq!(detail.body.description, "Last-good detail.");
        assert_eq!(detail.body.acceptance_criteria.len(), 1);
    }

    #[test]
    fn project_not_found_error_names_cause_when_board_lists_but_index_lacks_project() {
        let mut snap = IndexSnapshot::default();
        snap.board.push(BoardEntry {
            id: "proj-ghost".to_string(),
            path: PathBuf::from("/tmp/proj-ghost"),
            branch: "main".to_string(),
            status: "active".to_string(),
        });

        let err = project_not_found_error(&snap, "proj-ghost");

        assert_eq!(err.status, StatusCode::NOT_FOUND);
        assert!(
            err.message
                .contains("registered on the board but not yet loaded"),
            "message should name the cause: {}",
            err.message
        );
        assert!(
            err.message.contains("orgasmic restart"),
            "message should name the fix: {}",
            err.message
        );
    }

    #[test]
    fn project_not_found_error_stays_bare_when_id_absent_from_board() {
        let snap = IndexSnapshot::default();

        let err = project_not_found_error(&snap, "proj-missing");

        assert_eq!(err.status, StatusCode::NOT_FOUND);
        assert_eq!(err.message, "project not found");
    }

    struct TmuxSessionGuard(String);

    impl Drop for TmuxSessionGuard {
        fn drop(&mut self) {
            let _ = Command::new("tmux")
                .args(["kill-session", "-t", &self.0])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        }
    }

    fn tmux_on_path() -> bool {
        Command::new("which")
            .arg("tmux")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }

    #[tokio::test]
    async fn tmux_ws_mock_streams_and_echoes_send_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();

        let running = crate::Daemon::run(home.clone(), test_options())
            .await
            .expect("boot daemon");
        let token = read_token(&home);

        let unauth = tokio_tungstenite::connect_async(format!(
            "ws://{}/api/ws/tmux/missing-run",
            running.addr
        ))
        .await;
        assert!(unauth.is_err(), "tmux websocket should require auth");

        // Without the mock override an unknown run id is an honest error, not
        // a mock terminal that looks like a real attach.
        let mut ws = connect_tmux_ws(running.addr, &token, "missing-run").await;
        let err = next_ws_text_containing(&mut ws, "no live run").await;
        assert!(err.contains("missing-run"));

        // Forced mock streams pane frames and echoes composer send_keys.
        std::env::set_var("ORGASMIC_TMUX_WS_MOCK", "1");
        let mut ws = connect_tmux_ws(running.addr, &token, "missing-run").await;
        let first = next_ws_text_containing(&mut ws, "[mock tmux]").await;
        assert!(first.contains("missing-run") || first.contains("tick"));

        ws.send(Message::Text(
            serde_json::json!({"type": "send_keys", "text": "hello from composer"}).to_string(),
        ))
        .await
        .unwrap();
        let echo = next_ws_text_containing(&mut ws, "hello from composer").await;
        assert!(echo.contains("[mock tmux] send_keys"));
        std::env::remove_var("ORGASMIC_TMUX_WS_MOCK");

        let _ = running.shutdown.send(());
        let _ = running.join.await;
    }

    /// Regression: graceful shutdown must complete while websocket subscribers
    /// are still connected. Pre-fix, `serve_socket` only exited on client
    /// disconnect or bus close, so axum's drain phase could deadlock behind an
    /// open subscription and `join` hung indefinitely under parallel test load.
    #[tokio::test]
    async fn shutdown_completes_with_open_websocket_subscriber() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();

        let running = crate::Daemon::run(home.clone(), test_options())
            .await
            .expect("boot daemon");
        let token = read_token(&home);
        // Keep the subscription open across shutdown — never close it client-side.
        let _ws = connect_ws(running.addr, &token, "graph").await;

        let _ = running.shutdown.send(());
        tokio::time::timeout(Duration::from_secs(10), running.join)
            .await
            .expect("daemon join must complete while a ws client is still connected")
            .expect("daemon join");
    }

    #[tokio::test]
    async fn graph_node_created_emits_typed_event() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let project_root = tmp.path().join("proj");
        seed_project(&home, &project_root, "orgasmic");

        let running = crate::Daemon::run(home.clone(), test_options())
            .await
            .expect("boot daemon");
        let token = read_token(&home);
        let mut ws = connect_ws(running.addr, &token, "graph").await;
        let client = reqwest::Client::new();

        let resp = client
            .post(format!("http://{}/api/architecture", running.addr))
            .bearer_auth(&token)
            .json(&serde_json::json!({
                "project": "orgasmic",
                "request_id": "graph-node-created-test",
                "title": "Typed streams architecture node"
            }))
            .send()
            .await
            .unwrap();
        let status = resp.status();
        let body: Value = resp.json().await.unwrap();
        assert!(status.is_success(), "create architecture: {status} {body}");
        let minted_arch = body["id"].as_str().expect("minted arch id");

        let event = next_event_kind(&mut ws, "graph_node_created").await;
        let payload = &event["payload"];
        assert_eq!(payload["project_id"], "orgasmic");
        assert_eq!(payload["layer"], "architecture");
        assert_eq!(payload["node_id"], minted_arch);
        assert!(!payload["tx_id"].as_str().unwrap().is_empty());

        let _ = running.shutdown.send(());
        let _ = running.join.await;
    }

    #[tokio::test]
    async fn graph_node_revised_emits_typed_event() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let project_root = tmp.path().join("proj");
        seed_project(&home, &project_root, "orgasmic");
        write(
            project_root.join(".orgasmic/decisions.org"),
            "#+title: decisions\n#+orgasmic_version: 1\n\n* dec_001 Choice :topic:\n:PROPERTIES:\n:ID:                 dec_001\n:DECIDES:\n:END:\n",
        );

        let running = crate::Daemon::run(home.clone(), test_options())
            .await
            .expect("boot daemon");
        let token = read_token(&home);
        let mut ws = connect_ws(running.addr, &token, "graph").await;
        let client = reqwest::Client::new();

        let resp = client
            .post(format!("http://{}/api/decisions/dec_001", running.addr))
            .bearer_auth(&token)
            .json(&serde_json::json!({
                "project": "orgasmic",
                "request_id": "graph-node-revised-test",
                "action": "accept"
            }))
            .send()
            .await
            .unwrap();
        let status = resp.status();
        let body: Value = resp.json().await.unwrap();
        assert!(status.is_success(), "revise decision: {status} {body}");

        let event = next_event_kind(&mut ws, "graph_node_revised").await;
        let payload = &event["payload"];
        assert_eq!(payload["project_id"], "orgasmic");
        assert_eq!(payload["layer"], "decision");
        assert_eq!(payload["node_id"], "dec_001");
        assert_eq!(payload["action"], "accept");
        assert!(!payload["tx_id"].as_str().unwrap().is_empty());

        let _ = running.shutdown.send(());
        let _ = running.join.await;
    }

    #[tokio::test]
    async fn decision_parent_contract_and_reparent_tx_are_enforced() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let project_root = tmp.path().join("proj");
        seed_project(&home, &project_root, "orgasmic");
        let decisions_path = project_root.join(".orgasmic/decisions.org");
        write(
            decisions_path.clone(),
            "#+title: decisions\n#+orgasmic_version: 1\n\n\
* dec_AAAAA Root A\n\
:PROPERTIES:\n\
:ID:                 dec_AAAAA\n\
:END:\n\
\n\
* dec_BBBBB Child B\n\
:PROPERTIES:\n\
:ID:                 dec_BBBBB\n\
:PARENT:             dec_AAAAA\n\
:END:\n\
\n\
* dec_CCCCC Root C\n\
:PROPERTIES:\n\
:ID:                 dec_CCCCC\n\
:END:\n",
        );

        let running = crate::Daemon::run(home.clone(), test_options())
            .await
            .expect("boot daemon");
        let token = read_token(&home);
        let client = reqwest::Client::new();
        let base = format!("http://{}", running.addr);

        let missing_parent = client
            .post(format!("{base}/api/decisions"))
            .bearer_auth(&token)
            .json(&serde_json::json!({
                "project": "orgasmic",
                "id": "dec_MISS1",
                "title": "Missing parent",
                "properties": { "PARENT": "dec_NOPE1" }
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(missing_parent.status(), reqwest::StatusCode::BAD_REQUEST);

        let cross_class = client
            .post(format!("{base}/api/decisions"))
            .bearer_auth(&token)
            .json(&serde_json::json!({
                "project": "orgasmic",
                "id": "dec_XCLSS",
                "title": "Cross class parent",
                "properties": { "PARENT": "TASK-PRE" }
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(cross_class.status(), reqwest::StatusCode::BAD_REQUEST);

        let self_parent = client
            .post(format!("{base}/api/decisions"))
            .bearer_auth(&token)
            .json(&serde_json::json!({
                "project": "orgasmic",
                "id": "dec_SELF1",
                "title": "Self parent",
                "properties": { "PARENT": "dec_SELF1" }
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(self_parent.status(), reqwest::StatusCode::BAD_REQUEST);

        let cycle = client
            .post(format!("{base}/api/decisions/dec_AAAAA"))
            .bearer_auth(&token)
            .json(&serde_json::json!({
                "project": "orgasmic",
                "properties": { "PARENT": "dec_BBBBB" }
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(cycle.status(), reqwest::StatusCode::BAD_REQUEST);

        let reparent = client
            .post(format!("{base}/api/decisions/dec_BBBBB"))
            .bearer_auth(&token)
            .json(&serde_json::json!({
                "project": "orgasmic",
                "request_id": "decision-reparent-test",
                "properties": { "PARENT": "dec_CCCCC" }
            }))
            .send()
            .await
            .unwrap();
        let status = reparent.status();
        let body: Value = reparent.json().await.unwrap();
        assert!(status.is_success(), "reparent failed: {status} {body}");
        assert_eq!(body["action"], "reparent");
        let after = std::fs::read_to_string(&decisions_path).unwrap();
        assert!(
            after.contains(":PARENT:             dec_CCCCC\n"),
            "{after}"
        );

        let blocked_delete = client
            .post(format!("{base}/api/decisions/dec_CCCCC"))
            .bearer_auth(&token)
            .json(&serde_json::json!({
                "project": "orgasmic",
                "action": "delete"
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(blocked_delete.status(), reqwest::StatusCode::BAD_REQUEST);

        let mut tx_text = String::new();
        for entry in std::fs::read_dir(project_root.join(".orgasmic/tx")).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().and_then(|ext| ext.to_str()) == Some("org") {
                tx_text.push_str(&std::fs::read_to_string(path).unwrap());
            }
        }
        assert!(tx_text.contains("graph.decision.reparent"), "{tx_text}");

        let _ = running.shutdown.send(());
        let _ = running.join.await;
    }

    /// Battle-test F5: `/graph/parse-errors` must name which project a
    /// dangling reference belongs to (not just a global count), and
    /// `/reindex/:project` must clear the per-project count after a fix
    /// without a daemon restart.
    #[tokio::test]
    async fn parse_errors_route_attributes_project_and_reindex_clears_it() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let project_root = tmp.path().join("proj");
        seed_project(&home, &project_root, "orgasmic");
        let glossary_path = project_root.join(".orgasmic/glossary.org");
        write(
            glossary_path.clone(),
            "#+title: glossary\n#+orgasmic_version: 1\n\n* term_A A term\n:PROPERTIES:\n:ID:               term_A\n:RELATES_TO:       missing-slug\n:END:\n",
        );

        let running = crate::Daemon::run(home.clone(), test_options())
            .await
            .expect("boot daemon");
        let token = read_token(&home);
        let client = reqwest::Client::new();
        let base = format!("http://{}", running.addr);

        let errors: Value = client
            .get(format!("{base}/api/graph/parse-errors"))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let errors = errors.as_array().expect("parse-errors array");
        let found = errors
            .iter()
            .find(|e| {
                e["message"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("dangling reference `missing-slug`")
            })
            .expect("dangling reference surfaces as a parse error");
        assert_eq!(found["project_id"], "orgasmic");
        assert!(found["path"]
            .as_str()
            .unwrap_or_default()
            .ends_with("glossary.org"));

        let status: Value = client
            .get(format!("{base}/api/daemon/status"))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(
            status["parse_errors"].as_u64().unwrap_or(0) >= 1,
            "global aggregate count is unchanged by the enrichment: {status}"
        );

        // Fix the dangling reference on disk, then reindex just this
        // project — no daemon restart — and confirm the count drops to zero.
        write(
            glossary_path,
            "#+title: glossary\n#+orgasmic_version: 1\n\n* term_A A term\n:PROPERTIES:\n:ID:               term_A\n:END:\n",
        );
        let reindexed: Value = client
            .post(format!("{base}/api/reindex/orgasmic"))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(reindexed["projects"]["orgasmic"], 0, "{reindexed}");

        let errors_after: Value = client
            .get(format!("{base}/api/graph/parse-errors"))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(errors_after.as_array().unwrap().iter().all(|e| {
            !e["message"]
                .as_str()
                .unwrap_or_default()
                .contains("missing-slug")
        }));

        let _ = running.shutdown.send(());
        let _ = running.join.await;
    }

    #[tokio::test]
    async fn reindex_all_route_rebuilds_and_reports_per_project_counts() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let project_root = tmp.path().join("proj");
        seed_project(&home, &project_root, "orgasmic");

        let running = crate::Daemon::run(home.clone(), test_options())
            .await
            .expect("boot daemon");
        let token = read_token(&home);
        let client = reqwest::Client::new();
        let base = format!("http://{}", running.addr);

        let reindexed: Value = client
            .post(format!("{base}/api/reindex"))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(reindexed["projects"]["orgasmic"], 0, "{reindexed}");
        assert_eq!(reindexed["total_parse_errors"], 0, "{reindexed}");

        let _ = running.shutdown.send(());
        let _ = running.join.await;
    }

    #[tokio::test]
    async fn reindex_unknown_project_is_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let project_root = tmp.path().join("proj");
        seed_project(&home, &project_root, "orgasmic");

        let running = crate::Daemon::run(home.clone(), test_options())
            .await
            .expect("boot daemon");
        let token = read_token(&home);
        let client = reqwest::Client::new();
        let base = format!("http://{}", running.addr);

        let resp = client
            .post(format!("{base}/api/reindex/does-not-exist"))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::NOT_FOUND);

        let _ = running.shutdown.send(());
        let _ = running.join.await;
    }

    /// Battle-test F4 regression: `glossary create --relates-to "Semantic
    /// Routing Model, Expanded Routing Graph"` (prose, not ids) used to be
    /// accepted at write time and then poisoned the whole index with one
    /// dangling-ref parse error per word. It must now be rejected at write
    /// time, naming the property, an offending token, and the expected
    /// format, with nothing written to glossary.org.
    #[tokio::test]
    async fn glossary_create_rejects_prose_relates_to_at_write_time() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let project_root = tmp.path().join("proj");
        seed_project(&home, &project_root, "orgasmic");
        let glossary_path = project_root.join(".orgasmic/glossary.org");
        assert!(!glossary_path.exists());

        let running = crate::Daemon::run(home.clone(), test_options())
            .await
            .expect("boot daemon");
        let token = read_token(&home);
        let client = reqwest::Client::new();
        let base = format!("http://{}", running.addr);

        let resp = client
            .post(format!("{base}/api/glossary"))
            .bearer_auth(&token)
            .json(&serde_json::json!({
                "project": "orgasmic",
                "title": "Semantic Routing Model",
                "properties": {
                    "RELATES_TO": "Semantic Routing Model, Expanded Routing Graph"
                }
            }))
            .send()
            .await
            .unwrap();
        let status = resp.status();
        let body: Value = resp.json().await.unwrap();
        assert_eq!(status, reqwest::StatusCode::BAD_REQUEST, "{body}");
        let message = body["error"].as_str().unwrap_or_default();
        assert!(message.contains("RELATES_TO"), "{message}");
        assert!(message.contains("Semantic"), "{message}");
        assert!(message.contains("space-separated node ids"), "{message}");

        assert!(
            !glossary_path.exists(),
            "write-time rejection must not create glossary.org"
        );

        let _ = running.shutdown.send(());
        let _ = running.join.await;
    }

    #[tokio::test]
    async fn node_prop_set_rejects_unresolved_reference_token() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let project_root = tmp.path().join("proj");
        seed_project(&home, &project_root, "orgasmic");
        let decisions_path = project_root.join(".orgasmic/decisions.org");
        write(
            decisions_path.clone(),
            "#+title: decisions\n#+orgasmic_version: 1\n\n* dec_001 Choice\n:PROPERTIES:\n:ID:                 dec_001\n:END:\n",
        );
        let original = std::fs::read_to_string(&decisions_path).unwrap();

        let running = crate::Daemon::run(home.clone(), test_options())
            .await
            .expect("boot daemon");
        let token = read_token(&home);
        let client = reqwest::Client::new();
        let base = format!("http://{}", running.addr);

        let doc: Value = client
            .get(format!("{base}/api/org/node"))
            .bearer_auth(&token)
            .query(&[("project", "orgasmic"), ("id", "dec_001")])
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let base_version = doc["source"]["base_version"].as_str().unwrap();

        let resp = client
            .post(format!("{base}/api/org/node/dec_001/edit"))
            .bearer_auth(&token)
            .json(&serde_json::json!({
                "project": "orgasmic",
                "base_version": base_version,
                "ops": [{ "op": "set_property", "key": "RELATES_TO", "value": "dec_FAKE99" }]
            }))
            .send()
            .await
            .unwrap();
        let status = resp.status();
        let body: Value = resp.json().await.unwrap();
        assert_eq!(status, reqwest::StatusCode::BAD_REQUEST, "{body}");
        let message = body["error"].as_str().unwrap_or_default();
        assert!(message.contains("RELATES_TO"), "{message}");
        assert!(message.contains("dec_FAKE99"), "{message}");
        assert!(message.contains("space-separated node ids"), "{message}");

        let after = std::fs::read_to_string(&decisions_path).unwrap();
        assert_eq!(after, original, "write-nothing on reject");

        let _ = running.shutdown.send(());
        let _ = running.join.await;
    }

    #[tokio::test]
    async fn node_prop_set_force_writes_unresolved_reference_token_anyway() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let project_root = tmp.path().join("proj");
        seed_project(&home, &project_root, "orgasmic");
        let decisions_path = project_root.join(".orgasmic/decisions.org");
        write(
            decisions_path.clone(),
            "#+title: decisions\n#+orgasmic_version: 1\n\n* dec_001 Choice\n:PROPERTIES:\n:ID:                 dec_001\n:END:\n",
        );

        let running = crate::Daemon::run(home.clone(), test_options())
            .await
            .expect("boot daemon");
        let token = read_token(&home);
        let client = reqwest::Client::new();
        let base = format!("http://{}", running.addr);

        let doc: Value = client
            .get(format!("{base}/api/org/node"))
            .bearer_auth(&token)
            .query(&[("project", "orgasmic"), ("id", "dec_001")])
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let base_version = doc["source"]["base_version"].as_str().unwrap();

        let resp = client
            .post(format!("{base}/api/org/node/dec_001/edit"))
            .bearer_auth(&token)
            .json(&serde_json::json!({
                "project": "orgasmic",
                "base_version": base_version,
                "force": true,
                "ops": [{ "op": "set_property", "key": "RELATES_TO", "value": "dec_FAKE99" }]
            }))
            .send()
            .await
            .unwrap();
        let status = resp.status();
        let body: Value = resp.json().await.unwrap();
        assert!(status.is_success(), "force write: {status} {body}");

        let after = std::fs::read_to_string(&decisions_path).unwrap();
        assert!(
            after
                .lines()
                .any(|line| line.contains("RELATES_TO") && line.contains("dec_FAKE99")),
            "{after}"
        );

        let _ = running.shutdown.send(());
        let _ = running.join.await;
    }

    #[tokio::test]
    async fn node_prop_set_accepts_resolvable_reference_token() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let project_root = tmp.path().join("proj");
        seed_project(&home, &project_root, "orgasmic");
        let decisions_path = project_root.join(".orgasmic/decisions.org");
        write(
            decisions_path.clone(),
            "#+title: decisions\n#+orgasmic_version: 1\n\n\
* dec_001 Choice\n\
:PROPERTIES:\n\
:ID:                 dec_001\n\
:END:\n\
\n\
* dec_002 Other choice\n\
:PROPERTIES:\n\
:ID:                 dec_002\n\
:END:\n",
        );

        let running = crate::Daemon::run(home.clone(), test_options())
            .await
            .expect("boot daemon");
        let token = read_token(&home);
        let client = reqwest::Client::new();
        let base = format!("http://{}", running.addr);

        let doc: Value = client
            .get(format!("{base}/api/org/node"))
            .bearer_auth(&token)
            .query(&[("project", "orgasmic"), ("id", "dec_001")])
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let base_version = doc["source"]["base_version"].as_str().unwrap();

        let resp = client
            .post(format!("{base}/api/org/node/dec_001/edit"))
            .bearer_auth(&token)
            .json(&serde_json::json!({
                "project": "orgasmic",
                "base_version": base_version,
                "ops": [{ "op": "set_property", "key": "RELATES_TO", "value": "dec_002" }]
            }))
            .send()
            .await
            .unwrap();
        let status = resp.status();
        let body: Value = resp.json().await.unwrap();
        assert!(status.is_success(), "resolvable ref: {status} {body}");

        let after = std::fs::read_to_string(&decisions_path).unwrap();
        assert!(
            after
                .lines()
                .any(|line| line.contains("RELATES_TO") && line.contains("dec_002")),
            "{after}"
        );

        let _ = running.shutdown.send(());
        let _ = running.join.await;
    }

    /// TASK-KSD6D sink 1: `mutate_graph_heading` (decision/architecture/
    /// glossary revise) must front-run the same unresolvable-reference-token
    /// class TASK-4T80X guarded on create + node-prop-set, write-nothing on
    /// reject.
    #[tokio::test]
    async fn decision_revise_rejects_unresolved_reference_token() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let project_root = tmp.path().join("proj");
        seed_project(&home, &project_root, "orgasmic");
        let decisions_path = project_root.join(".orgasmic/decisions.org");
        write(
            decisions_path.clone(),
            "#+title: decisions\n#+orgasmic_version: 1\n\n* dec_001 Choice\n:PROPERTIES:\n:ID:                 dec_001\n:END:\n",
        );
        let original = std::fs::read_to_string(&decisions_path).unwrap();

        let running = crate::Daemon::run(home.clone(), test_options())
            .await
            .expect("boot daemon");
        let token = read_token(&home);
        let client = reqwest::Client::new();
        let base = format!("http://{}", running.addr);

        let resp = client
            .post(format!("{base}/api/decisions/dec_001"))
            .bearer_auth(&token)
            .json(&serde_json::json!({
                "project": "orgasmic",
                "properties": { "RELATES_TO": "some unresolvable prose" }
            }))
            .send()
            .await
            .unwrap();
        let status = resp.status();
        let body: Value = resp.json().await.unwrap();
        assert_eq!(status, reqwest::StatusCode::BAD_REQUEST, "{body}");
        let message = body["error"].as_str().unwrap_or_default();
        assert!(message.contains("RELATES_TO"), "{message}");
        assert!(message.contains("some"), "{message}");
        assert!(message.contains("space-separated node ids"), "{message}");

        let after = std::fs::read_to_string(&decisions_path).unwrap();
        assert_eq!(after, original, "write-nothing on reject");

        let _ = running.shutdown.send(());
        let _ = running.join.await;
    }

    #[tokio::test]
    async fn decision_revise_force_writes_unresolved_reference_token_anyway() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let project_root = tmp.path().join("proj");
        seed_project(&home, &project_root, "orgasmic");
        let decisions_path = project_root.join(".orgasmic/decisions.org");
        write(
            decisions_path.clone(),
            "#+title: decisions\n#+orgasmic_version: 1\n\n* dec_001 Choice\n:PROPERTIES:\n:ID:                 dec_001\n:END:\n",
        );

        let running = crate::Daemon::run(home.clone(), test_options())
            .await
            .expect("boot daemon");
        let token = read_token(&home);
        let client = reqwest::Client::new();
        let base = format!("http://{}", running.addr);

        let resp = client
            .post(format!("{base}/api/decisions/dec_001"))
            .bearer_auth(&token)
            .json(&serde_json::json!({
                "project": "orgasmic",
                "force": true,
                "properties": { "RELATES_TO": "dec_FAKE99" }
            }))
            .send()
            .await
            .unwrap();
        let status = resp.status();
        let body: Value = resp.json().await.unwrap();
        assert!(status.is_success(), "force write: {status} {body}");

        let after = std::fs::read_to_string(&decisions_path).unwrap();
        assert!(
            after
                .lines()
                .any(|line| line.contains("RELATES_TO") && line.contains("dec_FAKE99")),
            "{after}"
        );

        let _ = running.shutdown.send(());
        let _ = running.join.await;
    }

    #[tokio::test]
    async fn decision_revise_accepts_resolvable_reference_and_non_reference_property() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let project_root = tmp.path().join("proj");
        seed_project(&home, &project_root, "orgasmic");
        let decisions_path = project_root.join(".orgasmic/decisions.org");
        write(
            decisions_path.clone(),
            "#+title: decisions\n#+orgasmic_version: 1\n\n\
* dec_001 Choice\n\
:PROPERTIES:\n\
:ID:                 dec_001\n\
:END:\n\
\n\
* dec_002 Other choice\n\
:PROPERTIES:\n\
:ID:                 dec_002\n\
:END:\n",
        );

        let running = crate::Daemon::run(home.clone(), test_options())
            .await
            .expect("boot daemon");
        let token = read_token(&home);
        let client = reqwest::Client::new();
        let base = format!("http://{}", running.addr);

        let resp = client
            .post(format!("{base}/api/decisions/dec_001"))
            .bearer_auth(&token)
            .json(&serde_json::json!({
                "project": "orgasmic",
                "properties": { "RELATES_TO": "dec_002", "PRIORITY": "P1" }
            }))
            .send()
            .await
            .unwrap();
        let status = resp.status();
        let body: Value = resp.json().await.unwrap();
        assert!(
            status.is_success(),
            "resolvable ref + plain property: {status} {body}"
        );

        let after = std::fs::read_to_string(&decisions_path).unwrap();
        assert!(
            after
                .lines()
                .any(|line| line.contains("RELATES_TO") && line.contains("dec_002")),
            "{after}"
        );
        assert!(
            after
                .lines()
                .any(|line| line.contains("PRIORITY") && line.contains("P1")),
            "{after}"
        );

        let _ = running.shutdown.send(());
        let _ = running.join.await;
    }

    #[tokio::test]
    async fn architecture_revise_rejects_unresolved_reference_token() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let project_root = tmp.path().join("proj");
        seed_project(&home, &project_root, "orgasmic");
        let architecture_path = project_root.join(".orgasmic/architecture.org");
        write(
            architecture_path.clone(),
            "#+title: architecture\n#+orgasmic_version: 1\n\n* arch_001 Component\n:PROPERTIES:\n:ID:                 arch_001\n:DEPENDS_ON:\n:MOTIVATED_BY:\n:END:\n",
        );
        let original = std::fs::read_to_string(&architecture_path).unwrap();

        let running = crate::Daemon::run(home.clone(), test_options())
            .await
            .expect("boot daemon");
        let token = read_token(&home);
        let client = reqwest::Client::new();
        let base = format!("http://{}", running.addr);

        let resp = client
            .post(format!("{base}/api/architecture/arch_001"))
            .bearer_auth(&token)
            .json(&serde_json::json!({
                "project": "orgasmic",
                "properties": { "DEPENDS_ON": "Expanded Routing Graph" }
            }))
            .send()
            .await
            .unwrap();
        let status = resp.status();
        let body: Value = resp.json().await.unwrap();
        assert_eq!(status, reqwest::StatusCode::BAD_REQUEST, "{body}");
        let message = body["error"].as_str().unwrap_or_default();
        assert!(message.contains("DEPENDS_ON"), "{message}");
        assert!(message.contains("space-separated node ids"), "{message}");

        let after = std::fs::read_to_string(&architecture_path).unwrap();
        assert_eq!(after, original, "write-nothing on reject");

        let _ = running.shutdown.send(());
        let _ = running.join.await;
    }

    /// TASK-KSD6D sink 2: `post_task_create` must front-run the same
    /// unresolvable-reference-token class on DEPENDS_ON/IMPLEMENTS at
    /// creation time, write-nothing on reject.
    #[tokio::test]
    async fn task_create_rejects_unresolved_reference_token() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let project_root = tmp.path().join("proj");
        seed_project(&home, &project_root, "orgasmic");
        let backlog_path = task_file_path(&project_root, "backlog.org");
        let original = std::fs::read_to_string(&backlog_path).unwrap();

        let running = crate::Daemon::run(home.clone(), test_options())
            .await
            .expect("boot daemon");
        let token = read_token(&home);
        let client = reqwest::Client::new();
        let base = format!("http://{}", running.addr);

        let resp = client
            .post(format!("{base}/api/projects/orgasmic/tasks"))
            .bearer_auth(&token)
            .json(&serde_json::json!({
                "title": "New task",
                "properties": { "DEPENDS_ON": "TASK-FAKE1" }
            }))
            .send()
            .await
            .unwrap();
        let status = resp.status();
        let body: Value = resp.json().await.unwrap();
        assert_eq!(status, reqwest::StatusCode::BAD_REQUEST, "{body}");
        let message = body["error"].as_str().unwrap_or_default();
        assert!(message.contains("DEPENDS_ON"), "{message}");
        assert!(message.contains("TASK-FAKE1"), "{message}");
        assert!(message.contains("space-separated node ids"), "{message}");

        let after = std::fs::read_to_string(&backlog_path).unwrap();
        assert_eq!(after, original, "write-nothing on reject");

        let _ = running.shutdown.send(());
        let _ = running.join.await;
    }

    #[tokio::test]
    async fn task_create_force_writes_unresolved_reference_token_anyway() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let project_root = tmp.path().join("proj");
        seed_project(&home, &project_root, "orgasmic");
        let backlog_path = task_file_path(&project_root, "backlog.org");

        let running = crate::Daemon::run(home.clone(), test_options())
            .await
            .expect("boot daemon");
        let token = read_token(&home);
        let client = reqwest::Client::new();
        let base = format!("http://{}", running.addr);

        let resp = client
            .post(format!("{base}/api/projects/orgasmic/tasks"))
            .bearer_auth(&token)
            .json(&serde_json::json!({
                "title": "New task",
                "force": true,
                "properties": { "DEPENDS_ON": "TASK-FAKE1" }
            }))
            .send()
            .await
            .unwrap();
        let status = resp.status();
        let body: Value = resp.json().await.unwrap();
        assert!(status.is_success(), "force write: {status} {body}");

        let after = std::fs::read_to_string(&backlog_path).unwrap();
        assert!(
            after
                .lines()
                .any(|line| line.contains("DEPENDS_ON") && line.contains("TASK-FAKE1")),
            "{after}"
        );

        let _ = running.shutdown.send(());
        let _ = running.join.await;
    }

    #[tokio::test]
    async fn task_create_accepts_resolvable_reference_and_non_reference_property() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let project_root = tmp.path().join("proj");
        seed_project(&home, &project_root, "orgasmic");
        let backlog_path = task_file_path(&project_root, "backlog.org");

        let running = crate::Daemon::run(home.clone(), test_options())
            .await
            .expect("boot daemon");
        let token = read_token(&home);
        let client = reqwest::Client::new();
        let base = format!("http://{}", running.addr);

        let resp = client
            .post(format!("{base}/api/projects/orgasmic/tasks"))
            .bearer_auth(&token)
            .json(&serde_json::json!({
                "title": "New task",
                "properties": { "DEPENDS_ON": "TASK-PRE", "PRIORITY": "P1" }
            }))
            .send()
            .await
            .unwrap();
        let status = resp.status();
        let body: Value = resp.json().await.unwrap();
        assert!(
            status.is_success(),
            "resolvable ref + plain property: {status} {body}"
        );

        let after = std::fs::read_to_string(&backlog_path).unwrap();
        assert!(
            after
                .lines()
                .any(|line| line.contains("DEPENDS_ON") && line.contains("TASK-PRE")),
            "{after}"
        );
        assert!(
            after
                .lines()
                .any(|line| line.contains("PRIORITY") && line.contains("P1")),
            "{after}"
        );

        let _ = running.shutdown.send(());
        let _ = running.join.await;
    }

    #[tokio::test]
    async fn org_node_read_and_edit_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let project_root = tmp.path().join("proj");
        seed_project(&home, &project_root, "orgasmic");
        let decisions_path = project_root.join(".orgasmic/decisions.org");
        write(
            decisions_path.clone(),
            "#+title: decisions\n#+orgasmic_version: 1\n\n\
* dec_001 First choice    :topic-a:\n\
:PROPERTIES:\n:ID:            dec_001\n:GLOSSARY_REFS: alpha beta\n:END:\n\
** Context\nOriginal context.\n** Decision\nOriginal decision.\n** Consequences\nOriginal consequences.\n\n\
* dec_002 Second choice    :topic-b:\n\
:PROPERTIES:\n:ID:            dec_002\n:GLOSSARY_REFS: gamma\n:END:\n\
** Context\nSecond context.\n** Decision\nSecond decision.\n** Consequences\nSecond consequences.\n",
        );

        let original = std::fs::read_to_string(&decisions_path).unwrap();
        let dec_002_block = &original[original.find("* dec_002").unwrap()..];

        let running = crate::Daemon::run(home.clone(), test_options())
            .await
            .expect("boot daemon");
        let token = read_token(&home);
        let client = reqwest::Client::new();
        let base = format!("http://{}", running.addr);

        // Read the structured node document.
        let doc: Value = client
            .get(format!("{base}/api/org/node"))
            .bearer_auth(&token)
            .query(&[("project", "orgasmic"), ("id", "dec_001")])
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(doc["title"], "First choice");
        assert_eq!(doc["tags"], serde_json::json!(["topic-a"]));
        let context = doc["sections"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["title"] == "Context")
            .unwrap();
        assert_eq!(context["body"], "Original context.");
        let base_version = doc["source"]["base_version"].as_str().unwrap().to_string();
        assert!(!base_version.is_empty());

        // Apply a batch of edit ops: section body, glossary refs, tags. The
        // glossary refs are placeholder tokens (not real node ids), so this
        // uses --force (TASK-4T80X write-time reference guard) to keep
        // testing the unrelated edit-round-trip mechanics unchanged.
        let resp = client
            .post(format!("{base}/api/org/node/dec_001/edit?json=true"))
            .bearer_auth(&token)
            .json(&serde_json::json!({
                "project": "orgasmic",
                "request_id": "node-edit-test",
                "base_version": base_version,
                "force": true,
                "ops": [
                    { "op": "set_section_body", "title": "Context", "body": "New context body." },
                    { "op": "set_property", "key": "GLOSSARY_REFS", "value": "alpha beta gamma" },
                    { "op": "set_tags", "tags": ["topic-a", "extra"] }
                ]
            }))
            .send()
            .await
            .unwrap();
        let status = resp.status();
        let updated: Value = resp.json().await.unwrap();
        assert!(status.is_success(), "edit node: {status} {updated}");

        // Response reflects the new state with a fresh base_version.
        assert_eq!(updated["tags"], serde_json::json!(["topic-a", "extra"]));
        let new_context = updated["sections"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["title"] == "Context")
            .unwrap();
        assert_eq!(new_context["body"], "New context body.");
        let glossary = updated["properties"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["key"] == "GLOSSARY_REFS")
            .unwrap();
        assert_eq!(glossary["value"], "alpha beta gamma");
        let new_version = updated["source"]["base_version"].as_str().unwrap();
        assert_ne!(new_version, base_version);

        // On disk: only the touched bytes changed.
        let after = std::fs::read_to_string(&decisions_path).unwrap();
        assert!(after.contains("* dec_001 First choice    :topic-a:extra:\n"));
        assert!(after.contains("** Context\nNew context body.\n** Decision\nOriginal decision.\n"));
        assert!(after.contains(":GLOSSARY_REFS: alpha beta gamma\n"));
        // The sibling decision is byte-identical (separator preserved).
        assert!(
            after.contains(dec_002_block),
            "dec_002 block must be untouched"
        );

        // A stale base_version is rejected with 409.
        let conflict = client
            .post(format!("{base}/api/org/node/dec_001/edit"))
            .bearer_auth(&token)
            .json(&serde_json::json!({
                "project": "orgasmic",
                "base_version": base_version,
                "ops": [ { "op": "set_section_body", "title": "Context", "body": "stale" } ]
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(conflict.status(), reqwest::StatusCode::CONFLICT);

        let _ = running.shutdown.send(());
        let _ = running.join.await;
    }

    #[tokio::test]
    async fn org_node_title_edit_repairs_tokenless_heading_from_drawer_id() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let project_root = tmp.path().join("proj");
        seed_project(&home, &project_root, "orgasmic");
        let decisions_path = project_root.join(".orgasmic/decisions.org");
        write(
            decisions_path.clone(),
            "#+title: decisions\n#+orgasmic_version: 1\n\n\
* Tokenless decision    :drift:\n\
:PROPERTIES:\n:ID:            dec_FIXME\n:END:\n\
** Context\nNeeds repair.\n",
        );

        let running = crate::Daemon::run(home.clone(), test_options())
            .await
            .expect("boot daemon");
        let token = read_token(&home);
        let client = reqwest::Client::new();
        let base = format!("http://{}", running.addr);

        let doc: Value = client
            .get(format!("{base}/api/org/node"))
            .bearer_auth(&token)
            .query(&[("project", "orgasmic"), ("id", "dec_FIXME")])
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(doc["title"], "Tokenless decision");
        let base_version = doc["source"]["base_version"].as_str().unwrap().to_string();

        let resp = client
            .post(format!("{base}/api/org/node/dec_FIXME/edit?json=true"))
            .bearer_auth(&token)
            .json(&serde_json::json!({
                "project": "orgasmic",
                "base_version": base_version,
                "ops": [{ "op": "set_title", "title": "Repaired decision" }]
            }))
            .send()
            .await
            .unwrap();
        let status = resp.status();
        let updated: Value = resp.json().await.unwrap();
        assert!(status.is_success(), "repair title: {status} {updated}");
        assert_eq!(updated["title"], "Repaired decision");

        let after = std::fs::read_to_string(&decisions_path).unwrap();
        assert!(after.contains("* dec_FIXME Repaired decision    :drift:\n"));
        assert!(after.contains(":ID:            dec_FIXME\n"));

        let _ = running.shutdown.send(());
        let _ = running.join.await;
    }

    #[tokio::test]
    async fn org_node_project_layer_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let project_root = tmp.path().join("proj");
        seed_project(&home, &project_root, "orgasmic");
        // The `project` layer cannot be inferred from the id prefix, so this
        // node is only reachable via an explicit `kind=project` selector.
        let project_path = project_root.join(".orgasmic/project.org");
        write(
            project_path.clone(),
            "#+title: orgasmic\n#+orgasmic_version: 1\n\n\
* PROJECT orgasmic\n:PROPERTIES:\n:ID:               orgasmic\n:END:\n\n\
** Mission\nCoordinate agent work.\n\n** Operating Constraints\n- Stay greenfield.\n",
        );

        let running = crate::Daemon::run(home.clone(), test_options())
            .await
            .expect("boot daemon");
        let token = read_token(&home);
        let client = reqwest::Client::new();
        let base = format!("http://{}", running.addr);

        // Without `kind`, the id falls back to the glossary layer and misses.
        let missed = client
            .get(format!("{base}/api/org/node"))
            .bearer_auth(&token)
            .query(&[("project", "orgasmic"), ("id", "orgasmic")])
            .send()
            .await
            .unwrap();
        assert_eq!(missed.status(), reqwest::StatusCode::NOT_FOUND);

        // With `kind=project`, the PROJECT heading's sections come back.
        let doc: Value = client
            .get(format!("{base}/api/org/node"))
            .bearer_auth(&token)
            .query(&[
                ("project", "orgasmic"),
                ("id", "orgasmic"),
                ("kind", "project"),
            ])
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(doc["kind"], "project");
        let sections = doc["sections"].as_array().unwrap();
        let titles: Vec<&str> = sections
            .iter()
            .map(|s| s["title"].as_str().unwrap())
            .collect();
        assert_eq!(titles, vec!["Mission", "Operating Constraints"]);
        let base_version = doc["source"]["base_version"].as_str().unwrap().to_string();

        // Edit a section through the same layer selector.
        let resp = client
            .post(format!("{base}/api/org/node/orgasmic/edit?json=true"))
            .bearer_auth(&token)
            .json(&serde_json::json!({
                "project": "orgasmic",
                "kind": "project",
                "base_version": base_version,
                "ops": [
                    { "op": "set_section_body", "title": "Mission", "body": "Coordinate AI-agent work." }
                ]
            }))
            .send()
            .await
            .unwrap();
        let status = resp.status();
        let updated: Value = resp.json().await.unwrap();
        assert!(status.is_success(), "edit project node: {status} {updated}");
        let mission = updated["sections"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["title"] == "Mission")
            .unwrap();
        assert_eq!(mission["body"], "Coordinate AI-agent work.");

        let after = std::fs::read_to_string(&project_path).unwrap();
        assert!(after.contains("** Mission\nCoordinate AI-agent work.\n"));
        assert!(after.contains("** Operating Constraints\n- Stay greenfield.\n"));

        let _ = running.shutdown.send(());
        let _ = running.join.await;
    }

    #[tokio::test]
    async fn org_node_config_layer_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let project_root = tmp.path().join("proj");
        seed_project(&home, &project_root, "orgasmic");
        // config.org shares the project's :ID:, so — like `project` — it is
        // only reachable via an explicit `kind=config` selector.
        let project_path = project_root.join(".orgasmic/project.org");
        let config_path = project_root.join(".orgasmic/config.org");
        write(
            config_path.clone(),
            "#+title: orgasmic config\n#+orgasmic_version: 1\n\n\
* CONFIG orgasmic\n:PROPERTIES:\n:ID:                  orgasmic\n:DEFAULT_BRANCH:      main\n:END:\n",
        );

        let running = crate::Daemon::run(home.clone(), test_options())
            .await
            .expect("boot daemon");
        let token = read_token(&home);
        let client = reqwest::Client::new();
        let base = format!("http://{}", running.addr);

        let doc: Value = client
            .get(format!("{base}/api/org/node"))
            .bearer_auth(&token)
            .query(&[
                ("project", "orgasmic"),
                ("id", "orgasmic"),
                ("kind", "config"),
            ])
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(doc["kind"], "config");
        assert_eq!(doc["source"]["file"], ".orgasmic/config.org");
        let base_version = doc["source"]["base_version"].as_str().unwrap().to_string();

        let resp = client
            .post(format!("{base}/api/org/node/orgasmic/edit"))
            .bearer_auth(&token)
            .json(&serde_json::json!({
                "project": "orgasmic",
                "kind": "config",
                "base_version": base_version,
                "ops": [
                    { "op": "set_property", "key": "TEST_CMD", "value": "cargo test" }
                ]
            }))
            .send()
            .await
            .unwrap();
        let status = resp.status();
        let updated: Value = resp.json().await.unwrap();
        assert!(status.is_success(), "edit config node: {status} {updated}");

        let config_after = std::fs::read_to_string(&config_path).unwrap();
        assert!(config_after.contains(":TEST_CMD:"));
        assert!(config_after.contains("cargo test"));
        let project_after = std::fs::read_to_string(&project_path).unwrap();
        assert!(
            !project_after.contains("TEST_CMD"),
            "TEST_CMD leaked into project.org: {project_after}"
        );

        let _ = running.shutdown.send(());
        let _ = running.join.await;
    }

    #[tokio::test]
    async fn org_node_config_reserved_key_rejected_on_project_layer() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let project_root = tmp.path().join("proj");
        seed_project(&home, &project_root, "orgasmic");
        let project_path = project_root.join(".orgasmic/project.org");

        let running = crate::Daemon::run(home.clone(), test_options())
            .await
            .expect("boot daemon");
        let token = read_token(&home);
        let client = reqwest::Client::new();
        let base = format!("http://{}", running.addr);

        let doc: Value = client
            .get(format!("{base}/api/org/node"))
            .bearer_auth(&token)
            .query(&[
                ("project", "orgasmic"),
                ("id", "orgasmic"),
                ("kind", "project"),
            ])
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let base_version = doc["source"]["base_version"].as_str().unwrap().to_string();

        // This is the exact silent-wrong-file-write regression (TASK-JJ9RD):
        // a CONFIG-owned key must not land in project.org just because the id
        // resolved there under an explicit `--kind project`.
        let resp = client
            .post(format!("{base}/api/org/node/orgasmic/edit"))
            .bearer_auth(&token)
            .json(&serde_json::json!({
                "project": "orgasmic",
                "kind": "project",
                "base_version": base_version,
                "ops": [
                    { "op": "set_property", "key": "TEST_CMD", "value": "cargo test" }
                ]
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
        let body: Value = resp.json().await.unwrap();
        let message = body["error"].as_str().unwrap_or_default();
        assert!(
            message.contains("--kind config"),
            "error should name --kind config as the fix: {message}"
        );

        let project_after = std::fs::read_to_string(&project_path).unwrap();
        assert!(
            !project_after.contains("TEST_CMD"),
            "TEST_CMD leaked into project.org: {project_after}"
        );

        let _ = running.shutdown.send(());
        let _ = running.join.await;
    }

    #[tokio::test]
    async fn org_node_task_layer_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let project_root = tmp.path().join("proj");
        seed_project(&home, &project_root, "orgasmic");
        let sprint_path = task_file_path(&project_root, "backlog.org");
        write(
            sprint_path.clone(),
            "#+title: sprint\n#+orgasmic_version: 1\n\n\
* BACKLOG TASK-PRE Pre-boot task :work:\n\
:PROPERTIES:\n:ID:               TASK-PRE\n:PRIORITY:         P2\n:END:\n\n\
** Description\nLast-good detail.\n\n\
** Acceptance Criteria\n- [ ] Opens from the indexed snapshot.\n",
        );

        let running = crate::Daemon::run(home.clone(), test_options())
            .await
            .expect("boot daemon");
        let token = read_token(&home);
        let mut ws = connect_ws(running.addr, &token, "task").await;
        let client = reqwest::Client::new();
        let base = format!("http://{}", running.addr);

        let doc: Value = client
            .get(format!("{base}/api/org/node"))
            .bearer_auth(&token)
            .query(&[
                ("project", "orgasmic"),
                ("id", "TASK-PRE"),
                ("kind", "task"),
            ])
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(doc["kind"], "task");
        assert_eq!(doc["title"], "Pre-boot task");
        assert_eq!(doc["todo"], "BACKLOG");
        assert_eq!(doc["source"]["file"], task_file_rel("backlog.org"));
        let priority = doc["properties"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["key"] == "PRIORITY")
            .unwrap();
        assert_eq!(priority["value"], "P2");
        let base_version = doc["source"]["base_version"].as_str().unwrap().to_string();

        let resp = client
            .post(format!("{base}/api/org/node/TASK-PRE/edit?json=true"))
            .bearer_auth(&token)
            .json(&serde_json::json!({
                "project": "orgasmic",
                "kind": "task",
                "base_version": base_version,
                "request_id": "task-node-edit-test",
                "ops": [
                    { "op": "set_title", "title": "Pre-boot task updated" },
                    { "op": "set_section_body", "title": "Description", "body": "Edited task detail." },
                    { "op": "set_property", "key": "PRIORITY", "value": "P1" },
                    { "op": "set_tags", "tags": ["work", "edited"] }
                ]
            }))
            .send()
            .await
            .unwrap();
        let status = resp.status();
        let updated: Value = resp.json().await.unwrap();
        assert!(status.is_success(), "edit task node: {status} {updated}");
        assert_eq!(updated["title"], "Pre-boot task updated");
        assert_eq!(updated["tags"], serde_json::json!(["work", "edited"]));

        let event = next_event_kind(&mut ws, "task_updated").await;
        assert_eq!(event["payload"]["project_id"], "orgasmic");
        assert_eq!(event["payload"]["task_id"], "TASK-PRE");

        let after = std::fs::read_to_string(&sprint_path).unwrap();
        assert!(after.contains("* BACKLOG TASK-PRE Pre-boot task updated    :work:edited:\n"));
        assert!(after.lines().any(|line| {
            line.starts_with(":PRIORITY:") && line.split_whitespace().last() == Some("P1")
        }));
        assert!(after.contains("** Description\nEdited task detail.\n"));
        assert!(after.contains("** Acceptance Criteria\n- [ ] Opens from the indexed snapshot.\n"));

        let _ = running.shutdown.send(());
        let _ = running.join.await;
    }

    #[tokio::test]
    async fn task_create_lands_in_backlog_and_mints_id() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let project_root = tmp.path().join("proj");
        seed_project(&home, &project_root, "orgasmic");
        let backlog_path = task_file_path(&project_root, "backlog.org");
        write(
            backlog_path.clone(),
            "#+title: orgasmic backlog\n#+orgasmic_version: 1\n\n",
        );

        let running = crate::Daemon::run(home.clone(), test_options())
            .await
            .expect("boot daemon");
        let token = read_token(&home);
        let client = reqwest::Client::new();
        let base = format!("http://{}", running.addr);

        let resp = client
            .post(format!("{base}/api/projects/orgasmic/tasks"))
            .bearer_auth(&token)
            .json(&serde_json::json!({
                "title": "Filed through daemon",
                "tags": ["daemon", "test"],
                "properties": { "PRIORITY": "P3" },
                "body": "** Description\nCreated by test.\n",
                "request_id": "task-create-test",
            }))
            .send()
            .await
            .unwrap();
        let status = resp.status();
        let body: Value = resp.json().await.unwrap();
        assert!(status.is_success(), "create task: {status} {body}");
        let task_id = body["id"].as_str().expect("minted task id");
        assert!(task_id.starts_with("TASK-"));
        assert!(!body["tx_id"].as_str().unwrap().is_empty());

        let on_disk = std::fs::read_to_string(&backlog_path).unwrap();
        assert!(on_disk.contains(task_id));
        assert!(on_disk.contains("Filed through daemon"));
        assert!(on_disk.contains(":PRIORITY: P3"));
        assert!(on_disk.contains("** Description\nCreated by test.\n"));

        let legacy = client
            .post(format!("{base}/api/projects/orgasmic/tasks"))
            .bearer_auth(&token)
            .json(&serde_json::json!({
                "id": "TASK-001",
                "title": "Legacy id rejected",
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(legacy.status(), reqwest::StatusCode::BAD_REQUEST);

        let phantom = client
            .post(format!("{base}/api/projects/orgasmic/tasks"))
            .bearer_auth(&token)
            .json(&serde_json::json!({
                "title": "Phantom heading rejected",
                "body": "** Description\nFine.\n* DONE TASK-FAKE injected sibling\n",
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(phantom.status(), reqwest::StatusCode::BAD_REQUEST);

        let blocked_star = client
            .post(format!("{base}/api/projects/orgasmic/tasks"))
            .bearer_auth(&token)
            .json(&serde_json::json!({
                "title": "Star inside block allowed",
                "body": "** Description\n#+begin_example\n* literal star line\n#+end_example\n",
            }))
            .send()
            .await
            .unwrap();
        assert!(blocked_star.status().is_success());

        let _ = running.shutdown.send(());
        let _ = running.join.await;
    }

    #[tokio::test]
    async fn task_property_update_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let project_root = tmp.path().join("proj");
        seed_project(&home, &project_root, "orgasmic");

        let running = crate::Daemon::run(home.clone(), test_options())
            .await
            .expect("boot daemon");
        let token = read_token(&home);
        let client = reqwest::Client::new();
        let base = format!("http://{}", running.addr);

        let resp = client
            .post(format!(
                "{base}/api/projects/orgasmic/tasks/TASK-PRE?json=true"
            ))
            .bearer_auth(&token)
            .json(&serde_json::json!({
                "priority": "P1",
                "properties": { "WORKER": "implementer-cursor" },
                "request_id": "task-property-update-test",
            }))
            .send()
            .await
            .unwrap();
        let status = resp.status();
        let updated: Value = resp.json().await.unwrap();
        assert!(status.is_success(), "property update: {status} {updated}");
        let priority = updated["priority"].as_str().unwrap_or_default();
        assert_eq!(priority, "P1");

        let sprint_path = task_file_path(&project_root, "backlog.org");
        let on_disk = std::fs::read_to_string(&sprint_path).unwrap();
        assert!(on_disk.lines().any(|line| {
            line.starts_with(":PRIORITY:") && line.split_whitespace().last() == Some("P1")
        }));
        assert!(on_disk.contains(":WORKER:           implementer-cursor"));

        let _ = running.shutdown.send(());
        let _ = running.join.await;
    }

    #[tokio::test]
    async fn task_state_flip_regression() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let project_root = tmp.path().join("proj");
        seed_project(&home, &project_root, "orgasmic");

        let running = crate::Daemon::run(home.clone(), test_options())
            .await
            .expect("boot daemon");
        let token = read_token(&home);
        let client = reqwest::Client::new();
        let base = format!("http://{}", running.addr);

        let resp = client
            .post(format!(
                "{base}/api/projects/orgasmic/tasks/TASK-PRE?json=true"
            ))
            .bearer_auth(&token)
            .json(&serde_json::json!({
                "state": "in_progress",
                "request_id": "task-state-flip-regression",
            }))
            .send()
            .await
            .unwrap();
        let status = resp.status();
        let updated: Value = resp.json().await.unwrap();
        assert!(status.is_success(), "state flip: {status} {updated}");
        assert_eq!(updated["lifecycle_stage"], "in_progress");

        let backlog_path = task_file_path(&project_root, "backlog.org");
        let backlog_on_disk = std::fs::read_to_string(&backlog_path).unwrap();
        assert!(
            !backlog_on_disk.contains("TASK-PRE"),
            "cross-file move must remove the heading from the source file"
        );
        let in_progress_path = task_file_path(&project_root, "in_progress.org");
        let in_progress_on_disk = std::fs::read_to_string(&in_progress_path).unwrap();
        assert!(in_progress_on_disk.contains("* IN_PROGRESS TASK-PRE"));

        let _ = running.shutdown.send(());
        let _ = running.join.await;
    }

    #[tokio::test]
    async fn node_body_edit_rejects_column0_star_heading() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let project_root = tmp.path().join("proj");
        seed_project(&home, &project_root, "orgasmic");
        write(
            project_root.join(".orgasmic/decisions.org"),
            "#+title: decisions\n#+orgasmic_version: 1\n\n* dec_guard Guard    :topic:\n:PROPERTIES:\n:ID: dec_guard\n:END:\n\n** Context\nSafe.\n",
        );

        let running = crate::Daemon::run(home.clone(), test_options())
            .await
            .expect("boot daemon");
        let token = read_token(&home);
        let client = reqwest::Client::new();
        let base = format!("http://{}", running.addr);

        let doc: Value = client
            .get(format!("{base}/api/org/node"))
            .bearer_auth(&token)
            .query(&[("project", "orgasmic"), ("id", "dec_guard")])
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let base_version = doc["source"]["base_version"].as_str().unwrap();

        let resp = client
            .post(format!("{base}/api/org/node/dec_guard/edit"))
            .bearer_auth(&token)
            .json(&serde_json::json!({
                "project": "orgasmic",
                "base_version": base_version,
                "ops": [{ "op": "set_section_body", "title": "Context", "body": "* Phantom\n" }],
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);

        let _ = running.shutdown.send(());
        let _ = running.join.await;
    }

    #[tokio::test]
    async fn stage_requested_event_emitted_for_grill() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let project_root = tmp.path().join("proj");
        seed_project(&home, &project_root, "orgasmic");
        seed_minimal_graph(&project_root);

        let running = crate::Daemon::run(home.clone(), test_options())
            .await
            .expect("boot daemon");
        let token = read_token(&home);
        let mut ws = connect_ws(running.addr, &token, "run").await;
        let client = reqwest::Client::new();

        let resp = client
            .post(format!("http://{}/api/grill", running.addr))
            .bearer_auth(&token)
            .json(&serde_json::json!({
                "project": "orgasmic",
                "task_id": "TASK-036-GRILL",
                "request_id": "stage-requested-event-test",
                "reason": "test stage event"
            }))
            .send()
            .await
            .unwrap();
        let status = resp.status();
        let body: Value = resp.json().await.unwrap();
        assert!(status.is_success(), "grill stage: {status} {body}");

        let event = next_event_kind(&mut ws, "stage_requested").await;
        let payload = &event["payload"];
        assert_eq!(payload["stage"], "grill");
        assert_eq!(payload["project_id"], "orgasmic");
        assert_eq!(payload["task_id"], "TASK-036-GRILL");
        assert!(!payload["run_id"].as_str().unwrap().is_empty());
        assert!(!payload["tx_id"].as_str().unwrap().is_empty());

        let _ = running.shutdown.send(());
        let _ = running.join.await;
    }

    #[tokio::test]
    async fn stage_completion_event_emitted_when_run_completes() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let project_root = tmp.path().join("proj");
        seed_project(&home, &project_root, "orgasmic");
        let state = direct_stage_test_state(home.clone()).await;
        let mut rx = state.events.subscribe();
        let session_path = home.sessions().join("stage-completed.jsonl");
        let identity = RuntimeIdentity {
            run_id: "run-stage-completed".into(),
            runtime_id: "rt-stage-completed".into(),
            boot_id: "boot-stage-completed".into(),
        };
        write_stage_session(
            &session_path,
            identity,
            DriverEvent::RunComplete {
                summary: Some("done".into()),
            },
        );

        spawn_stage_completion_watcher(
            state.clone(),
            StageCompletion {
                stage: "grill".to_string(),
                project_id: "orgasmic".to_string(),
                task_id: "TASK-036-COMPLETE".to_string(),
                target: task_file_rel("backlog.org"),
                run_id: "run-stage-completed".to_string(),
                session_path,
            },
        );

        let event = next_bus_event_kind(&mut rx, Topic::Run, "stage_completed").await;
        let value = serde_json::to_value(&event).unwrap();
        assert_eq!(value["payload"]["stage"], "grill");
        assert_eq!(value["payload"]["project_id"], "orgasmic");
        assert_eq!(value["payload"]["task_id"], "TASK-036-COMPLETE");
        assert_eq!(value["payload"]["run_id"], "run-stage-completed");
        assert!(!value["payload"]["tx_id"].as_str().unwrap().is_empty());

        state.writer.shutdown().await;
    }

    #[tokio::test]
    async fn stage_failed_event_emitted_when_run_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let project_root = tmp.path().join("proj");
        seed_project(&home, &project_root, "orgasmic");
        let state = direct_stage_test_state(home.clone()).await;
        let mut rx = state.events.subscribe();
        let session_path = home.sessions().join("stage-failed.jsonl");
        let identity = RuntimeIdentity {
            run_id: "run-stage-failed".into(),
            runtime_id: "rt-stage-failed".into(),
            boot_id: "boot-stage-failed".into(),
        };
        write_stage_session(
            &session_path,
            identity,
            DriverEvent::RunFail {
                error_code: "unit".into(),
                error_markdown: "failed".into(),
            },
        );

        spawn_stage_completion_watcher(
            state.clone(),
            StageCompletion {
                stage: "grill".to_string(),
                project_id: "orgasmic".to_string(),
                task_id: "TASK-036-FAIL".to_string(),
                target: task_file_rel("backlog.org"),
                run_id: "run-stage-failed".to_string(),
                session_path,
            },
        );

        let event = next_bus_event_kind(&mut rx, Topic::Run, "stage_failed").await;
        let value = serde_json::to_value(&event).unwrap();
        assert_eq!(value["payload"]["stage"], "grill");
        assert_eq!(value["payload"]["project_id"], "orgasmic");
        assert_eq!(value["payload"]["task_id"], "TASK-036-FAIL");
        assert_eq!(value["payload"]["run_id"], "run-stage-failed");
        assert!(!value["payload"]["tx_id"].as_str().unwrap().is_empty());

        state.writer.shutdown().await;
    }

    #[tokio::test]
    async fn run_failed_tx_emitted_for_driver_error_early_exit() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let project_root = tmp.path().join("proj");
        seed_project(&home, &project_root, "orgasmic");
        let state = direct_stage_test_state(home.clone()).await;
        let mut rx = state.events.subscribe();
        let session_path = home.sessions().join("driver-error-early-exit.jsonl");
        let identity = RuntimeIdentity {
            run_id: "run-driver-error".into(),
            runtime_id: "rt-driver-error".into(),
            boot_id: "boot-driver-error".into(),
        };
        let mut writer = orgasmic_core::SessionWriter::open(&session_path, identity).unwrap();
        writer
            .append(
                SessionEventKind::Lifecycle,
                serde_json::to_value(Lifecycle::Acquire {
                    task_id: "TASK-076".into(),
                    kind: "implementer".into(),
                    worker_id: "implementer-codex-stdio".into(),
                })
                .unwrap(),
            )
            .unwrap();
        writer
            .append(
                SessionEventKind::DriverEvent,
                serde_json::to_value(DriverEvent::Ready {
                    protocol_version: "codex-appserver/1".into(),
                    capabilities: json!({}),
                })
                .unwrap(),
            )
            .unwrap();
        writer
            .append(
                SessionEventKind::DriverEvent,
                serde_json::to_value(DriverEvent::DriverError {
                    fatal: true,
                    message: "cursor-agent killed by SIGKILL".into(),
                })
                .unwrap(),
            )
            .unwrap();
        writer
            .append(
                SessionEventKind::Lifecycle,
                serde_json::to_value(Lifecycle::Release {
                    reason: "early-exit subprocess with no work envelopes".into(),
                    outcome: ReleaseOutcome::Interrupted,
                    finalized_by_worker: false,
                })
                .unwrap(),
            )
            .unwrap();

        spawn_stage_completion_watcher(
            state.clone(),
            StageCompletion {
                stage: "run".to_string(),
                project_id: "orgasmic".to_string(),
                task_id: "TASK-076".to_string(),
                target: task_file_rel("backlog.org"),
                run_id: "run-driver-error".to_string(),
                session_path,
            },
        );

        let event = next_bus_event_kind(&mut rx, Topic::Run, "stage_failed").await;
        let value = serde_json::to_value(&event).unwrap();
        assert_eq!(value["payload"]["stage"], "run");
        assert_eq!(value["payload"]["project_id"], "orgasmic");
        assert_eq!(value["payload"]["task_id"], "TASK-076");
        assert_eq!(value["payload"]["run_id"], "run-driver-error");

        let tx_body = std::fs::read_to_string(crate::default_home_tx_path(&home)).unwrap();
        assert!(tx_body.contains("run.failed"), "{tx_body}");
        assert!(
            tx_body.contains("driver error: cursor-agent killed by SIGKILL"),
            "{tx_body}"
        );

        state.writer.shutdown().await;
    }

    #[tokio::test]
    async fn recovery_marks_current_boot_acp_without_attach_proof_interrupted() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let identity = RuntimeIdentity {
            run_id: "run-acp-recovery".into(),
            runtime_id: "rt-acp-recovery".into(),
            boot_id: "boot-test".into(),
        };
        let project_root = tmp.path().join("proj");
        write_nonterminal_session(&project_root, identity, "acp/1", "implementer-claude-acp");

        let recovered =
            classify_session_files(&home, "boot-test", &[], std::slice::from_ref(&project_root))
                .await;

        let run = recovered
            .iter()
            .find(|run| run.run_id == "run-acp-recovery")
            .unwrap();
        assert_eq!(run.classification, "interrupted");
        assert!(run.reason.contains("could not prove"));
    }

    #[tokio::test]
    async fn recovery_reattaches_tmux_session_when_handle_exists() {
        let _live_guard = live_session_guard();
        if !tmux_on_path() {
            eprintln!(
                "skipping recovery_reattaches_tmux_session_when_handle_exists: tmux not on PATH"
            );
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let suffix = uuid::Uuid::new_v4().simple().to_string();
        let identity = RuntimeIdentity {
            run_id: format!("run-tmux-{suffix}"),
            runtime_id: format!("rt-{suffix}"),
            boot_id: "boot-test".into(),
        };
        let session_name = orgasmic_drivers::modes::tmux::tmux_session_name(&identity);
        let status = Command::new("tmux")
            .args(["new-session", "-d", "-s", &session_name, "sleep", "60"])
            .status()
            .unwrap();
        assert!(status.success(), "tmux test session should start");
        let _guard = TmuxSessionGuard(session_name);
        let project_root = tmp.path().join("proj");
        write_nonterminal_session(
            &project_root,
            identity.clone(),
            "tmux-tui/1",
            "manager-tmux-tui",
        );

        let recovered =
            classify_session_files(&home, "boot-test", &[], std::slice::from_ref(&project_root))
                .await;

        let run = recovered
            .iter()
            .find(|run| run.run_id == identity.run_id)
            .unwrap();
        assert_eq!(run.classification, "reattached");
        assert!(run.reason.contains("proved live runtime handle"));
        // Regression (dec_052): a live older-boot tmux session prefers
        // reattach_tmux as the first recovery action.
        assert_eq!(
            run.recovery_actions.first().map(|a| a.kind.as_str()),
            Some("reattach_tmux"),
            "live older-boot tmux must prefer reattach_tmux: {:?}",
            run.recovery_actions
        );
    }

    #[tokio::test]
    async fn legacy_run_without_native_metadata_only_offers_start_recovery_run() {
        // Regression (dec_052): no best-effort ~/.claude/projects discovery for
        // legacy runs without native metadata; with no live tmux and no native
        // metadata, the sole action is start_recovery_run.
        let envelopes: Vec<SessionEnvelope> = Vec::new();
        let home = Home::at(std::env::temp_dir().join("orgasmic-recovery-test-home"));
        let actions = resolve_recovery_actions(&home, &envelopes, "interrupted").await;
        assert_eq!(actions.len(), 1, "{actions:?}");
        assert_eq!(actions[0].kind, "start_recovery_run");
    }

    #[tokio::test]
    async fn native_claude_metadata_exposes_resume_native_fork() {
        let envelopes = vec![
            SessionEnvelope {
                seq: 0,
                time: Utc::now(),
                run_id: "run-x".into(),
                runtime_id: "rt-x".into(),
                boot_id: "boot-old".into(),
                kind: SessionEventKind::Lifecycle,
                event: serde_json::to_value(Lifecycle::Acquire {
                    task_id: "TASK-1".into(),
                    kind: "implementer".into(),
                    worker_id: "manager".into(),
                })
                .unwrap(),
            },
            SessionEnvelope {
                seq: 1,
                time: Utc::now(),
                run_id: "run-x".into(),
                runtime_id: "rt-x".into(),
                boot_id: "boot-old".into(),
                kind: SessionEventKind::Lifecycle,
                event: serde_json::to_value(Lifecycle::NativeRuntime {
                    provider: "claude".into(),
                    session_id: Some("rt-x".into()),
                    session_path: None,
                    launch_argv: vec!["claude".into(), "--session-id".into(), "rt-x".into()],
                    resume_argv: vec![
                        "claude".into(),
                        "--resume".into(),
                        "rt-x".into(),
                        "--fork-session".into(),
                    ],
                })
                .unwrap(),
            },
        ];
        let home = Home::at(std::env::temp_dir().join("orgasmic-recovery-test-home2"));
        // classification "interrupted" (tmux gone): no reattach, but native
        // resume/fork is available before start_recovery_run.
        let actions = resolve_recovery_actions(&home, &envelopes, "interrupted").await;
        let kinds: Vec<&str> = actions.iter().map(|a| a.kind.as_str()).collect();
        assert_eq!(kinds, ["resume_native_fork", "start_recovery_run"]);
        // Manager worker_id routes recovery to the manager surface.
        assert_eq!(actions[0].target, "manager");
    }

    #[test]
    fn start_recovery_run_never_uses_placeholder_shell() {
        // Regression (dec_052): the old continuation path launched a placeholder
        // shell. That command, and the removed continue endpoint, must not
        // exist anywhere in the recovery surface. Needles are assembled from
        // fragments so this test does not match its own source.
        let source = include_str!("api.rs");
        let placeholder = ["echo orgasmic run", "continuation"].join(" ");
        assert!(
            !source.contains(&placeholder),
            "recovery must never launch the placeholder continuation shell"
        );
        let removed_route = ["/runs/:id/", "continue"].concat();
        assert!(
            !source.contains(&removed_route),
            "the old continue endpoint must be removed, not aliased"
        );
    }

    #[test]
    fn recovery_prompt_includes_required_context() {
        let prior_path = std::path::Path::new("/abs/sessions/run-prior.jsonl");
        let envelopes = vec![SessionEnvelope {
            seq: 0,
            time: Utc::now(),
            run_id: "run-prior".into(),
            runtime_id: "rt".into(),
            boot_id: "boot".into(),
            kind: SessionEventKind::DriverEvent,
            event: json!({"type": "tool_call", "name": "edit", "args": {}, "seq": 0, "call_id": "c1"}),
        }];
        let prompt = build_recovery_prompt(
            "run-prior",
            prior_path,
            &envelopes,
            "2 files changed, 5 insertions(+)",
        );
        assert!(prompt.contains("run-prior"), "{prompt}");
        assert!(prompt.contains("/abs/sessions/run-prior.jsonl"), "{prompt}");
        assert!(prompt.contains("2 files changed"), "{prompt}");
        assert!(prompt.contains("Inspect before acting"), "{prompt}");
        assert!(prompt.contains("NOT been sent"), "{prompt}");
    }

    #[tokio::test]
    async fn graph_markers_route_returns_seeded_marker_files() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let project_root = tmp.path().join("proj");
        seed_project(&home, &project_root, "orgasmic");
        write(
            project_root.join("crates/orgasmic-core/src/org.rs"),
            "// orgasmic:arch_003,dec_004\n",
        );

        let running = crate::Daemon::run(home.clone(), test_options())
            .await
            .expect("boot daemon");
        let token = read_token(&home);
        let client = reqwest::Client::new();
        let resp = client
            .get(format!(
                "http://{}/api/graph/markers/arch_003",
                running.addr
            ))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_success(), "graph markers route");
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["node_id"], "arch_003");
        assert_eq!(
            body["files"],
            serde_json::json!(["crates/orgasmic-core/src/org.rs"])
        );

        let resp = client
            .get(format!(
                "http://{}/api/graph/markers/dec_missing",
                running.addr
            ))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_success(), "unknown graph marker route");
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["node_id"], "dec_missing");
        assert_eq!(body["files"], serde_json::json!([]));

        let _ = running.shutdown.send(());
        let _ = running.join.await;
    }

    #[tokio::test]
    async fn manager_drivers_catalog_includes_rmux_with_separate_mode_binary() {
        let Json(resp) = get_manager_drivers().await;
        // rmux is surfaced in the catalog with the same harnesses as tmux.
        let rmux = resp
            .drivers
            .iter()
            .find(|d| d.mode == "rmux" && d.harness == "codex")
            .expect("rmux/codex driver present in catalog");
        assert_eq!(rmux.mode_label, "rmux");
        // Full harness parity with tmux (claude, codex, cursor-agent, hermes).
        let rmux_harnesses: std::collections::BTreeSet<&str> = resp
            .drivers
            .iter()
            .filter(|d| d.mode == "rmux")
            .map(|d| d.harness.as_str())
            .collect();
        assert!(
            ["claude", "codex", "cursor-agent", "hermes"]
                .iter()
                .all(|h| rmux_harnesses.contains(h)),
            "rmux must offer the same harnesses as tmux, got {rmux_harnesses:?}"
        );
        // rmux carries a SEPARATE mode-level binary requirement, checked
        // independently of the harness binary (acceptance criterion).
        assert!(
            rmux.mode_binary.is_some(),
            "rmux must report a mode binary distinct from the harness binary"
        );
        assert!(
            rmux.mode_installed.is_some(),
            "rmux mode binary install status must be reported"
        );
    }

    #[tokio::test]
    async fn manager_drivers_catalog_omits_mode_binary_for_plain_modes() {
        let Json(resp) = get_manager_drivers().await;
        // tmux (and other modes without an extra binary) report no mode_binary.
        let tmux = resp
            .drivers
            .iter()
            .find(|d| d.mode == "tmux")
            .expect("tmux driver present");
        assert!(
            tmux.mode_binary.is_none(),
            "plain modes must not advertise a separate mode binary"
        );
        assert!(tmux.mode_installed.is_none());
    }

    #[test]
    fn mode_binary_status_only_tracks_rmux() {
        assert!(mode_binary_status("rmux").is_some());
        assert!(mode_binary_status("tmux").is_none());
        assert!(mode_binary_status("acp-stdio").is_none());
    }

    // ── artifact round-trip (TASK-ZEFEY acceptance) ───────────────────────────

    fn artifact_query(project: &str) -> ArtifactQuery {
        ArtifactQuery {
            project: Some(project.into()),
            version: None,
            include_consumed: None,
        }
    }

    // Traversal/malformed art_id must 400 on every artifact route and write
    // nothing — the accepted-shape guard is a choke point ahead of
    // artifact_dir(), not a per-handler afterthought (TASK-3K9ZG).
    #[tokio::test]
    async fn artifact_routes_reject_traversal_art_id_and_write_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let project_root = tmp.path().join("myproject");
        seed_project(&home, &project_root, "test-proj");
        let state = direct_stage_test_state(home.clone()).await;

        let artifacts_root = crate::artifacts::artifacts_dir(&project_root);
        assert!(
            !artifacts_root.exists(),
            "artifacts dir must not exist before any lawful submit"
        );
        // Escape target this traversal attempt would land on if the guard
        // failed to fire: project_root/.orgasmic/artifacts/../../evil
        // resolves (one ".." per intervening directory) to project_root/evil.
        let escape_target = project_root.join("evil");

        let evil = "../../evil";

        let submit_err = post_artifact_submit(
            State(state.clone()),
            Path(evil.to_string()),
            Query(artifact_query("test-proj")),
            Json(ArtifactSubmitRequest {
                content: "<RichText>x</RichText>\n".into(),
                title: Some("Evil".into()),
                subject_nodes: None,
                prompt: None,
            }),
        )
        .await
        .expect_err("traversal submit must be rejected");
        assert_eq!(submit_err.status, StatusCode::BAD_REQUEST);
        assert!(
            submit_err.message.contains("ART-"),
            "{}",
            submit_err.message
        );
        assert!(
            submit_err
                .message
                .contains("orgasmic id mint --class artifact"),
            "{}",
            submit_err.message
        );

        let get_err = get_artifact(
            State(state.clone()),
            Extension(Identity::Admin),
            Path(evil.to_string()),
            Query(artifact_query("test-proj")),
        )
        .await
        .expect_err("traversal get must be rejected");
        assert_eq!(get_err.status, StatusCode::BAD_REQUEST);

        let comment_err = post_artifact_add_comment(
            State(state.clone()),
            Extension(Identity::Admin),
            Path(evil.to_string()),
            Query(artifact_query("test-proj")),
            Json(ArtifactCommentRequest {
                message: "should not land".into(),
                anchor: None,
                resolution_target: None,
                author: None,
            }),
        )
        .await
        .expect_err("traversal comment must be rejected");
        assert_eq!(comment_err.status, StatusCode::BAD_REQUEST);

        let resolve_err = post_artifact_comment_resolve(
            State(state.clone()),
            Extension(Identity::Admin),
            Path((evil.to_string(), "CID-fake0001".to_string())),
            Query(artifact_query("test-proj")),
            Json(ArtifactCommentResolveRequest { resolved: true }),
        )
        .await
        .expect_err("traversal resolve must be rejected");
        assert_eq!(resolve_err.status, StatusCode::BAD_REQUEST);

        let regenerate_err = post_artifact_regenerate(
            State(state.clone()),
            Extension(Identity::Admin),
            Path(evil.to_string()),
            Query(artifact_query("test-proj")),
            Json(ArtifactRegenerateRequest { extra_prompt: None }),
        )
        .await
        .expect_err("traversal regenerate must be rejected");
        assert_eq!(regenerate_err.status, StatusCode::BAD_REQUEST);

        // Write-nothing: the escape attempt must not have touched disk, and
        // no tx entry must have been appended for it.
        assert!(
            !artifacts_root.exists(),
            "traversal attempt must leave the artifacts dir untouched"
        );
        assert!(
            !escape_target.exists(),
            "traversal attempt must not have escaped the artifacts dir"
        );
        let tx_path = project_root
            .join(".orgasmic")
            .join("tx")
            .join(format!("{}.org", Utc::now().format("%Y-%m")));
        let tx_contents = std::fs::read_to_string(&tx_path).unwrap_or_default();
        assert!(
            !tx_contents.contains("evil"),
            "rejected traversal id must not reach any tx entry: {tx_contents}"
        );
    }

    #[tokio::test]
    async fn artifact_create_submit_feedback_consume_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let project_root = tmp.path().join("myproject");
        seed_project(&home, &project_root, "test-proj");
        let state = direct_stage_test_state(home.clone()).await;

        let art_id = "ART-TESTA";

        // --- submit (create) ---
        let submit_resp = post_artifact_submit(
            State(state.clone()),
            Path(art_id.to_string()),
            Query(artifact_query("test-proj")),
            Json(ArtifactSubmitRequest {
                content: "<RichText>Hello world</RichText>\n".into(),
                title: Some("My Test Artifact".into()),
                subject_nodes: Some(vec!["arch_ARSPJ".into()]),
                prompt: Some("Generate a test artifact".into()),
            }),
        )
        .await
        .expect("submit should succeed");
        assert_eq!(submit_resp.0["artifact_id"], art_id);
        assert_eq!(submit_resp.0["version"], 1);

        // --- list ---
        let list_resp = get_artifacts(
            State(state.clone()),
            Extension(Identity::Admin),
            Query(artifact_query("test-proj")),
        )
        .await
        .expect("list should succeed");
        assert_eq!(list_resp.0.len(), 1);
        assert_eq!(list_resp.0[0].id, art_id);
        assert_eq!(list_resp.0[0].title, "My Test Artifact");
        assert_eq!(list_resp.0[0].version, 1);
        assert_eq!(list_resp.0[0].state, "submitted");
        assert_eq!(list_resp.0[0].open_comment_count, 0);

        // --- detail ---
        let detail_resp = get_artifact(
            State(state.clone()),
            Extension(Identity::Admin),
            Path(art_id.to_string()),
            Query(artifact_query("test-proj")),
        )
        .await
        .expect("detail should succeed");
        assert_eq!(detail_resp.0.summary.id, art_id);
        assert!(detail_resp.0.content.contains("RichText"));
        assert_eq!(detail_resp.0.comments.len(), 0);

        // --- feedback (add comment) ---
        let fb_resp = post_artifact_add_comment(
            State(state.clone()),
            Extension(Identity::Admin),
            Path(art_id.to_string()),
            Query(artifact_query("test-proj")),
            Json(ArtifactCommentRequest {
                message: "This looks good!".into(),
                anchor: None,
                resolution_target: None,
                author: Some("reviewer@test.com".into()),
            }),
        )
        .await
        .expect("feedback should succeed");
        let cid = fb_resp.0["cid"].as_str().unwrap().to_string();
        assert!(cid.starts_with("CID-"));

        // Index should reflect 1 open comment
        let list2 = get_artifacts(
            State(state.clone()),
            Extension(Identity::Admin),
            Query(artifact_query("test-proj")),
        )
        .await
        .expect("list 2");
        assert_eq!(list2.0[0].open_comment_count, 1);

        // --- consume (agent-facing axis: regeneration close-out) ---
        // `consumed` is set in production ONLY by regeneration close-out via
        // `consume_all_open_comments`; the people-facing resolve handler never
        // touches it. This test asserts exclusion-by-default, which keys off
        // `consumed` (not `resolved`), so it must drive the consume axis the
        // production way. The test already operates at the file layer below, so
        // apply `consume_all_open_comments` to reviews.org directly and refresh
        // the index projection.
        let reviews_path = artifact_dir(&project_root, art_id).join("reviews.org");
        let consumed_reviews =
            artifacts::consume_all_open_comments(&std::fs::read_to_string(&reviews_path).unwrap())
                .expect("consume open comments should succeed");
        std::fs::write(&reviews_path, &consumed_reviews).unwrap();
        let _ = state.index.refresh_project("test-proj").await;

        // Default view: consumed comments are excluded (MANAGER RULING,
        // TASK-Y2ZQJ).
        let detail2 = get_artifact(
            State(state.clone()),
            Extension(Identity::Admin),
            Path(art_id.to_string()),
            Query(artifact_query("test-proj")),
        )
        .await
        .expect("detail 2");
        assert!(
            detail2.0.comments.is_empty(),
            "consumed comments must be excluded by default"
        );

        // include_consumed=true surfaces it, resolved + consumed.
        let detail2_all = get_artifact(
            State(state.clone()),
            Extension(Identity::Admin),
            Path(art_id.to_string()),
            Query(ArtifactQuery {
                include_consumed: Some(true),
                ..artifact_query("test-proj")
            }),
        )
        .await
        .expect("detail 2 (include_consumed)");
        let comments = &detail2_all.0.comments;
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].cid, cid);
        assert!(comments[0].resolved);
        assert!(comments[0].consumed);

        // Open comment count should be 0 after consume
        let list3 = get_artifacts(
            State(state.clone()),
            Extension(Identity::Admin),
            Query(artifact_query("test-proj")),
        )
        .await
        .expect("list 3");
        assert_eq!(list3.0[0].open_comment_count, 0);

        // --- reject invalid MDX ---
        let bad_submit = post_artifact_submit(
            State(state.clone()),
            Path("ART-TESTB".to_string()),
            Query(artifact_query("test-proj")),
            Json(ArtifactSubmitRequest {
                content: "<GalacticWidget>bad block</GalacticWidget>".into(),
                title: Some("Bad".into()),
                subject_nodes: None,
                prompt: None,
            }),
        )
        .await;
        assert!(bad_submit.is_err());
        let err = bad_submit.unwrap_err();
        assert_eq!(err.status, StatusCode::UNPROCESSABLE_ENTITY);

        // --- refuse feedback on regenerating artifact ---
        // Force state to regenerating
        let art_dir = artifact_dir(&project_root, art_id);
        let org_content = std::fs::read_to_string(art_dir.join("artifact.org")).unwrap();
        let updated_org = update_artifact_org(&org_content, 1, "regenerating").unwrap();
        std::fs::write(art_dir.join("artifact.org"), &updated_org).unwrap();
        let fb2 = post_artifact_add_comment(
            State(state.clone()),
            Extension(Identity::Admin),
            Path(art_id.to_string()),
            Query(artifact_query("test-proj")),
            Json(ArtifactCommentRequest {
                message: "Should be refused".into(),
                anchor: None,
                resolution_target: None,
                author: None,
            }),
        )
        .await;
        assert!(fb2.is_err());
        assert_eq!(fb2.unwrap_err().status, StatusCode::CONFLICT);

        // --- re-submit (version bump) ---
        // Restore state to submitted first
        let org_content2 = std::fs::read_to_string(art_dir.join("artifact.org")).unwrap();
        let restored = update_artifact_org(&org_content2, 1, "submitted").unwrap();
        std::fs::write(art_dir.join("artifact.org"), &restored).unwrap();

        let resubmit = post_artifact_submit(
            State(state.clone()),
            Path(art_id.to_string()),
            Query(artifact_query("test-proj")),
            Json(ArtifactSubmitRequest {
                content: "<Diagram>updated</Diagram>".into(),
                title: None,
                subject_nodes: None,
                prompt: None,
            }),
        )
        .await
        .expect("re-submit should succeed");
        assert_eq!(resubmit.0["version"], 2);

        // versions/v1.mdx should be the archive
        assert!(art_dir.join("versions/v1.mdx").exists());
    }

    // Archived-version reads (TASK-EDQPG gap 2): GET ?version=N&include_consumed=true
    // for an archived version N must return that version's own comment thread
    // (consumed or not), while the current/live view keeps returning only its
    // own open thread — never leaking an older version's (consumed) comments
    // into the live view, and never leaking the live version's open comment
    // into an archived read.
    #[tokio::test]
    async fn get_artifact_archived_version_returns_that_versions_own_comment_thread() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let project_root = tmp.path().join("myproject");
        seed_project(&home, &project_root, "test-proj");
        let state = direct_stage_test_state(home.clone()).await;

        let art_id = "ART-VEDQP";

        // --- v1: create ---
        let _ = post_artifact_submit(
            State(state.clone()),
            Path(art_id.to_string()),
            Query(artifact_query("test-proj")),
            Json(ArtifactSubmitRequest {
                content: "<RichText>v1 body</RichText>\n".into(),
                title: Some("Version Scoping Test".into()),
                subject_nodes: Some(vec![]),
                prompt: Some("prompt".into()),
            }),
        )
        .await
        .expect("v1 submit should succeed");

        // --- v1 comment, from a viewer ---
        let fb1 = post_artifact_add_comment(
            State(state.clone()),
            Extension(Identity::Admin),
            Path(art_id.to_string()),
            Query(artifact_query("test-proj")),
            Json(ArtifactCommentRequest {
                message: "v1 feedback".into(),
                anchor: None,
                resolution_target: None,
                author: Some("viewer@test.com".into()),
            }),
        )
        .await
        .expect("v1 feedback should succeed");
        let v1_cid = fb1.0["cid"].as_str().unwrap().to_string();

        // --- regenerate close-out consumes the v1 thread. Driven at the file
        // layer (as the existing consume-axis test above does) rather than
        // through `close_out_artifact_regenerate_round`, which is off-limits
        // for this task. ---
        let art_dir = artifact_dir(&project_root, art_id);
        let reviews_path = art_dir.join("reviews.org");
        let consumed =
            artifacts::consume_all_open_comments(&std::fs::read_to_string(&reviews_path).unwrap())
                .expect("consume should succeed");
        std::fs::write(&reviews_path, &consumed).unwrap();

        // --- v2: submit archives v1.mdx and bumps version ---
        let _ = post_artifact_submit(
            State(state.clone()),
            Path(art_id.to_string()),
            Query(artifact_query("test-proj")),
            Json(ArtifactSubmitRequest {
                content: "<RichText>v2 body</RichText>\n".into(),
                title: None,
                subject_nodes: None,
                prompt: None,
            }),
        )
        .await
        .expect("v2 submit should succeed");

        // --- v2 comment: fresh, open thread ---
        let fb2 = post_artifact_add_comment(
            State(state.clone()),
            Extension(Identity::Admin),
            Path(art_id.to_string()),
            Query(artifact_query("test-proj")),
            Json(ArtifactCommentRequest {
                message: "v2 feedback".into(),
                anchor: None,
                resolution_target: None,
                author: Some("editor@test.com".into()),
            }),
        )
        .await
        .expect("v2 feedback should succeed");
        let v2_cid = fb2.0["cid"].as_str().unwrap().to_string();

        // --- archived view: version=1 & include_consumed=true ---
        let archived = get_artifact(
            State(state.clone()),
            Extension(Identity::Admin),
            Path(art_id.to_string()),
            Query(ArtifactQuery {
                version: Some(1),
                include_consumed: Some(true),
                ..artifact_query("test-proj")
            }),
        )
        .await
        .expect("archived detail should succeed");
        assert!(archived.0.content.contains("v1 body"));
        assert_eq!(archived.0.comments.len(), 1);
        assert_eq!(archived.0.comments[0].cid, v1_cid);
        assert!(archived.0.comments[0].consumed);

        // --- current/live view (criterion 5 invariant): only v2's own open
        // comment; v1's consumed comment must not leak in. ---
        let live = get_artifact(
            State(state.clone()),
            Extension(Identity::Admin),
            Path(art_id.to_string()),
            Query(artifact_query("test-proj")),
        )
        .await
        .expect("live detail should succeed");
        assert!(live.0.content.contains("v2 body"));
        assert_eq!(live.0.comments.len(), 1);
        assert_eq!(live.0.comments[0].cid, v2_cid);
    }

    // Same scenario as the handler-level test above, but driven over a real
    // bound TCP listener with `reqwest` (the pattern other wire-level tests in
    // this file use via `Daemon::run` + `test_options()`/`read_token()`).
    // Handler-level calls bypass Axum's `Query<ArtifactQuery>` extractor
    // entirely, so they can't prove `?include_consumed=true` actually
    // deserializes off a real URL the way the UI's `fetchArtifact` sends it;
    // this test exercises that real wire path end to end (TASK-EDQPG live
    // self-check).
    #[tokio::test]
    async fn get_artifact_archived_version_over_real_http_returns_scoped_thread() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let project_root = tmp.path().join("myproject");
        seed_project(&home, &project_root, "test-proj");

        let running = crate::Daemon::run(home.clone(), test_options())
            .await
            .expect("boot daemon");
        let token = read_token(&home);
        let client = reqwest::Client::new();
        let base = format!("http://{}/api", running.addr);
        let art_id = "ART-HTTPV";

        // --- v1: create, over real HTTP ---
        let submit1 = client
            .post(format!(
                "{base}/artifacts/{art_id}/submit?project=test-proj"
            ))
            .bearer_auth(&token)
            .json(&json!({
                "content": "<RichText>v1 body</RichText>\n",
                "title": "HTTP live probe",
                "prompt": "prompt",
            }))
            .send()
            .await
            .unwrap();
        assert!(
            submit1.status().is_success(),
            "submit v1: {:?}",
            submit1.status()
        );

        // --- v1 comment, over real HTTP ---
        let fb1 = client
            .post(format!(
                "{base}/artifacts/{art_id}/comments?project=test-proj"
            ))
            .bearer_auth(&token)
            .json(&json!({ "message": "v1 feedback", "author": "viewer@test.com" }))
            .send()
            .await
            .unwrap();
        assert!(
            fb1.status().is_success(),
            "add v1 comment: {:?}",
            fb1.status()
        );
        let fb1_body: Value = fb1.json().await.unwrap();
        let v1_cid = fb1_body["cid"].as_str().unwrap().to_string();

        // --- regenerate close-out consumes the v1 thread. Driven at the file
        // layer, same convention as the handler-level test above:
        // `close_out_artifact_regenerate_round` itself is off-limits/already
        // verified for this task, and driving a real regenerate here would
        // need a live worker/LLM credential this environment doesn't have. ---
        let art_dir = artifact_dir(&project_root, art_id);
        let reviews_path = art_dir.join("reviews.org");
        let consumed =
            artifacts::consume_all_open_comments(&std::fs::read_to_string(&reviews_path).unwrap())
                .expect("consume should succeed");
        std::fs::write(&reviews_path, &consumed).unwrap();

        // --- v2: submit archives v1.mdx and bumps version, over real HTTP ---
        let submit2 = client
            .post(format!(
                "{base}/artifacts/{art_id}/submit?project=test-proj"
            ))
            .bearer_auth(&token)
            .json(&json!({ "content": "<RichText>v2 body</RichText>\n" }))
            .send()
            .await
            .unwrap();
        assert!(
            submit2.status().is_success(),
            "submit v2: {:?}",
            submit2.status()
        );

        // --- v2 comment: fresh, open thread ---
        let fb2 = client
            .post(format!(
                "{base}/artifacts/{art_id}/comments?project=test-proj"
            ))
            .bearer_auth(&token)
            .json(&json!({ "message": "v2 feedback", "author": "editor@test.com" }))
            .send()
            .await
            .unwrap();
        assert!(
            fb2.status().is_success(),
            "add v2 comment: {:?}",
            fb2.status()
        );
        let fb2_body: Value = fb2.json().await.unwrap();
        let v2_cid = fb2_body["cid"].as_str().unwrap().to_string();

        // --- archived view over real HTTP: the exact query string the UI
        // sends for an archived version (fetchArtifact + includeConsumed). ---
        let archived_resp = client
            .get(format!(
                "{base}/artifacts/{art_id}?project=test-proj&version=1&include_consumed=true"
            ))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap();
        assert!(
            archived_resp.status().is_success(),
            "{:?}",
            archived_resp.status()
        );
        let archived: Value = archived_resp.json().await.unwrap();
        assert!(archived["content"].as_str().unwrap().contains("v1 body"));
        let archived_comments = archived["comments"].as_array().unwrap();
        assert_eq!(archived_comments.len(), 1);
        assert_eq!(archived_comments[0]["cid"], v1_cid);
        assert_eq!(archived_comments[0]["consumed"], true);

        // --- current/live view over real HTTP: no version, no
        // include_consumed — v1's consumed comment must not leak in. ---
        let live_resp = client
            .get(format!("{base}/artifacts/{art_id}?project=test-proj"))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap();
        assert!(live_resp.status().is_success(), "{:?}", live_resp.status());
        let live: Value = live_resp.json().await.unwrap();
        assert!(live["content"].as_str().unwrap().contains("v2 body"));
        let live_comments = live["comments"].as_array().unwrap();
        assert_eq!(live_comments.len(), 1);
        assert_eq!(live_comments[0]["cid"], v2_cid);

        let _ = running.shutdown.send(());
        let _ = running.join.await;
    }

    // People-facing resolve axis (dec_V44E4 / dec_KF2MR). Preserves the
    // people-axis half of the former feedback/consume roundtrip, which the
    // two-axis split separates from consumption: `post_artifact_comment_resolve`
    // toggles `resolved` only. It must NEVER set `consumed` (the agent-facing
    // axis, driven solely by regeneration close-out), and a resolved-but-
    // unconsumed comment must STILL appear in the default (exclude-consumed)
    // view.
    #[tokio::test]
    async fn artifact_comment_resolve_toggles_people_axis_without_consuming() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let project_root = tmp.path().join("myproject");
        seed_project(&home, &project_root, "test-proj");
        let state = direct_stage_test_state(home.clone()).await;

        let art_id = "ART-RSVAX";
        let _ = post_artifact_submit(
            State(state.clone()),
            Path(art_id.to_string()),
            Query(artifact_query("test-proj")),
            Json(ArtifactSubmitRequest {
                content: "<RichText>Hello</RichText>\n".into(),
                title: Some("Resolve axis".into()),
                subject_nodes: None,
                prompt: None,
            }),
        )
        .await
        .expect("submit should succeed");

        let fb = post_artifact_add_comment(
            State(state.clone()),
            Extension(Identity::Admin),
            Path(art_id.to_string()),
            Query(artifact_query("test-proj")),
            Json(ArtifactCommentRequest {
                message: "please tighten this".into(),
                anchor: None,
                resolution_target: None,
                author: Some("reviewer@test.com".into()),
            }),
        )
        .await
        .expect("comment add should succeed");
        let cid = fb.0["cid"].as_str().unwrap().to_string();

        // Resolve (people-facing axis only).
        let resolve_resp = post_artifact_comment_resolve(
            State(state.clone()),
            Extension(Identity::Admin),
            Path((art_id.to_string(), cid.clone())),
            Query(artifact_query("test-proj")),
            Json(ArtifactCommentResolveRequest { resolved: true }),
        )
        .await
        .expect("resolve should succeed");
        assert_eq!(resolve_resp.0["resolved"], true);

        // Resolved but NOT consumed: still visible in the default view.
        let detail = get_artifact(
            State(state.clone()),
            Extension(Identity::Admin),
            Path(art_id.to_string()),
            Query(artifact_query("test-proj")),
        )
        .await
        .expect("detail");
        let comment = detail
            .0
            .comments
            .iter()
            .find(|c| c.cid == cid)
            .expect("resolved comment must remain visible in the default view");
        assert!(comment.resolved, "resolve must set the people-facing axis");
        assert!(
            !comment.consumed,
            "resolve must never set the agent-facing consumed axis (dec_V44E4/dec_KF2MR)"
        );

        // Members can re-open (toggle back to open).
        let _ = post_artifact_comment_resolve(
            State(state.clone()),
            Extension(Identity::Admin),
            Path((art_id.to_string(), cid.clone())),
            Query(artifact_query("test-proj")),
            Json(ArtifactCommentResolveRequest { resolved: false }),
        )
        .await
        .expect("re-open should succeed");

        let detail2 = get_artifact(
            State(state.clone()),
            Extension(Identity::Admin),
            Path(art_id.to_string()),
            Query(artifact_query("test-proj")),
        )
        .await
        .expect("detail after reopen");
        let comment2 = detail2
            .0
            .comments
            .iter()
            .find(|c| c.cid == cid)
            .expect("re-opened comment still visible");
        assert!(!comment2.resolved, "re-open clears the resolved axis");
        assert!(
            !comment2.consumed,
            "re-open never touches the consumed axis"
        );
    }

    // Regression guard for TASK-Y2ZQJ finding M1: concurrent comment-adds on
    // the SAME artifact must all survive. Before the per-artifact write lock,
    // each handler read reviews.org outside any lock and then submitted a blind
    // full-content FileRewrite, so concurrent adds clobbered each other and only
    // the last writer's comment survived. multi_thread + N racers makes the lost
    // update reproducible if the serialization is ever removed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn artifact_concurrent_comment_adds_do_not_lose_writes() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let project_root = tmp.path().join("myproject");
        seed_project(&home, &project_root, "test-proj");
        let state = direct_stage_test_state(home.clone()).await;

        let art_id = "ART-RACE1";
        let _ = post_artifact_submit(
            State(state.clone()),
            Path(art_id.to_string()),
            Query(artifact_query("test-proj")),
            Json(ArtifactSubmitRequest {
                content: "<RichText>base</RichText>\n".into(),
                title: Some("Race".into()),
                subject_nodes: None,
                prompt: None,
            }),
        )
        .await
        .expect("submit should succeed");

        const N: usize = 12;
        let mut handles = Vec::new();
        for i in 0..N {
            let st = state.clone();
            handles.push(tokio::spawn(async move {
                post_artifact_add_comment(
                    State(st),
                    Extension(Identity::Admin),
                    Path(art_id.to_string()),
                    Query(artifact_query("test-proj")),
                    Json(ArtifactCommentRequest {
                        message: format!("comment number {i}"),
                        anchor: None,
                        resolution_target: None,
                        author: Some(format!("member-{i}")),
                    }),
                )
                .await
                .map(|j| j.0["cid"].as_str().unwrap().to_string())
            }));
        }
        let mut cids = Vec::new();
        for h in handles {
            cids.push(
                h.await
                    .unwrap()
                    .expect("concurrent feedback add should succeed"),
            );
        }

        // Distinct CIDs, and every add durably present in the store.
        let unique: std::collections::HashSet<_> = cids.iter().collect();
        assert_eq!(unique.len(), N, "each add should mint a distinct CID");

        let list = get_artifacts(
            State(state.clone()),
            Extension(Identity::Admin),
            Query(artifact_query("test-proj")),
        )
        .await
        .expect("list");
        assert_eq!(
            list.0[0].open_comment_count, N,
            "all {N} concurrent comment-adds must survive (no lost updates)"
        );

        let detail = get_artifact(
            State(state.clone()),
            Extension(Identity::Admin),
            Path(art_id.to_string()),
            Query(artifact_query("test-proj")),
        )
        .await
        .expect("detail");
        assert_eq!(detail.0.comments.len(), N);
        let got: std::collections::HashSet<_> =
            detail.0.comments.iter().map(|c| c.cid.clone()).collect();
        for cid in &cids {
            assert!(got.contains(cid), "comment {cid} was lost");
        }
    }

    // Regression guard for the TASK-2ZQSB reviewer M1: post_artifact_submit's
    // version-bump must serialize per artifact. Without the write lock, N
    // concurrent resubmits all read the same current version and blind-write
    // version+1, so the final version lands below 1+N (lost updates) and vN
    // archives collide. With the lock they serialize to sequential versions.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn artifact_concurrent_resubmits_do_not_lose_version_bumps() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let project_root = tmp.path().join("myproject");
        seed_project(&home, &project_root, "test-proj");
        let state = direct_stage_test_state(home.clone()).await;

        let art_id = "ART-RSBM1";
        let _ = post_artifact_submit(
            State(state.clone()),
            Path(art_id.to_string()),
            Query(artifact_query("test-proj")),
            Json(ArtifactSubmitRequest {
                content: "<RichText>v1</RichText>\n".into(),
                title: Some("Resub".into()),
                subject_nodes: None,
                prompt: None,
            }),
        )
        .await
        .expect("initial submit should succeed");

        const N: u32 = 8;
        let mut handles = Vec::new();
        for i in 0..N {
            let st = state.clone();
            handles.push(tokio::spawn(async move {
                post_artifact_submit(
                    State(st),
                    Path(art_id.to_string()),
                    Query(artifact_query("test-proj")),
                    Json(ArtifactSubmitRequest {
                        content: format!("<RichText>resubmit {i}</RichText>\n"),
                        title: None,
                        subject_nodes: None,
                        prompt: None,
                    }),
                )
                .await
                .map(|j| j.0["version"].as_u64().unwrap())
            }));
        }
        let mut versions = Vec::new();
        for h in handles {
            versions.push(h.await.unwrap().expect("resubmit should succeed"));
        }

        // Each resubmit returned a distinct version, and the final artifact.org
        // version is exactly 1 + N (v1 initial + N bumps, none lost).
        let unique: std::collections::HashSet<_> = versions.iter().collect();
        assert_eq!(
            unique.len(),
            N as usize,
            "each resubmit needs a distinct version"
        );
        let summary = load_artifact(&artifact_dir(&project_root, art_id)).unwrap();
        assert_eq!(
            summary.version,
            1 + N,
            "all {N} resubmits must land (no lost version bumps)"
        );
    }

    #[tokio::test]
    async fn artifact_get_unknown_version_is_distinct_404_from_unknown_artifact() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let project_root = tmp.path().join("myproject");
        seed_project(&home, &project_root, "test-proj");
        let state = direct_stage_test_state(home.clone()).await;

        let art_id = "ART-VERSA";
        let _ = post_artifact_submit(
            State(state.clone()),
            Path(art_id.to_string()),
            Query(artifact_query("test-proj")),
            Json(ArtifactSubmitRequest {
                content: "<RichText>hi</RichText>\n".into(),
                title: Some("Versioned".into()),
                subject_nodes: None,
                prompt: None,
            }),
        )
        .await
        .expect("submit should succeed");

        let missing_artifact = get_artifact(
            State(state.clone()),
            Extension(Identity::Admin),
            Path("ART-MSSNG".to_string()),
            Query(artifact_query("test-proj")),
        )
        .await
        .expect_err("unknown artifact must 404");
        assert_eq!(missing_artifact.status, StatusCode::NOT_FOUND);

        let missing_version = get_artifact(
            State(state.clone()),
            Extension(Identity::Admin),
            Path(art_id.to_string()),
            Query(ArtifactQuery {
                version: Some(9),
                ..artifact_query("test-proj")
            }),
        )
        .await
        .expect_err("unknown version must 404");
        assert_eq!(missing_version.status, StatusCode::NOT_FOUND);
        assert_ne!(
            missing_artifact.message, missing_version.message,
            "no-such-artifact and no-such-version must carry distinct messages"
        );
    }

    #[tokio::test]
    async fn artifact_feedback_add_leaves_no_partial_state_on_writer_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let project_root = tmp.path().join("myproject");
        seed_project(&home, &project_root, "test-proj");
        let state = direct_stage_test_state(home.clone()).await;

        let art_id = "ART-ATMC1";
        let _ = post_artifact_submit(
            State(state.clone()),
            Path(art_id.to_string()),
            Query(artifact_query("test-proj")),
            Json(ArtifactSubmitRequest {
                content: "<RichText>hi</RichText>\n".into(),
                title: Some("Atomicity".into()),
                subject_nodes: None,
                prompt: None,
            }),
        )
        .await
        .expect("submit should succeed");

        let art_dir = artifact_dir(&project_root, art_id);
        let tx_path = project_root
            .join(".orgasmic")
            .join("tx")
            .join(format!("{}.org", Utc::now().format("%Y-%m")));
        let tx_before = std::fs::read_to_string(&tx_path).unwrap_or_default();

        // Sabotage: reviews.org is a directory, so the writer transaction's
        // rewrite-path validation fails deterministically before any
        // tmp/rename/tx-append work happens.
        std::fs::remove_file(art_dir.join("reviews.org")).unwrap();
        std::fs::create_dir(art_dir.join("reviews.org")).unwrap();

        let result = post_artifact_add_comment(
            State(state.clone()),
            Extension(Identity::Admin),
            Path(art_id.to_string()),
            Query(artifact_query("test-proj")),
            Json(ArtifactCommentRequest {
                message: "should not land".into(),
                anchor: None,
                resolution_target: None,
                author: None,
            }),
        )
        .await;
        assert!(result.is_err());

        // reviews.org is untouched (still the sabotage directory — no
        // partial rewrite escaped the failed transaction).
        assert!(art_dir.join("reviews.org").is_dir());
        // The tx log must not have gained a phantom artifact.comment.added
        // entry for a comment that never landed.
        let tx_after = std::fs::read_to_string(&tx_path).unwrap_or_default();
        assert_eq!(
            tx_before, tx_after,
            "tx log must not record a comment that failed to write"
        );
    }

    #[tokio::test]
    async fn artifact_comment_resolve_leaves_no_partial_state_on_writer_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let project_root = tmp.path().join("myproject");
        seed_project(&home, &project_root, "test-proj");
        let state = direct_stage_test_state(home.clone()).await;

        let art_id = "ART-ATMC2";
        let _ = post_artifact_submit(
            State(state.clone()),
            Path(art_id.to_string()),
            Query(artifact_query("test-proj")),
            Json(ArtifactSubmitRequest {
                content: "<RichText>hi</RichText>\n".into(),
                title: Some("Atomicity".into()),
                subject_nodes: None,
                prompt: None,
            }),
        )
        .await
        .expect("submit should succeed");

        let fb_resp = post_artifact_add_comment(
            State(state.clone()),
            Extension(Identity::Admin),
            Path(art_id.to_string()),
            Query(artifact_query("test-proj")),
            Json(ArtifactCommentRequest {
                message: "to be consumed".into(),
                anchor: None,
                resolution_target: None,
                author: None,
            }),
        )
        .await
        .expect("feedback should succeed");
        let cid = fb_resp.0["cid"].as_str().unwrap().to_string();

        let art_dir = artifact_dir(&project_root, art_id);
        let tx_path = project_root
            .join(".orgasmic")
            .join("tx")
            .join(format!("{}.org", Utc::now().format("%Y-%m")));
        let tx_before = std::fs::read_to_string(&tx_path).unwrap_or_default();

        // Sabotage the write target the same way as the add-path test.
        std::fs::remove_file(art_dir.join("reviews.org")).unwrap();
        std::fs::create_dir(art_dir.join("reviews.org")).unwrap();

        let result = post_artifact_comment_resolve(
            State(state.clone()),
            Extension(Identity::Admin),
            Path((art_id.to_string(), cid)),
            Query(artifact_query("test-proj")),
            Json(ArtifactCommentResolveRequest { resolved: true }),
        )
        .await;
        assert!(result.is_err());

        assert!(art_dir.join("reviews.org").is_dir());
        let tx_after = std::fs::read_to_string(&tx_path).unwrap_or_default();
        assert_eq!(
            tx_before, tx_after,
            "tx log must not record a resolution that failed to write"
        );
    }

    // ── artifact generation launch (TASK-2ZQSB acceptance) ───────────────────

    /// Overrides the shipped `artifactor` worker with an rmux/custom pairing
    /// for tests: a minimal interactive harness script that stays live and
    /// accepts `send_input` followups (hot-session regenerate path). Skips
    /// when rmux is unavailable — same pattern as orgasmic-drivers rmux live
    /// tests.
    fn seed_test_artifactor_worker(home: &Home) {
        let harness = home.user().join("artifactor-test-harness.sh");
        write(
            harness.clone(),
            "#!/bin/sh\nwhile true; do\n  echo '> ready'\n  echo READY\n  IFS= read -r line || exit 0\n  echo \"ECHO:$line\"\ndone\n",
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&harness).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&harness, perms).unwrap();
        }
        let harness_path = harness.display();
        let worker_org = format!(
            "* WORKER artifactor\n:PROPERTIES:\n:ID:                          artifactor\n:KIND:                        artifactor\n:DRIVER:                      rmux\n:HARNESS:                     custom\n:HARNESS_ARGS:                {harness_path}\n:PROVIDERS:                   anthropic\n:MODELS:                      claude-sonnet-5\n:REASONING_EFFORTS:           high\n:DEFAULT_PROVIDER:            anthropic\n:DEFAULT_MODEL:               claude-sonnet-5\n:DEFAULT_EFFORT:              high\n:LINKED_SKILLS:\n:APPLICABLE_STATES:           working, done, blocked, cancelled\n:MAX_ITERATIONS:              1\n:CONTEXT_BUDGET:              4000\n:VERSION:                     1\n:END:\n\n** Notes\nTest override (TASK-RH4M5 hot-session).\n"
        );
        write(home.user().join("workers/artifactor.org"), &worker_org);
    }

    fn rmux_available_for_test() -> bool {
        orgasmic_drivers::modes::rmux::probe_rmux_binary().found
    }

    /// Like `seed_test_artifactor_worker` but the harness sleeps before showing
    /// its composer prompt so an immediate followup hits the busy gate.
    fn seed_busy_harness_artifactor_worker(home: &Home) {
        let harness = home.user().join("artifactor-busy-harness.sh");
        write(
            harness.clone(),
            "#!/bin/sh\necho BOOTING\nsleep 120\nwhile true; do\n  echo '> ready'\n  IFS= read -r line || exit 0\n  echo \"ECHO:$line\"\ndone\n",
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&harness).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&harness, perms).unwrap();
        }
        let harness_path = harness.display();
        let worker_org = format!(
            "* WORKER artifactor\n:PROPERTIES:\n:ID:                          artifactor\n:KIND:                        artifactor\n:DRIVER:                      rmux\n:HARNESS:                     custom\n:HARNESS_ARGS:                {harness_path}\n:PROVIDERS:                   anthropic\n:MODELS:                      claude-sonnet-5\n:VERSION:                     1\n:END:\n"
        );
        write(home.user().join("workers/artifactor.org"), &worker_org);
    }

    /// A worker record with an explicitly unsupported driver/harness pair —
    /// `Worker::from_org` rejects it at load time, so `load_stage_worker`
    /// fails before any run is ever acquired. Used to exercise the
    /// launch-never-happened revert path deterministically.
    fn seed_broken_test_artifactor_worker(home: &Home) {
        write(
            home.user().join("workers/artifactor.org"),
            "* WORKER artifactor\n:PROPERTIES:\n:ID:                          artifactor\n:KIND:                        artifactor\n:DRIVER:                      acp-ws\n:HARNESS:                     cursor-agent\n:PROVIDERS:                   anthropic\n:MODELS:                      claude-sonnet-5\n:VERSION:                     1\n:END:\n",
        );
    }

    #[test]
    fn derive_artifact_title_collapses_whitespace_and_truncates() {
        assert_eq!(derive_artifact_title("  hello   world  "), "hello world");
        let long = "word ".repeat(40);
        let title = derive_artifact_title(&long);
        assert!(title.chars().count() <= 81, "{title}");
        assert!(title.ends_with('…'), "{title}");
    }

    fn open_comment_count_for_node(artifacts: &[ArtifactSummary], node_id: &str) -> usize {
        artifacts
            .iter()
            .filter(|artifact| artifact.subject_nodes.iter().any(|node| node == node_id))
            .map(|artifact| artifact.open_comment_count)
            .sum()
    }

    #[test]
    fn artifact_org_content_renders_empty_subject_nodes_property() {
        let org = artifact_org_content("ART-ABC", "Title", &[], "prompt", 0, "regenerating");
        let file = OrgFile::parse(org, "artifact.org").unwrap();
        let heading = file.headings.first().unwrap();
        assert_eq!(heading.property("SUBJECT_NODES"), Some(""));
        let parsed: Vec<String> = heading
            .property("SUBJECT_NODES")
            .unwrap_or("")
            .split_whitespace()
            .map(str::to_string)
            .collect();
        assert!(parsed.is_empty());
    }

    #[tokio::test]
    async fn assemble_artifact_context_empty_nodes_omits_subject_section() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let project_root = tmp.path().join("myproject");
        seed_project(&home, &project_root, "test-proj");
        let state = direct_stage_test_state(home.clone()).await;

        let context = assemble_artifact_context(&state, "test-proj", &[]).await;
        assert!(
            context.is_empty(),
            "expected no subject context, got: {context}"
        );
    }

    #[test]
    fn compile_artifact_generate_prompt_bundle_empty_subject_renders_not_set() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        symlink_repo_source(&home);

        let bundle = compile_artifact_generate_prompt_bundle(
            &home,
            "test-proj",
            "ART-NONE",
            "",
            "Draft a grilling artifact from this prompt",
            "",
        )
        .expect("compile should succeed");
        assert!(
            bundle.contains("not set"),
            "empty subject context should render as not set: {bundle}"
        );
        assert!(
            bundle.contains("Draft a grilling artifact from this prompt"),
            "user prompt must still be present: {bundle}"
        );
        assert!(
            !bundle.contains("Graph neighborhood"),
            "node-less bundle must not include graph neighborhood: {bundle}"
        );
    }

    #[tokio::test]
    async fn assemble_artifact_context_includes_subject_node_and_neighborhood() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let project_root = tmp.path().join("myproject");
        seed_project(&home, &project_root, "test-proj");
        write(
            task_file_path(&project_root, "backlog.org"),
            "#+title: sprint\n#+orgasmic_version: 1\n\n* BACKLOG TASK-PRE Pre-boot task :work:\n:PROPERTIES:\n:ID:               TASK-PRE\n:END:\n\n* BACKLOG TASK-DEP2 Dependent task :work:\n:PROPERTIES:\n:ID:               TASK-DEP2\n:DEPENDS_ON:       TASK-PRE\n:END:\n",
        );
        let state = direct_stage_test_state(home.clone()).await;
        let _ = state.index.refresh_project("test-proj").await;

        let context =
            assemble_artifact_context(&state, "test-proj", &["TASK-DEP2".to_string()]).await;
        assert!(context.contains("TASK-DEP2"), "{context}");
        assert!(context.contains("Dependent task"), "{context}");
        assert!(context.contains("Graph neighborhood"), "{context}");
        assert!(context.contains("TASK-DEP2"), "{context}");
        assert!(context.contains("TASK-PRE"), "{context}");
    }

    #[test]
    fn assemble_regen_context_lists_open_comments_and_extra_prompt() {
        let comments = vec![
            artifacts::CommentRecord {
                cid: "CID-abc12345".into(),
                author: "reviewer@test.com".into(),
                version: 1,
                anchor: "{}".into(),
                resolution_target: String::new(),
                resolved: false,
                consumed: false,
                message: "tighten the copy".into(),
            },
            artifacts::CommentRecord {
                cid: "CID-def67890".into(),
                author: "other@test.com".into(),
                version: 1,
                anchor: "{}".into(),
                resolution_target: String::new(),
                resolved: true,
                consumed: false,
                message: "already addressed".into(),
            },
        ];
        let out = assemble_regen_context("<RichText>v1</RichText>", &comments, "make it punchier");
        assert!(out.contains("<RichText>v1</RichText>"));
        assert!(out.contains("reviewer@test.com"));
        assert!(out.contains("tighten the copy"));
        assert!(out.contains("make it punchier"));
        assert!(out.contains("[CID-abc12345] reviewer@test.com (anchor: {}; resolves: -; resolved: false): tighten the copy"));
        assert!(out.contains("[CID-def67890] other@test.com (anchor: {}; resolves: -; resolved: true): already addressed"));
    }

    #[tokio::test]
    async fn post_artifact_generate_creates_regenerating_record_and_launches_run() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let project_root = tmp.path().join("myproject");
        seed_project(&home, &project_root, "test-proj");
        seed_test_artifactor_worker(&home);
        let state = direct_stage_test_state(home.clone()).await;

        let resp = post_artifact_generate(
            State(state.clone()),
            Extension(Identity::Admin),
            Query(artifact_query("test-proj")),
            Json(ArtifactGenerateRequest {
                nodes: vec!["TASK-PRE".to_string()],
                prompt: "Draft a wireframe for the login flow".to_string(),
            }),
        )
        .await
        .expect("generate should succeed");

        let art_id = resp.0.artifact_id.clone();
        assert!(art_id.starts_with("ART-"), "{art_id}");
        assert!(!resp.0.run_id.is_empty());

        let art_dir = artifact_dir(&project_root, &art_id);
        let summary = load_artifact(&art_dir).expect("artifact.org readable");
        assert_eq!(summary.state, "regenerating");
        assert_eq!(summary.version, 0);
        assert_eq!(summary.subject_nodes, vec!["TASK-PRE".to_string()]);
        assert!(art_dir.join("reviews.org").exists());

        let snapshot = state.supervisor.snapshot().await;
        assert!(snapshot.runs.iter().any(|r| r.run_id == resp.0.run_id));
        assert!(snapshot
            .runs
            .iter()
            .any(|r| r.task_id == format!("artifact.generate:{art_id}")));

        let tx_path = project_root
            .join(".orgasmic")
            .join("tx")
            .join(format!("{}.org", Utc::now().format("%Y-%m")));
        let tx = std::fs::read_to_string(&tx_path).unwrap();
        assert!(tx.contains("artifact.created"));
        assert!(tx.contains(&art_id));

        let _ = state
            .supervisor
            .release(&resp.0.run_id, "test cleanup", ReleaseOutcome::Completed)
            .await;
    }

    #[tokio::test]
    async fn post_artifact_generate_with_empty_nodes_succeeds_end_to_end() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let project_root = tmp.path().join("myproject");
        seed_project(&home, &project_root, "test-proj");
        seed_test_artifactor_worker(&home);
        let state = direct_stage_test_state(home.clone()).await;

        let resp = post_artifact_generate(
            State(state.clone()),
            Extension(Identity::Admin),
            Query(artifact_query("test-proj")),
            Json(ArtifactGenerateRequest {
                nodes: vec![],
                prompt: "Grill this idea before any decision exists".to_string(),
            }),
        )
        .await
        .expect("generate with empty nodes should succeed");

        let art_id = resp.0.artifact_id.clone();
        assert!(art_id.starts_with("ART-"), "{art_id}");
        assert!(!resp.0.run_id.is_empty());

        let art_dir = artifact_dir(&project_root, &art_id);
        let org = std::fs::read_to_string(art_dir.join("artifact.org")).unwrap();
        let file = OrgFile::parse(org, "artifact.org").unwrap();
        assert_eq!(
            file.headings
                .first()
                .and_then(|h| h.property("SUBJECT_NODES")),
            Some("")
        );

        let summary = load_artifact(&art_dir).expect("artifact.org readable");
        assert_eq!(summary.state, "regenerating");
        assert_eq!(summary.version, 0);
        assert!(summary.subject_nodes.is_empty());

        let list = get_artifacts(
            State(state.clone()),
            Extension(Identity::Admin),
            Query(artifact_query("test-proj")),
        )
        .await
        .expect("list should include node-less artifact");
        assert!(
            list.0.iter().any(|artifact| artifact.id == art_id),
            "node-less artifact must appear in the global list: {list:?}"
        );

        let _ = state
            .supervisor
            .release(&resp.0.run_id, "test cleanup", ReleaseOutcome::Completed)
            .await;
    }

    #[tokio::test]
    async fn post_artifact_generate_empty_nodes_excluded_from_node_rollups() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let project_root = tmp.path().join("myproject");
        seed_project(&home, &project_root, "test-proj");
        seed_test_artifactor_worker(&home);
        let state = direct_stage_test_state(home.clone()).await;

        let attached_id = "ART-ATTCH";
        let _ = post_artifact_submit(
            State(state.clone()),
            Path(attached_id.to_string()),
            Query(artifact_query("test-proj")),
            Json(ArtifactSubmitRequest {
                content: "<RichText>attached</RichText>\n".into(),
                title: Some("Attached".into()),
                subject_nodes: Some(vec!["TASK-PRE".into()]),
                prompt: Some("Attached prompt".into()),
            }),
        )
        .await
        .expect("attached submit should succeed");

        let _ = post_artifact_add_comment(
            State(state.clone()),
            Extension(Identity::Admin),
            Path(attached_id.to_string()),
            Query(artifact_query("test-proj")),
            Json(ArtifactCommentRequest {
                message: "needs work".into(),
                anchor: None,
                resolution_target: None,
                author: Some("reviewer@test.com".into()),
            }),
        )
        .await
        .expect("feedback should succeed");

        let rollup_before = open_comment_count_for_node(
            &get_artifacts(
                State(state.clone()),
                Extension(Identity::Admin),
                Query(artifact_query("test-proj")),
            )
            .await
            .expect("list before generate")
            .0,
            "TASK-PRE",
        );
        assert_eq!(rollup_before, 1);

        let resp = post_artifact_generate(
            State(state.clone()),
            Extension(Identity::Admin),
            Query(artifact_query("test-proj")),
            Json(ArtifactGenerateRequest {
                nodes: vec![],
                prompt: "Prompt-only artifact".to_string(),
            }),
        )
        .await
        .expect("generate with empty nodes should succeed");

        let _ = state.index.refresh_project("test-proj").await;
        let artifacts = get_artifacts(
            State(state.clone()),
            Extension(Identity::Admin),
            Query(artifact_query("test-proj")),
        )
        .await
        .expect("list after generate")
        .0;
        assert_eq!(artifacts.len(), 2);

        let nodeless = artifacts
            .iter()
            .find(|artifact| artifact.id == resp.0.artifact_id)
            .expect("node-less artifact listed globally");
        assert!(nodeless.subject_nodes.is_empty());

        assert_eq!(open_comment_count_for_node(&artifacts, "TASK-PRE"), 1);
        assert!(
            !artifacts
                .iter()
                .filter(|artifact| artifact.subject_nodes.iter().any(|node| node == "TASK-PRE"))
                .any(|artifact| artifact.id == resp.0.artifact_id),
            "node-less artifact must not appear in TASK-PRE rollups"
        );

        let _ = state
            .supervisor
            .release(&resp.0.run_id, "test cleanup", ReleaseOutcome::Completed)
            .await;
    }

    #[tokio::test]
    async fn post_artifact_regenerate_on_nodeless_artifact_succeeds() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let project_root = tmp.path().join("myproject");
        seed_project(&home, &project_root, "test-proj");
        seed_test_artifactor_worker(&home);
        let state = direct_stage_test_state(home.clone()).await;

        let art_id = "ART-NDES0";
        let _ = post_artifact_submit(
            State(state.clone()),
            Path(art_id.to_string()),
            Query(artifact_query("test-proj")),
            Json(ArtifactSubmitRequest {
                content: "<RichText>v1</RichText>\n".into(),
                title: Some("Nodeless".into()),
                subject_nodes: Some(vec![]),
                prompt: Some("Prompt-only artifact".into()),
            }),
        )
        .await
        .expect("submit should succeed");

        let subject_context = assemble_artifact_context(&state, "test-proj", &[]).await;
        assert!(subject_context.is_empty());

        let resp = post_artifact_regenerate(
            State(state.clone()),
            Extension(Identity::Admin),
            Path(art_id.to_string()),
            Query(artifact_query("test-proj")),
            Json(ArtifactRegenerateRequest {
                extra_prompt: Some("tighten the prose".into()),
            }),
        )
        .await
        .expect("regenerate on node-less artifact should succeed");
        assert!(!resp.0.run_id.is_empty());

        let art_dir = artifact_dir(&project_root, art_id);
        let summary = load_artifact(&art_dir).unwrap();
        assert_eq!(summary.state, "regenerating");
        assert!(summary.subject_nodes.is_empty());

        let _ = state
            .supervisor
            .release(&resp.0.run_id, "test cleanup", ReleaseOutcome::Completed)
            .await;
    }

    #[tokio::test]
    async fn post_artifact_regenerate_hot_path_reuses_live_run_id() {
        let _live_guard = live_session_guard();
        if !rmux_available_for_test() {
            eprintln!("skipping post_artifact_regenerate_hot_path_reuses_live_run_id: rmux binary not found");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let project_root = tmp.path().join("myproject");
        seed_project(&home, &project_root, "test-proj");
        seed_test_artifactor_worker(&home);
        let state = direct_stage_test_state(home.clone()).await;

        let art_id = "ART-HTSS1";
        let _ = post_artifact_submit(
            State(state.clone()),
            Path(art_id.to_string()),
            Query(artifact_query("test-proj")),
            Json(ArtifactSubmitRequest {
                content: "<RichText>v1</RichText>\n".into(),
                title: Some("Hot session".into()),
                subject_nodes: Some(vec!["TASK-PRE".into()]),
                prompt: Some("Initial prompt".into()),
            }),
        )
        .await
        .expect("submit should succeed");

        let fb_resp = post_artifact_add_comment(
            State(state.clone()),
            Extension(Identity::Admin),
            Path(art_id.to_string()),
            Query(artifact_query("test-proj")),
            Json(ArtifactCommentRequest {
                message: "round 1 feedback".into(),
                anchor: None,
                resolution_target: None,
                author: Some("reviewer@test.com".into()),
            }),
        )
        .await
        .expect("feedback should succeed");
        let cid1 = fb_resp.0["cid"].as_str().unwrap().to_string();

        let first = post_artifact_regenerate(
            State(state.clone()),
            Extension(Identity::Admin),
            Path(art_id.to_string()),
            Query(artifact_query("test-proj")),
            Json(ArtifactRegenerateRequest {
                extra_prompt: Some("first pass".into()),
            }),
        )
        .await
        .expect("first regenerate should succeed (cold spawn)");

        let art_dir = artifact_dir(&project_root, art_id);
        assert_eq!(load_artifact(&art_dir).unwrap().state, "regenerating");

        // Simulate round 1 completing: submit v2 while the hot session stays live.
        let _ = post_artifact_submit(
            State(state.clone()),
            Path(art_id.to_string()),
            Query(artifact_query("test-proj")),
            Json(ArtifactSubmitRequest {
                content: "<RichText>v2</RichText>\n".into(),
                title: None,
                subject_nodes: None,
                prompt: None,
            }),
        )
        .await
        .expect("submit v2 should succeed");
        assert_eq!(load_artifact(&art_dir).unwrap().version, 2);

        let fb2 = post_artifact_add_comment(
            State(state.clone()),
            Extension(Identity::Admin),
            Path(art_id.to_string()),
            Query(artifact_query("test-proj")),
            Json(ArtifactCommentRequest {
                message: "round 2 feedback".into(),
                anchor: None,
                resolution_target: None,
                author: Some("reviewer@test.com".into()),
            }),
        )
        .await
        .expect("round 2 feedback should succeed");
        let cid2 = fb2.0["cid"].as_str().unwrap().to_string();

        let reviews_before = std::fs::read_to_string(art_dir.join("reviews.org")).unwrap();

        // Wait for the harness to finish the first dispatch and show its
        // composer prompt before routing round 2 via send_input.
        let second = {
            let mut last_err = None;
            let mut resp = None;
            for _ in 0..40 {
                match post_artifact_regenerate(
                    State(state.clone()),
                    Extension(Identity::Admin),
                    Path(art_id.to_string()),
                    Query(artifact_query("test-proj")),
                    Json(ArtifactRegenerateRequest {
                        extra_prompt: Some("second pass".into()),
                    }),
                )
                .await
                {
                    Ok(ok) => {
                        resp = Some(ok);
                        break;
                    }
                    Err(err) if err.status == StatusCode::CONFLICT => {
                        last_err = Some(err);
                        tokio::time::sleep(Duration::from_millis(250)).await;
                    }
                    Err(err) => panic!("unexpected regenerate error: {err:?}"),
                }
            }
            resp.unwrap_or_else(|| {
                panic!(
                    "second regenerate should route via send_input (hot path): {:?}",
                    last_err
                )
            })
        };
        assert_eq!(
            second.0.run_id, first.0.run_id,
            "two consecutive rounds must reuse one run_id"
        );
        assert_eq!(load_artifact(&art_dir).unwrap().state, "regenerating");

        let detail = load_artifact_detail(&art_dir, None, true).unwrap();
        for cid in [cid1, cid2] {
            let comment = detail
                .comments
                .iter()
                .find(|c| c.cid == cid)
                .expect("comment present");
            assert!(comment.consumed, "{comment:?}");
        }

        let tx_after = std::fs::read_to_string(
            project_root
                .join(".orgasmic/tx")
                .join(format!("{}.org", Utc::now().format("%Y-%m"))),
        )
        .unwrap_or_default();
        assert!(tx_after.contains("artifact.regenerated"));
        assert_ne!(
            reviews_before,
            std::fs::read_to_string(art_dir.join("reviews.org")).unwrap()
        );

        let _ = state
            .supervisor
            .release(&first.0.run_id, "test cleanup", ReleaseOutcome::Completed)
            .await;
    }

    #[tokio::test]
    async fn post_artifact_regenerate_hot_path_rejects_busy_harness_without_mutation() {
        let _live_guard = live_session_guard();
        if !rmux_available_for_test() {
            eprintln!(
                "skipping post_artifact_regenerate_hot_path_rejects_busy_harness_without_mutation: rmux binary not found"
            );
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let project_root = tmp.path().join("myproject");
        seed_project(&home, &project_root, "test-proj");
        seed_busy_harness_artifactor_worker(&home);
        let state = direct_stage_test_state(home.clone()).await;

        let art_id = "ART-BSYH1";
        let _ = post_artifact_submit(
            State(state.clone()),
            Path(art_id.to_string()),
            Query(artifact_query("test-proj")),
            Json(ArtifactSubmitRequest {
                content: "<RichText>v1</RichText>\n".into(),
                title: Some("Busy harness".into()),
                subject_nodes: Some(vec![]),
                prompt: Some("Initial".into()),
            }),
        )
        .await
        .expect("submit should succeed");

        let _ = post_artifact_add_comment(
            State(state.clone()),
            Extension(Identity::Admin),
            Path(art_id.to_string()),
            Query(artifact_query("test-proj")),
            Json(ArtifactCommentRequest {
                message: "needs work".into(),
                anchor: None,
                resolution_target: None,
                author: None,
            }),
        )
        .await
        .expect("feedback should succeed");

        let first = post_artifact_regenerate(
            State(state.clone()),
            Extension(Identity::Admin),
            Path(art_id.to_string()),
            Query(artifact_query("test-proj")),
            Json(ArtifactRegenerateRequest {
                extra_prompt: Some("first pass".into()),
            }),
        )
        .await
        .expect("first regenerate should succeed");

        let art_dir = artifact_dir(&project_root, art_id);
        let org_before = std::fs::read_to_string(art_dir.join("artifact.org")).unwrap();
        let reviews_before = std::fs::read_to_string(art_dir.join("reviews.org")).unwrap();
        let tx_path = project_root
            .join(".orgasmic/tx")
            .join(format!("{}.org", Utc::now().format("%Y-%m")));
        let tx_before = std::fs::read_to_string(&tx_path).unwrap_or_default();

        // Harness is mid-turn until the dispatch prompt is consumed and the
        // composer prompt reappears — followup must be refused with no durable
        // mutation (arch_045Q0.1 ordering).
        let err = post_artifact_regenerate(
            State(state.clone()),
            Extension(Identity::Admin),
            Path(art_id.to_string()),
            Query(artifact_query("test-proj")),
            Json(ArtifactRegenerateRequest {
                extra_prompt: Some("too soon".into()),
            }),
        )
        .await
        .expect_err("followup while harness busy must be refused");
        assert_eq!(err.status, StatusCode::CONFLICT);

        assert_eq!(
            std::fs::read_to_string(art_dir.join("artifact.org")).unwrap(),
            org_before
        );
        assert_eq!(
            std::fs::read_to_string(art_dir.join("reviews.org")).unwrap(),
            reviews_before
        );
        assert_eq!(
            std::fs::read_to_string(&tx_path).unwrap_or_default(),
            tx_before
        );

        let _ = state
            .supervisor
            .release(&first.0.run_id, "test cleanup", ReleaseOutcome::Completed)
            .await;
    }

    #[tokio::test]
    async fn post_artifact_regenerate_cold_spawns_after_forgotten_run() {
        let _live_guard = live_session_guard();
        if !rmux_available_for_test() {
            eprintln!(
                "skipping post_artifact_regenerate_cold_spawns_after_forgotten_run: rmux binary not found"
            );
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let project_root = tmp.path().join("myproject");
        seed_project(&home, &project_root, "test-proj");
        seed_test_artifactor_worker(&home);
        let state = direct_stage_test_state(home.clone()).await;

        let art_id = "ART-CDRS1";
        let _ = post_artifact_submit(
            State(state.clone()),
            Path(art_id.to_string()),
            Query(artifact_query("test-proj")),
            Json(ArtifactSubmitRequest {
                content: "<RichText>v1</RichText>\n".into(),
                title: Some("Restart path".into()),
                subject_nodes: Some(vec![]),
                prompt: Some("Initial".into()),
            }),
        )
        .await
        .expect("submit should succeed");

        let _ = post_artifact_add_comment(
            State(state.clone()),
            Extension(Identity::Admin),
            Path(art_id.to_string()),
            Query(artifact_query("test-proj")),
            Json(ArtifactCommentRequest {
                message: "round 1".into(),
                anchor: None,
                resolution_target: None,
                author: None,
            }),
        )
        .await
        .expect("feedback should succeed");

        let first = post_artifact_regenerate(
            State(state.clone()),
            Extension(Identity::Admin),
            Path(art_id.to_string()),
            Query(artifact_query("test-proj")),
            Json(ArtifactRegenerateRequest {
                extra_prompt: Some("first round".into()),
            }),
        )
        .await
        .expect("first regenerate should cold-spawn");
        let first_run_id = first.0.run_id.clone();

        // Simulate daemon restart: release drops the run from the supervisor.
        state
            .supervisor
            .release(&first_run_id, "simulate restart", ReleaseOutcome::Completed)
            .await
            .unwrap();

        let _ = post_artifact_submit(
            State(state.clone()),
            Path(art_id.to_string()),
            Query(artifact_query("test-proj")),
            Json(ArtifactSubmitRequest {
                content: "<RichText>v2</RichText>\n".into(),
                title: None,
                subject_nodes: None,
                prompt: None,
            }),
        )
        .await
        .expect("submit v2 after restart");

        let _ = post_artifact_add_comment(
            State(state.clone()),
            Extension(Identity::Admin),
            Path(art_id.to_string()),
            Query(artifact_query("test-proj")),
            Json(ArtifactCommentRequest {
                message: "after restart".into(),
                anchor: None,
                resolution_target: None,
                author: None,
            }),
        )
        .await
        .expect("feedback should succeed");

        let resp = post_artifact_regenerate(
            State(state.clone()),
            Extension(Identity::Admin),
            Path(art_id.to_string()),
            Query(artifact_query("test-proj")),
            Json(ArtifactRegenerateRequest {
                extra_prompt: Some("post-restart".into()),
            }),
        )
        .await
        .expect("regenerate should cold-spawn when no live holder");
        assert!(!resp.0.run_id.is_empty());
        assert_ne!(
            resp.0.run_id, first_run_id,
            "post-restart regenerate must spawn a fresh run"
        );
        assert_eq!(
            load_artifact(&artifact_dir(&project_root, art_id))
                .unwrap()
                .state,
            "regenerating"
        );

        let _ = state
            .supervisor
            .release(&resp.0.run_id, "test cleanup", ReleaseOutcome::Completed)
            .await;
    }

    #[tokio::test]
    async fn artifact_release_watcher_reverts_to_submitted_at_current_version_mid_round() {
        let _live_guard = live_session_guard();
        if !rmux_available_for_test() {
            eprintln!(
                "skipping artifact_release_watcher_reverts_to_submitted_at_current_version_mid_round: rmux binary not found"
            );
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let project_root = tmp.path().join("myproject");
        seed_project(&home, &project_root, "test-proj");
        seed_test_artifactor_worker(&home);
        let state = direct_stage_test_state(home.clone()).await;

        let art_id = "ART-RVRT2";
        let _ = post_artifact_submit(
            State(state.clone()),
            Path(art_id.to_string()),
            Query(artifact_query("test-proj")),
            Json(ArtifactSubmitRequest {
                content: "<RichText>v1</RichText>\n".into(),
                title: Some("Revert v1".into()),
                subject_nodes: Some(vec![]),
                prompt: Some("Initial".into()),
            }),
        )
        .await
        .expect("submit v1 should succeed");

        let _ = post_artifact_add_comment(
            State(state.clone()),
            Extension(Identity::Admin),
            Path(art_id.to_string()),
            Query(artifact_query("test-proj")),
            Json(ArtifactCommentRequest {
                message: "round 2 feedback".into(),
                anchor: None,
                resolution_target: None,
                author: None,
            }),
        )
        .await
        .expect("feedback should succeed");

        let regen = post_artifact_regenerate(
            State(state.clone()),
            Extension(Identity::Admin),
            Path(art_id.to_string()),
            Query(artifact_query("test-proj")),
            Json(ArtifactRegenerateRequest {
                extra_prompt: Some("round 2".into()),
            }),
        )
        .await
        .expect("regenerate should succeed");

        let art_dir = artifact_dir(&project_root, art_id);
        assert_eq!(load_artifact(&art_dir).unwrap().state, "regenerating");
        assert_eq!(load_artifact(&art_dir).unwrap().version, 1);

        state
            .supervisor
            .release(
                &regen.0.run_id,
                "test: session died mid-round-2",
                ReleaseOutcome::Failed,
            )
            .await
            .unwrap();

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let summary = load_artifact(&art_dir).unwrap();
            if summary.state == "submitted" && summary.version == 1 {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "watcher did not revert to submitted@v1 in time; state={} version={}",
                summary.state,
                summary.version
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    #[tokio::test]
    async fn post_artifact_generate_reverts_to_failed_when_worker_record_is_invalid() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let project_root = tmp.path().join("myproject");
        seed_project(&home, &project_root, "test-proj");
        seed_broken_test_artifactor_worker(&home);
        let state = direct_stage_test_state(home.clone()).await;

        let result = post_artifact_generate(
            State(state.clone()),
            Extension(Identity::Admin),
            Query(artifact_query("test-proj")),
            Json(ArtifactGenerateRequest {
                nodes: vec!["TASK-PRE".to_string()],
                prompt: "test".to_string(),
            }),
        )
        .await;
        let err = result.expect_err("generate must fail when the worker record is invalid");
        assert_ne!(err.status, StatusCode::CONFLICT);

        let summaries = artifacts::load_project_artifacts(&project_root);
        assert_eq!(summaries.len(), 1, "{summaries:?}");
        assert_eq!(summaries[0].state, "failed");
        assert_eq!(summaries[0].version, 0);
    }

    #[tokio::test]
    async fn post_artifact_regenerate_reverts_to_submitted_when_worker_record_is_invalid() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let project_root = tmp.path().join("myproject");
        seed_project(&home, &project_root, "test-proj");
        seed_broken_test_artifactor_worker(&home);
        let state = direct_stage_test_state(home.clone()).await;

        let art_id = "ART-BDWK1";
        let _ = post_artifact_submit(
            State(state.clone()),
            Path(art_id.to_string()),
            Query(artifact_query("test-proj")),
            Json(ArtifactSubmitRequest {
                content: "<RichText>v1</RichText>\n".into(),
                title: Some("Broken worker regen".into()),
                subject_nodes: None,
                prompt: None,
            }),
        )
        .await
        .expect("submit should succeed");

        let result = post_artifact_regenerate(
            State(state.clone()),
            Extension(Identity::Admin),
            Path(art_id.to_string()),
            Query(artifact_query("test-proj")),
            Json(ArtifactRegenerateRequest { extra_prompt: None }),
        )
        .await;
        let err = result.expect_err("regenerate must fail when the worker record is invalid");
        assert_ne!(err.status, StatusCode::CONFLICT);

        let art_dir = artifact_dir(&project_root, art_id);
        let summary = load_artifact(&art_dir).unwrap();
        assert_eq!(summary.state, "submitted");
        assert_eq!(summary.version, 1);
    }

    #[tokio::test]
    async fn artifact_release_watcher_reverts_regenerating_state_when_run_ends_without_submit() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let project_root = tmp.path().join("myproject");
        seed_project(&home, &project_root, "test-proj");
        seed_test_artifactor_worker(&home);
        let state = direct_stage_test_state(home.clone()).await;

        let resp = post_artifact_generate(
            State(state.clone()),
            Extension(Identity::Admin),
            Query(artifact_query("test-proj")),
            Json(ArtifactGenerateRequest {
                nodes: vec!["TASK-PRE".to_string()],
                prompt: "test".to_string(),
            }),
        )
        .await
        .expect("generate should succeed");
        let art_id = resp.0.artifact_id.clone();
        let art_dir = artifact_dir(&project_root, &art_id);

        assert_eq!(load_artifact(&art_dir).unwrap().state, "regenerating");

        // Simulate the run ending (crash/timeout/manual release) without ever
        // calling `orgasmic artifact submit`.
        state
            .supervisor
            .release(
                &resp.0.run_id,
                "test: simulate crash",
                ReleaseOutcome::Failed,
            )
            .await
            .unwrap();

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let summary = load_artifact(&art_dir).unwrap();
            if summary.state == "failed" {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "watcher did not revert regenerating state in time; state={}",
                summary.state
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        let tx_path = project_root
            .join(".orgasmic")
            .join("tx")
            .join(format!("{}.org", Utc::now().format("%Y-%m")));
        let tx = std::fs::read_to_string(&tx_path).unwrap();
        assert!(tx.contains("artifact.generation.failed"));
    }

    // ---- authorization wiring (arch_Z8CW2 / dec_KF2MR) ---------------------

    fn member(grants: &[(&str, &str)]) -> Identity {
        Identity::Member {
            name: "alice".into(),
            grants: grants
                .iter()
                .map(|(p, r)| (p.to_string(), r.to_string()))
                .collect(),
        }
    }

    fn graph_query(project: &str) -> GraphQuery {
        GraphQuery {
            project: Some(project.into()),
        }
    }

    fn seed_two_projects(home: &Home, root_a: &FsPath, root_b: &FsPath) {
        symlink_repo_source(home);
        for (root, id) in [(root_a, "proj-a"), (root_b, "proj-b")] {
            write(
                root.join(".orgasmic/project.org"),
                &format!(
                    "#+title: {id}\n#+orgasmic_version: 1\n\n* PROJECT {id}\n:PROPERTIES:\n:ID:               {id}\n:END:\n"
                ),
            );
            write(
                task_file_path(root, "backlog.org"),
                "#+title: sprint\n#+orgasmic_version: 1\n\n* BACKLOG TASK-PRE Pre-boot task :work:\n:PROPERTIES:\n:ID:               TASK-PRE\n:END:\n",
            );
        }
        write(
            home.board(),
            &format!(
                "#+title: orgasmic board\n#+orgasmic_version: 1\n\n* PROJECT proj-a\n:PROPERTIES:\n:ID:               proj-a\n:PATH:             {}\n:BRANCH:           main\n:STATUS:           active\n:END:\n\n* PROJECT proj-b\n:PROPERTIES:\n:ID:               proj-b\n:PATH:             {}\n:BRANCH:           main\n:STATUS:           active\n:END:\n",
                root_a.display(),
                root_b.display(),
            ),
        );
    }

    /// An `artifacts`-role member reaches artifact + project-detail reads but is
    /// 403'd on the tasks and graph surfaces (they lack TasksRead/GraphRead).
    #[tokio::test]
    async fn authz_artifacts_member_gated_on_tasks_and_graph_reads() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let project_root = tmp.path().join("proj-a");
        seed_project(&home, &project_root, "proj-a");
        seed_minimal_graph(&project_root);
        let state = direct_stage_test_state(home).await;

        let id = member(&[("proj-a", "artifacts")]);

        // ProjectRead + ArtifactsRead: allowed.
        assert!(
            get_project(
                State(state.clone()),
                Extension(id.clone()),
                Path("proj-a".into())
            )
            .await
            .is_ok(),
            "artifacts role carries ProjectRead"
        );
        assert!(
            get_artifacts(
                State(state.clone()),
                Extension(id.clone()),
                Query(artifact_query("proj-a"))
            )
            .await
            .is_ok(),
            "artifacts role carries ArtifactsRead"
        );

        // TasksRead: denied.
        let tasks = get_project_tasks(
            State(state.clone()),
            Extension(id.clone()),
            Path("proj-a".into()),
        )
        .await;
        assert_eq!(tasks.err().map(|e| e.status), Some(StatusCode::FORBIDDEN));

        // GraphRead: denied.
        let decisions = get_decisions(
            State(state.clone()),
            Extension(id.clone()),
            Query(graph_query("proj-a")),
        )
        .await;
        assert_eq!(
            decisions.err().map(|e| e.status),
            Some(StatusCode::FORBIDDEN)
        );
    }

    /// A `viewer` member reads tasks and the graph/decisions surfaces (200).
    #[tokio::test]
    async fn authz_viewer_member_reads_tasks_and_graph() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let project_root = tmp.path().join("proj-a");
        seed_project(&home, &project_root, "proj-a");
        seed_minimal_graph(&project_root);
        let state = direct_stage_test_state(home).await;

        let id = member(&[("proj-a", "viewer")]);

        assert!(get_project_tasks(
            State(state.clone()),
            Extension(id.clone()),
            Path("proj-a".into())
        )
        .await
        .is_ok());
        assert!(get_graph_nodes(
            State(state.clone()),
            Extension(id.clone()),
            Query(graph_query("proj-a"))
        )
        .await
        .is_ok());
        assert!(get_decisions(
            State(state.clone()),
            Extension(id.clone()),
            Query(graph_query("proj-a"))
        )
        .await
        .is_ok());
    }

    /// The cross-project list endpoints never 403 a member — they filter to the
    /// member's granted projects (admin sees all).
    #[tokio::test]
    async fn authz_board_and_projects_lists_filtered_to_member_grants() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let root_a = tmp.path().join("proj-a");
        let root_b = tmp.path().join("proj-b");
        seed_two_projects(&home, &root_a, &root_b);
        let state = direct_stage_test_state(home).await;

        // Admin sees both.
        assert_eq!(
            get_board(State(state.clone()), Extension(Identity::Admin))
                .await
                .0
                .len(),
            2
        );
        assert_eq!(
            get_projects(State(state.clone()), Extension(Identity::Admin))
                .await
                .0
                .len(),
            2
        );

        // A member granted only proj-a sees only proj-a in both lists.
        let id = member(&[("proj-a", "viewer")]);
        let board = get_board(State(state.clone()), Extension(id.clone())).await;
        assert_eq!(
            board.0.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(),
            vec!["proj-a"]
        );
        let projects = get_projects(State(state.clone()), Extension(id.clone())).await;
        assert_eq!(
            projects
                .0
                .iter()
                .map(|p| p.project_id.as_str())
                .collect::<Vec<_>>(),
            vec!["proj-a"]
        );
    }

    // ---- WebSocket authorization wiring -----------------------------------

    /// Mint a member session cookie via `POST /login` (the same flow the UI
    /// uses), returning the `name=value` pair to send on later requests.
    async fn member_session_cookie(addr: std::net::SocketAddr, member_token: &str) -> String {
        let client = reqwest::Client::new();
        let resp = client
            .post(format!("http://{addr}/api/login"))
            .json(&serde_json::json!({ "token": member_token }))
            .send()
            .await
            .unwrap();
        assert!(
            resp.status().is_success(),
            "member login failed: {}",
            resp.status()
        );
        let raw = resp
            .headers()
            .get("set-cookie")
            .expect("login must set a session cookie")
            .to_str()
            .unwrap();
        raw.split(';').next().unwrap().to_string()
    }

    async fn connect_ws_cookie(addr: std::net::SocketAddr, cookie: &str, topic: &str) -> TestWs {
        let url = format!("ws://{addr}/api/ws?topic={topic}");
        let mut req = url.into_client_request().unwrap();
        req.headers_mut().insert("cookie", cookie.parse().unwrap());
        let (ws, _resp) = tokio_tungstenite::connect_async(req).await.unwrap();
        ws
    }

    async fn connect_tmux_ws_cookie(
        addr: std::net::SocketAddr,
        cookie: &str,
        run_id: &str,
    ) -> Result<TestWs, tokio_tungstenite::tungstenite::Error> {
        let url = format!("ws://{addr}/api/ws/tmux/{run_id}");
        let mut req = url.into_client_request().unwrap();
        req.headers_mut().insert("cookie", cookie.parse().unwrap());
        tokio_tungstenite::connect_async(req)
            .await
            .map(|(ws, _)| ws)
    }

    /// The next event-bus message delivered to a socket, decoded.
    async fn next_ws_event(ws: &mut TestWs) -> Value {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                match ws.next().await.unwrap().unwrap() {
                    Message::Text(text) => return serde_json::from_str::<Value>(&text).unwrap(),
                    Message::Close(frame) => panic!("websocket closed early: {frame:?}"),
                    _ => {}
                }
            }
        })
        .await
        .expect("timed out waiting for a ws event")
    }

    /// An artifacts-only member's `/ws` receives artifact events but not the
    /// graph/task events their role can't see; admin receives everything.
    #[tokio::test]
    async fn ws_event_stream_filters_by_member_capability() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let project_root = tmp.path().join("proj-a");
        seed_project(&home, &project_root, "proj-a");
        let member_token = orgasmic_core::add_member(
            &home,
            "alice",
            &[("proj-a".to_string(), "artifacts".to_string())],
        )
        .unwrap();

        let running = crate::Daemon::run(home.clone(), test_options())
            .await
            .expect("boot daemon");
        let token = read_token(&home);
        let cookie = member_session_cookie(running.addr, &member_token).await;

        // Both sockets subscribe to every topic; filtering is per-identity.
        let mut member_ws = connect_ws_cookie(running.addr, &cookie, "").await;
        let mut admin_ws = connect_ws(running.addr, &token, "").await;

        let client = reqwest::Client::new();

        // (1) A graph event — visible to admin, filtered out for an artifacts member.
        let graph = client
            .post(format!("http://{}/api/architecture", running.addr))
            .bearer_auth(&token)
            .json(&serde_json::json!({
                "project": "proj-a",
                "request_id": "ws-authz-graph",
                "title": "WS authz architecture node"
            }))
            .send()
            .await
            .unwrap();
        assert!(
            graph.status().is_success(),
            "create arch: {}",
            graph.status()
        );

        // (2) An artifact event — visible to both.
        let artifact = client
            .post(format!(
                "http://{}/api/artifacts/ART-WSXYZ/submit?project=proj-a",
                running.addr
            ))
            .bearer_auth(&token)
            .json(&serde_json::json!({
                "content": "<RichText>hello</RichText>\n",
                "title": "WS gate artifact",
                "subject_nodes": ["arch_ARSPJ"],
                "prompt": "test"
            }))
            .send()
            .await
            .unwrap();
        assert!(
            artifact.status().is_success(),
            "submit artifact: {}",
            artifact.status()
        );

        // Admin sees the graph event (and later the artifact event).
        let admin_graph = next_event_kind(&mut admin_ws, "graph_node_created").await;
        assert_eq!(admin_graph["payload"]["project_id"], "proj-a");
        let admin_artifact = next_event_kind(&mut admin_ws, "artifact_changed").await;
        assert_eq!(admin_artifact["payload"]["project_id"], "proj-a");

        // The member's FIRST delivered event is the artifact event: the graph
        // event (published first) was filtered out by the topic gate.
        let member_first = next_ws_event(&mut member_ws).await;
        assert_eq!(
            member_first["payload"]["kind"], "artifact_changed",
            "artifacts member must not receive graph events"
        );
        assert_eq!(member_first["payload"]["project_id"], "proj-a");

        let _ = running.shutdown.send(());
        let _ = running.join.await;
    }

    /// A member is denied streaming a run they can't watch (the handshake is
    /// rejected before upgrade); admin is unaffected by the stream gate.
    #[tokio::test]
    async fn tmux_ws_member_denied_stream_admin_allowed() {
        let _live_guard = live_session_guard();
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let project_root = tmp.path().join("proj-a");
        seed_project(&home, &project_root, "proj-a");
        let member_token = orgasmic_core::add_member(
            &home,
            "alice",
            &[("proj-a".to_string(), "viewer".to_string())],
        )
        .unwrap();

        let running = crate::Daemon::run(home.clone(), test_options())
            .await
            .expect("boot daemon");
        let token = read_token(&home);
        let cookie = member_session_cookie(running.addr, &member_token).await;

        // 'missing-run' has no supervisor record → no resolvable project → a
        // member cannot be authorized, so the WS handshake is rejected (403).
        let member_attempt = connect_tmux_ws_cookie(running.addr, &cookie, "missing-run").await;
        assert!(
            member_attempt.is_err(),
            "member must be denied streaming a run they can't watch"
        );

        // Admin is unaffected: the socket upgrades and the daemon returns the
        // honest 'no live run' error frame rather than a 403.
        let mut admin_ws = connect_tmux_ws(running.addr, &token, "missing-run").await;
        let err = next_ws_text_containing(&mut admin_ws, "no live run").await;
        assert!(err.contains("missing-run"));

        let _ = running.shutdown.send(());
        let _ = running.join.await;
    }
}

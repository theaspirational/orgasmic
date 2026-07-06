// arch: arch_C87Z9.2
// orgasmic:arch_C87Z9, arch_Z3Z3V
//! WebSocket subscription endpoint.
//!
//! `GET /ws?topic=board[,task,...]` upgrades to a WS stream that emits the
//! daemon's event bus messages filtered to the requested topics. If no
//! topic is supplied, every topic is forwarded. AC #6.

use std::collections::HashSet;
use std::process::Stdio;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Extension, Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::broadcast::error::RecvError;
use tokio::time::{interval, Duration};
use tracing::debug;

use crate::api::ApiState;
use crate::authz::{self, Action, Identity};
use crate::events::Topic;

#[derive(Debug, Deserialize, Default)]
pub struct WsQuery {
    #[serde(default)]
    pub topic: Option<String>,
}

pub async fn handler(
    ws: WebSocketUpgrade,
    Query(q): Query<WsQuery>,
    Extension(identity): Extension<Identity>,
    State(state): State<ApiState>,
) -> Response {
    let topics: HashSet<Topic> = match q.topic.as_deref() {
        Some(raw) => raw
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .filter_map(Topic::parse)
            .collect(),
        None => Topic::ALL.into_iter().collect(),
    };
    let bus = state.events.clone();
    let shutdown = state.shutdown.clone();
    ws.on_upgrade(move |socket| serve_socket(socket, bus, topics, identity, shutdown))
}

/// Whether `identity` may send input into a live session. `sessions.interact`
/// is admin-only in v1 (dec_KF2MR): every member gets a strictly read-only
/// stream, so their composer/keystroke frames are dropped.
fn tmux_input_allowed(identity: &Identity) -> bool {
    matches!(identity, Identity::Admin)
}

/// Whether `identity` may stream a run's terminal. Admin always may; a member
/// must hold `SessionsWatch` on the run's project. A run we cannot map to a
/// project (orphan mux session, project-less system run, forced mock) is
/// unwatchable by a member and denied — we never fall back to a coarse allow.
fn tmux_stream_allowed(identity: &Identity, run_project: Option<&str>) -> bool {
    match run_project {
        Some(project) => authz::require(identity, Some(project), Action::SessionsWatch).is_ok(),
        None => matches!(identity, Identity::Admin),
    }
}

pub async fn tmux_handler(
    ws: WebSocketUpgrade,
    Path(run_id): Path<String>,
    Extension(identity): Extension<Identity>,
    State(state): State<ApiState>,
) -> Response {
    let force_mock = std::env::var("ORGASMIC_TMUX_WS_MOCK")
        .map(|value| value == "1")
        .unwrap_or(false);

    // Resolve which multiplexer backs this run *and* the run's project, from a
    // single supervisor snapshot. tmux is the default; runs whose driver
    // transport is `rmux` (TASK-104) attach through the same PTY bridge with
    // `rmux attach-session`, against the rmux session the driver created.
    let snapshot = state.supervisor.snapshot().await;
    let run_record = snapshot.runs.iter().find(|run| run.run_id == run_id);
    let run_project = run_record.and_then(|run| run.project_id.clone());

    // STREAM gate (dec_KF2MR): a member needs SessionsWatch on the run's
    // project; admin is unaffected. Reject before upgrading so the client sees
    // a clean 403 rather than a socket that opens then immediately closes.
    if !tmux_stream_allowed(&identity, run_project.as_deref()) {
        return (
            StatusCode::FORBIDDEN,
            "forbidden: sessions.watch is required to stream this run",
        )
            .into_response();
    }
    // INPUT gate: members stream read-only; only admin may write to the PTY.
    let allow_input = tmux_input_allowed(&identity);

    let target = if force_mock {
        None
    } else {
        run_record.map(|run| match MuxKind::from_transport(&run.driver) {
            MuxKind::Rmux => (
                MuxKind::Rmux,
                orgasmic_drivers::probe_rmux_binary()
                    .path
                    .unwrap_or_else(|| "rmux".to_string()),
                orgasmic_drivers::modes::rmux::rmux_session_name(&run.identity),
            ),
            MuxKind::Tmux => (
                MuxKind::Tmux,
                "tmux".to_string(),
                orgasmic_drivers::modes::tmux::tmux_session_name(&run.identity),
            ),
        })
    };

    // Defense-in-depth (1c): the supervisor has no record for this run, but a
    // matching mux session may still be alive — e.g. a system-wide manager
    // session whose run wasn't rehydrated by boot auto-reattach, or a request
    // landing in the window before reattach completed. Probe the live mux
    // daemons for an `orgasmic-<run_id>-*` session and attach directly rather
    // than dead-ending on "no live run".
    let target = match target {
        Some(target) => Some(target),
        None if force_mock => None,
        None => find_orphan_mux_session(&run_id).await,
    };

    let shutdown = state.shutdown.clone();
    let supervisor = state.supervisor.clone();
    ws.on_upgrade(move |socket| async move {
        match (target, force_mock) {
            (Some((kind, bin, session)), _) => {
                serve_mux_session_socket(
                    socket,
                    kind,
                    bin,
                    session,
                    run_id,
                    allow_input,
                    supervisor,
                    shutdown,
                )
                .await
            }
            (None, true) => serve_mock_tmux_socket(socket, run_id, allow_input, shutdown).await,
            // No live run record AND no surviving mux session: say so honestly
            // instead of serving a mock terminal that looks like a real attach.
            // Flagged `recoverable: false` so the UI distinguishes a truly gone
            // run from a transient reconnect window.
            (None, false) => {
                let mut socket = socket;
                let _ = socket
                    .send(Message::Text(
                        json!({
                            "type": "error",
                            "recoverable": false,
                            "message": format!("no live run {run_id}")
                        })
                        .to_string(),
                    ))
                    .await;
            }
        }
    })
}

async fn serve_socket(
    socket: WebSocket,
    bus: crate::events::EventBus,
    topics: HashSet<Topic>,
    identity: Identity,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    let (mut sender, mut receiver) = socket.split();
    let mut rx = bus.subscribe();
    let mut send_task = tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(event) => {
                    if !topics.is_empty() && !topics.contains(&event.topic) {
                        continue;
                    }
                    // Authorization (dec_KF2MR): admin sees everything; a member
                    // only receives events whose topic + project their grants
                    // permit. Skip anything they may not see.
                    if !authz::event_visible(&identity, event.topic, &event.payload) {
                        continue;
                    }
                    let payload = match serde_json::to_string(&event) {
                        Ok(s) => s,
                        Err(_) => continue,
                    };
                    if sender.send(Message::Text(payload)).await.is_err() {
                        break;
                    }
                }
                Err(RecvError::Lagged(skipped)) => {
                    debug!(skipped, "ws subscriber lagged");
                    continue;
                }
                Err(RecvError::Closed) => break,
            }
        }
    });
    let mut recv_task = tokio::spawn(async move {
        while let Some(msg) = receiver.next().await {
            match msg {
                Ok(Message::Close(_)) => break,
                Ok(Message::Ping(_))
                | Ok(Message::Pong(_))
                | Ok(Message::Text(_))
                | Ok(Message::Binary(_)) => {}
                Err(_) => break,
            }
        }
    });
    tokio::select! {
        _ = (&mut send_task) => recv_task.abort(),
        _ = (&mut recv_task) => send_task.abort(),
        // Graceful shutdown: drop the connection task instead of waiting for a
        // client disconnect that may never come — otherwise axum's drain phase
        // deadlocks behind still-connected subscribers. `changed()` erring
        // means the sender dropped, which also only happens at shutdown.
        _ = shutdown.changed() => {
            send_task.abort();
            recv_task.abort();
        }
    };
}

/// `/ws/tmux/:run_id` client-to-server frames.
///
/// Binary frames are raw keystroke bytes written straight to the attach PTY.
/// Text frames are JSON control messages:
/// - `send_keys` pastes composer text into the pane and presses Enter (the
///   ManagerComposer seam — independent of the PTY keyboard path).
/// - `resize` reshapes the PTY via TIOCSWINSZ so tmux repaints at the xterm's
///   real dimensions.
/// - `detach` requests a graceful `Ctrl-b d` client disconnect.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum TmuxClientFrame {
    SendKeys { text: String },
    Resize { cols: u16, rows: u16 },
    Detach,
}

async fn serve_mock_tmux_socket(
    mut socket: WebSocket,
    run_id: String,
    allow_input: bool,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    let mut seq: u64 = 0;
    let mut ticks = interval(Duration::from_secs(1));
    let intro = format!("[mock tmux] attached to {run_id}\r\n");
    if send_pane_frame(&mut socket, TmuxPaneFrame::Full(intro))
        .await
        .is_err()
    {
        return;
    }

    loop {
        tokio::select! {
            _ = shutdown.changed() => break,
            _ = ticks.tick() => {
                seq += 1;
                let frame = format!("[mock tmux] tick {seq}\r\n");
                if send_pane_frame(&mut socket, TmuxPaneFrame::Delta(frame)).await.is_err() {
                    break;
                }
            }
            msg = socket.next() => {
                let Some(msg) = msg else { break };
                let Ok(msg) = msg else { break };
                match msg {
                    // Read-only viewers (any non-admin) have input dropped; only
                    // admin composer sends are echoed.
                    Message::Text(text) if allow_input => {
                        if let Ok(TmuxClientFrame::SendKeys { text }) = serde_json::from_str::<TmuxClientFrame>(&text) {
                            let echo = format!("[mock tmux] send_keys: {text}\r\n");
                            if send_pane_frame(&mut socket, TmuxPaneFrame::Delta(echo)).await.is_err() {
                                break;
                            }
                        }
                    }
                    Message::Close(_) => break,
                    Message::Text(_) | Message::Ping(_) | Message::Pong(_) | Message::Binary(_) => {}
                }
            }
        }
    }
}

/// Which multiplexer backs a run's live terminal. tmux is the default; rmux
/// (TASK-104) drives runs whose driver transport is `rmux`. Both attach through
/// the same PTY bridge — only the binary and the attach/refresh verbs differ.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum MuxKind {
    Tmux,
    Rmux,
}

impl MuxKind {
    fn from_transport(transport: &str) -> Self {
        if transport == "rmux" {
            MuxKind::Rmux
        } else {
            MuxKind::Tmux
        }
    }

    /// `<bin> <args…>` that joins the detached session interactively. rmux
    /// spells the attach subcommand `attach-session`; tmux accepts `attach`.
    fn attach_args(self, session: &str) -> [&str; 3] {
        match self {
            MuxKind::Tmux => ["attach", "-t", session],
            MuxKind::Rmux => ["attach-session", "-t", session],
        }
    }
}

/// Initial PTY size before the browser reports its real dimensions via a
/// `resize` frame. Matches the tmux driver's new-session geometry so the
/// wrapped CLI's first paint is stable.
const TMUX_PTY_INIT_ROWS: u16 = 50;
const TMUX_PTY_INIT_COLS: u16 = 200;

/// Coalesce PTY reads: after the first chunk, drain the channel for up to one
/// display frame and send a single WS binary frame. Reduces per-frame overhead
/// under burst output (lifted from HAR's attach bridge).
const COALESCE_BUDGET: Duration = Duration::from_millis(16);
const COALESCE_MAX_BYTES: usize = 64 * 1024;

/// Probe the live mux daemons for a session named `orgasmic-<run_id>-*` when
/// the supervisor holds no record for `run_id`. Returns the kind/bin/session to
/// attach to, or `None` if no surviving session matches. rmux is probed first
/// (the system-wide manager case); tmux is the fallback.
async fn find_orphan_mux_session(run_id: &str) -> Option<(MuxKind, String, String)> {
    // Session-name shapes per driver (runtime_id suffix unknown here):
    // rmux:  `orgasmic-rmux-<run_id>-<runtime_id>` (rmux_session_name)
    // tmux:  `orgasmic-<run_id>-<runtime_id>`      (tmux_session_name)
    let rmux_prefix = format!("orgasmic-rmux-{run_id}-");
    let tmux_prefix = format!("orgasmic-{run_id}-");

    // rmux: enumerate via the SDK (connect, never start). A missing daemon or
    // empty list simply yields no match.
    if let Some(name) =
        orgasmic_drivers::modes::rmux::find_live_session_with_prefix(&rmux_prefix).await
    {
        let bin = orgasmic_drivers::probe_rmux_binary()
            .path
            .unwrap_or_else(|| "rmux".to_string());
        return Some((MuxKind::Rmux, bin, name));
    }

    // tmux: list-sessions by name; an absent server exits non-zero → no match.
    if let Ok(output) = tokio::process::Command::new("tmux")
        .args(["list-sessions", "-F", "#{session_name}"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .await
    {
        if output.status.success() {
            if let Some(name) = String::from_utf8_lossy(&output.stdout)
                .lines()
                .map(str::trim)
                .find(|name| name.starts_with(&tmux_prefix))
            {
                return Some((MuxKind::Tmux, "tmux".to_string(), name.to_string()));
            }
        }
    }

    None
}

/// Proxy an interactive `<mux> attach … -t <session>` over a PTY so the browser
/// xterm renders the live byte stream — typing, arrow keys, ctrl-combos, and
/// resizes all flow through, instead of the old 1s `capture-pane` polling. The
/// same machinery serves both tmux and rmux (TASK-104); `kind`/`bin` select the
/// multiplexer.
#[allow(clippy::too_many_arguments)]
async fn serve_mux_session_socket(
    socket: WebSocket,
    kind: MuxKind,
    bin: String,
    session: String,
    run_id: String,
    allow_input: bool,
    supervisor: crate::supervisor::Supervisor,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    use portable_pty::{native_pty_system, CommandBuilder, PtySize};

    let (mut sender, mut receiver) = socket.split();

    async fn fail(
        sender: &mut futures::stream::SplitSink<WebSocket, Message>,
        context: &str,
        err: impl std::fmt::Display,
    ) {
        let _ = sender
            .send(Message::Text(
                json!({"type": "error", "message": format!("{context}: {err}")}).to_string(),
            ))
            .await;
    }

    let pty_system = native_pty_system();
    let pair = match pty_system.openpty(PtySize {
        rows: TMUX_PTY_INIT_ROWS,
        cols: TMUX_PTY_INIT_COLS,
        pixel_width: 0,
        pixel_height: 0,
    }) {
        Ok(pair) => pair,
        Err(err) => return fail(&mut sender, "openpty", err).await,
    };

    let mut cmd = CommandBuilder::new(&bin);
    cmd.args(kind.attach_args(&session));
    // portable-pty starts children with an empty environment. tmux needs at
    // minimum TERM (to load a terminfo entry capable of `clear`/`cup`/etc.),
    // plus HOME / PATH / USER so the per-user tmux server socket is reachable.
    // Forward the daemon's env wholesale, then pin TERM and a UTF-8 locale —
    // in the C locale tmux's client-side wcwidth replaces every non-ASCII
    // byte with `_` regardless of what the pane actually holds.
    for (key, value) in std::env::vars() {
        cmd.env(key, value);
    }
    cmd.env("TERM", "xterm-256color");
    cmd.env("LC_ALL", "en_US.UTF-8");
    cmd.env("LANG", "en_US.UTF-8");

    let mut child = match pair.slave.spawn_command(cmd) {
        Ok(child) => child,
        Err(err) => return fail(&mut sender, "tmux attach", err).await,
    };
    // The child holds the slave end; drop ours so EOF propagates on exit.
    drop(pair.slave);

    let mut master_reader = match pair.master.try_clone_reader() {
        Ok(reader) => reader,
        Err(err) => {
            let _ = child.kill();
            return fail(&mut sender, "clone pty reader", err).await;
        }
    };
    let mut master_writer = match pair.master.take_writer() {
        Ok(writer) => writer,
        Err(err) => {
            let _ = child.kill();
            return fail(&mut sender, "take pty writer", err).await;
        }
    };
    let master_for_resize = std::sync::Arc::new(std::sync::Mutex::new(pair.master));

    // PTY→WS: a blocking thread reads raw bytes; an mpsc hands them to the
    // async sender. At most one of these per attached browser.
    let (pty_to_ws_tx, mut pty_to_ws_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
    let reader_handle = tokio::task::spawn_blocking(move || {
        use std::io::Read;
        let mut buf = [0u8; 8192];
        loop {
            match master_reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if pty_to_ws_tx.blocking_send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    // WS→PTY: a dedicated thread drains keystroke bytes so fast typing doesn't
    // burn one spawn_blocking per frame. An empty Vec is the shutdown sentinel.
    let (input_tx, input_rx) = std::sync::mpsc::channel::<Vec<u8>>();
    let writer_handle = std::thread::spawn(move || {
        use std::io::Write;
        while let Ok(bytes) = input_rx.recv() {
            if bytes.is_empty() {
                break;
            }
            if master_writer.write_all(&bytes).is_err() {
                break;
            }
            let _ = master_writer.flush();
        }
    });

    loop {
        tokio::select! {
            // Graceful daemon shutdown: detach this client so the axum drain
            // phase isn't held open by a browser that never disconnects.
            _ = shutdown.changed() => break,
            Some(first) = pty_to_ws_rx.recv() => {
                let mut batch = first;
                let deadline = tokio::time::Instant::now() + COALESCE_BUDGET;
                while batch.len() < COALESCE_MAX_BYTES {
                    match tokio::time::timeout_at(deadline, pty_to_ws_rx.recv()).await {
                        Ok(Some(more)) => batch.extend_from_slice(&more),
                        _ => break,
                    }
                }
                if sender.send(Message::Binary(batch)).await.is_err() {
                    break;
                }
            }
            msg = receiver.next() => {
                let Some(Ok(msg)) = msg else { break };
                match msg {
                    // Raw keystroke bytes: written to the PTY only for admin.
                    // A read-only viewer's keystrokes are silently dropped
                    // (sessions.interact is admin-only, dec_KF2MR).
                    Message::Binary(data) => {
                        if allow_input && input_tx.send(data).is_err() {
                            break;
                        }
                    }
                    Message::Text(text) => match serde_json::from_str::<TmuxClientFrame>(&text) {
                        // Composer send: admin-only. Dropped for read-only viewers.
                        Ok(TmuxClientFrame::SendKeys { text }) if allow_input => {
                            if let Err(err) = paste_text_and_enter(&bin, &session, &text).await {
                                fail(&mut sender, "send_keys", err).await;
                                break;
                            }
                            // Shared recording path (TASK-102 / dec_052): the
                            // tmux composer send is the dominant manager case,
                            // so it must land a durable composer_send lifecycle
                            // event just like the /runs/:id/input chat path.
                            // Best-effort — recording must not drop the attach.
                            if let Err(err) = supervisor.record_composer_send(&run_id, &text).await {
                                debug!(error = %err, run_id = %run_id, "tmux composer_send recording failed");
                            }
                        }
                        // Resize mutates the shared tmux geometry (a session
                        // sizes to its smallest attached client), so a read-only
                        // viewer could shrink the admin's pane. Admin-only, same
                        // as the other input frames (sessions.interact, dec_KF2MR).
                        Ok(TmuxClientFrame::Resize { cols, rows }) if allow_input => {
                            if let Ok(master) = master_for_resize.lock() {
                                let _ = master.resize(PtySize {
                                    rows,
                                    cols,
                                    pixel_width: 0,
                                    pixel_height: 0,
                                });
                            }
                            // master.resize() raises SIGWINCH on the attached
                            // client, but the multiplexer's automatic post-grow
                            // redraw is differential and leaves stale cells on
                            // screen when a client jumps in size (the "maximize
                            // the window" corruption). Force a full repaint once
                            // the new geometry has propagated to the server.
                            let session_refresh = session.clone();
                            let bin_refresh = bin.clone();
                            tokio::spawn(async move {
                                tokio::time::sleep(Duration::from_millis(80)).await;
                                refresh_session_clients(&bin_refresh, &session_refresh).await;
                            });
                        }
                        Ok(TmuxClientFrame::Detach) => {
                            // Graceful detach: Ctrl-b d so tmux sees a clean
                            // client disconnect; the session stays alive.
                            // A detach is a client-side disconnect, honored for
                            // anyone (including read-only viewers).
                            let _ = input_tx.send(vec![0x02, b'd']);
                            break;
                        }
                        // A send_keys or resize from a read-only viewer (guard
                        // above failed) lands here and is dropped, as does any
                        // unknown control frame.
                        Ok(_) | Err(_) => { /* read-only or unknown frame — ignore. */ }
                    },
                    Message::Close(_) => break,
                    Message::Ping(_) | Message::Pong(_) => {}
                }
            }
            else => break,
        }
    }

    // Tear down: stop the writer thread, kill the tmux client (the session
    // itself stays alive), and best-effort cancel the reader.
    let _ = input_tx.send(Vec::new());
    let _ = child.kill();
    let _ = child.wait();
    reader_handle.abort();
    drop(writer_handle); // Detached; the thread exits when its channel closes.
}

/// Mock-only server-to-client pane frames (`ORGASMIC_TMUX_WS_MOCK=1`). The
/// real bridge streams raw PTY bytes as WS binary frames; the mock keeps a
/// tiny JSON text protocol so the UI can be developed without tmux:
/// - `{"type":"pane_delta","text":"..."}` appends to the xterm buffer.
/// - `{"type":"pane_full","text":"..."}` replaces the xterm screen.
#[derive(Debug, Clone, PartialEq, Eq)]
enum TmuxPaneFrame {
    Delta(String),
    Full(String),
}

#[derive(Debug, Serialize, PartialEq, Eq)]
struct TmuxPaneFrameWire {
    #[serde(rename = "type")]
    ty: &'static str,
    text: String,
}

async fn send_pane_frame(socket: &mut WebSocket, frame: TmuxPaneFrame) -> Result<(), axum::Error> {
    socket.send(Message::Text(pane_frame_json(frame))).await
}

fn pane_frame_json(frame: TmuxPaneFrame) -> String {
    let wire = match frame {
        TmuxPaneFrame::Delta(text) => TmuxPaneFrameWire {
            ty: "pane_delta",
            text,
        },
        TmuxPaneFrame::Full(text) => TmuxPaneFrameWire {
            ty: "pane_full",
            text,
        },
    };
    serde_json::to_string(&wire).expect("tmux pane frame serializes")
}

/// Paste `text` into the session's active pane and press Enter, via the
/// multiplexer's buffer commands. tmux and rmux share this verb set
/// (`set-buffer`/`paste-buffer`/`delete-buffer`/`send-keys`), so `bin` selects
/// which one drives the composer send.
async fn paste_text_and_enter(bin: &str, session: &str, text: &str) -> anyhow::Result<()> {
    let buffer = format!("orgasmic-ws-{}", uuid::Uuid::new_v4().simple());
    run_mux(bin, ["set-buffer", "-b", &buffer, "--", text]).await?;
    let paste = run_mux(bin, ["paste-buffer", "-p", "-b", &buffer, "-t", session]).await;
    let _ = run_mux(bin, ["delete-buffer", "-b", &buffer]).await;
    paste?;
    run_mux(bin, ["send-keys", "-t", session, "Enter"]).await
}

/// Force the multiplexer to fully repaint every client attached to `session`.
/// The automatic post-SIGWINCH redraw after a client grows can leave stale
/// cells on screen; an explicit `refresh-client` per client redraws the whole
/// screen and clears the garbage. Best-effort: a session with no live clients
/// (or a mux that rejects the target) is a no-op, never a hard error on the
/// attach path. Applies to both tmux and rmux (both ship these verbs).
async fn refresh_session_clients(bin: &str, session: &str) {
    let listed = tokio::process::Command::new(bin)
        .args(["list-clients", "-t", session, "-F", "#{client_tty}"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .await;
    let Ok(listed) = listed else { return };
    if !listed.status.success() {
        return;
    }
    for tty in String::from_utf8_lossy(&listed.stdout).lines() {
        let tty = tty.trim();
        if tty.is_empty() {
            continue;
        }
        let _ = run_mux(bin, ["refresh-client", "-t", tty]).await;
    }
}

async fn run_mux<const N: usize>(bin: &str, args: [&str; N]) -> anyhow::Result<()> {
    let output = tokio::process::Command::new(bin)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .await?;
    if output.status.success() {
        Ok(())
    } else {
        anyhow::bail!(
            "{bin} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn mux_kind_routes_rmux_transport_and_defaults_to_tmux() {
        assert_eq!(MuxKind::from_transport("rmux"), MuxKind::Rmux);
        // Every non-rmux transport (tmux, tmux-tui, acp, …) attaches via tmux.
        assert_eq!(MuxKind::from_transport("tmux"), MuxKind::Tmux);
        assert_eq!(MuxKind::from_transport("tmux-tui"), MuxKind::Tmux);
        assert_eq!(MuxKind::from_transport("acp-ws"), MuxKind::Tmux);
    }

    #[test]
    fn mux_attach_verb_differs_by_kind() {
        // rmux spells the attach subcommand `attach-session`; tmux uses `attach`.
        assert_eq!(
            MuxKind::Rmux.attach_args("s"),
            ["attach-session", "-t", "s"]
        );
        assert_eq!(MuxKind::Tmux.attach_args("s"), ["attach", "-t", "s"]);
    }

    #[test]
    fn mock_pane_frames_keep_their_wire_shape() {
        let wire: Value =
            serde_json::from_str(&pane_frame_json(TmuxPaneFrame::Delta("world\r\n".into())))
                .unwrap();
        assert_eq!(wire["type"], "pane_delta");
        assert_eq!(wire["text"], "world\r\n");

        let wire: Value =
            serde_json::from_str(&pane_frame_json(TmuxPaneFrame::Full("screen\r\n".into())))
                .unwrap();
        assert_eq!(wire["type"], "pane_full");
        assert_eq!(wire["text"], "screen\r\n");
    }

    #[test]
    fn client_frame_contract_parses_all_control_messages() {
        assert!(matches!(
            serde_json::from_str::<TmuxClientFrame>(r#"{"type":"send_keys","text":"hi"}"#),
            Ok(TmuxClientFrame::SendKeys { text }) if text == "hi"
        ));
        assert!(matches!(
            serde_json::from_str::<TmuxClientFrame>(r#"{"type":"resize","cols":120,"rows":40}"#),
            Ok(TmuxClientFrame::Resize {
                cols: 120,
                rows: 40
            })
        ));
        assert!(matches!(
            serde_json::from_str::<TmuxClientFrame>(r#"{"type":"detach"}"#),
            Ok(TmuxClientFrame::Detach)
        ));
    }

    fn member(grants: &[(&str, &str)]) -> Identity {
        Identity::Member {
            name: "alice".into(),
            grants: grants
                .iter()
                .map(|(p, r)| (p.to_string(), r.to_string()))
                .collect(),
        }
    }

    #[test]
    fn tmux_input_is_admin_only() {
        // sessions.interact is admin-only in v1: every member is read-only,
        // regardless of role (even an editor with SessionsWatch).
        assert!(tmux_input_allowed(&Identity::Admin));
        assert!(!tmux_input_allowed(&member(&[("proj-a", "editor")])));
        assert!(!tmux_input_allowed(&member(&[("proj-a", "viewer")])));
        assert!(!tmux_input_allowed(&member(&[("*", "artifacts")])));
    }

    #[test]
    fn tmux_stream_requires_sessions_watch_on_the_runs_project() {
        // Admin streams anything, including a run with no resolvable project.
        assert!(tmux_stream_allowed(&Identity::Admin, Some("proj-a")));
        assert!(tmux_stream_allowed(&Identity::Admin, None));

        // viewer/editor hold SessionsWatch on their granted project.
        assert!(tmux_stream_allowed(
            &member(&[("proj-a", "viewer")]),
            Some("proj-a")
        ));
        assert!(tmux_stream_allowed(
            &member(&[("proj-a", "editor")]),
            Some("proj-a")
        ));

        // A member is denied a project they hold no grant for.
        assert!(!tmux_stream_allowed(
            &member(&[("proj-a", "viewer")]),
            Some("proj-b")
        ));

        // An artifacts-role member lacks SessionsWatch entirely.
        assert!(!tmux_stream_allowed(
            &member(&[("proj-a", "artifacts")]),
            Some("proj-a")
        ));

        // A run with no resolvable project is unwatchable by any member (fail
        // closed — no coarse bypass).
        assert!(!tmux_stream_allowed(&member(&[("*", "viewer")]), None));
    }
}

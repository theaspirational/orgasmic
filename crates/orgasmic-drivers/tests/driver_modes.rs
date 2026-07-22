use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use async_trait::async_trait;
use futures::{SinkExt, StreamExt};
use orgasmic_core::{DriverEvent, RuntimeIdentity, TextStream, SUPPORTED_WORKER_DRIVER_HARNESSES};
use orgasmic_drivers::{
    driver_for, driver_for_mode_harness, AcpStdioDriver, AcpWsDriver, AcpWsProtocol, ClaudeAdapter,
    CodexAdapter, CodexAppserverDriver, CursorAdapter, DriverConfig, DriverContext, DriverError,
    HarnessControlOutcome, HarnessEventAdapter, HarnessRequest, HermesAdapter, RunKind, StdioSpawn,
    SubprocessStreamJsonDriver, TmuxDriver, WorkerDriver, HARNESSES, MODES, SUPPORTED,
};
use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;
use tokio::time::{timeout, Duration};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::{accept_async, tungstenite::Message};

#[derive(Clone)]
struct MockAdapter {
    request: HarnessRequest,
}

#[async_trait]
impl HarnessEventAdapter for MockAdapter {
    fn harness(&self) -> &'static str {
        "mock"
    }

    fn clone_box(&self) -> Box<dyn HarnessEventAdapter> {
        Box::new(self.clone())
    }

    async fn parse_event(&mut self, raw: Value) -> Vec<DriverEvent> {
        vec![DriverEvent::TextChunk {
            stream: TextStream::Assistant,
            chunk: raw
                .get("msg")
                .and_then(Value::as_str)
                .unwrap_or("missing")
                .to_string(),
            seq: 0,
        }]
    }

    fn compose_request(
        &mut self,
        _ctx: &DriverContext,
        _config: &DriverConfig,
    ) -> Result<HarnessRequest, DriverError> {
        Ok(self.request.clone())
    }

    fn stdio_spawn(&self) -> Option<StdioSpawn> {
        Some(StdioSpawn {
            command: "sh".into(),
            args: vec!["-c".into(), "printf '%s\\n' '{\"msg\":\"stdio\"}'".into()],
            cwd: None,
            env: Vec::new(),
        })
    }
}

fn ctx() -> DriverContext {
    DriverContext {
        identity: RuntimeIdentity::new("run-mode-test", "boot-test"),
        run_kind: RunKind::Worker,
        task_id: "TASK-MODE".into(),
        worker_id: "mock".into(),
        project_id: None,
        worktree: None,
        babysitter_target: None,
    }
}

fn ctx_with_worktree(worktree: PathBuf) -> DriverContext {
    let mut ctx = ctx();
    ctx.worktree = Some(worktree);
    ctx
}

fn subprocess_request(message: &str) -> HarnessRequest {
    HarnessRequest::Subprocess {
        binary: "sh".into(),
        args: vec![
            "-c".into(),
            format!("printf '%s\\n' '{{\"msg\":\"{message}\"}}'"),
        ],
        env: BTreeMap::new(),
        cwd: None,
        stdin_payload: None,
        close_stdin: true,
    }
}

fn simulated_request() -> HarnessRequest {
    HarnessRequest::Simulated { events: Vec::new() }
}

fn force_inert_tmux_config() -> DriverConfig {
    DriverConfig::from_value(json!({"force_inert": true}))
}

#[test]
fn registry_matrix_matches_supported_const() {
    for &(mode, harness) in SUPPORTED {
        assert!(MODES.contains(&mode), "unknown supported mode {mode}");
        assert!(
            HARNESSES.contains(&harness),
            "unknown supported harness {harness}"
        );
        let driver = driver_for_mode_harness(mode, harness).expect("supported pair");
        assert_eq!(driver.transport(), mode);
        assert_eq!(driver.harness(), Some(harness));
    }

    for &mode in MODES {
        for &harness in HARNESSES {
            let supported = SUPPORTED.contains(&(mode, harness));
            assert_eq!(
                driver_for_mode_harness(mode, harness).is_some(),
                supported,
                "mode={mode} harness={harness}"
            );
        }
    }
    assert!(driver_for_mode_harness("acp-ws", "cursor-agent").is_none());
}

#[test]
fn core_and_drivers_supported_matrices_agree() {
    // Every registry pair is also a dispatchable worker pair — including
    // `custom`, whose worker templates wrap an arbitrary CLI via
    // `:HARNESS_ARGS:` (core enforces that property at parse time).
    assert_eq!(SUPPORTED_WORKER_DRIVER_HARNESSES, SUPPORTED);
}

#[test]
fn supported_matrix_includes_acp_stdio_codex() {
    assert!(SUPPORTED.contains(&("acp-stdio", "codex")));
    assert!(SUPPORTED_WORKER_DRIVER_HARNESSES.contains(&("acp-stdio", "codex")));
}

#[test]
fn acp_stdio_codex_driver_resolves() {
    assert!(driver_for_mode_harness("acp-stdio", "codex").is_some());
}

#[test]
fn acp_stdio_codex_adapter_provides_stdio_spawn_config() {
    let adapter = CodexAdapter::new();
    let spawn = adapter.stdio_spawn().expect("codex stdio_spawn");
    assert_eq!(spawn.command, "codex");
    assert_eq!(spawn.args, vec!["app-server".to_string()]);
    assert!(spawn.env.is_empty());
    assert!(
        spawn
            .cwd
            .as_ref()
            .map(|path| path.is_absolute())
            .unwrap_or(false),
        "codex stdio cwd must be absolute when set"
    );
}

#[test]
fn claude_adapter_stdio_spawn_preserves_current_behavior() {
    let adapter = ClaudeAdapter::new();
    let spawn = adapter.stdio_spawn().expect("claude stdio_spawn");
    assert_eq!(spawn.command, "claude");
    assert_eq!(
        spawn.args,
        vec![
            "--bare".to_string(),
            "-p".to_string(),
            "--input-format".to_string(),
            "stream-json".to_string(),
            "--output-format".to_string(),
            "stream-json".to_string(),
            "--include-partial-messages".to_string(),
            "--verbose".to_string(),
            "--no-session-persistence".to_string(),
        ]
    );
    assert!(spawn.cwd.is_none());
    assert!(spawn.env.is_empty());
}

#[tokio::test]
async fn subprocess_stream_json_routes_stdout_through_adapter() {
    let driver = SubprocessStreamJsonDriver::new(Box::new(MockAdapter {
        request: subprocess_request("subprocess"),
    }));
    let mut session = driver.acquire(ctx(), DriverConfig::empty()).await.unwrap();
    let event = session.events.recv().await.unwrap();
    assert!(matches!(
        event,
        DriverEvent::TextChunk { chunk, .. } if chunk == "subprocess"
    ));
}

/// Drop-guard that reaps a process *group* (`kill -- -<pgid>`) on every test
/// exit path, including assert-failure/panic unwinds. Mirrors the cursor-agent
/// smoke's `ProcessGroupGuard` (TASK-104.2) so a regression-test failure can
/// never orphan the spawned descendants we are deliberately leaking to. Null
/// stdio so the `kill` helper holds no piped test-runner fd; a no-op once the
/// group is already gone.
#[cfg(unix)]
struct ProcessGroupGuard(Option<u32>);

#[cfg(unix)]
impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        if let Some(pid) = self.0 {
            let _ = std::process::Command::new("kill")
                .arg("--")
                .arg(format!("-{pid}"))
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        }
    }
}

/// A subprocess request whose command forks a *surviving* backgrounded child
/// carrying `marker` in its argv, then itself blocks. The driver spawns this
/// under `setsid`, so the whole tree shares one process group: releasing the
/// session must reap the group, not just the direct child. `marker` is matchable
/// by `pgrep -f` against the backgrounded `sh`'s command line.
#[cfg(unix)]
fn forking_subprocess_request(marker: &str) -> HarnessRequest {
    HarnessRequest::Subprocess {
        binary: "sh".into(),
        args: vec![
            "-c".into(),
            format!(
                // Background a long-lived child tagged with the marker, announce
                // readiness on stdout (so the driver loop is live), then block so
                // the direct child only dies when the group is reaped.
                "sh -c ': {marker}; sleep 3600' & printf '%s\\n' '{{\"msg\":\"up\"}}'; sleep 3600"
            ),
        ],
        env: BTreeMap::new(),
        cwd: None,
        stdin_payload: None,
        close_stdin: true,
    }
}

#[cfg(unix)]
fn marker_processes_alive(marker: &str) -> bool {
    std::process::Command::new("pgrep")
        .arg("-f")
        .arg(marker)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// Regression for TASK-104.3: releasing a subprocess-stream-json session must
/// reap the whole `setsid` process group, not just the direct child. The mock
/// command backgrounds a surviving child; after release, no process carrying the
/// unique marker may remain.
#[cfg(unix)]
#[tokio::test]
async fn subprocess_release_reaps_whole_process_group() {
    let marker = format!("orgasmic-reap-104-3-{}", std::process::id());
    let driver = SubprocessStreamJsonDriver::new(Box::new(MockAdapter {
        request: forking_subprocess_request(&marker),
    }));
    let mut session = driver.acquire(ctx(), DriverConfig::empty()).await.unwrap();
    // Defensive: reap the group on any failure path before asserting.
    let _reap = ProcessGroupGuard(session.pid);

    // Wait for the ready line so the descendant has certainly forked.
    let event = timeout(Duration::from_secs(10), session.events.recv())
        .await
        .expect("subprocess did not emit readiness")
        .expect("event stream closed before readiness");
    assert!(matches!(event, DriverEvent::TextChunk { chunk, .. } if chunk == "up"));
    assert!(
        marker_processes_alive(&marker),
        "expected the forked descendant to be alive before release"
    );

    session
        .control
        .release("test cleanup")
        .await
        .expect("release");

    // Drain until the stream closes so the driver task has run its exit/reap path.
    loop {
        match timeout(Duration::from_secs(10), session.events.recv()).await {
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(_) => panic!("subprocess stream did not close after release"),
        }
    }

    // Poll briefly: the group KILL + child wait are async; allow the OS a moment
    // to tear the descendants down.
    let mut gone = false;
    for _ in 0..40 {
        if !marker_processes_alive(&marker) {
            gone = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        gone,
        "process group descendant survived release (marker {marker} still present)"
    );
}

fn simulated_ready_request() -> HarnessRequest {
    HarnessRequest::Simulated {
        events: vec![DriverEvent::Ready {
            protocol_version: "mock-simulated/1".into(),
            capabilities: json!({}),
        }],
    }
}

#[tokio::test]
async fn acp_stdio_keeps_simulated_when_adapter_does_not_upgrade() {
    let driver = AcpStdioDriver::new(Box::new(MockAdapter {
        request: simulated_ready_request(),
    }));
    let mut session = driver.acquire(ctx(), DriverConfig::empty()).await.unwrap();
    let event = session.events.recv().await.unwrap();
    assert!(matches!(
        event,
        DriverEvent::Ready { protocol_version, .. } if protocol_version == "mock-simulated/1"
    ));
}

#[tokio::test]
async fn acp_stdio_codex_jsonrpc_handshake_mock_peer() {
    let stdin_log = std::env::temp_dir().join(format!(
        "orgasmic-acp-stdio-stdin-{}.log",
        std::process::id()
    ));
    let _ = fs::remove_file(&stdin_log);

    const MOCK_PEER: &str = r#"
while IFS= read -r line; do
  [ -z "$line" ] && continue
  method=$(printf '%s' "$line" | sed -n 's/.*"method"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p')
  id=$(printf '%s' "$line" | sed -n 's/.*"id"[[:space:]]*:[[:space:]]*\([0-9][0-9]*\).*/\1/p')
  if [ -n "$ORGASMIC_STDIN_LOG" ] && [ -n "$method" ]; then
    printf '%s\n' "$method" >> "$ORGASMIC_STDIN_LOG"
  fi
  case "$method" in
    initialize)
      printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"userAgent\":\"fixture\"}}"
      ;;
    thread/start)
      printf '%s\n' "{\"jsonrpc\":\"2.0\",\"method\":\"warning\",\"params\":{\"message\":\"thread start still running\"}}"
      printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"thread\":{\"id\":\"thread-stdio-fixture\"}}}"
      ;;
    turn/start)
      printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"turn\":{\"id\":\"turn-stdio-fixture\",\"status\":\"inProgress\",\"items\":[]}}}"
      printf '%s\n' "{\"jsonrpc\":\"2.0\",\"method\":\"item/agentMessage/delta\",\"params\":{\"delta\":\"stdio handshake\"}}"
      printf '%s\n' "{\"jsonrpc\":\"2.0\",\"method\":\"rawResponseItem/completed\",\"params\":{\"item\":{\"type\":\"function_call\",\"name\":\"shell\",\"arguments\":\"{\\\"cmd\\\":\\\"echo hi\\\"}\",\"call_id\":\"call-stdio-tool\"}}}"
      ;;
    turn/interrupt)
      printf '%s\n' "{\"jsonrpc\":\"2.0\",\"method\":\"turn/completed\",\"params\":{\"threadId\":\"thread-stdio-fixture\",\"turn\":{\"id\":\"turn-stdio-fixture\",\"status\":\"completed\",\"items\":[]}}}"
      ;;
  esac
done
"#;

    struct CodexStdioMockHarness {
        inner: CodexAdapter,
        stdin_log_path: PathBuf,
    }

    #[async_trait]
    impl HarnessEventAdapter for CodexStdioMockHarness {
        fn harness(&self) -> &'static str {
            "codex"
        }

        fn clone_box(&self) -> Box<dyn HarnessEventAdapter> {
            Box::new(CodexStdioMockHarness {
                inner: CodexAdapter::new(),
                stdin_log_path: self.stdin_log_path.clone(),
            })
        }

        fn stdio_spawn(&self) -> Option<StdioSpawn> {
            Some(StdioSpawn {
                command: "sh".into(),
                args: vec!["-c".into(), MOCK_PEER.into()],
                cwd: None,
                env: vec![(
                    "ORGASMIC_STDIN_LOG".into(),
                    self.stdin_log_path.to_string_lossy().into_owned(),
                )],
            })
        }

        fn upgrades_simulated_to_subprocess(&self) -> bool {
            true
        }

        fn stdio_session_init(
            &mut self,
            ctx: &DriverContext,
            config: &DriverConfig,
        ) -> Result<Value, DriverError> {
            self.inner.stdio_session_init(ctx, config)
        }

        fn compose_request(
            &mut self,
            ctx: &DriverContext,
            config: &DriverConfig,
        ) -> Result<HarnessRequest, DriverError> {
            self.inner.compose_request(ctx, config)
        }

        async fn parse_event(&mut self, raw: Value) -> Vec<DriverEvent> {
            self.inner.parse_event(raw).await
        }

        async fn on_ws_thread_started(
            &mut self,
            endpoint: &str,
            thread_response: &Value,
        ) -> Result<Vec<DriverEvent>, DriverError> {
            self.inner
                .on_ws_thread_started(endpoint, thread_response)
                .await
        }

        fn ws_turn_start_params(&mut self) -> Result<Value, DriverError> {
            self.inner.ws_turn_start_params()
        }

        async fn on_ws_response(
            &mut self,
            method: &str,
            response: Value,
        ) -> Result<Vec<DriverEvent>, DriverError> {
            self.inner.on_ws_response(method, response).await
        }

        fn terminal_emitted(&self) -> bool {
            self.inner.terminal_emitted()
        }

        async fn release(&mut self, reason: String) -> Result<HarnessControlOutcome, DriverError> {
            self.inner.release(reason).await
        }
    }

    let driver = AcpStdioDriver::new(Box::new(CodexStdioMockHarness {
        inner: CodexAdapter::new(),
        stdin_log_path: stdin_log.clone(),
    }));
    let cfg = DriverConfig::from_value(json!({
        "model": "gpt-fixture",
        "reasoning_effort": "low",
    }));
    let mut session = driver.acquire(ctx(), cfg).await.unwrap();

    let intervening = timeout(Duration::from_secs(2), session.events.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        intervening,
        DriverEvent::TextChunk {
            stream: TextStream::System,
            chunk,
            ..
        } if chunk == "thread start still running"
    ));

    let ready = timeout(Duration::from_secs(2), session.events.recv())
        .await
        .unwrap()
        .unwrap();
    let DriverEvent::Ready { capabilities, .. } = ready else {
        panic!("expected Ready, got {ready:?}");
    };
    assert_eq!(capabilities["simulated"], false);
    assert_eq!(capabilities["thread_id"], "thread-stdio-fixture");
    assert!(capabilities["endpoint"]
        .as_str()
        .unwrap()
        .starts_with("stdio:"));

    let text = timeout(Duration::from_secs(2), session.events.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        text,
        DriverEvent::TextChunk {
            stream: TextStream::Assistant,
            chunk,
            ..
        } if chunk == "stdio handshake"
    ));

    let tool = timeout(Duration::from_secs(2), session.events.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        tool,
        DriverEvent::ToolCall {
            call_id,
            name,
            args,
            ..
        } if call_id == "call-stdio-tool" && name == "shell" && args["cmd"] == "echo hi"
    ));

    session.control.release("test done").await.unwrap();

    timeout(Duration::from_secs(2), async {
        loop {
            if fs::read_to_string(&stdin_log)
                .map(|content| content.contains("turn/interrupt"))
                .unwrap_or(false)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("turn/interrupt not observed on mock peer stdin");

    let complete = timeout(Duration::from_secs(2), session.events.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(complete, DriverEvent::RunComplete { .. }));

    if let Ok(Some(event)) = timeout(Duration::from_millis(300), session.events.recv()).await {
        panic!("unexpected event after terminal completion: {event:?}");
    }
}

/// TASK-100.3: while a codex turn is in flight but the harness produces no
/// output (it is buffering a long subprocess), the acp-stdio loop must emit
/// `Heartbeat` driver events on a cadence so the supervisor's stall detector
/// stays reset. Here the mock peer completes the handshake, starts a turn, then
/// goes silent (reading stdin, emitting nothing) until it sees `turn/interrupt`.
#[tokio::test]
async fn acp_stdio_codex_emits_heartbeats_during_quiet_turn() {
    // Mock peer: answer the handshake, start a turn, then emit nothing until
    // interrupted. The trailing `read` loop keeps stdin (and the process) alive
    // so the acp-stdio loop sits quiet — the only events it can produce in this
    // window are heartbeats.
    const QUIET_PEER: &str = r#"
while IFS= read -r line; do
  [ -z "$line" ] && continue
  method=$(printf '%s' "$line" | sed -n 's/.*"method"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p')
  id=$(printf '%s' "$line" | sed -n 's/.*"id"[[:space:]]*:[[:space:]]*\([0-9][0-9]*\).*/\1/p')
  case "$method" in
    initialize)
      printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"userAgent\":\"fixture\"}}"
      ;;
    thread/start)
      printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"thread\":{\"id\":\"thread-stdio-fixture\"}}}"
      ;;
    turn/start)
      printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"turn\":{\"id\":\"turn-stdio-fixture\",\"status\":\"inProgress\",\"items\":[]}}}"
      ;;
    turn/interrupt)
      printf '%s\n' "{\"jsonrpc\":\"2.0\",\"method\":\"turn/completed\",\"params\":{\"threadId\":\"thread-stdio-fixture\",\"turn\":{\"id\":\"turn-stdio-fixture\",\"status\":\"completed\",\"items\":[]}}}"
      ;;
  esac
done
"#;

    struct QuietCodexHarness {
        inner: CodexAdapter,
    }

    #[async_trait]
    impl HarnessEventAdapter for QuietCodexHarness {
        fn harness(&self) -> &'static str {
            "codex"
        }
        fn clone_box(&self) -> Box<dyn HarnessEventAdapter> {
            Box::new(QuietCodexHarness {
                inner: CodexAdapter::new(),
            })
        }
        fn stdio_spawn(&self) -> Option<StdioSpawn> {
            Some(StdioSpawn {
                command: "sh".into(),
                args: vec!["-c".into(), QUIET_PEER.into()],
                cwd: None,
                env: Vec::new(),
            })
        }
        fn upgrades_simulated_to_subprocess(&self) -> bool {
            true
        }
        fn stdio_session_init(
            &mut self,
            ctx: &DriverContext,
            config: &DriverConfig,
        ) -> Result<Value, DriverError> {
            self.inner.stdio_session_init(ctx, config)
        }
        fn compose_request(
            &mut self,
            ctx: &DriverContext,
            config: &DriverConfig,
        ) -> Result<HarnessRequest, DriverError> {
            self.inner.compose_request(ctx, config)
        }
        async fn parse_event(&mut self, raw: Value) -> Vec<DriverEvent> {
            self.inner.parse_event(raw).await
        }
        async fn on_ws_thread_started(
            &mut self,
            endpoint: &str,
            thread_response: &Value,
        ) -> Result<Vec<DriverEvent>, DriverError> {
            self.inner
                .on_ws_thread_started(endpoint, thread_response)
                .await
        }
        fn ws_turn_start_params(&mut self) -> Result<Value, DriverError> {
            self.inner.ws_turn_start_params()
        }
        async fn on_ws_response(
            &mut self,
            method: &str,
            response: Value,
        ) -> Result<Vec<DriverEvent>, DriverError> {
            self.inner.on_ws_response(method, response).await
        }
        fn terminal_emitted(&self) -> bool {
            self.inner.terminal_emitted()
        }
        async fn release(&mut self, reason: String) -> Result<HarnessControlOutcome, DriverError> {
            self.inner.release(reason).await
        }
    }

    let driver = AcpStdioDriver::new(Box::new(QuietCodexHarness {
        inner: CodexAdapter::new(),
    }));
    // 100ms cadence == the production floor (TASK-100.5); keeps the test fast
    // while staying at the lowest cadence production configs can request.
    let cfg = DriverConfig::from_value(json!({
        "model": "gpt-fixture",
        "reasoning_effort": "low",
        "heartbeat_interval_ms": 100,
    }));
    let mut session = driver.acquire(ctx(), cfg).await.unwrap();

    // First substantive event is Ready (handshake completed).
    let ready = timeout(Duration::from_secs(2), session.events.recv())
        .await
        .expect("ready timed out")
        .expect("event stream closed before ready");
    assert!(
        matches!(ready, DriverEvent::Ready { .. }),
        "expected Ready, got {ready:?}"
    );

    // The turn is now in flight and the peer is silent. Within a second of a
    // 60ms cadence we must observe multiple heartbeats and nothing else.
    let mut heartbeats = 0usize;
    let deadline = tokio::time::Instant::now() + Duration::from_millis(800);
    while tokio::time::Instant::now() < deadline {
        match timeout(Duration::from_millis(300), session.events.recv()).await {
            Ok(Some(DriverEvent::Heartbeat { seq })) => {
                assert_eq!(
                    seq as usize, heartbeats,
                    "heartbeat seq must be monotonic from 0"
                );
                heartbeats += 1;
                if heartbeats >= 3 {
                    break;
                }
            }
            Ok(Some(other)) => panic!("expected only heartbeats during quiet turn, got {other:?}"),
            Ok(None) => panic!("event stream closed during quiet turn"),
            Err(_) => {}
        }
    }
    assert!(
        heartbeats >= 3,
        "expected >=3 heartbeats during quiet turn, saw {heartbeats}"
    );

    // Release the run; assert no heartbeats arrive after the session ends.
    session.control.release("test done").await.unwrap();

    // Drain remaining events; the only legal events post-release are the
    // terminal RunComplete (from turn/completed) — never a Heartbeat.
    let mut saw_run_complete = false;
    while let Ok(Some(event)) = timeout(Duration::from_millis(400), session.events.recv()).await {
        match event {
            DriverEvent::Heartbeat { .. } => {
                panic!("heartbeat emitted after release / session close")
            }
            DriverEvent::RunComplete { .. } => saw_run_complete = true,
            _ => {}
        }
    }
    assert!(
        saw_run_complete,
        "expected RunComplete after interrupt-driven turn completion"
    );

    // Quiet settle window: confirm the ticker really stopped (no leaked timer).
    if let Ok(Some(event)) = timeout(Duration::from_millis(400), session.events.recv()).await {
        panic!("event after session drained (leaked heartbeat ticker?): {event:?}");
    }
}

/// TASK-100.5 (F1): the heartbeat arm must gate liveness on the *child*, not on
/// transport EOF. Here the peer answers the handshake, starts a turn, lets a few
/// heartbeats flow, then backgrounds a sibling that keeps the stdout pipe open
/// and exits its main process. EOF never arrives (the sibling holds fd 1), so a
/// fix that only watches the pipe would tick heartbeats forever. With the
/// `child.try_wait()` gate, heartbeats stop once the main process exits.
#[tokio::test]
async fn acp_stdio_heartbeats_stop_when_child_exits_with_pipe_held_open() {
    // Peer: handshake + turn, ~3 heartbeat windows of quiet, then spawn a
    // background `sleep` that inherits stdout (holding the pipe open past our
    // exit) and exit the main shell. The grandchild keeps fd 1 alive so the
    // acp-stdio transport never sees EOF.
    const EXITING_PEER: &str = r#"
while IFS= read -r line; do
  [ -z "$line" ] && continue
  method=$(printf '%s' "$line" | sed -n 's/.*"method"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p')
  id=$(printf '%s' "$line" | sed -n 's/.*"id"[[:space:]]*:[[:space:]]*\([0-9][0-9]*\).*/\1/p')
  case "$method" in
    initialize)
      printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"userAgent\":\"fixture\"}}"
      ;;
    thread/start)
      printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"thread\":{\"id\":\"thread-stdio-fixture\"}}}"
      ;;
    turn/start)
      printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"turn\":{\"id\":\"turn-stdio-fixture\",\"status\":\"inProgress\",\"items\":[]}}}"
      # Let a few heartbeats flow, then hand the stdout pipe to a backgrounded
      # sibling and exit. The sibling outlives us, keeping fd 1 open.
      sleep 0.35
      sleep 5 &
      exit 0
      ;;
  esac
done
"#;

    struct ExitingCodexHarness {
        inner: CodexAdapter,
    }

    #[async_trait]
    impl HarnessEventAdapter for ExitingCodexHarness {
        fn harness(&self) -> &'static str {
            "codex"
        }
        fn clone_box(&self) -> Box<dyn HarnessEventAdapter> {
            Box::new(ExitingCodexHarness {
                inner: CodexAdapter::new(),
            })
        }
        fn stdio_spawn(&self) -> Option<StdioSpawn> {
            Some(StdioSpawn {
                command: "sh".into(),
                args: vec!["-c".into(), EXITING_PEER.into()],
                cwd: None,
                env: Vec::new(),
            })
        }
        fn upgrades_simulated_to_subprocess(&self) -> bool {
            true
        }
        fn stdio_session_init(
            &mut self,
            ctx: &DriverContext,
            config: &DriverConfig,
        ) -> Result<Value, DriverError> {
            self.inner.stdio_session_init(ctx, config)
        }
        fn compose_request(
            &mut self,
            ctx: &DriverContext,
            config: &DriverConfig,
        ) -> Result<HarnessRequest, DriverError> {
            self.inner.compose_request(ctx, config)
        }
        async fn parse_event(&mut self, raw: Value) -> Vec<DriverEvent> {
            self.inner.parse_event(raw).await
        }
        async fn on_ws_thread_started(
            &mut self,
            endpoint: &str,
            thread_response: &Value,
        ) -> Result<Vec<DriverEvent>, DriverError> {
            self.inner
                .on_ws_thread_started(endpoint, thread_response)
                .await
        }
        fn ws_turn_start_params(&mut self) -> Result<Value, DriverError> {
            self.inner.ws_turn_start_params()
        }
        async fn on_ws_response(
            &mut self,
            method: &str,
            response: Value,
        ) -> Result<Vec<DriverEvent>, DriverError> {
            self.inner.on_ws_response(method, response).await
        }
        fn terminal_emitted(&self) -> bool {
            self.inner.terminal_emitted()
        }
        async fn release(&mut self, reason: String) -> Result<HarnessControlOutcome, DriverError> {
            self.inner.release(reason).await
        }
    }

    let driver = AcpStdioDriver::new(Box::new(ExitingCodexHarness {
        inner: CodexAdapter::new(),
    }));
    // 100ms cadence == the production floor; the test must keep working at it.
    let cfg = DriverConfig::from_value(json!({
        "model": "gpt-fixture",
        "reasoning_effort": "low",
        "heartbeat_interval_ms": 100,
    }));
    let mut session = driver.acquire(ctx(), cfg).await.unwrap();
    // The spawned `sh` is a process-group leader (setsid in detach_subprocess),
    // so its pid is the pgid for the whole peer subtree, including the
    // backgrounded sibling. We reap that group on cleanup to avoid an orphan
    // leak (TASK-104.1 shape).
    let peer_pid = session.pid.expect("stdio peer should report a pid");

    // Handshake completes -> Ready.
    let ready = timeout(Duration::from_secs(2), session.events.recv())
        .await
        .expect("ready timed out")
        .expect("event stream closed before ready");
    assert!(
        matches!(ready, DriverEvent::Ready { .. }),
        "expected Ready, got {ready:?}"
    );

    // Drain events continuously, tracking when we last saw a heartbeat. The
    // child stays alive ~350ms (a few cadence windows) then exits while the
    // sibling holds the pipe open. We require: (a) at least one heartbeat
    // arrived (cadence was running), and (b) heartbeats eventually STOP — a
    // drain window strictly longer than the cadence passes with none. Without
    // the try_wait gate, (b) never holds because the pipe never EOFs.
    let mut saw_heartbeat = false;
    let mut quiet_run = Duration::ZERO;
    // Cadence is 100ms; ~5 cadence windows of continuous silence proves the
    // ticker stopped rather than merely drained a transient buffer.
    let required_quiet = Duration::from_millis(550);
    let overall_deadline = tokio::time::Instant::now() + Duration::from_secs(4);
    loop {
        assert!(
            tokio::time::Instant::now() < overall_deadline,
            "heartbeats never stopped: ticker kept ticking after the child exited \
             (saw_heartbeat={saw_heartbeat})"
        );
        let poll = Duration::from_millis(100);
        match timeout(poll, session.events.recv()).await {
            Ok(Some(DriverEvent::Heartbeat { .. })) => {
                saw_heartbeat = true;
                quiet_run = Duration::ZERO;
            }
            Ok(Some(_)) => {
                // Non-heartbeat traffic is fine and does not count as a
                // heartbeat, but it does mean the loop is still emitting; reset
                // the quiet window so we only conclude on heartbeat silence.
                quiet_run = Duration::ZERO;
            }
            Ok(None) => break, // session closed -> heartbeats definitively stopped
            Err(_) => {
                quiet_run += poll;
                if saw_heartbeat && quiet_run >= required_quiet {
                    break;
                }
            }
        }
    }
    assert!(
        saw_heartbeat,
        "expected at least one heartbeat while the child was alive"
    );

    // Cleanup: reap the peer process group (main shell already exited; this
    // kills the lingering `sleep` sibling). Null stdio so the kill process does
    // not itself leak fds (TASK-104.1 shape).
    let _ = session.control.release("test done").await;
    let _ = std::process::Command::new("kill")
        .arg("--")
        .arg(format!("-{peer_pid}"))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

const MOCK_PEER_WITH_APPROVAL: &str = r#"
while IFS= read -r line; do
  [ -z "$line" ] && continue
  method=$(printf '%s' "$line" | sed -n 's/.*"method"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p')
  id=$(printf '%s' "$line" | sed -n 's/.*"id"[[:space:]]*:[[:space:]]*\([0-9][0-9]*\).*/\1/p')
  if [ -n "$ORGASMIC_STDIN_LOG" ]; then
    printf '%s\n' "$line" >> "$ORGASMIC_STDIN_LOG"
  fi
  case "$method" in
    initialize)
      printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"userAgent\":\"fixture\"}}"
      ;;
    thread/start)
      printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"thread\":{\"id\":\"thread-stdio-fixture\"}}}"
      ;;
    turn/start)
      printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"turn\":{\"id\":\"turn-stdio-fixture\",\"status\":\"inProgress\",\"items\":[]}}}"
      printf '%s\n' "{\"jsonrpc\":\"2.0\",\"method\":\"item/agentMessage/delta\",\"params\":{\"delta\":\"stdio handshake\"}}"
      printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":999,\"method\":\"exec_approval_request\",\"params\":{\"command\":\"cargo test --workspace\",\"threadId\":\"thread-stdio-fixture\",\"turnId\":\"turn-stdio-fixture\"}}"
      read -r approval_line
      printf '%s\n' "$approval_line" >> "$ORGASMIC_APPROVAL_LOG"
      printf '%s\n' "{\"jsonrpc\":\"2.0\",\"method\":\"rawResponseItem/completed\",\"params\":{\"item\":{\"type\":\"function_call\",\"name\":\"shell\",\"arguments\":\"{}\",\"call_id\":\"call-after-approval\"}}}"
      printf '%s\n' "{\"jsonrpc\":\"2.0\",\"method\":\"turn/completed\",\"params\":{\"threadId\":\"thread-stdio-fixture\",\"turn\":{\"id\":\"turn-stdio-fixture\",\"status\":\"completed\",\"items\":[]}}}"
      ;;
    turn/interrupt)
      printf '%s\n' "{\"jsonrpc\":\"2.0\",\"method\":\"turn/completed\",\"params\":{\"threadId\":\"thread-stdio-fixture\",\"turn\":{\"id\":\"turn-stdio-fixture\",\"status\":\"completed\",\"items\":[]}}}"
      ;;
  esac
done
"#;

struct CodexStdioApprovalHarness {
    inner: CodexAdapter,
    mock_peer: &'static str,
    stdin_log_path: PathBuf,
    approval_log_path: PathBuf,
}

#[async_trait]
impl HarnessEventAdapter for CodexStdioApprovalHarness {
    fn harness(&self) -> &'static str {
        "codex"
    }

    fn clone_box(&self) -> Box<dyn HarnessEventAdapter> {
        Box::new(CodexStdioApprovalHarness {
            inner: CodexAdapter::new(),
            mock_peer: self.mock_peer,
            stdin_log_path: self.stdin_log_path.clone(),
            approval_log_path: self.approval_log_path.clone(),
        })
    }

    fn stdio_spawn(&self) -> Option<StdioSpawn> {
        Some(StdioSpawn {
            command: "sh".into(),
            args: vec!["-c".into(), self.mock_peer.into()],
            cwd: None,
            env: vec![
                (
                    "ORGASMIC_STDIN_LOG".into(),
                    self.stdin_log_path.to_string_lossy().into_owned(),
                ),
                (
                    "ORGASMIC_APPROVAL_LOG".into(),
                    self.approval_log_path.to_string_lossy().into_owned(),
                ),
            ],
        })
    }

    fn upgrades_simulated_to_subprocess(&self) -> bool {
        true
    }

    fn stdio_session_init(
        &mut self,
        ctx: &DriverContext,
        config: &DriverConfig,
    ) -> Result<Value, DriverError> {
        self.inner.stdio_session_init(ctx, config)
    }

    fn compose_request(
        &mut self,
        ctx: &DriverContext,
        config: &DriverConfig,
    ) -> Result<HarnessRequest, DriverError> {
        self.inner.compose_request(ctx, config)
    }

    async fn parse_event(&mut self, raw: Value) -> Vec<DriverEvent> {
        self.inner.parse_event(raw).await
    }

    async fn try_handle_approval(
        &mut self,
        method: &str,
        params: &Value,
        allowlist: &orgasmic_core::SandboxAllowlist,
    ) -> Option<orgasmic_drivers::ApprovalResponse> {
        self.inner
            .try_handle_approval(method, params, allowlist)
            .await
    }

    async fn on_ws_thread_started(
        &mut self,
        endpoint: &str,
        thread_response: &Value,
    ) -> Result<Vec<DriverEvent>, DriverError> {
        self.inner
            .on_ws_thread_started(endpoint, thread_response)
            .await
    }

    fn ws_turn_start_params(&mut self) -> Result<Value, DriverError> {
        self.inner.ws_turn_start_params()
    }

    async fn on_ws_response(
        &mut self,
        method: &str,
        response: Value,
    ) -> Result<Vec<DriverEvent>, DriverError> {
        self.inner.on_ws_response(method, response).await
    }

    fn terminal_emitted(&self) -> bool {
        self.inner.terminal_emitted()
    }

    async fn release(&mut self, reason: String) -> Result<HarnessControlOutcome, DriverError> {
        self.inner.release(reason).await
    }
}

async fn run_codex_stdio_approval_handshake(
    config: DriverConfig,
    expect_approved: bool,
) -> (PathBuf, PathBuf) {
    let stdin_log = std::env::temp_dir().join(format!(
        "orgasmic-acp-stdio-stdin-{}-{}.log",
        std::process::id(),
        uuid_simple()
    ));
    let approval_log = std::env::temp_dir().join(format!(
        "orgasmic-acp-stdio-approval-{}-{}.log",
        std::process::id(),
        uuid_simple()
    ));
    let _ = fs::remove_file(&stdin_log);
    let _ = fs::remove_file(&approval_log);

    let driver = AcpStdioDriver::new(Box::new(CodexStdioApprovalHarness {
        inner: CodexAdapter::new(),
        mock_peer: MOCK_PEER_WITH_APPROVAL,
        stdin_log_path: stdin_log.clone(),
        approval_log_path: approval_log.clone(),
    }));
    let mut session = driver.acquire(ctx(), config).await.unwrap();

    let ready = timeout(Duration::from_secs(2), session.events.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(ready, DriverEvent::Ready { .. }));

    let text = timeout(Duration::from_secs(2), session.events.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        text,
        DriverEvent::TextChunk {
            stream: TextStream::Assistant,
            chunk,
            ..
        } if chunk == "stdio handshake"
    ));

    let approval_call = timeout(Duration::from_secs(2), session.events.recv())
        .await
        .unwrap()
        .unwrap();
    let DriverEvent::ToolCall { name, .. } = approval_call else {
        panic!("expected approval ToolCall, got {approval_call:?}");
    };
    assert_eq!(name, "exec_approval_request");

    let approval_result = timeout(Duration::from_secs(2), session.events.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        approval_result,
        DriverEvent::ToolResult { ok, output, .. }
            if ok == expect_approved
                && output.as_str() == Some(if expect_approved { "Approved" } else { "Denied" })
    ));

    let tool = timeout(Duration::from_secs(2), session.events.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(tool, DriverEvent::ToolCall { .. }));

    let complete = timeout(Duration::from_secs(2), session.events.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(complete, DriverEvent::RunComplete { .. }));

    (stdin_log, approval_log)
}

fn uuid_simple() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

#[tokio::test]
async fn acp_stdio_codex_approval_request_auto_grants_with_default_allowlist() {
    let (stdin_log, approval_log) = run_codex_stdio_approval_handshake(
        DriverConfig::from_value(json!({
            "model": "gpt-fixture",
            "reasoning_effort": "low",
        })),
        true,
    )
    .await;

    let approval_response = fs::read_to_string(&approval_log).expect("approval log");
    assert!(
        approval_response.contains("\"id\":999") || approval_response.contains("\"id\": 999"),
        "approval response missing request id: {approval_response}"
    );
    assert!(
        approval_response.contains("\"decision\":\"accept\"")
            || approval_response.contains("\"decision\": \"accept\""),
        "approval response missing accept decision: {approval_response}"
    );
    let stdin_lines = fs::read_to_string(&stdin_log).expect("stdin log");
    assert!(stdin_lines.contains("turn/start"));
}

#[tokio::test]
async fn acp_stdio_codex_approval_request_denies_when_exec_disallowed() {
    let (stdin_log, approval_log) = run_codex_stdio_approval_handshake(
        DriverConfig::from_value(json!({
            "model": "gpt-fixture",
            "reasoning_effort": "low",
            "sandbox_permissions": "allow_exec=false,allow_patch=true,allow_network=true,allow_writes_outside_cwd=true",
        })),
        false,
    )
    .await;

    let approval_response = fs::read_to_string(&approval_log).expect("approval log");
    assert!(
        approval_response.contains("\"decision\":\"decline\"")
            || approval_response.contains("\"decision\": \"decline\""),
        "approval response missing decline decision: {approval_response}"
    );
    let _stdin_lines = fs::read_to_string(&stdin_log).expect("stdin log");
}

#[tokio::test]
async fn acp_stdio_routes_stdout_through_adapter() {
    let driver = AcpStdioDriver::new(Box::new(MockAdapter {
        request: subprocess_request("stdio"),
    }));
    let mut session = driver.acquire(ctx(), DriverConfig::empty()).await.unwrap();
    let event = session.events.recv().await.unwrap();
    assert!(matches!(
        event,
        DriverEvent::TextChunk { chunk, .. } if chunk == "stdio"
    ));
}

#[tokio::test]
async fn acp_ws_routes_frames_through_adapter() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = accept_async(stream).await.unwrap();
        let init = ws.next().await.unwrap().unwrap();
        assert_eq!(
            init.into_text().unwrap(),
            json!({"type": "spawn"}).to_string()
        );
        ws.send(Message::Text(json!({"msg": "websocket"}).to_string()))
            .await
            .unwrap();
    });

    let driver = AcpWsDriver::new(Box::new(MockAdapter {
        request: HarnessRequest::AcpWs {
            endpoint: format!("ws://{addr}/acp"),
            headers: BTreeMap::new(),
            protocol: AcpWsProtocol::RawJson,
            session_init: json!({"type": "spawn"}),
        },
    }));
    let mut session = driver.acquire(ctx(), DriverConfig::empty()).await.unwrap();
    let event = session.events.recv().await.unwrap();
    assert!(matches!(
        event,
        DriverEvent::TextChunk { chunk, .. } if chunk == "websocket"
    ));
    server.await.unwrap();
}

#[tokio::test]
async fn tmux_inert_mode_emits_ready_then_closes_on_release_without_tmux_binary() {
    let driver = TmuxDriver::new(Box::new(MockAdapter {
        request: simulated_request(),
    }));
    let mut session = driver
        .acquire(ctx(), force_inert_tmux_config())
        .await
        .unwrap();
    let ready = session.events.recv().await.unwrap();
    assert!(matches!(
        ready,
        DriverEvent::Ready {
            protocol_version,
            capabilities,
        } if protocol_version == "tmux-tui/1" && capabilities["inert"] == true
    ));
    session.control.release("done").await.unwrap();
    assert!(session.events.recv().await.is_none());
}

#[tokio::test]
async fn codex_turn_start_terminal_notification_does_not_emit_followup_error() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = accept_async(stream).await.unwrap();

        let init = recv_json(&mut ws).await;
        assert_eq!(init["method"], "initialize");
        send_response(&mut ws, init["id"].clone(), json!({"userAgent": "fixture"})).await;

        let thread = recv_json(&mut ws).await;
        assert_eq!(thread["method"], "thread/start");
        send_response(
            &mut ws,
            thread["id"].clone(),
            json!({"thread": {"id": "thread-terminal"}}),
        )
        .await;

        let turn = recv_json(&mut ws).await;
        assert_eq!(turn["method"], "turn/start");
        ws.send(Message::Text(
            json!({
                "jsonrpc": "2.0",
                "method": "turn/completed",
                "params": {
                    "threadId": "thread-terminal",
                    "turn": {
                        "id": "turn-terminal",
                        "status": "completed",
                        "items": [],
                    },
                },
            })
            .to_string(),
        ))
        .await
        .unwrap();
        let _ = ws.close(None).await;
    });

    let driver = CodexAppserverDriver;
    let cfg = DriverConfig::from_value(json!({
        "endpoint": format!("ws://{addr}"),
        "model": "gpt-fixture",
    }));
    let mut session = driver.acquire(ctx(), cfg).await.unwrap();

    let ready = timeout(Duration::from_secs(2), session.events.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(ready, DriverEvent::Ready { .. }));

    let complete = timeout(Duration::from_secs(2), session.events.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(complete, DriverEvent::RunComplete { .. }));

    if let Ok(Some(event)) = timeout(Duration::from_millis(300), session.events.recv()).await {
        panic!("unexpected event after terminal completion: {event:?}");
    }
    server.await.unwrap();
}

#[tokio::test]
async fn hermes_connect_error_is_emitted_on_event_stream() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);

    let driver = driver_for_mode_harness("acp-ws", "hermes").expect("acp-ws hermes driver");
    let cfg = DriverConfig::from_value(json!({
        "endpoint": format!("ws://{addr}"),
        "session_token": "fixture-token",
    }));
    let mut session = driver.acquire(ctx(), cfg).await.unwrap();
    let event = timeout(Duration::from_secs(2), session.events.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        event,
        DriverEvent::DriverError {
            fatal: true,
            message,
        } if message.contains("websocket connect")
    ));
}

#[tokio::test]
async fn codex_connect_error_still_fails_acquire() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);

    let driver = CodexAppserverDriver;
    let cfg = DriverConfig::from_value(json!({"endpoint": format!("ws://{addr}")}));
    let result = driver.acquire(ctx(), cfg).await;
    let Err(err) = result else {
        panic!("codex unreachable endpoint should fail acquire");
    };
    assert!(matches!(err, DriverError::Transport(_)));
}

#[tokio::test]
async fn hermes_adapter_accepts_native_driver_event_fixture() {
    let mut adapter = HermesAdapter::new();
    let events = adapter
        .parse_event(json!({
            "type": "text_chunk",
            "stream": "assistant",
            "chunk": "native fixture",
            "seq": 4,
        }))
        .await;
    assert_eq!(
        events,
        vec![DriverEvent::TextChunk {
            stream: TextStream::Assistant,
            chunk: "native fixture".into(),
            seq: 4,
        }]
    );
}

#[tokio::test]
async fn adapter_fixtures_cover_public_harness_event_shapes() {
    let mut codex = CodexAdapter::new();
    let codex_events = codex
        .parse_event(json!({
            "jsonrpc": "2.0",
            "method": "item/agentMessage/delta",
            "params": {"delta": "codex fixture"},
        }))
        .await;
    assert_eq!(
        codex_events,
        vec![DriverEvent::TextChunk {
            stream: TextStream::Assistant,
            chunk: "codex fixture".into(),
            seq: 0,
        }]
    );

    let mut hermes = HermesAdapter::new();
    let hermes_events = hermes
        .parse_event(json!({
            "type": "text",
            "stream": "assistant",
            "text": "hermes fixture",
        }))
        .await;
    assert_eq!(
        hermes_events,
        vec![DriverEvent::TextChunk {
            stream: TextStream::Assistant,
            chunk: "hermes fixture".into(),
            seq: 0,
        }]
    );

    // Force simulated mode so the assertion is deterministic regardless of
    // whether the `claude` binary is on PATH on the host running the suite.
    // The real-wire gate fix (TASK-099) changed the default to real when the
    // binary is detectable; ORGASMIC_DRIVER_SIMULATE=1 is the explicit opt-in.
    std::env::set_var("ORGASMIC_DRIVER_SIMULATE", "1");
    let mut claude = ClaudeAdapter::new();
    let claude_request = claude
        .compose_request(&ctx(), &DriverConfig::empty())
        .unwrap();
    std::env::remove_var("ORGASMIC_DRIVER_SIMULATE");
    assert!(
        matches!(claude_request, HarnessRequest::Simulated { events } if matches!(events.first(), Some(DriverEvent::Ready { .. })))
    );

    let mut cursor = CursorAdapter::new();
    let cursor_request = cursor
        .compose_request(&ctx(), &DriverConfig::empty())
        .unwrap();
    assert!(
        matches!(cursor_request, HarnessRequest::Simulated { events } if matches!(events.first(), Some(DriverEvent::Ready { .. })))
    );
}

#[tokio::test]
async fn cursor_tool_policy_allows_brief_read_and_worktree_git() {
    let temp = tempfile::tempdir().unwrap();
    let project_root = temp.path().join("project");
    // Briefs live in a per-task subfolder under the dispatch dir; the sandbox
    // policy must still allow reads anywhere under `.orgasmic/tmp/dispatch`.
    let dispatch_dir = project_root.join(".orgasmic/tmp/dispatch/task-080-impl");
    fs::create_dir_all(&dispatch_dir).unwrap();
    let worktree = temp.path().join("worktree");
    fs::create_dir(&worktree).unwrap();
    let project_root = project_root.canonicalize().unwrap();
    let worktree = worktree.canonicalize().unwrap();
    let worktree_str = worktree.display().to_string();
    let brief_path =
        project_root.join(".orgasmic/tmp/dispatch/task-080-impl/task-080-impl-brief.md");

    let mut cursor = CursorAdapter::new();
    let ctx = ctx_with_worktree(worktree);
    let cfg = DriverConfig::from_value(json!({ "project_root": project_root }));
    let _ = cursor
        .compose_request(&ctx, &cfg)
        .expect("cursor request initializes translator");

    let cases = [
        json!({
            "type": "tool_use",
            "id": "read-brief",
            "name": "read",
            "input": {"path": brief_path}
        }),
        json!({
            "type": "tool_use",
            "id": "git-diff",
            "name": "shell",
            "input": {"command": format!("git -C {worktree_str} diff HEAD")}
        }),
        json!({
            "type": "tool_use",
            "id": "grep-worktree",
            "name": "shell",
            "input": {"command": format!("grep --glob '**/*.rs' --path {worktree_str} TASK-080")}
        }),
        json!({
            "type": "tool_use",
            "id": "await-helper",
            "name": "await",
            "input": {"regex": "test result", "taskId": "task-080"}
        }),
        json!({
            "type": "tool_use",
            "id": "kill-cleanup",
            "name": "shell",
            "input": {"command": "kill 12345 2>/dev/null"}
        }),
    ];

    for expected_id in [
        "read-brief",
        "git-diff",
        "grep-worktree",
        "await-helper",
        "kill-cleanup",
    ] {
        let raw = cases
            .iter()
            .find(|case| case["id"] == expected_id)
            .expect("case exists")
            .clone();
        let events = cursor.parse_event(raw).await;
        assert_eq!(events.len(), 1, "expected one event for {expected_id}");
        assert!(
            matches!(
                &events[0],
                DriverEvent::ToolCall { call_id, .. } if call_id == expected_id
            ),
            "expected allowed ToolCall for {expected_id}, got {:?}",
            events[0]
        );
    }
}

#[tokio::test]
#[cfg(unix)]
async fn cursor_tool_policy_blocks_dangerous_cases() {
    let temp = tempfile::tempdir().unwrap();
    let project_root = temp.path().join("project");
    let dispatch_dir = project_root.join(".orgasmic/tmp/dispatch");
    fs::create_dir_all(&dispatch_dir).unwrap();
    let worktree = temp.path().join("worktree");
    fs::create_dir(&worktree).unwrap();
    let project_root = project_root.canonicalize().unwrap();
    let worktree = worktree.canonicalize().unwrap();
    let worktree_str = worktree.display().to_string();

    let host_file = temp.path().join("host-secret.txt");
    fs::write(&host_file, "secret").unwrap();
    let symlink_path = project_root.join(format!(
        ".orgasmic/tmp/dispatch/task-080-symlink-{}",
        std::process::id()
    ));
    fs::create_dir_all(symlink_path.parent().unwrap()).unwrap();
    let _ = fs::remove_file(&symlink_path);
    std::os::unix::fs::symlink(&host_file, &symlink_path).unwrap();

    let mut cursor = CursorAdapter::new();
    let ctx = ctx_with_worktree(worktree);
    let cfg = DriverConfig::from_value(json!({ "project_root": project_root }));
    let _ = cursor
        .compose_request(&ctx, &cfg)
        .expect("cursor request initializes translator");

    let cases = [
        json!({
            "type": "tool_use",
            "id": "read-host-top",
            "name": "read",
            "input": {"path": "/etc/passwd"}
        }),
        json!({
            "type": "tool_use",
            "id": "git-other-top",
            "name": "shell",
            "input": {"command": "git -C /tmp/some/other/dir diff"}
        }),
        json!({
            "type": "tool_use",
            "id": "git-redir-top",
            "name": "shell",
            "input": {"command": format!("git -C {worktree_str} diff > /tmp/leak")}
        }),
        json!({
            "type": "tool_use",
            "id": "read-decoy-top",
            "name": "read",
            "input": {"path": "/etc/passwd", "cwd": worktree_str.clone()}
        }),
        json!({
            "type": "tool_use",
            "id": "read-symlink-top",
            "name": "read",
            "input": {"path": symlink_path.display().to_string()}
        }),
        json!({
            "type": "tool_call",
            "subtype": "started",
            "call_id": "read-host-nested",
            "tool_call": {"name": "read", "args": {"path": "/etc/passwd"}}
        }),
        json!({
            "type": "tool_call",
            "subtype": "started",
            "call_id": "git-other-nested",
            "tool_call": {"name": "shell", "args": {"command": "git -C /tmp/some/other/dir diff"}}
        }),
        json!({
            "type": "tool_call",
            "subtype": "started",
            "call_id": "git-redir-nested",
            "tool_call": {"name": "shell", "args": {"command": format!("git -C {worktree_str} diff > /tmp/leak")}}
        }),
        json!({
            "type": "tool_call",
            "subtype": "started",
            "call_id": "read-decoy-nested",
            "tool_call": {"name": "read", "args": {"path": "/etc/passwd", "cwd": worktree_str.clone()}}
        }),
        json!({
            "type": "tool_call",
            "subtype": "started",
            "call_id": "read-symlink-nested",
            "tool_call": {"name": "read", "args": {"path": symlink_path.display().to_string()}}
        }),
    ];

    for raw in cases {
        let events = cursor.parse_event(raw).await;
        assert_eq!(events.len(), 1, "expected one policy telemetry event");
        assert!(
            matches!(
                &events[0],
                DriverEvent::TextChunk {
                    stream: TextStream::System,
                    chunk,
                    ..
                } if chunk.contains("ignored by orgasmic tool policy")
            ),
            "expected blocked tool telemetry, got {:?}",
            events[0]
        );
    }

    let _ = fs::remove_file(&symlink_path);
}

#[tokio::test]
async fn legacy_drivers_and_explicit_pairs_emit_equivalent_start_events() {
    let cases = [
        ("claude-acp", "acp-stdio", "claude", DriverConfig::empty()),
        ("codex-appserver", "acp-ws", "codex", DriverConfig::empty()),
        (
            "cursor-agent",
            "subprocess-stream-json",
            "cursor-agent",
            DriverConfig::empty(),
        ),
        ("hermes", "acp-stdio", "hermes", DriverConfig::empty()),
        ("tmux-tui", "tmux", "claude", force_inert_tmux_config()),
    ];

    for (legacy, mode, harness, config) in cases {
        // Share one identity so harness-aware native metadata (e.g. the tmux
        // claude --session-id pinned to runtime_id) is identical across the
        // legacy and explicit drivers being compared.
        let shared = ctx();
        let legacy_ready = normalize_volatile_ready_event(
            first_event(driver_for(legacy).unwrap(), shared.clone(), config.clone()).await,
        );
        let explicit_ready = normalize_volatile_ready_event(
            first_event(
                driver_for_mode_harness(mode, harness).unwrap(),
                shared,
                config,
            )
            .await,
        );
        assert_eq!(
            legacy_ready, explicit_ready,
            "legacy={legacy} mode={mode} harness={harness}"
        );
    }
}

async fn first_event(
    driver: Box<dyn WorkerDriver>,
    ctx: DriverContext,
    config: DriverConfig,
) -> DriverEvent {
    let mut session = driver.acquire(ctx, config).await.unwrap();
    let event = session.events.recv().await.unwrap();
    session.control.release("test cleanup").await.unwrap();
    event
}

fn normalize_volatile_ready_event(event: DriverEvent) -> DriverEvent {
    let DriverEvent::Ready {
        protocol_version,
        mut capabilities,
    } = event
    else {
        return event;
    };
    if let Some(object) = capabilities.as_object_mut() {
        object.remove("session_id");
    }
    DriverEvent::Ready {
        protocol_version,
        capabilities,
    }
}

async fn recv_json<S>(ws: &mut WebSocketStream<S>) -> Value
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    loop {
        let message = ws.next().await.unwrap().unwrap();
        let value = match message {
            Message::Text(text) => serde_json::from_str(&text).unwrap(),
            Message::Binary(bytes) => serde_json::from_slice(&bytes).unwrap(),
            Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => Value::Null,
            Message::Close(_) => Value::Null,
        };
        if !value.is_null() {
            return value;
        }
    }
}

async fn send_response<S>(ws: &mut WebSocketStream<S>, id: Value, result: Value)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    ws.send(Message::Text(
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result,
        })
        .to_string(),
    ))
    .await
    .unwrap();
}

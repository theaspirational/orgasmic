// orgasmic:arch_C87Z9, dec_YYMSK
//! `notify`-based filesystem watcher with daemon-side debounce (dec_022).
//!
//! Watches every project root registered on the board plus the home tx
//! directory. Filesystem events are coalesced inside a configurable
//! debounce window (default 200ms) before triggering an index refresh.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use anyhow::{Context, Result};
use notify::{Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use orgasmic_core::Home;
use tokio::sync::mpsc;
use tracing::{debug, trace, warn};

use crate::events::{EventBus, EventPayload, Topic};
use crate::index::Index;

#[derive(Debug, Clone)]
pub struct WatcherConfig {
    pub debounce: Duration,
    /// When false, no `notify` backend is created: `spawn` returns a handle
    /// whose commands are accepted but no-op'd, and no filesystem events are
    /// ever delivered. The bulk of the daemon integration suite never asserts
    /// on watcher-driven refresh, so it opts out to avoid the ~0.8s-per-call
    /// macOS FSEvents registration latency that otherwise dominates boot.
    pub enabled: bool,
}

impl Default for WatcherConfig {
    fn default() -> Self {
        Self {
            debounce: Duration::from_millis(200),
            enabled: true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct WatcherHandle {
    cmd: mpsc::Sender<WatcherCommand>,
}

#[derive(Debug)]
enum WatcherCommand {
    WatchProject(PathBuf),
    Unwatch(PathBuf),
    Refresh,
}

fn forward_events(
    raw_tx: mpsc::UnboundedSender<Event>,
) -> impl FnMut(notify::Result<Event>) + Send + 'static {
    move |res: notify::Result<Event>| match res {
        Ok(event) => send_raw_event(&raw_tx, event),
        Err(err) => warn!(error = %err, "fs watcher error"),
    }
}

fn send_raw_event(raw_tx: &mpsc::UnboundedSender<Event>, event: Event) {
    debug!(
        kind = ?event.kind,
        paths = ?event.paths,
        "raw fs watcher event"
    );
    let _ = raw_tx.send(event);
}

impl WatcherHandle {
    pub async fn watch_project(&self, path: PathBuf) -> Result<()> {
        self.cmd
            .send(WatcherCommand::WatchProject(path))
            .await
            .context("watcher gone")
    }

    pub async fn unwatch(&self, path: PathBuf) -> Result<()> {
        self.cmd
            .send(WatcherCommand::Unwatch(path))
            .await
            .context("watcher gone")
    }

    pub async fn request_refresh(&self) -> Result<()> {
        self.cmd
            .send(WatcherCommand::Refresh)
            .await
            .context("watcher gone")
    }
}

pub fn spawn(
    home: Home,
    index: Index,
    events: EventBus,
    cfg: WatcherConfig,
) -> Result<WatcherHandle> {
    let (cmd_tx, cmd_rx) = mpsc::channel::<WatcherCommand>(64);

    if !cfg.enabled {
        // Disabled watcher: drain commands so senders never block, but never
        // create a `notify` backend (avoids macOS FSEvents registration cost).
        debug!("fs watcher disabled; no filesystem events will be delivered");
        tokio::spawn(async move {
            let mut rx = cmd_rx;
            while rx.recv().await.is_some() {}
        });
        return Ok(WatcherHandle { cmd: cmd_tx });
    }

    let (raw_tx, raw_rx) = mpsc::unbounded_channel::<Event>();

    let mut watcher = RecommendedWatcher::new(forward_events(raw_tx.clone()), Config::default())
        .context("create notify watcher")?;
    debug!(kind = ?RecommendedWatcher::kind(), "fs watcher started");

    let watched: Arc<StdMutex<HashSet<PathBuf>>> = Arc::new(StdMutex::new(HashSet::new()));
    let home_tx = canonical(&home.tx());
    if home_tx.is_dir() {
        if let Err(err) = watcher.watch(&home_tx, RecursiveMode::Recursive) {
            warn!(path = %home_tx.display(), error = %err, "watch home tx failed");
        } else {
            debug!(path = %home_tx.display(), "watching home tx");
            watched.lock().unwrap().insert(home_tx.clone());
        }
    }
    let board = canonical(&home.board());
    if let Some(parent) = board.parent() {
        if parent.is_dir() {
            if let Err(err) = watcher.watch(parent, RecursiveMode::NonRecursive) {
                warn!(path = %parent.display(), error = %err, "watch user dir failed");
            } else {
                debug!(path = %parent.display(), "watching user dir");
                watched.lock().unwrap().insert(parent.to_path_buf());
            }
        }
    }

    let watcher = Arc::new(StdMutex::new(watcher));
    let watched_for_cmd = watched.clone();
    let watcher_for_cmd = watcher.clone();
    let raw_tx_for_cmd = raw_tx.clone();
    tokio::spawn(command_loop(
        cmd_rx,
        watcher_for_cmd,
        watched_for_cmd,
        raw_tx_for_cmd,
    ));

    // The debounce loop needs to be able to register a watch for a project
    // that appears on the board after boot (dec_YYMSK follow-up). It only
    // sends `WatcherCommand`s over this handle, which `command_loop` (a
    // separate task) drains — it never calls back into its own event loop,
    // so this is not re-entrant.
    let handle_for_flush = WatcherHandle {
        cmd: cmd_tx.clone(),
    };
    tokio::spawn(debounce_loop(
        raw_rx,
        index,
        home,
        events,
        cfg.debounce,
        handle_for_flush,
    ));

    Ok(WatcherHandle { cmd: cmd_tx })
}

async fn command_loop(
    mut rx: mpsc::Receiver<WatcherCommand>,
    watcher: Arc<StdMutex<RecommendedWatcher>>,
    watched: Arc<StdMutex<HashSet<PathBuf>>>,
    _raw_tx: mpsc::UnboundedSender<Event>,
) {
    while let Some(cmd) = rx.recv().await {
        match cmd {
            WatcherCommand::WatchProject(path) => {
                let path = canonical(&path);
                if watched.lock().unwrap().contains(&path) {
                    continue;
                }
                let orgasmic_dir = path.join(".orgasmic");
                if !orgasmic_dir.is_dir() {
                    debug!(path = %orgasmic_dir.display(), "skipping non-existent .orgasmic");
                    continue;
                }
                let mut w = watcher.lock().unwrap();
                if let Err(err) = w.watch(&orgasmic_dir, RecursiveMode::Recursive) {
                    warn!(path = %orgasmic_dir.display(), error = %err, "watch project failed");
                    continue;
                }
                debug!(
                    project_root = %path.display(),
                    path = %orgasmic_dir.display(),
                    "watching project .orgasmic"
                );
                drop(w);
                watched.lock().unwrap().insert(path);
            }
            WatcherCommand::Unwatch(path) => {
                let path = canonical(&path);
                let orgasmic_dir = path.join(".orgasmic");
                let mut w = watcher.lock().unwrap();
                let _ = w.unwatch(&orgasmic_dir);
                drop(w);
                watched.lock().unwrap().remove(&path);
            }
            WatcherCommand::Refresh => {
                // No-op nudge: the debounce loop drains on every event.
            }
        }
    }
}

async fn debounce_loop(
    mut raw: mpsc::UnboundedReceiver<Event>,
    index: Index,
    home: Home,
    events: EventBus,
    debounce: Duration,
    watcher: WatcherHandle,
) {
    let mut pending: HashSet<PathBuf> = HashSet::new();
    loop {
        // Wait for the first event of the next batch.
        let Some(event) = raw.recv().await else {
            break;
        };
        record_event(&mut pending, &event);
        // Collect any additional events within the debounce window.
        let deadline = tokio::time::Instant::now() + debounce;
        loop {
            let sleep = tokio::time::sleep_until(deadline);
            tokio::select! {
                maybe_event = raw.recv() => {
                    let Some(event) = maybe_event else {
                        // Channel closed mid-batch; process and exit after.
                        flush(&mut pending, &index, &home, &events, &watcher).await;
                        return;
                    };
                    record_event(&mut pending, &event);
                }
                _ = sleep => {
                    break;
                }
            }
        }
        flush(&mut pending, &index, &home, &events, &watcher).await;
    }
}

pub(crate) fn canonical(p: &std::path::Path) -> PathBuf {
    if let Ok(canon) = std::fs::canonicalize(p) {
        return canon;
    }

    let mut current = p;
    let mut missing = Vec::new();
    while let Some(parent) = current.parent() {
        if let Some(name) = current.file_name() {
            missing.push(name.to_os_string());
        }
        if let Ok(mut canon) = std::fs::canonicalize(parent) {
            for component in missing.iter().rev() {
                canon.push(component);
            }
            return canon;
        }
        current = parent;
    }

    p.to_path_buf()
}

fn record_event(pending: &mut HashSet<PathBuf>, event: &Event) {
    if !matches!(
        event.kind,
        EventKind::Create(_)
            | EventKind::Modify(_)
            | EventKind::Remove(_)
            | EventKind::Any
            | EventKind::Other
    ) {
        return;
    }
    crate::supervisor::record_watcher_event_handled();
    // macOS/FSEvents can report atomic rewrites such as
    // `backlog.org.<uuid>.tmp -> backlog.org` as temp-file or parent-dir
    // events. Filtering to `.org` here drops those production edits, so
    // collect the path and let `flush` decide whether it belongs to a known
    // project, board, or tx root.
    for path in &event.paths {
        trace!(path = %path.display(), "recorded fs path");
        pending.insert(path.clone());
    }
}

async fn flush(
    pending: &mut HashSet<PathBuf>,
    index: &Index,
    home: &Home,
    events: &EventBus,
    watcher: &WatcherHandle,
) {
    if pending.is_empty() {
        return;
    }
    let snap = index.snapshot().await;
    let board = canonical(&home.board());
    let home_tx_dir = canonical(&home.tx());

    let mut project_paths: Vec<(String, PathBuf)> = Vec::new();
    let mut home_tx_paths = Vec::new();
    let mut touched_board = false;

    for path in pending.drain() {
        let canon = canonical(&path);
        if canon == board {
            touched_board = true;
            debug!(
                path = %path.display(),
                canon = %canon.display(),
                classified = "board",
                "fs path classified"
            );
            continue;
        }
        if canon.starts_with(&home_tx_dir) {
            home_tx_paths.push(canon.clone());
            debug!(
                path = %path.display(),
                canon = %canon.display(),
                classified = "home_tx",
                "fs path classified"
            );
            continue;
        }
        let mut classified = "dropped".to_string();
        for entry in &snap.board {
            let entry_root = canonical(&entry.path);
            if canon.starts_with(&entry_root) {
                let tmp_dir = canonical(&entry_root.join(".orgasmic/tmp"));
                if canon.starts_with(&tmp_dir) {
                    classified = "dropped_tmp".to_string();
                    break;
                }
                let views_dir = canonical(&entry_root.join(".orgasmic/views"));
                if canon.starts_with(&views_dir) {
                    classified = "dropped_views".to_string();
                    break;
                }
                project_paths.push((entry.id.clone(), canon.clone()));
                classified = format!("project={}", entry.id);
                break;
            }
        }
        debug!(
            path = %path.display(),
            canon = %canon.display(),
            classified = %classified,
            "fs path classified"
        );
    }

    if touched_board {
        let registered_before = snap
            .board
            .iter()
            .cloned()
            .map(|entry| (entry.id.clone(), entry))
            .collect::<HashMap<_, _>>();
        index.refresh_board().await;
        events.publish(Topic::Board, EventPayload::BoardRefreshed);
        let refreshed_board = index.snapshot().await.board;
        let refreshed_ids = refreshed_board
            .iter()
            .map(|entry| entry.id.as_str())
            .collect::<HashSet<_>>();
        for prior in registered_before.values() {
            if !refreshed_ids.contains(prior.id.as_str()) {
                if let Err(err) = watcher.unwatch(prior.path.clone()).await {
                    warn!(project = %prior.id, error = %err, "unwatch removed project failed");
                }
            }
        }
        for entry in refreshed_board {
            let prior = registered_before.get(&entry.id);
            if prior == Some(&entry) {
                continue;
            }
            if let Some(prior) = prior.filter(|prior| prior.path != entry.path) {
                if let Err(err) = watcher.unwatch(prior.path.clone()).await {
                    warn!(project = %entry.id, error = %err, "unwatch prior project root failed");
                }
            }
            // A new or changed board registration must own a watch for its
            // current root, while its operational projection remains unloaded
            // until first access or a project-local event.
            if let Err(err) = watcher.watch_project(entry.path.clone()).await {
                warn!(project = %entry.id, error = %err, "watch new or changed project failed");
            }
            if prior.is_some() {
                index.spawn_repo_url_reprobe_for(entry.id.clone(), entry.path.clone());
            } else {
                index.spawn_repo_url_refresh_for(entry.id.clone(), entry.path.clone());
            }
        }
    }
    for path in home_tx_paths {
        let result = if path.extension().and_then(|ext| ext.to_str()) == Some("org") {
            index.apply_written_path(&path).await.map(|_| ())
        } else {
            index.schedule_home_tx_refresh().await
        };
        match result {
            Ok(()) => events.publish(Topic::Daemon, EventPayload::DaemonHeartbeat),
            Err(err) => warn!(path = %path.display(), error = %err, "home tx refresh failed"),
        }
    }
    let mut published = HashSet::new();
    for (project_id, path) in project_paths {
        let changed = match index.apply_written_path(&path).await {
            Ok(changed) => changed,
            Err(err) => {
                warn!(project = %project_id, path = %path.display(), error = %err, "project refresh failed");
                continue;
            }
        };
        if !changed || !published.insert(project_id.clone()) {
            continue;
        }
        events.publish(
            Topic::Board,
            EventPayload::ProjectIndexed {
                project_id: project_id.clone(),
            },
        );
        events.publish(
            Topic::Task,
            EventPayload::TaskUpdated {
                project_id,
                task_id: "*".into(),
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::EventBus;
    use notify::event::{ModifyKind, RenameMode};
    use orgasmic_core::LifecycleStage;
    use std::collections::HashSet;
    use std::io::Write;
    use std::path::Path;

    const PROJECT_ORG: &str = "#+title: x\n#+orgasmic_version: 1\n\n* PROJECT proj-x\n:PROPERTIES:\n:ID:               proj-x\n:END:\n";
    const INITIAL_TASK: &str = "#+title: orgasmic task TASK-001\n#+orgasmic_version: 2\n\n* BACKLOG TASK-001 do :work:\n:PROPERTIES:\n:ID:               TASK-001\n:END:\n";
    const SECOND_TASK: &str = "#+title: orgasmic task TASK-002\n#+orgasmic_version: 2\n\n* BACKLOG TASK-002 next :work:\n:PROPERTIES:\n:ID:               TASK-002\n:END:\n";
    const IMPLEMENTED_TASK: &str = "#+title: orgasmic task TASK-001\n#+orgasmic_version: 2\n\n* IN_PROGRESS TASK-001 do :work:\n:PROPERTIES:\n:ID:               TASK-001\n:END:\n";

    struct ProjectFixture {
        home: Home,
        index: Index,
        bus: EventBus,
        _handle: WatcherHandle,
        orgasmic_dir: PathBuf,
        sprint: PathBuf,
    }

    async fn setup_project(
        root: &Path,
        project_root: PathBuf,
        board_path: PathBuf,
    ) -> ProjectFixture {
        let home = Home::at(root.join("home"));
        home.ensure().unwrap();
        let orgasmic_dir = project_root.join(".orgasmic");
        std::fs::create_dir_all(&orgasmic_dir).unwrap();
        std::fs::create_dir_all(orgasmic_dir.join("tasks")).unwrap();
        std::fs::write(orgasmic_dir.join("project.org"), PROJECT_ORG).unwrap();
        let sprint = orgasmic_dir.join("tasks/TASK-001/node.org");
        std::fs::create_dir_all(sprint.parent().unwrap()).unwrap();
        std::fs::write(&sprint, INITIAL_TASK).unwrap();
        std::fs::write(
            home.board(),
            format!(
                "#+title: orgasmic board\n#+orgasmic_version: 1\n\n* PROJECT proj-x\n:PROPERTIES:\n:ID:               proj-x\n:PATH:             {}\n:BRANCH:           main\n:STATUS:           active\n:END:\n",
                board_path.display()
            ),
        )
        .unwrap();

        let index = Index::new(home.clone());
        index.rebuild().await;
        let bus = EventBus::new();
        let handle = spawn(
            home.clone(),
            index.clone(),
            bus.clone(),
            WatcherConfig {
                debounce: Duration::from_millis(50),
                enabled: true,
            },
        )
        .unwrap();
        handle.watch_project(board_path).await.unwrap();
        // Settle so notify registers the directory before we touch the file.
        tokio::time::sleep(Duration::from_millis(300)).await;

        ProjectFixture {
            home,
            index,
            bus,
            _handle: handle,
            orgasmic_dir,
            sprint,
        }
    }

    async fn eventually(
        index: &Index,
        check: impl Fn(&crate::index::IndexSnapshot) -> bool,
    ) -> bool {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            let snap = index.snapshot().await;
            if check(&snap) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        false
    }

    #[tokio::test]
    async fn spawn_smoke() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let index = Index::new(home.clone());
        let bus = EventBus::new();
        let handle = spawn(home, index, bus, WatcherConfig::default()).unwrap();
        // Smoke: should be able to push a watch request.
        handle
            .watch_project(tmp.path().join("missing"))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn fs_event_triggers_project_refresh() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path().join("proj");
        let fixture = setup_project(tmp.path(), project_root.clone(), project_root.clone()).await;
        let mut sub = fixture.bus.subscribe();

        // Mutate the task node. Touch twice to
        // increase the odds of at least one event landing on slow platforms.
        std::fs::write(&fixture.sprint, IMPLEMENTED_TASK).unwrap();
        tokio::time::sleep(Duration::from_millis(120)).await;
        std::fs::write(&fixture.sprint, INITIAL_TASK).unwrap();

        let mut saw_event = false;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(250), sub.recv()).await {
                Ok(Ok(evt)) => {
                    if matches!(evt.payload, EventPayload::ProjectIndexed { .. })
                        || matches!(evt.payload, EventPayload::TaskUpdated { .. })
                    {
                        saw_event = true;
                        break;
                    }
                }
                _ => continue,
            }
        }
        assert!(saw_event, "expected ProjectIndexed or TaskUpdated event");
    }

    #[tokio::test]
    async fn fs_event_triggers_project_refresh_after_atomic_task_file_rename() {
        let tmp = tempfile::Builder::new()
            .prefix("orgasmic-watch-")
            .tempdir_in("/tmp")
            .unwrap();
        let project_root = tmp.path().join("proj");
        let fixture = setup_project(tmp.path(), project_root.clone(), project_root.clone()).await;
        let edit_project_root = std::fs::canonicalize(&project_root).unwrap();
        let edit_orgasmic_dir = edit_project_root.join(".orgasmic");

        let task_dir = edit_orgasmic_dir.join("tasks/TASK-002");
        std::fs::create_dir_all(&task_dir).unwrap();
        let tmp_path = task_dir.join("node.org.tmp");
        let mut file = std::fs::File::create(&tmp_path).unwrap();
        file.write_all(SECOND_TASK.as_bytes()).unwrap();
        file.sync_all().unwrap();
        drop(file);
        std::fs::rename(&tmp_path, task_dir.join("node.org")).unwrap();

        assert!(
            eventually(&fixture.index, |snap| snap
                .task("proj-x", "TASK-002")
                .is_some())
            .await,
            "expected atomic node.org rename to refresh project"
        );
    }

    #[tokio::test]
    async fn fs_event_triggers_project_refresh_after_cli_task_file_rename() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path().join("proj");
        let fixture = setup_project(tmp.path(), project_root.clone(), project_root).await;

        let open_sprint = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&fixture.sprint)
            .unwrap();
        let tmp_path = fixture
            .orgasmic_dir
            .join("tasks/TASK-001/node.org.00000000-0000-0000-0000-000000000000.tmp");
        std::fs::write(&tmp_path, IMPLEMENTED_TASK).unwrap();
        std::fs::rename(&tmp_path, &fixture.sprint).unwrap();
        drop(open_sprint);

        assert!(
            eventually(&fixture.index, |snap| {
                snap.task("proj-x", "TASK-001")
                    .map(|task| task.lifecycle_stage == LifecycleStage::InProgress)
                    .unwrap_or(false)
            })
            .await,
            "expected CLI-shaped node.org rename to refresh project"
        );
    }

    #[tokio::test]
    async fn changed_board_path_moves_watch_to_the_new_project_root() {
        let tmp = tempfile::tempdir().unwrap();
        let old_root = tmp.path().join("old");
        let new_root = tmp.path().join("new");
        let fixture = setup_project(tmp.path(), old_root.clone(), old_root).await;
        let new_orgasmic = new_root.join(".orgasmic");
        std::fs::create_dir_all(new_orgasmic.join("tasks/TASK-001")).unwrap();
        std::fs::write(new_orgasmic.join("project.org"), PROJECT_ORG).unwrap();
        let new_sprint = new_orgasmic.join("tasks/TASK-001/node.org");
        std::fs::write(&new_sprint, INITIAL_TASK).unwrap();
        std::fs::write(
            fixture.home.board(),
            format!(
                "#+title: orgasmic board\n#+orgasmic_version: 1\n\n* PROJECT proj-x\n:PROPERTIES:\n:ID:               proj-x\n:PATH:             {}\n:BRANCH:           next\n:STATUS:           active\n:END:\n",
                new_root.display()
            ),
        )
        .unwrap();
        let mut pending = HashSet::from([fixture.home.board()]);
        flush(
            &mut pending,
            &fixture.index,
            &fixture.home,
            &fixture.bus,
            &fixture._handle,
        )
        .await;
        assert_eq!(fixture.index.snapshot().await.board[0].path, new_root);

        // Let the command loop replace the watch before touching the new root.
        tokio::time::sleep(Duration::from_millis(300)).await;
        let second_task = new_orgasmic.join("tasks/TASK-002/node.org");
        std::fs::create_dir_all(second_task.parent().unwrap()).unwrap();
        std::fs::write(&second_task, SECOND_TASK).unwrap();

        assert!(
            eventually(&fixture.index, |snap| {
                snap.project("proj-x").is_some_and(|project| {
                    project.root == new_root
                        && project.tasks.iter().any(|task| task.id == "TASK-002")
                })
            })
            .await,
            "expected the changed registration's new root to drive watcher convergence"
        );
    }

    #[tokio::test]
    async fn temp_only_atomic_rewrite_event_refreshes_project() {
        let tmp = tempfile::Builder::new()
            .prefix("orgasmic-watch-")
            .tempdir_in("/tmp")
            .unwrap();
        let project_root = tmp.path().join("proj");
        let fixture = setup_project(tmp.path(), project_root.clone(), project_root.clone()).await;

        let second_task = project_root.join(".orgasmic/tasks/TASK-002/node.org");
        std::fs::create_dir_all(second_task.parent().unwrap()).unwrap();
        std::fs::write(&second_task, SECOND_TASK).unwrap();
        let deleted_tmp_path = project_root
            .join(".orgasmic/tasks/TASK-002")
            .join("node.org.00000000-0000-0000-0000-000000000000.tmp");
        let event = Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Any)))
            .add_path(deleted_tmp_path);
        let mut pending = HashSet::new();
        record_event(&mut pending, &event);

        flush(
            &mut pending,
            &fixture.index,
            &fixture.home,
            &fixture.bus,
            &fixture._handle,
        )
        .await;
        let snap = fixture.index.snapshot().await;
        assert!(
            snap.task("proj-x", "TASK-002").is_some(),
            "expected temp-only atomic rewrite event to refresh project"
        );
    }
}

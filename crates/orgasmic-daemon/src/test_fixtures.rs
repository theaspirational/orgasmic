//! Shared executable fixtures for daemon unit tests.
//!
//! macOS evaluates a newly-created executable through Gatekeeper on its first
//! exec. That evaluation is serialized system-wide, so creating one script per
//! parallel test turns a small idle cost into long process-start queues. Keep
//! all spawn-based unit-test behavior behind this one pre-warmed file and pass
//! per-test variation through argv or adjacent data files.
//!
//! Prefer, in order (measured 2026-07-26, see `.orgasmic/gotchas.org`):
//! 1. invoke a fresh script through a signed interpreter (`/bin/sh script`)
//! 2. reuse [`shared_test_executable`] with per-test argv
//! 3. [`link_shared_test_executable`] (inode is the cache key; a copy re-pays)

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

// orgasmic:TASK-STWVB
/// Distinct newly-created executables that paid a first-exec evaluation.
/// Test-only; dumped at process exit for the TASK-STWVB before/after report.
static FRESH_FIRST_EXECS: AtomicU64 = AtomicU64::new(0);
/// Hard-links of the shared fixture (warm path; no new inode scan).
static SHARED_LINK_REUSES: AtomicU64 = AtomicU64::new(0);
static STATS_HOOK: OnceLock<()> = OnceLock::new();

fn ensure_stats_dump_hook() {
    STATS_HOOK.get_or_init(|| {
        // Safety: registering an atexit handler that only reads atomics and
        // writes stderr is sound for this test binary.
        extern "C" fn dump() {
            let fresh = FRESH_FIRST_EXECS.load(Ordering::Relaxed);
            let links = SHARED_LINK_REUSES.load(Ordering::Relaxed);
            eprintln!(
                "orgasmic-fixture-exec-stats: fresh_first_execs={fresh} shared_link_reuses={links}"
            );
        }
        unsafe {
            libc::atexit(dump);
        }
    });
}

/// Test-only counters for TASK-STWVB part A (read by measurement probes).
#[cfg(test)]
#[allow(dead_code)]
pub(crate) fn fresh_executable_first_exec_count() -> u64 {
    FRESH_FIRST_EXECS.load(Ordering::Relaxed)
}

#[cfg(test)]
#[allow(dead_code)]
pub(crate) fn shared_link_reuse_count() -> u64 {
    SHARED_LINK_REUSES.load(Ordering::Relaxed)
}

const FIXTURE_SCRIPT: &str = r#"#!/bin/bash
# Pinned-exec wrapper link: a production regression that stops passing the
# sentinel argv must fail loudly (exit 97), not fall through to a prelude arm
# (e.g. `config` -> /bin/sleep 30). Check BEFORE the argv prelude so every
# argv value is covered (TASK-STWVB.1 / F7).
if [ -f "$0.orgasmic-pinned-wrapper" ]; then
  [ "$1" = __exec-pinned ] || exit 97
  shift
  target=$1
  shift
  shift 3
  exec "$target" "$@"
fi

mode=$1
case "$mode" in
  warm)
    exit 0
    ;;
  artifact-ready)
    shift
    while true; do
      echo '> ready'
      echo READY
      IFS= read -r line || exit 0
      echo "ECHO:$line"
    done
    ;;
  artifact-busy)
    shift
    echo BOOTING
    /bin/sleep 120
    while true; do
      echo '> ready'
      IFS= read -r line || exit 0
      echo "ECHO:$line"
    done
    ;;
  cursor-worker-sibling)
    ready=$2
    /bin/sleep 300 &
    (exec -a worker-server /bin/sleep 300) &
    /usr/bin/touch "$ready"
    /bin/sleep 3600
    ;;
  __exec-pinned)
    shift
    target=$1
    shift
    shift 3
    exec "$target" "$@"
    ;;
  config)
    # The index timeout test invokes this fixture as `git config ...`.
    exec /bin/sleep 30
    ;;
esac

# Per-link behavior when production argv cannot carry a fixture mode (mux
# probes pass `-V`, resume passes provider flags). Adjacent files keep the
# inode shared; only the data files vary per test.
fixture_mode_file="$0.orgasmic-mode"
if [ -f "$fixture_mode_file" ]; then
  fixture_mode=$(/bin/cat "$fixture_mode_file")
  case "$fixture_mode" in
    hanging-mux)
      version=$(/bin/cat "$0.mux-version")
      armed=$(/bin/cat "$0.mux-armed")
      pidfile=$(/bin/cat "$0.mux-pidfile")
      for a in "$@"; do
        [ "$a" = -V ] && { echo "rmux $version"; exit 0; }
      done
      [ -f "$armed" ] || exit 0
      echo $$ > "$pidfile"
      exec /bin/sleep 120
      ;;
    malicious-claude)
      log=$(/bin/cat "$0.malicious-log")
      printf '%s\n' "$0 $*" > "$log"
      exec /usr/bin/true
      ;;
    claude-capture)
      trusted_log=$(/bin/cat "$0.trusted-log")
      projects_dir=$(/bin/cat "$0.projects-dir")
      fork_id=$(/bin/cat "$0.fork-id")
      cwd=$(/bin/cat "$0.cwd")
      printf '%s\n' "$0 $*" > "$trusted_log"
      previous=
      resumed=
      for arg in "$@"; do
        [ "$previous" != --resume ] || resumed=$arg
        previous=$arg
      done
      /bin/mkdir -p "$projects_dir"
      printf '{"sessionId":"%s","cwd":"%s","forkedFrom":{"sessionId":"%s"}}\n' \
        "$fork_id" "$cwd" "$resumed" > "$projects_dir/$fork_id.jsonl"
      exec /bin/sleep 600
      ;;
    claude-chain)
      argv_log=$(/bin/cat "$0.argv-log")
      counter=$(/bin/cat "$0.counter")
      projects_dir=$(/bin/cat "$0.projects-dir")
      cwd=$(/bin/cat "$0.cwd")
      n=0
      [ ! -f "$counter" ] || n=$(/bin/cat "$counter")
      n=$((n + 1))
      printf '%s\n' "$n" > "$counter"
      if [ "$n" -eq 1 ]; then
        fork=fork-chain-first
      else
        fork=fork-chain-second
      fi
      previous=
      resumed=
      for arg in "$@"; do
        [ "$previous" != --resume ] || resumed=$arg
        previous=$arg
      done
      printf '%s\n' "$0 $*" >> "$argv_log"
      /bin/mkdir -p "$projects_dir"
      printf '{"sessionId":"%s","cwd":"%s","forkedFrom":{"sessionId":"%s"}}\n' \
        "$fork" "$cwd" "$resumed" > "$projects_dir/$fork.jsonl"
      /bin/sleep 2
      exit 42
      ;;
  esac
fi

# The simple trusted-Claude fixture only needs to be executable.
exit 0
"#;

static SHARED_EXECUTABLE: OnceLock<PathBuf> = OnceLock::new();

fn fixture_content_tag() -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    FIXTURE_SCRIPT.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// Return the one executable fixture for this daemon unit-test binary.
///
/// The on-disk name includes a content hash of [`FIXTURE_SCRIPT`], so editing
/// the script cannot leave a stale file at the old path. A leftover file at
/// this path with the wrong contents is overwritten.
pub(crate) fn shared_test_executable() -> &'static Path {
    ensure_stats_dump_hook();
    SHARED_EXECUTABLE
        .get_or_init(|| {
            let test_binary = std::env::current_exe().expect("resolve daemon test binary");
            let file_name = test_binary
                .file_name()
                .expect("daemon test binary has a file name")
                .to_string_lossy();
            let tag = fixture_content_tag();
            let path = test_binary.with_file_name(format!("{file_name}.fixture-{tag}.sh"));

            let needs_write = match std::fs::read_to_string(&path) {
                Ok(existing) => existing != FIXTURE_SCRIPT,
                Err(_) => true,
            };
            if needs_write {
                let parent = path.parent().expect("daemon test binary has a parent");
                let mut pending =
                    tempfile::NamedTempFile::new_in(parent).expect("create shared test fixture");
                pending
                    .write_all(FIXTURE_SCRIPT.as_bytes())
                    .expect("write shared test fixture");
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt as _;
                    std::fs::set_permissions(
                        pending.path(),
                        std::fs::Permissions::from_mode(0o755),
                    )
                    .expect("make shared test fixture executable");
                }
                // Replace atomically when possible. On persist race, rewrite via
                // a fresh temp file + rename — never truncate the shared inode
                // in place (every converted site hard-links it; TASK-STWVB.1/F8).
                match pending.persist(&path) {
                    Ok(_) => {
                        FRESH_FIRST_EXECS.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(error) => {
                        let mut retry = tempfile::NamedTempFile::new_in(parent)
                            .expect("retry shared test fixture temp");
                        retry
                            .write_all(FIXTURE_SCRIPT.as_bytes())
                            .expect("write retry shared test fixture");
                        #[cfg(unix)]
                        {
                            use std::os::unix::fs::PermissionsExt as _;
                            std::fs::set_permissions(
                                retry.path(),
                                std::fs::Permissions::from_mode(0o755),
                            )
                            .expect("chmod retry shared test fixture");
                        }
                        retry.persist(&path).unwrap_or_else(|persist_error| {
                            panic!(
                                "overwrite shared test fixture at {}: persist failed ({error}); \
                                 retry rename failed ({persist_error})",
                                path.display()
                            )
                        });
                        FRESH_FIRST_EXECS.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }

            assert_eq!(
                std::fs::read_to_string(&path).expect("read shared test fixture"),
                FIXTURE_SCRIPT,
                "shared test fixture must match its test binary"
            );
            let status = Command::new(&path)
                .arg("warm")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .expect("pre-warm shared test fixture");
            assert!(status.success(), "shared test fixture pre-warm failed");
            path
        })
        .as_path()
}

/// Put the shared executable at a production-shaped test path without creating
/// a new file (and therefore without another first-exec evaluation).
///
/// Set `ORGASMIC_DISABLE_SHARED_FIXTURE_LINKS=1` to force a fresh byte-identical
/// copy instead (TASK-STWVB.1 / F9 settling probe). Test-only kill switch.
pub(crate) fn link_shared_test_executable(path: &Path) {
    ensure_stats_dump_hook();
    if std::env::var_os("ORGASMIC_DISABLE_SHARED_FIXTURE_LINKS").is_some() {
        // Byte-identical copy: pays a first-exec scan. Same contents as the
        // shared fixture so behavior is unchanged; only the inode differs.
        let contents = std::fs::read(shared_test_executable())
            .expect("read shared fixture for kill-switch copy");
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create kill-switch fixture parent");
        }
        std::fs::write(path, contents).unwrap_or_else(|error| {
            panic!(
                "write kill-switch fresh fixture at {}: {error}",
                path.display()
            )
        });
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap_or_else(
                |error| {
                    panic!(
                        "chmod kill-switch fresh fixture at {}: {error}",
                        path.display()
                    )
                },
            );
        }
        FRESH_FIRST_EXECS.fetch_add(1, Ordering::Relaxed);
        return;
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create shared fixture link parent");
    }
    let staged = path.with_extension(format!("fixture-link-{}", uuid::Uuid::new_v4()));
    std::fs::hard_link(shared_test_executable(), &staged).unwrap_or_else(|error| {
        panic!(
            "stage shared test fixture link for {}: {error}",
            path.display()
        )
    });
    std::fs::rename(&staged, path)
        .unwrap_or_else(|error| panic!("link shared test fixture at {}: {error}", path.display()));
    SHARED_LINK_REUSES.fetch_add(1, Ordering::Relaxed);
}

/// Write a brand-new executable and count it as a first-exec cost.
///
/// Prefer [`link_shared_test_executable`] or a signed interpreter. This exists
/// so remaining sites stay visible in the TASK-STWVB fixture-exec stats.
#[cfg(test)]
pub(crate) fn create_fresh_test_executable(path: &Path, contents: &str) {
    ensure_stats_dump_hook();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create fresh fixture parent");
    }
    std::fs::write(path, contents)
        .unwrap_or_else(|error| panic!("write fresh test executable {}: {error}", path.display()));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap_or_else(
            |error| panic!("chmod fresh test executable {}: {error}", path.display()),
        );
    }
    FRESH_FIRST_EXECS.fetch_add(1, Ordering::Relaxed);
}

/// Write one per-test value next to a linked executable. Recovery tests cannot
/// prepend fixture-only argv because they exercise production resume argv, so
/// the shared script reads these values using its invoked hard-link path.
pub(crate) fn write_linked_fixture_value(path: &Path, name: &str, value: &Path) {
    std::fs::write(
        format!("{}.{}", path.display(), name),
        value.as_os_str().as_encoded_bytes(),
    )
    .unwrap_or_else(|error| {
        panic!(
            "write shared fixture value {name} for {}: {error}",
            path.display()
        )
    });
}

pub(crate) fn write_linked_fixture_text(path: &Path, name: &str, value: &str) {
    std::fs::write(format!("{}.{}", path.display(), name), value).unwrap_or_else(|error| {
        panic!(
            "write shared fixture value {name} for {}: {error}",
            path.display()
        )
    });
}

/// Install the shared fixture as a hanging mux binary for stall-probe tests.
///
/// Per-test paths live in adjacent files so every arm reuses one warmed inode.
#[cfg(test)]
pub(crate) fn link_hanging_mux_shim(path: &Path, version: &str, armed: &Path, pidfile: &Path) {
    link_shared_test_executable(path);
    write_linked_fixture_text(path, "orgasmic-mode", "hanging-mux");
    write_linked_fixture_text(path, "mux-version", version);
    write_linked_fixture_value(path, "mux-armed", armed);
    write_linked_fixture_value(path, "mux-pidfile", pidfile);
}

// orgasmic:task_BCYMM
/// A fixture process spawned in its own process group, whose whole tree is
/// reaped when the handle drops.
///
/// Several fixture arms outlive their foreground command on purpose
/// (`cursor-worker-sibling` backgrounds two sleeps and then sleeps an hour;
/// `artifact-busy` sleeps 120s). Killing those as a *trailing statement* in the
/// test body is skipped by every panic above it, and TASK-BCYMM measured the
/// result: fixture trees reparented to init, still holding `/bin/sleep 3600`
/// 40 minutes later, executing from worktrees that had already been removed.
/// This is the process-side answer to the same problem TASK-Z3093 solved for
/// rmux sessions — ownership registered at spawn, reaped on `Drop`, which runs
/// on the unwind path.
///
/// Spawning in a fresh group (`process_group(0)`, so the child's pid *is* the
/// group id) is what makes the tree reapable: signalling only the foreground
/// pid orphans the backgrounded children to init.
#[cfg(unix)]
pub(crate) struct FixtureProcess {
    child: std::process::Child,
    group: orgasmic_drivers::modes::rmux::test_tooling::OwnedProcessGroup,
}

#[cfg(unix)]
impl FixtureProcess {
    /// Pid of the spawned process, which is also its process-group id.
    pub(crate) fn id(&self) -> u32 {
        self.child.id()
    }
}

#[cfg(unix)]
impl Drop for FixtureProcess {
    fn drop(&mut self) {
        // Group first, direct child second: the group reap is what stops the
        // backgrounded children, and the `wait` afterwards collects the
        // leader's zombie so the test binary does not accumulate them.
        self.group.reap();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Spawn `command` in its own process group behind a [`FixtureProcess`].
///
/// Use this for any fixture whose foreground command outlives the assertions
/// that follow it. Configure stdio on `command` as usual — null or piped, never
/// inherited, so a backgrounded child cannot hold a piped `cargo test | tail`
/// open past test completion.
#[cfg(unix)]
pub(crate) fn spawn_in_own_process_group(command: &mut Command, what: &str) -> FixtureProcess {
    use std::os::unix::process::CommandExt as _;

    let child = command
        .process_group(0)
        .spawn()
        .unwrap_or_else(|error| panic!("spawn {what}: {error}"));
    // Register before returning: the caller must never hold an unowned tree,
    // not even for the one statement it would take to register it itself.
    let group = orgasmic_drivers::modes::rmux::test_tooling::owned_process_group(child.id());
    FixtureProcess { child, group }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// Pids in `pgid`'s process group that are still alive and not zombies,
    /// with their command lines — so a failure names the survivor rather than
    /// just counting it.
    fn live_group_members(pgid: u32) -> Vec<String> {
        // `ps -g` selects by session leader on BSD/macOS, not by process group,
        // so select everything and filter on the `pgid` column here.
        let output = Command::new("ps")
            .args(["-A", "-o", "pid=,pgid=,stat=,command="])
            .stdin(Stdio::null())
            .output()
            .expect("ps process table");
        let wanted = pgid.to_string();
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::trim)
            .filter(|line| {
                let mut fields = line.split_whitespace();
                let (_pid, group, stat) = (fields.next(), fields.next(), fields.next());
                // A zombie is already reaped as far as this task is concerned:
                // it holds no `sleep`, executes nothing, and disappears with
                // the test binary. The leak being fixed is a RUNNING orphan.
                group == Some(wanted.as_str()) && !stat.is_some_and(|s| s.starts_with('Z'))
            })
            .map(ToOwned::to_owned)
            .collect()
    }

    // orgasmic:TASK-STWVB.1
    /// Positive control for the malicious-claude arm: exec the linked shim and
    /// assert the side-channel log appears. Without this, the only consumer
    /// asserts the shim never runs, and `/bin/true` vs `/usr/bin/true` can rot
    /// into vacuity again (F6).
    #[test]
    fn malicious_claude_shim_writes_log_and_exits_cleanly() {
        let tmp = tempfile::tempdir().expect("malicious shim tempdir");
        let shim = tmp.path().join("claude");
        let log = tmp.path().join("malicious-argv.log");
        link_shared_test_executable(&shim);
        write_linked_fixture_text(&shim, "orgasmic-mode", "malicious-claude");
        write_linked_fixture_value(&shim, "malicious-log", &log);
        let status = Command::new(&shim)
            .args(["--resume", "sess-1", "-p", "hello"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("exec malicious shim");
        assert!(
            status.success(),
            "malicious-claude arm must exec cleanly (exit 0), got {status}"
        );
        let body = std::fs::read_to_string(&log).expect("malicious log must exist");
        assert!(
            body.contains("--resume"),
            "malicious log must capture argv: {body}"
        );
    }

    // orgasmic:TASK-STWVB.1
    /// The exit-97 sentinel must fire for prelude argv values, not only for
    /// unknown ones. `config` is the hang-risk shape (would otherwise sleep 30).
    #[test]
    fn pinned_wrapper_sentinel_fires_for_prelude_argv() {
        let tmp = tempfile::tempdir().expect("pinned wrapper tempdir");
        let wrapper = tmp.path().join("orgasmic");
        link_shared_test_executable(&wrapper);
        write_linked_fixture_text(&wrapper, "orgasmic-pinned-wrapper", "1");
        let status = Command::new(&wrapper)
            .arg("config")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("exec pinned wrapper with prelude argv");
        assert_eq!(
            status.code(),
            Some(97),
            "pinned-wrapper sentinel must exit 97 for prelude argv, got {status}"
        );
    }

    // orgasmic:task_BCYMM
    /// The acceptance: a test body that panics *between* spawning a fixture
    /// tree and cleaning it up must still leave nothing behind.
    ///
    /// `cursor-worker-sibling` is the arm TASK-BCYMM measured: it backgrounds
    /// two `sleep 300`s and then sleeps an hour in the foreground, so nothing
    /// in the tree exits on its own and killing only the foreground pid orphans
    /// the rest to init. The panic here stands in for the real one — the
    /// `fake cursor-agent did not start children` deadline in
    /// `supervisor::tests::poll_direct_child_pid_prefers_worker_server_over_generic_sibling`,
    /// which fires under load and used to skip that test's trailing kill.
    #[test]
    fn panicking_body_still_reaps_the_whole_fixture_process_group() {
        let tmp = tempfile::tempdir().expect("fixture tempdir");
        let ready = tmp.path().join("children-ready");
        let observed = std::sync::Arc::new(std::sync::Mutex::new(None));

        let recorder = std::sync::Arc::clone(&observed);
        let unwound = std::panic::catch_unwind(move || {
            let fixture = spawn_in_own_process_group(
                Command::new(shared_test_executable())
                    .args(["cursor-worker-sibling", ready.to_str().unwrap()])
                    .stdin(Stdio::piped())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null()),
                "cursor-worker-sibling leak probe",
            );
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            while !ready.exists() {
                assert!(
                    std::time::Instant::now() < deadline,
                    "fixture did not start its children"
                );
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            let members = live_group_members(fixture.id());
            assert!(
                members.len() >= 3,
                "expected the fixture, its generic sibling and its worker-server \
                 sibling to be live before the panic, got {members:?}"
            );
            *recorder.lock().unwrap() = Some(fixture.id());
            // Stands in for any assertion that can fail above the cleanup.
            panic!("simulated mid-body failure");
        });
        assert!(unwound.is_err(), "the probe body must have panicked");

        let pgid = observed
            .lock()
            .unwrap()
            .take()
            .expect("probe recorded its process group");
        // The reap is synchronous in `Drop`, but the members' own exits are
        // not: allow the group a moment to actually leave the process table
        // before calling it a leak.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut survivors = live_group_members(pgid);
        while !survivors.is_empty() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(50));
            survivors = live_group_members(pgid);
        }
        assert!(
            survivors.is_empty(),
            "fixture process group {pgid} survived the panicking body; still alive:\n{}",
            survivors.join("\n")
        );
    }
}

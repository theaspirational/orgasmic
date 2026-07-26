//! Shared executable fixtures for daemon unit tests.
//!
//! macOS evaluates a newly-created executable through Gatekeeper on its first
//! exec. That evaluation is serialized system-wide, so creating one script per
//! parallel test turns a small idle cost into long process-start queues. Keep
//! all spawn-based unit-test behavior behind this one pre-warmed file and pass
//! per-test variation through argv or adjacent data files.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;

const FIXTURE_SCRIPT: &str = r#"#!/bin/bash
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

fixture_mode_file="$0.orgasmic-mode"
if [ -f "$fixture_mode_file" ]; then
  fixture_mode=$(/bin/cat "$fixture_mode_file")
  case "$fixture_mode" in
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

/// Return the one executable fixture for this daemon unit-test binary.
///
/// The script lives beside the hashed test binary, so recompilation naturally
/// gives changed fixture contents a new path. `persist_noclobber` makes setup
/// safe if the same test binary is started concurrently.
pub(crate) fn shared_test_executable() -> &'static Path {
    SHARED_EXECUTABLE
        .get_or_init(|| {
            let test_binary = std::env::current_exe().expect("resolve daemon test binary");
            let file_name = test_binary
                .file_name()
                .expect("daemon test binary has a file name")
                .to_string_lossy();
            let path = test_binary.with_file_name(format!("{file_name}.fixture.sh"));

            if !path.exists() {
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
                match pending.persist_noclobber(&path) {
                    Ok(_) => {}
                    Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(error) => panic!("publish shared test fixture: {}", error.error),
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
pub(crate) fn link_shared_test_executable(path: &Path) {
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

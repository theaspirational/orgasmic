// orgasmic:TASK-TCTTD
//! `orgasmic verify TASK-<id>` — replay a shipped injection proof.
//!
//! An injection proof used to be a ritual: the manager re-authored a probe at
//! merge time, against the tree that was *already fixed*. That is precisely
//! where false green lives — a probe that does not bite is indistinguishable
//! from a fix that works, and four such probes shipped in one session before
//! anyone noticed.
//!
//! This module replaces the ritual with replay. The implementer authors the
//! proof once, while the defect still reproduces, and commits it as an
//! artifact next to the fix. Everything afterwards is `git apply`, run, assert
//! the pinned red, revert, run, assert green. A probe that has silently stopped
//! biting fails the replay itself, loudly, instead of passing as a fix.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use anyhow::{bail, Context, Result};
use clap::Args;

/// The drivers-suite test that spawns a real provider turn and bills real
/// money. Every stored command touching that suite must skip it, and this
/// module refuses to run one that does not.
const BILLED_DRIVERS_TEST: &str = "legacy_drivers_and_explicit_pairs_emit_equivalent_start_events";
const BILLED_DRIVERS_SKIP: &str =
    "--skip legacy_drivers_and_explicit_pairs_emit_equivalent_start_events";
const DRIVERS_PACKAGE: &str = "orgasmic-drivers";

/// Run-id and home leak into subprocesses inside a dispatch and break the
/// suites a replay is trying to reproduce. Scrubbed from the child regardless
/// of what the stored command says.
const SCRUBBED_ENV: &[&str] = &["ORGASMIC_RUN_ID", "ORGASMIC_HOME"];

/// The daemon writes here continuously, and no injection patch ever touches it,
/// so its churn must not make a repo permanently unverifiable.
const DAEMON_OWNED_PREFIX: &str = ".orgasmic/";

pub const VERIFY_AFTER_HELP: &str = "\
Artifact layout (default `<repo>/verify/<TASK-ID>/`, override with --artifact):

  injection.patch   git patch that REINTRODUCES the defect onto the fixed tree.
                    Authored while the defect still reproduced — normally the
                    reverse of the fix. Applied with `git apply`; if it no
                    longer applies, verify FAILS as a stale artifact.

  cmd               Exactly one shell command line (blank lines and `#`
                    comments ignored), run from the repo root. Prefix cargo
                    runs with `env -u ORGASMIC_RUN_ID -u ORGASMIC_HOME`; any
                    command touching the drivers suite MUST carry
                    `-- --skip legacy_drivers_and_explicit_pairs_emit_equivalent_start_events`
                    or verify refuses to run it.

  expect-red        The pinned failure signature. Directives, one per line:
                      exit: nonzero        (default) any failing exit
                      exit: <n>            that exact exit code
                      contains: <text>     must appear in the output (repeatable,
                                           ALL must match)
                      contains-any: <text> repeatable; at least ONE must match.
                                           Use for a probe with more than one
                                           legitimate failure mode.
                    At least one contains/contains-any is required: a signature
                    that matches any failure is not a signature.

Replay: assert clean tree -> apply patch -> run cmd -> assert RED matching the
signature -> revert patch -> assert tree byte-identical -> run cmd -> assert
GREEN. Any deviation, including a red run whose output does not match the
pinned signature, exits nonzero.

--all replays every directory under <repo>/verify/ and prints one line per
artifact plus `N/M pass`; --check only runs `git apply --check` on each
injection.patch (no build, no test, under a second) and prints
`<id> replayable|STALE (<git apply error>)` plus `N/M replayable`. Both exit
nonzero when any artifact fails. --json emits one object per artifact and,
with --all, a final `{total, ok, failed}` line.

Examples:
  orgasmic verify TASK-R74E8
  orgasmic verify TASK-R74E8 --json
  orgasmic verify TASK-R74E8 --artifact /tmp/candidate-artifact
  orgasmic verify --all --check
  orgasmic verify --all --json";

#[derive(Args, Debug)]
pub struct VerifyArgs {
    /// Task id whose injection proof to replay (e.g. TASK-R74E8).
    #[arg(required_unless_present = "all")]
    pub task: Option<String>,
    /// Artifact directory; defaults to `<repo>/verify/<TASK-ID>`.
    #[arg(long)]
    pub artifact: Option<PathBuf>,
    /// Emit a machine-readable result instead of narrating.
    #[arg(long)]
    pub json: bool,
    /// Every artifact under `<repo>/verify/`, one line each, nonzero if any fails.
    #[arg(long, conflicts_with_all = ["task", "artifact"])]
    pub all: bool,
    /// Only check that injection.patch still applies; run nothing.
    #[arg(long)]
    pub check: bool,
}

pub fn cmd_verify(args: VerifyArgs) -> Result<()> {
    let cwd = std::env::current_dir().context("resolve current directory")?;
    let repo = git_toplevel(&cwd)?;
    let targets = if args.all {
        artifacts_under(&repo.join("verify"))?
    } else {
        let task = args.task.expect("clap requires a task without --all");
        let dir = args
            .artifact
            .unwrap_or_else(|| repo.join("verify").join(&task));
        vec![(task, dir)]
    };
    let mode = if args.check {
        Mode::Check
    } else {
        Mode::Replay
    };
    run_targets(&repo, &targets, mode, args.json, args.all)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// `git apply --check` only: is the proof still replayable?
    Check,
    /// The full red-then-green replay.
    Replay,
}

/// Every `<verify>/<id>/` directory, sorted. Files (README, registry) are not
/// artifacts; a directory without an injection.patch is one that fails.
fn artifacts_under(verify: &Path) -> Result<Vec<(String, PathBuf)>> {
    let mut out: Vec<(String, PathBuf)> = std::fs::read_dir(verify)
        .with_context(|| format!("read {}", verify.display()))?
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().is_dir())
        .map(|entry| {
            (
                entry.file_name().to_string_lossy().to_string(),
                entry.path(),
            )
        })
        .collect();
    out.sort();
    Ok(out)
}

/// Run every target; one line per artifact when sweeping or checking, the
/// full narration for a single replay. Nonzero when any target fails.
fn run_targets(
    repo: &Path,
    targets: &[(String, PathBuf)],
    mode: Mode,
    json: bool,
    summary: bool,
) -> Result<()> {
    let terse = summary || mode == Mode::Check;
    let (ok_word, bad_word) = match mode {
        Mode::Check => ("replayable", "STALE"),
        Mode::Replay => ("pass", "FAIL"),
    };
    let mut ok = 0;
    for (task, dir) in targets {
        let result = match mode {
            Mode::Check => Artifact::load(dir)
                .and_then(|artifact| check_patch(repo, &artifact.patch))
                .map(|()| None),
            Mode::Replay => replay(repo, task, dir, json || terse).map(Some),
        };
        match result {
            Ok(report) => {
                ok += 1;
                if json {
                    println!(
                        "{}",
                        match report {
                            Some(report) => report.to_json(task, dir),
                            None => serde_json::json!({
                                "task": task,
                                "artifact": dir.display().to_string(),
                                "result": ok_word,
                            })
                            .to_string(),
                        }
                    );
                } else if terse {
                    println!("{task} {ok_word}");
                }
            }
            Err(err) => {
                let reason = format!("{err:#}");
                if json {
                    println!(
                        "{}",
                        serde_json::json!({
                            "task": task,
                            "artifact": dir.display().to_string(),
                            "result": bad_word.to_lowercase(),
                            "reason": reason,
                        })
                    );
                } else if terse {
                    println!(
                        "{task} {bad_word} ({})",
                        reason.lines().next().unwrap_or("")
                    );
                } else {
                    return Err(err);
                }
            }
        }
    }
    let total = targets.len();
    if summary {
        if json {
            println!(
                "{}",
                serde_json::json!({ "total": total, "ok": ok, "failed": total - ok })
            );
        } else {
            println!("{ok}/{total} {ok_word}");
        }
    }
    if ok < total {
        bail!(
            "{} of {total} verify artifact(s) {}",
            total - ok,
            match mode {
                Mode::Check => "no longer apply to this tree",
                Mode::Replay => "failed to replay",
            }
        );
    }
    Ok(())
}

/// One replay phase's observed result.
#[derive(Debug)]
struct PhaseOutcome {
    exit: Option<i32>,
    output: String,
    log: PathBuf,
}

#[derive(Debug)]
struct Report {
    command: String,
    red: PhaseOutcome,
    green: PhaseOutcome,
}

impl Report {
    fn to_json(&self, task: &str, dir: &Path) -> String {
        serde_json::json!({
            "task": task,
            "artifact": dir.display().to_string(),
            "command": self.command,
            "red": {
                "exit": self.red.exit,
                "log": self.red.log.display().to_string(),
            },
            "green": {
                "exit": self.green.exit,
                "log": self.green.log.display().to_string(),
            },
            "result": "pass",
        })
        .to_string()
    }
}

fn replay(repo: &Path, task: &str, dir: &Path, quiet: bool) -> Result<Report> {
    let artifact = Artifact::load(dir)?;
    let mut say = Narrator { quiet };

    say.line(&format!("verify {task}"));
    say.line(&format!("  artifact  {}", dir.display()));
    say.line(&format!("  command   {}", artifact.cmd));

    let dirty = dirty_paths(repo)?;
    if !dirty.is_empty() {
        bail!(
            "refusing to replay on a dirty tree — verify applies and reverts a patch \
             and must leave the tree byte-identical.\n  dirty:\n    {}",
            dirty.join("\n    ")
        );
    }
    say.line("  [tree]    clean");

    apply_patch(repo, &artifact.patch, Direction::Forward).with_context(|| {
        format!(
            "stale artifact: {} no longer applies to this tree. The proof must be \
             re-authored against a reproducing defect, not skipped",
            artifact.patch.display()
        )
    })?;
    let mut guard = InjectionGuard {
        repo,
        patch: &artifact.patch,
        armed: true,
    };
    say.line("  [inject]  injection.patch applied");

    let red = run_command(repo, &artifact.cmd, task, "red")?;
    let mismatches = artifact.signature.mismatches(red.exit, &red.output);
    if !mismatches.is_empty() {
        // Restore before reporting: a failed replay must not leave the defect
        // in the tree, and the operator reads the reason afterwards either way.
        guard.disarm_and_revert()?;
        bail!(
            "FALSE GREEN GUARD TRIPPED — the injected defect did not produce the pinned \
             red for {task}.\n  {}\n\n  This means one of: the injection patch no longer \
             reintroduces the defect, the command no longer exercises it, or the pinned \
             signature is wrong. Do NOT treat the fix as verified.\n  red log: {}",
            mismatches.join("\n  "),
            red.log.display()
        );
    }
    say.line(&format!(
        "  [red]     as pinned — exit {}, signature matched\n            log: {}",
        describe_exit(red.exit),
        red.log.display()
    ));

    guard.disarm_and_revert()?;
    let dirty = dirty_paths(repo)?;
    if !dirty.is_empty() {
        bail!(
            "reverting the injection did not restore the tree byte-identically:\n    {}",
            dirty.join("\n    ")
        );
    }
    say.line("  [revert]  reverted; tree byte-identical");

    let green = run_command(repo, &artifact.cmd, task, "green")?;
    if green.exit != Some(0) {
        bail!(
            "the fixed tree is not green: `{}` exited {}.\n  green log: {}",
            artifact.cmd,
            describe_exit(green.exit),
            green.log.display()
        );
    }
    say.line(&format!(
        "  [green]   passes without the injection — exit 0\n            log: {}",
        green.log.display()
    ));
    say.line(&format!(
        "verify {task}: PASS — red-then-green replay reproduced"
    ));

    Ok(Report {
        command: artifact.cmd,
        red,
        green,
    })
}

struct Narrator {
    quiet: bool,
}

impl Narrator {
    fn line(&mut self, text: &str) {
        if !self.quiet {
            println!("{text}");
        }
    }
}

// ---------------------------------------------------------------------------
// artifact
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct Artifact {
    patch: PathBuf,
    cmd: String,
    signature: RedSignature,
}

impl Artifact {
    fn load(dir: &Path) -> Result<Self> {
        if !dir.is_dir() {
            bail!(
                "no verify artifact at {}. An injection proof is authored pre-fix and \
                 committed with the fix; there is nothing to replay",
                dir.display()
            );
        }
        let patch = dir.join("injection.patch");
        if !patch.is_file() {
            bail!("missing {}", patch.display());
        }
        let cmd_path = dir.join("cmd");
        let cmd = parse_cmd(&read(&cmd_path)?, &cmd_path)?;
        guard_billed_test(&cmd, &cmd_path)?;
        let expect_path = dir.join("expect-red");
        let signature = RedSignature::parse(&read(&expect_path)?, &expect_path)?;
        Ok(Self {
            patch,
            cmd,
            signature,
        })
    }
}

fn read(path: &Path) -> Result<String> {
    std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))
}

fn meaningful_lines(raw: &str) -> Vec<&str> {
    raw.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .collect()
}

fn parse_cmd(raw: &str, path: &Path) -> Result<String> {
    let lines = meaningful_lines(raw);
    match lines.as_slice() {
        [one] => Ok((*one).to_string()),
        [] => bail!("{} has no command line", path.display()),
        many => bail!(
            "{} has {} command lines; the artifact stores exactly one so the replay \
             and a human reproduce the same thing",
            path.display(),
            many.len()
        ),
    }
}

/// Refuse any stored command that could spend real money.
///
/// Two ways to get billed: name the harness test outside a `--skip`, or run the
/// drivers suite without skipping it at all.
fn guard_billed_test(cmd: &str, path: &Path) -> Result<()> {
    let without_skip = cmd.replace(BILLED_DRIVERS_SKIP, "");
    if without_skip.contains(BILLED_DRIVERS_TEST) {
        bail!(
            "{} names the billed harness test `{BILLED_DRIVERS_TEST}` outside a \
             `--skip`. That test spawns a real provider turn and bills real money",
            path.display()
        );
    }
    if cmd.contains(DRIVERS_PACKAGE) && !cmd.contains(BILLED_DRIVERS_SKIP) {
        bail!(
            "{} runs the drivers suite without `{BILLED_DRIVERS_SKIP}`. That suite \
             contains the billed harness test; the skip is mandatory",
            path.display()
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// red signature
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
enum ExpectedExit {
    NonZero,
    Exactly(i32),
}

#[derive(Debug)]
struct RedSignature {
    exit: ExpectedExit,
    all: Vec<String>,
    any: Vec<String>,
}

impl RedSignature {
    fn parse(raw: &str, path: &Path) -> Result<Self> {
        let mut exit = ExpectedExit::NonZero;
        let mut all = Vec::new();
        let mut any = Vec::new();
        for line in meaningful_lines(raw) {
            let Some((key, value)) = line.split_once(':') else {
                bail!(
                    "{}: `{line}` is not a `<directive>: <value>` line",
                    path.display()
                );
            };
            let value = value.trim();
            match key.trim() {
                "exit" => {
                    exit = if value == "nonzero" {
                        ExpectedExit::NonZero
                    } else {
                        ExpectedExit::Exactly(value.parse().with_context(|| {
                            format!(
                                "{}: `exit: {value}` is not `nonzero` or an integer",
                                path.display()
                            )
                        })?)
                    };
                }
                "contains" => all.push(value.to_string()),
                "contains-any" => any.push(value.to_string()),
                other => bail!(
                    "{}: unknown directive `{other}`. Known: exit, contains, contains-any",
                    path.display()
                ),
            }
        }
        if all.is_empty() && any.is_empty() {
            bail!(
                "{} pins no output at all. A signature that accepts any failure is not \
                 a signature — it is the false green this verb exists to catch",
                path.display()
            );
        }
        if exit == ExpectedExit::Exactly(0) {
            bail!(
                "{} expects exit 0 from the injected tree. The injection must fail",
                path.display()
            );
        }
        Ok(Self { exit, all, any })
    }

    /// Human-readable reasons the observed run is not the pinned red. Empty
    /// means it matched.
    fn mismatches(&self, exit: Option<i32>, output: &str) -> Vec<String> {
        let mut out = Vec::new();
        // Checked ahead of the declared expectation so the headline reason is
        // always the one that matters: the probe did not bite.
        if exit == Some(0) {
            out.push(
                "the injected tree exited 0 — the command PASSED with the defect \
                 reintroduced, so it does not detect the defect"
                    .to_string(),
            );
        } else if let ExpectedExit::Exactly(want) = self.exit {
            if exit != Some(want) {
                out.push(format!(
                    "expected exit {want}, observed {}",
                    describe_exit(exit)
                ));
            }
        }
        for needle in &self.all {
            if !output.contains(needle.as_str()) {
                out.push(format!("output does not contain `{needle}`"));
            }
        }
        if !self.any.is_empty() && !self.any.iter().any(|n| output.contains(n.as_str())) {
            out.push(format!(
                "output contains none of the expected alternatives: {}",
                self.any
                    .iter()
                    .map(|n| format!("`{n}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        out
    }
}

fn describe_exit(exit: Option<i32>) -> String {
    match exit {
        Some(code) => code.to_string(),
        None => "killed by signal".to_string(),
    }
}

// ---------------------------------------------------------------------------
// git + process
// ---------------------------------------------------------------------------

fn git(repo: &Path, args: &[&str]) -> Result<Output> {
    Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .with_context(|| format!("run git {}", args.join(" ")))
}

fn git_toplevel(cwd: &Path) -> Result<PathBuf> {
    let output = git(cwd, &["rev-parse", "--show-toplevel"])?;
    if !output.status.success() {
        bail!(
            "not inside a git worktree ({}): verify replays a patch against a repo",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let path = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim().to_string());
    std::fs::canonicalize(&path).with_context(|| format!("canonicalize {}", path.display()))
}

/// Working-tree paths that are neither committed nor daemon-owned, as
/// `git status --porcelain` lines. Shared with `manager dispatch` and
/// `dispatch-status` (TASK-GCTMA, TASK-EXN3N).
pub(crate) fn dirty_paths(repo: &Path) -> Result<Vec<String>> {
    let output = git(repo, &["status", "--porcelain"])?;
    if !output.status.success() {
        bail!(
            "git status failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| line.len() > 3)
        .filter(|line| !line[3..].trim_matches('"').starts_with(DAEMON_OWNED_PREFIX))
        .map(|line| line.to_string())
        .collect())
}

#[derive(Clone, Copy)]
enum Direction {
    Forward,
    Reverse,
}

fn apply_patch(repo: &Path, patch: &Path, direction: Direction) -> Result<()> {
    let flags: &[&str] = match direction {
        Direction::Forward => &[],
        Direction::Reverse => &["--reverse"],
    };
    git_apply(repo, patch, flags).map(|_| ())
}

/// Would the patch still apply? Touches nothing; the error is git's first
/// stderr line (`error: patch failed: <file>:<line>`), which names what moved.
fn check_patch(repo: &Path, patch: &Path) -> Result<()> {
    git_apply(repo, patch, &["--check"]).map(|_| ())
}

fn git_apply(repo: &Path, patch: &Path, flags: &[&str]) -> Result<()> {
    let patch = patch
        .canonicalize()
        .with_context(|| format!("canonicalize {}", patch.display()))?;
    let patch = patch.to_string_lossy().to_string();
    let mut args = vec!["apply", "--whitespace=nowarn"];
    args.extend_from_slice(flags);
    args.push(patch.as_str());
    let output = git(repo, &args)?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if flags == ["--check"] {
            bail!(
                "{}",
                stderr.lines().next().unwrap_or("git apply --check failed")
            );
        }
        bail!(
            "git apply{} failed: {}",
            flags.iter().map(|f| format!(" {f}")).collect::<String>(),
            stderr.trim()
        );
    }
    Ok(())
}

/// Restores the tree on every exit path, including a panic or an early `?`.
struct InjectionGuard<'a> {
    repo: &'a Path,
    patch: &'a Path,
    armed: bool,
}

impl InjectionGuard<'_> {
    fn disarm_and_revert(&mut self) -> Result<()> {
        if !self.armed {
            return Ok(());
        }
        self.armed = false;
        apply_patch(self.repo, self.patch, Direction::Reverse).context("revert the injection patch")
    }
}

impl Drop for InjectionGuard<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Err(err) = apply_patch(self.repo, self.patch, Direction::Reverse) {
            eprintln!(
                "CRITICAL: the injected defect is still in your tree — reverting \
                 {} failed: {err:#}. Run `git apply --reverse {}` or `git checkout -- .` \
                 before doing anything else.",
                self.patch.display(),
                self.patch.display()
            );
        }
    }
}

fn run_command(repo: &Path, cmd: &str, task: &str, phase: &str) -> Result<PhaseOutcome> {
    let mut child = Command::new("sh");
    child.arg("-c").arg(cmd).current_dir(repo);
    for key in SCRUBBED_ENV {
        child.env_remove(key);
    }
    let output = child
        .output()
        .with_context(|| format!("run stored command: {cmd}"))?;
    let mut text = String::from_utf8_lossy(&output.stdout).to_string();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    let log = log_path(task, phase);
    if let Some(parent) = log.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    std::fs::write(&log, &text).with_context(|| format!("write {}", log.display()))?;
    Ok(PhaseOutcome {
        exit: output.status.code(),
        output: text,
        log,
    })
}

fn log_path(task: &str, phase: &str) -> PathBuf {
    std::env::temp_dir()
        .join("orgasmic-verify")
        .join(task)
        .join(format!("{phase}.log"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sig(raw: &str) -> RedSignature {
        RedSignature::parse(raw, Path::new("expect-red")).unwrap()
    }

    #[test]
    fn signature_without_any_output_pin_is_rejected() {
        let err = RedSignature::parse("exit: nonzero\n", Path::new("expect-red")).unwrap_err();
        assert!(
            format!("{err}").contains("pins no output"),
            "unexpected: {err}"
        );
    }

    #[test]
    fn signature_rejects_unknown_directives() {
        let err = RedSignature::parse("matches: boom\n", Path::new("expect-red")).unwrap_err();
        assert!(
            format!("{err}").contains("unknown directive"),
            "unexpected: {err}"
        );
    }

    /// The whole point of the verb: a run that PASSES with the defect
    /// reintroduced is the false green, and it must be named as such.
    #[test]
    fn green_run_under_injection_is_a_mismatch() {
        let reasons = sig("contains: boom\n").mismatches(Some(0), "boom");
        assert_eq!(reasons.len(), 1, "{reasons:?}");
        assert!(reasons[0].contains("PASSED with the defect"), "{reasons:?}");
    }

    #[test]
    fn red_run_with_the_wrong_output_is_a_mismatch() {
        let reasons = sig("contains: assertion `left == right`\n")
            .mismatches(Some(101), "error: could not compile `orgasmic-daemon`");
        assert_eq!(reasons.len(), 1, "{reasons:?}");
        assert!(reasons[0].contains("does not contain"), "{reasons:?}");
    }

    #[test]
    fn matching_red_has_no_mismatches() {
        let reasons =
            sig("exit: 101\ncontains: FAILED\ncontains-any: mode a\ncontains-any: mode b\n")
                .mismatches(Some(101), "test x ... FAILED\npanicked at mode b");
        assert!(reasons.is_empty(), "{reasons:?}");
    }

    #[test]
    fn contains_any_requires_at_least_one_alternative() {
        let reasons = sig("contains-any: mode a\ncontains-any: mode b\n")
            .mismatches(Some(101), "some other failure");
        assert_eq!(reasons.len(), 1, "{reasons:?}");
        assert!(reasons[0].contains("none of the expected"), "{reasons:?}");
    }

    #[test]
    fn signal_death_counts_as_nonzero() {
        assert!(sig("contains: boom\n").mismatches(None, "boom").is_empty());
    }

    #[test]
    fn cmd_file_must_hold_exactly_one_line() {
        let path = Path::new("cmd");
        assert_eq!(
            parse_cmd("# note\n\ncargo test -p orgasmic-cli\n", path).unwrap(),
            "cargo test -p orgasmic-cli"
        );
        assert!(parse_cmd("a\nb\n", path)
            .unwrap_err()
            .to_string()
            .contains("2 command lines"));
        assert!(parse_cmd("# only a comment\n", path)
            .unwrap_err()
            .to_string()
            .contains("no command line"));
    }

    #[test]
    fn billed_harness_test_may_only_appear_behind_the_skip() {
        let path = Path::new("cmd");
        guard_billed_test(
            &format!("cargo test -p orgasmic-drivers -- {BILLED_DRIVERS_SKIP}"),
            path,
        )
        .expect("skipped drivers run is allowed");

        let err = guard_billed_test(
            &format!("cargo test -p orgasmic-drivers {BILLED_DRIVERS_TEST}"),
            path,
        )
        .unwrap_err();
        assert!(format!("{err}").contains("outside a `--skip`"), "{err}");

        let err = guard_billed_test("cargo test -p orgasmic-drivers", path).unwrap_err();
        assert!(format!("{err}").contains("mandatory"), "{err}");
    }

    // -- end-to-end replay against a throwaway git repo ---------------------

    struct Fixture {
        _tmp: tempfile::TempDir,
        repo: PathBuf,
        artifact: PathBuf,
    }

    impl Fixture {
        /// A repo whose `probe.sh` fails when `src.txt` says BROKEN, plus an
        /// artifact whose patch flips it to BROKEN. That is a real red-then-green
        /// replay in a few milliseconds.
        fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let repo = tmp.path().join("repo");
            std::fs::create_dir_all(&repo).unwrap();
            for args in [
                vec!["init", "-q", "-b", "main"],
                vec!["config", "user.email", "verify@test"],
                vec!["config", "user.name", "verify"],
            ] {
                let out = git(&repo, &args).unwrap();
                assert!(out.status.success(), "git {args:?}");
            }
            std::fs::write(repo.join("src.txt"), "FIXED\n").unwrap();
            std::fs::write(
                repo.join("probe.sh"),
                "#!/bin/sh\nif grep -q BROKEN src.txt; then\n  echo 'probe: drain budget bounds the server lifetime' >&2\n  exit 101\nfi\necho 'probe: ok'\n",
            )
            .unwrap();
            let out = git(&repo, &["add", "-A"]).unwrap();
            assert!(out.status.success());
            let out = git(&repo, &["commit", "-qm", "base"]).unwrap();
            assert!(out.status.success());

            let artifact = tmp.path().join("artifact");
            std::fs::create_dir_all(&artifact).unwrap();
            std::fs::write(
                artifact.join("injection.patch"),
                "diff --git a/src.txt b/src.txt\n--- a/src.txt\n+++ b/src.txt\n@@ -1 +1 @@\n-FIXED\n+BROKEN\n",
            )
            .unwrap();
            std::fs::write(artifact.join("cmd"), "sh probe.sh\n").unwrap();
            std::fs::write(
                artifact.join("expect-red"),
                "exit: 101\ncontains: drain budget bounds the server lifetime\n",
            )
            .unwrap();

            Self {
                _tmp: tmp,
                repo,
                artifact,
            }
        }

        fn write(&self, name: &str, body: &str) {
            std::fs::write(self.artifact.join(name), body).unwrap();
        }

        /// A fake `verify/`: one good artifact, one whose patch no longer
        /// applies, one directory with no patch at all, and the README that
        /// must be ignored. Outside the repo so it does not dirty the tree.
        fn verify_dir(&self) -> PathBuf {
            let verify = self._tmp.path().join("verify");
            for id in ["TASK-GOOD", "TASK-STALE"] {
                let dir = verify.join(id);
                std::fs::create_dir_all(&dir).unwrap();
                for name in ["injection.patch", "cmd", "expect-red"] {
                    std::fs::copy(self.artifact.join(name), dir.join(name)).unwrap();
                }
            }
            std::fs::write(
                verify.join("TASK-STALE/injection.patch"),
                "diff --git a/src.txt b/src.txt\n--- a/src.txt\n+++ b/src.txt\n@@ -1 +1 @@\n-A LINE THAT MOVED AWAY\n+BROKEN\n",
            )
            .unwrap();
            std::fs::create_dir_all(verify.join("TASK-NOPATCH")).unwrap();
            std::fs::write(verify.join("TASK-NOPATCH/README.md"), "a runbook\n").unwrap();
            std::fs::write(verify.join("README.md"), "not an artifact\n").unwrap();
            verify
        }

        fn replay(&self) -> Result<Report> {
            super::replay(&self.repo, "TASK-FIXTURE", &self.artifact, true)
        }

        fn assert_tree_restored(&self) {
            assert!(
                dirty_paths(&self.repo).unwrap().is_empty(),
                "replay left the tree dirty: {:?}",
                dirty_paths(&self.repo).unwrap()
            );
            assert_eq!(
                std::fs::read_to_string(self.repo.join("src.txt")).unwrap(),
                "FIXED\n"
            );
        }
    }

    #[test]
    fn sweep_enumerates_only_directories_in_sorted_order() {
        let fixture = Fixture::new();
        let ids: Vec<String> = artifacts_under(&fixture.verify_dir())
            .unwrap()
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert_eq!(ids, ["TASK-GOOD", "TASK-NOPATCH", "TASK-STALE"]);
    }

    /// The cheap sweep: a moved patch and a missing patch both surface, the
    /// good one counts, nothing runs, and the tree is never touched.
    #[test]
    fn check_all_counts_stale_and_patchless_artifacts_without_touching_the_tree() {
        let fixture = Fixture::new();
        let targets = artifacts_under(&fixture.verify_dir()).unwrap();
        let err = run_targets(&fixture.repo, &targets, Mode::Check, false, true).unwrap_err();
        assert!(
            format!("{err:#}").contains("2 of 3 verify artifact(s) no longer apply"),
            "{err:#}"
        );
        fixture.assert_tree_restored();

        let stale = fixture.verify_dir().join("TASK-STALE/injection.patch");
        let reason = format!("{:#}", check_patch(&fixture.repo, &stale).unwrap_err());
        assert!(
            reason.starts_with("error: patch failed: src.txt"),
            "{reason}"
        );
        assert!(
            !reason.contains('\n'),
            "one line, not git's whole stderr: {reason}"
        );
        check_patch(&fixture.repo, &fixture.artifact.join("injection.patch"))
            .expect("the good patch still applies");
    }

    /// The full sweep replays each artifact in turn and is red if any is.
    #[test]
    fn all_replays_every_artifact_and_fails_when_one_does() {
        let fixture = Fixture::new();
        let targets = artifacts_under(&fixture.verify_dir()).unwrap();
        let err = run_targets(&fixture.repo, &targets, Mode::Replay, true, true).unwrap_err();
        assert!(
            format!("{err:#}").contains("2 of 3 verify artifact(s) failed to replay"),
            "{err:#}"
        );
        fixture.assert_tree_restored();

        let good: Vec<_> = targets
            .into_iter()
            .filter(|(id, _)| id == "TASK-GOOD")
            .collect();
        run_targets(&fixture.repo, &good, Mode::Replay, false, true).expect("all green");
        fixture.assert_tree_restored();
    }

    #[test]
    fn well_formed_artifact_replays_red_then_green() {
        let fixture = Fixture::new();
        let report = fixture.replay().expect("replay");
        assert_eq!(report.red.exit, Some(101));
        assert_eq!(report.green.exit, Some(0));
        fixture.assert_tree_restored();
    }

    /// The false-green detector. A patch that does not reintroduce the defect
    /// leaves the command green, and verify must refuse rather than report a
    /// verified fix.
    #[test]
    fn injection_that_does_not_change_behaviour_fails_loudly() {
        let fixture = Fixture::new();
        fixture.write(
            "injection.patch",
            "diff --git a/src.txt b/src.txt\n--- a/src.txt\n+++ b/src.txt\n@@ -1 +1 @@\n-FIXED\n+FIXED (harmless edit)\n",
        );
        let err = fixture.replay().unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("FALSE GREEN GUARD TRIPPED"), "{text}");
        assert!(text.contains("PASSED with the defect"), "{text}");
        fixture.assert_tree_restored();
    }

    /// A real failure that is not the pinned one is equally untrustworthy: the
    /// probe may be failing for a reason unrelated to the defect.
    #[test]
    fn red_that_does_not_match_the_pinned_signature_fails_loudly() {
        let fixture = Fixture::new();
        fixture.write(
            "expect-red",
            "exit: 101\ncontains: a signature nobody will ever emit\n",
        );
        let err = fixture.replay().unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("FALSE GREEN GUARD TRIPPED"), "{text}");
        assert!(text.contains("a signature nobody will ever emit"), "{text}");
        fixture.assert_tree_restored();
    }

    #[test]
    fn stale_patch_fails_instead_of_skipping() {
        let fixture = Fixture::new();
        fixture.write(
            "injection.patch",
            "diff --git a/src.txt b/src.txt\n--- a/src.txt\n+++ b/src.txt\n@@ -1 +1 @@\n-A LINE THAT MOVED AWAY\n+BROKEN\n",
        );
        let err = fixture.replay().unwrap_err();
        assert!(format!("{err:#}").contains("stale artifact"), "{err:#}");
        fixture.assert_tree_restored();
    }

    #[test]
    fn dirty_tree_is_refused_before_anything_is_applied() {
        let fixture = Fixture::new();
        std::fs::write(fixture.repo.join("src.txt"), "LOCAL WORK\n").unwrap();
        let err = fixture.replay().unwrap_err();
        assert!(format!("{err:#}").contains("dirty tree"), "{err:#}");
        assert_eq!(
            std::fs::read_to_string(fixture.repo.join("src.txt")).unwrap(),
            "LOCAL WORK\n",
            "a refused replay must not touch the operator's work"
        );
    }

    /// Daemon writes under `.orgasmic/` are continuous in a live project and no
    /// injection patch ever touches them; they must not make a repo permanently
    /// unverifiable.
    #[test]
    fn daemon_owned_churn_does_not_count_as_dirty() {
        let fixture = Fixture::new();
        std::fs::create_dir_all(fixture.repo.join(".orgasmic/state")).unwrap();
        std::fs::write(fixture.repo.join(".orgasmic/state/live.json"), "{}\n").unwrap();
        let report = fixture.replay().expect("replay");
        assert_eq!(report.green.exit, Some(0));
    }

    /// The green half matters too: if the command fails on the fixed tree, the
    /// fix is not what the artifact claims.
    #[test]
    fn green_phase_failure_is_reported() {
        let fixture = Fixture::new();
        std::fs::write(fixture.repo.join("src.txt"), "BROKEN\n").unwrap();
        let out = git(&fixture.repo, &["commit", "-qam", "ship the defect"]).unwrap();
        assert!(out.status.success());
        fixture.write(
            "injection.patch",
            "diff --git a/src.txt b/src.txt\n--- a/src.txt\n+++ b/src.txt\n@@ -1 +1 @@\n-BROKEN\n+BROKEN AND ALSO REWORDED\n",
        );
        let err = fixture.replay().unwrap_err();
        assert!(format!("{err:#}").contains("not green"), "{err:#}");
    }
}

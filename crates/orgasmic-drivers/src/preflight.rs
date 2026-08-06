// orgasmic:arch_A53QX, TASK-TJKFC
//! Shared machinery for harness readiness probes.
//!
//! Every adapter answers the same question — "could a worker launched with this
//! configuration start?" — and every adapter has the same two ways of getting it
//! wrong: rejecting a dispatch it should not have, or billing the operator for
//! the privilege of asking. The rules live on [`crate::WorkerDriver::preflight`];
//! this module is the small amount of code that makes following them cheap.
//!
//! Nothing here spawns a worker or submits a turn. A probe reads a credential
//! the harness already holds, locally, in well under a second.

use std::process::Stdio;
use std::time::Duration;

use crate::r#trait::Preflight;

/// How long a harness's own status command may take before the probe gives up.
///
/// Measured on 2026-07-25: `claude auth status` 0.28 s, `cursor-agent status`
/// and `codex login status` comparable. The bound exists so a wedged harness
/// costs a dispatch nothing rather than hanging it. Exceeding it is
/// inconclusive, never fatal — a slow answer is not a wrong one.
pub(crate) const STATUS_TIMEOUT: Duration = Duration::from_secs(5);

/// How many times the probe puts the question before it accepts silence.
///
/// A timeout has two causes that are indistinguishable from here, and they
/// deserve opposite answers. A *wedged harness* will never reply; a *busy
/// machine* has not replied yet. Measured 2026-07-29 under a loaded workspace
/// test run: a `claude` stub that answers in 0.28 s idle failed to reach the
/// first line of its own script inside the bound — the child was still waiting
/// to exec when the 5 s elapsed — and the identical file exec'd normally 30 s
/// later in the same process. Nothing was wrong with the harness; the machine
/// was busy.
///
/// Giving up after one attempt turned that into an admitted dispatch for a
/// logged-out harness: inconclusive is not fatal (see
/// [`crate::WorkerDriver::preflight`]), so the safeguard silently switched
/// itself off under exactly the condition it is needed most — an operator
/// running several dispatches at once (TASK-GEZHQ). One retry separates the two
/// causes: a wedged harness misses both attempts and still costs a bounded
/// `STATUS_ATTEMPTS * STATUS_TIMEOUT`, which is still nothing against a
/// dispatch, while a busy machine usually answers the second time.
///
/// Only a *timeout* is retried. A spawn error is a definitive "cannot ask" —
/// the binary is missing — and asking twice reaches the same answer more slowly.
pub(crate) const STATUS_ATTEMPTS: usize = 2;

/// What a harness's status command said.
///
/// Both streams are kept because the harnesses disagree about which one a
/// status answer belongs on: claude writes JSON to stdout, and `codex login
/// status` writes its answer to *stderr* while leaving stdout empty. A reader
/// that assumed stdout silently classified a logged-in codex as "no probe"
/// (caught by `installed_harnesses_answer_their_own_readiness_probe` — the
/// reason that test asserts against real binaries).
///
/// Deliberately no exit status. Measured 2026-07-25: `claude auth status` exits
/// **1** when logged out, so gating on a zero exit turned the one answer this
/// whole mechanism exists to catch into "inconclusive" — the exact bug, in the
/// exact place, that the probe was written to prevent elsewhere. A non-zero
/// exit from a status command usually *is* the answer. What separates a verdict
/// from a non-answer is whether the harness said something we recognise, so
/// that is the only thing the classifiers look at.
pub(crate) struct StatusOutput {
    /// Standard output, for harnesses that answer in structured form.
    pub stdout: String,
    /// Both streams, for phrase matching where the harness picks either.
    pub combined: String,
}

/// Run a harness's own status command.
///
/// `None` means the question could not be put at all — the binary is missing,
/// the spawn failed, or it did not answer within [`STATUS_ATTEMPTS`] tries —
/// which every caller must treat as inconclusive rather than as a "no".
///
/// Every path to `None` is logged. A silent one is what made TASK-GEZHQ's
/// admitted dispatch unreadable from its own artifacts: the run record said the
/// preflight had no opinion, and nothing said why.
pub(crate) async fn read_status_output(command: &str, args: &[&str]) -> Option<StatusOutput> {
    for attempt in 1..=STATUS_ATTEMPTS {
        let mut cmd = tokio::process::Command::new(command);
        cmd.args(args)
            // A preflight must never prompt. A null stdin makes an interactive
            // fallback impossible rather than merely unlikely: the harness cannot
            // read an answer that nobody is there to give.
            .stdin(Stdio::null())
            .kill_on_drop(true);
        match tokio::time::timeout(STATUS_TIMEOUT, cmd.output()).await {
            Ok(Ok(output)) => {
                let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
                let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
                return Some(StatusOutput {
                    combined: format!("{stdout}{stderr}"),
                    stdout,
                });
            }
            // Not retried: see [`STATUS_ATTEMPTS`].
            Ok(Err(error)) => {
                tracing::warn!(
                    command,
                    %error,
                    "harness status command could not be spawned; this dispatch's \
                     credentials go unchecked"
                );
                return None;
            }
            Err(_) => tracing::warn!(
                command,
                attempt,
                attempts = STATUS_ATTEMPTS,
                timeout_secs = STATUS_TIMEOUT.as_secs(),
                "harness status command did not answer in time"
            ),
        }
    }
    tracing::warn!(
        command,
        attempts = STATUS_ATTEMPTS,
        "harness status command never answered; this dispatch's credentials go \
         unchecked and a worker that cannot authenticate will fail after it owns \
         a lease, a session and a worktree"
    );
    None
}

/// The exact phrases a prose-answering harness uses for each login state.
///
/// Both are required, and both must have been *observed* from the harness
/// rather than guessed. See [`classify_prose_login`] for why the pair matters.
pub(crate) struct ProseLogin {
    /// Observed output when the harness has no usable login.
    pub logged_out: &'static str,
    /// Observed output when it does.
    pub logged_in: &'static str,
}

/// Classify a harness that answers about its login in prose rather than JSON.
///
/// `cursor-agent status` and `codex login status` both exit 0 whether or not
/// they are logged in and print a sentence, so the sentence is the only signal
/// available. String matching is normally a poor basis for refusing to do work,
/// and the asymmetry here is what makes it acceptable:
///
/// - Only the exactly-observed logged-out phrase produces [`Preflight::Fatal`].
/// - Anything unrecognised produces [`Preflight::Unsupported`], which restores
///   the behaviour that existed before this probe did.
///
/// So a harness that renames its message costs us a probe, never a working
/// dispatch. The failure mode of a wrong guess is losing the safeguard, not
/// blocking the operator — which is the only direction worth being wrong in.
pub(crate) fn classify_prose_login(
    stdout: &str,
    phrases: &ProseLogin,
    fatal_reason: &str,
) -> Preflight {
    // Order matters: "Not logged in" and "Logged in as" share a substring, and
    // the rejection must be the more specific match.
    if stdout.contains(phrases.logged_out) {
        return Preflight::fatal(fatal_reason.to_string());
    }
    if stdout.contains(phrases.logged_in) {
        return Preflight::Ready;
    }
    Preflight::Unsupported
}

/// Verdict for a worker that will present an API key from the environment.
///
/// Deliberately narrow, and shared because every harness that takes a key has
/// the same two facts available: an empty key is a certain failure worth
/// rejecting for free, and a non-empty key is *not* evidence of a working
/// worker. Only the provider can say whether a key is accepted, and asking
/// costs a billed turn (see [`crate::WorkerDriver::preflight`]), so the honest
/// verdict for a present key is that nothing was checked.
pub(crate) fn classify_api_key(api_key: Option<&str>, empty_reason: &str) -> Preflight {
    match api_key {
        Some(key) if key.trim().is_empty() => Preflight::fatal(empty_reason.to_string()),
        _ => Preflight::Unsupported,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Captured from cursor-agent and codex on 2026-07-25, both states, by
    /// pointing `HOME` at an empty directory rather than by logging out.
    fn cursor_phrases() -> ProseLogin {
        ProseLogin {
            logged_out: "Not logged in",
            logged_in: "Logged in as",
        }
    }

    #[test]
    fn the_observed_logged_out_sentence_rejects_the_dispatch() {
        let verdict = classify_prose_login("Not logged in\n", &cursor_phrases(), "run login");
        assert_eq!(verdict.rejects_dispatch(), Some("run login"));
    }

    #[test]
    fn the_observed_logged_in_sentence_is_ready() {
        let verdict = classify_prose_login(
            "✓ Logged in as operator@example.com\n",
            &cursor_phrases(),
            "run login",
        );
        assert_eq!(verdict, Preflight::Ready);
    }

    /// The property the prose matcher is built around: an unfamiliar answer
    /// costs the safeguard, never a dispatch. A harness that rewords its
    /// message must not start refusing the operator's work.
    #[test]
    fn an_unrecognised_answer_never_rejects_a_dispatch() {
        for output in [
            "authenticated: yes",              // a plausible future rewording
            "",                                // no output at all
            "error: unknown command 'status'", // an older harness
        ] {
            let verdict = classify_prose_login(output, &cursor_phrases(), "run login");
            assert_eq!(verdict, Preflight::Unsupported, "{output:?}");
        }
    }

    /// A status command on `PATH` is not needed: [`read_status_output`] takes
    /// the command it runs, so a stub can be addressed by its own path and no
    /// test here has to mutate process-global `PATH` (`.orgasmic/gotchas.org`).
    fn write_stub(dir: &std::path::Path, body: &str) -> std::path::PathBuf {
        let stub = dir.join("harness-stub");
        std::fs::write(&stub, body).unwrap();
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&stub).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&stub, perms).unwrap();
        stub
    }

    /// The property TASK-GEZHQ turns on: one slow start must not be taken for
    /// an answer.
    ///
    /// This is what the load did, done to the code — the stub hangs past
    /// [`STATUS_TIMEOUT`] the first time it is asked and answers instantly the
    /// second, which is the shape measured under a loaded workspace run (a
    /// child still waiting to exec at 5 s, the same file exec'ing normally
    /// moments later). With a single attempt the probe reports "could not ask"
    /// for a harness that was about to speak, and every caller reads that as
    /// "no opinion" and lets the dispatch through.
    ///
    /// Deliberately not a mocked clock: the thing under test is a real child
    /// that is late, and the one attempt this costs is the price of proving it.
    #[tokio::test]
    async fn a_status_command_that_starts_late_is_asked_again_rather_than_given_up_on() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("asked-once");
        let stub = write_stub(
            dir.path(),
            &format!(
                r#"#!/bin/sh
if [ ! -f "{marker}" ]; then
  : > "{marker}"
  # Outlast the bound without ever answering, exactly as a child that has not
  # reached its first instruction looks from the parent's side.
  sleep 120
fi
printf '%s\n' 'Not logged in'
exit 1
"#,
                marker = marker.display()
            ),
        );

        let status = read_status_output(stub.to_str().unwrap(), &["status"]).await;

        let status = status.expect("the second attempt answered, so the probe has an answer");
        assert!(
            status.combined.contains("Not logged in"),
            "the retry must carry the harness's real answer, not an empty one: {:?}",
            status.combined
        );
        assert!(
            marker.exists(),
            "the first attempt must actually have been made"
        );
    }

    /// The other half of the retry rule: a missing binary is a definitive
    /// "cannot ask", so it costs one attempt and no timeout. Without this the
    /// retry would double the price of the commonest inconclusive case — a
    /// harness that is simply not installed.
    #[tokio::test]
    async fn a_binary_that_cannot_be_spawned_is_not_asked_twice() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("no-such-harness");

        let started = std::time::Instant::now();
        let status = read_status_output(missing.to_str().unwrap(), &["status"]).await;

        assert!(status.is_none(), "a missing binary cannot answer");
        assert!(
            started.elapsed() < STATUS_TIMEOUT,
            "a spawn failure must not be retried through the timeout: {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn an_empty_key_is_fatal_and_a_present_one_is_merely_unchecked() {
        assert!(classify_api_key(Some(""), "empty")
            .rejects_dispatch()
            .is_some());
        assert!(classify_api_key(Some("  \n"), "empty")
            .rejects_dispatch()
            .is_some());
        assert_eq!(
            classify_api_key(Some("sk-not-real"), "empty"),
            Preflight::Unsupported
        );
        assert_eq!(classify_api_key(None, "empty"), Preflight::Unsupported);
    }
}

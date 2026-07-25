// arch: arch_A53QX.2
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
/// the spawn failed, or it did not answer in time — which every caller must
/// treat as inconclusive rather than as a "no".
pub(crate) async fn read_status_output(command: &str, args: &[&str]) -> Option<StatusOutput> {
    let mut cmd = tokio::process::Command::new(command);
    cmd.args(args)
        // A preflight must never prompt. A null stdin makes an interactive
        // fallback impossible rather than merely unlikely: the harness cannot
        // read an answer that nobody is there to give.
        .stdin(Stdio::null())
        .kill_on_drop(true);
    let output = tokio::time::timeout(STATUS_TIMEOUT, cmd.output())
        .await
        .ok()?
        .ok()?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    Some(StatusOutput {
        combined: format!("{stdout}{stderr}"),
        stdout,
    })
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

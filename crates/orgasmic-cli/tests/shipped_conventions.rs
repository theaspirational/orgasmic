//! Shipped-prose guards (TASK-6AYEJ.1). The manager dispatch convention is an
//! instruction sheet an agent follows literally, so a regression in it is a
//! behavioural regression with no compiler and no runtime to catch it. These
//! are content assertions on `shipped/prompt-studio/conventions/`, not on code.

use std::path::PathBuf;

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

fn manager_dispatch_convention() -> String {
    let path = repo_root().join("shipped/prompt-studio/conventions/manager-dispatch.org");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// The explicit opt-in that lets a dangerous command appear in the convention
/// at all. TASK-6AYEJ.2: the guard below used to anchor on a prose sentence
/// ("destroys exactly that"), which meant an innocent rewording failed it and a
/// reworded dangerous command (`git worktree remove <path> --force`) evaded it.
/// A structured marker is neither.
const DANGEROUS_EXAMPLE_MARKER: &str = "[DANGEROUS-EXAMPLE]";

/// How far back from a dangerous command the marker may sit. Wide enough for a
/// lead-in clause, narrow enough that a marker elsewhere in the paragraph does
/// not license it.
const MARKER_LOOKBEHIND: usize = 200;

/// How far past `git worktree remove` a `--force` still counts as belonging to
/// that command. Generous enough to span a path argument.
const COMMAND_WINDOW: usize = 160;

/// Largest byte index <= `at` that is a char boundary. The convention is UTF-8
/// prose full of em dashes, so fixed-width windows must be clamped.
fn floor_boundary(text: &str, at: usize) -> usize {
    let mut at = at.min(text.len());
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

/// Every offset at which the convention spells out a destructive git command,
/// regardless of how its arguments are spelled or ordered.
///
/// Two shapes matter (TASK-6AYEJ.2):
/// - `git worktree remove` anywhere on the same command as `--force`/`-f`, so
///   `git worktree remove <path> --force` is caught as well as the adjacent
///   form the old substring guard looked for;
/// - a forced branch delete under any spelling (`-D`, `--delete --force`),
///   whatever the branch is called.
fn dangerous_command_offsets(text: &str) -> Vec<(usize, &'static str)> {
    let mut hits = Vec::new();
    for (index, _) in text.match_indices("git worktree remove") {
        // Stop at the end of the line so a later, unrelated `--force` mention
        // cannot be blamed on this command.
        let window = &text[index..floor_boundary(text, index + COMMAND_WINDOW)];
        let window = window.split('\n').next().unwrap_or(window);
        if window.contains("--force") || window.contains(" -f ") {
            hits.push((index, "forced worktree removal"));
        }
    }
    for needle in ["git branch -D", "git branch --delete --force"] {
        for (index, _) in text.match_indices(needle) {
            hits.push((index, "forced branch deletion"));
        }
    }
    hits
}

/// TASK-6AYEJ finding 1: step 4 used to tell the manager to run
/// `git worktree remove --force && git branch -D` by hand. That destroys
/// exactly the data `dispatch-close` exists to salvage — `finalize --commit` is
/// optional, so a worker may legitimately leave uncommitted output, and the
/// close path salvages it to `refs/orgasmic/salvage/<sha>` and then removes
/// WITHOUT `--force` so git's own clean check gates the removal. The prose is
/// fixed; this is the guard that keeps it fixed.
#[test]
fn manager_convention_never_instructs_forced_worktree_removal_by_hand() {
    let text = manager_dispatch_convention();

    assert!(
        text.contains("*Do not remove the worktree or delete the branch by hand.*"),
        "step 4 must still forbid hand cleanup"
    );
    assert!(
        text.contains("dispatch-close --worktree-remove --branch-delete="),
        "step 4 must still name dispatch-close as the sole success-path cleanup"
    );
    assert!(
        text.contains("refs/orgasmic/salvage/<sha>"),
        "the salvage rationale must survive; without it the instruction reads as arbitrary"
    );

    // A destructive command may appear only as an explicitly marked example.
    // Zero occurrences is fine — the guard never demands the warning exist.
    for (offset, what) in dangerous_command_offsets(&text) {
        let start = floor_boundary(&text, offset.saturating_sub(MARKER_LOOKBEHIND));
        let end = floor_boundary(&text, offset + COMMAND_WINDOW);
        assert!(
            text[start..offset].contains(DANGEROUS_EXAMPLE_MARKER),
            "the convention spells out {what} without a preceding \
             `{DANGEROUS_EXAMPLE_MARKER}` marker, so it reads as an instruction:\n{}",
            &text[start..end]
        );
    }
}

/// The guard's own regression test (TASK-6AYEJ.2). The previous guard matched
/// the adjacent substring `worktree remove --force`, so the realistic variants
/// below — the ORIGINAL dangerous instruction among them — walked straight
/// past it. Each of these must be detected; the safe prose must not be.
#[test]
fn dangerous_command_detector_catches_reworded_variants() {
    for dangerous in [
        "run git worktree remove --force && git branch -D task-NNN-impl",
        "run git worktree remove /tmp/wt --force to clean up",
        "run git worktree remove \"$WT\" -f then move on",
        "then git branch -D whatever-the-branch-is",
        "then git branch --delete --force whatever-the-branch-is",
    ] {
        assert!(
            !dangerous_command_offsets(dangerous).is_empty(),
            "detector missed a dangerous variant: {dangerous}"
        );
    }

    for safe in [
        // The real convention's safe sentences, and a plain reworded one.
        "cleanup belongs to =dispatch-close --worktree-remove --branch-delete=",
        "the close path removes without =--force=, so git's clean check gates it",
        "use git worktree remove only through dispatch-close\nnever pass --force",
        "delete the branch with the --branch-delete flag",
    ] {
        assert!(
            dangerous_command_offsets(safe).is_empty(),
            "detector fired on safe prose: {safe}"
        );
    }
}

/// TASK-6AYEJ.1: `dispatch-close` is generation-bound. The convention must
/// document `--started-tx`, or the fence exists in the CLI and is never used.
#[test]
fn manager_convention_documents_generation_bound_close() {
    let text = manager_dispatch_convention();
    assert!(
        text.contains("--started-tx"),
        "step 4 must tell the manager to pass --started-tx"
    );
    assert!(
        text.contains("dispatch-close --task TASK-NNN --started-tx <started_tx> --status done"),
        "the copyable close command must carry --started-tx"
    );
    assert!(
        text.contains("manager.dispatch_started"),
        "the prose must say what --started-tx names"
    );
    // TASK-6AYEJ.2: the flag is no longer advisory. The convention must not go
    // back to describing what a tokenless close *selects*, because it now
    // selects nothing — it is refused.
    assert!(
        text.contains("REFUSED"),
        "the prose must say a tokenless close of a live dispatch is refused"
    );
}

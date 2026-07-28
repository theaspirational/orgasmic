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

    // `git worktree remove --force` may appear ONCE, and only inside the
    // sentence explaining that it is the data-loss path. Any second occurrence,
    // or a first one that lost its warning context, is the instruction coming
    // back.
    let force_hits: Vec<usize> = text
        .match_indices("worktree remove --force")
        .map(|(i, _)| i)
        .collect();
    assert_eq!(
        force_hits.len(),
        1,
        "expected exactly one (explanatory) mention of forced worktree removal, found {}",
        force_hits.len()
    );
    let hit = force_hits[0];
    let window = &text[hit.saturating_sub(400)..(hit + 400).min(text.len())];
    assert!(
        window.contains("destroys exactly that"),
        "the only mention of forced removal must be the warning, not an instruction:\n{window}"
    );
    assert!(
        !text.contains("git branch -D task-"),
        "the convention must not spell out a by-hand branch deletion command"
    );
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
}

//! The git operations a dispatch-record close stands down from, as data.
//!
//! This is its own module for one reason: `tests/shipped_conventions.rs` is an
//! integration test and `orgasmic-cli` has no lib target, so a `const` living
//! in `manager.rs` would be unreachable from it. The pin that keeps the shipped
//! manager convention and this array in step (TASK-QGWK7.1.1.1.1 B-2) was
//! therefore written over the convention prose ALONE while its comment claimed
//! that "dropping one from either side fails here" — and
//! TASK-QGWK7.1.1.1.1.1 then dropped an entry from the array with all four
//! convention tests still green (TASK-QGWK7.1.1.1.1.1.1 D-2). A file both the
//! binary and the test can reach is what makes the claim true: the test
//! `#[path]`-includes this module and compares this array against the list the
//! prose gives, in both directions.

/// The refusal name for the one marker that is a LEFTOVER rather than an
/// owner: a `.git/sequencer` todo list with no pick currently stopped. It needs
/// its own advice — `--continue` alone is wrong for it, because the range may
/// already have been abandoned — so the refusal message branches on this.
pub const STOPPED_PICK_RANGE: &str = "revert or cherry-pick sequence";

/// `(marker file under `.git`, the name the refusal uses, the name the shipped
/// manager-dispatch convention lists it under)`.
///
/// The two names differ where one marker answers for a word the convention
/// spells differently: `rebase-apply` is the `am` backend (and the pre-2.26
/// rebase backend), so it answers for the convention's `am`, while
/// `rebase-merge` answers for `rebase`.
///
/// Checked in order, and `sequencer` MUST stay last: every stopped pick carries
/// the todo list as well as its `*_HEAD` marker, so an earlier position would
/// report a "sequence" for an ordinary stopped pick and hand out the wrong
/// remedy.
///
/// `sequencer` is in this array because a stopped range does NOT always keep a
/// `*_HEAD` marker beside it (TASK-QGWK7.1.1.1.1.1.1 D-1, measured on git
/// 2.52.0): resolve the conflict and `git commit` — an ordinary, documented
/// route — and git clears `REVERT_HEAD` while leaving the todo list, from which
/// `git revert --continue` still resumes the range. TASK-QGWK7.1.1.1.1.1
/// removed the entry on the belief that no such state exists; it does, and
/// without the entry that live range is unguarded.
pub const SEQUENCER_MARKERS: &[(&str, &str, &str)] = &[
    ("rebase-merge", "rebase", "rebase"),
    ("rebase-apply", "rebase or am", "am"),
    ("MERGE_HEAD", "merge", "merge"),
    ("CHERRY_PICK_HEAD", "cherry-pick", "cherry-pick"),
    ("REVERT_HEAD", "revert", "revert"),
    ("BISECT_LOG", "bisect", "bisect"),
    (
        "sequencer",
        STOPPED_PICK_RANGE,
        "an interrupted revert/cherry-pick sequence",
    ),
];

//! Shipped-prose guards (TASK-6AYEJ.1). The manager dispatch convention is an
//! instruction sheet an agent follows literally, so a regression in it is a
//! behavioural regression with no compiler and no runtime to catch it. These
//! are content assertions on `shipped/prompt-studio/conventions/`, not on code.

use std::path::PathBuf;

/// The guard's own marker table, reached the only way an integration test can
/// reach it: `orgasmic-cli` is a bin-only crate, so the array lives in a
/// dependency-free module both targets include (TASK-QGWK7.1.1.1.1.1.1 D-2).
#[path = "../src/sequencer_markers.rs"]
mod sequencer_markers;

fn repo_root() -> PathBuf {
    let mut here = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    loop {
        if here.join("shipped/entry/router.org").is_file() {
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

/// How much of the offending text to quote back in a failure. Display only —
/// detection is tokenized, not windowed (TASK-6AYEJ.3).
const COMMAND_WINDOW: usize = 160;

/// Punctuation that wraps a command in Org or Markdown prose (`=verbatim=`,
/// `~code~`, `` `code` ``, `*bold*`) or in ordinary sentence flow. Stripped from
/// both ends of every token, so `-f=` and `--force,` read as the flags they are.
const TOKEN_EDGE_PUNCTUATION: &[char] = &[
    '=', '~', '`', '*', '"', '\'', '(', ')', '[', ']', '{', '}', '<', '>', ',', '.', ':', '?', '!',
];

/// Git global options that consume the NEXT token as their value, so the
/// subcommand is two tokens further on (`git -C /repo worktree remove …`).
const GIT_GLOBAL_OPTIONS_WITH_VALUE: &[&str] = &[
    "-C",
    "-c",
    "--git-dir",
    "--work-tree",
    "--namespace",
    "--exec-path",
    "--super-prefix",
];

/// Largest byte index <= `at` that is a char boundary. The convention is UTF-8
/// prose full of em dashes, so fixed-width windows must be clamped.
fn floor_boundary(text: &str, at: usize) -> usize {
    let mut at = at.min(text.len());
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

/// Byte ranges of the individual shell commands in `text`.
///
/// A command ends at `;`, `&`, `|` (so `&&`/`||`/pipelines split), or a newline
/// that is not a backslash continuation. Splitting first is what makes the rest
/// of the detector safe: a `--force` mentioned in the next clause can never be
/// blamed on this command, and one spelled on the continuation line still can.
fn command_segments(text: &str) -> Vec<(usize, usize)> {
    let mut segments = Vec::new();
    let mut start = 0usize;
    for (index, ch) in text.char_indices() {
        let ends_command = match ch {
            ';' | '&' | '|' => true,
            '\n' => !text[start..index]
                .trim_end_matches([' ', '\t'])
                .ends_with('\\'),
            _ => false,
        };
        if ends_command {
            if index > start {
                segments.push((start, index));
            }
            start = index + ch.len_utf8();
        }
    }
    if start < text.len() {
        segments.push((start, text.len()));
    }
    segments
}

/// Whitespace-separated tokens of one command, each stripped of surrounding
/// prose punctuation, paired with its byte offset in the original text.
fn command_tokens(text: &str, (from, to): (usize, usize)) -> Vec<(usize, &str)> {
    let segment = &text[from..to];
    let mut words = Vec::new();
    let mut word_start: Option<usize> = None;
    for (index, ch) in segment.char_indices() {
        if ch.is_whitespace() {
            if let Some(start) = word_start.take() {
                words.push((start, index));
            }
        } else if word_start.is_none() {
            word_start = Some(index);
        }
    }
    if let Some(start) = word_start {
        words.push((start, segment.len()));
    }
    words
        .into_iter()
        .map(|(start, end)| {
            (
                from + start,
                segment[start..end].trim_matches(TOKEN_EDGE_PUNCTUATION),
            )
        })
        .filter(|(_, token)| !token.is_empty())
        .collect()
}

/// Index of the git SUBCOMMAND within the tokens that follow `git`, skipping
/// any global options in between.
fn git_subcommand_index(words: &[&str]) -> usize {
    let mut index = 0;
    while let Some(word) = words.get(index) {
        if !word.starts_with('-') {
            break;
        }
        index += 1;
        if GIT_GLOBAL_OPTIONS_WITH_VALUE.contains(word) {
            index += 1;
        }
    }
    index
}

/// `git`, however it is invoked: bare, absolute (`/usr/bin/git`), or relative
/// (`./git`). TASK-1T3FZ: a path-qualified invocation used to be invisible.
fn is_git_invocation(word: &str) -> bool {
    word.rsplit('/').next() == Some("git")
}

/// A line-continuation backslash. It is punctuation holding one command
/// together across lines, never an argument, so the scan steps over it —
/// TASK-1T3FZ: `git \` + newline + `worktree remove … --force` used to read as
/// the subcommand being `\`, and scored zero.
fn is_continuation(word: &str) -> bool {
    !word.is_empty() && word.chars().all(|ch| ch == '\\')
}

/// A short flag bundle carrying `letter` (`-f`, `-fq`, `-Dq`), never a long one.
fn short_flag_carries(word: &str, letter: char) -> bool {
    word.starts_with('-') && !word.starts_with("--") && word[1..].contains(letter)
}

fn is_force_flag(word: &str) -> bool {
    word == "--force" || short_flag_carries(word, 'f')
}

/// Every offset at which the convention spells out a destructive git command,
/// regardless of how its arguments are spelled, ordered, punctuated, or wrapped.
///
/// TASK-6AYEJ.3: this used to match fixed substrings inside fixed byte windows,
/// and every realistic variant walked past it — Org markup fused to the flag
/// (`-f=`), a git global option splitting the phrase (`git -C /repo worktree
/// remove`), a `--force` on a continuation line. Fixed windows cannot be patched
/// into correctness one special case at a time, so the text is now split into
/// commands and tokenized, and the shapes are asserted over TOKENS:
/// - `git … worktree remove` with a force flag anywhere later in the same
///   command;
/// - a forced branch delete under any spelling (`-D`, `-d --force`,
///   `--delete --force`), whatever the branch is called.
///
/// TASK-1T3FZ: it then classified only the FIRST `git` per segment, so any
/// ordinary safe mention in front of the dangerous one made the guard vacuous
/// (`run git status, then git worktree remove $WT --force` scored zero). Every
/// `git` in a command is now a candidate.
fn dangerous_command_offsets(text: &str) -> Vec<(usize, &'static str)> {
    let mut hits = Vec::new();
    for segment in command_segments(text) {
        let tokens = command_tokens(text, segment);
        for (index, (offset, token)) in tokens.iter().enumerate() {
            if !is_git_invocation(token) {
                continue;
            }
            let words: Vec<&str> = tokens[index + 1..]
                .iter()
                .map(|(_, word)| *word)
                .filter(|word| !is_continuation(word))
                .collect();
            let rest = &words[git_subcommand_index(&words).min(words.len())..];
            match rest.first().copied() {
                Some("worktree") => {
                    // Only a force flag AFTER `remove` counts: prose such as
                    // "never pass --force to git worktree remove" is advice,
                    // not a command.
                    if rest.get(1) == Some(&"remove")
                        && rest[2..].iter().any(|word| is_force_flag(word))
                    {
                        hits.push((*offset, "forced worktree removal"));
                    }
                }
                Some("branch") => {
                    let flags = &rest[1..];
                    let capital_d = flags.iter().any(|word| short_flag_carries(word, 'D'));
                    let forced_lowercase = flags
                        .iter()
                        .any(|word| *word == "--delete" || short_flag_carries(word, 'd'))
                        && flags.iter().any(|word| is_force_flag(word));
                    if capital_d || forced_lowercase {
                        hits.push((*offset, "forced branch deletion"));
                    }
                }
                _ => {}
            }
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

    // TASK-6AYEJ.3: the previous detector found NOTHING in this file — the
    // marked example included — so the loop below was vacuous and would have
    // stayed green through any rewording. The marker is the convention's own
    // admission that a dangerous command is spelled out nearby; if it is
    // present, the detector must see that command. (A file with no marker at
    // all is still fine: the guard never demands the warning exist.)
    if text.contains(DANGEROUS_EXAMPLE_MARKER) {
        assert!(
            !dangerous_command_offsets(&text).is_empty(),
            "the convention carries a `{DANGEROUS_EXAMPLE_MARKER}` marker but the detector \
             found no dangerous command — the guard below is asserting nothing"
        );
    }

    // A destructive command may appear only as an explicitly marked example.
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

/// The guard's own regression test (TASK-6AYEJ.2, extended by TASK-6AYEJ.3).
/// Each generation of this detector has been defeated by ordinary spelling
/// variation: first by `worktree remove --force` not being adjacent, then by
/// markup punctuation, git global options, and line continuations. The list
/// below is therefore the floor, not the target — the three inputs a reviewer's
/// exact-logic probe drove through the byte-window version are in it, together
/// with the wrappings the convention itself actually uses.
#[test]
fn dangerous_command_detector_catches_reworded_variants() {
    for dangerous in [
        "run git worktree remove --force && git branch -D task-NNN-impl",
        "run git worktree remove /tmp/wt --force to clean up",
        "run git worktree remove \"$WT\" -f then move on",
        "then git branch -D whatever-the-branch-is",
        "then git branch --delete --force whatever-the-branch-is",
        // TASK-6AYEJ.3, reviewer probe: all three returned ZERO hits before.
        "=git worktree remove \"$WT\" -f=",
        "git -C /repo worktree remove \"$WT\" --force",
        "git worktree remove \"$WT\" \\\n  --force\n",
        // TASK-1T3FZ, reviewer probe: all three returned ZERO hits against the
        // detector that read only the first `git` token per segment. The first
        // is the one that mattered — one safe `git` in front of a forced
        // removal made the whole guard vacuous.
        "run git status, then git worktree remove $WT --force",
        "git \\\n  worktree remove $WT --force\n",
        "/usr/bin/git worktree remove $WT --force",
        // And the wrappings this convention is written in, plus argument
        // orders and flag bundles no fixed window would have survived.
        "~git worktree remove $WT --force~",
        "`git worktree remove $WT --force`",
        "*git worktree remove $WT --force*",
        "git worktree remove -f \"$WT\"",
        "git worktree remove \"$WT\" -fq",
        "git --git-dir=/repo/.git worktree remove \"$WT\" --force",
        "git -c core.hooksPath=/dev/null worktree remove \"$WT\" --force",
        "git worktree remove \"$WT\" --force; git branch -D task-NNN-impl",
        "git worktree remove \"$WT\" --force &&\n  git branch --force --delete task-NNN-impl",
        "cd /repo | git worktree remove $WT --force",
        "git branch \\\n  -D \\\n  task-NNN-impl",
        "git branch -d task-NNN-impl --force",
        "(git worktree remove \"$WT\" --force)",
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
        // TASK-6AYEJ.3: the tokenizer must not turn advice into an instruction.
        "never pass --force to git worktree remove",
        // The safe commands themselves: unforced removal is what the close path
        // does, and an unforced delete is not the hazard.
        "git worktree remove \"$WT\"",
        "git -C /repo worktree remove \"$WT\"",
        "git branch -d already-merged-branch",
        "git worktree list --porcelain",
        // A force flag in the NEXT command must not be blamed on this one.
        "git worktree remove \"$WT\"\ngit push --force-with-lease",
        // TASK-1T3FZ: scanning every `git` must not invent hits either — a
        // safe mention before a safe command stays safe, and a name that
        // merely ends in `git` is not git.
        "run git status, then git worktree list --porcelain",
        "/usr/bin/git worktree remove \"$WT\"",
        "legit worktree remove $WT --force",
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

/// TASK-QGWK7 / TASK-QGWK7.1: after close the report must be findable; the
/// Integrate step used to point only at a tmp/ path that close then deleted.
#[test]
fn manager_convention_names_post_close_report_path() {
    let text = manager_dispatch_convention();
    // dec_E01MC: the promoted record moved into the task node dir's reserved
    // `dispatches/<started-tx>/` (AP971.1), matching `dispatch_record_dir`.
    assert!(
        text.contains(".orgasmic/tasks/<ID>/dispatches/<started_tx>/report.md"),
        "step 4 must name where a closed dispatch's report lives"
    );
    assert!(
        text.contains("Retention: keep forever"),
        "the retention policy must be stated, not a silent delete"
    );
    assert!(
        text.contains("24–30 MB/yr") && text.contains("64 KB"),
        "year-one growth for last.txt and the stdout.log bound must be stated"
    );
    assert!(
        text.contains(":REPORT_PATH:"),
        "the prose must say the close tx names the promoted path"
    );
    // TASK-QGWK7.1.1: the durability mechanism moved from staging to a
    // dedicated commit, and the reason it moved (staging blocks the merge the
    // gate prescribes next) is the part a manager has to be able to read.
    assert!(
        text.contains("chore(orgasmic): dispatch record"),
        "the prose must name the record commit the close writes"
    );
    assert!(
        text.contains("makes =git merge= refuse"),
        "the prose must say why staging alone was not enough"
    );
    // TASK-QGWK7.1.1.1 F-7 / F-4: three properties of the record commit a
    // manager cannot derive from the files, and one sidecar value that means
    // two different things.
    assert!(
        text.contains("means the promote failed between the two renames"),
        "the prose must say what a non-zero stdout.log.bytes with no stdout.log means"
    );
    assert!(
        text.contains("are UNSIGNED"),
        "the prose must say record commits are unsigned, so a signature-enforcing repo knows"
    );
    assert!(
        text.contains("=--no-worktree-remove= close"),
        "the prose must say a promote-only close advances the branch too"
    );
    assert!(
        text.contains("64 KiB excerpt"),
        "the prose must state the real excerpt size"
    );
    // TASK-QGWK7.1.1.1.1 B-1/B-2: the sequencer list was prose only — nothing
    // pinned it, so it could drift from `sequencer_operation_in_progress` and
    // did (it promised a guarantee for `revert` the code did not provide).
    // The list itself is now compared against the code array, in order and both
    // ways, by `the_sequencer_list_matches_the_guards_array` below.
    for phrase in [
        // A CLEAN staged revert is refused too, not only a conflicted one.
        "A staged",
        "=git revert -n= counts",
        // B-2: which branch the repair lands on.
        "*The repair lands on",
        "the branch you are standing on when you re-run*",
    ] {
        assert!(
            text.contains(phrase),
            "the prose must state the persistence guarantee the code provides, and it is \
             missing {phrase:?}"
        );
    }
}

/// TASK-QGWK7.1.1.1.1.1.1 D-2. The list of operations a close stands down from
/// is a promise the convention makes on the code's behalf, so it has to be
/// checked against the code — BOTH ways.
///
/// The pin this replaces asserted `text.contains("rebase, am, merge, \
/// cherry-pick, revert, bisect")` and claimed in its own comment that "dropping
/// one from either side fails here". It did not: nothing read the code array at
/// all, so TASK-QGWK7.1.1.1.1.1 dropped the `sequencer` entry from the guard
/// and every convention test stayed green while the prose went on promising the
/// refusal. Comparing the parsed prose list against
/// [`sequencer_markers::SEQUENCER_MARKERS`] as an ordered sequence fails on a
/// drop from either side, and on a reorder.
#[test]
fn the_sequencer_list_matches_the_guards_array() {
    let text = manager_dispatch_convention();
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");

    // The convention wraps, so read the parenthetical off whitespace-collapsed
    // prose. The anchor is the sentence's own claim, not a line number.
    const ANCHOR: &str = "A close does not persist while a sequencer operation owns HEAD* (";
    let after = flat.split_once(ANCHOR).unwrap_or_else(|| {
        panic!("the convention must still say {ANCHOR:?} and follow it with the operation list")
    });
    let listed = after
        .1
        .split_once(')')
        .unwrap_or_else(|| panic!("the operation list after {ANCHOR:?} must be closed by `)`"))
        .0
        .split(", ")
        .map(|item| item.trim_start_matches("or ").trim())
        .collect::<Vec<_>>();

    let expected = sequencer_markers::SEQUENCER_MARKERS
        .iter()
        .map(|&(_, _, in_prose)| in_prose)
        .collect::<Vec<_>>();
    assert_eq!(
        listed, expected,
        "the convention's operation list and `sequencer_operation_in_progress`'s marker array \
         must name the same operations in the same order — dropping one from EITHER side, or \
         reordering, fails here"
    );

    // `sequencer` is checked last on purpose (every stopped pick carries the
    // todo list too), and it is the entry whose refusal branches to its own
    // remedy. Pin the position so a reorder cannot silently make an ordinary
    // stopped pick report as a leftover range.
    let (_, last_operation, _) = sequencer_markers::SEQUENCER_MARKERS
        .last()
        .copied()
        .expect("the guard must refuse on at least one marker");
    assert_eq!(
        last_operation,
        sequencer_markers::STOPPED_PICK_RANGE,
        "the coarse `.git/sequencer` marker must stay last, so a stopped pick is still named \
         by its own `*_HEAD` marker"
    );
}

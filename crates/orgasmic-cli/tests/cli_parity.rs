use std::collections::BTreeSet;
use std::path::PathBuf;

// orgasmic:task_K5NDR
#[path = "common/env_isolation.rs"]
mod env_isolation;
use env_isolation::orgasmic_command;

const ALLOWED_DEFERRED_LEAVES: &[&str] = &[];
const MAIN_RS: &str = include_str!("../src/main.rs");

/// ALL shipped prose an agent follows literally — entry text, prompt specs,
/// manager conventions, skill references. A command named in any of it that the
/// binary does not have is a runtime dead end with nothing else to catch it:
/// TASK-JQARS was filed because `orgasmic manager drivers` had been documented
/// for months while the CLI answered `unrecognized subcommand 'drivers'`, and
/// the manager who hit it fell back to reading Rust source.
///
/// TASK-HXSW0 widened this from `shipped/entry` alone, which is where that one
/// instance happened to be found; nothing had ever checked the rest.
const SHIPPED_DIR: &str = "shipped";

/// Below this, the extractor is not finding the corpus it thinks it is —
/// a silently-empty guard is the failure mode this whole test exists against.
const MINIMUM_SHIPPED_INVOCATIONS: usize = 100;

/// Floor on values the value-enum gate actually compared against clap.
///
/// orgasmic:TASK-RQ270.5.1 — `MINIMUM_SHIPPED_INVOCATIONS` alone was green while
/// the gate compared only 5 values (compact-layout help only). Assert against
/// comparisons, not invocation count, so a broken extractor goes red.
const MINIMUM_ENUM_VALUE_COMPARISONS: usize = 6;

#[test]
fn clap_leaf_commands_do_not_dispatch_to_not_implemented() {
    assert_sorted(ALLOWED_DEFERRED_LEAVES);

    let leaves = clap_leaf_paths();
    let leaf_set = leaves.iter().map(String::as_str).collect::<BTreeSet<_>>();
    let allow_set = ALLOWED_DEFERRED_LEAVES
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    let deferred = not_implemented_paths(MAIN_RS);

    let unknown = ALLOWED_DEFERRED_LEAVES
        .iter()
        .copied()
        .filter(|path| !leaf_set.contains(path))
        .collect::<Vec<_>>();
    assert!(
        unknown.is_empty(),
        "allow-list contains non-leaf command(s): {unknown:?}\nknown leaves: {leaves:#?}"
    );

    let unexpected = deferred
        .iter()
        .filter(|path| leaf_set.contains(path.as_str()) && !allow_set.contains(path.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    assert!(
        unexpected.is_empty(),
        "leaf command(s) still dispatch to not_implemented: {unexpected:?}"
    );

    let non_leaf_deferred = deferred
        .iter()
        .filter(|path| !leaf_set.contains(path.as_str()) && !allow_set.contains(path.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    assert!(
        non_leaf_deferred.is_empty(),
        "not_implemented command path(s) are not clap leaves: {non_leaf_deferred:?}"
    );
}

fn clap_leaf_paths() -> Vec<String> {
    let mut pending = vec![Vec::<String>::new()];
    let mut leaves = Vec::new();
    while let Some(path) = pending.pop() {
        let help = help_for(&path);
        let subcommands = subcommands_from_help(&help);
        if subcommands.is_empty() {
            if !path.is_empty() {
                leaves.push(format_command_path(&path));
            }
        } else {
            for subcommand in subcommands {
                let mut next = path.clone();
                next.push(subcommand);
                pending.push(next);
            }
        }
    }
    leaves.sort();
    leaves
}

fn help_for(path: &[String]) -> String {
    let output = orgasmic_command()
        .args(path)
        .arg("--help")
        .output()
        .expect("run orgasmic --help");
    assert!(
        output.status.success(),
        "help failed for {}: status={:?}\nstderr={}",
        format_command_path(path),
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("help output is utf-8")
}

fn subcommands_from_help(help: &str) -> Vec<String> {
    let mut in_commands = false;
    let mut out = Vec::new();
    for line in help.lines() {
        if line.trim() == "Commands:" {
            in_commands = true;
            continue;
        }
        if !in_commands {
            continue;
        }
        if line.trim().is_empty() || !line.starts_with("  ") {
            break;
        }
        let Some(name) = line.split_whitespace().next() else {
            continue;
        };
        if name != "help" {
            out.push(name.to_string());
        }
    }
    out.sort();
    out
}

fn not_implemented_paths(source: &str) -> Vec<String> {
    let mut out = source
        .lines()
        .filter_map(|line| {
            let start = line.find("not_implemented(")? + "not_implemented(".len();
            let rest = line[start..].trim_start();
            let rest = rest.strip_prefix('"')?;
            let end = rest.find('"')?;
            Some(rest[..end].to_string())
        })
        .collect::<Vec<_>>();
    out.sort();
    out
}

fn format_command_path(path: &[String]) -> String {
    if path.is_empty() {
        "orgasmic".to_string()
    } else {
        format!("orgasmic {}", path.join(" "))
    }
}

fn assert_sorted(values: &[&str]) {
    let mut sorted = values.to_vec();
    sorted.sort();
    assert_eq!(values, sorted, "allow-list must stay sorted");
}

/// Every `orgasmic ...` invocation written in shipped entry text must resolve
/// against the real binary — subcommand path and named flags.
///
/// This is the general defect TASK-JQARS was filed under: shipped prose and the
/// CLI surface drifted apart with nothing to catch it. The check runs the built
/// binary rather than reading clap source, so it fails the same way an agent
/// following the text would.
#[test]
fn shipped_entry_commands_resolve_against_the_cli() {
    let invocations = shipped_entry_invocations();
    assert!(
        invocations.len() >= MINIMUM_SHIPPED_INVOCATIONS,
        "only {} `orgasmic ...` invocations found under {SHIPPED_DIR}/ (expected at least \
         {MINIMUM_SHIPPED_INVOCATIONS}); the extractor is broken, not the prose",
        invocations.len()
    );

    let mut failures = Vec::new();
    for invocation in &invocations {
        for path in invocation.command_paths() {
            let help = match try_help(&path) {
                Ok(help) => help,
                Err(err) => {
                    failures.push(format!(
                        "{}: `{}` does not resolve ({err})",
                        invocation.source,
                        format_command_path(&path)
                    ));
                    continue;
                }
            };
            for flag in &invocation.flags {
                if !help_mentions_flag(&help, flag) {
                    failures.push(format!(
                        "{}: `{}` has no {flag} flag",
                        invocation.source,
                        format_command_path(&path)
                    ));
                }
            }
        }
    }

    assert!(
        failures.is_empty(),
        "shipped entry text names commands the CLI does not have:\n  {}",
        failures.join("\n  ")
    );
}

/// Value-enum arguments named in shipped prose must be in clap's
/// `possible_values` for that flag.
///
/// orgasmic:TASK-RQ270.5 — stage D retired `--class architecture` while
/// `manager-dispatch.org` still minted it; verb-path parity could not see the
/// drift because `orgasmic id mint` itself resolves.
///
/// orgasmic:TASK-RQ270.5.1 — long-help options put `[possible values: …]` on a
/// continuation line; treating that as free-string skipped the highest-traffic
/// enum sites. Flag-absent is now a failure, distinct from free-string.
#[test]
fn shipped_value_enum_arguments_match_clap_possible_values() {
    let invocations = shipped_entry_invocations();
    assert!(
        invocations.len() >= MINIMUM_SHIPPED_INVOCATIONS,
        "only {} `orgasmic ...` invocations found under {SHIPPED_DIR}/ (expected at least \
         {MINIMUM_SHIPPED_INVOCATIONS}); the extractor is broken, not the prose",
        invocations.len()
    );

    let mut failures = Vec::new();
    let mut compared = 0usize;
    for invocation in &invocations {
        if invocation.enum_values.is_empty() {
            continue;
        }
        for path in invocation.command_paths() {
            let help = match try_help(&path) {
                Ok(help) => help,
                Err(err) => {
                    failures.push(format!(
                        "{}: `{}` does not resolve ({err})",
                        invocation.source,
                        format_command_path(&path)
                    ));
                    continue;
                }
            };
            for (flag, value_token) in &invocation.enum_values {
                match possible_values_for_flag(&help, flag) {
                    FlagValueEnum::Absent => {
                        failures.push(format!(
                            "{}: `{}` has no {flag} flag in --help (value-enum check \
                             cannot skip a missing flag)",
                            invocation.source,
                            format_command_path(&path)
                        ));
                    }
                    FlagValueEnum::FreeString => {
                        // Genuine free-string flag (e.g. `--mode`): no bracket.
                    }
                    FlagValueEnum::Values(allowed) => {
                        for value in value_token.split('|') {
                            let value = value.trim();
                            if value.is_empty() || value.starts_with('<') || value.starts_with('[')
                            {
                                continue;
                            }
                            compared += 1;
                            if !allowed.iter().any(|a| a == value) {
                                failures.push(format!(
                                    "{}: `{} {} {}` is not in clap possible values {:?} for {}",
                                    invocation.source,
                                    format_command_path(&path),
                                    flag,
                                    value,
                                    allowed,
                                    flag
                                ));
                            }
                        }
                    }
                }
            }
        }
    }

    assert!(
        compared >= MINIMUM_ENUM_VALUE_COMPARISONS,
        "only {compared} value-enum comparisons ran against clap possible_values \
         (expected at least {MINIMUM_ENUM_VALUE_COMPARISONS}); the extractor or \
         help-entry parser is broken, not the prose"
    );
    assert!(
        failures.is_empty(),
        "shipped prose names value-enum arguments the CLI rejects:\n  {}",
        failures.join("\n  ")
    );
}

/// Every flag and positional on every leaf command must say what it does.
///
/// TASK-HXSW0's item 1: `manager dispatch --help` rendered `--task`, `--brief`,
/// `--from`, `--model`, `--effort`, `--worktree`, `--branch` and `--reason`
/// with BLANK descriptions, so the acceptance criterion — an agent discovers
/// any flag it needs from `--help` alone, without reading Rust source — was
/// unreachable on the primary agent verb. A blank `#[arg]` doc is a bug here,
/// not a style nit, and this is what makes it one.
#[test]
fn every_flag_and_argument_has_a_description() {
    let leaves = clap_leaf_paths();
    assert!(
        !leaves.is_empty(),
        "no leaf commands found; the help walker is broken"
    );
    let mut blank = Vec::new();
    for leaf in &leaves {
        let path = leaf
            .strip_prefix("orgasmic ")
            .unwrap_or("")
            .split(' ')
            .map(str::to_string)
            .collect::<Vec<_>>();
        for name in undocumented_help_entries(&help_for(&path)) {
            blank.push(format!("{leaf}: {name}"));
        }
    }
    assert!(
        blank.is_empty(),
        "{} flag(s)/argument(s) render with no description in --help:\n  {}",
        blank.len(),
        blank.join("\n  ")
    );
}

/// Flags and positionals in one rendered `--help` whose description is empty.
///
/// Reads the rendered help rather than the clap source on purpose: what an
/// agent can learn is what the binary prints, and clap renders short and long
/// help differently (description inline vs on its own indented line).
fn undocumented_help_entries(help: &str) -> Vec<String> {
    let lines = help.lines().collect::<Vec<_>>();
    let mut out = Vec::new();
    let mut in_section = false;
    for (index, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if trimmed == "Options:" || trimmed == "Arguments:" {
            in_section = true;
            continue;
        }
        if !in_section {
            continue;
        }
        if !trimmed.is_empty() && !line.starts_with(' ') {
            in_section = false;
            continue;
        }
        let Some((indent, name, rest)) = split_help_entry(line) else {
            continue;
        };
        if name == "--help" || name == "--version" {
            continue;
        }
        let mut description = rest.trim().to_string();
        if description.is_empty() {
            // Long-help style: the description sits on the following lines,
            // indented past the flag.
            for next in &lines[index + 1..] {
                let next_indent = next.len() - next.trim_start().len();
                if next.trim().is_empty() || next_indent <= indent {
                    break;
                }
                description.push_str(next.trim());
                description.push(' ');
            }
        }
        let description = description.trim();
        // `[possible values: …]` is clap's own value enumeration, not a
        // description of what the flag is for.
        if description.is_empty() || description.starts_with("[possible values") {
            out.push(name.to_string());
        }
    }
    out
}

/// `(indent, name, rest-of-line)` for a help line that introduces a flag or a
/// positional, else `None`.
fn split_help_entry(line: &str) -> Option<(usize, &str, &str)> {
    let indent = line.len() - line.trim_start().len();
    if indent == 0 || indent > 8 {
        return None;
    }
    let body = line.trim_start();
    // `-x, --long …` → drop the short alias.
    let body = match body.split_once(", --") {
        Some((short, tail)) if short.len() == 2 && short.starts_with('-') => {
            &line[line.len() - tail.len() - "--".len()..]
        }
        _ => body,
    };
    let (name, rest) = match body.split_once(char::is_whitespace) {
        Some(split) => split,
        None => (body, ""),
    };
    let is_flag = name.starts_with("--");
    let is_positional = name.starts_with('<') || name.starts_with('[');
    if !is_flag && !is_positional {
        return None;
    }
    // Skip the flag's own value placeholder (`--project <PROJECT>`) and any
    // further positionals on the same line (`<ID> <KEY> <VALUE>`).
    let mut rest = rest.trim_start();
    while rest.starts_with('<') || rest.starts_with('[') {
        let close = if rest.starts_with('<') { '>' } else { ']' };
        match rest.find(close) {
            Some(end) => rest = rest[end + 1..].trim_start(),
            None => break,
        }
    }
    Some((indent, name, rest))
}

/// Value-enum flags whose clap `possible_values` shipped prose must respect.
///
/// TASK-RQ270.5: verb-path parity alone was green while
/// `--class architecture` still shipped after stage D retired that class.
/// Only flags that clap renders with `[possible values: …]` are checked;
/// free-string flags such as `--mode` (driver catalog) are skipped.
///
/// Left hand-maintained (TASK-RQ270.5.1): deriving it from the binary's help
/// is possible now that entry parsing works, but would widen this fix; the
/// Absent-vs-FreeString split already stops silent skip on unknown flags.
const VALUE_ENUM_FLAGS: &[&str] = &["--class", "--kind", "--status", "--mode"];

#[test]
fn enum_values_in_tokens_accepts_equals_form() {
    let got = enum_values_in_tokens(&["id", "mint", "--class=architecture"]);
    assert_eq!(
        got,
        vec![("--class".to_string(), "architecture".to_string())]
    );
    let spaced = enum_values_in_tokens(&["id", "mint", "--class", "task"]);
    assert_eq!(spaced, vec![("--class".to_string(), "task".to_string())]);
}

#[test]
fn possible_values_for_flag_reads_multiline_help_entries() {
    // Compact (bracket on the flag line) and long-help (bracket on a
    // continuation line, possibly after a blank) must both resolve.
    let compact = "Options:\n      --class <CLASS>  Id class [possible values: task, decision]\n";
    match possible_values_for_flag(compact, "--class") {
        FlagValueEnum::Values(v) => assert_eq!(v, vec!["task", "decision"]),
        other => panic!("compact layout: {other:?}"),
    }

    let long = concat!(
        "Options:\n",
        "      --kind <KIND>\n",
        "          Worker persona\n",
        "\n",
        "          [possible values: implementer, reviewer]\n",
        "      --mode <MODE>\n",
        "          Free-string transport mode\n",
    );
    match possible_values_for_flag(long, "--kind") {
        FlagValueEnum::Values(v) => assert_eq!(v, vec!["implementer", "reviewer"]),
        other => panic!("long-help layout: {other:?}"),
    }
    match possible_values_for_flag(long, "--mode") {
        FlagValueEnum::FreeString => {}
        other => panic!("free-string should be FreeString, got {other:?}"),
    }
    match possible_values_for_flag(long, "--missing") {
        FlagValueEnum::Absent => {}
        other => panic!("missing flag should be Absent, got {other:?}"),
    }
}

/// One `orgasmic ...` invocation quoted in shipped prose.
struct EntryInvocation {
    /// `file:span` for the failure message.
    source: String,
    /// Subcommand words, each possibly an `a|b` alternation.
    words: Vec<String>,
    /// Long flags named after the subcommand path.
    flags: Vec<String>,
    /// `(flag, value-token)` pairs for value-enum flags, where the value may
    /// itself be an `a|b` alternation (`--class task|decision|architecture`).
    enum_values: Vec<(String, String)>,
}

impl EntryInvocation {
    /// Expand `a|b` alternations into one concrete command path each, so
    /// `orgasmic node body set|append` checks both.
    fn command_paths(&self) -> Vec<Vec<String>> {
        let mut paths = vec![Vec::<String>::new()];
        for word in &self.words {
            let alternatives = word.split('|').collect::<Vec<_>>();
            paths = paths
                .into_iter()
                .flat_map(|path| {
                    alternatives.iter().map(move |alt| {
                        let mut next = path.clone();
                        next.push((*alt).to_string());
                        next
                    })
                })
                .collect();
        }
        paths
    }
}

fn shipped_entry_invocations() -> Vec<EntryInvocation> {
    let root = repo_root().join(SHIPPED_DIR);
    let mut out = Vec::new();
    for path in shipped_prose_files(&root) {
        let name = path
            .strip_prefix(repo_root())
            .unwrap_or(&path)
            .to_string_lossy()
            .to_string();
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        let org = path.extension().and_then(|e| e.to_str()) == Some("org");
        for span in code_spans(&name, &text, org) {
            out.extend(invocations_in_span(&name, &span));
        }
    }
    out
}

/// Every prose file under `shipped/`, sorted, in the two markups it ships in.
fn shipped_prose_files(root: &std::path::Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let entries =
            std::fs::read_dir(&dir).unwrap_or_else(|e| panic!("read {}: {e}", dir.display()));
        for entry in entries {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                pending.push(path);
            } else if matches!(
                path.extension().and_then(|e| e.to_str()),
                Some("org") | Some("md")
            ) {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

/// Code spans in one shipped prose file, in the forms an agent reads a command
/// out of: fenced/`#+begin_src` blocks (one span per line, because each line is
/// its own command) and inline code (one span each, newline-flattened because
/// org prose wraps mid-span).
///
/// `=verbatim=` and `~code~` are included for org, which is how the manager
/// conventions spell every command they instruct.
/// `text` with fenced / `#+begin_src` regions blanked out, fences included.
/// Pass (2) of [`code_spans`] reads those regions line by line instead.
fn strip_code_blocks(text: &str, org: bool) -> String {
    let mut out = String::new();
    let mut in_block = false;
    for line in text.lines() {
        let trimmed = line.trim_start().to_ascii_lowercase();
        let fence = if org {
            trimmed.starts_with("#+begin_src")
                || trimmed.starts_with("#+begin_example")
                || trimmed.starts_with("#+end_src")
                || trimmed.starts_with("#+end_example")
        } else {
            trimmed.starts_with("```")
        };
        if fence {
            in_block = if org {
                trimmed.starts_with("#+begin_")
            } else {
                !in_block
            };
            out.push('\n');
            continue;
        }
        if !in_block {
            out.push_str(line);
        }
        out.push('\n');
    }
    out
}

fn code_spans(name: &str, text: &str, org: bool) -> Vec<String> {
    let mut out = Vec::new();

    // (1) Inline code, over the file flattened — org prose wraps mid-span, so
    // splitting on newlines first would cut commands in half. Block regions are
    // removed first: a Markdown fence is itself three backticks, so leaving
    // them in swallows the whole document into one span.
    let flat = strip_code_blocks(text, org).replace('\n', " ");
    assert_eq!(
        flat.matches('`').count() % 2,
        0,
        "{name} has an unbalanced inline-code backtick; the extractor cannot trust it"
    );
    out.extend(flat.split('`').skip(1).step_by(2).map(str::to_string));

    // (2) Block bodies, ONE SPAN PER LINE: each line of a fenced or `#+begin_src`
    // block is its own command, and flattening them would splice consecutive
    // commands into one nonexistent verb path.
    let mut in_block = false;
    let mut continued = false;
    for line in text.lines() {
        let trimmed = line.trim_start().to_ascii_lowercase();
        if org {
            if trimmed.starts_with("#+begin_src") || trimmed.starts_with("#+begin_example") {
                in_block = true;
                continued = false;
                continue;
            }
            if trimmed.starts_with("#+end_src") || trimmed.starts_with("#+end_example") {
                in_block = false;
                continued = false;
                continue;
            }
        } else if trimmed.starts_with("```") {
            in_block = !in_block;
            continued = false;
            continue;
        }
        if !in_block {
            continue;
        }
        // A trailing `\` continues one command onto the next line.
        if continued {
            if let Some(head) = out.last_mut() {
                head.push(' ');
                head.push_str(line.trim());
            }
        } else {
            out.push(line.trim().to_string());
        }
        continued = false;
        if let Some(head) = out.last_mut() {
            if head.ends_with('\\') {
                head.pop();
                continued = true;
            }
        }
    }

    // (3) Org `=verbatim=` / `~code~`, matched pairwise per LINE. Unbalanced
    // `=` is ordinary in org prose (`KEY=VALUE`), so a span is taken only when
    // the delimiter actually CLOSES on the same line — otherwise a wrapped
    // command would be read as a truncated one and blamed for a flag it never
    // finished spelling.
    if org {
        for line in text.lines() {
            for delimiter in ['=', '~'] {
                let parts = line.split(delimiter).collect::<Vec<_>>();
                for (index, part) in parts.iter().enumerate() {
                    if index % 2 == 1 && index + 1 < parts.len() {
                        out.push((*part).to_string());
                    }
                }
            }
        }
    }
    out
}

/// Pull every `orgasmic <words...> [--flags]` invocation out of one code span.
fn invocations_in_span(file: &str, span: &str) -> Vec<EntryInvocation> {
    let tokens = span.split_whitespace().collect::<Vec<_>>();
    let mut out = Vec::new();
    for (start, token) in tokens.iter().enumerate() {
        if *token != "orgasmic" {
            continue;
        }
        let rest = &tokens[start + 1..];
        let words = rest
            .iter()
            .take_while(|token| is_subcommand_word(token))
            .map(|token| (*token).to_string())
            .collect::<Vec<_>>();
        // Flags belong to this invocation until the next one starts.
        let owned: Vec<&str> = rest
            .iter()
            .take_while(|token| **token != "orgasmic")
            .copied()
            .collect();
        let flags = owned
            .iter()
            .filter_map(|token| long_flag(token))
            .collect::<Vec<_>>();
        let enum_values = enum_values_in_tokens(&owned);
        out.push(EntryInvocation {
            source: format!("{file}: `{}`", span.trim()),
            words,
            flags,
            enum_values,
        });
    }
    out
}

/// `(--flag, value)` pairs for [`VALUE_ENUM_FLAGS`] in one invocation's tokens.
fn enum_values_in_tokens(tokens: &[&str]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut index = 0;
    while index < tokens.len() {
        let trimmed =
            tokens[index].trim_matches(|c| matches!(c, '[' | ']' | '(' | ')' | ',' | '.' | ';'));

        // `--flag=value` (TASK-RQ270.5.1): long_flag alone rejects `=` in the
        // name, so fold the equals form here before looking at the next token.
        if let Some((flag_tok, raw)) = trimmed.split_once('=') {
            if let Some(flag) = long_flag(flag_tok) {
                if VALUE_ENUM_FLAGS.contains(&flag.as_str()) {
                    let value = raw.trim_matches(|c| {
                        matches!(c, '[' | ']' | '(' | ')' | ',' | '.' | ';' | '=')
                    });
                    if looks_like_enum_value(value) {
                        out.push((flag, value.to_string()));
                    }
                }
                index += 1;
                continue;
            }
        }

        let Some(flag) = long_flag(tokens[index]) else {
            index += 1;
            continue;
        };
        if !VALUE_ENUM_FLAGS.contains(&flag.as_str()) {
            index += 1;
            continue;
        }
        let Some(raw) = tokens.get(index + 1) else {
            break;
        };
        // Prose usually writes `--flag value` or `--flag a|b|c`.
        let value =
            raw.trim_matches(|c| matches!(c, '[' | ']' | '(' | ')' | ',' | '.' | ';' | '='));
        if value.is_empty() || long_flag(value).is_some() {
            index += 1;
            continue;
        }
        if looks_like_enum_value(value) {
            out.push((flag, value.to_string()));
            index += 2;
        } else {
            index += 1;
        }
    }
    out
}

/// A value token is letters/digits/dashes/underscores, or an a|b alternation of
/// those. Placeholders (`<CLASS>`) fail the char check.
fn looks_like_enum_value(value: &str) -> bool {
    !value.is_empty()
        && value.split('|').all(|part| {
            !part.is_empty()
                && part
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        })
}

/// Result of looking up a flag's clap `[possible values: …]` in rendered help.
///
/// orgasmic:TASK-RQ270.5.1 — `None` used to conflate "flag absent", "free-string",
/// and "multi-line help layout". Those are three different outcomes.
#[derive(Debug)]
enum FlagValueEnum {
    /// Flag does not appear as an Options/Arguments entry in this help.
    Absent,
    /// Flag is present but clap rendered no `[possible values: …]` bracket.
    FreeString,
    Values(Vec<String>),
}

/// Clap's `[possible values: a, b, c]` list for one flag in a `--help` blob.
///
/// Parses help as option *entries* via [`split_help_entry`]: clap's long-help
/// layout puts the bracket on a continuation line under the flag, so a
/// same-line-only scan silently treated every long-help enum as free-string.
fn possible_values_for_flag(help: &str, flag: &str) -> FlagValueEnum {
    let lines = help.lines().collect::<Vec<_>>();
    let mut in_section = false;
    for (index, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if trimmed == "Options:" || trimmed == "Arguments:" {
            in_section = true;
            continue;
        }
        if !in_section {
            continue;
        }
        if !trimmed.is_empty() && !line.starts_with(' ') {
            in_section = false;
            continue;
        }
        let Some((indent, name, rest)) = split_help_entry(line) else {
            continue;
        };
        if name != flag {
            continue;
        }
        // Whole entry: flag line remainder plus continuation lines until the
        // next entry at the same indent. Blank lines inside the entry stay
        // part of it — clap often puts `[possible values: …]` after a blank.
        let mut entry = rest.to_string();
        for next in &lines[index + 1..] {
            if next.trim().is_empty() {
                continue;
            }
            let next_indent = next.len() - next.trim_start().len();
            if next_indent <= indent {
                break;
            }
            entry.push(' ');
            entry.push_str(next.trim());
        }
        return match entry.find("[possible values:") {
            Some(start) => match parse_possible_values_bracket(&entry[start..]) {
                Some(values) => FlagValueEnum::Values(values),
                None => FlagValueEnum::FreeString,
            },
            None => FlagValueEnum::FreeString,
        };
    }
    FlagValueEnum::Absent
}

fn parse_possible_values_bracket(fragment: &str) -> Option<Vec<String>> {
    let start = fragment.find("[possible values:")? + "[possible values:".len();
    let rest = fragment[start..].trim_start();
    let end = rest.find(']')?;
    let inner = rest[..end].trim();
    if inner.is_empty() {
        return Some(Vec::new());
    }
    Some(
        inner
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
    )
}

/// A bare subcommand word: lowercase, possibly an `a|b` alternation. Anything
/// else (a flag, a `<placeholder>`, shell punctuation, `...`) ends the path.
fn is_subcommand_word(token: &str) -> bool {
    !token.is_empty()
        && token.split('|').all(|part| {
            !part.is_empty()
                && part.starts_with(|c: char| c.is_ascii_lowercase())
                && part
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        })
}

/// `[--model` / `--kind` → `--model` / `--kind`; `--flag=value` → `--flag`.
fn long_flag(token: &str) -> Option<String> {
    let trimmed = token.trim_matches(|c| matches!(c, '[' | ']' | '(' | ')' | ',' | '.' | ';'));
    let name = trimmed.strip_prefix("--")?;
    let name = name.split_once('=').map(|(n, _)| n).unwrap_or(name);
    if name.is_empty()
        || !name.starts_with(|c: char| c.is_ascii_lowercase())
        || !name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return None;
    }
    Some(format!("--{name}"))
}

fn help_mentions_flag(help: &str, flag: &str) -> bool {
    help.split(|c: char| c.is_whitespace() || c == ',' || c == '=')
        .any(|token| token == flag)
}

fn try_help(path: &[String]) -> Result<String, String> {
    let output = orgasmic_command()
        .args(path)
        .arg("--help")
        .output()
        .map_err(|e| format!("spawn failed: {e}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr)
            .lines()
            .next()
            .unwrap_or("no stderr")
            .to_string());
    }
    String::from_utf8(output.stdout).map_err(|e| format!("help output is not utf-8: {e}"))
}

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

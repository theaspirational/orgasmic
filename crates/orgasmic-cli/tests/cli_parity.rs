use std::collections::BTreeSet;
use std::path::PathBuf;
use std::process::Command;

const ALLOWED_DEFERRED_LEAVES: &[&str] = &[];
const MAIN_RS: &str = include_str!("../src/main.rs");

/// Shipped entry text an agent follows literally. A command named here that the
/// binary does not have is a runtime dead end with nothing else to catch it:
/// TASK-JQARS was filed because `orgasmic manager drivers` had been documented
/// here for months while the CLI answered `unrecognized subcommand 'drivers'`,
/// and the manager who hit it fell back to reading Rust source.
const SHIPPED_ENTRY_DIR: &str = "shipped/entry";

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
    let output = Command::new(env!("CARGO_BIN_EXE_orgasmic"))
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
        !invocations.is_empty(),
        "no `orgasmic ...` invocations found in {SHIPPED_ENTRY_DIR}; the extractor is broken, \
         not the prose"
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

/// One `orgasmic ...` invocation quoted in shipped prose.
struct EntryInvocation {
    /// `file:span` for the failure message.
    source: String,
    /// Subcommand words, each possibly an `a|b` alternation.
    words: Vec<String>,
    /// Long flags named after the subcommand path.
    flags: Vec<String>,
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
    let dir = repo_root().join(SHIPPED_ENTRY_DIR);
    let mut entries = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
        .map(|entry| entry.expect("dir entry").path())
        .filter(|path| path.extension().and_then(|e| e.to_str()) == Some("org"))
        .collect::<Vec<_>>();
    entries.sort();

    let mut out = Vec::new();
    for path in entries {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("<entry>")
            .to_string();
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        // Inline code spans may wrap across lines in org prose.
        let flat = text.replace('\n', " ");
        assert_eq!(
            flat.matches('`').count() % 2,
            0,
            "{name} has an unbalanced inline-code backtick; the extractor cannot trust it"
        );
        for span in flat.split('`').skip(1).step_by(2) {
            out.extend(invocations_in_span(&name, span));
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
        let flags = rest
            .iter()
            .take_while(|token| **token != "orgasmic")
            .filter_map(|token| long_flag(token))
            .collect::<Vec<_>>();
        out.push(EntryInvocation {
            source: format!("{file}: `{}`", span.trim()),
            words,
            flags,
        });
    }
    out
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

/// `[--model` / `--kind` → `--model` / `--kind`; anything else → `None`.
fn long_flag(token: &str) -> Option<String> {
    let trimmed = token.trim_matches(|c| matches!(c, '[' | ']' | '(' | ')' | ',' | '.' | ';'));
    let name = trimmed.strip_prefix("--")?;
    if name.is_empty()
        || !name.starts_with(|c: char| c.is_ascii_lowercase())
        || !name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return None;
    }
    Some(trimmed.to_string())
}

fn help_mentions_flag(help: &str, flag: &str) -> bool {
    help.split(|c: char| c.is_whitespace() || c == ',' || c == '=')
        .any(|token| token == flag)
}

fn try_help(path: &[String]) -> Result<String, String> {
    let output = Command::new(env!("CARGO_BIN_EXE_orgasmic"))
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

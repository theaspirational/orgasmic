use std::collections::BTreeSet;
use std::process::Command;

const ALLOWED_DEFERRED_LEAVES: &[&str] = &[];
const MAIN_RS: &str = include_str!("../src/main.rs");

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

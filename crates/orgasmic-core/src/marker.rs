//! Canonical advisory code-marker grammar shared with `scripts/id-migration.py`.
//!
//! Markers are `// orgasmic:<ids>` or `# orgasmic:<ids>` line comments. Only
//! structured payloads (comma-separated ids with optional `:opt_` suffixes) are
//! indexed; free-text tails and markers embedded in string literals are ignored.

/// Crockford-safe marker id byte (includes `.` for subtask markers like `task_CJWT3.1`).
pub fn is_marker_id_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b':' | b'_' | b'-' | b'.')
}

/// True when `before` ends at a line-comment token (`//` or `#`) with only whitespace
/// between the token and the marker.
pub fn has_comment_token_before_marker(before: &str) -> bool {
    let slash = before.rfind("//").map(|pos| (pos, 2));
    let hash = before.rfind('#').map(|pos| (pos, 1));
    let token = match (slash, hash) {
        (Some(left), Some(right)) => Some(if left.0 > right.0 { left } else { right }),
        (Some(token), None) | (None, Some(token)) => Some(token),
        (None, None) => None,
    };
    let Some((pos, len)) = token else {
        return false;
    };
    before[pos + len..].chars().all(char::is_whitespace)
}

/// True when `payload` is only comma-separated marker ids (plus optional `:opt_` suffixes).
pub fn is_structured_marker_payload(payload: &str) -> bool {
    let bytes = payload.as_bytes();
    let mut pos = 0;
    let mut saw_id = false;
    loop {
        while pos < bytes.len() && bytes[pos].is_ascii_whitespace() {
            pos += 1;
        }
        let start = pos;
        while pos < bytes.len() && is_marker_id_byte(bytes[pos]) {
            pos += 1;
        }
        if start == pos {
            break;
        }
        saw_id = true;
        while pos < bytes.len() && bytes[pos].is_ascii_whitespace() {
            pos += 1;
        }
        if pos < bytes.len() && bytes[pos] == b',' {
            pos += 1;
            continue;
        }
        break;
    }
    if !saw_id {
        return false;
    }
    payload[pos..].trim().is_empty()
}

fn strip_marker_option_suffix(id: &str) -> &str {
    if let Some((node_id, option_id)) = id.rsplit_once(':') {
        if option_id.starts_with("opt_") {
            return node_id;
        }
    }
    id
}

/// Parse comma-separated marker ids from a structured payload.
pub fn parse_marker_payload(payload: &str) -> Vec<String> {
    let bytes = payload.as_bytes();
    let mut pos = 0;
    let mut ids = Vec::new();

    loop {
        while pos < bytes.len() && bytes[pos].is_ascii_whitespace() {
            pos += 1;
        }
        let start = pos;
        while pos < bytes.len() && is_marker_id_byte(bytes[pos]) {
            pos += 1;
        }
        if start == pos {
            break;
        }
        ids.push(strip_marker_option_suffix(&payload[start..pos]).to_string());
        while pos < bytes.len() && bytes[pos].is_ascii_whitespace() {
            pos += 1;
        }
        if pos < bytes.len() && bytes[pos] == b',' {
            pos += 1;
            continue;
        }
        break;
    }

    ids
}

/// Normalize a parsed marker token to the node id used for graph lookup.
pub fn normalize_marker_node_id(raw: &str) -> String {
    if let Some(rest) = raw.strip_prefix("task_") {
        return format!("TASK-{rest}");
    }
    if let Some((base, suffix)) = raw.rsplit_once('.') {
        if raw.starts_with("arch_") && suffix.chars().all(|c| c.is_ascii_digit()) {
            return base.to_string();
        }
    }
    raw.to_string()
}

fn comment_scan_region<'a>(line: &'a str, ext: Option<&str>) -> Option<&'a str> {
    let trimmed = line.trim_start();
    if trimmed.starts_with("//") || trimmed.starts_with('#') {
        return Some(line);
    }
    match ext {
        Some("py" | "sh" | "bash" | "zsh") => comment_region_from_hash(line),
        _ => comment_region_from_slash_slash(line),
    }
}

fn comment_region_from_slash_slash(line: &str) -> Option<&str> {
    let mut in_single = false;
    let mut in_double = false;
    let mut escape = false;
    let mut last_comment_start = None;
    for (idx, ch) in line.char_indices() {
        if escape {
            escape = false;
            continue;
        }
        match ch {
            '\\' if in_double || in_single => escape = true,
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            '/' if !in_single && !in_double => {
                if line[idx..].starts_with("//") {
                    last_comment_start = Some(idx);
                }
            }
            _ => {}
        }
    }
    last_comment_start.map(|start| &line[start..])
}

fn comment_region_from_hash(line: &str) -> Option<&str> {
    let mut in_single = false;
    let mut in_double = false;
    let mut escape = false;
    for (idx, ch) in line.char_indices() {
        if escape {
            escape = false;
            continue;
        }
        match ch {
            '\\' if in_double || in_single => escape = true,
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            '#' if !in_single && !in_double => return Some(&line[idx..]),
            _ => {}
        }
    }
    None
}

/// Extract normalized node ids from one source line (comment regions only).
pub fn marker_node_ids_in_line(line: &str, ext: Option<&str>) -> Vec<String> {
    let Some(region) = comment_scan_region(line, ext) else {
        return Vec::new();
    };
    let mut ids = Vec::new();
    let mut search_start = 0;
    while let Some(offset) = region[search_start..].find("orgasmic:") {
        let marker_start = search_start + offset;
        if !has_comment_token_before_marker(&region[..marker_start]) {
            search_start = marker_start + "orgasmic:".len();
            continue;
        }
        let payload = &region[marker_start + "orgasmic:".len()..];
        if !is_structured_marker_payload(payload) {
            search_start = marker_start + "orgasmic:".len();
            continue;
        }
        for raw in parse_marker_payload(payload) {
            ids.push(normalize_marker_node_id(&raw));
        }
        search_start = marker_start + "orgasmic:".len();
    }
    ids
}

/// True when marker scans should skip this path (mirrors id-migration.py).
pub fn should_skip_marker_path(rel: &str) -> bool {
    let skip_prefixes = [
        "archive/",
        "target/",
        ".git/",
        "node_modules/",
        "ui/dist/",
        ".orgasmic/tmp/",
    ];
    if skip_prefixes
        .iter()
        .any(|prefix| rel.starts_with(prefix) || rel.trim_end_matches('/') == *prefix)
    {
        return true;
    }
    if rel.contains("/__pycache__/") || rel.ends_with(".pyc") {
        return true;
    }
    if rel.starts_with("crates/") && rel.contains("/tests/") {
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn structured_payload_rejects_free_text_tail() {
        assert!(!is_structured_marker_payload(
            "task_156 — free-text comment must stay"
        ));
        assert!(is_structured_marker_payload("task_HC7PW,dec_QWEQ8"));
    }

    #[test]
    fn subtask_marker_keeps_dot_suffix() {
        let ids = marker_node_ids_in_line("// orgasmic:task_CJWT3.1", Some("ts"));
        assert_eq!(ids, vec!["TASK-CJWT3.1"]);
    }

    #[test]
    fn arch_subtask_marker_normalizes_to_parent() {
        let ids = marker_node_ids_in_line("// orgasmic:arch_WZFAX.1", Some("rs"));
        assert_eq!(ids, vec!["arch_WZFAX"]);
    }

    #[test]
    fn string_literal_marker_is_ignored() {
        let line = r#"    let payload = "// orgasmic:task_999\n";"#;
        assert!(marker_node_ids_in_line(line, Some("rs")).is_empty());
    }

    #[test]
    fn tests_directory_is_skipped() {
        assert!(should_skip_marker_path("crates/demo/tests/fixture.rs"));
        assert!(!should_skip_marker_path("crates/demo/src/lib.rs"));
    }
}

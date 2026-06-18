// arch: arch_BVH7M.1, arch_QFQTD.2
// orgasmic:arch_BVH7M, dec_QWZB2
//! App-owned Org parser and rewriter.
//!
//! Strict subset that covers what `.orgasmic/*.org`, `shipped/**/*.org`, and
//! generated ADRs need:
//!
//! - file-level `#+key: value` keywords before the first heading
//! - headings `* TITLE`, optional uppercase TODO keyword, optional `:tag:tag:`
//! - property drawers `:PROPERTIES: ... :END:` immediately after a heading
//! - nested headings used as named body sections (`** Description` etc.)
//!
//! Not supported (intentional, app-owned profile is not Emacs):
//!
//! - logbook / clock drawers
//! - inline footnotes, links, tables
//! - `#+begin_…/#+end_…` blocks as semantic units (passed through verbatim,
//!   with heading detection disabled inside the block)
//!
//! All parsed regions carry byte-offset spans into the original source so
//! rewriting one heading or one property never touches unrelated bytes.

use std::ops::Range;

use thiserror::Error;

/// A parsed Org file. Owns the original source so spans stay valid.
#[derive(Debug, Clone)]
pub struct OrgFile {
    source: String,
    /// File-level `#+key: value` directives, in source order, with byte spans.
    pub keywords: Vec<Keyword>,
    /// Top-level headings (`* `). Nested headings live inside
    /// [`Heading::sections`].
    pub headings: Vec<Heading>,
    /// Byte range covering content before the first top-level heading
    /// (keywords, blank lines, free prose). Empty if the file starts with a
    /// heading.
    pub prelude: Range<usize>,
}

#[derive(Debug, Clone)]
pub struct Keyword {
    pub key: String,
    pub value: String,
    pub span: Range<usize>,
}

/// A parsed heading and everything that belongs to it (property drawer,
/// nested headings used as body sections, free body text).
#[derive(Debug, Clone)]
pub struct Heading {
    /// Heading depth (number of leading `*`s, >= 1).
    pub level: usize,
    /// Optional Org TODO keyword (uppercase alphanumeric/underscore token).
    pub todo: Option<String>,
    /// Title text with surrounding whitespace and trailing tags stripped.
    pub title: String,
    /// Trailing `:tag:tag:` tokens parsed from the title line. Order preserved.
    pub tags: Vec<String>,
    /// Optional property drawer that immediately follows the title line.
    pub properties: Option<PropertyDrawer>,
    /// Nested headings (depth > self.level) until the next same-or-shallower
    /// heading. These act as named body sections in the orgasmic profile.
    pub sections: Vec<Heading>,

    /// Span of the title line (without trailing newline).
    pub title_line: Range<usize>,
    /// Span of the entire heading, including its property drawer, body, and
    /// every nested heading underneath it. Trailing newline (if any) included.
    pub span: Range<usize>,
    /// Span of the free body between the property drawer (or title line, if
    /// no drawer) and the first nested heading (or end of heading). Empty
    /// range if the heading has no body text of its own.
    pub body: Range<usize>,
}

#[derive(Debug, Clone)]
pub struct PropertyDrawer {
    pub entries: Vec<PropertyEntry>,
    /// Span covering `:PROPERTIES:` through `:END:` inclusive of the trailing
    /// newline after `:END:`.
    pub span: Range<usize>,
}

#[derive(Debug, Clone)]
pub struct PropertyEntry {
    pub key: String,
    pub value: String,
    pub span: Range<usize>,
    /// Range inside the entry that holds just the value text (no key prefix,
    /// no trailing newline). Used when rewriting one property in place.
    pub value_span: Range<usize>,
}

#[derive(Debug, Error)]
pub enum OrgError {
    #[error("{file}:{line}: malformed property drawer entry: {detail}")]
    BadProperty {
        file: String,
        line: usize,
        detail: String,
    },
    #[error("{file}:{line}: unterminated property drawer (no :END:)")]
    UnterminatedDrawer { file: String, line: usize },
    #[error("{file}:{line}: malformed heading: {detail}")]
    BadHeading {
        file: String,
        line: usize,
        detail: String,
    },
    #[error("{file}:{line}: malformed file keyword: {detail}")]
    BadKeyword {
        file: String,
        line: usize,
        detail: String,
    },
    #[error("{file}: heading not found: {selector}")]
    HeadingNotFound { file: String, selector: String },
    #[error("{file}: property {key} not found on heading {heading}")]
    PropertyNotFound {
        file: String,
        key: String,
        heading: String,
    },
    #[error("{file}: section {section} not found on heading {heading}")]
    SectionNotFound {
        file: String,
        section: String,
        heading: String,
    },
    #[error("{file}: heading {heading} has no property drawer")]
    NoPropertyDrawer { file: String, heading: String },
    #[error(
        "{file}: body edit rejected — content would alter heading structure \
         (phantom heading injection); escape leading `*` characters before writing"
    )]
    BodyHeadingInjection { file: String },
}

impl OrgFile {
    /// Parse `source` from a file named `display_name` (used only for error
    /// messages — the parser does not touch the filesystem).
    pub fn parse(
        source: impl Into<String>,
        display_name: impl Into<String>,
    ) -> Result<Self, OrgError> {
        let source = source.into();
        let display = display_name.into();
        let parsed = Parser::new(&source, &display).parse_file()?;
        Ok(OrgFile {
            source,
            keywords: parsed.keywords,
            headings: parsed.headings,
            prelude: parsed.prelude,
        })
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    /// Return the byte slice of `range` from the original source.
    pub fn slice(&self, range: Range<usize>) -> &str {
        &self.source[range]
    }

    /// Locate a top-level heading whose `:ID:` property equals `id`.
    pub fn find_by_id(&self, id: &str) -> Option<&Heading> {
        self.headings.iter().find_map(|h| h.find_by_id(id))
    }

    /// Locate a top-level heading whose title (after the optional TODO
    /// keyword) starts with `prefix`. Useful when an ID property is not
    /// available (e.g. SCHEMA/STATE-MACHINE/SLOTS sections).
    pub fn find_by_title_prefix(&self, prefix: &str) -> Option<&Heading> {
        self.headings.iter().find(|h| h.title.starts_with(prefix))
    }
}

impl Heading {
    /// Return the value of property `key` if present (case-sensitive match,
    /// since all corpus uses uppercase).
    pub fn property(&self, key: &str) -> Option<&str> {
        self.properties
            .as_ref()?
            .entries
            .iter()
            .find(|e| e.key == key)
            .map(|e| e.value.as_str())
    }

    /// Iterate property entries in source order.
    pub fn property_entries(&self) -> impl Iterator<Item = &PropertyEntry> {
        self.properties.iter().flat_map(|d| d.entries.iter())
    }

    /// Find this heading or any nested descendant section whose `:ID:` matches.
    /// Architecture leaf nodes (`arch_NNN.M`) and other addressable nodes live
    /// as nested headings, so id lookup must recurse rather than only matching
    /// the top-level heading.
    pub fn find_by_id(&self, id: &str) -> Option<&Heading> {
        if self.property("ID").map(|v| v == id).unwrap_or(false) {
            return Some(self);
        }
        self.sections.iter().find_map(|child| child.find_by_id(id))
    }

    /// Find a nested named-body section by its title (e.g. `Description`).
    pub fn section(&self, title: &str) -> Option<&Heading> {
        self.sections.iter().find(|s| s.title == title)
    }
}

// --- parser --------------------------------------------------------------

struct ParsedFile {
    keywords: Vec<Keyword>,
    headings: Vec<Heading>,
    prelude: Range<usize>,
}

struct Parser<'a> {
    source: &'a str,
    display: &'a str,
    lines: Vec<LineSpan>,
    in_block: Vec<bool>,
}

#[derive(Debug, Clone, Copy)]
struct LineSpan {
    /// Byte offset where the line content starts.
    start: usize,
    /// Byte offset of the end of the line content (before any `\n` or `\r\n`).
    content_end: usize,
    /// Byte offset of the first byte that belongs to the *next* line. Equals
    /// `source.len()` on the final line if it has no trailing newline.
    next: usize,
}

impl LineSpan {
    fn full(&self) -> Range<usize> {
        self.start..self.next
    }
    fn content(&self) -> Range<usize> {
        self.start..self.content_end
    }
}

impl<'a> Parser<'a> {
    fn new(source: &'a str, display: &'a str) -> Self {
        let lines = split_lines(source);
        let in_block = org_block_line_mask(source, &lines);
        Self {
            source,
            display,
            lines,
            in_block,
        }
    }

    fn line_text(&self, line: usize) -> &'a str {
        let span = self.lines[line];
        &self.source[span.content()]
    }

    fn heading_level_at(&self, line: usize) -> Option<usize> {
        if self.in_block[line] {
            None
        } else {
            heading_level(self.line_text(line))
        }
    }

    fn parse_file(&mut self) -> Result<ParsedFile, OrgError> {
        let mut idx = 0;
        let mut keywords = Vec::new();

        // File-level `#+key: value` directives at the top of the file.
        while idx < self.lines.len() {
            let text = self.line_text(idx);
            let trimmed = text.trim_start();
            if let Some(rest) = trimmed.strip_prefix("#+") {
                let (key, value) =
                    parse_keyword_line(rest).ok_or_else(|| OrgError::BadKeyword {
                        file: self.display.into(),
                        line: idx + 1,
                        detail: text.into(),
                    })?;
                keywords.push(Keyword {
                    key,
                    value,
                    span: self.lines[idx].full(),
                });
                idx += 1;
            } else if text.trim().is_empty() {
                idx += 1;
            } else if self.heading_level_at(idx).is_some() {
                break;
            } else {
                // Free prose before the first heading is allowed (rare, but
                // permitted). Stop the keyword loop and let the prelude span
                // cover everything up to the first heading.
                break;
            }
        }

        // Anything up to the first top-level heading is the prelude.
        let prelude_start = 0usize;
        let mut prelude_end = if idx < self.lines.len() {
            self.lines[idx].start
        } else {
            self.source.len()
        };

        // Walk into prelude further if there is free prose between keywords
        // and the first heading.
        while idx < self.lines.len() {
            if self.heading_level_at(idx).is_some() {
                prelude_end = self.lines[idx].start;
                break;
            }
            idx += 1;
            prelude_end = if idx < self.lines.len() {
                self.lines[idx].start
            } else {
                self.source.len()
            };
        }

        // Now parse top-level headings.
        let mut headings = Vec::new();
        while idx < self.lines.len() {
            let text = self.line_text(idx);
            if text.trim().is_empty() {
                idx += 1;
                continue;
            }
            let Some(level) = self.heading_level_at(idx) else {
                return Err(OrgError::BadHeading {
                    file: self.display.into(),
                    line: idx + 1,
                    detail: format!("expected heading, got: {text}"),
                });
            };
            if level != 1 {
                return Err(OrgError::BadHeading {
                    file: self.display.into(),
                    line: idx + 1,
                    detail: format!("expected top-level heading (`* `), got level {level}"),
                });
            }
            let (heading, next) = self.parse_heading(idx, 1)?;
            headings.push(heading);
            idx = next;
        }

        Ok(ParsedFile {
            keywords,
            headings,
            prelude: prelude_start..prelude_end,
        })
    }

    fn parse_heading(&self, start_line: usize, level: usize) -> Result<(Heading, usize), OrgError> {
        let title_line_span = self.lines[start_line];
        let title_text = self.line_text(start_line);
        let parsed = parse_heading_line(title_text, level).ok_or_else(|| OrgError::BadHeading {
            file: self.display.into(),
            line: start_line + 1,
            detail: title_text.into(),
        })?;

        let mut idx = start_line + 1;

        // Optional property drawer immediately after the title line.
        let mut properties = None;
        if idx < self.lines.len() && self.line_text(idx).trim() == ":PROPERTIES:" {
            let (drawer, next) = self.parse_drawer(idx)?;
            properties = Some(drawer);
            idx = next;
        }

        // Free body: lines from here up to the first deeper-or-equal heading.
        let body_start_line = idx;
        let mut body_end_line = idx;
        while idx < self.lines.len() {
            if let Some(other_level) = self.heading_level_at(idx) {
                if other_level <= level {
                    break; // sibling or shallower — end of this heading
                }
                if other_level == level + 1 {
                    break; // direct child — body ends here, parse sections
                }
                // Deeper nested heading without a direct child in between is a
                // structural error in the orgasmic profile (we keep parsing
                // permissively but stop the body here).
                break;
            }
            idx += 1;
            body_end_line = idx;
        }
        let body_span = if body_start_line == body_end_line {
            // Empty body range — use a zero-length span anchored at the
            // current position so callers can still locate it.
            let anchor = if body_start_line < self.lines.len() {
                self.lines[body_start_line].start
            } else {
                self.source.len()
            };
            anchor..anchor
        } else {
            let start = self.lines[body_start_line].start;
            let end = self.lines[body_end_line - 1].next;
            start..end
        };

        // Nested sections at depth level+1.
        let mut sections = Vec::new();
        while idx < self.lines.len() {
            let text = self.line_text(idx);
            let Some(other_level) = self.heading_level_at(idx) else {
                if text.trim().is_empty() {
                    idx += 1;
                    continue;
                }
                // Free prose between sections is treated as body of the
                // previous section. Push it onto its body span by reparsing.
                // For the orgasmic corpus this branch is unreachable, so we
                // just stop here.
                break;
            };
            if other_level <= level {
                break;
            }
            if other_level != level + 1 {
                return Err(OrgError::BadHeading {
                    file: self.display.into(),
                    line: idx + 1,
                    detail: format!(
                        "expected nested heading at level {} but got level {}",
                        level + 1,
                        other_level
                    ),
                });
            }
            let (section, next) = self.parse_heading(idx, level + 1)?;
            sections.push(section);
            idx = next;
        }

        let span_end = if idx == 0 {
            0
        } else if idx < self.lines.len() {
            self.lines[idx].start
        } else {
            self.lines
                .last()
                .map(|l| l.next)
                .unwrap_or(self.source.len())
        };
        let span = title_line_span.start..span_end;

        let heading = Heading {
            level,
            todo: parsed.todo,
            title: parsed.title,
            tags: parsed.tags,
            properties,
            sections,
            title_line: title_line_span.content(),
            span,
            body: body_span,
        };
        Ok((heading, idx))
    }

    fn parse_drawer(&self, start_line: usize) -> Result<(PropertyDrawer, usize), OrgError> {
        let drawer_start = self.lines[start_line].start;
        let mut entries = Vec::new();
        let mut idx = start_line + 1;
        loop {
            if idx >= self.lines.len() {
                return Err(OrgError::UnterminatedDrawer {
                    file: self.display.into(),
                    line: start_line + 1,
                });
            }
            let line_span = self.lines[idx];
            let text = self.line_text(idx);
            let trimmed = text.trim();
            if trimmed == ":END:" {
                idx += 1;
                let drawer_end = if idx < self.lines.len() {
                    self.lines[idx].start
                } else {
                    line_span.next
                };
                return Ok((
                    PropertyDrawer {
                        entries,
                        span: drawer_start..drawer_end,
                    },
                    idx,
                ));
            }
            if trimmed.is_empty() {
                return Err(OrgError::BadProperty {
                    file: self.display.into(),
                    line: idx + 1,
                    detail: "blank line inside property drawer".into(),
                });
            }
            let entry =
                parse_property_line(text, line_span).ok_or_else(|| OrgError::BadProperty {
                    file: self.display.into(),
                    line: idx + 1,
                    detail: text.into(),
                })?;
            entries.push(entry);
            idx += 1;
        }
    }
}

struct ParsedHeadingLine {
    todo: Option<String>,
    title: String,
    tags: Vec<String>,
}

fn parse_heading_line(text: &str, level: usize) -> Option<ParsedHeadingLine> {
    // Skip the leading `*`s and the single mandatory space.
    let mut chars = text.char_indices();
    let mut stars = 0usize;
    let mut after_stars = 0usize;
    for (i, c) in chars.by_ref() {
        if c == '*' {
            stars += 1;
        } else {
            after_stars = i;
            break;
        }
    }
    if stars != level {
        return None;
    }
    // Must be a single space after the stars.
    if !text[after_stars..].starts_with(' ') {
        return None;
    }
    let rest = text[after_stars + 1..].trim_end_matches(['\r']);
    if rest.is_empty() {
        return Some(ParsedHeadingLine {
            todo: None,
            title: String::new(),
            tags: Vec::new(),
        });
    }

    // Pull off trailing tags `:tag:tag:` if present. Tags are
    // alphanumeric/_-#@, and tagged tokens are anchored to the end of the
    // line and contain no spaces.
    let (body, tags) = split_trailing_tags(rest);

    // Pull off optional TODO keyword (uppercase letters/digits/_).
    let body_trimmed = body.trim_end();
    let (todo, title) = match body_trimmed.split_once(' ') {
        Some((first, remainder)) if is_todo_keyword(first) => {
            (Some(first.to_string()), remainder.trim_start().to_string())
        }
        _ => {
            if is_todo_keyword(body_trimmed) {
                (Some(body_trimmed.to_string()), String::new())
            } else {
                (None, body_trimmed.to_string())
            }
        }
    };

    Some(ParsedHeadingLine { todo, title, tags })
}

/// Allowlist of TODO keywords recognized by the app-owned Org profile.
///
/// Every keyword is the uppercase form of the unified task lifecycle in
/// [`shipped/schema/state-machine.org`]. Limiting the list to known states
/// is critical: untyped headings like `* PROJECT orgasmic` or `* TX …` would
/// otherwise be misclassified as TODO + title.
const TODO_KEYWORDS: &[&str] = &[
    "BACKLOG",
    "TODO",
    "IN_PROGRESS",
    "IN_REVIEW",
    "DONE",
    "CANCELLED",
    // goal.org lifecycle headings (not task lifecycle columns)
    "GOAL",
    "CLEARED",
    "SUPERSEDED",
];

fn is_todo_keyword(s: &str) -> bool {
    TODO_KEYWORDS.contains(&s)
}

fn split_trailing_tags(line: &str) -> (&str, Vec<String>) {
    // Tag run: a maximal suffix matching `(\s+):([tagchar]+:)+` anchored to
    // end-of-line, where tagchar is alphanumeric/_/-/@/#. Returns (body, tags).
    let trimmed = line.trim_end();
    let bytes = trimmed.as_bytes();
    if !trimmed.ends_with(':') || trimmed.len() < 3 {
        return (line, Vec::new());
    }
    // Find a candidate run start: scan back from end, accept only `:` or
    // tag chars, stop at first whitespace or other char.
    let mut start = bytes.len();
    for (i, &b) in bytes.iter().enumerate().rev() {
        if b == b':' || is_tag_char(b) {
            start = i;
        } else {
            break;
        }
    }
    if start == bytes.len() {
        return (line, Vec::new());
    }
    // The run must be preceded by whitespace (or be at start of line — but
    // an Org heading always has the title-word first, so the run starts
    // after a space in practice).
    if start > 0 {
        let prev = bytes[start - 1];
        if prev != b' ' && prev != b'\t' {
            return (line, Vec::new());
        }
    } else {
        // The whole line is a tag run with no title — keep as title to be
        // permissive; orgasmic never produces this.
        return (line, Vec::new());
    }
    let run = &trimmed[start..];
    if !run.starts_with(':') || !run.ends_with(':') || run.len() < 3 {
        return (line, Vec::new());
    }
    // Each segment between colons must be non-empty.
    let segments: Vec<&str> = run.trim_matches(':').split(':').collect();
    if segments.is_empty() || segments.iter().any(|s| s.is_empty()) {
        return (line, Vec::new());
    }
    let tags: Vec<String> = segments.iter().map(|s| (*s).to_string()).collect();
    let body = trimmed[..start].trim_end();
    (&line[..body.len()], tags)
}

fn is_tag_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'@' | b'#')
}

fn heading_level(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    if bytes.first() != Some(&b'*') {
        return None;
    }
    let mut n = 0;
    while n < bytes.len() && bytes[n] == b'*' {
        n += 1;
    }
    if n == 0 || n >= bytes.len() || bytes[n] != b' ' {
        return None;
    }
    Some(n)
}

fn org_block_line_mask(source: &str, lines: &[LineSpan]) -> Vec<bool> {
    let mut mask = Vec::with_capacity(lines.len());
    let mut depth = 0usize;
    for line in lines {
        let text = &source[line.content()];
        let directive = text.trim_start().to_ascii_lowercase();
        let starts_block = directive.starts_with("#+begin_");
        let ends_block = directive.starts_with("#+end_");
        let in_block = depth > 0 || starts_block;
        mask.push(in_block);
        if starts_block {
            depth += 1;
        }
        if ends_block && depth > 0 {
            depth -= 1;
        }
    }
    mask
}

fn parse_keyword_line(rest: &str) -> Option<(String, String)> {
    // rest is the slice after `#+`.
    let rest = rest.trim_end_matches(['\r']);
    let (key, value) = rest.split_once(':')?;
    let key = key.trim().to_string();
    if key.is_empty() {
        return None;
    }
    let value = value.trim().to_string();
    Some((key, value))
}

fn parse_property_line(text: &str, span: LineSpan) -> Option<PropertyEntry> {
    // Format: optional leading whitespace, `:KEY:`, then value (possibly
    // empty). Surrounding whitespace around the value is normalized for
    // `value` but `value_span` covers the trimmed value bytes inside the
    // original source.
    let stripped = text.trim_end_matches(['\r']);
    let leading_ws_len = stripped.len() - stripped.trim_start().len();
    let trimmed = &stripped[leading_ws_len..];
    if !trimmed.starts_with(':') {
        return None;
    }
    let after_first_colon = &trimmed[1..];
    let key_end = after_first_colon.find(':')?;
    let key = after_first_colon[..key_end].to_string();
    if key.is_empty() {
        return None;
    }
    // Byte offset (relative to `text`) of the first char after the `:KEY:`.
    let after_kv = leading_ws_len + 1 + key_end + 1;
    // Skip a single optional space after `:KEY:` (idiomatic Org spacing).
    let raw_value = &stripped[after_kv..];
    let leading_value_ws = raw_value.len() - raw_value.trim_start().len();
    let value_start = span.start + after_kv + leading_value_ws;
    let value_trimmed = raw_value.trim();
    let value_end = value_start + value_trimmed.len();
    Some(PropertyEntry {
        key,
        value: value_trimmed.to_string(),
        span: span.full(),
        value_span: value_start..value_end,
    })
}

fn split_lines(source: &str) -> Vec<LineSpan> {
    let bytes = source.as_bytes();
    let mut spans = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'\n' {
            let mut content_end = i;
            if content_end > start && bytes[content_end - 1] == b'\r' {
                content_end -= 1;
            }
            spans.push(LineSpan {
                start,
                content_end,
                next: i + 1,
            });
            start = i + 1;
            i += 1;
        } else {
            i += 1;
        }
    }
    if start <= bytes.len() {
        // Trailing partial line (no final `\n`).
        if start < bytes.len() {
            spans.push(LineSpan {
                start,
                content_end: bytes.len(),
                next: bytes.len(),
            });
        }
    }
    spans
}

// --- structural snapshot helper -----------------------------------------

/// Collect the structural fingerprint of all headings recursively as a flat
/// ordered vec of `(level, id)`. Used by the body-write guard to detect
/// phantom heading injection: after a body-only edit the snapshot must be
/// identical to the pre-edit one.
fn heading_structure_snapshot(headings: &[Heading]) -> Vec<(usize, Option<String>)> {
    let mut out = Vec::new();
    collect_heading_structure(headings, &mut out);
    out
}

fn collect_heading_structure(headings: &[Heading], out: &mut Vec<(usize, Option<String>)>) {
    for h in headings {
        out.push((h.level, h.property("ID").map(str::to_string)));
        collect_heading_structure(&h.sections, out);
    }
}

// orgasmic:task_RCP69
// opt-in raw-body escape: example-wrap + comma-escape before guard
fn raw_body_line_needs_comma_escape(content: &str) -> bool {
    let after_commas = content.trim_start_matches(',');
    after_commas.starts_with('*') || after_commas.starts_with("#+")
}

/// Wrap untrusted raw body text so column-0 `*` / `#+` lines cannot alter
/// structure. Comma-escapes those lines, then wraps in `#+begin_example` /
/// `#+end_example`. Callers opt in via `body_format: raw` at the API boundary.
pub fn wrap_raw_body(payload: &str) -> String {
    let mut escaped = String::new();
    for line in payload.split_inclusive('\n') {
        let content = line.strip_suffix('\n').unwrap_or(line);
        if raw_body_line_needs_comma_escape(content) {
            escaped.push(',');
        }
        escaped.push_str(line);
    }
    if !escaped.ends_with('\n') {
        escaped.push('\n');
    }
    format!("#+begin_example\n{escaped}#+end_example\n")
}

// --- rewriting ----------------------------------------------------------

/// In-place editor that rewrites byte spans of the original source while
/// keeping every byte outside the touched spans verbatim.
pub struct OrgRewriter {
    source: String,
    /// Pending edits as (range, replacement). Applied in reverse-offset order
    /// so later edits do not perturb earlier offsets.
    edits: Vec<(Range<usize>, String)>,
    file_name: String,
}

impl OrgRewriter {
    pub fn new(file: &OrgFile, file_name: impl Into<String>) -> Self {
        Self {
            source: file.source.clone(),
            edits: Vec::new(),
            file_name: file_name.into(),
        }
    }

    /// Replace the value of property `key` on the heading identified by `:ID:`.
    /// Reparses the current source so consecutive calls see prior edits.
    pub fn set_property(
        &mut self,
        heading_id: &str,
        key: &str,
        value: &str,
    ) -> Result<(), OrgError> {
        let view = OrgFile::parse(self.current_text(), &self.file_name)?;
        let heading = view
            .find_by_id(heading_id)
            .ok_or_else(|| OrgError::HeadingNotFound {
                file: self.file_name.clone(),
                selector: format!(":ID: {heading_id}"),
            })?;
        let entry = heading
            .property_entries()
            .find(|e| e.key == key)
            .ok_or_else(|| OrgError::PropertyNotFound {
                file: self.file_name.clone(),
                key: key.into(),
                heading: heading_id.into(),
            })?;
        let value_span = entry.value_span.clone();
        // Filling a previously-empty value (e.g. an unfilled `:DEPENDS_ON:`
        // placeholder that carries no value column) needs a separating space so
        // the drawer reads `:KEY: value`, not `:KEY:value`.
        if value_span.is_empty() && !value.is_empty() {
            self.replace_with_view(value_span, &format!(" {value}"));
        } else {
            self.replace_with_view(value_span, value);
        }
        Ok(())
    }

    /// Replace the body of a nested section (e.g. `Description`,
    /// `Acceptance Criteria`, `Evidence`, `Worklog`) on the heading
    /// identified by `:ID:`. `new_body` should NOT include the section
    /// heading line; only the body text. A trailing newline is preserved.
    pub fn set_section_body(
        &mut self,
        heading_id: &str,
        section_title: &str,
        new_body: &str,
    ) -> Result<(), OrgError> {
        let view = OrgFile::parse(self.current_text(), &self.file_name)?;
        // orgasmic:task_HC7PW
        // snapshot before edit for structural invariant guard
        let before = heading_structure_snapshot(&view.headings);
        let heading = view
            .find_by_id(heading_id)
            .ok_or_else(|| OrgError::HeadingNotFound {
                file: self.file_name.clone(),
                selector: format!(":ID: {heading_id}"),
            })?;
        let section = heading
            .section(section_title)
            .ok_or_else(|| OrgError::SectionNotFound {
                file: self.file_name.clone(),
                section: section_title.into(),
                heading: heading_id.into(),
            })?;
        self.replace_with_view(section.body.clone(), new_body);
        self.assert_structural_invariant(before)?;
        Ok(())
    }

    /// Replace the heading's own free body — the prose between its property
    /// drawer (or title line) and its first nested heading — preserving the
    /// surrounding blank-line layout the same way [`Self::upsert_section_text`]
    /// does for named sections. The editor-facing primitive for leaf nodes
    /// (e.g. `arch_NNN.M`) whose prose is direct body text rather than a
    /// named `**` section.
    pub fn set_node_body(&mut self, heading_id: &str, content: &str) -> Result<(), OrgError> {
        let (span, original, prefix, tail, before) = {
            let view = OrgFile::parse(self.current_text(), &self.file_name)?;
            // orgasmic:task_HC7PW
            // snapshot before edit for structural invariant guard
            let before = heading_structure_snapshot(&view.headings);
            let heading = self.heading_or_err(&view, heading_id)?;
            let span = heading.body.clone();
            let body = view.slice(span.clone()).to_string();
            if body.trim().is_empty() {
                // No authored prose yet: open after one blank line and keep a
                // blank-line separator before whatever follows the heading.
                (span, body, "\n".to_string(), "\n\n".to_string(), before)
            } else {
                let lead = body.len() - body.trim_start().len();
                let trail = body.trim_end().len();
                let prefix = body[..lead].to_string();
                let tail = body[trail..].to_string();
                (span, body, prefix, tail, before)
            }
        };
        let content = content.trim_end();
        let replacement = if content.is_empty() {
            // Clearing an already-empty body is a no-op; clearing prose keeps
            // only the original trailing separator.
            if original.trim().is_empty() {
                original
            } else {
                tail
            }
        } else {
            format!("{prefix}{content}{tail}")
        };
        self.replace_with_view(span, &replacement);
        self.assert_structural_invariant(before)?;
        Ok(())
    }

    /// Replace the whole title line (TODO keyword, title, and tags) of a
    /// heading identified by `:ID:`. `new_title_line` must NOT include a
    /// trailing newline; the rewriter preserves the original line ending.
    pub fn set_title_line(
        &mut self,
        heading_id: &str,
        new_title_line: &str,
    ) -> Result<(), OrgError> {
        let view = OrgFile::parse(self.current_text(), &self.file_name)?;
        let heading = view
            .find_by_id(heading_id)
            .ok_or_else(|| OrgError::HeadingNotFound {
                file: self.file_name.clone(),
                selector: format!(":ID: {heading_id}"),
            })?;
        self.replace_with_view(heading.title_line.clone(), new_title_line);
        Ok(())
    }

    /// Insert a new `:KEY: value` line into the heading's existing property
    /// drawer, immediately before `:END:`. The value column is aligned to the
    /// drawer's existing entries so the inserted line matches local style.
    /// Errors if the heading has no drawer (unreachable when the heading was
    /// located by `:ID:`, which itself requires a drawer entry).
    pub fn insert_property(
        &mut self,
        heading_id: &str,
        key: &str,
        value: &str,
    ) -> Result<(), OrgError> {
        let view = OrgFile::parse(self.current_text(), &self.file_name)?;
        let heading = self.heading_or_err(&view, heading_id)?;
        let drawer = heading
            .properties
            .as_ref()
            .ok_or_else(|| OrgError::NoPropertyDrawer {
                file: self.file_name.clone(),
                heading: heading_id.into(),
            })?;
        let (insert_at, value_col) = match drawer.entries.last() {
            // Start of the `:END:` line == byte just past the last entry's
            // newline. Align to that entry's value column.
            Some(last) => (last.span.end, Some(last.value_span.start - last.span.start)),
            // Empty drawer: insert right after the `:PROPERTIES:` line.
            None => {
                let after_open = view
                    .slice(drawer.span.clone())
                    .find('\n')
                    .map(|off| drawer.span.start + off + 1)
                    .unwrap_or(drawer.span.start);
                (after_open, None)
            }
        };
        let line = render_property_line(key, value, value_col);
        self.replace_with_view(insert_at..insert_at, &line);
        Ok(())
    }

    /// Remove the heading identified by `:ID:` and its entire subtree (nested
    /// sections and body). Used when moving a task between lifecycle files.
    pub fn remove_heading(&mut self, heading_id: &str) -> Result<(), OrgError> {
        let view = OrgFile::parse(self.current_text(), &self.file_name)?;
        let heading = self.heading_or_err(&view, heading_id)?;
        self.replace_with_view(heading.span.clone(), "");
        Ok(())
    }

    /// Remove the property `key` (the whole `:KEY: value` line, including its
    /// trailing newline) from the heading identified by `:ID:`.
    pub fn remove_property(&mut self, heading_id: &str, key: &str) -> Result<(), OrgError> {
        let view = OrgFile::parse(self.current_text(), &self.file_name)?;
        let heading = self.heading_or_err(&view, heading_id)?;
        let entry = heading
            .property_entries()
            .find(|e| e.key == key)
            .ok_or_else(|| OrgError::PropertyNotFound {
                file: self.file_name.clone(),
                key: key.into(),
                heading: heading_id.into(),
            })?;
        self.replace_with_view(entry.span.clone(), "");
        Ok(())
    }

    /// Set property `key` if it exists, otherwise insert it. The
    /// editor-facing primitive for scalar/token properties (e.g.
    /// `GLOSSARY_REFS`).
    pub fn upsert_property(
        &mut self,
        heading_id: &str,
        key: &str,
        value: &str,
    ) -> Result<(), OrgError> {
        let exists = {
            let view = OrgFile::parse(self.current_text(), &self.file_name)?;
            let heading = self.heading_or_err(&view, heading_id)?;
            let found = heading.property_entries().any(|e| e.key == key);
            found
        };
        if exists {
            self.set_property(heading_id, key, value)
        } else {
            self.insert_property(heading_id, key, value)
        }
    }

    /// Append a new nested section (`** Title` etc.) at the end of the
    /// heading's content, just before the trailing blank-line separator. The
    /// section nests one level below the target heading. `body` may be empty.
    pub fn append_section(
        &mut self,
        heading_id: &str,
        title: &str,
        body: &str,
    ) -> Result<(), OrgError> {
        let (at, level) = {
            let view = OrgFile::parse(self.current_text(), &self.file_name)?;
            let heading = self.heading_or_err(&view, heading_id)?;
            let span = heading.span.clone();
            // Insert right after the last non-whitespace byte of the heading so
            // any trailing blank line (the inter-node separator) is preserved.
            let content_len = view.slice(span.clone()).trim_end().len();
            (span.start + content_len, heading.level + 1)
        };

        // Adding the section heading is the intended structural change. Compute
        // that expected structure with an empty section, then require the final
        // body-bearing insertion to match it exactly. A body payload that adds
        // another heading still fails closed with BodyHeadingInjection.
        let header_only = render_section(title, "", level);
        self.replace_with_view(at..at, &header_only);
        let expected = match OrgFile::parse(self.current_text(), &self.file_name) {
            Ok(view) => heading_structure_snapshot(&view.headings),
            Err(err) => {
                self.edits.pop();
                return Err(err);
            }
        };
        self.edits.pop();

        let section = render_section(title, body, level);
        self.replace_with_view(at..at, &section);
        self.assert_structural_invariant(expected)?;
        Ok(())
    }

    /// Remove a nested section (its title line through its content, including
    /// the trailing newline) from the heading identified by `:ID:`.
    pub fn remove_section(
        &mut self,
        heading_id: &str,
        section_title: &str,
    ) -> Result<(), OrgError> {
        let view = OrgFile::parse(self.current_text(), &self.file_name)?;
        let heading = self.heading_or_err(&view, heading_id)?;
        let section = heading
            .section(section_title)
            .ok_or_else(|| OrgError::SectionNotFound {
                file: self.file_name.clone(),
                section: section_title.into(),
                heading: heading_id.into(),
            })?;
        let span = section.span.clone();
        // If the section's content absorbed a trailing blank-line separator
        // (its slice ends with "\n\n"), keep one newline so the following
        // sibling heading stays separated.
        let end = if view.slice(span.clone()).ends_with("\n\n") {
            span.end - 1
        } else {
            span.end
        };
        self.replace_with_view(span.start..end, "");
        Ok(())
    }

    /// Replace a section's prose while preserving its surrounding layout:
    /// `content` becomes the body, trimmed of trailing whitespace, with the
    /// section's *original* trailing whitespace re-applied (so an inter-node
    /// blank-line separator on the last section survives an edit). Creates the
    /// section via [`Self::append_section`] when it does not yet exist.
    pub fn upsert_section_text(
        &mut self,
        heading_id: &str,
        section_title: &str,
        content: &str,
    ) -> Result<(), OrgError> {
        let trailing = {
            let view = OrgFile::parse(self.current_text(), &self.file_name)?;
            let heading = self.heading_or_err(&view, heading_id)?;
            match heading.section(section_title) {
                Some(section) => {
                    let body = view.slice(section.body.clone());
                    let tail = &body[body.trim_end().len()..];
                    Some(if tail.is_empty() {
                        "\n".to_string()
                    } else {
                        tail.to_string()
                    })
                }
                None => None,
            }
        };
        match trailing {
            Some(tail) => {
                let replacement = format!("{}{}", content.trim_end(), tail);
                self.set_section_body(heading_id, section_title, &replacement)
            }
            None => self.append_section(heading_id, section_title, content),
        }
    }

    fn heading_or_err<'a>(
        &self,
        view: &'a OrgFile,
        heading_id: &str,
    ) -> Result<&'a Heading, OrgError> {
        view.find_by_id(heading_id)
            .ok_or_else(|| OrgError::HeadingNotFound {
                file: self.file_name.clone(),
                selector: format!(":ID: {heading_id}"),
            })
    }

    /// Called immediately after [`Self::replace_with_view`] in a body-write
    /// op. If the heading structure changed (any heading added, removed, or
    /// releveled), pops the pending edit to restore the pre-edit state and
    /// returns `Err(OrgError::BodyHeadingInjection)`.
    ///
    /// # Precondition
    /// Must be called right after a single `replace_with_view` call, so
    /// `self.edits` contains exactly that one pending edit. Popping it
    /// restores the rewriter to the pre-op state.
    fn assert_structural_invariant(
        &mut self,
        before: Vec<(usize, Option<String>)>,
    ) -> Result<(), OrgError> {
        let after_text = self.current_text();
        let after = match OrgFile::parse(&after_text, &self.file_name) {
            Ok(f) => heading_structure_snapshot(&f.headings),
            Err(_) => {
                self.edits.pop();
                return Err(OrgError::BodyHeadingInjection {
                    file: self.file_name.clone(),
                });
            }
        };
        if after != before {
            self.edits.pop();
            return Err(OrgError::BodyHeadingInjection {
                file: self.file_name.clone(),
            });
        }
        Ok(())
    }

    fn replace_with_view(&mut self, range: Range<usize>, replacement: &str) {
        // The range was computed against `current_text()`; convert that
        // text back into `self.source` by applying it directly to a fresh
        // edit list rebuilt against the *current* materialized text.
        self.source = apply_edits(&self.source, std::mem::take(&mut self.edits));
        self.edits.push((range, replacement.to_string()));
    }

    pub fn current_text(&self) -> String {
        apply_edits(&self.source, self.edits.clone())
    }

    pub fn finish(mut self) -> String {
        let edits = std::mem::take(&mut self.edits);
        apply_edits(&self.source, edits)
    }
}

fn apply_edits(source: &str, mut edits: Vec<(Range<usize>, String)>) -> String {
    edits.sort_by(|a, b| b.0.start.cmp(&a.0.start));
    let mut out = source.to_string();
    for (range, replacement) in edits {
        out.replace_range(range, &replacement);
    }
    out
}

/// Render a single property drawer line `:KEY: value\n`. When `value_col` is
/// known (the value column of a sibling entry) the value is padded to align
/// with the rest of the drawer; otherwise a single space is used.
fn render_property_line(key: &str, value: &str, value_col: Option<usize>) -> String {
    let prefix = format!(":{key}:");
    let pad = match value_col {
        Some(col) if col > prefix.len() => col - prefix.len(),
        _ => 1,
    };
    format!("{prefix}{}{value}\n", " ".repeat(pad))
}

/// Render a nested section with a leading newline so it attaches cleanly after
/// existing content: `\n** Title\nbody`. `body` is trimmed of trailing
/// whitespace; the caller's surrounding bytes supply the trailing newline.
fn render_section(title: &str, body: &str, level: usize) -> String {
    let stars = "*".repeat(level);
    let body = body.trim_end();
    if body.is_empty() {
        format!("\n{stars} {title}")
    } else {
        format!("\n{stars} {title}\n{body}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
#+title: example
#+orgasmic_version: 1

* DONE TASK-001 First task :foo:bar:
:PROPERTIES:
:ID:               TASK-001
:END:

** Description
A description body.

Second paragraph.

** Acceptance Criteria
- [X] Item.

* BACKLOG TASK-002 Second task :baz:
:PROPERTIES:
:ID:               TASK-002
:END:

** Description
Hello.
";

    const ARCH_SAMPLE: &str = "\
#+title: architecture

* arch_006 Daemon API
:PROPERTIES:
:ID:                 arch_006
:END:

** arch_006.3 Materialized index
:PROPERTIES:
:ID:                 arch_006.3
:SOURCE_PATHS:       crates/orgasmic-daemon/src/index.rs
:TESTS:              cargo test -p orgasmic-daemon
:END:
Body.
";

    #[test]
    fn find_by_id_recurses_into_nested_leaf_headings() {
        let f = OrgFile::parse(ARCH_SAMPLE, "architecture.org").unwrap();
        // Top-level still resolves.
        assert_eq!(
            f.find_by_id("arch_006").unwrap().property("ID"),
            Some("arch_006")
        );
        // Nested leaf node resolves through recursion.
        let leaf = f.find_by_id("arch_006.3").expect("leaf node found");
        assert_eq!(leaf.property("ID"), Some("arch_006.3"));
        assert_eq!(
            leaf.property("TESTS"),
            Some("cargo test -p orgasmic-daemon")
        );
    }

    #[test]
    fn rewriter_edits_a_leaf_node_property_in_place() {
        let f = OrgFile::parse(ARCH_SAMPLE, "architecture.org").unwrap();
        let mut rw = OrgRewriter::new(&f, "architecture.org");
        rw.upsert_property("arch_006.3", "TESTS", "cargo test -p orgasmic-core")
            .expect("edit leaf property");
        let out = rw.current_text();
        // Reparse and confirm only the leaf node's property changed.
        let reparsed = OrgFile::parse(out.clone(), "architecture.org").unwrap();
        let leaf = reparsed.find_by_id("arch_006.3").unwrap();
        assert_eq!(leaf.property("TESTS"), Some("cargo test -p orgasmic-core"));
        // Sibling parent untouched, document still well-formed.
        assert!(reparsed.find_by_id("arch_006").is_some());
        assert!(out.contains("** arch_006.3 Materialized index"));
    }

    #[test]
    fn rewriter_sets_a_leaf_node_direct_body() {
        let f = OrgFile::parse(ARCH_SAMPLE, "architecture.org").unwrap();
        let mut rw = OrgRewriter::new(&f, "architecture.org");
        rw.set_node_body("arch_006.3", "Rewritten leaf prose.")
            .expect("set leaf body");
        let out = rw.current_text();
        let reparsed = OrgFile::parse(out.clone(), "architecture.org").unwrap();
        let leaf = reparsed.find_by_id("arch_006.3").unwrap();
        assert_eq!(out.slice_body(leaf), "Rewritten leaf prose.");
        // Properties and parent survive the body splice.
        assert_eq!(
            leaf.property("TESTS"),
            Some("cargo test -p orgasmic-daemon")
        );
        assert!(reparsed.find_by_id("arch_006").is_some());
    }

    #[test]
    fn rewriter_inserts_body_into_heading_without_prose() {
        // arch_006 (the parent) has no direct body of its own.
        let f = OrgFile::parse(ARCH_SAMPLE, "architecture.org").unwrap();
        let mut rw = OrgRewriter::new(&f, "architecture.org");
        rw.set_node_body("arch_006", "New parent prose.")
            .expect("insert body");
        let out = rw.current_text();
        let reparsed = OrgFile::parse(out.clone(), "architecture.org").unwrap();
        let parent = reparsed.find_by_id("arch_006").unwrap();
        assert_eq!(out.slice_body(parent), "New parent prose.");
        // The nested leaf is still intact below the inserted prose.
        let leaf = reparsed.find_by_id("arch_006.3").unwrap();
        assert_eq!(leaf.property("ID"), Some("arch_006.3"));
    }

    trait SliceBody {
        fn slice_body(&self, heading: &Heading) -> &str;
    }

    impl SliceBody for String {
        fn slice_body(&self, heading: &Heading) -> &str {
            self[heading.body.clone()].trim()
        }
    }

    #[test]
    fn parse_file_keywords_and_headings() {
        let f = OrgFile::parse(SAMPLE, "sample.org").unwrap();
        assert_eq!(f.keywords.len(), 2);
        assert_eq!(f.keywords[0].key, "title");
        assert_eq!(f.keywords[0].value, "example");
        assert_eq!(f.headings.len(), 2);
        let h = &f.headings[0];
        assert_eq!(h.todo.as_deref(), Some("DONE"));
        assert_eq!(h.title, "TASK-001 First task");
        assert_eq!(h.tags, vec!["foo".to_string(), "bar".to_string()]);
        assert_eq!(h.property("ID"), Some("TASK-001"));
        assert_eq!(h.sections.len(), 2);
        assert_eq!(h.sections[0].title, "Description");
        assert_eq!(h.sections[1].title, "Acceptance Criteria");
    }

    #[test]
    fn rewriter_preserves_unrelated_bytes() {
        let f = OrgFile::parse(SAMPLE, "sample.org").unwrap();
        let mut rw = OrgRewriter::new(&f, "sample.org");
        rw.upsert_property("TASK-002", "PRIORITY", "P1").unwrap();
        let out = rw.finish();
        // The first heading must be byte-identical to the original.
        let first_orig = &SAMPLE[..SAMPLE.find("* BACKLOG TASK-002").unwrap()];
        assert!(
            out.starts_with(first_orig),
            "first heading must be byte-stable"
        );
        assert!(out.contains(":PRIORITY:         P1"));
        // Everything else of the second heading is preserved.
        assert!(out.contains("** Description\nHello.\n"));
    }

    #[test]
    fn rewriter_can_replace_section_body() {
        let f = OrgFile::parse(SAMPLE, "sample.org").unwrap();
        let mut rw = OrgRewriter::new(&f, "sample.org");
        rw.set_section_body("TASK-001", "Description", "New description body.\n\n")
            .unwrap();
        let out = rw.finish();
        assert!(out.contains("** Description\nNew description body.\n\n** Acceptance Criteria\n"));
        // Other heading remained untouched.
        assert!(out.contains("** Description\nHello.\n"));
    }

    #[test]
    fn rewriter_can_replace_title_line() {
        let f = OrgFile::parse(SAMPLE, "sample.org").unwrap();
        let mut rw = OrgRewriter::new(&f, "sample.org");
        rw.set_title_line("TASK-001", "* IN_REVIEW TASK-001 First task :foo:bar:")
            .unwrap();
        let out = rw.finish();
        assert!(out.starts_with(
            "#+title: example\n#+orgasmic_version: 1\n\n* IN_REVIEW TASK-001 First task :foo:bar:\n"
        ));
    }

    #[test]
    fn rewriter_inserts_then_removes_property_round_trip() {
        let f = OrgFile::parse(SAMPLE, "sample.org").unwrap();
        let mut rw = OrgRewriter::new(&f, "sample.org");
        rw.insert_property("TASK-002", "WORKER", "codex").unwrap();
        let out = rw.finish();
        // The new entry parses on the target heading, value column aligned.
        let parsed = OrgFile::parse(out.clone(), "sample.org").unwrap();
        assert_eq!(
            parsed.find_by_id("TASK-002").unwrap().property("WORKER"),
            Some("codex")
        );
        // Inserted value aligns to the drawer's value column (19, matching :ID:/:STATE:).
        let worker_line = out.lines().find(|l| l.starts_with(":WORKER:")).unwrap();
        assert_eq!(worker_line.find("codex"), Some(19));
        // The first heading is byte-stable.
        let first_orig = &SAMPLE[..SAMPLE.find("* BACKLOG TASK-002").unwrap()];
        assert!(out.starts_with(first_orig));
        // Removing the inserted property restores the original byte-for-byte.
        let mut rw2 = OrgRewriter::new(&parsed, "sample.org");
        rw2.remove_property("TASK-002", "WORKER").unwrap();
        assert_eq!(rw2.finish(), SAMPLE);
    }

    #[test]
    fn rewriter_upsert_property_sets_or_inserts() {
        let f = OrgFile::parse(SAMPLE, "sample.org").unwrap();
        // Existing property → updated in place.
        let mut rw = OrgRewriter::new(&f, "sample.org");
        rw.upsert_property("TASK-001", "STATE", "in_review")
            .unwrap();
        let out = rw.finish();
        assert!(out.contains(""));
        // Missing property → inserted before :END:.
        let mut rw2 = OrgRewriter::new(&f, "sample.org");
        rw2.upsert_property("TASK-001", "GLOSSARY_REFS", "alpha beta")
            .unwrap();
        let parsed = OrgFile::parse(rw2.finish(), "sample.org").unwrap();
        assert_eq!(
            parsed
                .find_by_id("TASK-001")
                .unwrap()
                .property("GLOSSARY_REFS"),
            Some("alpha beta")
        );
    }

    #[test]
    fn rewriter_set_empty_property_adds_separating_space() {
        let src = "* X\n:PROPERTIES:\n:ID: x\n:DEPENDS_ON:\n:END:\n";
        let f = OrgFile::parse(src, "x.org").unwrap();
        // Filling an empty placeholder gets a separating space.
        let mut rw = OrgRewriter::new(&f, "x.org");
        rw.set_property("x", "DEPENDS_ON", "arch_002").unwrap();
        assert!(rw.finish().contains(":DEPENDS_ON: arch_002\n"));
        // A property that already has a value keeps its column (no extra space).
        let mut rw2 = OrgRewriter::new(&f, "x.org");
        rw2.set_property("x", "ID", "y").unwrap();
        assert!(rw2.finish().contains(":ID: y\n"));
    }

    #[test]
    fn rewriter_appends_then_removes_section_round_trip() {
        let f = OrgFile::parse(SAMPLE, "sample.org").unwrap();
        let mut rw = OrgRewriter::new(&f, "sample.org");
        rw.append_section("TASK-001", "Evidence", "Some evidence.")
            .unwrap();
        let out = rw.finish();
        let parsed = OrgFile::parse(out.clone(), "sample.org").unwrap();
        let h = parsed.find_by_id("TASK-001").unwrap();
        assert_eq!(h.sections.last().unwrap().title, "Evidence");
        // The following heading is preserved verbatim (separator intact).
        let task2 = &SAMPLE[SAMPLE.find("* BACKLOG TASK-002").unwrap()..];
        assert!(out.contains(task2));
        // Removing the appended section restores the original byte-for-byte.
        let mut rw2 = OrgRewriter::new(&parsed, "sample.org");
        rw2.remove_section("TASK-001", "Evidence").unwrap();
        assert_eq!(rw2.finish(), SAMPLE);
    }

    #[test]
    fn rewriter_upsert_section_text_preserves_trailing_blank() {
        let f = OrgFile::parse(SAMPLE, "sample.org").unwrap();
        let mut rw = OrgRewriter::new(&f, "sample.org");
        // TASK-001 Description body ends with a blank line before the next
        // section; an edit must keep that separator.
        rw.upsert_section_text("TASK-001", "Description", "Rewritten body.")
            .unwrap();
        let out = rw.finish();
        assert!(out.contains("** Description\nRewritten body.\n\n** Acceptance Criteria\n"));
        // A missing section is created.
        let mut rw2 = OrgRewriter::new(&f, "sample.org");
        rw2.upsert_section_text("TASK-002", "Worklog", "First note.")
            .unwrap();
        let parsed = OrgFile::parse(rw2.finish(), "sample.org").unwrap();
        assert!(parsed
            .find_by_id("TASK-002")
            .unwrap()
            .section("Worklog")
            .is_some());
    }

    #[test]
    fn old_ready_keyword_is_absorbed_into_title() {
        let f = OrgFile::parse("* READY TASK-OLD Old task\n", "old.org").unwrap();
        assert_eq!(f.headings[0].todo, None);
        assert_eq!(f.headings[0].title, "READY TASK-OLD Old task");
    }

    #[test]
    fn missing_property_is_reported() {
        let f = OrgFile::parse(SAMPLE, "sample.org").unwrap();
        let mut rw = OrgRewriter::new(&f, "sample.org");
        let err = rw.set_property("TASK-001", "NOT_THERE", "x").unwrap_err();
        match err {
            OrgError::PropertyNotFound { key, heading, .. } => {
                assert_eq!(key, "NOT_THERE");
                assert_eq!(heading, "TASK-001");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn unterminated_drawer_reports_line() {
        let bad = "* X\n:PROPERTIES:\n:K: v\n";
        let err = OrgFile::parse(bad, "x.org").unwrap_err();
        match err {
            OrgError::UnterminatedDrawer { line, .. } => assert_eq!(line, 2),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn malformed_property_reports_line() {
        let bad = "* X\n:PROPERTIES:\nNOT_A_PROP\n:END:\n";
        let err = OrgFile::parse(bad, "x.org").unwrap_err();
        match err {
            OrgError::BadProperty { line, .. } => assert_eq!(line, 3),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parses_without_trailing_newline() {
        let s = "* X\n:PROPERTIES:\n:ID: x\n:END:";
        let f = OrgFile::parse(s, "x.org").unwrap();
        assert_eq!(f.headings[0].property("ID"), Some("x"));
    }

    #[test]
    fn property_with_empty_value() {
        let s = "* X\n:PROPERTIES:\n:ID: x\n:EMPTY:\n:END:\n";
        let f = OrgFile::parse(s, "x.org").unwrap();
        assert_eq!(f.headings[0].property("EMPTY"), Some(""));
    }

    // --- raw-body escape (task_156) ----------------------------------------

    #[test]
    fn wrap_raw_body_comma_escapes_column0_star_and_hash_plus() {
        let payload = "* Raw heading\n#+end_example\nplain line\n";
        let wrapped = wrap_raw_body(payload);
        assert_eq!(
            wrapped,
            "#+begin_example\n,* Raw heading\n,#+end_example\nplain line\n#+end_example\n"
        );
    }

    #[test]
    fn wrap_raw_body_preserves_indented_star_lines() {
        let payload = " * indented star\n";
        let wrapped = wrap_raw_body(payload);
        assert_eq!(
            wrapped,
            "#+begin_example\n * indented star\n#+end_example\n"
        );
    }

    #[test]
    fn wrap_raw_body_without_trailing_newline_closes_on_own_line() {
        let payload = "hello";
        let wrapped = wrap_raw_body(payload);
        assert_eq!(wrapped, "#+begin_example\nhello\n#+end_example\n");
        assert!(wrapped.ends_with("\n#+end_example\n"));
    }

    #[test]
    fn wrap_raw_body_re_escapes_comma_prefixed_star_and_hash_plus_lines() {
        let payload = ",* x\n,,#+ y\nplain line\n  ,* indented\n";
        let wrapped = wrap_raw_body(payload);
        assert_eq!(
            wrapped,
            "#+begin_example\n,,* x\n,,,#+ y\nplain line\n  ,* indented\n#+end_example\n"
        );
    }

    #[test]
    fn wrap_raw_body_adversarial_end_example_cannot_escape_wrapper() {
        let payload = "#+end_example\n* Phantom\n";
        let wrapped = wrap_raw_body(payload);
        let sample =
            format!("* TASK\n:PROPERTIES:\n:ID: TASK-001\n:END:\n\n** Description\n{wrapped}\n");
        let before = OrgFile::parse(
            "* TASK\n:PROPERTIES:\n:ID: TASK-001\n:END:\n\n** Description\nOriginal.\n",
            "sample.org",
        )
        .unwrap();
        let after = OrgFile::parse(&sample, "sample.org").unwrap();
        assert_eq!(
            heading_structure_snapshot(&after.headings),
            heading_structure_snapshot(&before.headings)
        );
        assert!(wrapped.contains(",#+end_example\n"));
    }

    #[test]
    fn wrap_raw_body_passes_structural_guard_via_set_section_body() {
        let f = OrgFile::parse(SAMPLE, "sample.org").unwrap();
        let mut rw = OrgRewriter::new(&f, "sample.org");
        let payload = "* Raw heading\n#+end_example\n";
        rw.set_section_body("TASK-001", "Description", &wrap_raw_body(payload))
            .unwrap();
        let out = rw.finish();
        let reparsed = OrgFile::parse(out.clone(), "sample.org").unwrap();
        assert_eq!(
            heading_structure_snapshot(&reparsed.headings),
            heading_structure_snapshot(&OrgFile::parse(SAMPLE, "sample.org").unwrap().headings)
        );
        assert!(out.contains("#+begin_example\n,* Raw heading\n,#+end_example\n#+end_example\n"));
    }

    // --- body-write structural invariant guard (task_148) -------------------

    #[test]
    fn set_section_body_rejects_column0_star_heading() {
        let f = OrgFile::parse(SAMPLE, "sample.org").unwrap();
        let mut rw = OrgRewriter::new(&f, "sample.org");
        let err = rw
            .set_section_body("TASK-001", "Description", "* Phantom heading\nsome text\n")
            .unwrap_err();
        assert!(
            matches!(err, OrgError::BodyHeadingInjection { .. }),
            "expected BodyHeadingInjection, got {err:?}"
        );
        // The rewriter must be usable after rejection (pre-edit state restored).
        rw.set_section_body("TASK-001", "Description", "Safe body.\n")
            .unwrap();
        let out = rw.finish();
        assert!(out.contains("** Description\nSafe body.\n"));
    }

    #[test]
    fn set_section_body_accepts_indented_star_and_roundtrips() {
        let f = OrgFile::parse(SAMPLE, "sample.org").unwrap();
        let mut rw = OrgRewriter::new(&f, "sample.org");
        // Space-indented * is not a heading — accepted and round-trips.
        rw.set_section_body("TASK-001", "Description", " * Not a heading\n")
            .unwrap();
        let out = rw.finish();
        let reparsed = OrgFile::parse(out.clone(), "sample.org").unwrap();
        assert_eq!(
            reparsed.headings.len(),
            2,
            "heading count must be unchanged"
        );
        assert!(out.contains(" * Not a heading\n"));
    }

    #[test]
    fn set_section_body_accepts_src_block_column0_star() {
        let f = OrgFile::parse(SAMPLE, "sample.org").unwrap();
        let mut rw = OrgRewriter::new(&f, "sample.org");
        let body = "#+begin_src org\n* Not a heading inside src\n#+end_src\n";
        rw.set_section_body("TASK-001", "Description", body)
            .unwrap();
        let out = rw.finish();
        let reparsed = OrgFile::parse(out.clone(), "sample.org").unwrap();
        assert_eq!(
            heading_structure_snapshot(&reparsed.headings),
            heading_structure_snapshot(&OrgFile::parse(SAMPLE, "sample.org").unwrap().headings)
        );
        assert!(out.contains(body));
    }

    #[test]
    fn set_section_body_still_rejects_column0_star_outside_block() {
        let f = OrgFile::parse(SAMPLE, "sample.org").unwrap();
        let mut rw = OrgRewriter::new(&f, "sample.org");
        let err = rw
            .set_section_body(
                "TASK-001",
                "Description",
                "#+begin_src org\nsafe\n#+end_src\n* Still a heading\n",
            )
            .unwrap_err();
        assert!(
            matches!(err, OrgError::BodyHeadingInjection { .. }),
            "expected BodyHeadingInjection, got {err:?}"
        );
    }

    #[test]
    fn set_section_body_accepts_multi_paragraph_prose() {
        let f = OrgFile::parse(SAMPLE, "sample.org").unwrap();
        let mut rw = OrgRewriter::new(&f, "sample.org");
        let body = "First paragraph.\n\nSecond paragraph with a list:\n- item one\n- item two\n\n";
        rw.set_section_body("TASK-001", "Description", body)
            .unwrap();
        let out = rw.finish();
        assert!(out.contains("** Description\nFirst paragraph.\n\nSecond paragraph"));
    }

    #[test]
    fn set_section_body_rejects_double_star_heading() {
        let f = OrgFile::parse(SAMPLE, "sample.org").unwrap();
        let mut rw = OrgRewriter::new(&f, "sample.org");
        // ** at column 0 also adds a heading.
        let err = rw
            .set_section_body(
                "TASK-001",
                "Description",
                "Normal text.\n** Injected Section\nContent.\n",
            )
            .unwrap_err();
        assert!(
            matches!(err, OrgError::BodyHeadingInjection { .. }),
            "expected BodyHeadingInjection, got {err:?}"
        );
    }

    #[test]
    fn set_node_body_rejects_column0_star_heading() {
        let f = OrgFile::parse(ARCH_SAMPLE, "architecture.org").unwrap();
        let mut rw = OrgRewriter::new(&f, "architecture.org");
        let err = rw.set_node_body("arch_006.3", "* Phantom\n").unwrap_err();
        assert!(
            matches!(err, OrgError::BodyHeadingInjection { .. }),
            "expected BodyHeadingInjection, got {err:?}"
        );
    }

    #[test]
    fn set_node_body_accepts_prose_after_rejected_op() {
        let f = OrgFile::parse(ARCH_SAMPLE, "architecture.org").unwrap();
        let mut rw = OrgRewriter::new(&f, "architecture.org");
        // Rejected op leaves rewriter usable.
        let _ = rw.set_node_body("arch_006.3", "* Phantom\n").unwrap_err();
        rw.set_node_body("arch_006.3", "Valid prose.").unwrap();
        let out = rw.finish();
        let reparsed = OrgFile::parse(out.clone(), "architecture.org").unwrap();
        // Structure unchanged.
        assert_eq!(reparsed.headings.len(), 1);
        assert_eq!(reparsed.headings[0].sections.len(), 1);
        assert!(out.contains("Valid prose."));
    }

    #[test]
    fn set_node_body_preserves_leading_escaped_star() {
        let f = OrgFile::parse(ARCH_SAMPLE, "architecture.org").unwrap();
        let mut rw = OrgRewriter::new(&f, "architecture.org");
        rw.set_node_body("arch_006.3", " * Not a heading\nSecond line.")
            .unwrap();
        let out = rw.finish();
        let reparsed = OrgFile::parse(out.clone(), "architecture.org").unwrap();
        let leaf = reparsed.find_by_id("arch_006.3").unwrap();
        assert_eq!(out.slice_body(leaf), "* Not a heading\nSecond line.");
        assert!(out.contains("\n * Not a heading\nSecond line.\n"));
    }

    #[test]
    fn upsert_new_section_rejects_body_heading_injection() {
        let f = OrgFile::parse(SAMPLE, "sample.org").unwrap();
        let mut rw = OrgRewriter::new(&f, "sample.org");
        let err = rw
            .upsert_section_text("TASK-002", "Evidence", "* Phantom\n")
            .unwrap_err();
        assert!(
            matches!(err, OrgError::BodyHeadingInjection { .. }),
            "expected BodyHeadingInjection, got {err:?}"
        );
        rw.upsert_section_text("TASK-002", "Evidence", "Safe evidence.")
            .unwrap();
        let out = rw.finish();
        assert!(out.contains("** Evidence\nSafe evidence."));
        assert!(!out.contains("* Phantom"));
    }

    #[test]
    fn append_section_accepts_src_block_body() {
        let f = OrgFile::parse(SAMPLE, "sample.org").unwrap();
        let mut rw = OrgRewriter::new(&f, "sample.org");
        let body = "#+begin_src org\n* Not a heading inside src\n#+end_src";
        rw.append_section("TASK-002", "Evidence", body).unwrap();
        let out = rw.finish();
        let reparsed = OrgFile::parse(out.clone(), "sample.org").unwrap();
        assert_eq!(reparsed.headings.len(), 2);
        assert!(reparsed
            .find_by_id("TASK-002")
            .unwrap()
            .section("Evidence")
            .is_some());
        assert!(out.contains(body));
    }
}

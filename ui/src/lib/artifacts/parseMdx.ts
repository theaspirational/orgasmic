// Recursive-descent parser for Agent-Native artifact.mdx content.
//
// The daemon's own `validate_mdx` (crates/orgasmic-daemon/src/artifacts.rs)
// is a byte-scan structural gate only: it finds a top-level block's body by
// the first literal matching `</Name>` and never looks inside it. That means
// same-name nesting is untracked and a body containing its own literal
// `</Name>` substring ends the scan early. This module is the real parser
// the daemon deliberately defers to us (TASK-T25XQ / the TASK-Y2ZQJ review
// note carried onto this task) — it must get both cases right:
//
//  - Nesting is handled by genuine recursion (parseNodeSequence calls itself
//    for a nested element with the same name), not a manual depth counter or
//    "first occurrence" search, so `<Section><Section>…</Section> tail
//    </Section>` closes at the correct outer tag.
//  - A body that needs to contain its own literal `</Name>`-shaped substring
//    (e.g. a Code sample that prints JSX) is authored via a `{`...`}`
//    template-literal ATTRIBUTE (`code={`...`}`), never as bare children —
//    that's the one place MDX/JSX itself is unambiguous, because the content
//    lives inside a backtick string the tag-boundary scanner treats as one
//    opaque token (see attrValue.ts / findMatchingBrace below), never as
//    characters the children scanner walks. Code/AnnotatedCode/Wireframe/
//    Diagram/Prototype/Mermaid/SequenceDiagram/FlowChart all read their raw
//    text body from a named prop first and fall back to children text only
//    when the prop is absent (safe for short, unambiguous content).
//
// No JSX/JS is ever evaluated: attribute expressions go through the
// constrained JSON5-lite grammar in attrValue.ts, and only the 22 registered
// block names (plus the Column/Tab/Screen structural wrappers, recognized
// solely inside their expected parent) are ever instantiated as components.
// A parse failure at any node — malformed tag, unterminated attribute,
// unknown block — becomes an inline `error` AST node; it never throws past
// this module, so one bad block cannot blank the rest of the document.

import { AttrParseError, parseAttrExpression } from './attrValue';
import type { AttrValue, MdxNode } from './types';

function isNameChar(ch: string): boolean {
  return /[A-Za-z0-9-]/.test(ch);
}

function describeError(err: unknown): string {
  return err instanceof Error ? err.message : String(err);
}

function readQuotedAt(text: string, pos: number, quote: string): { value: string; nextPos: number } {
  let i = pos + 1;
  let out = '';
  while (i < text.length) {
    const ch = text[i];
    if (ch === '\\' && i + 1 < text.length) {
      out += text[i + 1];
      i += 2;
      continue;
    }
    if (ch === quote) return { value: out, nextPos: i + 1 };
    out += ch;
    i += 1;
  }
  throw new AttrParseError(`unterminated string literal (missing closing ${quote})`);
}

/** Index right after the `{` at `openIdx`'s matching `}`, skipping over
 * quoted/backtick string contents (with backslash escaping) so a stray `{`,
 * `}`, `<`, or `>` inside a code/html body never confuses the boundary. */
function findMatchingBrace(text: string, openIdx: number): number {
  let depth = 0;
  let i = openIdx;
  let quote: string | null = null;
  while (i < text.length) {
    const ch = text[i];
    if (quote) {
      if (ch === '\\') {
        i += 2;
        continue;
      }
      if (ch === quote) quote = null;
      i += 1;
      continue;
    }
    if (ch === '"' || ch === "'" || ch === '`') {
      quote = ch;
      i += 1;
      continue;
    }
    if (ch === '{') {
      depth += 1;
      i += 1;
      continue;
    }
    if (ch === '}') {
      depth -= 1;
      i += 1;
      if (depth === 0) return i;
      continue;
    }
    i += 1;
  }
  throw new AttrParseError('unterminated `{...}` attribute expression (missing closing `}`)');
}

type OpenTag = {
  name: string;
  props: Record<string, AttrValue>;
  selfClosing: boolean;
  nextPos: number;
};

function parseOpenTag(text: string, pos: number): OpenTag {
  let i = pos + 1; // skip '<'
  const nameStart = i;
  while (i < text.length && isNameChar(text[i]!)) i += 1;
  if (i === nameStart) throw new AttrParseError('malformed tag: missing element name');
  const name = text.slice(nameStart, i);
  const props: Record<string, AttrValue> = {};

  for (;;) {
    while (i < text.length && /\s/.test(text[i]!)) i += 1;
    if (i >= text.length) throw new AttrParseError(`unterminated tag <${name}> (missing closing >)`);
    if (text[i] === '/' && text[i + 1] === '>') return { name, props, selfClosing: true, nextPos: i + 2 };
    if (text[i] === '>') return { name, props, selfClosing: false, nextPos: i + 1 };

    const attrStart = i;
    while (i < text.length && /[A-Za-z0-9_-]/.test(text[i]!)) i += 1;
    if (i === attrStart) {
      throw new AttrParseError(
        `malformed attribute near "${text.slice(i, i + 12)}" in <${name}>`,
      );
    }
    const attrName = text.slice(attrStart, i);
    while (i < text.length && /\s/.test(text[i]!)) i += 1;

    if (text[i] === '=') {
      i += 1;
      while (i < text.length && /\s/.test(text[i]!)) i += 1;
      const quote = text[i];
      if (quote === '"' || quote === "'") {
        const { value, nextPos } = readQuotedAt(text, i, quote);
        props[attrName] = value;
        i = nextPos;
      } else if (quote === '{') {
        const close = findMatchingBrace(text, i);
        const raw = text.slice(i + 1, close - 1);
        try {
          props[attrName] = parseAttrExpression(raw) as AttrValue;
        } catch (err) {
          throw new AttrParseError(`attribute \`${attrName}\` in <${name}>: ${describeError(err)}`);
        }
        i = close;
      } else {
        throw new AttrParseError(`expected a quote or { for attribute \`${attrName}\` in <${name}>`);
      }
    } else {
      props[attrName] = true; // boolean shorthand: `<Checklist collapsed>`
    }
  }
}

type ElementParse = { node: MdxNode; nextPos: number };

function parseElement(text: string, pos: number): ElementParse {
  const open = parseOpenTag(text, pos);
  if (open.selfClosing) {
    return { node: { kind: 'element', name: open.name, props: open.props, children: [] }, nextPos: open.nextPos };
  }
  const { children, nextPos } = parseNodeSequence(text, open.nextPos, open.name);
  return { node: { kind: 'element', name: open.name, props: open.props, children }, nextPos };
}

type SequenceResult = { children: MdxNode[]; nextPos: number; closed: boolean };

/**
 * Parse a run of text/elements starting at `pos`. When `closingTagName` is
 * non-null, stops at (and consumes) the matching `</closingTagName>`; a
 * mismatched closing tag is left UNCONSUMED so the enclosing call (looking
 * for its own, different, tag name) gets a chance to match it — this is what
 * makes a genuinely missing close tag resync at the right ancestor instead of
 * silently swallowing siblings. When `closingTagName` is null (document
 * root), any closing tag encountered is stray by definition and reported
 * without halting the scan.
 */
function parseNodeSequence(text: string, pos: number, closingTagName: string | null): SequenceResult {
  const children: MdxNode[] = [];
  let textStart = pos;
  let i = pos;

  function flushText(end: number): void {
    if (end > textStart) {
      const raw = text.slice(textStart, end);
      if (raw.trim().length > 0) children.push({ kind: 'text', markdown: raw });
    }
    textStart = end;
  }

  while (i < text.length) {
    if (text[i] !== '<') {
      i += 1;
      continue;
    }
    const nextCh = text[i + 1] ?? '';

    if (nextCh === '/') {
      let j = i + 2;
      const nameStart = j;
      while (j < text.length && isNameChar(text[j]!)) j += 1;
      // Mirror the opening-tag rule: only an uppercase name is a JSX
      // component boundary. A lowercase closing tag (`</span>`, `</div>`,
      // any bare HTML inside Wireframe/Diagram/Screen children) is passed
      // through as literal text — without this check every HTML closing tag
      // in a wireframe/diagram body misreports as a stray closing tag.
      if (j > nameStart && /[A-Z]/.test(text[nameStart]!)) {
        let k = j;
        while (k < text.length && /\s/.test(text[k]!)) k += 1;
        if (text[k] === '>') {
          const closeName = text.slice(nameStart, j);
          if (closingTagName !== null && closeName === closingTagName) {
            flushText(i);
            return { children, nextPos: k + 1, closed: true };
          }
          flushText(i);
          if (closingTagName !== null) {
            children.push({
              kind: 'error',
              message: `expected </${closingTagName}> but found </${closeName}>`,
            });
            return { children, nextPos: i, closed: false };
          }
          children.push({
            kind: 'error',
            message: `unexpected closing tag </${closeName}> with no matching open tag`,
          });
          i = k + 1;
          textStart = i;
          continue;
        }
      }
      i += 1;
      continue;
    }

    if (/[A-Z]/.test(nextCh)) {
      flushText(i);
      try {
        const { node, nextPos } = parseElement(text, i);
        children.push(node);
        i = nextPos;
      } catch (err) {
        children.push({ kind: 'error', message: describeError(err) });
        const found = text.indexOf('<', i + 1);
        i = found === -1 ? text.length : found;
      }
      textStart = i;
      continue;
    }

    // Lowercase tag (plain HTML, passed through untouched — mirrors the
    // daemon's own PascalCase-only rule) or a bare `<` in prose: not a JSX
    // component boundary, so it is literal text.
    i += 1;
  }

  flushText(text.length);
  if (closingTagName !== null) {
    children.push({ kind: 'error', message: `unclosed tag <${closingTagName}> (missing </${closingTagName}>)` });
  }
  return { children, nextPos: text.length, closed: false };
}

/** Parse an artifact.mdx string into a top-level node list. Never throws —
 * any structural failure surfaces as an inline `{kind:'error'}` node. */
export function parseArtifactMdx(source: string): MdxNode[] {
  if (!source) return [];
  return parseNodeSequence(source, 0, null).children;
}

/** Read a block's raw text body: prefer the named prop (the safe form for
 * content that may contain adversarial/lookalike substrings), falling back
 * to concatenated children markdown text for simply-authored blocks. */
export function textBody(node: Extract<MdxNode, { kind: 'element' }>, propName: string): string {
  const propValue = node.props[propName];
  if (typeof propValue === 'string') return propValue;
  return node.children
    .map((child) => (child.kind === 'text' ? child.markdown : ''))
    .join('')
    .trim();
}

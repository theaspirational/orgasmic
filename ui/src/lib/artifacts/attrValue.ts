// Constrained parser for `{...}` JSX attribute expressions. This is
// intentionally NOT a JS evaluator (no `eval`/`new Function`): it recognizes a
// JSON5-lite grammar (object/array/string/number/boolean/null literals, bare
// identifiers, unquoted object keys, trailing commas) plus one extra literal
// form MDX authors need for multi-line raw content — a backtick template
// string, taken as a literal string with no `${}` interpolation. That last
// form is how Code/Wireframe/Diagram/Prototype/Mermaid bodies carry arbitrary
// text (including a literal `</Code>`-like substring) safely inside an
// attribute rather than as JSX children, where it would be ambiguous with
// tag boundaries.

export class AttrParseError extends Error {}

function isIdentStart(ch: string): boolean {
  return /[A-Za-z_$]/.test(ch);
}
function isIdentPart(ch: string): boolean {
  return /[A-Za-z0-9_$]/.test(ch);
}

class Cursor {
  constructor(
    public text: string,
    public pos = 0,
  ) {}
  get done(): boolean {
    return this.pos >= this.text.length;
  }
  peek(offset = 0): string {
    return this.text[this.pos + offset] ?? '';
  }
  skipWs(): void {
    while (!this.done && /\s/.test(this.peek())) this.pos += 1;
  }
}

function readQuoted(cur: Cursor, quote: string): string {
  cur.pos += 1; // opening quote
  let out = '';
  while (!cur.done) {
    const ch = cur.peek();
    if (ch === '\\' && cur.pos + 1 < cur.text.length) {
      out += cur.peek(1);
      cur.pos += 2;
      continue;
    }
    if (ch === quote) {
      cur.pos += 1;
      return out;
    }
    out += ch;
    cur.pos += 1;
  }
  throw new AttrParseError(`unterminated string literal (missing closing ${quote})`);
}

function readTemplateLiteral(cur: Cursor): string {
  // Backtick strings are literal text, never interpolated — this is the
  // vehicle for raw code/html/mermaid bodies that may contain arbitrary
  // characters, including sequences that look like closing tags.
  cur.pos += 1; // opening backtick
  const ESCAPES: Record<string, string> = { n: '\n', t: '\t', r: '\r', '`': '`', '\\': '\\', $: '$' };
  let out = '';
  while (!cur.done) {
    const ch = cur.peek();
    if (ch === '\\' && cur.pos + 1 < cur.text.length) {
      const next = cur.peek(1);
      out += ESCAPES[next] ?? next; // unrecognized escape: JS semantics drop the backslash
      cur.pos += 2;
      continue;
    }
    if (ch === '`') {
      cur.pos += 1;
      return out;
    }
    out += ch;
    cur.pos += 1;
  }
  throw new AttrParseError('unterminated template literal (missing closing `)');
}

function readNumber(cur: Cursor): number {
  const start = cur.pos;
  if (cur.peek() === '-' || cur.peek() === '+') cur.pos += 1;
  while (!cur.done && /[0-9]/.test(cur.peek())) cur.pos += 1;
  if (cur.peek() === '.') {
    cur.pos += 1;
    while (!cur.done && /[0-9]/.test(cur.peek())) cur.pos += 1;
  }
  if (cur.peek() === 'e' || cur.peek() === 'E') {
    cur.pos += 1;
    if (cur.peek() === '-' || cur.peek() === '+') cur.pos += 1;
    while (!cur.done && /[0-9]/.test(cur.peek())) cur.pos += 1;
  }
  const raw = cur.text.slice(start, cur.pos);
  const value = Number(raw);
  if (Number.isNaN(value)) throw new AttrParseError(`invalid number literal \`${raw}\``);
  return value;
}

function readIdentifier(cur: Cursor): string {
  const start = cur.pos;
  while (!cur.done && isIdentPart(cur.peek())) cur.pos += 1;
  return cur.text.slice(start, cur.pos);
}

function parseValue(cur: Cursor): unknown {
  cur.skipWs();
  if (cur.done) throw new AttrParseError('unexpected end of expression');
  const ch = cur.peek();
  if (ch === '{') return parseObject(cur);
  if (ch === '[') return parseArray(cur);
  if (ch === '"' || ch === "'") return readQuoted(cur, ch);
  if (ch === '`') return readTemplateLiteral(cur);
  if (ch === '-' || ch === '+' || /[0-9]/.test(ch)) return readNumber(cur);
  if (isIdentStart(ch)) {
    const ident = readIdentifier(cur);
    if (ident === 'true') return true;
    if (ident === 'false') return false;
    if (ident === 'null' || ident === 'undefined') return null;
    // Bare word with no quotes — treat as a literal string rather than
    // failing; generators occasionally emit unquoted enum-like values.
    return ident;
  }
  throw new AttrParseError(`unexpected character \`${ch}\` in attribute expression`);
}

function parseArray(cur: Cursor): unknown[] {
  cur.pos += 1; // '['
  const out: unknown[] = [];
  cur.skipWs();
  while (!cur.done && cur.peek() !== ']') {
    out.push(parseValue(cur));
    cur.skipWs();
    if (cur.peek() === ',') {
      cur.pos += 1;
      cur.skipWs();
    } else break;
  }
  cur.skipWs();
  if (cur.peek() !== ']') throw new AttrParseError('unterminated array literal (missing `]`)');
  cur.pos += 1;
  return out;
}

function parseObjectKey(cur: Cursor): string {
  cur.skipWs();
  const ch = cur.peek();
  if (ch === '"' || ch === "'") return readQuoted(cur, ch);
  if (isIdentStart(ch)) return readIdentifier(cur);
  throw new AttrParseError(`invalid object key near \`${cur.text.slice(cur.pos, cur.pos + 12)}\``);
}

function parseObject(cur: Cursor): Record<string, unknown> {
  cur.pos += 1; // '{'
  const out: Record<string, unknown> = {};
  cur.skipWs();
  while (!cur.done && cur.peek() !== '}') {
    const key = parseObjectKey(cur);
    cur.skipWs();
    if (cur.peek() !== ':') throw new AttrParseError(`expected \`:\` after key \`${key}\``);
    cur.pos += 1;
    out[key] = parseValue(cur);
    cur.skipWs();
    if (cur.peek() === ',') {
      cur.pos += 1;
      cur.skipWs();
    } else break;
  }
  cur.skipWs();
  if (cur.peek() !== '}') throw new AttrParseError('unterminated object literal (missing `}`)');
  cur.pos += 1;
  return out;
}

/** Parse the raw text of a `{...}` JSX attribute expression (braces already
 * stripped) into a plain JS value. Throws {@link AttrParseError} on anything
 * that isn't valid JSON5-lite or a bare backtick template string. */
export function parseAttrExpression(raw: string): unknown {
  const cur = new Cursor(raw.trim());
  if (cur.done) return null;
  const value = parseValue(cur);
  cur.skipWs();
  if (!cur.done) {
    throw new AttrParseError(`unexpected trailing content \`${cur.text.slice(cur.pos, cur.pos + 20)}\``);
  }
  return value;
}

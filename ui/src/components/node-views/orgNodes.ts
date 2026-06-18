export type NodeKind = 'decision' | 'architecture' | 'glossary';

export type OrgProperty = {
  key: string;
  value: string;
  start: number;
  end: number;
};

export type OrgSourceNode = {
  level: number;
  title: string;
  heading: string;
  start: number;
  end: number;
  bodyStart: number;
  bodyEnd: number;
  body: string;
  properties: OrgProperty[];
};

export type OrgSourceDocument = {
  root: OrgSourceNode | null;
  sections: OrgSourceNode[];
};

type SourceLine = {
  text: string;
  start: number;
  end: number;
};

type HeadingLine = SourceLine & {
  level: number;
  title: string;
  lineIndex: number;
};

type PropertyDrawer = {
  endLineStart: number;
  end: number;
  properties: OrgProperty[];
};

export function inferNodeKind(id: string | null | undefined): NodeKind | null {
  if (!id) return null;
  if (id.startsWith('dec_')) return 'decision';
  if (id.startsWith('arch_')) return 'architecture';
  return 'glossary';
}

export function shortPath(path: string | null | undefined): string {
  if (!path) return '—';
  const normalized = path.replaceAll('\\', '/');
  return normalized.slice(normalized.lastIndexOf('/') + 1);
}

export function firstSentence(value: string | null | undefined): string {
  const text = value?.replace(/\s+/g, ' ').trim();
  if (!text) return '—';
  const match = /^(.+?[.!?])(?:\s|$)/.exec(text);
  return match?.[1] ?? text;
}

export function parseOrgSourceNodes(source: string): OrgSourceDocument {
  const lines = splitSourceLines(source);
  const headings = findHeadings(lines);
  const rootHeading = headings[0] ?? null;
  if (!rootHeading) return { root: null, sections: [] };

  const rootEnd = findNodeEnd(source, headings, 0);
  const root = toOrgNode(source, lines, headings, 0, rootEnd);
  const sections = headings
    .map((heading, index) => ({ heading, index }))
    .filter(({ heading }) => heading.level === rootHeading.level + 1)
    .filter(({ heading }) => heading.start > rootHeading.start && heading.start < rootEnd)
    .map(({ index }) => toOrgNode(source, lines, headings, index, findNodeEnd(source, headings, index)));

  return { root, sections };
}

export function updateOrgNodeProperty(
  source: string,
  targetTitle: string | null,
  key: string,
  value: string,
): string {
  const doc = parseOrgSourceNodes(source);
  const node = targetTitle === null ? doc.root : doc.sections.find((section) => section.title === targetTitle);
  if (!node) return source;

  const cleanValue = value.replace(/\s+/g, ' ').trim();
  const existing = node.properties.find((property) => property.key === key);
  if (existing) {
    const line = source.slice(existing.start, existing.end);
    const newline = line.endsWith('\n') ? '\n' : '';
    const withoutNewline = newline ? line.slice(0, -1) : line;
    const prefix = withoutNewline.match(/^(:[^:]+:\s*)/)?.[1] ?? `:${key}: `;
    return replaceRange(source, existing.start, existing.end, `${prefix}${cleanValue}${newline}`);
  }

  const drawer = findPropertyDrawer(splitSourceLines(source), node.start, node.heading.length);
  if (!drawer) {
    const drawerText = `:PROPERTIES:\n${formatPropertyLine(key, cleanValue)}:END:\n`;
    return replaceRange(source, node.start + node.heading.length, node.start + node.heading.length, drawerText);
  }
  return replaceRange(source, drawer.endLineStart, drawer.endLineStart, formatPropertyLine(key, cleanValue));
}

export function updateOrgNodeBody(source: string, sectionTitle: string, body: string): string {
  const node = parseOrgSourceNodes(source).sections.find((section) => section.title === sectionTitle);
  if (!node) return source;
  const normalized = body.trimEnd();
  return replaceRange(source, node.bodyStart, node.bodyEnd, normalized ? `${normalized}\n` : '');
}

export function updateOrgRootBody(source: string, body: string): string {
  const node = parseOrgSourceNodes(source).root;
  if (!node) return source;
  const normalized = body.trimEnd();
  return replaceRange(source, node.bodyStart, node.bodyEnd, normalized ? `${normalized}\n` : '');
}

function splitSourceLines(source: string): SourceLine[] {
  const lines = source.match(/[^\n]*\n|[^\n]+/g) ?? [];
  let offset = 0;
  return lines.map((text) => {
    const line = { text, start: offset, end: offset + text.length };
    offset += text.length;
    return line;
  });
}

function findHeadings(lines: SourceLine[]): HeadingLine[] {
  return lines.flatMap((line, lineIndex) => {
    const match = /^(\*+)\s+(.+?)\s*$/.exec(line.text.replace(/\n$/, ''));
    if (!match) return [];
    return [
      {
        ...line,
        level: match[1]!.length,
        title: match[2]!.trim(),
        lineIndex,
      },
    ];
  });
}

function findNodeEnd(source: string, headings: HeadingLine[], index: number): number {
  const current = headings[index]!;
  const next = headings.slice(index + 1).find((heading) => heading.level <= current.level);
  return next?.start ?? source.length;
}

function toOrgNode(
  source: string,
  lines: SourceLine[],
  headings: HeadingLine[],
  index: number,
  nodeEnd: number,
): OrgSourceNode {
  const heading = headings[index]!;
  const drawer = parsePropertyDrawer(lines, heading.lineIndex);
  const bodyStart = drawer?.end ?? heading.end;
  const body = source.slice(bodyStart, nodeEnd).replace(/\n+$/, '');
  return {
    level: heading.level,
    title: heading.title,
    heading: heading.text,
    start: heading.start,
    end: nodeEnd,
    bodyStart,
    bodyEnd: nodeEnd,
    body,
    properties: drawer?.properties ?? [],
  };
}

function parsePropertyDrawer(lines: SourceLine[], headingLineIndex: number): PropertyDrawer | null {
  const startLine = lines[headingLineIndex + 1];
  if (!startLine || startLine.text.trim() !== ':PROPERTIES:') return null;

  const properties: OrgProperty[] = [];
  for (let index = headingLineIndex + 2; index < lines.length; index += 1) {
    const line = lines[index]!;
    if (line.text.trim() === ':END:') {
      return { endLineStart: line.start, end: line.end, properties };
    }
    const match = /^:([^:\s]+):\s*(.*?)\s*$/.exec(line.text.replace(/\n$/, ''));
    if (match) {
      properties.push({
        key: match[1]!,
        value: match[2] ?? '',
        start: line.start,
        end: line.end,
      });
    }
  }
  return null;
}

function findPropertyDrawer(lines: SourceLine[], nodeStart: number, headingLength: number): PropertyDrawer | null {
  const headingLineIndex = lines.findIndex((line) => line.start === nodeStart && line.text.length === headingLength);
  if (headingLineIndex < 0) return null;
  return parsePropertyDrawer(lines, headingLineIndex);
}

function formatPropertyLine(key: string, value: string): string {
  return `:${key}:${' '.repeat(Math.max(1, 20 - key.length))}${value}\n`;
}

function replaceRange(source: string, start: number, end: number, replacement: string): string {
  return `${source.slice(0, start)}${replacement}${source.slice(end)}`;
}

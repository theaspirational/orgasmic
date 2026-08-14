import { Fragment, type ReactNode } from 'react';

import { decorateText, useRichText } from '@/lib/richText';
import { cn } from '@/lib/utils';

type RichCtx = ReturnType<typeof useRichText>;

type OrgInline =
  | { kind: 'text'; value: string }
  | { kind: 'bold'; value: string }
  | { kind: 'italic'; value: string }
  | { kind: 'code'; value: string }
  | { kind: 'link'; target: string; label?: string };

type OrgBlock =
  | { kind: 'paragraph'; inlines: OrgInline[] }
  | { kind: 'list'; items: OrgInline[][] }
  | { kind: 'quote'; blocks: OrgBlock[] };

function parseInlines(source: string): OrgInline[] {
  const out: OrgInline[] = [];
  const pushText = (value: string) => {
    if (!value) return;
    const previous = out.at(-1);
    if (previous?.kind === 'text') {
      previous.value += value;
    } else {
      out.push({ kind: 'text', value });
    }
  };
  const isBoundary = (value: string | undefined) =>
    value == null || /[\s()[\]{}'"“”‘’.,!?;:<>—–-]/.test(value);
  const delimitedAt = (index: number, marker: '*' | '/' | '=' | '~') => {
    if (!isBoundary(source[index - 1])) return null;
    const first = source[index + 1];
    if (!first || /\s/.test(first) || first === marker) return null;
    const end = source.indexOf(marker, index + 1);
    if (end < 0 || /\s/.test(source[end - 1]!) || !isBoundary(source[end + 1])) return null;
    return { value: source.slice(index + 1, end), end: end + 1 };
  };

  let index = 0;
  while (index < source.length) {
    const rest = source.slice(index);
    const labeled = /^\[\[([^\]]+)\]\[([^\]]+)\]\]/.exec(rest);
    if (labeled) {
      out.push({ kind: 'link', target: labeled[1]!, label: labeled[2]! });
      index += labeled[0].length;
      continue;
    }
    const plain = /^\[\[([^\]]+)\]\]/.exec(rest);
    if (plain) {
      out.push({ kind: 'link', target: plain[1]! });
      index += plain[0].length;
      continue;
    }

    const marker = source[index];
    if (marker === '*' || marker === '/' || marker === '=' || marker === '~') {
      const delimited = delimitedAt(index, marker);
      if (delimited) {
        out.push(
          marker === '*'
            ? { kind: 'bold', value: delimited.value }
            : marker === '/'
              ? { kind: 'italic', value: delimited.value }
              : { kind: 'code', value: delimited.value },
        );
        index = delimited.end;
        continue;
      }
    }

    pushText(source[index]!);
    index += 1;
  }
  return out;
}

function parseBlocks(source: string): OrgBlock[] {
  const trimmed = source.trim();
  if (!trimmed) return [];
  const blocks: OrgBlock[] = [];
  const lines = trimmed.split('\n');
  let paragraphLines: string[] = [];
  let listItems: string[] = [];

  const flushParagraph = () => {
    if (paragraphLines.length === 0) return;
    blocks.push({ kind: 'paragraph', inlines: parseInlines(paragraphLines.join(' ')) });
    paragraphLines = [];
  };
  const flushList = () => {
    if (listItems.length === 0) return;
    blocks.push({ kind: 'list', items: listItems.map(parseInlines) });
    listItems = [];
  };

  for (let index = 0; index < lines.length; index += 1) {
    const line = lines[index]!;
    const content = line.trim();
    if (!content) {
      flushParagraph();
      flushList();
      continue;
    }

    if (/^#\+begin_quote$/i.test(content)) {
      const end = lines.findIndex(
        (candidate, candidateIndex) =>
          candidateIndex > index && /^#\+end_quote$/i.test(candidate.trim()),
      );
      if (end >= 0) {
        flushParagraph();
        flushList();
        blocks.push({ kind: 'quote', blocks: parseBlocks(lines.slice(index + 1, end).join('\n')) });
        index = end;
        continue;
      }
    }

    const listItem = /^\s*-\s+(.+)$/.exec(line);
    if (listItem) {
      flushParagraph();
      listItems.push(listItem[1]!.trim());
      continue;
    }

    if (listItems.length > 0 && /^\s+/.test(line)) {
      listItems[listItems.length - 1] = `${listItems.at(-1)} ${content}`;
      continue;
    }

    flushList();
    paragraphLines.push(content);
  }
  flushParagraph();
  flushList();
  return blocks;
}

function renderInline(
  inline: OrgInline,
  key: number,
  ctx: RichCtx,
  interactive = true,
  compact = false,
): ReactNode {
  switch (inline.kind) {
    case 'text':
      return (
        <Fragment key={key}>
          {interactive ? decorateText(inline.value, ctx) : inline.value}
        </Fragment>
      );
    case 'bold':
      return <strong key={key}>{inline.value}</strong>;
    case 'italic':
      return <em key={key}>{inline.value}</em>;
    case 'code':
      return (
        <code
          key={key}
          className={cn(
            'font-mono text-muted-foreground',
            compact
              ? 'rounded-sm bg-muted/70 px-0.5 py-0 align-baseline'
              : 'rounded bg-muted px-1 py-0.5 text-[0.9em]',
          )}
          style={compact ? { fontSize: 'inherit', lineHeight: 'inherit' } : undefined}
        >
          {inline.value}
        </code>
      );
    case 'link': {
      const label = inline.label ?? inline.target;
      if (!interactive) {
        return (
          <span key={key} className="text-primary underline decoration-dotted underline-offset-2">
            {label}
          </span>
        );
      }
      if (/^https?:\/\//i.test(inline.target)) {
        return (
          <a key={key} href={inline.target} className="text-primary underline-offset-2 hover:underline">
            {label}
          </a>
        );
      }
      return (
        <code key={key} className="rounded bg-muted px-1 py-0.5 font-mono text-[0.9em] text-muted-foreground">
          {inline.target}
        </code>
      );
    }
  }
}

export function OrgInlineText({
  source,
  interactive = true,
  compact = false,
}: {
  source: string;
  interactive?: boolean;
  compact?: boolean;
}) {
  const ctx = useRichText();
  return (
    <Fragment>
      {parseInlines(source).map((inline, index) =>
        renderInline(inline, index, ctx, interactive, compact),
      )}
    </Fragment>
  );
}

function renderBlock(block: OrgBlock, key: number, ctx: RichCtx): ReactNode {
  switch (block.kind) {
    case 'paragraph':
      return (
        <p key={key} className="leading-relaxed">
          {block.inlines.map((inline, index) => renderInline(inline, index, ctx))}
        </p>
      );
    case 'list':
      return (
        <ul key={key} className="list-disc space-y-1 pl-5">
          {block.items.map((item, index) => (
            <li key={index}>{item.map((inline, inlineIndex) => renderInline(inline, inlineIndex, ctx))}</li>
          ))}
        </ul>
      );
    case 'quote':
      return (
        <blockquote key={key} className="border-l-2 border-border pl-3 text-muted-foreground">
          <div className="flex flex-col gap-2">
            {block.blocks.map((inner, index) => renderBlock(inner, index, ctx))}
          </div>
        </blockquote>
      );
  }
}

export function OrgBody({ source, className }: { source?: string | null; className?: string }) {
  const ctx = useRichText();
  if (!source?.trim()) return null;
  const blocks = parseBlocks(source);
  if (blocks.length === 0) return null;
  return (
    <div className={cn('flex flex-col gap-3 text-sm', className)}>
      {blocks.map((block, index) => renderBlock(block, index, ctx))}
    </div>
  );
}

export { parseBlocks, parseInlines };

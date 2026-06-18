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
  let rest = source;
  while (rest.length > 0) {
    const labeled = /^\[\[([^\]]+)\]\[([^\]]+)\]\]/.exec(rest);
    if (labeled) {
      out.push({ kind: 'link', target: labeled[1]!, label: labeled[2]! });
      rest = rest.slice(labeled[0].length);
      continue;
    }
    const plain = /^\[\[([^\]]+)\]\]/.exec(rest);
    if (plain) {
      out.push({ kind: 'link', target: plain[1]! });
      rest = rest.slice(plain[0].length);
      continue;
    }
    const tilde = /^~([^~\n]+)~/.exec(rest);
    if (tilde) {
      out.push({ kind: 'code', value: tilde[1]! });
      rest = rest.slice(tilde[0].length);
      continue;
    }
    const equals = /^=([^=\n]+)=/.exec(rest);
    if (equals) {
      out.push({ kind: 'code', value: equals[1]! });
      rest = rest.slice(equals[0].length);
      continue;
    }
    const bold = /^\*([^*\n]+)\*/.exec(rest);
    if (bold) {
      out.push({ kind: 'bold', value: bold[1]! });
      rest = rest.slice(bold[0].length);
      continue;
    }
    const italic = /^\/([^/\n]+)\//.exec(rest);
    if (italic) {
      out.push({ kind: 'italic', value: italic[1]! });
      rest = rest.slice(italic[0].length);
      continue;
    }
    const next = rest.search(/(\[\[|\*[^*\n]+\*|\/[^/\n]+\/|=([^=\n]+)=|~[^~\n]+~)/);
    if (next <= 0) {
      out.push({ kind: 'text', value: rest });
      break;
    }
    out.push({ kind: 'text', value: rest.slice(0, next) });
    rest = rest.slice(next);
  }
  return out;
}

function parseBlocks(source: string): OrgBlock[] {
  const trimmed = source.trim();
  if (!trimmed) return [];
  const blocks: OrgBlock[] = [];
  const parts = trimmed.split(/\n\s*\n/);
  for (const part of parts) {
    const block = part.trim();
    if (!block) continue;
    const quote = /^#\+begin_quote\n([\s\S]*?)\n#\+end_quote$/i.exec(block);
    if (quote) {
      blocks.push({ kind: 'quote', blocks: parseBlocks(quote[1]!) });
      continue;
    }
    const lines = block.split('\n').map((line) => line.trim()).filter(Boolean);
    if (lines.length > 0 && lines.every((line) => line.startsWith('- '))) {
      blocks.push({
        kind: 'list',
        items: lines.map((line) => parseInlines(line.slice(2))),
      });
      continue;
    }
    blocks.push({ kind: 'paragraph', inlines: parseInlines(block.replace(/\n/g, ' ')) });
  }
  return blocks;
}

function renderInline(inline: OrgInline, key: number, ctx: RichCtx): ReactNode {
  switch (inline.kind) {
    case 'text':
      return <Fragment key={key}>{decorateText(inline.value, ctx)}</Fragment>;
    case 'bold':
      return <strong key={key}>{inline.value}</strong>;
    case 'italic':
      return <em key={key}>{inline.value}</em>;
    case 'code':
      return (
        <code key={key} className="rounded bg-muted px-1 py-0.5 font-mono text-[0.9em] text-muted-foreground">
          {inline.value}
        </code>
      );
    case 'link': {
      const label = inline.label ?? inline.target;
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

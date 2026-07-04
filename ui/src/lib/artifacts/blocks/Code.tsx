import { useState } from 'react';
import { Check, ChevronDown, ChevronUp, Copy } from 'lucide-react';

import { Button } from '@/components/ui/button';
import { cn } from '@/lib/utils';
import type { MdxNode } from '../types';
import { textBody } from '../parseMdx';
import { asNumber, asOptionalString, asString } from './propUtils';
import { useShikiHtml } from './useShiki';

export function CodeBody({
  code,
  language,
  filename,
  caption,
  maxLines,
}: {
  code: string;
  language: string;
  filename?: string;
  caption?: string;
  maxLines?: number;
}) {
  const { html, loading } = useShikiHtml(code, language);
  const [copied, setCopied] = useState(false);
  const lineCount = code.split('\n').length;
  const collapsible = typeof maxLines === 'number' && maxLines > 0 && lineCount > maxLines;
  const [expanded, setExpanded] = useState(!collapsible);

  function copy() {
    void navigator.clipboard?.writeText(code).then(() => {
      setCopied(true);
      window.setTimeout(() => setCopied(false), 1200);
    });
  }

  return (
    <div className="overflow-hidden rounded-lg border">
      {filename || language ? (
        <div className="flex items-center justify-between gap-2 border-b bg-muted/40 px-3 py-1.5">
          <span className="truncate font-mono text-xs text-muted-foreground">{filename || language}</span>
          <Button type="button" variant="ghost" size="icon-sm" onClick={copy} aria-label="Copy code">
            {copied ? <Check className="size-3.5" /> : <Copy className="size-3.5" />}
          </Button>
        </div>
      ) : null}
      <div
        className={cn('overflow-x-auto text-xs [&_pre]:m-0 [&_pre]:p-3', !expanded && 'max-h-64 overflow-y-hidden')}
      >
        {html ? (
          <div dangerouslySetInnerHTML={{ __html: html }} />
        ) : (
          <pre className="m-0 p-3 font-mono">
            <code>{loading ? code : code}</code>
          </pre>
        )}
      </div>
      {collapsible ? (
        <button
          type="button"
          onClick={() => setExpanded((v) => !v)}
          className="flex w-full items-center justify-center gap-1 border-t bg-muted/20 py-1 text-xs text-muted-foreground hover:bg-muted/40"
        >
          {expanded ? <ChevronUp className="size-3" /> : <ChevronDown className="size-3" />}
          {expanded ? `Collapse to ${maxLines} lines` : `Show all ${lineCount} lines`}
        </button>
      ) : null}
      {caption ? <p className="border-t bg-muted/20 px-3 py-1.5 text-xs text-muted-foreground">{caption}</p> : null}
    </div>
  );
}

export function Code({ node }: { node: Extract<MdxNode, { kind: 'element' }> }) {
  const code = textBody(node, 'code');
  if (!code) return null;
  return (
    <CodeBody
      code={code}
      language={asString(node.props.language, 'text')}
      filename={asOptionalString(node.props.filename)}
      caption={asOptionalString(node.props.caption)}
      maxLines={typeof node.props.maxLines === 'number' ? asNumber(node.props.maxLines, 0) : undefined}
    />
  );
}

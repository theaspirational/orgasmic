import { useEffect, useRef } from 'react';

import { cn } from '@/lib/utils';
import type { AttrValue, MdxNode } from '../types';
import { textBody } from '../parseMdx';
import { asArray, asOptionalString, asRecord, asString } from './propUtils';
import { useShikiHtml } from './useShiki';

type Annotation = { lines: string; label?: string; note: string };

function readAnnotations(raw: AttrValue | undefined): Annotation[] {
  return asArray(raw)
    .map((entry) => {
      const record = asRecord(entry);
      return {
        lines: asString(record.lines),
        label: asOptionalString(record.label),
        note: asString(record.note),
      };
    })
    .filter((a) => a.lines);
}

function lineRange(spec: string): [number, number] {
  const [startRaw, endRaw] = spec.split('-');
  const start = Number.parseInt(startRaw ?? '', 10);
  const end = endRaw ? Number.parseInt(endRaw, 10) : start;
  return [Number.isFinite(start) ? start : 1, Number.isFinite(end) ? end : start];
}

export function AnnotatedCode({ node }: { node: Extract<MdxNode, { kind: 'element' }> }) {
  const code = textBody(node, 'code');
  const language = asString(node.props.language, 'text');
  const filename = asOptionalString(node.props.filename);
  const annotations = readAnnotations(node.props.annotations);
  const { html } = useShikiHtml(code, language);
  const containerRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    const container = containerRef.current;
    if (!container || !html) return;
    const lineEls = Array.from(container.querySelectorAll<HTMLElement>('.line'));
    lineEls.forEach((el) => el.classList.remove('annotated-code-line'));
    for (const annotation of annotations) {
      const [start, end] = lineRange(annotation.lines);
      for (let n = start; n <= end; n += 1) {
        lineEls[n - 1]?.classList.add('annotated-code-line');
      }
    }
  }, [html, annotations]);

  if (!code) return null;

  return (
    <div className="overflow-hidden rounded-lg border">
      {filename ? (
        <div className="border-b bg-muted/40 px-3 py-1.5 font-mono text-xs text-muted-foreground">{filename}</div>
      ) : null}
      <div
        ref={containerRef}
        className={cn(
          'overflow-x-auto text-xs [&_.annotated-code-line]:-mx-3 [&_.annotated-code-line]:bg-primary/10 [&_.annotated-code-line]:px-3',
          '[&_pre]:m-0 [&_pre]:p-3',
        )}
      >
        {html ? <div dangerouslySetInnerHTML={{ __html: html }} /> : <pre className="m-0 p-3 font-mono">{code}</pre>}
      </div>
      {annotations.length > 0 ? (
        <ul className="flex flex-col gap-1.5 border-t bg-muted/20 px-3 py-2 text-xs">
          {annotations.map((annotation, index) => (
            <li key={index} className="flex gap-2">
              <span className="shrink-0 rounded bg-primary/15 px-1.5 py-0.5 font-mono text-[0.7rem] text-primary">
                {annotation.lines}
              </span>
              <span className="text-muted-foreground">
                {annotation.label ? <strong className="text-foreground">{annotation.label}: </strong> : null}
                {annotation.note}
              </span>
            </li>
          ))}
        </ul>
      ) : null}
    </div>
  );
}

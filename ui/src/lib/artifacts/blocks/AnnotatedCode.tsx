import { useLayoutEffect, useMemo, useRef, useState, type RefObject } from 'react';

import { cn } from '@/lib/utils';
import type { AttrValue, MdxNode } from '../types';
import { textBody } from '../parseMdx';
import { asArray, asOptionalString, asRecord, asString } from './propUtils';
import { useShikiHtml } from './useShiki';

type Annotation = { lines: string; label?: string; note: string };
type Bubble = Annotation & { start: number; top: number };

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

/** Bakes the `annotated-code-line` marker into shiki's own output string
 * (its Nth `<span class="line">` per 1-indexed line, in source order)
 * instead of mutating the DOM after the fact. React resets a
 * `dangerouslySetInnerHTML` node's children on any re-render of that fiber
 * even when the `__html` string is unchanged (verified empirically — a
 * classList mutation applied in a layout effect that also calls setState,
 * as the bubble-measuring effect below does, gets silently wiped the moment
 * that setState triggers the next commit). Marking the string itself has no
 * such lifetime problem: it's set again every render, correctly, by
 * definition. */
function markAnnotatedLines(html: string, annotations: Annotation[]): string {
  if (!html || annotations.length === 0) return html;
  const marked = new Set<number>();
  for (const annotation of annotations) {
    const [start, end] = lineRange(annotation.lines);
    for (let n = start; n <= end; n += 1) marked.add(n);
  }
  if (marked.size === 0) return html;
  let lineIndex = 0;
  return html.replace(/<span class="line"/g, (match) => {
    lineIndex += 1;
    return marked.has(lineIndex) ? '<span class="line annotated-code-line"' : match;
  });
}

/** Anchors each annotation's margin bubble to the vertical position of the
 * code line its range starts at, measured against the scroll container so
 * the bubble tracks the exact line rather than an index-based estimate (line
 * heights can vary slightly with shiki's per-token markup). Read-only: it
 * never mutates the highlighted markup (see markAnnotatedLines above for
 * why), only measures it. Recomputes on a `ResizeObserver` tick too, since a
 * narrow viewport can wrap/reflow the code after first mount. */
function useAnnotationBubbles(
  containerRef: RefObject<HTMLDivElement | null>,
  html: string | null,
  annotations: Annotation[],
): Bubble[] {
  const [bubbles, setBubbles] = useState<Bubble[]>([]);

  useLayoutEffect(() => {
    const container = containerRef.current;
    if (!container || !html || annotations.length === 0) {
      setBubbles([]);
      return undefined;
    }

    const measure = () => {
      const lineEls = Array.from(container.querySelectorAll<HTMLElement>('.line'));
      const containerTop = container.getBoundingClientRect().top;
      const next = annotations.map((annotation) => {
        const [start] = lineRange(annotation.lines);
        const anchor = lineEls[start - 1];
        const top = anchor ? anchor.getBoundingClientRect().top - containerTop + container.scrollTop : 0;
        return { ...annotation, start, top };
      });
      setBubbles(next);
    };

    measure();
    if (typeof ResizeObserver === 'undefined') return undefined;
    const observer = new ResizeObserver(measure);
    observer.observe(container);
    return () => observer.disconnect();
  }, [containerRef, html, annotations]);

  return bubbles;
}

export function AnnotatedCode({ node }: { node: Extract<MdxNode, { kind: 'element' }> }) {
  const code = textBody(node, 'code');
  const language = asString(node.props.language, 'text');
  const filename = asOptionalString(node.props.filename);
  const annotations = useMemo(() => readAnnotations(node.props.annotations), [node.props.annotations]);
  const { html } = useShikiHtml(code, language);
  const markedHtml = useMemo(() => (html ? markAnnotatedLines(html, annotations) : html), [html, annotations]);
  const containerRef = useRef<HTMLDivElement>(null);
  const bubbles = useAnnotationBubbles(containerRef, html, annotations);

  if (!code) return null;

  return (
    <div className="overflow-hidden rounded-lg border">
      {filename ? (
        <div className="border-b bg-muted/40 px-3 py-1.5 font-mono text-xs text-muted-foreground">{filename}</div>
      ) : null}
      <div className="flex">
        <div
          ref={containerRef}
          className={cn(
            'min-w-0 flex-1 overflow-x-auto text-xs [&_.annotated-code-line]:-mx-3 [&_.annotated-code-line]:border-l-2 [&_.annotated-code-line]:border-l-primary [&_.annotated-code-line]:bg-primary/5 [&_.annotated-code-line]:px-3',
            '[&_pre]:m-0 [&_pre]:p-3',
          )}
        >
          {markedHtml ? (
            <div dangerouslySetInnerHTML={{ __html: markedHtml }} />
          ) : (
            <pre className="m-0 p-3 font-mono">{code}</pre>
          )}
        </div>
        {bubbles.length > 0 ? (
          <div className="relative hidden w-48 shrink-0 border-l bg-muted/10 sm:block" aria-hidden="true">
            {bubbles.map((bubble, index) => (
              <div
                key={index}
                data-annotation-line={bubble.lines}
                className="absolute inset-x-1.5 rounded-md border bg-card px-2 py-1 text-[0.68rem] leading-snug text-muted-foreground"
                style={{ top: bubble.top }}
              >
                {bubble.label ? <strong className="block text-foreground">{bubble.label}</strong> : null}
                {bubble.note}
              </div>
            ))}
          </div>
        ) : null}
      </div>
      {bubbles.length > 0 ? (
        <ul className="flex flex-col gap-1.5 border-t bg-muted/20 px-3 py-2 text-xs sm:hidden">
          {bubbles.map((bubble, index) => (
            <li key={index} data-annotation-line={bubble.lines} className="flex gap-2">
              <span className="shrink-0 rounded bg-primary/15 px-1.5 py-0.5 font-mono text-[0.7rem] text-primary">
                {bubble.lines}
              </span>
              <span className="text-muted-foreground">
                {bubble.label ? <strong className="text-foreground">{bubble.label}: </strong> : null}
                {bubble.note}
              </span>
            </li>
          ))}
        </ul>
      ) : null}
    </div>
  );
}

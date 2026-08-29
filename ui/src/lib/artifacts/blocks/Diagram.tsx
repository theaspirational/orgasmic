import { useMemo } from 'react';

import type { MdxNode } from '../types';
import { textBody } from '../parseMdx';
import { sanitizeHtmlFragment } from '../sanitize';
import { asNumber, asOptionalString, asString } from './propUtils';

const DIAGRAM_TOKENS = [
  '--wf-ink',
  '--wf-muted',
  '--wf-line',
  '--wf-paper',
  '--wf-card',
  '--wf-accent',
  '--wf-accent-fg',
  '--wf-accent-soft',
  '--wf-warn',
  '--wf-ok',
  '--wf-radius',
  '--sans',
] as const;

/** Diagram HTML/CSS runs in a sandboxed, script-disabled iframe (this task's
 * brief: "author HTML/CSS in a sandboxed frame") rather than inline, because
 * unlike Wireframe it carries author-supplied CSS with no selector scoping —
 * an iframe boundary is what stops that CSS from ever reaching the app shell.
 * The app's own token values are forwarded in as literal `--wf-*`/`--sans`
 * custom properties (computed once per render) so `.diagram-*` helper
 * classes still theme correctly even though the iframe shares no cascade
 * with the parent document. */
function buildDiagramSrcDoc(sanitizedHtml: string, authorCss: string): string {
  const root = typeof document !== 'undefined' ? document.documentElement : null;
  const tokenCss = DIAGRAM_TOKENS.map((name) => {
    const value = root ? getComputedStyle(root).getPropertyValue(name).trim() : '';
    return value ? `${name}: ${value};` : '';
  }).join('\n');

  return `<!doctype html><html><head><meta charset="utf-8" />
<style>
:root { ${tokenCss} }
html, body { margin: 0; padding: 12px; height: 100%; background: var(--wf-paper); color: var(--wf-ink); font-family: var(--sans); font-size: 13px; box-sizing: border-box; }
* { box-sizing: border-box; }
/* A root-level SVG with only a viewBox defaults to width:100% of the frame,
 * so on wide screens its aspect-ratio height blows past the iframe's fixed
 * height and clips. Scale it to fit both axes instead. */
body > svg { display: block; margin: 0 auto; width: auto; height: auto; max-width: 100%; max-height: 100%; }
.diagram-panel, .diagram-card { background: var(--wf-card); border: 1.2px solid var(--wf-line); border-radius: var(--wf-radius); padding: 12px; }
.diagram-node, .diagram-box { background: var(--wf-card); border: 1.2px solid var(--wf-line); border-radius: calc(var(--wf-radius) * 0.7); padding: 8px 12px; display: inline-block; }
.diagram-pill { display: inline-flex; align-items: center; border: 1px solid var(--wf-line); border-radius: 999px; padding: 2px 9px; font-size: 0.82em; background: var(--wf-card); }
.diagram-muted { color: var(--wf-muted); }
</style>
<style>${authorCss}</style>
</head><body>${sanitizedHtml}</body></html>`;
}

/** Diagram-design output (the diagram-design skill vendored at
 * vendor/diagram-design) arrives as an SVG/HTML fragment carrying its own
 * `<style>` blocks. The sanitizer forbids `<style>` tags, so lift them out
 * first and feed their content to the iframe alongside the `css` attribute —
 * same trust level either way, since both only ever style a sandboxed,
 * script-disabled frame. */
const STYLE_BLOCK = /<style[^>]*>([\s\S]*?)<\/style>/gi;

export function Diagram({ node }: { node: Extract<MdxNode, { kind: 'element' }> }) {
  const rawHtml = textBody(node, 'html');
  const css = asString(node.props.css);
  const caption = asOptionalString(node.props.caption);
  const height = asNumber(node.props.height, 256);
  const { fragment, extractedCss } = useMemo(() => {
    let lifted = '';
    const fragment = rawHtml.replace(STYLE_BLOCK, (_match, body: string) => {
      lifted += `\n${body}`;
      return '';
    });
    return { fragment, extractedCss: lifted };
  }, [rawHtml]);
  const sanitized = useMemo(() => sanitizeHtmlFragment(fragment), [fragment]);
  const srcDoc = useMemo(
    () => buildDiagramSrcDoc(sanitized, `${css}${extractedCss}`),
    [sanitized, css, extractedCss],
  );

  if (!rawHtml.trim()) return null;

  return (
    <figure className="flex flex-col gap-1.5">
      <iframe
        title={caption ?? 'Diagram'}
        srcDoc={srcDoc}
        sandbox=""
        style={{ height }}
        className="w-full resize-y overflow-auto rounded-lg border bg-card"
      />
      {caption ? <figcaption className="text-center text-xs text-muted-foreground">{caption}</figcaption> : null}
    </figure>
  );
}

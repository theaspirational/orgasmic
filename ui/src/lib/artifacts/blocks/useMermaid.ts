import { useEffect, useState } from 'react';

let renderCounter = 0;

/** Render Mermaid source to an SVG string. `securityLevel: 'strict'` disables
 * HTML labels/click bindings so untrusted diagram text (subject nodes, prior
 * artifact content) cannot inject markup through a node label. Re-renders
 * when `dark` flips so diagrams re-theme with the app instead of keeping
 * whichever palette was baked in at first render. */
export function useMermaidSvg(source: string, dark: boolean): { svg: string | null; error: string | null } {
  const [svg, setSvg] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    setSvg(null);
    setError(null);
    const trimmed = source.trim();
    if (!trimmed) return undefined;

    void (async () => {
      try {
        const { default: mermaid } = await import('mermaid');
        mermaid.initialize({
          startOnLoad: false,
          securityLevel: 'strict',
          theme: dark ? 'dark' : 'default',
          fontFamily: 'var(--sans)',
        });
        renderCounter += 1;
        const id = `orgasmic-mermaid-${renderCounter}`;
        const { svg: rendered } = await mermaid.render(id, trimmed);
        if (!cancelled) setSvg(rendered);
      } catch (err) {
        if (!cancelled) setError(err instanceof Error ? err.message : String(err));
      }
    })();

    return () => {
      cancelled = true;
    };
  }, [source, dark]);

  return { svg, error };
}

import { useTheme } from '@/lib/theme';
import type { MdxNode } from '../types';
import { textBody } from '../parseMdx';
import { UnrenderableBlock } from './shared';
import { useMermaidSvg } from './useMermaid';

export function MermaidBody({ source, caption }: { source: string; caption?: string }) {
  const { resolved } = useTheme();
  const { svg, error } = useMermaidSvg(source, resolved === 'black-paper');

  if (!source.trim()) return null;
  if (error) return <UnrenderableBlock name="Mermaid diagram" message={error} />;

  return (
    <div className="flex flex-col gap-1.5 rounded-lg border bg-card p-3">
      {svg ? (
        <div className="mermaid-diagram overflow-x-auto [&_svg]:mx-auto" dangerouslySetInnerHTML={{ __html: svg }} />
      ) : (
        <div className="flex h-24 items-center justify-center text-xs text-muted-foreground">Rendering diagram…</div>
      )}
      {caption ? <p className="text-center text-xs text-muted-foreground">{caption}</p> : null}
    </div>
  );
}

export function Mermaid({ node }: { node: Extract<MdxNode, { kind: 'element' }> }) {
  const source = textBody(node, 'source');
  return <MermaidBody source={source} />;
}

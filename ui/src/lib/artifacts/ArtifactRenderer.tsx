import { useMemo } from 'react';

import { parseArtifactMdx } from './parseMdx';
import { renderNodes } from './blocks';

/** Parse + render an artifact.mdx string. The one entry point ArtifactView
 * (and any future embed) should use — it owns the untrusted-content path end
 * to end (constrained parse, sanitized raw-HTML blocks, no MDX/JS eval). */
export function ArtifactRenderer({ content }: { content: string }) {
  const nodes = useMemo(() => parseArtifactMdx(content), [content]);
  if (nodes.length === 0) {
    return <p className="text-sm text-muted-foreground">This artifact has no renderable content yet.</p>;
  }
  return <div className="orgasmic-artifact flex flex-col gap-4">{renderNodes(nodes)}</div>;
}

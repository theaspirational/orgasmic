import type { MdxNode } from '../types';
import { textBody } from '../parseMdx';
import { MermaidBody } from './Mermaid';

/** Sugar over Mermaid with a fixed graph kind: the body is the sequence
 * content only (participants/messages), the `sequenceDiagram` header is
 * implied and prepended unless already present. */
export function SequenceDiagram({ node }: { node: Extract<MdxNode, { kind: 'element' }> }) {
  const body = textBody(node, 'source');
  const source = /^\s*sequenceDiagram\b/.test(body) ? body : `sequenceDiagram\n${body}`;
  return <MermaidBody source={source} />;
}

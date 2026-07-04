import type { MdxNode } from '../types';
import { textBody } from '../parseMdx';
import { asString } from './propUtils';
import { MermaidBody } from './Mermaid';

/** Sugar over Mermaid with a fixed graph kind: the body is the node/edge
 * content only, the `graph <direction>` header is implied and prepended
 * unless already present. */
export function FlowChart({ node }: { node: Extract<MdxNode, { kind: 'element' }> }) {
  const body = textBody(node, 'source');
  const direction = asString(node.props.direction, 'TD');
  const source = /^\s*(graph|flowchart)\b/.test(body) ? body : `graph ${direction}\n${body}`;
  return <MermaidBody source={source} />;
}

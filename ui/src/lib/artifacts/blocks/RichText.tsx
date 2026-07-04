import type { MdxNode } from '../types';
import { textBody } from '../parseMdx';
import { Markdown } from './Markdown';

export function RichText({ node }: { node: Extract<MdxNode, { kind: 'element' }> }) {
  const body = textBody(node, 'content') || textBody(node, 'body');
  return <Markdown text={body} />;
}

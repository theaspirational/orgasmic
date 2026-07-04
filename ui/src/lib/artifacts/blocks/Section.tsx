import type { MdxNode } from '../types';
import { asOptionalString } from './propUtils';
import { renderNodes } from './index';

/** A titled grouping container — the one container block with no structural
 * wrapper tag of its own; its children are rendered through the same
 * top-level dispatch (any of the 22 blocks, or prose, may nest here). */
export function Section({ node }: { node: Extract<MdxNode, { kind: 'element' }> }) {
  const title = asOptionalString(node.props.title);
  return (
    <section className="flex flex-col gap-3 rounded-lg border bg-muted/10 p-4">
      {title ? <h3 className="text-sm font-semibold">{title}</h3> : null}
      {renderNodes(node.children, 'section')}
    </section>
  );
}

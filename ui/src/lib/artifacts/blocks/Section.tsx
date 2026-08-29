import type { MdxNode } from '../types';
import { asOptionalString } from './propUtils';
import { renderNodes } from './index';

/** A titled grouping container — the one container block with no structural
 * wrapper tag of its own; its children are rendered through the same
 * top-level dispatch (any of the 19 blocks, or prose, may nest here). */
export function Section({ node }: { node: Extract<MdxNode, { kind: 'element' }> }) {
  const title = asOptionalString(node.props.title);
  return (
    <section className="flex flex-col gap-4 border-t border-border/60 pt-6 [&:first-child]:border-t-0 [&:first-child]:pt-0">
      {title ? (
        <h3 className="text-lg font-semibold tracking-tight text-foreground">{title}</h3>
      ) : null}
      {renderNodes(node.children, 'section')}
    </section>
  );
}

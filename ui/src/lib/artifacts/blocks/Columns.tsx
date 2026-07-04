import { cn } from '@/lib/utils';
import type { MdxNode } from '../types';
import { asOptionalString } from './propUtils';
import { UnrenderableBlock } from './shared';
import { renderNodes } from './index';

/**
 * A multi-column side-by-side layout — good for before/after or current/
 * target comparisons. Each column is a nested `<Column label="...">` wrapper
 * (recognized only here, never at the document top level); its own children
 * go back through the full block dispatch.
 */
export function Columns({ node }: { node: Extract<MdxNode, { kind: 'element' }> }) {
  const columns = node.children.filter(
    (child): child is Extract<MdxNode, { kind: 'element' }> => child.kind === 'element' && child.name === 'Column',
  );
  if (columns.length === 0) {
    return <UnrenderableBlock name="Columns" message="no <Column> children found" />;
  }
  return (
    <div
      className={cn('grid gap-4')}
      style={{ gridTemplateColumns: `repeat(${Math.min(columns.length, 4)}, minmax(0, 1fr))` }}
    >
      {columns.map((column, index) => {
        const label = asOptionalString(column.props.label);
        return (
          <div key={index} className="flex min-w-0 flex-col gap-2">
            {label ? <h4 className="text-xs font-medium text-muted-foreground">{label}</h4> : null}
            {renderNodes(column.children, `col-${index}`)}
          </div>
        );
      })}
    </div>
  );
}

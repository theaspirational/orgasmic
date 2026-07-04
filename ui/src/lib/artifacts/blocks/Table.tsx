import type { AttrValue, MdxNode } from '../types';
import { asArray, asString } from './propUtils';

function readRow(row: AttrValue): string[] {
  return asArray(row).map((cell) => asString(cell));
}

export function Table({ node }: { node: Extract<MdxNode, { kind: 'element' }> }) {
  const headers = asArray(node.props.headers).map((cell) => asString(cell));
  const rows = asArray(node.props.rows).map(readRow);
  const caption = asString(node.props.caption) || undefined;
  if (headers.length === 0 && rows.length === 0) return null;

  return (
    <div className="overflow-hidden rounded-lg border">
      <div className="overflow-x-auto">
        <table className="w-full border-collapse text-sm">
          {headers.length > 0 ? (
            <thead>
              <tr className="border-b bg-muted/40">
                {headers.map((header, index) => (
                  <th key={index} className="px-3 py-2 text-left font-medium text-muted-foreground">
                    {header}
                  </th>
                ))}
              </tr>
            </thead>
          ) : null}
          <tbody>
            {rows.map((row, rowIndex) => (
              <tr key={rowIndex} className="border-b last:border-b-0">
                {row.map((cell, cellIndex) => (
                  <td key={cellIndex} className="px-3 py-2 align-top">
                    {cell}
                  </td>
                ))}
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      {caption ? <p className="border-t bg-muted/20 px-3 py-1.5 text-xs text-muted-foreground">{caption}</p> : null}
    </div>
  );
}

import type { AttrValue, MdxNode } from '../types';
import { asArray, asOptionalString, asRecord, asString } from './propUtils';

type TimelineItem = { date?: string; label: string; body?: string };

function readItem(raw: AttrValue): TimelineItem {
  const record = asRecord(raw);
  return {
    date: asOptionalString(record.date),
    label: asString(record.label),
    body: asOptionalString(record.body),
  };
}

export function Timeline({ node }: { node: Extract<MdxNode, { kind: 'element' }> }) {
  const items = asArray(node.props.items).map(readItem);
  if (items.length === 0) return null;
  return (
    <ol className="flex flex-col">
      {items.map((item, index) => (
        <li key={index} className="relative flex gap-3 pb-4 last:pb-0">
          <div className="flex flex-col items-center">
            <span className="mt-1 size-2 shrink-0 rounded-full bg-primary" aria-hidden="true" />
            {index < items.length - 1 ? <span className="mt-1 w-px flex-1 bg-border" aria-hidden="true" /> : null}
          </div>
          <div className="min-w-0 pb-1">
            {item.date ? <p className="text-xs text-muted-foreground">{item.date}</p> : null}
            <p className="text-sm font-medium">{item.label}</p>
            {item.body ? <p className="text-xs text-muted-foreground">{item.body}</p> : null}
          </div>
        </li>
      ))}
    </ol>
  );
}

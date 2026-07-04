import { Check } from 'lucide-react';

import { cn } from '@/lib/utils';
import type { AttrValue, MdxNode } from '../types';
import { asArray, asBool, asRecord, asString } from './propUtils';
import { BlockCard } from './shared';

type ChecklistItem = { label: string; done: boolean; note?: string };

function readItems(raw: AttrValue | undefined): ChecklistItem[] {
  return asArray(raw).map((entry) => {
    const record = asRecord(entry);
    return {
      label: asString(record.label, asString(entry, '')),
      done: asBool(record.done ?? record.checked),
      note: asString(record.note) || undefined,
    };
  });
}

export function Checklist({ node }: { node: Extract<MdxNode, { kind: 'element' }> }) {
  const items = readItems(node.props.items);
  if (items.length === 0) return null;
  return (
    <BlockCard>
      <ul className="flex flex-col gap-2">
        {items.map((item, index) => (
          <li key={index} className="flex items-start gap-2.5">
            <span
              className={cn(
                'mt-0.5 flex size-4 shrink-0 items-center justify-center rounded-sm border',
                item.done ? 'border-primary bg-primary text-primary-foreground' : 'border-border bg-background',
              )}
              aria-hidden="true"
            >
              {item.done ? <Check className="size-3" /> : null}
            </span>
            <div className="min-w-0">
              <p className={cn('text-sm', item.done && 'text-muted-foreground line-through')}>{item.label}</p>
              {item.note ? <p className="text-xs text-muted-foreground">{item.note}</p> : null}
            </div>
          </li>
        ))}
      </ul>
    </BlockCard>
  );
}

import { X } from 'lucide-react';

import { Badge } from '@/components/ui/badge';
import { chipLabel } from '@/lib/conversations';
import type { ConversationContextChip } from '@/lib/types';
import { cn } from '@/lib/utils';

function chipTitle(chip: ConversationContextChip): string {
  switch (chip.kind) {
    case 'node':
      return `Node ${chip.id}`;
    case 'attachment':
      return `${chip.node} · ${chip.id} @ ${chip.revision}`;
    case 'range':
      return `${chip.node} · ${chip.attachment} @ ${chip.revision}`;
    case 'selection':
      return chip.text;
  }
}

/** Context chips (CHAT-SCOPE §5): the pinned scoped node first, then the
 * optional chips — removable on the composer, read-only on a sent message. */
export function ContextChips({
  pinned,
  chips,
  onRemove,
  className,
}: {
  pinned?: string | null;
  chips: ConversationContextChip[];
  onRemove?: (index: number) => void;
  className?: string;
}) {
  if (!pinned && chips.length === 0) return null;
  return (
    <div className={cn('flex min-w-0 flex-wrap items-center gap-1', className)} data-testid="context-chips">
      {pinned ? (
        <Badge variant="secondary" className="font-mono" title={`Every message carries ${pinned}`}>
          {pinned}
        </Badge>
      ) : null}
      {chips.map((chip, index) => {
        const label = chipLabel(chip);
        return (
          <Badge
            key={`${chip.kind}:${index}`}
            variant="outline"
            className={cn('max-w-64 font-mono', onRemove && 'gap-1 pr-1')}
            title={chipTitle(chip)}
          >
            <span className="truncate">{label}</span>
            {onRemove ? (
              <button
                type="button"
                aria-label={`Remove ${label}`}
                className="rounded-full p-0.5 transition-colors hover:bg-foreground/10"
                onClick={() => onRemove(index)}
              >
                <X className="size-3" />
              </button>
            ) : null}
          </Badge>
        );
      })}
    </div>
  );
}

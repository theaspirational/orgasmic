import type { ButtonHTMLAttributes, MouseEvent, PointerEvent } from 'react';
import { toast } from 'sonner';

import { badgeVariants } from '@/components/ui/badge';
import { copyText } from '@/lib/clipboard';
import { cn } from '@/lib/utils';

type CopyIdBadgeProps = Omit<ButtonHTMLAttributes<HTMLButtonElement>, 'children' | 'onClick'> & {
  value: string;
  label?: string;
  variant?: 'default' | 'secondary' | 'destructive' | 'outline' | 'ghost' | 'link';
};

export function CopyIdBadge({
  value,
  label = value,
  variant = 'outline',
  className,
  onPointerDown,
  ...props
}: CopyIdBadgeProps) {
  function copy(event: MouseEvent<HTMLButtonElement>) {
    event.preventDefault();
    event.stopPropagation();
    void copyText(value)
      .then(() => toast.success(`Copied ${value}`))
      .catch(() => toast.error(`Could not copy ${value}`));
  }

  function stopPointer(event: PointerEvent<HTMLButtonElement>) {
    event.stopPropagation();
    onPointerDown?.(event);
  }

  return (
    <button
      type="button"
      aria-label={`Copy ${label}`}
      title={`Copy ${label}`}
      className={cn(
        badgeVariants({ variant }),
        'cursor-copy select-none font-mono hover:border-primary/50 hover:bg-accent hover:text-accent-foreground',
        className,
      )}
      onPointerDown={stopPointer}
      onClick={copy}
      {...props}
    >
      {value}
    </button>
  );
}

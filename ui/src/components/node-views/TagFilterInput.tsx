import { useState } from 'react';
import { Check, ChevronsUpDown, X } from 'lucide-react';

import { Badge } from '@/components/ui/badge';
import {
  Command,
  CommandEmpty,
  CommandGroup,
  CommandInput,
  CommandItem,
  CommandList,
} from '@/components/ui/command';
import { Popover, PopoverContent, PopoverTrigger } from '@/components/ui/popover';
import { cn } from '@/lib/utils';

/**
 * A chip-style multi-select: the field shows each chosen tag as a removable
 * chip and opens a searchable command list to add more. Replaces a plain
 * search box where filtering is purely tag-based.
 */
export function TagFilterInput({
  options,
  selected,
  onChange,
  placeholder = 'Filter by tag',
  className,
  ariaControls,
}: {
  options: string[];
  selected: string[];
  onChange: (next: string[]) => void;
  placeholder?: string;
  className?: string;
  ariaControls?: string;
}) {
  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState('');

  function toggle(tag: string) {
    onChange(selected.includes(tag) ? selected.filter((t) => t !== tag) : [...selected, tag]);
  }

  function remove(tag: string) {
    onChange(selected.filter((t) => t !== tag));
  }

  return (
    <Popover open={open} onOpenChange={setOpen}>
      <PopoverTrigger asChild>
        <button
          type="button"
          role="combobox"
          aria-expanded={open}
          aria-controls={ariaControls}
          className={cn(
            'flex min-h-9 w-full items-center gap-1.5 rounded-md border border-input bg-transparent px-2 py-1 text-left text-sm shadow-xs transition-colors',
            'focus-visible:border-ring focus-visible:outline-none focus-visible:ring-[3px] focus-visible:ring-ring/50',
            className,
          )}
        >
          <div className="flex flex-1 flex-wrap items-center gap-1">
            {selected.length === 0 ? (
              <span className="px-1 text-muted-foreground">{placeholder}</span>
            ) : (
              selected.map((tag) => (
                <Badge key={tag} variant="secondary" className="gap-1 pr-1">
                  {tag}
                  <span
                    role="button"
                    tabIndex={-1}
                    aria-label={`Remove ${tag}`}
                    className="rounded-full p-0.5 transition-colors hover:bg-foreground/10"
                    onPointerDown={(event) => event.stopPropagation()}
                    onClick={(event) => {
                      event.stopPropagation();
                      remove(tag);
                    }}
                  >
                    <X className="size-3" />
                  </span>
                </Badge>
              ))
            )}
          </div>
          <ChevronsUpDown className="size-4 shrink-0 opacity-50" />
        </button>
      </PopoverTrigger>
      <PopoverContent align="start" className="min-w-72 w-[var(--radix-popover-trigger-width)] p-0">
        <Command>
          <CommandInput
            value={query}
            onValueChange={setQuery}
            placeholder="Search tags"
            onKeyDown={(event) => {
              if (event.key === 'Backspace' && query === '' && selected.length > 0) {
                remove(selected[selected.length - 1]);
              }
            }}
          />
          <CommandList>
            <CommandEmpty>No tags.</CommandEmpty>
            <CommandGroup>
              {options.map((tag) => {
                const active = selected.includes(tag);
                return (
                  <CommandItem key={tag} value={tag} onSelect={() => toggle(tag)}>
                    <Check className={cn('size-4', active ? 'opacity-100' : 'opacity-0')} />
                    {tag}
                  </CommandItem>
                );
              })}
            </CommandGroup>
          </CommandList>
        </Command>
      </PopoverContent>
    </Popover>
  );
}

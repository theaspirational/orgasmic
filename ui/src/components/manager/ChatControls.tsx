import { useMemo, useState } from 'react';
import {
  Bot,
  Box,
  BrainCircuit,
  ChevronDown,
  SlidersHorizontal,
} from 'lucide-react';

import { Button } from '@/components/ui/button';
import {
  Command,
  CommandEmpty,
  CommandGroup,
  CommandInput,
  CommandItem,
  CommandList,
} from '@/components/ui/command';
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuLabel,
  DropdownMenuRadioGroup,
  DropdownMenuRadioItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from '@/components/ui/dropdown-menu';
import { Popover, PopoverContent, PopoverTrigger } from '@/components/ui/popover';
import { cn } from '@/lib/utils';

import {
  availableChatAccess,
  CHAT_PROVIDER_ORDER,
  chatProviderLabel,
  type ChatModelOption,
  type ChatProviderId,
  type ChatSelection,
} from './chatProviders';

const EFFORT_LABELS: Record<string, string> = {
  low: 'Low',
  medium: 'Medium',
  high: 'High',
  xhigh: 'Extra High',
  max: 'Max',
  ultracode: 'Ultracode',
  ultrathink: 'Ultrathink',
};

function ProviderMark({ provider }: { provider: ChatProviderId }) {
  if (provider === 'codex') return <Bot className="size-4" />;
  if (provider === 'claude') return <BrainCircuit className="size-4" />;
  return <Box className="size-4" />;
}

export function ChatControls({
  value,
  onChange,
  models,
  availableProviders,
  disabled = false,
  liveCatalog = false,
  catalogLoading = false,
  catalogMessage = null,
  showAccess = false,
}: {
  value: ChatSelection;
  onChange: (next: ChatSelection) => void;
  models: ChatModelOption[];
  availableProviders: ChatProviderId[];
  disabled?: boolean;
  liveCatalog?: boolean;
  catalogLoading?: boolean;
  catalogMessage?: string | null;
  showAccess?: boolean;
}) {
  const [pickerOpen, setPickerOpen] = useState(false);
  const [query, setQuery] = useState('');
  const selectedModel = models.find((model) => model.id === value.model);
  const current = models.filter((model) => !model.legacy);
  const legacy = models.filter((model) => model.legacy);
  const efforts = useMemo(() => selectedModel?.efforts ?? [], [selectedModel]);
  const exactQuery = query.trim();
  const customCandidate =
    exactQuery.length > 0 && !models.some((model) => model.id === exactQuery);

  function selectProvider(provider: ChatProviderId) {
    if (!availableProviders.includes(provider)) return;
    if (liveCatalog && provider === value.provider) return;
    const supportedAccess = availableChatAccess(provider);
    onChange({
      ...value,
      provider,
      model: '',
      effort: '',
      access: supportedAccess.includes(value.access) ? value.access : 'full-access',
    });
    setQuery('');
  }

  function selectModel(model: string) {
    onChange({ ...value, model, effort: '' });
    setPickerOpen(false);
    setQuery('');
  }

  return (
    <div className="flex min-w-0 flex-wrap items-center gap-1.5">
      <Popover open={pickerOpen} onOpenChange={setPickerOpen}>
        <PopoverTrigger asChild>
          <Button
            type="button"
            variant="ghost"
            size="sm"
            disabled={disabled}
            className="h-8 min-w-0 max-w-full gap-1.5 px-2.5"
            aria-label="Choose provider and model"
          >
            <ProviderMark provider={value.provider} />
            <span className="truncate font-medium">
              {selectedModel?.label ?? (value.model || `${chatProviderLabel(value.provider)} default`)}
            </span>
            <ChevronDown className="size-3.5 text-muted-foreground" />
          </Button>
        </PopoverTrigger>
        <PopoverContent
          align="start"
          side="top"
          sideOffset={8}
          className="w-[min(36rem,calc(100vw-2rem))] overflow-hidden p-0"
        >
          <div className="grid min-h-80 grid-cols-[4.5rem_minmax(0,1fr)] sm:grid-cols-[9rem_minmax(0,1fr)]">
            <div className="flex flex-col gap-1 border-r p-2" aria-label="Chat providers">
              {CHAT_PROVIDER_ORDER.map((provider) => {
                const available = availableProviders.includes(provider);
                return (
                  <Button
                    key={provider}
                    type="button"
                    variant={provider === value.provider ? 'secondary' : 'ghost'}
                    size="sm"
                    disabled={!available || (liveCatalog && provider === value.provider)}
                    onClick={() => selectProvider(provider)}
                    className="h-10 justify-center gap-2 px-2 sm:justify-start"
                    aria-label={`${chatProviderLabel(provider)}${available ? '' : ' unavailable'}`}
                  >
                    <ProviderMark provider={provider} />
                    <span className="hidden sm:inline">{chatProviderLabel(provider)}</span>
                  </Button>
                );
              })}
            </div>
            <Command shouldFilter>
              <CommandInput
                value={query}
                onValueChange={setQuery}
                placeholder="Search models…"
                aria-label="Search models"
              />
              <CommandList className="max-h-80 p-2">
                {!liveCatalog || !value.model ? (
                  <CommandItem
                    value={`${chatProviderLabel(value.provider)} provider default`}
                    data-checked={!value.model}
                    disabled={liveCatalog}
                    onSelect={() => selectModel('')}
                    className="min-h-12"
                  >
                    <ProviderMark provider={value.provider} />
                    <div className="min-w-0">
                      <div className="font-medium">Provider default</div>
                      <div className="text-xs text-muted-foreground">
                        {liveCatalog ? 'Current runtime default' : 'Let the provider choose'}
                      </div>
                    </div>
                  </CommandItem>
                ) : null}
                {current.length > 0 ? (
                  <CommandGroup heading="Models">
                    {current.map((model) => (
                      <ModelItem
                        key={model.id}
                        model={model}
                        provider={value.provider}
                        selected={model.id === value.model}
                        onSelect={selectModel}
                      />
                    ))}
                  </CommandGroup>
                ) : null}
                {legacy.length > 0 ? (
                  <CommandGroup heading="Legacy models">
                    {legacy.map((model) => (
                      <ModelItem
                        key={model.id}
                        model={model}
                        provider={value.provider}
                        selected={model.id === value.model}
                        onSelect={selectModel}
                      />
                    ))}
                  </CommandGroup>
                ) : null}
                {customCandidate ? (
                  <CommandGroup heading="Custom model">
                    <CommandItem value={exactQuery} onSelect={() => selectModel(exactQuery)}>
                      <ProviderMark provider={value.provider} />
                      <span className="truncate">Use “{exactQuery}”</span>
                    </CommandItem>
                  </CommandGroup>
                ) : null}
                <CommandEmpty>
                  {catalogLoading
                    ? 'Loading provider models…'
                    : catalogMessage ?? 'No matching models. Type an exact model id or use the provider default.'}
                </CommandEmpty>
              </CommandList>
              {models.length === 0 && !liveCatalog && query.length === 0 ? (
                <p className="border-t px-4 py-3 text-xs leading-relaxed text-muted-foreground">
                  {catalogLoading
                    ? 'Loading provider models…'
                    : catalogMessage ?? 'No catalog was returned. Use the provider default, or type an exact model id.'}
                </p>
              ) : null}
            </Command>
          </div>
        </PopoverContent>
      </Popover>

      <DropdownMenu>
        <DropdownMenuTrigger asChild>
          <Button
            type="button"
            variant="ghost"
            size="icon-sm"
            disabled={disabled}
            aria-label="Chat traits"
            title="Chat traits"
            className="text-muted-foreground"
          >
            <SlidersHorizontal className="size-4" />
          </Button>
        </DropdownMenuTrigger>
        <DropdownMenuContent align="start" side="top" className="w-56">
          <DropdownMenuLabel>Reasoning</DropdownMenuLabel>
          <DropdownMenuRadioGroup
            value={value.effort || 'default'}
            onValueChange={(effort) =>
              onChange({ ...value, effort: effort === 'default' ? '' : effort })
            }
          >
            {!liveCatalog || !value.effort ? (
              <DropdownMenuRadioItem value="default" disabled={liveCatalog}>
                Provider default
              </DropdownMenuRadioItem>
            ) : null}
            {efforts.map((effort) => (
              <DropdownMenuRadioItem key={effort} value={effort}>
                {EFFORT_LABELS[effort] ?? effort}
              </DropdownMenuRadioItem>
            ))}
          </DropdownMenuRadioGroup>
          {value.provider !== 'opencode' ? (
            <>
              <DropdownMenuSeparator />
              <DropdownMenuLabel>Service tier</DropdownMenuLabel>
              <DropdownMenuRadioGroup
                value={value.serviceTier || 'standard'}
                onValueChange={(serviceTier) =>
                  onChange({
                    ...value,
                    serviceTier: serviceTier === 'fast' ? 'fast' : '',
                  })
                }
              >
                <DropdownMenuRadioItem value="standard">Standard</DropdownMenuRadioItem>
                <DropdownMenuRadioItem value="fast">Fast</DropdownMenuRadioItem>
              </DropdownMenuRadioGroup>
            </>
          ) : null}
          {showAccess ? (
            <>
              <DropdownMenuSeparator />
              <DropdownMenuLabel>Access</DropdownMenuLabel>
              <DropdownMenuRadioGroup
                value={value.access}
                onValueChange={(access) =>
                  onChange({ ...value, access: access as ChatSelection['access'] })
                }
              >
                {availableChatAccess(value.provider).map((access) => (
                  <DropdownMenuRadioItem key={access} value={access}>
                    {access === 'auto-accept-edits'
                      ? 'Auto-accept edits'
                      : access === 'full-access'
                        ? 'Full access'
                        : 'Auto'}
                  </DropdownMenuRadioItem>
                ))}
              </DropdownMenuRadioGroup>
            </>
          ) : null}
        </DropdownMenuContent>
      </DropdownMenu>
    </div>
  );
}

function ModelItem({
  model,
  provider,
  selected,
  onSelect,
}: {
  model: ChatModelOption;
  provider: ChatProviderId;
  selected: boolean;
  onSelect: (model: string) => void;
}) {
  return (
    <CommandItem
      value={`${model.label} ${model.id}`}
      data-checked={selected}
      onSelect={() => onSelect(model.id)}
      className={cn('min-h-12', selected && 'bg-muted')}
    >
      <ProviderMark provider={provider} />
      <div className="min-w-0">
        <div className="truncate font-medium">{model.label}</div>
        <div className="truncate text-xs text-muted-foreground">{chatProviderLabel(provider)}</div>
      </div>
    </CommandItem>
  );
}

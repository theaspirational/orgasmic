// orgasmic:TASK-SZEWA, dec_WDR5K
import { useEffect, useMemo, useState } from 'react';

import { Input } from '@/components/ui/input';
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select';
import { fetchManagerDrivers } from '@/lib/api';
import type { ManagerDriverProfile } from '@/lib/types';
import { useResource } from '@/lib/useResource';

export type TransportSelection = {
  mode: string;
  harness: string;
  model: string;
  effort: string;
  harness_args: string[];
};

/** Kind + installed (mode, harness) selectors backed by `/managers/drivers`. */
export function TransportPicker({
  kindLabel,
  value,
  onChange,
  requireInstalled = true,
}: {
  kindLabel: string;
  value: TransportSelection;
  onChange: (next: TransportSelection) => void;
  requireInstalled?: boolean;
}) {
  const drivers = useResource('transport-picker-drivers', fetchManagerDrivers);
  const profiles = useMemo(() => {
    const list = drivers.data?.drivers ?? [];
    return requireInstalled
      ? list.filter((d) => d.installed && (d.mode_installed ?? true))
      : list;
  }, [drivers.data?.drivers, requireInstalled]);

  useEffect(() => {
    if (!value.mode && !value.harness && profiles.length > 0) {
      const preferred =
        profiles.find((d) => d.mode === 'rmux' && d.harness === 'claude') ??
        profiles.find((d) => d.harness !== 'custom') ??
        profiles[0];
      if (preferred) {
        onChange({
          ...value,
          mode: preferred.mode,
          harness: preferred.harness,
        });
      }
    }
  }, [onChange, profiles, value]);

  const selectedKey = value.mode && value.harness ? `${value.mode}/${value.harness}` : '';

  return (
    <div className="flex flex-col gap-3">
      <div className="flex flex-col gap-1.5 text-sm">
        <span className="font-medium">Kind</span>
        <Input value={kindLabel} readOnly className="font-mono text-xs" />
      </div>
      <div className="flex flex-col gap-1.5 text-sm">
        <span className="font-medium">Mode / harness</span>
        <Select
          value={selectedKey}
          onValueChange={(key) => {
            const [mode, harness] = key.split('/');
            if (!mode || !harness) return;
            onChange({ ...value, mode, harness });
          }}
          disabled={profiles.length === 0}
        >
          <SelectTrigger>
            <SelectValue
              placeholder={
                drivers.loading
                  ? 'Loading drivers…'
                  : profiles.length === 0
                    ? 'No installed drivers'
                    : 'Select transport'
              }
            />
          </SelectTrigger>
          <SelectContent>
            {profiles.map((driver) => (
              <SelectItem
                key={`${driver.mode}/${driver.harness}`}
                value={`${driver.mode}/${driver.harness}`}
              >
                {driverLabel(driver)}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
      </div>
      {value.harness === 'custom' ? (
        <label className="flex flex-col gap-1.5 text-sm">
          <span className="font-medium">Custom argv</span>
          <Input
            value={value.harness_args.join(' ')}
            onChange={(event) =>
              onChange({
                ...value,
                harness_args: tokenizeHarnessArgs(event.target.value),
              })
            }
            placeholder="opencode --print-logs"
            className="font-mono text-xs"
          />
          <span className="text-[11px] text-muted-foreground">
            Space-separated tokens; preserved verbatim for the next launch.
          </span>
        </label>
      ) : null}
      <div className="grid gap-3 sm:grid-cols-2">
        <label className="flex flex-col gap-1.5 text-sm">
          <span className="font-medium">Model</span>
          <Input
            value={value.model}
            onChange={(event) => onChange({ ...value, model: event.target.value })}
            placeholder="harness default"
            className="font-mono text-xs"
          />
        </label>
        <label className="flex flex-col gap-1.5 text-sm">
          <span className="font-medium">Effort</span>
          <Input
            value={value.effort}
            onChange={(event) => onChange({ ...value, effort: event.target.value })}
            placeholder="harness default"
            className="font-mono text-xs"
          />
        </label>
      </div>
      <p className="text-[11px] text-muted-foreground">
        Leave model/effort empty to use the harness default. Values pass through unvalidated.
      </p>
    </div>
  );
}

function driverLabel(driver: ManagerDriverProfile): string {
  return `${driver.display_name} (${driver.mode}/${driver.harness})`;
}

export function emptyTransportSelection(): TransportSelection {
  return { mode: '', harness: '', model: '', effort: '', harness_args: [] };
}

function tokenizeHarnessArgs(input: string): string[] {
  const trimmed = input.trim();
  if (!trimmed) return [];
  return trimmed.split(/\s+/);
}

/** Local state helper for dialogs that need a transport selection. */
export function useTransportSelection(
  initial?: Partial<TransportSelection>,
): [TransportSelection, (next: TransportSelection) => void] {
  return useState<TransportSelection>({
    ...emptyTransportSelection(),
    ...initial,
  });
}

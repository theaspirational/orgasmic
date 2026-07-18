// orgasmic:TASK-SZEWA, dec_WDR5K
import { useEffect, useState } from 'react';
import { toast } from 'sonner';

import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select';
import { fetchRunRuntimeOptions, postRunRuntimeOptions } from '@/lib/api';
import type { RuntimeOptionsCatalog } from '@/lib/types';

/** Live structured catalog when the adapter supports it; otherwise free-text. */
export function RuntimeOptionsBar({ runId }: { runId: string }) {
  const [catalog, setCatalog] = useState<RuntimeOptionsCatalog | null>(null);
  const [unsupported, setUnsupported] = useState(false);
  const [model, setModel] = useState('');
  const [effort, setEffort] = useState('');
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    let cancelled = false;
    setCatalog(null);
    setUnsupported(false);
    setModel('');
    setEffort('');
    void fetchRunRuntimeOptions(runId)
      .then((response) => {
        if (cancelled) return;
        setCatalog(response.catalog);
        setModel(response.catalog.current.model ?? '');
        setEffort(response.catalog.current.reasoning_effort ?? '');
      })
      .catch(() => {
        if (cancelled) return;
        setUnsupported(true);
      });
    return () => {
      cancelled = true;
    };
  }, [runId]);

  async function apply() {
    setBusy(true);
    try {
      const response = await postRunRuntimeOptions(runId, {
        model: model.trim() || null,
        reasoning_effort: effort.trim() || null,
      });
      if (!response.accepted) {
        throw new Error(response.message ?? 'Harness rejected runtime options');
      }
      toast.success('Runtime options updated');
    } catch (err) {
      toast.error('Runtime options update failed', {
        description: err instanceof Error ? err.message : String(err),
      });
    } finally {
      setBusy(false);
    }
  }

  const liveSwitching = catalog?.live_switching ?? false;
  const canApplyLive = liveSwitching && !unsupported;

  if (unsupported && !catalog) {
    return (
      <div className="flex flex-wrap items-end gap-2 border-b px-3 py-2 text-xs text-muted-foreground">
        Live runtime switching is not available for this harness.
      </div>
    );
  }

  if (!catalog) return null;

  const hasModels = catalog.models.length > 0;
  return (
    <div className="flex flex-wrap items-end gap-2 border-b px-3 py-2">
      <label className="flex min-w-48 flex-1 flex-col gap-1 text-xs">
        <span className="text-muted-foreground">Model ({catalog.source})</span>
        {hasModels ? (
          <Select value={model} onValueChange={setModel} disabled={!canApplyLive}>
            <SelectTrigger className="h-8 font-mono text-xs">
              <SelectValue placeholder="harness default" />
            </SelectTrigger>
            <SelectContent>
              {catalog.models.map((entry) => (
                <SelectItem key={entry.id} value={entry.id}>
                  {entry.label}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
        ) : (
          <Input
            value={model}
            onChange={(event) => setModel(event.target.value)}
            placeholder="harness default"
            className="h-8 font-mono text-xs"
            disabled={!canApplyLive}
          />
        )}
      </label>
      <label className="flex min-w-28 flex-col gap-1 text-xs">
        <span className="text-muted-foreground">Effort</span>
        {catalog.efforts.length > 0 ? (
          <Select value={effort} onValueChange={setEffort} disabled={!canApplyLive}>
            <SelectTrigger className="h-8 font-mono text-xs">
              <SelectValue placeholder="harness default" />
            </SelectTrigger>
            <SelectContent>
              {catalog.efforts.map((entry) => (
                <SelectItem key={entry} value={entry}>
                  {entry}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
        ) : (
          <Input
            value={effort}
            onChange={(event) => setEffort(event.target.value)}
            placeholder="harness default"
            className="h-8 font-mono text-xs"
            disabled={!canApplyLive}
          />
        )}
      </label>
      {canApplyLive ? (
        <Button type="button" size="sm" disabled={busy} onClick={() => void apply()}>
          Apply
        </Button>
      ) : (
        <span className="pb-1 text-[11px] text-muted-foreground">Live switch unsupported</span>
      )}
    </div>
  );
}

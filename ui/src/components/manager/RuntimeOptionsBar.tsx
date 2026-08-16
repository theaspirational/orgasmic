import { useEffect, useState } from 'react';
import { Loader2 } from 'lucide-react';
import { toast } from 'sonner';

import { fetchRunRuntimeOptions, postRunRuntimeOptions } from '@/lib/api';
import type { RunRuntimeOptionsRequest, RuntimeOptionsCatalog } from '@/lib/types';

import { ChatControls } from './ChatControls';
import {
  chatProviderLabel,
  runtimeModels,
  type ChatProviderId,
  type ChatSelection,
} from './chatProviders';

/** T3-style model/traits controls backed by the live native-provider catalog. */
export function RuntimeOptionsBar({
  runId,
  provider = 'codex',
}: {
  runId: string;
  provider?: ChatProviderId;
}) {
  const [catalog, setCatalog] = useState<RuntimeOptionsCatalog | null>(null);
  const [unsupported, setUnsupported] = useState(false);
  const [selection, setSelection] = useState<ChatSelection>({
    provider,
    model: '',
    effort: '',
    serviceTier: '',
    access: 'full-access',
  });
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    let cancelled = false;
    setCatalog(null);
    setUnsupported(false);
    setSelection({ provider, model: '', effort: '', serviceTier: '', access: 'full-access' });
    void fetchRunRuntimeOptions(runId)
      .then((response) => {
        if (cancelled) return;
        setCatalog(response.catalog);
        setSelection({
          provider,
          model: response.catalog.current.model ?? '',
          effort: response.catalog.current.reasoning_effort ?? '',
          serviceTier: response.catalog.current.speed === 'fast' ? 'fast' : '',
          access: 'full-access',
        });
      })
      .catch(() => {
        if (!cancelled) setUnsupported(true);
      });
    return () => {
      cancelled = true;
    };
  }, [provider, runId]);

  async function changeRuntime(next: ChatSelection) {
    const previous = selection;
    setSelection(next);
    if (!(catalog?.live_switching ?? false)) return;
    setBusy(true);
    try {
      const patch: RunRuntimeOptionsRequest = {
        ...(next.model !== previous.model ? { model: next.model || null } : {}),
        ...(next.effort !== previous.effort
          ? { reasoning_effort: next.effort || null }
          : {}),
        ...(next.serviceTier !== previous.serviceTier
          ? { speed: next.serviceTier === 'fast' ? 'fast' : 'normal' }
          : {}),
      };
      const response = await postRunRuntimeOptions(runId, patch);
      if (!response.accepted) {
        throw new Error(response.message ?? 'Provider rejected the change');
      }
    } catch (err) {
      setSelection(previous);
      toast.error('Runtime options update failed', {
        description: err instanceof Error ? err.message : String(err),
      });
    } finally {
      setBusy(false);
    }
  }

  if (unsupported && !catalog) {
    return (
      <span className="text-xs text-muted-foreground">
        {chatProviderLabel(provider)} options are fixed for this conversation
      </span>
    );
  }

  if (!catalog) {
    return (
      <Loader2 className="size-3.5 animate-spin text-muted-foreground motion-reduce:animate-none" />
    );
  }

  return (
    <ChatControls
      value={selection}
      onChange={(next) => void changeRuntime(next)}
      models={runtimeModels(catalog.models)}
      availableProviders={[provider]}
      disabled={busy || !catalog.live_switching}
      liveCatalog
    />
  );
}

import type {
  ManagerChatCatalogResponse,
  RunSummary,
  RuntimeModelOption,
} from '@/lib/types';

export type ChatProviderId = 'codex' | 'claude' | 'opencode';
export type ChatAccess = 'supervised' | 'auto-accept-edits' | 'auto' | 'full-access';

export type ChatModelOption = {
  id: string;
  label: string;
  legacy?: boolean;
  efforts?: string[];
};

export type ChatSelection = {
  provider: ChatProviderId;
  model: string;
  effort: string;
  serviceTier: '' | 'fast';
  access: ChatAccess;
};

export const CHAT_PROVIDER_ORDER: ChatProviderId[] = ['codex', 'claude', 'opencode'];

export function availableChatAccess(provider: ChatProviderId): ChatAccess[] {
  if (provider === 'codex') return ['auto-accept-edits', 'auto', 'full-access'];
  if (provider === 'claude') return ['auto', 'full-access'];
  // OpenCode SDK approvals require an explicit permission-reply transport.
  // Until RunDock exposes that control, never offer a mode that can deadlock
  // a turn waiting for an approval the UI cannot resolve.
  return ['full-access'];
}

export function chatProviderLabel(provider: ChatProviderId): string {
  if (provider === 'codex') return 'Codex';
  if (provider === 'claude') return 'Claude';
  return 'OpenCode';
}

export function isNativeChatRun(
  run: Pick<RunSummary, 'driver' | 'harness'>,
): boolean {
  return (
    run.driver === 'stdio' &&
    (run.harness === 'codex-chat' ||
      run.harness === 'claude-sdk' ||
      run.harness === 'opencode')
  );
}

export function chatProviderFromRun(
  run: Pick<RunSummary, 'driver' | 'harness'>,
): ChatProviderId | null {
  if (run.driver !== 'stdio') return null;
  if (run.harness === 'codex-chat') return 'codex';
  if (run.harness === 'claude-sdk') return 'claude';
  if (run.harness === 'opencode') return 'opencode';
  return null;
}

export function availableChatProviders(
  catalog: ManagerChatCatalogResponse | null,
): ChatProviderId[] {
  return CHAT_PROVIDER_ORDER.filter((provider) => {
    const entry = catalog?.providers.find((candidate) => candidate.id === provider);
    return Boolean(entry && !entry.message);
  });
}

export function setupModels(
  catalog: ManagerChatCatalogResponse | null,
  provider: ChatProviderId,
): ChatModelOption[] {
  return (catalog?.providers.find((entry) => entry.id === provider)?.models ?? []).map(
    (model) => ({
      id: model.id,
      label: model.label,
      legacy: model.legacy,
      efforts: model.reasoning_efforts,
    }),
  );
}

export function setupCatalogMessage(
  catalog: ManagerChatCatalogResponse | null,
  provider: ChatProviderId,
): string | null {
  return catalog?.providers.find((entry) => entry.id === provider)?.message ?? null;
}

export function runtimeModels(models: RuntimeModelOption[]): ChatModelOption[] {
  return models.map((model) => ({
    id: model.id,
    label: model.label,
    efforts: model.reasoning_efforts,
  }));
}

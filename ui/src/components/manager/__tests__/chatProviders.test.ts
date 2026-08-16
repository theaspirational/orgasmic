import { describe, expect, it } from 'vitest';

import type { ManagerChatCatalogResponse } from '@/lib/types';

import {
  availableChatAccess,
  availableChatProviders,
  chatProviderFromRun,
  isNativeChatRun,
  setupModels,
} from '../chatProviders';

describe('canonical chat providers', () => {
  it('offers only access modes each headless provider can resolve', () => {
    expect(availableChatAccess('codex')).toEqual([
      'auto-accept-edits',
      'auto',
      'full-access',
    ]);
    expect(availableChatAccess('claude')).toEqual(['auto', 'full-access']);
    expect(availableChatAccess('opencode')).toEqual(['full-access']);
  });

  it('advertises only SDK/app-server providers whose live catalog probe succeeded', () => {
    const catalog: ManagerChatCatalogResponse = {
      providers: [
        { id: 'codex', source: 'codex:model/list', models: [], message: null },
        { id: 'claude', source: 'claude-agent-sdk:2.1.233', models: [] },
        {
          id: 'opencode',
          source: 'opencode-sdk',
          models: [],
          message: 'OpenCode is unavailable',
        },
      ],
    };

    expect(availableChatProviders(catalog)).toEqual(['codex', 'claude']);
  });

  it('recognizes only dedicated Chat runtimes and recovers their provider', () => {
    const runs = [
      { driver: 'stdio', harness: 'codex-chat' },
      { driver: 'stdio', harness: 'claude-sdk' },
      { driver: 'stdio', harness: 'opencode' },
    ];
    expect(runs.every(isNativeChatRun)).toBe(true);
    expect(runs.map(chatProviderFromRun)).toEqual(['codex', 'claude', 'opencode']);
    expect(isNativeChatRun({ driver: 'stdio', harness: 'claude' })).toBe(false);
    expect(isNativeChatRun({ driver: 'tmux', harness: 'opencode' })).toBe(false);
  });

  it('maps the SDK-backed catalog into picker models', () => {
    const catalog: ManagerChatCatalogResponse = {
      providers: [
        {
          id: 'claude',
          source: 'claude-agent-sdk:2.1.233',
          models: [
            {
              id: 'claude-sonnet-5',
              label: 'Claude Sonnet 5',
              legacy: false,
              reasoning_efforts: ['low', 'high'],
            },
          ],
        },
      ],
    };

    expect(setupModels(catalog, 'claude')).toEqual([
      {
        id: 'claude-sonnet-5',
        label: 'Claude Sonnet 5',
        legacy: false,
        efforts: ['low', 'high'],
      },
    ]);
  });
});

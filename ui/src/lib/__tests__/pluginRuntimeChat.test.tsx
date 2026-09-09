// @vitest-environment jsdom
import { afterEach, expect, it, vi } from 'vitest';

import { createPluginRuntime, type PluginChatDock, type PluginContext, type PluginStatus } from '../pluginRuntime';

afterEach(() => vi.restoreAllMocks());

function status(id: string): PluginStatus {
  return { id, revision: '1', enabled: true, error: null, manifest: { ui: 'ui/index.js', node_type: { collection: id, id_prefix: `${id}-`, label: id, label_plural: id, required_properties: [], states: [], transitions: {}, regenerate_prompt: null, chat_prompt: null } } };
}

it('routes openChat and chatContext to the dock handle, and warns without one', async () => {
  const dock: PluginChatDock = { openChat: vi.fn(), chatContext: vi.fn() };
  let handle: PluginChatDock | null = null;
  let ctx!: PluginContext;
  const runtime = createPluginRuntime('demo', { baseUrl: 'http://localhost' }, async () => ({ register(c: PluginContext) { ctx = c; } }), () => {}, () => handle);
  await runtime.reconcile([status('meetings')]);
  const warn = vi.spyOn(console, 'warn').mockImplementation(() => {});
  const range = { kind: 'range', node: 'MEET-1', attachment: 'rec', revision: 'sha', start_ms: 0, end_ms: 30000 } as const;

  // No dock mounted: a warning, never a throw.
  ctx.openChat({ node: 'MEET-1', purpose: 'meeting', context: [range] });
  ctx.chatContext([range]);
  expect(warn).toHaveBeenCalledTimes(2);
  expect(dock.openChat).not.toHaveBeenCalled();

  handle = dock;
  ctx.openChat({ node: 'MEET-1', purpose: 'meeting', context: [range] });
  expect(dock.openChat).toHaveBeenCalledWith({ node: 'MEET-1', purpose: 'meeting', context: [range] });
  ctx.chatContext(null);
  expect(dock.chatContext).toHaveBeenCalledWith(null);
  expect(warn).toHaveBeenCalledTimes(2);

  runtime.dispose();
  expect(() => ctx.openChat({ node: 'MEET-1' })).toThrow('Plugin disposed');
});

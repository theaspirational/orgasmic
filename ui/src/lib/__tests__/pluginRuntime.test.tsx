// @vitest-environment jsdom
import '@testing-library/jest-dom/vitest';
import { act, cleanup, render, screen } from '@testing-library/react';
import { afterEach, expect, it, vi } from 'vitest';
import { createPluginRuntime, PluginRuntimeContext, usePluginView, type PluginContext, type PluginStatus } from '../pluginRuntime';

afterEach(cleanup);
function status(id: string, revision: string): PluginStatus {
  return { id, revision, enabled: true, error: null, manifest: { ui: 'ui/index.js', node_type: { collection: id, id_prefix: `${id}-`, label: id, label_plural: id, required_properties: [], states: [], transitions: {}, regenerate_prompt: null } } };
}
function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((done) => { resolve = done; });
  return { promise, resolve };
}
function View({ collection }: { collection: string }) {
  const plugin = usePluginView(collection);
  return plugin ? <plugin.View projectId="demo" collection={collection} onOpenNode={() => {}} /> : <p>Generic {collection}</p>;
}

it('registers, atomically reloads current views, isolates failures, preserves drafts, and aborts all disposed requests', async () => {
  type Module = { register: (ctx: PluginContext) => void | (() => void) | Promise<() => void> };
  const first = deferred<Module>();
  const pending = deferred<Module>();
  const reverted = deferred<Module>();
  const reports = vi.fn();
  let meetings!: PluginContext;
  let replacement!: PluginContext;
  const undo = vi.fn();
  const lateUndo = vi.fn();
  const load = vi.fn(async (url: string): Promise<Module> => {
    if (url.includes('/@1/')) return first.promise;
    if (url.includes('/@2/')) return { register(ctx) { ctx.registerStyles('.bad { color: red; }'); throw new Error('broken save'); } };
    if (url.includes('/@3/')) return { register(ctx) { replacement = ctx; ctx.registerNodeView('meetings', () => <p>Meetings revised</p>); return undo; } };
    if (url.includes('/@late/')) return pending.promise;
    if (url.includes('/@reverted/')) return reverted.promise;
    return { register(ctx) { ctx.registerNodeView('notes', () => <p>Notes plugin</p>); return () => { throw new Error('bad disposer'); }; } };
  });
  const runtime = createPluginRuntime('demo', { baseUrl: 'http://localhost' }, load, reports);
  render(<PluginRuntimeContext.Provider value={runtime}><View collection="meetings" /><View collection="notes" /></PluginRuntimeContext.Provider>);
  let opening!: Promise<void>;
  await act(async () => {
    opening = runtime.reconcile([status('meetings', '1'), status('notes', 'n')]);
  });
  expect(screen.getByText('Notes plugin')).toBeInTheDocument();
  await act(async () => {
    first.resolve({ register(ctx) { meetings = ctx; ctx.registerNodeView('meetings', () => <p>Meetings plugin</p>); ctx.registerStyles('.list { display: grid; }'); } });
    await opening;
  });
  expect(screen.getByText('Meetings plugin')).toBeInTheDocument();
  expect(screen.getByText('Notes plugin')).toBeInTheDocument(); // no stale shadow-map overwrite
  expect(() => meetings.registerNodeView('meetings', () => null)).toThrow('register only inside register(ctx)');
  expect(() => meetings.registerStyles('p { color: red; }')).toThrow('register only inside register(ctx)');
  expect(document.querySelectorAll('style[data-plugin]')).toHaveLength(1);
  meetings.setDraft('MEET-1', { title: 'Unsaved notes' });
  await act(async () => { await runtime.reconcile([status('meetings', '2'), status('notes', 'n')]); });
  expect(screen.getByText('Meetings plugin')).toBeInTheDocument();
  expect(document.querySelectorAll('style[data-plugin]')).toHaveLength(1);
  await runtime.reconcile([status('meetings', '2'), status('notes', 'n')]);
  expect(reports).toHaveBeenCalledTimes(1);
  await act(async () => { await runtime.reconcile([status('meetings', '3'), status('notes', 'n')]); });
  expect(screen.getByText('Meetings revised')).toBeInTheDocument();
  expect(meetings.signal.aborted).toBe(true);
  expect(replacement.getDraft('MEET-1')).toEqual({ title: 'Unsaved notes' });
  let reverting!: Promise<void>;
  await act(async () => { reverting = runtime.reconcile([status('meetings', 'reverted'), status('notes', 'n')]); });
  await act(async () => { await runtime.reconcile([status('meetings', '3'), status('notes', 'n')]); });
  const revertedRegister = vi.fn();
  await act(async () => { reverted.resolve({ register: revertedRegister }); await reverting; });
  expect(revertedRegister).not.toHaveBeenCalled();
  expect(screen.getByText('Meetings revised')).toBeInTheDocument();
  let requestSignal!: AbortSignal;
  const fetchMock = vi.spyOn(globalThis, 'fetch').mockImplementation((_url, init) => {
    requestSignal = init!.signal as AbortSignal;
    return new Promise((_resolve, reject) => requestSignal.addEventListener('abort', () => reject(new DOMException('Disposed', 'AbortError'))));
  });
  const request = replacement.get('/org/node?id=MEET-1').catch((error) => error.name);
  const posting = replacement.post('/org/node?project=other', { project: 'other', title: 'Scoped' }).catch((error) => error.name);
  const [postUrl, postInit] = fetchMock.mock.calls[1];
  expect(new URL(String(postUrl)).searchParams.get('project')).toBe('demo');
  expect(JSON.parse(String(postInit?.body)).project).toBe('demo');
  expect(() => replacement.get('https://unrelated.example/api')).toThrow('relative API path');
  let late!: Promise<void>;
  await act(async () => { late = runtime.reconcile([status('meetings', '3'), status('notes', 'late')]); });
  await act(async () => { await runtime.reconcile([]); });
  expect(requestSignal.aborted).toBe(true);
  expect(await request).toBe('AbortError');
  expect(await posting).toBe('AbortError');
  expect(screen.getByText('Generic meetings')).toBeInTheDocument();
  expect(screen.getByText('Generic notes')).toBeInTheDocument();
  expect(undo).toHaveBeenCalledOnce();
  expect(document.querySelectorAll('style[data-plugin]')).toHaveLength(0);
  await act(async () => { pending.resolve({ register() { return lateUndo; } }); await late; });
  expect(lateUndo).not.toHaveBeenCalled(); // register never runs after a disabled import resolves
  runtime.dispose();
  fetchMock.mockRestore();
});

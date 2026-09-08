import { Component, createContext, useContext, useEffect, useMemo, useRef, useSyncExternalStore, type ComponentType, type ReactNode } from 'react';
import { toast } from 'sonner';
import type { NodeTypeDescriptor } from './api';
import { ensurePluginUiSession, requestWithProfile, type TransportProfile } from './transport';
import { useEventStream } from '@/hooks/useEventStream';
import { subscribeResync } from './resync';

export type PluginStatus = {
  id: string; enabled: boolean; error: string | null; revision: string;
  manifest: { ui: string | null; node_type: NodeTypeDescriptor | null } | null;
};
export type PluginViewProps = { projectId: string; collection: string; nodeId?: string; onOpenNode: (id: string) => void };
export type PluginContext = {
  pluginId: string; projectId: string; signal: AbortSignal;
  registerNodeView: (collection: string, view: ComponentType<PluginViewProps>) => () => void;
  registerStyles: (css: string) => () => void;
  get: <T>(path: string) => Promise<T>;
  post: <T>(path: string, body?: unknown) => Promise<T>;
  getDraft: <T>(nodeId: string) => T | undefined;
  setDraft: (nodeId: string, draft: unknown) => void;
  clearDraft: (nodeId: string) => void;
};
type PluginModule = { register: (ctx: PluginContext) => void | (() => void) | Promise<void | (() => void)> };
type Registration = { pluginId: string; revision: string; View: ComponentType<PluginViewProps> };
type Activation = { revision: string; dispose: () => void };
const emptyViews = new Map<string, Registration>();
const noopSubscribe = () => () => {};
const reportedSessionErrors = new Set<string>();

export function createPluginRuntime(projectId: string, profile: TransportProfile,
  load: (url: string) => Promise<PluginModule> = (url) => import(/* @vite-ignore */ url),
  report: (message: string) => void = (message) => toast.error(message),
) {
  let views = emptyViews;
  let closed = false;
  const active = new Map<string, Activation>();
  const pending = new Map<string, Activation>();
  const attempted = new Map<string, string>();
  const listeners = new Set<() => void>();
  const drafts = new Map<string, unknown>();
  const emit = () => listeners.forEach((listener) => listener());
  const safely = (fn: () => void) => { try { fn(); } catch (error) { report(`Plugin cleanup failed: ${String(error)}`); } };
  function remove(id: string) {
    const old = active.get(id);
    active.delete(id);
    views = new Map([...views].filter(([, view]) => view.pluginId !== id));
    safely(() => old?.dispose());
    safely(() => pending.get(id)?.dispose());
    pending.delete(id);
    attempted.delete(id);
    emit();
  }
  async function activate(status: PluginStatus) {
    const { id, revision } = status;
    // A file can revert to the active revision while its replacement imports.
    const inFlight = pending.get(id);
    if (inFlight && inFlight.revision !== revision) { pending.delete(id); attempted.delete(id); inFlight.dispose(); }
    if (active.get(id)?.revision === revision || attempted.get(id) === revision) return;
    attempted.set(id, revision);
    pending.get(id)?.dispose();
    const controller = new AbortController();
    const cleanup: Array<() => void> = [];
    const styles: HTMLStyleElement[] = [];
    const staged = new Map<string, Registration>();
    let disposed = false;
    let registering = false;
    const activation = { revision, dispose() {
      if (disposed) return;
      disposed = true;
      controller.abort();
      for (const undo of cleanup.reverse()) safely(undo);
    } };
    pending.set(id, activation);
    const alive = () => { if (disposed || closed) throw new DOMException('Plugin disposed', 'AbortError'); };
    const scoped = (path: string) => {
      const url = new URL(path, 'http://plugin.invalid');
      if (url.origin !== 'http://plugin.invalid') throw new Error('ctx transport requires a relative API path');
      url.searchParams.set('project', projectId);
      return url.pathname + url.search;
    };
    const ctx: PluginContext = {
      pluginId: id, projectId, signal: controller.signal,
      registerNodeView(collection, View) {
        alive();
        if (!registering) throw new Error('register only inside register(ctx)');
        if (collection !== status.manifest?.node_type?.collection) throw new Error('Plugin can only register its own collection');
        const registration = { pluginId: id, revision, View };
        staged.set(collection, registration);
        const undo = () => {
          if (staged.get(collection) === registration) staged.delete(collection);
          if (views.get(collection) === registration) { views = new Map(views); views.delete(collection); emit(); }
        };
        cleanup.push(undo);
        return undo;
      },
      registerStyles(css) {
        alive();
        if (!registering) throw new Error('register only inside register(ctx)');
        const style = document.createElement('style');
        style.dataset.plugin = id;
        // Native CSS nesting, under the host-owned plugin wrapper. UI is
        // trusted same-origin code; selector scoping is not a sandbox.
        style.textContent = `[data-plugin="${id}"] {\n${css}\n}`;
        styles.push(style);
        const undo = () => { style.remove(); const index = styles.indexOf(style); if (index >= 0) styles.splice(index, 1); };
        cleanup.push(undo);
        return undo;
      },
      get<T>(path: string) { alive(); return requestWithProfile<T>(profile, scoped(path), { signal: controller.signal }); },
      post<T>(path: string, body?: unknown) {
        alive();
        if (body != null && (typeof body !== 'object' || Array.isArray(body))) throw new Error('ctx.post requires a JSON object');
        return requestWithProfile<T>(profile, scoped(path), { method: 'POST', body: { ...body as object, project: projectId }, signal: controller.signal });
      },
      getDraft: <T,>(nodeId: string) => drafts.get(`${id}:${nodeId}`) as T | undefined,
      setDraft(nodeId, draft) { alive(); drafts.set(`${id}:${nodeId}`, draft); },
      clearDraft(nodeId) { drafts.delete(`${id}:${nodeId}`); },
    };
    try {
      const url = `/plugins/${encodeURIComponent(id)}/ui/${encodeURIComponent(projectId)}/@${revision}/index.js`;
      const module = await load(url);
      alive();
      registering = true;
      const result = module.register(ctx);
      const undo = result instanceof Promise ? await result : result;
      registering = false;
      if (undo) cleanup.push(undo);
      // A newer reconciliation may have disabled/replaced this pending import.
      if (disposed || closed || pending.get(id) !== activation) {
        if (undo) safely(undo);
        return;
      }
      const previous = active.get(id);
      // Read CURRENT views at commit, never a snapshot captured before await.
      views = new Map([...views].filter(([, view]) => view.pluginId !== id));
      for (const [collection, view] of staged) views.set(collection, view);
      styles.forEach((style) => document.head.append(style));
      active.set(id, activation);
      pending.delete(id);
      safely(() => previous?.dispose());
      emit();
    } catch (error) {
      registering = false;
      activation.dispose();
      if (pending.get(id) === activation) pending.delete(id);
      if (!closed && !(error instanceof DOMException && error.name === 'AbortError')) report(`Plugin ${id} failed to load: ${String(error)}`);
    }
  }
  return {
    subscribe(listener: () => void) { listeners.add(listener); return () => { listeners.delete(listener); }; },
    snapshot: () => views,
    resume() { closed = false; },
    async reconcile(statuses: PluginStatus[]) {
      if (closed) return;
      const desired = statuses.filter((p) => p.enabled && !p.error && p.manifest?.ui);
      const ids = new Set(desired.map((p) => p.id));
      for (const id of new Set([...active.keys(), ...pending.keys(), ...attempted.keys()])) if (!ids.has(id)) remove(id);
      await Promise.all(desired.map(activate));
    },
    dispose() { closed = true; for (const id of new Set([...active.keys(), ...pending.keys()])) remove(id); attempted.clear(); drafts.clear(); },
  };
}

type Runtime = ReturnType<typeof createPluginRuntime>;
export const PluginRuntimeContext = createContext<Runtime | null>(null);

export function usePluginRuntime(projectId: string | null, profile: TransportProfile, enabled: boolean, identity: string) {
  const runtime = useMemo(() => createPluginRuntime(projectId ?? '', profile), [projectId, profile.baseUrl, profile.token, identity]);
  const refreshRef = useRef(() => {});
  useEventStream((event) => {
    if (event.topic === 'board' && event.payload.kind === 'board_refreshed') refreshRef.current();
  });
  useEffect(() => {
    if (!projectId || !enabled) return;
    runtime.resume();
    const controller = new AbortController();
    let revision = 0;
    let sessionReady = false;
    let previousError = '';
    async function refresh() {
      if (controller.signal.aborted) return;
      const current = ++revision;
      const stale = () => controller.signal.aborted || current !== revision;
      try {
        const statuses = await requestWithProfile<PluginStatus[]>(profile, `/plugins?project=${encodeURIComponent(projectId!)}`, { signal: controller.signal });
        if (stale()) return;
        if (!sessionReady && statuses.some((p) => p.enabled && !p.error && p.manifest?.ui)) {
          try {
            await ensurePluginUiSession(profile, controller.signal);
            sessionReady = true;
          } catch (error) {
            const key = `${profile.baseUrl}:${String(error)}`;
            if (!stale() && !reportedSessionErrors.has(key)) { reportedSessionErrors.add(key); toast.error(String(error)); }
            return;
          }
        }
        // A slow import/register must not block observing a later disable event.
        if (!stale()) void runtime.reconcile(statuses);
        previousError = '';
      } catch (error) {
        if (!controller.signal.aborted && String(error) !== previousError) { previousError = String(error); toast.error(`Plugin status unavailable: ${previousError}`); }
      }
    }
    refreshRef.current = () => { void refresh(); };
    const unsubscribe = subscribeResync(refreshRef.current);
    void refresh();
    return () => { controller.abort(); refreshRef.current = () => {}; unsubscribe(); runtime.dispose(); };
  }, [runtime, projectId, enabled]);
  return runtime;
}

export function usePluginView(collection: string) {
  const runtime = useContext(PluginRuntimeContext);
  return useSyncExternalStore(runtime?.subscribe ?? noopSubscribe, runtime?.snapshot ?? (() => emptyViews)).get(collection);
}

export class PluginViewBoundary extends Component<{ children: ReactNode; fallback: ReactNode }, { failed: boolean }> {
  state = { failed: false };
  static getDerivedStateFromError() { return { failed: true }; }
  componentDidCatch(error: Error) { toast.error(`Plugin view failed: ${error.message}`); }
  render() { return this.state.failed ? this.props.fallback : this.props.children; }
}

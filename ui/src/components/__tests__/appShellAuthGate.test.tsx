// @vitest-environment jsdom
import '@testing-library/jest-dom/vitest';
import { act, cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import type { ReactNode } from 'react';

const routeMocks = vi.hoisted(() => ({
  pathname: '/projects/orsl/artifacts/ART-P81EE',
  renderOutlet: vi.fn(() => null as ReactNode),
  navigate: vi.fn(),
}));

vi.mock('@tanstack/react-router', async () => {
  const React = await import('react');
  const Link = React.forwardRef<HTMLAnchorElement, Record<string, unknown>>(function Link(
    { children, to, params: _params, search: _search, activeProps: _activeProps, ...props },
    ref,
  ) {
    return (
      <a ref={ref} href={typeof to === 'string' ? to : '#'} {...props}>
        {children as ReactNode}
      </a>
    );
  });

  return {
    Link,
    Outlet: () => routeMocks.renderOutlet(),
    useNavigate: () => routeMocks.navigate,
    useRouterState: ({ select }: { select?: (state: { location: { pathname: string } }) => unknown } = {}) => {
      const state = { location: { pathname: routeMocks.pathname } };
      return select ? select(state) : state;
    },
  };
});

vi.mock('@/hooks/useEventStream', () => ({
  useEventStream: vi.fn(),
  useWsStatus: () => 'closed',
}));

vi.mock('@/lib/appUpdate', () => ({
  UPDATE_AUTO_CHECK_MS: 60_000,
  UPDATE_LAST_NOTIFIED_KEY: 'test:last-update-notified',
  checkAppUpdate: vi.fn(async () => null),
  savedUpdateChannel: () => 'stable',
}));

vi.mock('@/components/ConnectionBanner', () => ({
  ConnectionBanner: () => null,
}));

vi.mock('@/components/ProjectTabs', () => ({
  ProjectTabs: () => null,
}));

vi.mock('@/components/ProjectAddDialog', () => ({
  ProjectAddDialog: () => null,
}));

vi.mock('@/components/ProjectsManageDialog', () => ({
  ProjectsManageDialog: () => null,
}));

vi.mock('@/components/notifications/NotificationBell', () => ({
  NotificationBell: () => null,
}));

vi.mock('@/components/manager/RunDock', () => ({
  RunDock: () => null,
}));

import { AppShell } from '../AppShell';
import { MeProvider } from '@/hooks/useMe';
import { RefreshProvider } from '@/hooks/useRefreshBus';
import { BackendProfileProvider } from '@/lib/backend';
import { ThemeProvider } from '@/lib/theme';
import { fetchArtifact } from '@/lib/api';
import { setAuthMode } from '@/lib/transport';
import { useResource } from '@/lib/useResource';
import type { ArtifactDetail, Me } from '@/lib/types';

const events: string[] = [];
let pendingMe: Deferred<Response> | null = null;

type Deferred<T> = {
  promise: Promise<T>;
  resolve: (value: T) => void;
  reject: (reason?: unknown) => void;
};

function defer<T>(): Deferred<T> {
  let resolve!: (value: T) => void;
  let reject!: (reason?: unknown) => void;
  const promise = new Promise<T>((res, rej) => {
    resolve = res;
    reject = rej;
  });
  return { promise, resolve, reject };
}

function jsonResponse(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { 'content-type': 'application/json' },
  });
}

function textResponse(body: string, status: number): Response {
  return new Response(body, { status, headers: { 'content-type': 'text/plain' } });
}

function memberMe(): Me {
  return {
    identity: 'member',
    name: 'member',
    projects: [
      {
        projectId: 'orsl',
        role: 'reader',
        capabilities: ['project.read', 'artifacts.read', 'artifacts.comment'],
      },
    ],
  };
}

function artifact(): ArtifactDetail {
  return {
    id: 'ART-P81EE',
    title: 'Deep-linked artifact',
    subject_nodes: [],
    version: 1,
    state: 'submitted',
    open_comment_count: 0,
    prompt: 'show the artifact',
    content: 'artifact body',
    comments: [],
  };
}

function pathOf(input: RequestInfo | URL): string {
  const url = new URL(String(input), window.location.origin);
  return `${url.pathname}${url.search}`;
}

function installFetch(handler?: (path: string, init?: RequestInit) => Response | Promise<Response> | undefined) {
  const fetchMock = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
    const path = pathOf(input);
    const handled = handler?.(path, init);
    if (handled) return handled;

    if (path === '/api/projects') return jsonResponse([]);
    if (path === '/api/login') {
      events.push('login');
      return jsonResponse({ name: 'member', expires_at: '2099-01-01T00:00:00Z' });
    }
    if (path === '/api/me') {
      events.push('me');
      return pendingMe ? pendingMe.promise : jsonResponse(memberMe());
    }
    if (path === '/api/healthz') {
      events.push('admin-healthz');
      expect(init?.headers).toMatchObject({ authorization: 'Bearer admin-token' });
      return jsonResponse({ ok: true });
    }
    if (path === '/api/daemon/status') {
      events.push('admin-status');
      expect(init?.headers).toMatchObject({ authorization: 'Bearer admin-token' });
      return jsonResponse({ ok: true });
    }
    if (path === '/api/artifacts/ART-P81EE?project=orsl') {
      events.push('artifact');
      return jsonResponse(artifact());
    }
    throw new Error(`Unexpected fetch: ${path}`);
  });
  vi.stubGlobal('fetch', fetchMock);
  return fetchMock;
}

function RepresentativeArtifactRoute() {
  const artifactResource = useResource('representative-artifact-route', () =>
    fetchArtifact('ART-P81EE', 'orsl'),
  );
  if (artifactResource.error) return <div role="alert">route error</div>;
  if (!artifactResource.data) return <div>Loading protected artifact</div>;
  return <div>Protected artifact loaded</div>;
}


function selectMemberTab() {
  const tab = screen.getByRole('tab', { name: 'Member token' });
  fireEvent.pointerDown(tab, { button: 0, ctrlKey: false });
  fireEvent.mouseDown(tab, { button: 0, ctrlKey: false });
  fireEvent.pointerUp(tab, { button: 0, ctrlKey: false });
  fireEvent.mouseUp(tab, { button: 0, ctrlKey: false });
  fireEvent.click(tab);
}

function renderShell() {
  routeMocks.renderOutlet.mockImplementation(() => <RepresentativeArtifactRoute />);
  return render(
    <BackendProfileProvider>
      <MeProvider>
        <ThemeProvider>
          <RefreshProvider>
            <AppShell />
          </RefreshProvider>
        </ThemeProvider>
      </MeProvider>
    </BackendProfileProvider>,
  );
}

beforeEach(() => {
  events.length = 0;
  pendingMe = null;
  routeMocks.pathname = '/projects/orsl/artifacts/ART-P81EE';
  routeMocks.navigate.mockReset();
  routeMocks.renderOutlet.mockReset();
  window.localStorage.clear();
  setAuthMode('bearer');
  vi.stubGlobal(
    'matchMedia',
    vi.fn((query: string) => ({
      matches: query.includes('prefers-color-scheme: light'),
      media: query,
      onchange: null,
      addListener: vi.fn(),
      removeListener: vi.fn(),
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
      dispatchEvent: vi.fn(),
    })),
  );
});

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

describe('AppShell protected-route auth gate', () => {
  it('keeps the deep-linked route fetch unmounted until a first member login finishes, then loads without reload', async () => {
    pendingMe = defer<Response>();
    installFetch();

    renderShell();

    expect(screen.getByText('Connect to daemon')).toBeInTheDocument();
    expect(events).not.toContain('artifact');

    selectMemberTab();
    fireEvent.change(screen.getByPlaceholderText('Member token'), { target: { value: 'valid-member-token' } });
    fireEvent.click(screen.getByRole('button', { name: 'Sign in' }));

    await waitFor(() => expect(events).toContain('me'));
    expect(events).toEqual(expect.arrayContaining(['login', 'me']));
    expect(events).not.toContain('artifact');

    await act(async () => {
      pendingMe?.resolve(jsonResponse(memberMe()));
      await pendingMe?.promise;
    });

    await waitFor(() => expect(events).toContain('artifact'));
    expect(events.indexOf('artifact')).toBeGreaterThan(events.indexOf('me'));
    expect(await screen.findByText('Protected artifact loaded')).toBeInTheDocument();
    expect(screen.queryByText('Connect to daemon')).toBeNull();
  });

  it('preserves first-time admin bearer login before mounting the protected route', async () => {
    installFetch();

    renderShell();

    expect(screen.getByText('Connect to daemon')).toBeInTheDocument();
    expect(events).not.toContain('artifact');

    fireEvent.change(screen.getByLabelText('Bearer token'), { target: { value: 'admin-token' } });
    fireEvent.click(screen.getByRole('button', { name: 'Connect' }));

    await waitFor(() => expect(events).toContain('artifact'));
    expect(events.indexOf('artifact')).toBeGreaterThan(events.indexOf('admin-status'));
    expect(await screen.findByText('Protected artifact loaded')).toBeInTheDocument();
  });

  it('preserves restored member-cookie sessions by mounting the protected route immediately', async () => {
    window.localStorage.setItem('orgasmic.member.session', '1');
    window.localStorage.setItem('orgasmic.member.me', JSON.stringify(memberMe()));
    const fetchMock = installFetch();

    renderShell();

    await waitFor(() => expect(events).toContain('artifact'));
    expect(screen.queryByText('Connect to daemon')).toBeNull();
    expect(await screen.findByText('Protected artifact loaded')).toBeInTheDocument();

    const artifactCall = fetchMock.mock.calls.find(([input]) =>
      pathOf(input as RequestInfo | URL).startsWith('/api/artifacts/ART-P81EE'),
    );
    expect(artifactCall?.[1]?.credentials).toBe('include');
  });

  it('keeps a failed member token on the gate and never mounts the protected route', async () => {
    installFetch((path) => {
      if (path === '/api/login') {
        events.push('login');
        return textResponse('invalid token', 401);
      }
      return undefined;
    });

    renderShell();

    selectMemberTab();
    fireEvent.change(screen.getByPlaceholderText('Member token'), { target: { value: 'expired-member-token' } });
    fireEvent.click(screen.getByRole('button', { name: 'Sign in' }));

    expect(await screen.findByRole('alert')).toHaveTextContent('Invalid or expired member token.');
    expect(events).not.toContain('artifact');
    expect(screen.getByText('Connect to daemon')).toBeInTheDocument();
  });
});

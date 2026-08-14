// @vitest-environment jsdom
import '@testing-library/jest-dom/vitest';
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

const mocks = vi.hoisted(() => ({
  fetchDaemonStatus: vi.fn(),
  fetchRecoveryStatus: vi.fn(),
  fetchWhoami: vi.fn(),
  fetchTx: vi.fn(),
  fetchTxWithCoverage: vi.fn(),
  fetchParseErrorsWithCoverage: vi.fn(),
  loadFullParseErrorCoverage: vi.fn(),
}));

vi.mock('@/lib/api', async (importOriginal) => ({
  ...(await importOriginal<typeof import('@/lib/api')>()),
  ...mocks,
}));

vi.mock('@/hooks/useRefreshBus', () => ({
  useRefreshToken: () => 0,
}));

vi.mock('@/hooks/use-mobile', () => ({
  useIsMobile: () => false,
}));

vi.mock('@/hooks/useEventStream', () => ({
  useEventStream: () => undefined,
}));

vi.mock('sonner', () => ({
  toast: { success: vi.fn(), warning: vi.fn(), error: vi.fn() },
}));

import { StatusView } from '../StatusView';
import { NotificationBell } from '../notifications/NotificationBell';

const partial = {
  errors: [],
  coverage: {
    state: 'partial' as const,
    detail: 'partial; ready=1/2; markers=0/2; marker_unloaded=[one,two]',
    failures: {},
  },
};

const complete = {
  errors: [],
  coverage: {
    state: 'complete' as const,
    detail: 'complete; ready=2/2; markers=2/2',
    failures: {},
  },
};

beforeEach(() => {
  vi.clearAllMocks();
  window.localStorage.clear();
  mocks.fetchDaemonStatus.mockResolvedValue({
    name: 'orgasmic',
    version: 'test',
    boot_id: 'boot-test',
    pid: 1,
    started_at: '2026-08-11T00:00:00Z',
    home: '/tmp/home',
    projects: 1,
    parse_errors: 0,
    tx_count: 0,
  });
  mocks.fetchRecoveryStatus.mockResolvedValue({
    boot_id: 'boot-test',
    acquisition_paused: false,
    live_runs: [],
    interrupted_runs: [],
    reattached_runs: [],
    terminal_noop_runs: [],
    ambiguous_runs: [],
    note: '',
  });
  mocks.fetchWhoami.mockResolvedValue({ authenticated: true, boot_id: 'boot-test' });
  mocks.fetchTx.mockResolvedValue([]);
  mocks.fetchTxWithCoverage.mockResolvedValue({
    records: [],
    coverage: { state: 'complete', detail: 'complete; loaded=[]; failed=[]', failures: {} },
  });
  mocks.fetchParseErrorsWithCoverage.mockResolvedValue(partial);
  mocks.loadFullParseErrorCoverage.mockResolvedValue(complete);
});

afterEach(cleanup);

describe('parse-error coverage', () => {
  it('shows coordinator delay separately from failed project paths', async () => {
    mocks.fetchDaemonStatus.mockResolvedValue({
      name: 'orgasmic',
      version: 'test',
      boot_id: 'boot-test',
      pid: 1,
      started_at: '2026-08-11T00:00:00Z',
      home: '/tmp/home',
      projects: 0,
      registered_projects: 1,
      delayed_projects: { queued: 'coordinator wait timed out behind slow scans' },
      failed_projects: {},
      parse_errors: 0,
      tx_count: 0,
    });
    mocks.fetchParseErrorsWithCoverage.mockResolvedValue(complete);

    render(<StatusView />);
    expect(await screen.findByText('Delayed')).toBeInTheDocument();
    expect(screen.getByText('queued: coordinator wait timed out behind slow scans')).toBeInTheDocument();
    expect(screen.queryByText('Failed')).not.toBeInTheDocument();
  });

  it('does not present a partial zero as complete and offers an explicit full load', async () => {
    mocks.fetchParseErrorsWithCoverage
      .mockResolvedValueOnce(partial)
      .mockResolvedValue(complete);
    render(<StatusView />);

    expect(await screen.findByText('Parse-error coverage is partial')).toBeInTheDocument();
    expect(screen.getByText('0 partial')).toBeInTheDocument();

    fireEvent.click(screen.getByRole('button', { name: 'Load full coverage' }));
    await waitFor(() => expect(mocks.loadFullParseErrorCoverage).toHaveBeenCalledTimes(1));
    await waitFor(() => expect(mocks.fetchParseErrorsWithCoverage).toHaveBeenCalledTimes(2));
  });

  it('keeps partial parse-error coverage out of notifications and the unread count', async () => {
    render(
      <NotificationBell
        projectId={null}
        onNavigate={vi.fn()}
        onOpenTask={vi.fn()}
      />,
    );

    const bell = await screen.findByRole('button', { name: 'Notifications' });
    expect(mocks.loadFullParseErrorCoverage).not.toHaveBeenCalled();
    fireEvent.click(bell);

    expect(await screen.findByText('No parse errors.')).toBeInTheDocument();
    expect(screen.queryByText('Parse-error coverage is partial')).not.toBeInTheDocument();
    expect(screen.queryByRole('button', { name: 'Load full coverage' })).not.toBeInTheDocument();
    expect(mocks.loadFullParseErrorCoverage).not.toHaveBeenCalled();
  });

  it('names projects skipped by an explicit partial coverage load', async () => {
    mocks.loadFullParseErrorCoverage.mockResolvedValue({
      errors: [],
      coverage: {
        state: 'partial',
        detail: 'partial; marker_unloaded=[blocked]',
        failures: { blocked: 'filesystem scan timed out' },
      },
    });
    render(<StatusView />);

    fireEvent.click(await screen.findByRole('button', { name: 'Load full coverage' }));
    expect(await screen.findByText('Skipped projects: blocked')).toBeInTheDocument();
  });

  it('surfaces partial unscoped activity coverage in notifications', async () => {
    mocks.fetchTxWithCoverage.mockResolvedValue({
      records: [],
      coverage: {
        state: 'partial',
        detail: 'partial; loaded=[healthy]; failed=[blocked]',
        failures: {},
      },
    });
    render(
      <NotificationBell
        projectId={null}
        onNavigate={vi.fn()}
        onOpenTask={vi.fn()}
      />,
    );

    const bell = await screen.findByRole('button', { name: 'Notifications: 1 unread' });
    fireEvent.click(bell);
    expect(await screen.findByText('Activity coverage is partial')).toBeInTheDocument();
    expect(screen.getByText('Failed project ledgers: blocked.')).toBeInTheDocument();
  });

  it('renders coordinator delay separately without a dismiss action', async () => {
    mocks.fetchParseErrorsWithCoverage.mockResolvedValue(complete);
    mocks.fetchTxWithCoverage.mockResolvedValue({
      records: [],
      coverage: {
        state: 'partial',
        detail: 'partial; loaded=[]; delayed=[queued%20project]; failed=[]',
        failures: {},
      },
    });

    render(<NotificationBell projectId={null} onNavigate={vi.fn()} onOpenTask={vi.fn()} />);

    fireEvent.click(await screen.findByRole('button', { name: 'Notifications: 1 unread' }));
    expect(await screen.findByText('Activity loading is delayed')).toBeInTheDocument();
    expect(screen.getByText(/queued project/)).toBeInTheDocument();
    expect(screen.queryByRole('button', { name: 'Dismiss' })).not.toBeInTheDocument();
    expect(screen.queryByText('Activity coverage is partial')).not.toBeInTheDocument();
  });

  it('dismisses partial activity coverage for one failed-project set and reopens for a changed set', async () => {
    mocks.fetchParseErrorsWithCoverage.mockResolvedValue(complete);
    mocks.fetchTxWithCoverage.mockResolvedValue({
      records: [],
      coverage: {
        state: 'partial',
        detail: 'partial; loaded=[healthy]; failed=[blocked,offline]',
        failures: {},
      },
    });

    const first = render(
      <NotificationBell projectId={null} onNavigate={vi.fn()} onOpenTask={vi.fn()} />,
    );
    fireEvent.click(await screen.findByRole('button', { name: 'Notifications: 1 unread' }));
    fireEvent.click(await screen.findByRole('button', { name: 'Dismiss' }));
    await waitFor(() => {
      expect(screen.getByRole('button', { name: 'Notifications' })).toBeInTheDocument();
    });
    first.unmount();

    mocks.fetchTxWithCoverage.mockResolvedValue({
      records: [],
      coverage: {
        state: 'partial',
        detail: 'partial; loaded=[healthy]; failed=[offline,blocked]',
        failures: {},
      },
    });
    const same = render(
      <NotificationBell projectId={null} onNavigate={vi.fn()} onOpenTask={vi.fn()} />,
    );
    expect(await screen.findByRole('button', { name: 'Notifications' })).toBeInTheDocument();
    expect(screen.queryByText('Activity coverage is partial')).not.toBeInTheDocument();
    same.unmount();

    mocks.fetchTxWithCoverage.mockResolvedValue({
      records: [],
      coverage: {
        state: 'partial',
        detail: 'partial; loaded=[healthy]; delayed=[queued]; failed=[offline,blocked]',
        failures: {},
      },
    });
    const delayed = render(
      <NotificationBell projectId={null} onNavigate={vi.fn()} onOpenTask={vi.fn()} />,
    );
    fireEvent.click(await screen.findByRole('button', { name: 'Notifications: 1 unread' }));
    expect(await screen.findByText('Activity loading is delayed')).toBeInTheDocument();
    expect(screen.queryByText('Activity coverage is partial')).not.toBeInTheDocument();
    delayed.unmount();

    mocks.fetchTxWithCoverage.mockResolvedValue({
      records: [],
      coverage: {
        state: 'partial',
        detail: 'partial; loaded=[healthy]; delayed=[queued]; failed=[blocked,replacement]',
        failures: {},
      },
    });
    render(<NotificationBell projectId={null} onNavigate={vi.fn()} onOpenTask={vi.fn()} />);
    const changed = await screen.findByRole('button', { name: 'Notifications: 2 unread' });
    fireEvent.click(changed);
    expect(await screen.findByText('Activity coverage is partial')).toBeInTheDocument();
    expect(screen.getByText('Activity loading is delayed')).toBeInTheDocument();
  });
});

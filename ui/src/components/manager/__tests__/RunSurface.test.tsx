// @vitest-environment jsdom
import '@testing-library/jest-dom/vitest';
import { cleanup, render, screen, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, expect, it, vi } from 'vitest';

import type { RunSummary } from '@/lib/types';

const mocks = vi.hoisted(() => ({ member: true, fetchRunRuntimeOptions: vi.fn() }));
vi.mock('@/hooks/useMe', () => ({ useMe: () => ({ isMember: mocks.member, can: () => true }) }));
vi.mock('@/hooks/useTranscriptStream', () => ({
  useTranscriptStream: () => ({ parts: [], responseAt: 0, loading: false, error: null }),
}));
vi.mock('@/lib/api', () => ({
  fetchRunRuntimeOptions: mocks.fetchRunRuntimeOptions,
  postRunRuntimeOptions: vi.fn(),
  postRunInput: vi.fn(),
  fetchSkills: vi.fn().mockResolvedValue([]),
}));

import { RunSurface } from '../RunSurface';

const run: RunSummary = {
  run_id: 'run-live', task_id: 'CONV-1', kind: 'chat', driver: 'stdio', harness: 'claude-sdk',
  project_id: 'demo', sub_state: null, identity: { run_id: 'run-live', runtime_id: 'rt', boot_id: 'boot' },
  session_path: '/sessions/run-live.jsonl', event_count: 0,
};

beforeEach(() => {
  // The transcript's stick-to-bottom hook observes its container; jsdom has no ResizeObserver.
  vi.stubGlobal('ResizeObserver', class { observe() {} unobserve() {} disconnect() {} });
});
afterEach(() => { cleanup(); vi.clearAllMocks(); vi.unstubAllGlobals(); mocks.member = true; });

it("skips the admin-only runtime options bar on a member's live conversation but keeps the composer", async () => {
  mocks.fetchRunRuntimeOptions.mockResolvedValue({ catalog: { current: {}, models: [], efforts: [], speeds: [], providers: [] } });
  const view = render(<RunSurface run={run} onPromptSent={() => {}} conversation={{ onSend: async () => true }} />);
  expect(screen.getByPlaceholderText('Send to agent')).toBeInTheDocument();
  expect(mocks.fetchRunRuntimeOptions).not.toHaveBeenCalled();

  mocks.member = false;
  view.rerender(<RunSurface run={run} onPromptSent={() => {}} conversation={{ onSend: async () => true }} />);
  await waitFor(() => expect(mocks.fetchRunRuntimeOptions).toHaveBeenCalledWith('run-live'));
});

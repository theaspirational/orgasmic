// @vitest-environment jsdom
import '@testing-library/jest-dom/vitest';
import { cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { RunDockProvider, useRunDock } from '@/lib/runDock';
import type { RunSummary } from '@/lib/types';

const mocks = vi.hoisted(() => ({
  fetchConversations: vi.fn(),
  fetchConversation: vi.fn(),
  fetchNodeLinks: vi.fn(),
  fetchRun: vi.fn(),
  findNodeConversation: vi.fn(),
  postConversationCreate: vi.fn(),
  postConversationInput: vi.fn(),
  grants: {} as Record<string, boolean>,
}));

vi.mock('@/lib/api', async () => ({
  ...(await vi.importActual<typeof import('@/lib/api')>('@/lib/api')),
  fetchConversations: mocks.fetchConversations,
  fetchConversation: mocks.fetchConversation,
  fetchNodeLinks: mocks.fetchNodeLinks,
  fetchRun: mocks.fetchRun,
  findNodeConversation: mocks.findNodeConversation,
  postConversationCreate: mocks.postConversationCreate,
  postConversationInput: mocks.postConversationInput,
  fetchManagerChatCatalog: vi.fn().mockResolvedValue({
    providers: [{ id: 'codex', source: 'test', models: [], message: null }],
  }),
  fetchSkills: vi.fn().mockResolvedValue([]),
}));
vi.mock('@/hooks/useEventStream', () => ({ useEventStream: () => undefined }));
vi.mock('@/hooks/useTranscriptStream', () => ({
  useTranscriptStream: () => ({ parts: [], responseAt: 0, loading: false, error: null }),
}));
vi.mock('@/hooks/useMe', () => ({
  useMe: () => ({
    identity: 'admin',
    me: null,
    isMember: false,
    can: (_project: string, action: string) => mocks.grants[action] !== false,
  }),
}));

import { CHAT_EXECUTE_LABEL, ConversationPanel } from '../ConversationPanel';

function conversationDoc(id: string, runs: string, extra: Record<string, string> = {}) {
  return {
    id, kind: 'conversations', title: `Chat ${id}`, todo: 'OPEN', tags: [], body: '', sections: [],
    properties: Object.entries({ PURPOSE: 'discuss', OWNER: 'admin', RUNS: runs, ...extra }).map(([key, value]) => ({ key, value })),
    source: { file: `conversations/${id}/node.org`, base_version: 'v1' },
  };
}

function liveRun(conversationId: string): RunSummary {
  return {
    run_id: 'run-live', task_id: conversationId, kind: 'chat', driver: 'stdio', harness: 'claude-sdk',
    project_id: 'demo', sub_state: null, identity: { run_id: 'run-live', runtime_id: 'rt', boot_id: 'boot' },
    session_path: '/sessions/run-live.jsonl', event_count: 0,
  };
}

function Probe({ node }: { node?: string }) {
  const { openChat } = useRunDock();
  return <button onClick={() => openChat(node ? { node } : {})}>Probe open</button>;
}

function panel(liveRuns: RunSummary[] = [], node?: string) {
  return (
    <RunDockProvider>
      <Probe node={node} />
      <ConversationPanel projectId="demo" readOnly={false} liveRuns={liveRuns} onRefresh={() => {}} />
    </RunDockProvider>
  );
}

beforeEach(() => {
  vi.clearAllMocks();
  mocks.grants = {};
  window.localStorage.clear();
  mocks.fetchConversations.mockResolvedValue([
    { id: 'CONV-ARCH', title: 'Old archived', todo: 'ARCHIVED', layer: 'conversations', outgoing: [], source_file: '' },
    { id: 'CONV-LIVE', title: 'Live one', todo: 'OPEN', layer: 'conversations', outgoing: [], source_file: '' },
    { id: 'CONV-IDLE', title: 'Idle one', todo: 'OPEN', layer: 'conversations', outgoing: [], source_file: '' },
  ]);
  mocks.fetchConversation.mockImplementation(async (id: string) => conversationDoc(id, 'run-a run-b:resumed run-c:cold'));
  mocks.fetchNodeLinks.mockResolvedValue([]);
  mocks.fetchRun.mockResolvedValue({ source: '', run: {} });
});
afterEach(cleanup);

describe('ConversationPanel', () => {
  it('lists open conversations first, marks the live one, and offers New chat', async () => {
    render(panel([liveRun('CONV-LIVE')]));
    const nav = await screen.findByRole('navigation', { name: 'Conversations' });
    await within(nav).findByText('Idle one');
    const labels = within(nav).getAllByRole('button').map((button) => button.textContent);
    expect(labels).toEqual(['New chat', 'Live one', 'Idle one', 'Old archived']);
    expect(within(within(nav).getByRole('button', { name: /Live one/ })).getByLabelText('Live')).toBeInTheDocument();
    expect(within(within(nav).getByRole('button', { name: /Idle one/ })).queryByLabelText('Live')).toBeNull();
  });

  it('shows the selected conversation as its run segments with a continue composer', async () => {
    render(panel());
    fireEvent.click(await screen.findByRole('button', { name: /Idle one/ }));
    const segments = await screen.findAllByTestId('conversation-segment');
    expect(segments.map((segment) => segment.textContent)).toEqual([
      expect.stringContaining('Startedrun-a'),
      expect.stringContaining('Resumedrun-b'),
      expect.stringContaining('Continued without native memoryrun-c'),
    ]);
    // Older runs are collapsed and not fetched until opened; the current one loads.
    await waitFor(() => expect(mocks.fetchRun).toHaveBeenCalledWith('run-c'));
    expect(mocks.fetchRun).not.toHaveBeenCalledWith('run-a');
    // Sending continues through the conversation route and switches to the returned run.
    mocks.postConversationInput.mockResolvedValue({ run_id: 'run-d', mode: 'cold' });
    const composer = await screen.findByPlaceholderText('Send to agent');
    await waitFor(() => expect(composer).toBeEnabled());
    fireEvent.change(composer, { target: { value: 'continue please' } });
    fireEvent.click(screen.getByRole('button', { name: 'Send message' }));
    await waitFor(() => expect(mocks.postConversationInput).toHaveBeenCalledWith('CONV-IDLE', 'demo', { message: 'continue please', context: undefined }));
    await waitFor(() => expect(mocks.fetchRun).toHaveBeenCalledWith('run-d'));
  });

  it('pins the scoped node as a chip and sends it as context', async () => {
    mocks.fetchNodeLinks.mockResolvedValue([{ id: 'l', source: 'CONV-IDLE', target: 'dec_1', kind: 'RELATES_TO', revision: 0, deleted: false, anchors: [] }]);
    mocks.postConversationInput.mockResolvedValue({ run_id: 'run-c', mode: 'live' });
    render(panel());
    fireEvent.click(await screen.findByRole('button', { name: /Idle one/ }));
    expect(await screen.findByText('dec_1')).toBeInTheDocument();
    const composer = await screen.findByPlaceholderText('Send to agent');
    await waitFor(() => expect(composer).toBeEnabled());
    fireEvent.change(composer, { target: { value: 'hi' } });
    fireEvent.click(screen.getByRole('button', { name: 'Send message' }));
    await waitFor(() => expect(mocks.postConversationInput).toHaveBeenCalledWith('CONV-IDLE', 'demo', { message: 'hi', context: [{ kind: 'node', id: 'dec_1' }] }));
  });

  it('opens the newest OPEN conversation about a node, or a scoped setup when there is none', async () => {
    mocks.findNodeConversation.mockResolvedValueOnce('CONV-LIVE');
    const view = render(panel([], 'dec_1'));
    fireEvent.click(screen.getByRole('button', { name: 'Probe open' }));
    await waitFor(() => expect(mocks.findNodeConversation).toHaveBeenCalledWith('demo', 'dec_1'));
    const nav = await screen.findByRole('navigation', { name: 'Conversations' });
    await waitFor(() => expect(within(nav).getByRole('button', { name: /Live one/ })).toHaveAttribute('aria-current', 'true'));
    view.unmount();

    mocks.findNodeConversation.mockResolvedValueOnce(null);
    mocks.postConversationCreate.mockResolvedValue({ id: 'CONV-NEW', run_id: 'run-new', mode: 'cold' });
    render(panel([], 'dec_1'));
    fireEvent.click(screen.getByRole('button', { name: 'Probe open' }));
    expect(await screen.findByText('About dec_1')).toBeInTheDocument();
    const composer = await screen.findByPlaceholderText('Ask about dec_1');
    await waitFor(() => expect(composer).toBeEnabled());
    fireEvent.change(composer, { target: { value: 'first message' } });
    fireEvent.click(screen.getByRole('button', { name: 'Send message' }));
    await waitFor(() => expect(mocks.postConversationCreate).toHaveBeenCalledWith('demo', expect.objectContaining({ purpose: 'discuss', node: 'dec_1', provider: 'codex', message: 'first message' })));
    expect(window.localStorage.getItem('orgasmic.rundock.conversation.v1')).toBe('CONV-NEW');
  });

  it('disables the composer with the host-agent notice when chat.execute is missing', async () => {
    mocks.grants = { 'chat.execute': false };
    render(panel());
    expect((await screen.findAllByText(CHAT_EXECUTE_LABEL)).length).toBeGreaterThan(0);
    fireEvent.click(await screen.findByRole('button', { name: /Idle one/ }));
    await screen.findAllByTestId('conversation-segment');
    expect(screen.getByPlaceholderText('Send to agent')).toBeDisabled();
    expect(screen.getAllByText(CHAT_EXECUTE_LABEL).length).toBeGreaterThan(0);
  });

  it('hides New chat and the composer without chat.write', async () => {
    mocks.grants = { 'chat.write': false };
    render(panel());
    const nav = await screen.findByRole('navigation', { name: 'Conversations' });
    await within(nav).findByText('Idle one');
    expect(within(nav).queryByRole('button', { name: 'New chat' })).toBeNull();
    fireEvent.click(within(nav).getByRole('button', { name: /Idle one/ }));
    await screen.findAllByTestId('conversation-segment');
    expect(screen.getByRole('status')).toHaveTextContent(/read-only/i);
  });
});

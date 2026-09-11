// @vitest-environment jsdom
import '@testing-library/jest-dom/vitest';
import { cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { RunDockProvider, useRunDock } from '@/lib/runDock';
import { HttpError } from '@/lib/transport';
import type { ConversationContextChip, RunSummary } from '@/lib/types';

const mocks = vi.hoisted(() => ({
  fetchGraphNodes: vi.fn(),
  fetchOrgNode: vi.fn(),
  fetchNodeLinks: vi.fn(),
  fetchRun: vi.fn(),
  findNodeConversation: vi.fn(),
  postConversationCreate: vi.fn(),
  postConversationInput: vi.fn(),
  grants: {} as Record<string, boolean>,
}));

vi.mock('@/lib/api', async () => ({
  ...(await vi.importActual<typeof import('@/lib/api')>('@/lib/api')),
  fetchGraphNodes: mocks.fetchGraphNodes,
  fetchOrgNode: mocks.fetchOrgNode,
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

import { CHAT_EXECUTE_LABEL, ConversationPanel, FinishedRunPanel, IN_FLIGHT_LABEL, NO_RESUME_LABEL } from '../ConversationPanel';

function conversationDoc(id: string, runs: string, extra: Record<string, string> = {}) {
  return {
    id, kind: 'conversations', title: `Chat ${id}`, todo: 'OPEN', tags: [], body: '', sections: [],
    properties: Object.entries({ PURPOSE: 'discuss', OWNER: 'admin', RUNS: runs, ...extra }).map(([key, value]) => ({ key, value })),
    source: { file: `conversations/${id}/node.org`, base_version: 'v1' },
  };
}

function liveRun(conversationId: string, extra: Partial<RunSummary> = {}): RunSummary {
  return {
    run_id: 'run-live', task_id: conversationId, kind: 'chat', driver: 'stdio', harness: 'claude-sdk',
    project_id: 'demo', sub_state: null, identity: { run_id: 'run-live', runtime_id: 'rt', boot_id: 'boot' },
    session_path: '/sessions/run-live.jsonl', event_count: 0, ...extra,
  };
}

function Probe({ node, context }: { node?: string; context?: ConversationContextChip[] }) {
  const { openChat } = useRunDock();
  return <button onClick={() => openChat(node ? { node, context } : {})}>Probe open</button>;
}

function panel(liveRuns: RunSummary[] = [], node?: string, context?: ConversationContextChip[]) {
  return (
    <RunDockProvider>
      <Probe node={node} context={context} />
      <ConversationPanel projectId="demo" liveRuns={liveRuns} onRefresh={() => {}} />
    </RunDockProvider>
  );
}

const RANGE: ConversationContextChip = { kind: 'range', node: 'dec_1', attachment: 'rec', revision: 'sha', start_ms: 90000, end_ms: 120000 };
const SELECTION: ConversationContextChip = { kind: 'selection', text: 'a'.repeat(50) };

beforeEach(() => {
  vi.clearAllMocks();
  mocks.grants = {};
  window.localStorage.clear();
  // A live RunSurface mounts the auto-scrolling transcript, which observes resizes.
  vi.stubGlobal('ResizeObserver', class { observe() {} unobserve() {} disconnect() {} });
  mocks.fetchGraphNodes.mockResolvedValue([
    { id: 'CONV-ARCH', title: 'Old archived', todo: 'ARCHIVED', layer: 'conversations', outgoing: [], source_file: '' },
    { id: 'CONV-LIVE', title: 'Live one', todo: 'OPEN', layer: 'conversations', outgoing: [], source_file: '' },
    { id: 'CONV-IDLE', title: 'Idle one', todo: 'OPEN', layer: 'conversations', outgoing: [], source_file: '' },
  ]);
  mocks.fetchOrgNode.mockImplementation(async (id: string) => conversationDoc(id, 'run-a run-b:resumed run-c:cold'));
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

  it('shows openChat context as removable chips, sends the rest after the scoped node, then clears them', async () => {
    mocks.findNodeConversation.mockResolvedValueOnce('CONV-IDLE');
    mocks.fetchNodeLinks.mockResolvedValue([{ id: 'l', source: 'CONV-IDLE', target: 'dec_1', kind: 'RELATES_TO', revision: 0, deleted: false, anchors: [] }]);
    mocks.postConversationInput.mockResolvedValue({ run_id: 'run-c', mode: 'live' });
    render(panel([], 'dec_1', [RANGE, SELECTION]));
    fireEvent.click(screen.getByRole('button', { name: 'Probe open' }));
    const selectionLabel = `${'a'.repeat(40)}…`;
    expect(await screen.findByText('1:30–2:00')).toBeInTheDocument();
    expect(screen.getByText(selectionLabel)).toBeInTheDocument();
    expect(await screen.findByText('dec_1')).toBeInTheDocument();
    // The pinned scoped chip cannot be removed; optional ones can.
    expect(screen.queryByRole('button', { name: 'Remove dec_1' })).toBeNull();
    fireEvent.click(screen.getByRole('button', { name: 'Remove 1:30–2:00' }));
    expect(screen.queryByText('1:30–2:00')).toBeNull();
    const composer = await screen.findByPlaceholderText('Send to agent');
    await waitFor(() => expect(composer).toBeEnabled());
    fireEvent.change(composer, { target: { value: 'about this' } });
    fireEvent.click(screen.getByRole('button', { name: 'Send message' }));
    await waitFor(() => expect(mocks.postConversationInput).toHaveBeenCalledWith('CONV-IDLE', 'demo', {
      message: 'about this', context: [{ kind: 'node', id: 'dec_1' }, SELECTION],
    }));
    await waitFor(() => expect(screen.queryByText(selectionLabel)).toBeNull());
    expect(screen.getByText('dec_1')).toBeInTheDocument();
  });

  it('carries openChat context into a scoped setup and sends it with the first message', async () => {
    mocks.findNodeConversation.mockResolvedValueOnce(null);
    mocks.postConversationCreate.mockResolvedValue({ id: 'CONV-NEW', run_id: 'run-new', mode: 'cold' });
    render(panel([], 'dec_1', [RANGE]));
    fireEvent.click(screen.getByRole('button', { name: 'Probe open' }));
    expect(await screen.findByText('1:30–2:00')).toBeInTheDocument();
    const composer = await screen.findByPlaceholderText('Ask about dec_1');
    await waitFor(() => expect(composer).toBeEnabled());
    fireEvent.change(composer, { target: { value: 'first' } });
    fireEvent.click(screen.getByRole('button', { name: 'Send message' }));
    await waitFor(() => expect(mocks.postConversationCreate).toHaveBeenCalledWith('demo', expect.objectContaining({ node: 'dec_1', message: 'first', context: [RANGE] })));
  });

  it('marks a conversation live through a dispatch run that records its conversation_id', async () => {
    render(panel([liveRun('TASK-1', { run_id: 'run-worker', harness: 'claude', conversation_id: 'CONV-LIVE' })]));
    const nav = await screen.findByRole('navigation', { name: 'Conversations' });
    await within(nav).findByText('Live one');
    expect(within(within(nav).getByRole('button', { name: /Live one/ })).getByLabelText('Live')).toBeInTheDocument();
    expect(within(within(nav).getByRole('button', { name: /Idle one/ })).queryByLabelText('Live')).toBeNull();
  });

  it('shows the purpose and the short worktree in the conversation header', async () => {
    mocks.fetchOrgNode.mockImplementation(async (id: string) =>
      conversationDoc(id, 'run-a', { PURPOSE: 'implement', WORKTREE: '/tmp/wt/sprint-TASK-1' }));
    render(panel());
    fireEvent.click(await screen.findByRole('button', { name: /Idle one/ }));
    const header = await screen.findByRole('banner');
    expect(within(header).getByText('implement')).toBeInTheDocument();
    expect(within(header).getByText('sprint-TASK-1')).toHaveAttribute('title', '/tmp/wt/sprint-TASK-1');
  });

  it('keeps the composer down after a 409 no_resume until a live run appears', async () => {
    mocks.postConversationInput.mockRejectedValue(
      new HttpError(409, JSON.stringify({ error: 'no native session to resume; dispatch a new attempt', code: 'no_resume' })),
    );
    const view = render(panel());
    fireEvent.click(await screen.findByRole('button', { name: /Idle one/ }));
    const composer = await screen.findByPlaceholderText('Send to agent');
    await waitFor(() => expect(composer).toBeEnabled());
    fireEvent.change(composer, { target: { value: 'continue' } });
    fireEvent.click(screen.getByRole('button', { name: 'Send message' }));
    await waitFor(() => expect(screen.getByPlaceholderText('Send to agent')).toBeDisabled());
    expect(screen.getAllByText(NO_RESUME_LABEL).length).toBeGreaterThan(0);

    view.rerender(panel([liveRun('TASK-9', { run_id: 'run-attempt', harness: 'claude', conversation_id: 'CONV-IDLE' })]));
    await waitFor(() => expect(screen.getByPlaceholderText('Send to agent')).toBeEnabled());
    expect(screen.queryByText(NO_RESUME_LABEL)).toBeNull();
  });

  it('shows a retry notice for an in-flight 409 and the daemon text for a 400, leaving the composer enabled', async () => {
    mocks.postConversationInput
      .mockRejectedValueOnce(new HttpError(409, JSON.stringify({ error: 'CONV-IDLE launch already in flight' })))
      .mockRejectedValueOnce(new HttpError(400, JSON.stringify({ error: 'the artifactor is not running; use Regenerate to start a new round' })));
    render(panel());
    fireEvent.click(await screen.findByRole('button', { name: /Idle one/ }));
    const composer = await screen.findByPlaceholderText('Send to agent');
    await waitFor(() => expect(composer).toBeEnabled());
    fireEvent.change(composer, { target: { value: 'again' } });
    fireEvent.click(screen.getByRole('button', { name: 'Send message' }));
    expect(await screen.findByText(IN_FLIGHT_LABEL)).toBeInTheDocument();
    await waitFor(() => expect(composer).toBeEnabled());
    expect(composer).toHaveValue('again');

    fireEvent.click(screen.getByRole('button', { name: 'Send message' }));
    expect(await screen.findByText('the artifactor is not running; use Regenerate to start a new round')).toBeInTheDocument();
    await waitFor(() => expect(composer).toBeEnabled());
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

describe('FinishedRunPanel', () => {
  const runMeta = (conversationId?: string) =>
    JSON.stringify({ seq: 1, kind: 'lifecycle', event: { phase: 'run_meta', transport: 'stdio', driver_config: {}, ...(conversationId ? { conversation_id: conversationId } : null) } });
  const send = JSON.stringify({ seq: 2, kind: 'lifecycle', event: { phase: 'composer_send', text: 'Continue please' } });

  it('hands a finished run over to the conversation its session names', async () => {
    mocks.fetchRun.mockResolvedValue({ source: `${runMeta('CONV-DONE')}\n${send}`, run: {} });
    const onConversation = vi.fn();
    render(<FinishedRunPanel runId="run-done" onRefresh={() => {}} onClose={() => {}} onConversation={onConversation} />);
    await waitFor(() => expect(onConversation).toHaveBeenCalledWith('CONV-DONE'));
    expect(screen.queryByTestId('finished-run-panel')).toBeNull();
  });

  it('keeps the transcript readable when no conversation owns the run', async () => {
    mocks.fetchRun.mockResolvedValue({ source: `${runMeta()}\n${send}`, run: {} });
    const onConversation = vi.fn();
    render(<FinishedRunPanel runId="run-lone" onRefresh={() => {}} onClose={() => {}} onConversation={onConversation} />);
    expect(await screen.findByText('Continue please')).toBeInTheDocument();
    expect(screen.getByText(/no longer live/)).toBeInTheDocument();
    expect(onConversation).not.toHaveBeenCalled();
  });
});

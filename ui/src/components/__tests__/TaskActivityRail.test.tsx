// @vitest-environment jsdom
import '@testing-library/jest-dom/vitest';
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import type { ReactNode } from 'react';

const mocks = vi.hoisted(() => ({
  postTaskComment: vi.fn(),
  editTaskComment: vi.fn(),
  deleteTaskComment: vi.fn(),
  toastError: vi.fn(),
}));

vi.mock('@/lib/api', () => ({
  fetchProject: vi.fn(),
  fetchTask: vi.fn(),
  fetchTaskActivity: vi.fn(),
  postTaskComment: (...args: unknown[]) => mocks.postTaskComment(...args),
  editTaskComment: (...args: unknown[]) => mocks.editTaskComment(...args),
  deleteTaskComment: (...args: unknown[]) => mocks.deleteTaskComment(...args),
}));

vi.mock('sonner', () => ({ toast: { error: mocks.toastError } }));

vi.mock('@/components/ui/scroll-area', () => ({
  ScrollArea: ({ children }: { children: ReactNode }) => <div>{children}</div>,
}));

import { TaskActivityRail, taskActivityPresentation, threadActivity } from '../TaskDialog';
import type { ActivityEntry } from '@/lib/types';

function entry(overrides: Partial<ActivityEntry> = {}): ActivityEntry {
  return {
    tx_id: 'tx-1',
    time: '[2026-08-13 Thu 14:52:15]',
    kind: 'state_transition',
    actor: 'aspirational',
    body: 'transition TASK-MYKGD to in_progress',
    artifacts: [],
    ...overrides,
  };
}

describe('task activity presentation', () => {
  it('attributes human comments to the person and technical events to their real source', () => {
    expect(taskActivityPresentation(entry({ kind: 'comment', actor: 'Nadia', body: 'Looks good.' })))
      .toMatchObject({ source: 'human', sourceLabel: 'Nadia', eventLabel: 'Team comment' });
    expect(taskActivityPresentation(entry())).toEqual({
      source: 'daemon',
      sourceLabel: 'Orgasmic daemon',
      eventLabel: 'Status change',
      body: 'TASK-MYKGD moved to In progress',
    });
    expect(taskActivityPresentation(entry({ kind: 'comment', actor: 'agent.reviewer' })))
      .toMatchObject({ source: 'agent', sourceLabel: 'Reviewer agent', eventLabel: 'Agent update' });
  });
});

describe('TaskActivityRail comments', () => {
  beforeEach(() => {
    mocks.postTaskComment.mockReset();
    mocks.editTaskComment.mockReset();
    mocks.deleteTaskComment.mockReset();
    mocks.toastError.mockReset();
  });

  afterEach(() => cleanup());

  it('shows automated events distinctly and posts a trimmed team comment', async () => {
    mocks.postTaskComment.mockResolvedValueOnce({ tx_id: 'tx-comment' });
    const onChanged = vi.fn();
    render(
      <TaskActivityRail
        projectId="vscode-orsl"
        taskId="TASK-MYKGD"
        entries={[entry()]}
        loading={false}
        canComment
        onChanged={onChanged}
        embedded
      />,
    );

    expect(screen.getByText('Orgasmic daemon')).toBeInTheDocument();
    expect(screen.getByText('Status change')).toBeInTheDocument();
    expect(screen.queryByText('aspirational')).not.toBeInTheDocument();

    fireEvent.change(screen.getByLabelText('Add a comment'), {
      target: { value: '  Please verify this edge case.  ' },
    });
    fireEvent.click(screen.getByRole('button', { name: 'Comment' }));

    await waitFor(() =>
      expect(mocks.postTaskComment).toHaveBeenCalledWith(
        'vscode-orsl',
        'TASK-MYKGD',
        { body: 'Please verify this edge case.' },
      ),
    );
    expect(onChanged).toHaveBeenCalledOnce();
    expect(screen.getByLabelText('Add a comment')).toHaveValue('');
  });

  it('omits the composer without the task comment capability', () => {
    render(
      <TaskActivityRail
        projectId="vscode-orsl"
        taskId="TASK-MYKGD"
        entries={[]}
        loading={false}
        canComment={false}
        onChanged={() => {}}
        embedded
      />,
    );

    expect(screen.queryByRole('form', { name: 'Add task comment' })).not.toBeInTheDocument();
    expect(screen.getByText('No activity yet.')).toBeInTheDocument();
  });

  it('keeps the draft and reports the server error when posting fails', async () => {
    mocks.postTaskComment.mockRejectedValueOnce(new Error('Comment write failed'));
    render(
      <TaskActivityRail
        projectId="vscode-orsl"
        taskId="TASK-MYKGD"
        entries={[]}
        loading={false}
        canComment
        onChanged={() => {}}
        embedded
      />,
    );

    const composer = screen.getByLabelText('Add a comment');
    fireEvent.change(composer, { target: { value: 'Do not lose this draft' } });
    fireEvent.click(screen.getByRole('button', { name: 'Comment' }));

    await waitFor(() => expect(mocks.toastError).toHaveBeenCalledWith('Comment write failed'));
    expect(composer).toHaveValue('Do not lose this draft');
    expect(screen.getByRole('button', { name: 'Comment' })).toBeEnabled();
  });

  it('threads replies and exposes reply, edit, and delete mutations', async () => {
    mocks.postTaskComment.mockResolvedValue({ tx_id: 'reply' });
    mocks.editTaskComment.mockResolvedValue({ entry_id: 'tx-root', action: 'edited' });
    mocks.deleteTaskComment.mockResolvedValue({ entry_id: 'tx-root', action: 'deleted' });
    const root = entry({ tx_id: 'tx-root', kind: 'comment', actor: 'Nadia', body: 'Root' });
    const child = entry({
      tx_id: 'tx-child',
      kind: 'comment',
      actor: 'Bo',
      body: 'Child',
      in_reply_to: 'tx-root',
    });
    expect(threadActivity([child, root]).map(({ entry, depth }) => [entry.tx_id, depth])).toEqual([
      ['tx-root', 0],
      ['tx-child', 1],
    ]);

    render(
      <TaskActivityRail
        projectId="orgasmic"
        taskId="TASK-1"
        entries={[root]}
        loading={false}
        canComment
        onChanged={vi.fn()}
        embedded
      />,
    );

    fireEvent.click(screen.getByRole('button', { name: 'Reply' }));
    fireEvent.change(screen.getByPlaceholderText('Write a reply…'), {
      target: { value: '  Nested reply  ' },
    });
    fireEvent.click(screen.getAllByRole('button', { name: 'Reply' }).at(-1)!);
    await waitFor(() =>
      expect(mocks.postTaskComment).toHaveBeenCalledWith('orgasmic', 'TASK-1', {
        body: 'Nested reply',
        in_reply_to: 'tx-root',
      }),
    );

    fireEvent.click(screen.getByRole('button', { name: 'Edit' }));
    fireEvent.change(screen.getByLabelText('Edit comment'), { target: { value: '  Revised  ' } });
    fireEvent.click(screen.getByRole('button', { name: 'Save' }));
    await waitFor(() =>
      expect(mocks.editTaskComment).toHaveBeenCalledWith(
        'orgasmic',
        'TASK-1',
        'tx-root',
        'Root',
        'Revised',
      ),
    );

    fireEvent.click(screen.getByRole('button', { name: 'Delete' }));
    fireEvent.click(await screen.findByRole('button', { name: 'Delete' }));
    await waitFor(() =>
      expect(mocks.deleteTaskComment).toHaveBeenCalledWith(
        'orgasmic',
        'TASK-1',
        'tx-root',
        'Root',
      ),
    );
  });
});

// @vitest-environment jsdom
import '@testing-library/jest-dom/vitest';
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import type { ReactNode } from 'react';

const mocks = vi.hoisted(() => ({
  postTaskComment: vi.fn(),
  toastError: vi.fn(),
}));

vi.mock('@/lib/api', () => ({
  fetchProject: vi.fn(),
  fetchTask: vi.fn(),
  fetchTaskActivity: vi.fn(),
  postTaskComment: (...args: unknown[]) => mocks.postTaskComment(...args),
}));

vi.mock('sonner', () => ({ toast: { error: mocks.toastError } }));

vi.mock('@/components/ui/scroll-area', () => ({
  ScrollArea: ({ children }: { children: ReactNode }) => <div>{children}</div>,
}));

import { TaskActivityRail, taskActivityPresentation } from '../TaskDialog';
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
});

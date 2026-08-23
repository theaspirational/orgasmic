// @vitest-environment jsdom
import '@testing-library/jest-dom/vitest';
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import type { OrgNodeDoc } from '@/lib/orgdoc/types';

const { fetchOrgNodeMock, postOrgNodeEditMock } = vi.hoisted(() => ({
  fetchOrgNodeMock: vi.fn(),
  postOrgNodeEditMock: vi.fn(),
}));

vi.mock('@/lib/api', async () => {
  const actual = await vi.importActual<typeof import('@/lib/api')>('@/lib/api');
  return {
    ...actual,
    fetchOrgNode: fetchOrgNodeMock,
    postOrgNodeEdit: postOrgNodeEditMock,
  };
});

import { TASK_DESCRIPTOR } from '../descriptor';
import { NodeDocEditor, type NodeDirectory } from '../NodeDocEditor';

const directory: NodeDirectory = {
  labelFor: (id) => id,
  suggestionsFor: () => [],
};

function taskDoc({
  body = '',
  descriptionSection,
  parentTask,
}: {
  body?: string;
  descriptionSection?: string;
  parentTask?: string;
} = {}): OrgNodeDoc {
  return {
    id: 'TASK-TEST',
    kind: 'task',
    title: 'Test task',
    todo: 'BACKLOG',
    tags: [],
    body,
    properties: [
      { key: 'ID', value: 'TASK-TEST' },
      ...(parentTask ? [{ key: 'PARENT_TASK', value: parentTask }] : []),
    ],
    sections:
      descriptionSection === undefined
        ? []
        : [{ title: 'Description', body: descriptionSection }],
    source: {
      file: '.orgasmic/tasks/backlog.org',
      base_version: 'version-1',
    },
  };
}

function renderEditor(mode: 'view' | 'edit') {
  return render(
    <NodeDocEditor
      projectId="orgasmic"
      nodeId="TASK-TEST"
      descriptor={TASK_DESCRIPTOR}
      directory={directory}
      onOpenNode={vi.fn()}
      mode={mode}
      apiKind="task"
    />,
  );
}

describe('NodeDocEditor task description shape compatibility', () => {
  beforeEach(() => {
    fetchOrgNodeMock.mockReset();
    postOrgNodeEditMock.mockReset();
  });

  afterEach(cleanup);

  it('renders a direct heading body as the task description', async () => {
    fetchOrgNodeMock.mockResolvedValue(taskDoc({ body: 'Direct body description.' }));

    renderEditor('view');

    expect(await screen.findByText('Direct body description.')).toBeInTheDocument();
    expect(screen.queryByText('Describe the task...')).not.toBeInTheDocument();
  });

  it('renders a named Description section when present', async () => {
    fetchOrgNodeMock.mockResolvedValue(
      taskDoc({ descriptionSection: 'Section description.' }),
    );

    renderEditor('view');

    expect(await screen.findByText('Section description.')).toBeInTheDocument();
  });

  it('preserves a direct-body description shape when saving', async () => {
    const updated = taskDoc({ body: 'Updated direct description.' });
    updated.source.base_version = 'version-2';
    fetchOrgNodeMock.mockResolvedValue(taskDoc({ body: 'Direct body description.' }));
    postOrgNodeEditMock.mockResolvedValue(updated);

    renderEditor('edit');

    const description = await screen.findByPlaceholderText('Describe the task...');
    fireEvent.change(description, { target: { value: 'Updated direct description.' } });
    fireEvent.click(await screen.findByRole('button', { name: 'Save' }));

    await waitFor(() =>
      expect(postOrgNodeEditMock).toHaveBeenCalledWith(
        'TASK-TEST',
        {
          baseVersion: 'version-1',
          ops: [{ op: 'set_body', body: 'Updated direct description.' }],
        },
        'orgasmic',
        'task',
      ),
    );
  });

  it('preserves a named Description section shape when saving', async () => {
    const updated = taskDoc({ descriptionSection: 'Updated section description.' });
    updated.source.base_version = 'version-2';
    fetchOrgNodeMock.mockResolvedValue(
      taskDoc({ descriptionSection: 'Section description.' }),
    );
    postOrgNodeEditMock.mockResolvedValue(updated);

    renderEditor('edit');

    const description = await screen.findByPlaceholderText('Describe the task...');
    fireEvent.change(description, { target: { value: 'Updated section description.' } });
    fireEvent.click(await screen.findByRole('button', { name: 'Save' }));

    await waitFor(() =>
      expect(postOrgNodeEditMock).toHaveBeenCalledWith(
        'TASK-TEST',
        {
          baseVersion: 'version-1',
          ops: [
            {
              op: 'set_section_body',
              title: 'Description',
              body: 'Updated section description.',
            },
          ],
        },
        'orgasmic',
        'task',
      ),
    );
  });
});

// orgasmic:task_ZKZBF.2
// PARENT_TASK is a dead key: the daemon refuses to write it (parentage
// derives from the id grammar), so an editable field bound to it — which
// hideWhenEmpty resurrects in edit mode — was a save that could only ever
// come back a 400. The field is gone from TASK_DESCRIPTOR; this pins that
// edit mode offers no unsaveable Parent Task field, even for a task still
// carrying a legacy :PARENT_TASK: drawer line.
describe('NodeDocEditor offers no unsaveable Parent Task field', () => {
  beforeEach(() => {
    fetchOrgNodeMock.mockReset();
    postOrgNodeEditMock.mockReset();
  });

  afterEach(cleanup);

  it('renders no Parent Task field in edit mode for a task with a legacy value', async () => {
    fetchOrgNodeMock.mockResolvedValue(taskDoc({ parentTask: 'TASK-PARENT' }));

    renderEditor('edit');

    // Wait for the editor to load, then assert the field is absent.
    expect(await screen.findByPlaceholderText('Describe the task...')).toBeInTheDocument();
    expect(screen.queryByText('Parent Task')).not.toBeInTheDocument();
    expect(screen.queryByPlaceholderText('Link a parent task...')).not.toBeInTheDocument();
  });

  it('emits no PARENT_TASK op when editing another field', async () => {
    const updated = taskDoc({ body: 'Updated.', parentTask: 'TASK-PARENT' });
    updated.source.base_version = 'version-2';
    fetchOrgNodeMock.mockResolvedValue(taskDoc({ body: 'Stale.', parentTask: 'TASK-PARENT' }));
    postOrgNodeEditMock.mockResolvedValue(updated);

    renderEditor('edit');

    const description = await screen.findByPlaceholderText('Describe the task...');
    fireEvent.change(description, { target: { value: 'Updated.' } });
    fireEvent.click(await screen.findByRole('button', { name: 'Save' }));

    await waitFor(() => expect(postOrgNodeEditMock).toHaveBeenCalledTimes(1));
    const ops = postOrgNodeEditMock.mock.calls[0][1].ops;
    expect(
      ops.some((op: { op: string; key?: string }) => op.key === 'PARENT_TASK'),
    ).toBe(false);
  });
});

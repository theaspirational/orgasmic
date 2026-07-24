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
}: {
  body?: string;
  descriptionSection?: string;
} = {}): OrgNodeDoc {
  return {
    id: 'TASK-TEST',
    kind: 'task',
    title: 'Test task',
    todo: 'BACKLOG',
    tags: [],
    body,
    properties: [{ key: 'ID', value: 'TASK-TEST' }],
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

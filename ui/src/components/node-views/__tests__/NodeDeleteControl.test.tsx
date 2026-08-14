// @vitest-environment jsdom
import '@testing-library/jest-dom/vitest';
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

const mocks = vi.hoisted(() => ({
  postOrgNodeDelete: vi.fn(),
  refreshBump: vi.fn(),
  toastSuccess: vi.fn(),
}));

vi.mock('@/lib/api', () => ({
  postOrgNodeDelete: mocks.postOrgNodeDelete,
}));

vi.mock('@/hooks/useRefreshBus', () => ({
  useRefreshBump: () => mocks.refreshBump,
}));

vi.mock('sonner', () => ({
  toast: { success: mocks.toastSuccess },
}));

import { HttpError } from '@/lib/transport';

import { NodeDeleteControl } from '../NodeDeleteControl';

beforeEach(() => {
  vi.clearAllMocks();
  mocks.postOrgNodeDelete.mockResolvedValue({
    id: 'dec_7RMZJ',
    changed: { deleted: 'true' },
    tx_id: 'tx-delete',
  });
});

afterEach(cleanup);

describe('NodeDeleteControl', () => {
  it('confirms and deletes through the OCC-protected node endpoint', async () => {
    const onDeleted = vi.fn();
    render(
      <NodeDeleteControl
        projectId="orgasmic"
        id="dec_7RMZJ"
        kind="decision"
        title="New sub-decision"
        baseVersion="version-1"
        onDeleted={onDeleted}
      />,
    );

    fireEvent.click(screen.getByRole('button', { name: 'Delete decision dec_7RMZJ' }));
    expect(screen.getByRole('heading', { name: 'Delete this decision?' })).toBeInTheDocument();
    expect(screen.getByText(/permanently removed/)).toBeInTheDocument();

    fireEvent.click(screen.getByRole('button', { name: 'Delete node' }));

    await waitFor(() => {
      expect(mocks.postOrgNodeDelete).toHaveBeenCalledWith(
        'dec_7RMZJ',
        { baseVersion: 'version-1' },
        'orgasmic',
        'decision',
      );
    });
    expect(mocks.refreshBump).toHaveBeenCalledTimes(1);
    expect(mocks.toastSuccess).toHaveBeenCalledWith('Deleted dec_7RMZJ');
    expect(onDeleted).toHaveBeenCalledTimes(1);
  });

  it('keeps the confirmation open and shows the daemon recovery message on rejection', async () => {
    mocks.postOrgNodeDelete.mockRejectedValue(
      new HttpError(400, 'node dec_7RMZJ still has inbound references from TASK-123; remove or redirect them before deleting'),
    );

    render(
      <NodeDeleteControl
        projectId="orgasmic"
        id="dec_7RMZJ"
        kind="decision"
        title="New sub-decision"
        baseVersion="version-1"
        onDeleted={vi.fn()}
      />,
    );

    fireEvent.click(screen.getByRole('button', { name: 'Delete decision dec_7RMZJ' }));
    fireEvent.click(screen.getByRole('button', { name: 'Delete node' }));

    expect(await screen.findByRole('alert')).toHaveTextContent(/remove or redirect them before deleting/);
    expect(screen.getByRole('heading', { name: 'Delete this decision?' })).toBeInTheDocument();
  });

  it('blocks deletion while a decision still has children', () => {
    render(
      <NodeDeleteControl
        projectId="orgasmic"
        id="dec_PARENT"
        kind="decision"
        title="Parent decision"
        baseVersion="version-1"
        childCount={2}
        onDeleted={vi.fn()}
      />,
    );

    fireEvent.click(screen.getByRole('button', { name: 'Delete decision dec_PARENT' }));

    expect(screen.getByRole('alert')).toHaveTextContent('This decision has 2 child decisions.');
    expect(screen.getByRole('button', { name: 'Delete node' })).toBeDisabled();
    expect(mocks.postOrgNodeDelete).not.toHaveBeenCalled();
  });
});

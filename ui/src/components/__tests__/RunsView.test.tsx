// @vitest-environment jsdom
import '@testing-library/jest-dom/vitest';
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { RECOVERY_REMEDIATION_HINTS } from '@/lib/types';
import type { RecoveredRun, RunSummary, RecoveryInventoryResponse } from '@/lib/types';

const { openRunMock, postRunReleaseMock, fetchRecoveryInventoryMock } = vi.hoisted(() => ({
  openRunMock: vi.fn(),
  postRunReleaseMock: vi.fn(async () => ({})),
  fetchRecoveryInventoryMock: vi.fn(),
}));

vi.mock('@/lib/runDock', () => ({
  useRunDock: () => ({ openRun: openRunMock }),
}));

vi.mock('@/lib/api', async () => {
  const actual = await vi.importActual<typeof import('@/lib/api')>('@/lib/api');
  return {
    ...actual,
    fetchRecoveryInventory: fetchRecoveryInventoryMock,
    postRunRelease: postRunReleaseMock,
  };
});

import { RunsView } from '../RunsView';

function run(runId: string, driver: string): RunSummary {
  return {
    run_id: runId,
    task_id: driver === 'external' ? 'manager.launch:proj' : 'TASK-ONE',
    kind: 'worker',
    role: driver === 'external' ? 'manager' : 'implementer',
    driver,
    harness: driver === 'external' ? 'external' : 'claude',
    project_id: 'proj',
    sub_state: null,
    identity: { run_id: runId, runtime_id: `rt-${runId}`, boot_id: 'boot' },
    session_path: `/sessions/${runId}.jsonl`,
    event_count: 0,
  };
}

function response(live: RunSummary[]): RecoveryInventoryResponse {
  return {
    live,
    interrupted: [],
    reattached: [],
    failed_recoverable: [],
    ambiguous: [],
    terminal_noop: [],
  };
}

describe('RunsView external manager action', () => {
  beforeEach(() => {
    openRunMock.mockClear();
    postRunReleaseMock.mockClear();
    fetchRecoveryInventoryMock.mockReset();
  });

  afterEach(cleanup);

  it('renders End instead of Open and releases without creating a dock tab', async () => {
    fetchRecoveryInventoryMock.mockResolvedValue(response([run('run-external', 'external')]));
    render(<RunsView projectId="proj" />);

    const end = await screen.findByRole('button', { name: 'End' });
    expect(screen.queryByRole('button', { name: 'Open' })).not.toBeInTheDocument();
    fireEvent.click(end);

    await waitFor(() => expect(postRunReleaseMock).toHaveBeenCalledWith('run-external'));
    expect(openRunMock).not.toHaveBeenCalled();
  });

  it('keeps Open for an attachable run', async () => {
    fetchRecoveryInventoryMock.mockResolvedValue(response([run('run-worker', 'tmux')]));
    render(<RunsView projectId="proj" />);
    fireEvent.click(await screen.findByRole('button', { name: 'Open' }));
    expect(openRunMock).toHaveBeenCalledWith({ runId: 'run-worker' });
  });
});

/// orgasmic:TASK-2QK4P.1.1.1.1.1 P1a — the F3 operator notice, pinned where it
/// is actually READ.
///
/// The daemon decorates ONLY `failed_recoverable` records with
/// `recovery_unobserved*`, and `GET /runs` serializes them under that key.
/// `RecoveryInventoryResponse` omitted the key and `flattenRuns` never flattened
/// it, so `recoveryUnobservedNotice` ran only on buckets that cannot carry the
/// diagnostic and the refused recoveries were absent from the table entirely.
///
/// `tsc` CANNOT hold this. A field the backend sends and the frontend type omits
/// is not a type error — structural typing ignores extra properties arriving on
/// the wire — so a typecheck was green while the surface was broken. Only
/// rendering the response and reading the cell proves it.
///
/// Injection to see it red: drop `failed_recoverable` from `flattenRuns` (or
/// from the response type) and both assertions below fail.
describe('RunsView refused-recovery diagnostic', () => {
  const SUBJECT = '.orgasmic/sessions/run-refused.jsonl';
  const REMEDIATION = 'repair_session_file';

  function refusedRun(): RecoveredRun {
    return {
      run_id: 'run-refused',
      runtime_id: 'rt-refused',
      boot_id: 'boot-refused',
      session_path: `/proj/${SUBJECT}`,
      classification: 'failed_recoverable',
      reason: 'protocol_end_without_finalize',
      recovery_actions: [],
      // Exactly what `apply_recovery_unobserved_to_run` writes: the tag, the
      // sanitized project-relative subject, and the remediation CLASS (the
      // sentence itself lives in `RECOVERY_REMEDIATION_HINTS`).
      recovery_unobserved: 'claim_store_unreadable',
      recovery_unobserved_subject: SUBJECT,
      recovery_unobserved_remediation: REMEDIATION,
      suppressed_recovery_actions: [],
    };
  }

  beforeEach(() => {
    openRunMock.mockClear();
    fetchRecoveryInventoryMock.mockReset();
  });

  afterEach(cleanup);

  it('renders the refused recovery and names both the file and the repair', async () => {
    fetchRecoveryInventoryMock.mockResolvedValue({
      ...response([]),
      failed_recoverable: [refusedRun()],
    });
    render(<RunsView projectId="proj" />);

    // The row exists at all — the bucket used to be dropped on the floor.
    const cell = await screen.findByText('run-refused');
    const row = cell.closest('tr');
    expect(row).not.toBeNull();
    expect(row).toHaveTextContent('failed_recoverable');

    // And the REASON cell carries both halves of the operator's repair: the
    // named file, and the documented sentence for its remediation class.
    const reason = row!.querySelectorAll('td')[6];
    expect(reason).toBeDefined();
    expect(reason.textContent).toContain(SUBJECT);
    expect(reason.textContent).toContain(RECOVERY_REMEDIATION_HINTS[REMEDIATION]);
    expect(reason.textContent).toContain('claim_store_unreadable');
  });

  it('leaves an ordinary failed_recoverable reason alone', async () => {
    const plain = { ...refusedRun() };
    delete plain.recovery_unobserved;
    delete plain.recovery_unobserved_subject;
    delete plain.recovery_unobserved_remediation;
    fetchRecoveryInventoryMock.mockResolvedValue({
      ...response([]),
      failed_recoverable: [plain],
    });
    render(<RunsView projectId="proj" />);

    const row = (await screen.findByText('run-refused')).closest('tr');
    const reason = row!.querySelectorAll('td')[6];
    expect(reason.textContent).toBe('protocol_end_without_finalize');
  });
});

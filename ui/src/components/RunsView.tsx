import { useMemo, useState } from 'react';

import { fetchRun, fetchRuns } from '../lib/api';
import { useRunDock } from '../lib/runDock';
import type { RecoveredRun, RunsResponse, RunSummary } from '../lib/types';
import { useResource } from '../lib/useResource';
import { Badge, DataTable, ErrorPanel, JsonPanel, Loading } from './Primitives';

type RunRow = Record<string, unknown> & {
  run_id: string;
  classification: string;
  task_id: string;
  kind: string;
  runtime_id: string;
  boot_id: string;
  reason: string;
  sub_state?: string | null;
};

function liveRows(runs: RunSummary[]): RunRow[] {
  return runs.map((run) => ({
    run_id: run.run_id,
    classification: 'live',
    task_id: run.task_id,
    kind: run.role ?? run.kind,
    runtime_id: run.identity.runtime_id,
    boot_id: run.identity.boot_id,
    reason: `${run.event_count} events`,
    sub_state: run.sub_state ?? null,
  }));
}

function recoveredRows(classification: string, runs: RecoveredRun[]): RunRow[] {
  return runs.map((run) => ({
    run_id: run.run_id,
    classification,
    task_id: '',
    kind: '',
    runtime_id: run.runtime_id,
    boot_id: run.boot_id,
    reason: run.reason,
  }));
}

function flattenRuns(data: RunsResponse | null): RunRow[] {
  if (!data) return [];
  return [
    ...liveRows(data.live),
    ...recoveredRows('interrupted', data.interrupted),
    ...recoveredRows('reattached', data.reattached),
    ...recoveredRows('ambiguous', data.ambiguous),
    ...recoveredRows('terminal_noop', data.terminal_noop),
  ];
}

export function RunsView({ projectId: _projectId }: { projectId: string | null }) {
  const { openRun } = useRunDock();
  const runs = useResource('runs', fetchRuns);
  const [selectedRunId, setSelectedRunId] = useState<string | null>(null);
  const detail = useResource(
    `run-detail:${selectedRunId ?? 'none'}`,
    () => fetchRun(selectedRunId ?? ''),
    { enabled: Boolean(selectedRunId) },
  );
  const rows = useMemo(() => flattenRuns(runs.data), [runs.data]);

  if (runs.loading && !runs.data) return <Loading />;

  return (
    <section className="panel-stack">
      <div className="panel">
        <header className="panel-header">
          <h2>Runs</h2>
          <Badge>{rows.length}</Badge>
        </header>
        {runs.error ? <ErrorPanel error={runs.error} /> : null}
      </div>

      <div className="panel">
        <header className="panel-header">
          <h3>Run index</h3>
        </header>
        <DataTable
          rowKey={(row) => String(row.run_id)}
          rows={rows}
          onRowClick={(row) => setSelectedRunId(String(row.run_id))}
          columns={[
            { key: 'run_id', label: 'Run' },
            {
              key: 'classification',
              label: 'State',
              render: (row) => (
                <span className="inline-flex flex-wrap items-center gap-1.5">
                  <span>{String(row.classification)}</span>
                  {row.sub_state ? (
                    <span className="font-mono text-[10px] text-muted-foreground">
                      {String(row.sub_state)}
                    </span>
                  ) : null}
                </span>
              ),
            },
            { key: 'task_id', label: 'Task' },
            { key: 'kind', label: 'Kind' },
            { key: 'runtime_id', label: 'Runtime' },
            { key: 'boot_id', label: 'Boot' },
            { key: 'reason', label: 'Reason' },
            {
              key: 'action',
              label: 'Action',
              render: (row) =>
                row.classification === 'live' ? (
                  <button
                    type="button"
                    className="btn"
                    onClick={(event) => {
                      event.stopPropagation();
                      openRun({ runId: String(row.run_id) });
                    }}
                  >
                    Open
                  </button>
                ) : (
                  ''
                ),
            },
          ]}
        />
      </div>

      <div className="panel">
        <header className="panel-header">
          <h3>Run detail</h3>
          {selectedRunId ? <Badge>{selectedRunId}</Badge> : null}
        </header>
        {!selectedRunId ? (
          <p className="muted">Select a run.</p>
        ) : detail.loading && !detail.data ? (
          <Loading />
        ) : detail.error ? (
          <ErrorPanel error={detail.error} />
        ) : (
          <JsonPanel value={detail.data} />
        )}
      </div>
    </section>
  );
}

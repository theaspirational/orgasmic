import { useState } from 'react';
import { toast } from 'sonner';

import { Button } from '@/components/ui/button';
import { useRefreshToken } from '../hooks/useRefreshBus';
import {
  fetchDaemonStatus,
  fetchParseErrorsWithCoverage,
  fetchRecoveryStatus,
  fetchWhoami,
  loadFullParseErrorCoverage,
} from '../lib/api';
import { useResource } from '../lib/useResource';
import { Badge, DataTable, ErrorPanel, JsonPanel, KeyValue, Loading } from './Primitives';

function runList(runs: { run_id: string; reason?: string }[] | undefined): string {
  if (!runs || runs.length === 0) return '—';
  return runs.map((run) => run.reason ? `${run.run_id} (${run.reason})` : run.run_id).join(', ');
}

export function StatusView() {
  const [loadingFullCoverage, setLoadingFullCoverage] = useState(false);
  const refresh = useRefreshToken();
  const status = useResource(`daemon-status:${refresh}`, fetchDaemonStatus);
  const recovery = useResource(`recovery-status:${refresh}`, fetchRecoveryStatus);
  const parseErrors = useResource(`parse-errors:${refresh}`, fetchParseErrorsWithCoverage);
  const whoami = useResource(`whoami:${refresh}`, fetchWhoami);

  const loading = status.loading && !status.data;
  const coverageIncomplete = parseErrors.data?.coverage.state !== 'complete';

  const loadFullCoverage = async () => {
    setLoadingFullCoverage(true);
    try {
      await loadFullParseErrorCoverage();
      await parseErrors.refresh();
      toast.success('Full parse-error coverage loaded');
    } catch (error) {
      toast.error('Full coverage failed', {
        description: error instanceof Error ? error.message : String(error),
      });
    } finally {
      setLoadingFullCoverage(false);
    }
  };

  if (loading) return <Loading />;

  return (
    <section className="panel-stack">
      <div className="panel">
        <header className="panel-header">
          <h2>Daemon status</h2>
          {status.data ? <Badge tone="ok">{status.data.version}</Badge> : null}
        </header>
        {status.error ? (
          <ErrorPanel error={status.error} />
        ) : (
          <>
            <KeyValue label="Boot ID" value={status.data?.boot_id} />
            <KeyValue label="PID" value={status.data?.pid} />
            <KeyValue label="Started" value={status.data?.started_at} />
            <KeyValue label="Home" value={status.data?.home} />
            <KeyValue label="Machine" value={status.data?.machine} />
            <KeyValue
              label="Bind"
              value={
                status.data?.bind_host && status.data?.bind_port
                  ? `${status.data.bind_host}:${status.data.bind_port}`
                  : undefined
              }
            />
            <KeyValue label="UI hash" value={status.data?.ui_asset_hash?.slice(0, 16)} />
            <KeyValue label="Projects ready" value={status.data?.projects} />
            <KeyValue label="Projects registered" value={status.data?.registered_projects} />
            {status.data?.unloaded_projects?.length ? (
              <KeyValue
                label="Unloaded"
                value={status.data.unloaded_projects.join(', ')}
              />
            ) : null}
            {status.data?.loading_projects?.length ? (
              <KeyValue label="Loading" value={status.data.loading_projects.join(', ')} />
            ) : null}
            {status.data?.failed_projects && Object.keys(status.data.failed_projects).length > 0 ? (
              <KeyValue
                label="Failed"
                value={Object.entries(status.data.failed_projects)
                  .map(([id, error]) => `${id}: ${error}`)
                  .join('; ')}
              />
            ) : null}
            <KeyValue label="Tx entries" value={status.data?.tx_count} />
            <KeyValue label="Parse errors (count)" value={status.data?.parse_errors} />
            <KeyValue label="Index rebuilt" value={status.data?.rebuilt_at} />
          </>
        )}
      </div>

      <div className="panel">
        <header className="panel-header">
          <h3>Recovery</h3>
        </header>
        {recovery.error ? (
          <ErrorPanel error={recovery.error} />
        ) : (
          <>
            <KeyValue label="Boot ID" value={recovery.data?.boot_id} />
            <KeyValue label="Acquisition paused" value={String(recovery.data?.acquisition_paused ?? false)} />
            <KeyValue label="Live runs" value={String(recovery.data?.live_runs.length ?? 0)} />
            <KeyValue label="Interrupted runs" value={runList(recovery.data?.interrupted_runs)} />
            <KeyValue label="Reattached runs" value={runList(recovery.data?.reattached_runs)} />
            <KeyValue label="Terminal no-op" value={runList(recovery.data?.terminal_noop_runs)} />
            <KeyValue label="Ambiguous runs" value={runList(recovery.data?.ambiguous_runs)} />
            <p className="muted">{recovery.data?.note}</p>
          </>
        )}
      </div>

      <div className="panel">
        <header className="panel-header">
          <h3>Auth</h3>
        </header>
        {whoami.error ? (
          <ErrorPanel error={whoami.error} />
        ) : (
          <JsonPanel value={whoami.data} />
        )}
      </div>

      <div className="panel">
        <header className="panel-header">
          <h3>Parse errors</h3>
          <Badge tone={coverageIncomplete || (parseErrors.data?.errors.length ?? 0) > 0 ? 'warn' : 'ok'}>
            {parseErrors.data?.errors.length ?? 0}
            {coverageIncomplete ? ' partial' : ''}
          </Badge>
        </header>
        {parseErrors.loading && !parseErrors.data ? (
          <Loading />
        ) : parseErrors.error ? (
          <ErrorPanel error={parseErrors.error} />
        ) : (
          <>
            {coverageIncomplete ? (
              <div className="mb-3 flex flex-wrap items-center justify-between gap-3 rounded-lg border bg-muted/40 p-3">
                <div className="min-w-0">
                  <p className="text-sm font-medium">Parse-error coverage is partial</p>
                  <p className="mt-1 break-words text-xs text-muted-foreground">
                    Identity diagnostics are not complete until source markers load for every project.
                    {parseErrors.data?.coverage.detail ? ` ${parseErrors.data.coverage.detail}` : ''}
                  </p>
                </div>
                <Button
                  type="button"
                  size="sm"
                  variant="outline"
                  disabled={loadingFullCoverage}
                  onClick={() => void loadFullCoverage()}
                >
                  {loadingFullCoverage ? 'Loading coverage…' : 'Load full coverage'}
                </Button>
              </div>
            ) : null}
            <DataTable
              rowKey={(row) => `${String(row.path)}:${String(row.at)}`}
              rows={(parseErrors.data?.errors ?? []).map((item) => ({
                path: item.path,
                line: item.line ?? '',
                message: item.message,
                at: item.at,
              }))}
              columns={[
                { key: 'path', label: 'Path' },
                { key: 'line', label: 'Line' },
                { key: 'message', label: 'Message' },
                { key: 'at', label: 'At' },
              ]}
            />
          </>
        )}
      </div>
    </section>
  );
}

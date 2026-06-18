import { fetchManagerState, fetchTask, fetchTx } from '../lib/api';
import { useRefreshToken } from '../hooks/useRefreshBus';
import { lifecycleStageLabel } from '../lib/types';
import { useResource } from '../lib/useResource';
import { Badge, DataTable, ErrorPanel, JsonPanel, KeyValue, Loading } from './Primitives';

export function TaskView({
  projectId,
  taskId,
}: {
  projectId: string;
  taskId: string;
}) {
  const refresh = useRefreshToken();
  const task = useResource(`task:${projectId}:${taskId}:${refresh}`, () => fetchTask(projectId, taskId));
  const runs = useResource(`manager-state:${refresh}`, fetchManagerState);
  const tx = useResource(`tx:${projectId}:${refresh}`, () => fetchTx(projectId, 30));

  if (task.loading && !task.data) return <Loading />;
  if (task.error) return <ErrorPanel error={task.error} />;

  const taskRuns = (runs.data?.runs ?? []).filter((run) => run.task_id === taskId);

  return (
    <section className="panel-stack">
      <div className="panel">
        <header className="panel-header">
          <h2>{taskId}</h2>
          <Badge>{lifecycleStageLabel(task.data?.lifecycle_stage)}</Badge>
        </header>
        <KeyValue label="Title" value={task.data?.title} />
        <KeyValue label="Priority" value={task.data?.priority} />
        <KeyValue label="Worker" value={task.data?.worker} />
        <KeyValue label="Source file" value={task.data?.source_file} />
        <KeyValue label="Tags" value={(task.data?.tags ?? []).join(', ') || '—'} />
      </div>

      <div className="panel">
        <header className="panel-header">
          <h3>Runs</h3>
          <span className="muted">{taskRuns.length}</span>
        </header>
        {runs.error ? (
          <ErrorPanel error={runs.error} />
        ) : (
          <DataTable
            rowKey={(row) => String(row.run_id)}
            rows={taskRuns.map((run) => ({
              run_id: run.run_id,
              role: run.role ?? run.kind,
              event_count: run.event_count,
              session_path: run.session_path,
            }))}
            columns={[
              { key: 'run_id', label: 'Run ID' },
              { key: 'role', label: 'Role' },
              { key: 'event_count', label: 'Events' },
              { key: 'session_path', label: 'Session' },
            ]}
          />
        )}
      </div>

      <div className="panel">
        <header className="panel-header">
          <h3>Recent tx (project)</h3>
        </header>
        {tx.loading && !tx.data ? (
          <Loading />
        ) : tx.error ? (
          <ErrorPanel error={tx.error} />
        ) : (
          <JsonPanel value={tx.data} />
        )}
      </div>
    </section>
  );
}

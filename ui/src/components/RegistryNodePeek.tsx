import { lazy, Suspense } from 'react';
import { Dialog, DialogContent, DialogHeader, DialogTitle } from '@/components/ui/dialog';
import { ErrorPanel, Loading } from '@/components/Primitives';
import { NodeModal } from '@/components/node-views/NodeModal';
import { GenericNodeDialog } from '@/components/GenericNodeView';
import { TaskDialogChunkFallback } from '@/components/TaskDialogChunkFallback';
import { fetchOrgNode } from '@/lib/api';
import { useNodeTypes } from '@/lib/nodeTypes';
import { useResource } from '@/lib/useResource';

const TaskDialog = lazy(() => import('@/components/TaskDialog').then((module) => ({ default: module.TaskDialog })));

export function RegistryNodePeek({ projectId, nodeId, historyDepth, onBack, onClose, onOpenNode }: {
  projectId: string; nodeId: string; historyDepth: number;
  onBack: () => void; onClose: () => void; onOpenNode: (id: string) => void;
}) {
  const registry = useNodeTypes();
  const matched = registry.data?.find((type) => nodeId.startsWith(type.id_prefix));
  const compiled = matched?.collection === 'tasks' ? 'task'
    : matched?.collection === 'decisions' ? 'decision'
      : matched?.collection === 'glossary' ? 'glossary' : undefined;
  const resource = useResource(`peek:${projectId}:${nodeId}`, () => fetchOrgNode(nodeId, projectId), { enabled: !compiled });
  const doc = resource.data;
  // The registry owns prefixes; compiled dialogs need no preliminary node read.
  const kind = compiled ?? doc?.kind;
  if (kind === 'task') return <Suspense fallback={<TaskDialogChunkFallback taskId={nodeId} historyDepth={historyDepth} onBack={onBack} onClose={onClose} />}>
    <TaskDialog projectId={projectId} taskId={nodeId} historyDepth={historyDepth} onBack={onBack} onClose={onClose} onSelectTask={onOpenNode} />
  </Suspense>;
  if (kind === 'decision' || kind === 'glossary') return <NodeModal projectId={projectId} nodeKind={kind} />;
  if (!doc) return <Dialog open onOpenChange={(open) => !open && onClose()}>
    <DialogContent aria-describedby={undefined}>
      <DialogHeader><DialogTitle>{nodeId}</DialogTitle></DialogHeader>
      {resource.error ? <ErrorPanel error={resource.error} /> : <Loading label="Loading node…" />}
    </DialogContent>
  </Dialog>;
  return <GenericNodeDialog projectId={projectId} initialDocument={doc}
    type={registry.data?.find((type) => type.collection === doc.collection)}
    historyDepth={historyDepth} onBack={onBack} onClose={onClose} onOpenNode={onOpenNode} />;
}

import { lazy, Suspense } from 'react';
import { Dialog, DialogContent, DialogHeader, DialogTitle } from '@/components/ui/dialog';
import { ErrorPanel, Loading } from '@/components/Primitives';
import { NodeModal } from '@/components/node-views/NodeModal';
import { GenericNodeDialog } from '@/components/GenericNodeView';
import { TaskDialogChunkFallback } from '@/components/TaskDialogChunkFallback';
import { PeekBackButton } from '@/components/PeekBackButton';
import { NodeBacklinks } from '@/components/NodeBacklinks';
import { fetchOrgNode } from '@/lib/api';
import { useNodeTypes } from '@/lib/nodeTypes';
import { useResource } from '@/lib/useResource';
import { PluginViewBoundary, usePluginView } from '@/lib/pluginRuntime';

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
  const plugin = usePluginView(doc?.collection ?? matched?.collection ?? '');
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
  const fallback = <GenericNodeDialog projectId={projectId} initialDocument={doc}
    type={registry.data?.find((type) => type.collection === doc.collection)}
    historyDepth={historyDepth} onBack={onBack} onClose={onClose} onOpenNode={onOpenNode} />;
  if (!plugin || doc.schema_matches === false) return fallback;
  return <PluginViewBoundary key={`${nodeId}:${plugin.revision}`} fallback={fallback}>
    <Dialog open onOpenChange={(open) => !open && onClose()}><DialogContent aria-describedby={undefined} className="max-h-[90dvh] overflow-y-auto sm:max-w-3xl">
      <DialogHeader><PeekBackButton depth={historyDepth} onBack={onBack} /><DialogTitle>{doc.title}</DialogTitle></DialogHeader>
      <div data-plugin={plugin.pluginId}><plugin.View projectId={projectId} collection={doc.collection ?? doc.kind} nodeId={nodeId} onOpenNode={onOpenNode} /></div>
      <NodeBacklinks projectId={projectId} nodeId={nodeId} />
    </DialogContent></Dialog>
  </PluginViewBoundary>;
}

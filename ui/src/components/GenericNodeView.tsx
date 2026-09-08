import { useMemo, useState } from 'react';
import { useNavigate, useRouterState } from '@tanstack/react-router';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Dialog, DialogContent, DialogDescription, DialogHeader, DialogTitle } from '@/components/ui/dialog';
import { ErrorPanel, Loading, PageHeader } from '@/components/Primitives';
import { PeekBackButton } from '@/components/PeekBackButton';
import { NodeListView } from '@/components/node-views/NodeListView';
import { NodeDocEditor, type NodeDirectory } from '@/components/orgdoc/NodeDocEditor';
import { collectionDescriptor } from '@/components/orgdoc/descriptor';
import { useMe } from '@/hooks/useMe';
import { useRefreshBump, useRefreshToken } from '@/hooks/useRefreshBus';
import { fetchGraphNodes, postOrgNodeEdit, type NodeTypeDescriptor } from '@/lib/api';
import { pushEntityPeek } from '@/lib/entityPeek';
import { useNodeTypes } from '@/lib/nodeTypes';
import type { OrgNodeDoc } from '@/lib/orgdoc/types';
import { routeSearch } from '@/lib/searchState';
import { useResource } from '@/lib/useResource';

const directory: NodeDirectory = { labelFor: (id) => id, suggestionsFor: () => [] };

export function GenericNodeView({ projectId, collection }: { projectId: string; collection: string }) {
  const registry = useNodeTypes();
  const { can } = useMe();
  const refresh = useRefreshToken();
  const navigate = useNavigate();
  const pathname = useRouterState({ select: (state) => state.location.pathname });
  const [search, setSearch] = useState('');
  const canRead = can(projectId, 'graph.read');
  // Hooks stay above all loading/error/permission returns.
  const resource = useResource(`collection:${projectId}:${collection}:${refresh}`, () => fetchGraphNodes(projectId, collection), { enabled: canRead });
  const type = registry.data?.find((item) => item.collection === collection);
  const nodes = resource.data ?? [];
  const filtered = nodes.filter((node) => `${node.id} ${node.title ?? ''} ${node.todo ?? ''}`.toLowerCase().includes(search.toLowerCase()));

  if (!canRead) return <ErrorPanel error="You do not have permission to read this collection." />;
  if (registry.error) return <ErrorPanel error={registry.error} />;
  if (!registry.data) return <Loading label="Loading node types…" />;
  if (!type) return <ErrorPanel error={`The ${collection} descriptor is unavailable. No data has been changed.`} />;
  return <div className="flex flex-col gap-4">
    <PageHeader title={type.label_plural} count={nodes.length} />
    {resource.error ? <ErrorPanel error={resource.error} /> : null}
    <NodeListView ariaLabel={type.label_plural} items={filtered} getId={(node) => node.id}
      loading={resource.loading && !resource.data} search={search} onSearchChange={setSearch}
      emptyLabel={search ? 'No matching nodes.' : `No ${type.label_plural.toLowerCase()} yet.`}
      onSelect={(id) => void navigate({ search: routeSearch((previous) => pushEntityPeek(pathname, previous, id)) })}
      renderRow={(node) => <div className="flex min-w-0 flex-1 items-center gap-3">
        <span className="shrink-0 font-mono text-xs text-muted-foreground">{node.id}</span>
        <span className="min-w-0 flex-1 break-words text-sm">{node.title || node.id}</span>
        {node.todo ? <Badge variant="outline">{node.todo}</Badge> : null}
      </div>} />
  </div>;
}

export function GenericNodeDialog({ projectId, initialDocument, type, historyDepth, onBack, onClose, onOpenNode }: {
  projectId: string;
  initialDocument: OrgNodeDoc;
  type?: NodeTypeDescriptor;
  historyDepth: number;
  onBack: () => void;
  onClose: () => void;
  onOpenNode: (id: string) => void;
}) {
  const { can } = useMe();
  const bump = useRefreshBump();
  const [mode, setMode] = useState<'view' | 'edit'>('view');
  const [document, setDocument] = useState<OrgNodeDoc | null>(null);
  const [reload, setReload] = useState(0);
  const [savingState, setSavingState] = useState(false);
  const [error, setError] = useState<unknown>(null);
  const doc = document ?? initialDocument;
  const descriptor = useMemo(() => collectionDescriptor(type ?? {
    collection: doc.kind, id_prefix: '', label: doc.kind, label_plural: doc.kind,
    required_properties: [], states: [], transitions: {}, regenerate_prompt: null,
  }), [type, doc.kind]);
  const state = doc.todo?.toLowerCase() ?? '';
  const schemaMatches = Boolean(type && (!type.states.length || type.states.includes(state)));
  const canEdit = can(projectId, 'nodes.write') && schemaMatches;
  const nextStates = type?.transitions[state] ?? [];

  async function changeState(next: string) {
    if (!document || !canEdit || savingState || mode === 'edit') return;
    setSavingState(true);
    setError(null);
    try {
      await postOrgNodeEdit(doc.id, { baseVersion: doc.source.base_version, ops: [{ op: 'set_state', state: next }] }, projectId, doc.kind);
      setDocument(null);
      setReload((value) => value + 1);
      bump();
    } catch (cause) {
      setError(cause);
    } finally {
      setSavingState(false);
    }
  }

  return <Dialog open onOpenChange={(open) => !open && onClose()}>
    <DialogContent className="flex max-h-[90dvh] flex-col overflow-hidden sm:max-w-3xl">
      <DialogHeader className="pr-6">
        <div className="flex items-start gap-2">
          <PeekBackButton depth={historyDepth} onBack={onBack} />
          <div className="min-w-0">
            <DialogDescription className="font-mono text-xs">{doc.id}</DialogDescription>
            <DialogTitle className="break-words">{doc.title}</DialogTitle>
          </div>
        </div>
      </DialogHeader>
      <div className="flex flex-wrap items-center justify-between gap-3 border-b pb-3">
        {type?.states.length ? <label className="flex items-center gap-2 text-sm">State
          <select aria-label="State" value={state} disabled={!document || !canEdit || savingState || mode === 'edit'}
            onChange={(event) => void changeState(event.target.value)} className="h-9 rounded-md border bg-background px-2 text-sm">
            <option value={state}>{doc.todo || 'Unrecognized state'}</option>
            {nextStates.filter((next) => next !== state).map((next) => <option key={next} value={next}>{next.toUpperCase()}</option>)}
          </select>
        </label> : <span className="text-sm text-muted-foreground">{type?.label ?? doc.kind}</span>}
        {canEdit ? <Button variant="outline" size="sm" disabled={!document || savingState} onClick={() => setMode((current) => current === 'view' ? 'edit' : 'view')}>
          {mode === 'view' ? 'Edit' : 'View'}
        </Button> : <Badge variant="outline">Read only</Badge>}
      </div>
      <div className="min-h-0 overflow-y-auto">
        {!schemaMatches ? <p className="mb-3 text-sm text-muted-foreground">The descriptor is unavailable or does not recognize this node state. Editing is disabled.</p> : null}
        {error ? <ErrorPanel error={error} /> : null}
        <NodeDocEditor key={reload} projectId={projectId} nodeId={doc.id} apiKind={doc.kind}
          descriptor={descriptor} directory={directory} onOpenNode={onOpenNode}
          mode={canEdit ? mode : 'view'} readOnly={!canEdit} onDocumentChange={setDocument} />
      </div>
    </DialogContent>
  </Dialog>;
}

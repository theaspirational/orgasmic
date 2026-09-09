import { useNavigate, useRouterState } from '@tanstack/react-router';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { useMe } from '@/hooks/useMe';
import { useEventStream } from '@/hooks/useEventStream';
import { fetchOrgNode } from '@/lib/api';
import { CONVERSATION_PREFIX, conversationProperty } from '@/lib/conversations';
import { get } from '@/lib/transport';
import { useOptionalRunDock } from '@/lib/runDock';
import { useResource } from '@/lib/useResource';
import { pushEntityPeek } from '@/lib/entityPeek';
import { routeSearch } from '@/lib/searchState';
import { mediaSearch, mediaTime, type NodeLink, type MediaAnchor } from '@/lib/nodeServices';

export function NodeBacklinks({ projectId, nodeId }: { projectId: string; nodeId: string }) {
  const { can } = useMe();
  const navigate = useNavigate();
  const dock = useOptionalRunDock();
  const pathname = useRouterState({ select: (state) => state.location.pathname });
  const enabled = can(projectId, 'links.read');
  const links = useResource(`backlinks:${projectId}:${nodeId}`,
    () => get<NodeLink[]>(`/links?project=${encodeURIComponent(projectId)}&node=${encodeURIComponent(nodeId)}&incoming=true`), { enabled });
  useEventStream((event) => {
    if (enabled && event.topic === 'graph' && event.payload.project_id === projectId) void links.refresh();
  });
  if (!enabled || (!links.error && !links.data?.length)) return null;
  const open = (link: NodeLink, anchor?: MediaAnchor) => void navigate({ search: routeSearch((previous) => ({
    ...pushEntityPeek(pathname, previous, link.source), ...mediaSearch(link.source, anchor),
  })) });
  // Conversations about this node (CHAT-SCOPE C1) open in the dock, not as a peek.
  const conversations = links.data?.filter((link) => link.source.startsWith(CONVERSATION_PREFIX)) ?? [];
  const others = links.data?.filter((link) => !link.source.startsWith(CONVERSATION_PREFIX)) ?? [];
  return <section aria-label="Backlinks" className="flex flex-col gap-2 border-t pt-4">
    <h3 className="text-sm font-medium">Backlinks</h3>
    {links.error ? <p role="alert" className="text-sm text-destructive">Could not load backlinks. <Button variant="link" onClick={() => void links.refresh()}>Retry</Button></p> : null}
    {conversations.length ? <>
      <h4 className="text-xs font-medium text-muted-foreground">Conversations</h4>
      <ul className="flex flex-col gap-1">{conversations.map((link) => <li key={link.id}>
        <ConversationBacklink projectId={projectId} link={link}
          onOpen={dock ? () => dock.openChat({ conversationId: link.source }) : () => open(link)} />
      </li>)}</ul>
    </> : null}
    <ul className="flex flex-col gap-1">{others.map((link) => <li key={link.id} className="flex flex-wrap items-center gap-2">
      <Button variant="link" className="h-auto p-0" onClick={() => open(link)}>{link.source}</Button>
      {link.anchors.map((anchor, i) => <Button key={i} variant="outline" size="sm" onClick={() => open(link, anchor)}>
        {mediaTime(anchor.start_ms)}{anchor.label ? ` · ${anchor.label}` : ''}
      </Button>)}
    </li>)}</ul>
  </section>;
}

function ConversationBacklink({ projectId, link, onOpen }: { projectId: string; link: NodeLink; onOpen: () => void }) {
  const doc = useResource(`backlink-conversation:${projectId}:${link.source}`, () => fetchOrgNode(link.source, projectId));
  const purpose = conversationProperty(doc.data, 'PURPOSE');
  return <div className="flex flex-wrap items-center gap-2">
    <Button variant="link" className="h-auto p-0" onClick={onOpen}>{doc.data?.title || link.source}</Button>
    {purpose ? <Badge variant="secondary">{purpose}</Badge> : null}
    {doc.data?.todo ? <Badge variant="outline">{doc.data.todo}</Badge> : null}
  </div>;
}

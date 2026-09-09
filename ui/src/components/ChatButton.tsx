import { MessageCircle } from 'lucide-react';

import { Button } from '@/components/ui/button';
import { useMe } from '@/hooks/useMe';
import { useOptionalRunDock } from '@/lib/runDock';

/** "Chat about this node" (or this project, without `node`) — opens the dock's
 * Chat tab on the node's newest OPEN conversation or a scoped setup
 * (CHAT-SCOPE C1). Hidden without chat.write or outside the dock provider. */
export function ChatButton({
  projectId,
  node,
  className,
}: {
  projectId: string;
  node?: string;
  className?: string;
}) {
  const { can } = useMe();
  const dock = useOptionalRunDock();
  if (!dock || !can(projectId, 'chat.write')) return null;
  return (
    <Button
      type="button"
      variant="outline"
      size="sm"
      className={className}
      aria-label={node ? `Chat about ${node}` : 'Chat'}
      onClick={() => dock.openChat(node ? { node } : {})}
    >
      <MessageCircle />
      Chat
    </Button>
  );
}

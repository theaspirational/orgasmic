import { useCallback, useEffect, useMemo, useState, type ReactNode } from 'react';
import { Loader2, MessageCircle, Plus } from 'lucide-react';
import { toast } from 'sonner';

import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { useEventStream } from '@/hooks/useEventStream';
import { useMe } from '@/hooks/useMe';
import {
  fetchConversation,
  fetchConversations,
  fetchNodeLinks,
  fetchRun,
  findNodeConversation,
  isRunGoneError,
  postConversationCreate,
  postConversationInput,
  postRunRelease,
} from '@/lib/api';
import {
  conversationOwnedBy,
  conversationProperty,
  conversationRuns,
  orderConversations,
  segmentLabel,
  type ConversationRun,
} from '@/lib/conversations';
import { useRunDock, type ChatTarget } from '@/lib/runDock';
import { isConversationRun } from '@/lib/runLabels';
import { parseSessionSource } from '@/lib/transcriptParts';
import { TranscriptStream } from '@/lib/transcriptStream';
import type { ConversationContextChip, DaemonEvent, RunSummary } from '@/lib/types';
import { cn } from '@/lib/utils';
import { useResource } from '@/lib/useResource';

import { ChatSetup } from './ChatSetup';
import { TranscriptPartsView } from './ManagerChatTranscript';
import { ManagerComposer } from './ManagerComposer';
import { ReadOnlySessionBar } from './ReadOnlySessionBar';
import { RunSurface } from './RunSurface';
import type { ChatSelection } from './chatProviders';

export const CHAT_EXECUTE_LABEL = 'Chat runs an agent on the host. Ask an admin to grant chat.execute.';

// The Chat tab (CHAT-SCOPE C1): the project's conversations on the left, the
// selected one on the right. A conversation renders as its runs; the current
// run is a live RunSurface when the supervisor holds it, else its transcript
// with a composer that continues through POST /conversations/:id/input.
export function ConversationPanel({
  projectId,
  readOnly,
  liveRuns,
  onRefresh,
}: {
  projectId: string | null;
  /** The dock's sessions.interact gate; chat grants narrow it further. */
  readOnly: boolean;
  liveRuns: RunSummary[];
  onRefresh: () => void;
}) {
  const { can } = useMe();
  const { chatTarget, setChatTarget } = useRunDock();
  const canWrite = can(projectId, 'chat.write');
  const canExecute = can(projectId, 'chat.execute');
  const chatReadOnly = readOnly || !canWrite;
  const disabledLabel = !chatReadOnly && !canExecute ? CHAT_EXECUTE_LABEL : null;

  const conversations = useResource(
    `conversations:${projectId ?? 'none'}`,
    () => fetchConversations(projectId ?? ''),
    { enabled: Boolean(projectId) },
  );
  useEventStream(
    useCallback(
      (event: DaemonEvent) => {
        if (event.topic === 'graph' && event.payload.project_id === projectId) void conversations.refresh();
      },
      [conversations, projectId],
    ),
  );
  const liveByConversation = useMemo(
    () => new Map(liveRuns.filter(isConversationRun).map((run) => [run.task_id, run])),
    [liveRuns],
  );
  const entries = useMemo(
    () => orderConversations(conversations.data ?? [], liveByConversation.keys()),
    [conversations.data, liveByConversation],
  );

  // openChat({node}) lands here as a lookup; resolve it against the node's
  // backlinks into its newest OPEN conversation or a scoped setup.
  useEffect(() => {
    if (chatTarget.kind !== 'lookup' || !projectId) return;
    const { node, purpose } = chatTarget;
    let cancelled = false;
    findNodeConversation(projectId, node)
      .catch(() => null)
      .then((conversationId) => {
        if (cancelled) return;
        setChatTarget(
          conversationId
            ? { kind: 'conversation', conversationId }
            : { kind: 'setup', node, purpose },
        );
      });
    return () => {
      cancelled = true;
    };
  }, [chatTarget, projectId, setChatTarget]);

  async function handleStart(target: ChatTarget & { kind: 'setup' }, selection: ChatSelection, message: string) {
    if (chatReadOnly || !projectId) return false;
    const result = await postConversationCreate(projectId, {
      purpose: target.purpose ?? 'discuss',
      node: target.node ?? null,
      provider: selection.provider,
      model: selection.model || null,
      effort: selection.effort || null,
      access: selection.access,
      service_tier: selection.serviceTier || null,
      mode: 'chat',
      message,
    });
    setChatTarget({ kind: 'conversation', conversationId: result.id });
    onRefresh();
    void conversations.refresh();
    return true;
  }

  const newChatActive = chatTarget.kind !== 'conversation';
  return (
    <div className="flex h-full min-h-0 flex-col sm:flex-row">
      <nav
        aria-label="Conversations"
        className="flex shrink-0 gap-1 overflow-x-auto border-b p-1.5 sm:w-56 sm:flex-col sm:overflow-y-auto sm:border-b-0 sm:border-r"
      >
        {!chatReadOnly ? (
          <ConversationRow
            active={newChatActive}
            icon={<Plus className="size-3.5 shrink-0" />}
            label="New chat"
            onClick={() => setChatTarget({ kind: 'setup' })}
          />
        ) : null}
        {entries.map((entry) => (
          <ConversationRow
            key={entry.id}
            active={chatTarget.kind === 'conversation' && chatTarget.conversationId === entry.id}
            icon={<MessageCircle className="size-3.5 shrink-0" />}
            label={entry.title}
            title={`${entry.id}${entry.todo ? ` · ${entry.todo}` : ''}`}
            live={entry.live}
            archived={entry.todo === 'ARCHIVED'}
            onClick={() => setChatTarget({ kind: 'conversation', conversationId: entry.id })}
          />
        ))}
        {conversations.data && entries.length === 0 ? (
          <p className="px-2 py-1.5 text-xs text-muted-foreground">No conversations yet.</p>
        ) : null}
      </nav>
      <div className="min-h-0 min-w-0 flex-1">
        {chatTarget.kind === 'lookup' ? (
          <div className="flex h-full items-center justify-center gap-2 text-sm text-muted-foreground">
            <Loader2 className="size-4 animate-spin motion-reduce:animate-none" />
            Finding the conversation about {chatTarget.node}…
          </div>
        ) : chatTarget.kind === 'setup' ? (
          <ChatSetup
            key={`${projectId ?? 'no-project'}:${chatTarget.node ?? ''}`}
            projectId={projectId}
            readOnly={chatReadOnly}
            scopeNode={chatTarget.node ?? null}
            disabledLabel={disabledLabel}
            onStart={(selection, message) => handleStart(chatTarget, selection, message)}
          />
        ) : projectId ? (
          <ConversationView
            key={chatTarget.conversationId}
            projectId={projectId}
            conversationId={chatTarget.conversationId}
            liveRun={liveByConversation.get(chatTarget.conversationId) ?? null}
            readOnly={chatReadOnly}
            disabledLabel={disabledLabel}
            onRefresh={onRefresh}
            onMissing={() => setChatTarget({ kind: 'setup' })}
          />
        ) : null}
      </div>
    </div>
  );
}

function ConversationRow({
  active,
  icon,
  label,
  title,
  live,
  archived,
  onClick,
}: {
  active: boolean;
  icon: ReactNode;
  label: string;
  title?: string;
  live?: boolean;
  archived?: boolean;
  onClick: () => void;
}) {
  return (
    <button
      type="button"
      aria-current={active ? 'true' : undefined}
      title={title}
      onClick={onClick}
      className={cn(
        'flex shrink-0 items-center gap-2 rounded-md px-2 py-1.5 text-left text-xs sm:w-full',
        active ? 'bg-accent text-accent-foreground' : 'text-muted-foreground hover:bg-muted hover:text-foreground',
        archived && 'opacity-60',
      )}
    >
      {icon}
      <span className="max-w-40 truncate sm:max-w-none sm:flex-1">{label}</span>
      {live ? (
        <span aria-label="Live" title="Live" className="size-1.5 shrink-0 rounded-full bg-primary" />
      ) : null}
    </button>
  );
}

function ConversationView({
  projectId,
  conversationId,
  liveRun,
  readOnly,
  disabledLabel,
  onRefresh,
  onMissing,
}: {
  projectId: string;
  conversationId: string;
  liveRun: RunSummary | null;
  readOnly: boolean;
  disabledLabel: string | null;
  onRefresh: () => void;
  onMissing: () => void;
}) {
  const { identity, me } = useMe();
  const doc = useResource(`conversation:${projectId}:${conversationId}`, () =>
    fetchConversation(conversationId, projectId),
  );
  const scope = useResource(`conversation-scope:${projectId}:${conversationId}`, () =>
    fetchNodeLinks(projectId, conversationId),
  );
  // RUNS grows on every continuation; the daemon announces it as a graph event.
  useEventStream(
    useCallback(
      (event: DaemonEvent) => {
        if (event.topic === 'graph' && event.payload.project_id === projectId) void doc.refresh();
      },
      [doc, projectId],
    ),
  );
  // The run a send just started, until RUNS catches up.
  const [sentRun, setSentRun] = useState<string | null>(null);

  const runs = useMemo(() => conversationRuns(doc.data), [doc.data]);
  const currentRunId = liveRun?.run_id ?? sentRun ?? runs.at(-1)?.runId ?? null;
  const older = runs.filter((run) => run.runId !== currentRunId);
  const currentMode = runs.find((run) => run.runId === currentRunId)?.mode ?? 'start';
  const scopedNode = scope.data?.find((link) => !link.deleted)?.target ?? null;

  const owned = conversationOwnedBy(conversationProperty(doc.data, 'OWNER'), {
    identity,
    name: me?.name ?? null,
  });
  const composerDisabled =
    disabledLabel ??
    (doc.data?.todo === 'ARCHIVED'
      ? 'This conversation is archived.'
      : !owned
        ? 'Only the owner or an admin can continue this conversation.'
        : null);

  async function send(text: string): Promise<boolean> {
    const context: ConversationContextChip[] | undefined = scopedNode ? [{ kind: 'node', id: scopedNode }] : undefined;
    const result = await postConversationInput(conversationId, projectId, { message: text, context });
    if (result.run_id !== currentRunId) {
      setSentRun(result.run_id);
      void doc.refresh();
    }
    onRefresh();
    return true;
  }

  async function stop() {
    if (!liveRun) return;
    try {
      await postRunRelease(liveRun.run_id);
      toast.success('Run stopped');
    } catch (err) {
      if (!isRunGoneError(err)) {
        toast.error('Stopping run failed', { description: err instanceof Error ? err.message : String(err) });
        return;
      }
    }
    onRefresh();
  }

  const chips = scopedNode ? (
    <Badge variant="secondary" className="font-mono" title={`Every message carries ${scopedNode}`}>
      {scopedNode}
    </Badge>
  ) : null;
  const conversation = { onSend: send, chips, disabledLabel: composerDisabled };

  if (doc.error) {
    return (
      <div className="flex h-full flex-col items-center justify-center gap-3 p-6 text-center">
        <p className="text-sm text-muted-foreground">
          {isRunGoneError(doc.error) ? `Conversation ${conversationId} no longer exists.` : String(doc.error)}
        </p>
        <div className="flex items-center gap-2">
          <Button type="button" variant="outline" size="sm" onClick={() => void doc.refresh()}>Retry</Button>
          <Button type="button" variant="ghost" size="sm" onClick={onMissing}>New chat</Button>
        </div>
      </div>
    );
  }

  return (
    <div className="flex h-full min-h-0 flex-col bg-muted/20">
      {older.length > 0 ? (
        <div className="max-h-[40%] shrink-0 overflow-y-auto border-b">
          {older.map((run) => (
            <Segment key={run.runId} run={run} collapsible />
          ))}
        </div>
      ) : null}
      <div className="min-h-0 flex-1">
        {liveRun ? (
          <RunSurface
            run={liveRun}
            onPromptSent={() => {}}
            onStop={readOnly ? undefined : stop}
            conversation={conversation}
            readOnly={readOnly}
          />
        ) : (
          <div className="flex h-full min-h-0 flex-col">
            <div className="min-h-0 flex-1 overflow-y-auto">
              {currentRunId ? (
                <Segment run={{ runId: currentRunId, mode: currentMode }} />
              ) : doc.data ? (
                <p className="p-6 text-center text-sm text-muted-foreground">
                  No runs yet. Send a message to start one.
                </p>
              ) : null}
            </div>
            <div className="shrink-0 px-3 pb-3 pt-2 sm:px-4 sm:pb-4">
              <div className="mx-auto w-full max-w-3xl">
                {readOnly ? (
                  <ReadOnlySessionBar />
                ) : (
                  <ManagerComposer
                    runId={composerDisabled || !doc.data ? null : conversationId}
                    connectionState="open"
                    placeholder="Send to agent"
                    readyLabel="Enter to send · Shift+Enter for a new line · the agent resumes or restarts as needed"
                    unavailableLabel={composerDisabled ?? 'Loading conversation…'}
                    onSend={send}
                    controls={chips}
                  />
                )}
              </div>
            </div>
          </div>
        )}
      </div>
    </div>
  );
}

// One run of a conversation: its whole session text from GET /runs/:id folded
// through the same reducer the live stream uses. Older runs start collapsed and
// load on expand.
function Segment({ run, collapsible = false }: { run: ConversationRun; collapsible?: boolean }) {
  const [open, setOpen] = useState(!collapsible);
  const detail = useResource(`conversation-run:${run.runId}`, () => fetchRun(run.runId), { enabled: open });
  const parts = useMemo(() => {
    if (!detail.data) return [];
    const stream = new TranscriptStream();
    stream.reset(parseSessionSource(detail.data.source));
    return stream.parts();
  }, [detail.data]);
  const body = detail.error ? (
    <p className="text-sm text-muted-foreground">Transcript unavailable on this machine.</p>
  ) : detail.data ? (
    parts.length ? <TranscriptPartsView parts={parts} /> : <p className="text-sm text-muted-foreground">Empty run.</p>
  ) : (
    <p className="text-sm text-muted-foreground">Loading transcript…</p>
  );
  const heading = (
    <span className="flex items-center gap-2 text-[11px] uppercase text-muted-foreground">
      <span>{segmentLabel(run.mode)}</span>
      <code className="font-mono normal-case">{run.runId}</code>
    </span>
  );
  if (!collapsible) {
    return (
      <div className="mx-auto flex w-full max-w-3xl flex-col gap-4 px-4 py-6 sm:px-6" data-testid="conversation-segment">
        {heading}
        {body}
      </div>
    );
  }
  return (
    <details
      className="mx-auto w-full max-w-3xl px-4 py-2 sm:px-6"
      data-testid="conversation-segment"
      onToggle={(event) => setOpen(event.currentTarget.open)}
    >
      <summary className="cursor-pointer select-none">{heading}</summary>
      {open ? <div className="mt-3 flex flex-col gap-4">{body}</div> : null}
    </details>
  );
}

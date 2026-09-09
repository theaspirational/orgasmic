// Pure helpers for conversation nodes (CHAT-SCOPE C1). No transport imports so
// they unit-test in the node environment.
import type { OrgNodeDoc } from './orgdoc/types';
import type { GraphNodeSummary, MeIdentity } from './types';

export const CONVERSATION_PREFIX = 'CONV-';

export type ConversationRunMode = 'start' | 'resumed' | 'cold';
export type ConversationRun = { runId: string; mode: ConversationRunMode };

/** `RUNS` is space separated, oldest first: `run-A run-B:resumed run-C:cold`. */
export function parseConversationRuns(raw: string | null | undefined): ConversationRun[] {
  return (raw ?? '')
    .split(/\s+/)
    .filter(Boolean)
    .map((entry) => {
      const at = entry.lastIndexOf(':');
      const suffix = at > 0 ? entry.slice(at + 1) : '';
      if (suffix === 'resumed' || suffix === 'cold') return { runId: entry.slice(0, at), mode: suffix };
      return { runId: entry, mode: 'start' };
    });
}

export function conversationProperty(doc: Pick<OrgNodeDoc, 'properties'> | null | undefined, key: string): string | null {
  return doc?.properties.find((property) => property.key === key)?.value ?? null;
}

export function conversationRuns(doc: Pick<OrgNodeDoc, 'properties'> | null | undefined): ConversationRun[] {
  return parseConversationRuns(conversationProperty(doc, 'RUNS'));
}

export function segmentLabel(mode: ConversationRunMode): string {
  if (mode === 'resumed') return 'Resumed';
  if (mode === 'cold') return 'Continued without native memory';
  return 'Started';
}

export type ConversationEntry = { id: string; title: string; todo: string | null; live: boolean };

/** Open conversations first, live ones marked; server order otherwise. */
export function orderConversations(
  nodes: Pick<GraphNodeSummary, 'id' | 'title' | 'todo'>[],
  liveIds: Iterable<string>,
): ConversationEntry[] {
  const live = new Set(liveIds);
  const entries = nodes.map((node) => ({
    id: node.id,
    title: node.title || node.id,
    todo: node.todo ?? null,
    live: live.has(node.id),
  }));
  return [...entries.filter((entry) => entry.todo === 'OPEN'), ...entries.filter((entry) => entry.todo !== 'OPEN')];
}

/** Newest OPEN conversation by CREATED_AT; ISO timestamps compare as strings. */
export function newestOpenConversation(docs: OrgNodeDoc[]): string | null {
  const open = docs.filter((doc) => doc.todo === 'OPEN');
  open.sort((a, b) =>
    (conversationProperty(b, 'CREATED_AT') ?? '').localeCompare(conversationProperty(a, 'CREATED_AT') ?? ''),
  );
  return open[0]?.id ?? null;
}

/** Admin always continues; a member only their own. Unknown owner → let the daemon decide (403 surfaces). */
export function conversationOwnedBy(
  owner: string | null,
  viewer: { identity: MeIdentity; name: string | null },
): boolean {
  if (viewer.identity === 'admin' || !owner || !viewer.name) return true;
  return owner === JSON.stringify(['member', viewer.name]);
}

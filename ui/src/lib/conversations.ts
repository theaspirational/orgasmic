// Pure helpers for conversation nodes (CHAT-SCOPE C1). No transport imports so
// they unit-test in the node environment.
import { mediaTime } from './nodeServices';
import type { OrgNodeDoc } from './orgdoc/types';
import type { ConversationContextChip, GraphNodeSummary, MeIdentity } from './types';

export const CONVERSATION_PREFIX = 'CONV-';

const CONTEXT_OPEN = '<<<orgasmic-context';
const CONTEXT_CLOSE = '>>>';
const SELECTION_LABEL_CHARS = 40;

/** Composer/transcript chip text (CHAT-SCOPE C2): ids and ranges, never content. */
export function chipLabel(chip: ConversationContextChip): string {
  switch (chip.kind) {
    case 'node':
      return chip.id;
    case 'attachment':
      return `${chip.id}@${chip.revision.slice(0, 7)}`;
    case 'range':
      return `${mediaTime(chip.start_ms)}–${mediaTime(chip.end_ms)}`;
    case 'selection':
      return chip.text.length > SELECTION_LABEL_CHARS ? `${chip.text.slice(0, SELECTION_LABEL_CHARS)}…` : chip.text;
  }
}

export type ParsedContextBlock = {
  chips: ConversationContextChip[];
  /** The operator's own text: everything after the block. */
  message: string;
  /** Scope prompt / transcript tail the daemon put before the block, if any. */
  prefix: string;
};

/** The daemon wraps every conversation send as `<<<orgasmic-context`, one JSON
 * chip per line, `>>>`, then the operator's text. Null when the text carries
 * no complete block; malformed chip lines are skipped. */
export function parseContextBlock(text: string): ParsedContextBlock | null {
  const open = text.indexOf(CONTEXT_OPEN);
  if (open < 0) return null;
  const bodyStart = open + CONTEXT_OPEN.length;
  const close = text.indexOf(`\n${CONTEXT_CLOSE}`, bodyStart);
  if (close < 0) return null;
  const chips: ConversationContextChip[] = [];
  for (const line of text.slice(bodyStart, close).split('\n')) {
    if (!line.trim()) continue;
    try {
      const parsed = JSON.parse(line) as ConversationContextChip;
      if (parsed && typeof parsed === 'object' && typeof parsed.kind === 'string') chips.push(parsed);
    } catch {
      /* not a chip line */
    }
  }
  const messageStart = close + 1 + CONTEXT_CLOSE.length;
  return {
    chips,
    message: text.slice(messageStart).replace(/^\n/, ''),
    prefix: text.slice(0, open).trim(),
  };
}

/** `WORKTREE` shown short: the checkout's directory name. */
export function worktreeLabel(path: string): string {
  return path.split('/').filter(Boolean).at(-1) ?? path;
}

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

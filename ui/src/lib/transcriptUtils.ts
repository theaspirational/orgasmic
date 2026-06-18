// orgasmic:task_GKTWY

// --- Fix 1: Initial prompt extraction ---

export function extractPromptBundle(
  envelopes: Array<{ kind?: string; event?: Record<string, unknown> }>,
): string | null {
  for (const env of envelopes) {
    if (env.kind !== 'lifecycle') continue;
    const dc = env.event?.driver_config;
    if (typeof dc !== 'object' || dc === null || Array.isArray(dc)) continue;
    const pbt = (dc as Record<string, unknown>).prompt_bundle_text;
    if (typeof pbt === 'string' && pbt.length > 0) return pbt;
  }
  return null;
}

// --- Fix 2: Stderr routing ---

const ANSI_RE = /\x1b\[[0-9;]*[a-zA-Z]/g;

export function stripAnsi(text: string): string {
  return text.replace(ANSI_RE, '');
}

// --- Fix 3: Tool-group pairing and summary ---

export type ToolActivity = {
  summary?: string;
  meta?: Array<[string, string]>;
  raw?: string;
};

export type ToolEntry = {
  id: string;
  label: string;
  activity?: ToolActivity;
  callId?: string;
};

export type CommandStatus = 'running' | string;

export type PairedCommand = {
  callId: string;
  summary: string;
  status: CommandStatus;
  meta: Array<[string, string]>;
  raw: string;
};

export type GroupDisplayItem =
  | { type: 'paired'; command: PairedCommand }
  | { type: 'single'; entry: ToolEntry };

const COMMAND_LABELS = new Set(['command request', 'command started', 'command result', 'command error']);

export function groupToolEntries(entries: ToolEntry[]): GroupDisplayItem[] {
  // Collect all callIds that belong to exec-command triplets
  const commandCallIds = new Set<string>();
  for (const e of entries) {
    if (e.callId && (e.label === 'command request' || e.label === 'command started')) {
      commandCallIds.add(e.callId);
    }
  }

  // Build per-callId groups
  type Group = { request?: ToolEntry; started?: ToolEntry; result?: ToolEntry };
  const groups = new Map<string, Group>();
  for (const e of entries) {
    if (!e.callId || !commandCallIds.has(e.callId)) continue;
    const g = groups.get(e.callId) ?? {};
    if (e.label === 'command request') g.request = e;
    else if (e.label === 'command started') g.started = e;
    else if (e.label === 'command result' || e.label === 'command error') g.result = e;
    groups.set(e.callId, g);
  }

  // Walk in sequence order; emit one paired item for the first member of each triplet
  const emitted = new Set<string>();
  const result: GroupDisplayItem[] = [];

  for (const e of entries) {
    if (e.callId && commandCallIds.has(e.callId)) {
      if (emitted.has(e.callId)) continue;
      emitted.add(e.callId);

      const g = groups.get(e.callId)!;
      const source = g.request ?? g.started;
      const summary = source?.activity?.summary ?? 'command';

      let status: CommandStatus = 'running';
      if (g.result) {
        const m = /^exit\s+(-?\d+)/.exec(g.result.activity?.summary ?? '');
        status = m ? `exit ${m[1]}` : 'exit 0';
      }

      const meta: Array<[string, string]> = source?.activity?.meta ?? [];
      const rawParts = [g.request?.activity?.raw, g.started?.activity?.raw, g.result?.activity?.raw].filter(Boolean);
      const raw = rawParts.join('\n\n---\n\n');

      result.push({ type: 'paired', command: { callId: e.callId, summary, status, meta, raw } });
    } else if (!COMMAND_LABELS.has(e.label) || !e.callId) {
      // Unpaired entries: any non-exec entry, or exec entries without callId
      result.push({ type: 'single', entry: e });
    }
    // Skip command-label entries that have a callId but weren't the first (already emitted above)
  }

  return result;
}

// --- Fix 4: transcript block grouping (pure; tested without the component) ---

export type TranscriptRole = 'assistant' | 'user' | 'tool' | 'system' | 'work';

export type ActivityDetail = {
  label?: string;
  summary?: string;
  meta?: Array<[string, string]>;
  preview?: string;
  raw?: string;
};

export type TranscriptEntry = {
  id: string;
  role: TranscriptRole;
  label: string;
  text: string;
  time?: string;
  mergeKey?: string;
  activity?: ActivityDetail;
  callId?: string;
};

export type TranscriptBlock =
  | { type: 'entry'; id: string; entry: TranscriptEntry }
  | { type: 'tool-group'; id: string; entries: TranscriptEntry[] };

export function transcriptBlocks(entries: TranscriptEntry[]): TranscriptBlock[] {
  const blocks: TranscriptBlock[] = [];
  let pendingTools: TranscriptEntry[] = [];

  const flushTools = () => {
    if (pendingTools.length === 0) return;
    const first = pendingTools[0];
    // Key on the first entry only. The group's identity must stay stable as
    // tool calls stream in and append to it; including the last entry's id
    // would change the key on every new call, remounting the <details> and
    // discarding the user's manual expand (uncontrolled DOM open state).
    blocks.push({
      type: 'tool-group',
      id: `${first.id}:tools`,
      entries: pendingTools,
    });
    pendingTools = [];
  };

  for (const entry of entries) {
    if (entry.role === 'tool') {
      pendingTools.push(entry);
      continue;
    }
    flushTools();
    blocks.push({ type: 'entry', id: entry.id, entry });
  }

  flushTools();
  return blocks;
}

function cloneTranscriptEntry(entry: TranscriptEntry): TranscriptEntry {
  return {
    ...entry,
    activity: entry.activity
      ? {
          ...entry.activity,
          meta: entry.activity.meta ? [...entry.activity.meta] : undefined,
        }
      : undefined,
  };
}

function appendText(left: string | undefined, right: string | undefined): string | undefined {
  if (left === undefined && right === undefined) return undefined;
  return `${left ?? ''}${right ?? ''}`;
}

export function coalesceTextChunks(entries: TranscriptEntry[]): TranscriptEntry[] {
  const coalesced: TranscriptEntry[] = [];
  for (const entry of entries) {
    const previous = coalesced[coalesced.length - 1];
    if (
      previous?.mergeKey &&
      entry.mergeKey &&
      previous.mergeKey === entry.mergeKey &&
      previous.role === entry.role &&
      previous.label === entry.label
    ) {
      previous.text += entry.text;
      previous.time = entry.time ?? previous.time;
      if (previous.mergeKey === 'tool:stderr-diagnostics' && previous.activity && entry.activity) {
        previous.activity.preview = appendText(previous.activity.preview, entry.activity.preview ?? entry.text);
        previous.activity.raw = appendText(previous.activity.raw, entry.activity.raw ?? entry.text);
      }
      continue;
    }
    coalesced.push(cloneTranscriptEntry(entry));
  }
  return coalesced.map(({ mergeKey: _mergeKey, ...entry }) => entry);
}

// Returns summary text for the group header: the most recent command text.
// Prefers "command request" label (clean "run cmd" format) before "command started".
export function selectGroupSummary(entries: ToolEntry[]): string {
  for (let i = entries.length - 1; i >= 0; i--) {
    if (entries[i].label === 'command request') {
      const s = entries[i].activity?.summary?.trim();
      if (s) return s;
    }
  }
  for (let i = entries.length - 1; i >= 0; i--) {
    if (entries[i].label === 'command started') {
      const s = entries[i].activity?.summary?.trim();
      if (s) return s;
    }
  }
  for (let i = entries.length - 1; i >= 0; i--) {
    const s = entries[i].activity?.summary?.trim();
    if (s) return s;
  }
  return 'tool calls';
}

import { describe, expect, it } from 'vitest';

import {
  conversationOwnedBy,
  conversationRuns,
  newestOpenConversation,
  orderConversations,
  parseConversationRuns,
  segmentLabel,
} from '../conversations';
import type { OrgNodeDoc } from '../orgdoc/types';

function doc(id: string, todo: string | null, properties: Record<string, string>): OrgNodeDoc {
  return {
    id, kind: 'conversations', title: id, todo, tags: [], body: '', sections: [],
    properties: Object.entries(properties).map(([key, value]) => ({ key, value })),
    source: { file: `conversations/${id}/node.org`, base_version: 'v1' },
  };
}

describe('conversation RUNS', () => {
  it('parses oldest-first entries with resumed and cold suffixes', () => {
    expect(parseConversationRuns('run-01J9X2 run-01J9Y7:resumed  run-01J9Z1:cold\n')).toEqual([
      { runId: 'run-01J9X2', mode: 'start' },
      { runId: 'run-01J9Y7', mode: 'resumed' },
      { runId: 'run-01J9Z1', mode: 'cold' },
    ]);
    expect(parseConversationRuns(null)).toEqual([]);
    // Unknown suffixes stay part of the id rather than becoming a mode.
    expect(parseConversationRuns('run-a:other')).toEqual([{ runId: 'run-a:other', mode: 'start' }]);
  });

  it('reads RUNS off the node document and labels each segment', () => {
    const runs = conversationRuns(doc('CONV-1', 'OPEN', { RUNS: 'run-a run-b:cold' }));
    expect(runs.map((run) => segmentLabel(run.mode))).toEqual(['Started', 'Continued without native memory']);
    expect(segmentLabel('resumed')).toBe('Resumed');
    expect(conversationRuns(null)).toEqual([]);
  });
});

describe('conversation list', () => {
  it('puts OPEN conversations first and marks the ones with a live run', () => {
    const entries = orderConversations(
      [
        { id: 'CONV-A', title: 'Archived first', todo: 'ARCHIVED' },
        { id: 'CONV-B', title: '', todo: 'OPEN' },
        { id: 'CONV-C', title: 'Open live', todo: 'OPEN' },
      ],
      ['CONV-C', 'TASK-1'],
    );
    expect(entries).toEqual([
      { id: 'CONV-B', title: 'CONV-B', todo: 'OPEN', live: false },
      { id: 'CONV-C', title: 'Open live', todo: 'OPEN', live: true },
      { id: 'CONV-A', title: 'Archived first', todo: 'ARCHIVED', live: false },
    ]);
  });

  it('picks the newest OPEN conversation about a node, or none', () => {
    expect(newestOpenConversation([
      doc('CONV-OLD', 'OPEN', { CREATED_AT: '2026-09-01T10:00:00Z' }),
      doc('CONV-ARCHIVED', 'ARCHIVED', { CREATED_AT: '2026-09-09T10:00:00Z' }),
      doc('CONV-NEW', 'OPEN', { CREATED_AT: '2026-09-08T10:00:00Z' }),
    ])).toBe('CONV-NEW');
    expect(newestOpenConversation([doc('CONV-ARCHIVED', 'ARCHIVED', {})])).toBeNull();
    expect(newestOpenConversation([])).toBeNull();
  });
});

describe('conversation ownership', () => {
  it('lets admins and owners continue, blocks other members, defers unknowns to the daemon', () => {
    const owner = JSON.stringify(['member', 'anna']);
    expect(conversationOwnedBy(owner, { identity: 'admin', name: null })).toBe(true);
    expect(conversationOwnedBy(owner, { identity: 'member', name: 'anna' })).toBe(true);
    expect(conversationOwnedBy(owner, { identity: 'member', name: 'bob' })).toBe(false);
    expect(conversationOwnedBy(null, { identity: 'member', name: 'bob' })).toBe(true);
    expect(conversationOwnedBy(owner, { identity: 'member', name: null })).toBe(true);
  });
});

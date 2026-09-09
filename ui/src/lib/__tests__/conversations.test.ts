import { describe, expect, it } from 'vitest';

import {
  chipLabel,
  conversationOwnedBy,
  conversationRuns,
  newestOpenConversation,
  orderConversations,
  parseConversationRuns,
  parseContextBlock,
  segmentLabel,
  worktreeLabel,
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

describe('context chips', () => {
  it('labels chips by id, attachment@revision, time range, or the first 40 characters', () => {
    expect(chipLabel({ kind: 'node', id: 'dec_1' })).toBe('dec_1');
    expect(chipLabel({ kind: 'attachment', node: 'MEET-1', id: 'rec', revision: 'abcdef0123456789' })).toBe('rec@abcdef0');
    expect(chipLabel({ kind: 'range', node: 'MEET-1', attachment: 'rec', revision: 'sha', start_ms: 90000, end_ms: 120000 })).toBe('1:30–2:00');
    expect(chipLabel({ kind: 'selection', text: 'short' })).toBe('short');
    expect(chipLabel({ kind: 'selection', text: 'x'.repeat(41) })).toBe(`${'x'.repeat(40)}…`);
  });

  it('parses the daemon context block out of a sent message', () => {
    const block = [
      'scope prompt', '', '<<<orgasmic-context',
      '{"kind":"node","id":"dec_1"}',
      '{"kind":"range","node":"MEET-1","attachment":"rec","revision":"sha","start_ms":0,"end_ms":30000}',
      '>>>', 'hello there',
    ].join('\n');
    expect(parseContextBlock(block)).toEqual({
      chips: [
        { kind: 'node', id: 'dec_1' },
        { kind: 'range', node: 'MEET-1', attachment: 'rec', revision: 'sha', start_ms: 0, end_ms: 30000 },
      ],
      message: 'hello there',
      prefix: 'scope prompt',
    });
    // Always-present block, no chips.
    expect(parseContextBlock('<<<orgasmic-context\n>>>\nplain')).toEqual({ chips: [], message: 'plain', prefix: '' });
  });

  it('anchors on the last block when the scope prompt quotes the delimiter', () => {
    const prompt = [
      'Messages carry a delimited <<<orgasmic-context block naming ids.',
      '{"kind":"node","id":"NOT-A-CHIP"}',
      '>>>',
      'more prompt',
    ].join('\n');
    const sent = `${prompt}\n\n<<<orgasmic-context\n{"kind":"node","id":"MEET-1"}\n>>>\nwhat was decided?`;
    expect(parseContextBlock(sent)).toEqual({
      chips: [{ kind: 'node', id: 'MEET-1' }],
      message: 'what was decided?',
      prefix: prompt,
    });
  });

  it('leaves text without a complete block alone and skips malformed chip lines', () => {
    expect(parseContextBlock('just a message')).toBeNull();
    expect(parseContextBlock('<<<orgasmic-context\n{"kind":"node","id":"dec_1"}\nnever closed')).toBeNull();
    expect(parseContextBlock('<<<orgasmic-context\nnot json\n{"no":"kind"}\n{"kind":"node","id":"dec_1"}\n>>>\nok')).toEqual({
      chips: [{ kind: 'node', id: 'dec_1' }], message: 'ok', prefix: '',
    });
  });

  it('shortens a worktree path to its directory name', () => {
    expect(worktreeLabel('/tmp/wt/sprint-TASK-1')).toBe('sprint-TASK-1');
    expect(worktreeLabel('/tmp/wt/sprint-TASK-1/')).toBe('sprint-TASK-1');
    expect(worktreeLabel('relative')).toBe('relative');
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

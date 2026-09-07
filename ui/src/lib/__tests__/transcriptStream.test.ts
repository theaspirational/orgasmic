import { describe, expect, it } from 'vitest';
import { TranscriptStream } from '../transcriptStream';
import {
  normalizeTranscriptParts,
  type SessionEnvelope,
} from '../transcriptParts';

const rows: SessionEnvelope[] = [
  {
    kind: 'lifecycle',
    event: {
      phase: 'run_meta',
      driver_config: { prompt_bundle_text: 'Initial prompt' },
    },
  },
  {
    event: {
      type: 'provider_runtime',
      event: { type: 'session.started', payload: {} },
    },
  },
  { kind: 'lifecycle', event: { phase: 'composer_send', text: 'Hello' } },
  { event: { type: 'text_chunk', stream: 'user', chunk: 'Hello' } },
  {
    event: {
      type: 'provider_runtime',
      event: {
        type: 'content.delta',
        itemId: 'r',
        payload: { streamKind: 'reasoning_text', delta: 'Thinking' },
      },
    },
  },
  {
    event: {
      type: 'provider_runtime',
      event: {
        type: 'content.delta',
        itemId: 'a',
        payload: { streamKind: 'assistant_text', delta: 'Before ' },
      },
    },
  },
  {
    event: {
      type: 'provider_runtime',
      event: {
        type: 'content.delta',
        itemId: 'a',
        payload: { streamKind: 'assistant_text', delta: 'tool' },
      },
    },
  },
  {
    event: {
      type: 'provider_runtime',
      event: {
        type: 'item.started',
        itemId: 't',
        payload: {
          itemType: 'command_execution',
          data: { input: { command: 'pwd' } },
        },
      },
    },
  },
  {
    event: {
      type: 'provider_runtime',
      event: {
        type: 'item.completed',
        itemId: 't',
        payload: { itemType: 'command_execution', data: { output: '/tmp' } },
      },
    },
  },
  {
    event: {
      type: 'provider_runtime',
      event: {
        type: 'content.delta',
        itemId: 'b',
        payload: { streamKind: 'assistant_text', delta: 'After tool' },
      },
    },
  },
  {
    event: {
      type: 'provider_runtime',
      event: { type: 'turn.completed', payload: { state: 'completed' } },
    },
  },
].map((e, seq) => ({
  kind: 'driver_event',
  ...e,
  seq,
  time: '2026-09-07T10:00:00Z',
}));

describe('incremental transcript', () => {
  it('matches full replay for every batch size, including duplicate delivery and reconnect', () => {
    const expected = normalizeTranscriptParts(
      rows.map((row) => JSON.stringify(row)).join('\n'),
    );
    for (let size = 1; size <= rows.length; size++) {
      const stream = new TranscriptStream();
      for (let i = 0; i < rows.length; i += size) {
        const batch = rows.slice(i, i + size);
        stream.append(batch);
        stream.append(batch);
      }
      expect(stream.parts()).toEqual(expected);
      stream.reset(rows);
      expect(stream.parts()).toEqual(expected);
    }
  });
  it('refuses gaps without advancing state, then recovers from snapshot', () => {
    const stream = new TranscriptStream();
    stream.append(rows.slice(0, 3));
    const before = stream.parts();
    expect(() => stream.append(rows.slice(4, 6))).toThrow(/gap/);
    expect(stream.parts()).toEqual(before);
    stream.reset(rows);
    expect(
      stream
        .parts()
        .filter((p) => p.type === 'text')
        .map((p) => p.text),
    ).toEqual(['Initial prompt', 'Hello', 'Before tool', 'After tool']);
  });
});

const captures = import.meta.glob('./fixtures/acp/*.jsonl', {
  query: '?raw',
  import: 'default',
  eager: true,
});
for (const [path, source] of Object.entries(captures)) {
  it(`replays real ${path} without losing prose, reasoning, or tool identities`, () => {
    const events: SessionEnvelope[] = (source as string)
      .trim()
      .split('\n')
      .map((line) => JSON.parse(line));
    const expected = normalizeTranscriptParts(source as string);
    const stream = new TranscriptStream();
    for (const event of events) stream.append([event, event]);
    expect(stream.parts()).toEqual(expected);
    const updates = events
      .map((e) => (e.event?.message as any)?.params?.update)
      .filter(Boolean);
    for (const [kind, partType] of [
      ['agent_message_chunk', 'text'],
      ['agent_thought_chunk', 'reasoning'],
    ]) {
      const raw = updates
        .filter((u) => u.sessionUpdate === kind && u.content.type === 'text')
        .map((u) => u.content.text)
        .join('');
      const displayed = expected
        .filter(
          (p) =>
            p.type === partType &&
            (p.type !== 'text' || p.role === 'assistant'),
        )
        .map((p) => ('text' in p ? p.text : ''))
        .join('');
      expect(displayed).toBe(raw);
    }
    for (const update of updates) {
      const output = update.rawOutput;
      const exit =
        output?.exitCode ?? output?.exit_code ?? output?.metadata?.exit;
      if (
        update.status === 'failed' ||
        (typeof exit === 'number' && exit !== 0)
      ) {
        expect(
          expected.find((p) => p.type === 'tool' && p.id === update.toolCallId),
        ).toMatchObject({ state: 'error' });
      }
    }
    const toolIds = new Set(
      updates
        .filter((u) => u.sessionUpdate === 'tool_call')
        .map((u) => u.toolCallId),
    );
    for (const id of toolIds)
      expect(expected.some((p) => p.type === 'tool' && p.id === id)).toBe(true);
    expect(
      expected.filter(
        (p) => p.type === 'tool' && ['running', 'streaming'].includes(p.state),
      ),
    ).toHaveLength(0);
  });
}

it('accepts compacted snapshot gaps but detects gaps in live delivery', () => {
  const stream = new TranscriptStream();
  stream.reset([rows[0], rows[3]]);
  stream.append([rows[7]], false, true);
  expect(() => stream.append([rows[9]])).toThrow(/gap/);
  stream.append([rows[8], rows[8], rows[9]]);
});

it('retains unknown future updates and non-text content for inspection', () => {
  const stream = new TranscriptStream();
  stream.reset(
    ['future_vendor_update', 'agent_message_chunk'].map(
      (sessionUpdate, seq) => ({
        seq,
        event: {
          type: 'acp',
          message: {
            method: 'session/update',
            params: {
              update: {
                sessionUpdate,
                extra: 'retained',
                content: {
                  type: 'image',
                  mimeType: 'image/png',
                  data: 'example',
                },
              },
            },
          },
        },
      }),
    ),
  );
  expect(stream.parts()).toHaveLength(2);
  expect(
    stream
      .parts()
      .map((p) => ('fullText' in p ? p.fullText : ''))
      .join(''),
  ).toContain('retained');
  expect(
    stream
      .parts()
      .map((p) => ('fullText' in p ? p.fullText : ''))
      .join(''),
  ).toContain('image/png');
});

it('uses delivery order when a reopened session writer restarts its own sequence', () => {
  const stream = new TranscriptStream();
  stream.reset([
    {
      seq: 0,
      delivery_seq: 0,
      event: { type: 'text_chunk', stream: 'assistant', chunk: 'First. ' },
    },
  ]);
  stream.append([
    {
      seq: 0,
      delivery_seq: 1,
      event: { type: 'text_chunk', stream: 'assistant', chunk: 'Second.' },
    },
  ]);
  expect(
    stream
      .parts()
      .filter((p) => p.type === 'text')
      .map((p) => p.text)
      .join(''),
  ).toContain('Second.');
});

it('does not duplicate vendor user echoes or misclassify vendor reasoning as warnings', () => {
  const stream = new TranscriptStream();
  stream.reset([
    {
      seq: 0,
      event: { type: 'text_chunk', stream: 'user', chunk: 'Question' },
    },
    {
      seq: 1,
      event: {
        type: 'acp',
        message: {
          method: 'session/update',
          params: {
            update: {
              sessionUpdate: 'user_message_chunk',
              content: { type: 'text', text: 'Question' },
            },
          },
        },
      },
    },
    {
      seq: 2,
      event: {
        type: 'acp',
        message: {
          method: 'session/update',
          params: {
            update: {
              sessionUpdate: 'agent_thought_chunk',
              content: {
                type: 'text',
                text: 'Warning: consider this carefully.',
              },
            },
          },
        },
      },
    },
  ]);
  expect(stream.parts().filter((p) => p.type === 'text')).toHaveLength(1);
  expect(
    stream
      .parts()
      .filter((p) => p.type === 'reasoning')
      .map((p) => p.text),
  ).toEqual(['Warning: consider this carefully.']);
});

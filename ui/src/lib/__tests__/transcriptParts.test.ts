import { describe, expect, it } from 'vitest';

import {
  hasResponseAfterPending,
  normalizeTranscriptParts,
  type SessionEnvelope,
  type TranscriptToolPart,
} from '../transcriptParts';

function source(...envelopes: SessionEnvelope[]): string {
  return envelopes.map((envelope) => JSON.stringify(envelope)).join('\n');
}

function event(
  seq: number,
  payload: Record<string, unknown>,
  time = `2026-07-16T10:00:${String(seq).padStart(2, '0')}Z`,
): SessionEnvelope {
  return { seq, time, kind: 'driver_event', event: payload };
}

// Copied from dispatch-TASK-P0FAQ-implementer-20260716T190738.jsonl with
// paths and payload text shortened. Codex emits the outer `exec` call plus
// command_execution/file_change item-started calls without matching results.
const realCodexStartedItems: SessionEnvelope[] = [
  event(3, {
    args: 'const result = await tools.exec_command({ cmd: "orgasmic entry" });',
    call_id: 'call_1igTc3Sv101HjM2369zFVhiv',
    name: 'exec',
    seq: 0,
    type: 'tool_call',
  }),
  event(4, {
    args: {
      aggregatedOutput: null,
      command: "/bin/zsh -lc 'orgasmic entry'",
      commandActions: [{ command: 'orgasmic entry', type: 'unknown' }],
      cwd: '/repo',
      durationMs: null,
      exitCode: null,
      id: 'exec-5372ffcc-d1af-4af5-af17-33edbb97a9f2',
      processId: '33050',
      source: 'unifiedExecStartup',
      status: 'inProgress',
      type: 'commandExecution',
    },
    call_id: 'exec-5372ffcc-d1af-4af5-af17-33edbb97a9f2',
    name: 'command_execution',
    seq: 1,
    type: 'tool_call',
  }),
  event(234, {
    args: {
      changes: [
        {
          diff: '@@ -1 +1 @@\n-old\n+new',
          kind: { move_path: null, type: 'update' },
          path: '/repo/ui/src/components/ai-elements/tool.tsx',
        },
      ],
      id: 'exec-9ff061ee-f824-447a-863e-5fc35022ed33',
      status: 'inProgress',
      type: 'fileChange',
    },
    call_id: 'exec-9ff061ee-f824-447a-863e-5fc35022ed33',
    name: 'file_change',
    seq: 110,
    type: 'tool_call',
  }),
];

describe('normalizeTranscriptParts', () => {
  it('renders canonical provider events without telemetry or duplicate terminal summaries', () => {
    const parts = normalizeTranscriptParts(
      source(
        event(1, {
          type: 'provider_runtime',
          event: {
            eventId: 'evt-1',
            provider: 'claude',
            threadId: 'thread-1',
            createdAt: '2026-08-15T10:00:00Z',
            turnId: 'turn-1',
            itemId: 'answer-1',
            type: 'content.delta',
            payload: { streamKind: 'assistant_text', delta: 'Done.' },
          },
        }),
        event(2, {
          type: 'provider_runtime',
          event: {
            eventId: 'evt-2',
            provider: 'claude',
            threadId: 'thread-1',
            createdAt: '2026-08-15T10:00:01Z',
            type: 'account.rate-limits.updated',
            payload: { rateLimits: { type: 'five_hour' } },
          },
        }),
        event(3, {
          type: 'provider_runtime',
          event: {
            eventId: 'evt-3',
            provider: 'claude',
            threadId: 'thread-1',
            createdAt: '2026-08-15T10:00:02Z',
            turnId: 'turn-1',
            type: 'turn.completed',
            payload: { state: 'completed' },
          },
        }),
        event(4, { type: 'run_complete', summary: null }),
      ),
    );

    expect(parts).toEqual([
      expect.objectContaining({ type: 'text', role: 'assistant', text: 'Done.' }),
    ]);
  });

  it('coalesces canonical streamed assistant deltas that do not carry an item id', () => {
    const parts = normalizeTranscriptParts(
      source(
        event(1, {
          type: 'provider_runtime',
          event: {
            eventId: 'delta-1',
            provider: 'codex',
            threadId: 'thread-1',
            turnId: 'turn-1',
            createdAt: '2026-08-15T10:00:00Z',
            type: 'content.delta',
            payload: { streamKind: 'assistant_text', delta: 'No files' },
          },
        }),
        event(2, {
          type: 'provider_runtime',
          event: {
            eventId: 'delta-2',
            provider: 'codex',
            threadId: 'thread-1',
            turnId: 'turn-1',
            createdAt: '2026-08-15T10:00:01Z',
            type: 'content.delta',
            payload: { streamKind: 'assistant_text', delta: ' were changed.' },
          },
        }),
      ),
    );

    expect(parts).toEqual([
      expect.objectContaining({ type: 'text', role: 'assistant', text: 'No files were changed.' }),
    ]);
  });

  it('uses the canonical content index to keep separated assistant segments uniquely keyed', () => {
    const parts = normalizeTranscriptParts(
      source(
        event(1, {
          type: 'provider_runtime',
          event: {
            eventId: 'delta-1',
            provider: 'claude',
            threadId: 'thread-1',
            turnId: 'turn-1',
            createdAt: '2026-08-15T10:00:00Z',
            type: 'content.delta',
            payload: { streamKind: 'assistant_text', contentIndex: 0, delta: 'Before tool.' },
          },
        }),
        event(2, {
          type: 'provider_runtime',
          event: {
            eventId: 'tool-started',
            provider: 'claude',
            threadId: 'thread-1',
            turnId: 'turn-1',
            itemId: 'tool-1',
            createdAt: '2026-08-15T10:00:01Z',
            type: 'item.started',
            payload: {
              itemType: 'file_read',
              status: 'inProgress',
              title: 'Read',
              data: { toolName: 'Read', input: { path: '/repo/README.md' } },
            },
          },
        }),
        event(3, {
          type: 'provider_runtime',
          event: {
            eventId: 'delta-2',
            provider: 'claude',
            threadId: 'thread-1',
            turnId: 'turn-1',
            createdAt: '2026-08-15T10:00:02Z',
            type: 'content.delta',
            payload: { streamKind: 'assistant_text', contentIndex: 2, delta: 'After tool.' },
          },
        }),
      ),
    );

    const assistantParts = parts.filter(
      (part) => part.type === 'text' && part.role === 'assistant',
    );
    expect(assistantParts.map((part) => part.id)).toEqual([
      'assistant_text:thread-1:turn-1:0',
      'assistant_text:thread-1:turn-1:2',
    ]);
  });

  it('accumulates Claude tool input deltas into the completed canonical tool', () => {
    const parts = normalizeTranscriptParts(
      source(
        event(1, {
          type: 'provider_runtime',
          event: {
            eventId: 'tool-started',
            provider: 'claude',
            threadId: 'thread-1',
            turnId: 'turn-1',
            itemId: 'tool-1',
            createdAt: '2026-08-15T10:00:00Z',
            type: 'item.started',
            payload: {
              itemType: 'command_execution',
              status: 'inProgress',
              title: 'Bash',
              data: { toolName: 'Bash', input: {} },
            },
          },
        }),
        event(2, {
          type: 'provider_runtime',
          event: {
            eventId: 'tool-delta-1',
            provider: 'claude',
            threadId: 'thread-1',
            turnId: 'turn-1',
            itemId: 'tool-1',
            createdAt: '2026-08-15T10:00:01Z',
            type: 'item.updated',
            payload: {
              itemType: 'command_execution',
              status: 'inProgress',
              title: 'Bash',
              data: { toolName: 'Bash', inputDelta: '{"command":' },
            },
          },
        }),
        event(3, {
          type: 'provider_runtime',
          event: {
            eventId: 'tool-delta-2',
            provider: 'claude',
            threadId: 'thread-1',
            turnId: 'turn-1',
            itemId: 'tool-1',
            createdAt: '2026-08-15T10:00:02Z',
            type: 'item.updated',
            payload: {
              itemType: 'command_execution',
              status: 'inProgress',
              title: 'Bash',
              data: { toolName: 'Bash', inputDelta: '"pwd"}' },
            },
          },
        }),
        event(4, {
          type: 'provider_runtime',
          event: {
            eventId: 'tool-completed',
            provider: 'claude',
            threadId: 'thread-1',
            turnId: 'turn-1',
            itemId: 'tool-1',
            createdAt: '2026-08-15T10:00:03Z',
            type: 'item.completed',
            payload: {
              itemType: 'command_execution',
              status: 'completed',
              title: 'Bash',
              data: { toolName: 'Bash', input: {}, result: 'ok' },
            },
          },
        }),
      ),
    );

    expect(parts).toEqual([
      expect.objectContaining({
        type: 'tool',
        input: { command: 'pwd' },
        summary: 'pwd',
        state: 'completed',
      }),
    ]);
  });

  it('completes canonical tools when status is omitted or the turn closes', () => {
    const completedWithoutStatus = normalizeTranscriptParts(
      source(
        event(1, {
          type: 'provider_runtime',
          event: {
            eventId: 'completed',
            provider: 'claude',
            threadId: 'thread-1',
            turnId: 'turn-1',
            itemId: 'tool-1',
            createdAt: '2026-08-15T10:00:00Z',
            type: 'item.completed',
            payload: { itemType: 'file_read', title: 'Read', data: { input: { path: '/tmp/a' } } },
          },
        }),
      ),
    );
    expect(completedWithoutStatus).toEqual([
      expect.objectContaining({ type: 'tool', state: 'completed', ok: true }),
    ]);

    const closedWhileStreaming = normalizeTranscriptParts(
      source(
        event(1, {
          type: 'provider_runtime',
          event: {
            eventId: 'updated',
            provider: 'claude',
            threadId: 'thread-1',
            turnId: 'turn-1',
            itemId: 'tool-2',
            createdAt: '2026-08-15T10:00:00Z',
            type: 'item.updated',
            payload: { itemType: 'file_read', title: 'Read', data: { input: { path: '/tmp/b' } } },
          },
        }),
        event(2, {
          type: 'provider_runtime',
          event: {
            eventId: 'turn-completed',
            provider: 'claude',
            threadId: 'thread-1',
            turnId: 'turn-1',
            createdAt: '2026-08-15T10:00:01Z',
            type: 'turn.completed',
            payload: { state: 'completed' },
          },
        }),
      ),
    );
    expect(closedWhileStreaming).toEqual([
      expect.objectContaining({ type: 'tool', state: 'completed' }),
    ]);
  });

  it('merges Codex exec completion into its detailed command activity', () => {
    const parts = normalizeTranscriptParts(
      source(
        event(1, {
          type: 'provider_runtime',
          event: {
            eventId: 'exec-started',
            provider: 'codex',
            threadId: 'thread-1',
            turnId: 'turn-1',
            itemId: 'call-exec',
            createdAt: '2026-08-15T10:00:00Z',
            type: 'item.started',
            payload: {
              itemType: 'exec',
              status: 'inProgress',
              title: 'exec',
              data: { toolName: 'exec', input: 'const result = await tools.exec_command(...)' },
            },
          },
        }),
        event(2, {
          type: 'provider_runtime',
          event: {
            eventId: 'command-started',
            provider: 'codex',
            threadId: 'thread-1',
            turnId: 'turn-1',
            itemId: 'command-exec-1',
            createdAt: '2026-08-15T10:00:01Z',
            type: 'item.started',
            payload: {
              itemType: 'command_execution',
              status: 'inProgress',
              title: 'command_execution',
              data: {
                toolName: 'command_execution',
                input: { command: 'pwd', status: 'inProgress', type: 'commandExecution' },
              },
            },
          },
        }),
        event(3, {
          type: 'provider_runtime',
          event: {
            eventId: 'exec-completed',
            provider: 'codex',
            threadId: 'thread-1',
            turnId: 'turn-1',
            itemId: 'call-exec',
            createdAt: '2026-08-15T10:00:02Z',
            type: 'item.completed',
            payload: {
              itemType: 'tool_call',
              status: 'completed',
              title: 'Tool',
              data: { output: ['/repo'], ok: true },
            },
          },
        }),
      ),
    );

    expect(parts).toEqual([
      expect.objectContaining({
        type: 'tool',
        callId: 'call-exec',
        name: 'command_execution',
        label: 'Ran command',
        state: 'completed',
        input: { command: 'pwd', status: 'inProgress', type: 'commandExecution' },
        output: ['/repo'],
        ok: true,
        summary: 'pwd',
      }),
    ]);
  });

  it('preserves a canonical tool name when a generic completion arrives', () => {
    const parts = normalizeTranscriptParts(
      source(
        event(1, {
          type: 'provider_runtime',
          event: {
            eventId: 'wait-started',
            provider: 'codex',
            threadId: 'thread-1',
            turnId: 'turn-1',
            itemId: 'call-wait',
            createdAt: '2026-08-15T10:00:00Z',
            type: 'item.started',
            payload: {
              itemType: 'wait',
              status: 'inProgress',
              title: 'wait',
              data: { toolName: 'wait', input: { yield_time_ms: 30_000 } },
            },
          },
        }),
        event(2, {
          type: 'provider_runtime',
          event: {
            eventId: 'wait-completed',
            provider: 'codex',
            threadId: 'thread-1',
            turnId: 'turn-1',
            itemId: 'call-wait',
            createdAt: '2026-08-15T10:00:30Z',
            type: 'item.completed',
            payload: { itemType: 'tool_call', status: 'completed', title: 'Tool', data: {} },
          },
        }),
      ),
    );

    expect(parts).toEqual([
      expect.objectContaining({ type: 'tool', name: 'wait', label: 'wait', state: 'completed' }),
    ]);
  });

  it('keeps canonical app-server stderr out of the conversational transcript', () => {
    const parts = normalizeTranscriptParts(
      source(
        event(1, {
          type: 'provider_runtime',
          event: {
            eventId: 'session-started',
            provider: 'codex',
            threadId: 'thread-1',
            createdAt: '2026-08-15T10:00:00Z',
            type: 'session.started',
            payload: {},
          },
        }),
        event(2, {
          type: 'text_chunk',
          stream: 'stderr',
          chunk: 'ERROR codex_models_manager::cache: failed to renew cache TTL',
        }),
      ),
    );

    expect(parts).toEqual([]);
  });

  it('pairs canonical driver user echoes with composer sends without collapsing repeated messages', () => {
    const parts = normalizeTranscriptParts(
      source(
        event(1, { type: 'text_chunk', stream: 'user', chunk: 'hi' }),
        { seq: 2, kind: 'lifecycle', event: { phase: 'composer_send', text: 'hi' } },
        event(3, {
          type: 'provider_runtime',
          event: {
            eventId: 'event-1',
            provider: 'opencode',
            threadId: 'thread-1',
            createdAt: '2026-08-15T10:00:00Z',
            type: 'session.started',
            payload: {},
          },
        }),
        event(4, { type: 'text_chunk', stream: 'user', chunk: 'hi' }),
        { seq: 5, kind: 'lifecycle', event: { phase: 'composer_send', text: 'hi' } },
      ),
    );

    expect(
      parts
        .filter((part): part is Extract<typeof part, { type: 'text' }> => part.type === 'text')
        .filter((part) => part.role === 'user')
        .map((part) => part.text),
    ).toEqual(['hi', 'hi']);
  });

  it('keeps an unmatched canonical driver user echo as a persistence fallback', () => {
    const parts = normalizeTranscriptParts(
      source(
        event(1, { type: 'text_chunk', stream: 'user', chunk: 'keep me' }),
        event(2, {
          type: 'provider_runtime',
          event: {
            eventId: 'event-1',
            provider: 'opencode',
            threadId: 'thread-1',
            createdAt: '2026-08-15T10:00:00Z',
            type: 'session.started',
            payload: {},
          },
        }),
      ),
    );

    expect(parts).toEqual([
      expect.objectContaining({ type: 'text', role: 'user', text: 'keep me' }),
    ]);
  });

  it('normalizes a real OpenCode command lifecycle into a compact command tool', () => {
    const output = 'ORGASMIC_ENTRY_FAST_V1 abc123\nPROJECT /repo\n';
    const parts = normalizeTranscriptParts(
      source(
        event(1, {
          type: 'provider_runtime',
          event: {
            eventId: 'event-started',
            provider: 'opencode',
            threadId: 'thread-1',
            turnId: 'turn-1',
            itemId: 'call-1',
            createdAt: '2026-08-15T10:00:00Z',
            type: 'item.started',
            payload: {
              itemType: 'command_execution',
              status: 'inProgress',
              title: 'bash',
              data: { tool: 'bash', state: { input: {}, raw: '', status: 'pending' } },
            },
          },
        }),
        event(2, {
          type: 'provider_runtime',
          event: {
            eventId: 'event-completed',
            provider: 'opencode',
            threadId: 'thread-1',
            turnId: 'turn-1',
            itemId: 'call-1',
            createdAt: '2026-08-15T10:00:01Z',
            type: 'item.completed',
            payload: {
              itemType: 'command_execution',
              status: 'completed',
              title: 'orgasmic entry',
              detail: output,
              data: {
                tool: 'bash',
                state: {
                  input: { command: 'orgasmic entry' },
                  metadata: { exit: 0, output, truncated: false },
                  output,
                  status: 'completed',
                  title: 'orgasmic entry',
                },
              },
            },
          },
        }),
      ),
    );

    expect(parts).toEqual([
      expect.objectContaining({
        type: 'tool',
        callId: 'call-1',
        name: 'command_execution',
        label: 'Ran command',
        state: 'completed',
        input: { command: 'orgasmic entry' },
        output,
        ok: true,
        summary: 'orgasmic entry',
      }),
    ]);
  });

  it('keeps a real OpenCode web fetch result out of the tool header', () => {
    const url = 'https://opencode.ai/docs/providers/';
    const output = 'Providers | OpenCode\n\nLarge fetched document body';
    const parts = normalizeTranscriptParts(
      source(
        event(1, {
          type: 'provider_runtime',
          event: {
            eventId: 'event-started',
            provider: 'opencode',
            threadId: 'thread-1',
            turnId: 'turn-1',
            itemId: 'call-webfetch',
            createdAt: '2026-08-15T10:00:00Z',
            type: 'item.started',
            payload: {
              itemType: 'web_search',
              status: 'inProgress',
              title: 'webfetch',
              data: { tool: 'webfetch', state: { input: {}, raw: '', status: 'pending' } },
            },
          },
        }),
        event(2, {
          type: 'provider_runtime',
          event: {
            eventId: 'event-completed',
            provider: 'opencode',
            threadId: 'thread-1',
            turnId: 'turn-1',
            itemId: 'call-webfetch',
            createdAt: '2026-08-15T10:00:01Z',
            type: 'item.completed',
            payload: {
              itemType: 'web_search',
              status: 'completed',
              title: `${url} (text/html)`,
              detail: output,
              data: {
                tool: 'webfetch',
                state: {
                  input: { url },
                  output,
                  output_bounded: { bytes: 51_453, retained_bytes: 2_048 },
                  status: 'completed',
                  title: `${url} (text/html)`,
                },
              },
            },
          },
        }),
      ),
    );

    expect(parts).toEqual([
      expect.objectContaining({
        type: 'tool',
        callId: 'call-webfetch',
        name: 'web_search',
        label: 'Fetched page',
        state: 'completed',
        input: { url },
        output,
        ok: true,
        summary: url,
      }),
    ]);
  });

  it('coalesces adjacent text chunks by stream while preserving role and the latest time', () => {
    const parts = normalizeTranscriptParts(
      source(
        event(1, { type: 'text_chunk', stream: 'assistant', chunk: 'Hello ' }),
        event(2, { type: 'text_chunk', stream: 'assistant', chunk: 'world' }),
        event(3, { type: 'text_chunk', stream: 'user', chunk: 'Thanks' }),
      ),
    );

    expect(parts).toHaveLength(2);
    expect(parts[0]).toMatchObject({
      type: 'text',
      role: 'assistant',
      text: 'Hello world',
      time: '2026-07-16T10:00:02Z',
    });
    expect(parts[1]).toMatchObject({ type: 'text', role: 'user', text: 'Thanks' });
  });

  it('maps system thought chunks to reasoning and completes them when visible output follows', () => {
    const parts = normalizeTranscriptParts(
      source(
        event(1, { type: 'text_chunk', stream: 'system', chunk: 'Inspecting ' }),
        event(2, { type: 'text_chunk', stream: 'system', chunk: 'the code.' }),
        event(3, { type: 'text_chunk', stream: 'assistant', chunk: 'Done.' }),
      ),
    );

    expect(parts[0]).toMatchObject({
      type: 'reasoning',
      text: 'Inspecting the code.',
      state: 'completed',
    });
    expect(parts[1]).toMatchObject({ type: 'text', role: 'assistant', text: 'Done.' });
  });

  it('leaves a trailing reasoning chunk streaming', () => {
    const parts = normalizeTranscriptParts(
      source(event(1, { type: 'text_chunk', stream: 'system', chunk: 'Still thinking' })),
    );
    expect(parts[0]).toMatchObject({ type: 'reasoning', state: 'streaming' });
  });

  it('does not complete reasoning for a content-free heartbeat', () => {
    const parts = normalizeTranscriptParts(
      source(
        event(1, { type: 'text_chunk', stream: 'system', chunk: 'Still thinking' }),
        event(2, { type: 'heartbeat', seq: 2 }),
      ),
    );
    expect(parts[0]).toMatchObject({ type: 'reasoning', state: 'streaming' });
  });

  it('renders nothing for a content-free tmux pane_activity signal', () => {
    const parts = normalizeTranscriptParts(
      source(
        event(1, { type: 'text_chunk', stream: 'system', chunk: 'Still thinking' }),
        event(2, { type: 'pane_activity', seq: 0, bytes: 16480 }),
      ),
    );
    expect(parts).toHaveLength(1);
    expect(parts[0]).toMatchObject({ type: 'reasoning', state: 'streaming' });
  });

  it('pairs a tool result into the matching call and marks it completed', () => {
    const output = 'Chunk ID: abc\nWall time: 0.2 seconds\nProcess exited with code 0\nOutput:\nok';
    const parts = normalizeTranscriptParts(
      source(
        event(1, {
          type: 'tool_call',
          call_id: 'call-1',
          name: 'exec_command',
          args: { cmd: 'npm test', workdir: '/repo' },
        }),
        event(2, { type: 'tool_result', call_id: 'call-1', ok: true, output }),
      ),
    );

    expect(parts).toHaveLength(1);
    expect(parts[0]).toMatchObject({
      type: 'tool',
      callId: 'call-1',
      name: 'exec_command',
      state: 'completed',
      input: { cmd: 'npm test', workdir: '/repo' },
      output,
      ok: true,
      summary: 'run npm test',
      meta: [
        ['cwd', '/repo'],
        ['chunk', 'abc'],
      ],
    });
  });

  it('maps ok=false to error and leaves calls without results running', () => {
    const parts = normalizeTranscriptParts(
      source(
        event(1, { type: 'tool_call', call_id: 'call-running', name: 'read', args: { path: 'a.ts' } }),
        event(2, { type: 'tool_call', call_id: 'call-error', name: 'write', args: { path: 'b.ts' } }),
        event(3, { type: 'tool_result', call_id: 'call-error', ok: false, output: 'permission denied' }),
      ),
    );
    const tools = parts as TranscriptToolPart[];

    expect(tools[0].state).toBe('running');
    expect(tools[0].ok).toBeNull();
    expect(tools[1]).toMatchObject({ state: 'error', ok: false, output: 'permission denied' });
  });

  it('keeps real Codex exec, command_execution, and file_change starts running while live', () => {
    const tools = normalizeTranscriptParts(source(...realCodexStartedItems)).filter(
      (part): part is TranscriptToolPart => part.type === 'tool',
    );

    expect(tools.map((tool) => [tool.name, tool.state])).toEqual([
      ['exec', 'running'],
      ['command_execution', 'running'],
      ['file_change', 'running'],
    ]);
  });

  it.each<[string, SessionEnvelope, TranscriptToolPart['state']]>([
    ['run_complete', event(640, { type: 'run_complete' }), 'completed'],
    ['run_fail', event(640, { type: 'run_fail', message: 'driver failed' }), 'error'],
    [
      'lifecycle release',
      {
        seq: 640,
        time: '2026-07-16T19:26:52.835304Z',
        kind: 'lifecycle',
        event: {
          finalized_by_worker: true,
          outcome: 'cancelled',
          phase: 'release',
          reason: 'worker finalize for TASK-P0FAQ',
        },
      },
      'completed',
    ],
  ])('closes real Codex item-started tools as %s reaches the transcript', (_label, terminal, state) => {
    const tools = normalizeTranscriptParts(source(...realCodexStartedItems, terminal)).filter(
      (part): part is TranscriptToolPart => part.type === 'tool',
    );

    expect(tools).toHaveLength(3);
    expect(tools.every((tool) => tool.state === state)).toBe(true);
  });

  it('keeps an unpaired successful tool result visible as running', () => {
    const parts = normalizeTranscriptParts(
      source(event(1, { type: 'tool_result', call_id: 'missing-call', ok: true, output: { value: 1 } })),
    );
    expect(parts[0]).toMatchObject({
      type: 'tool',
      callId: 'missing-call',
      name: 'tool result',
      state: 'running',
      output: { value: 1 },
      ok: true,
    });
  });

  it('pairs a result that arrives before its call into one completed tool part', () => {
    const parts = normalizeTranscriptParts(
      source(
        event(1, {
          type: 'tool_result',
          call_id: 'out-of-order-call',
          ok: true,
          output: { content: 'file contents' },
        }),
        event(2, {
          type: 'tool_call',
          call_id: 'out-of-order-call',
          name: 'read',
          args: { path: 'src/app.ts' },
        }),
      ),
    );

    expect(parts).toHaveLength(1);
    expect(parts[0]).toMatchObject({
      type: 'tool',
      callId: 'out-of-order-call',
      name: 'read',
      state: 'completed',
      input: { path: 'src/app.ts' },
      output: { content: 'file contents' },
      ok: true,
    });
  });

  it('routes stderr to coalesced ANSI-free diagnostics and filters known info noise', () => {
    const parts = normalizeTranscriptParts(
      source(
        event(1, {
          type: 'text_chunk',
          stream: 'stderr',
          chunk: '2026-07-16 10:00:00 [INFO] agent.runtime: ready\n',
        }),
        event(2, { type: 'text_chunk', stream: 'stderr', chunk: '\u001b[31mfirst\u001b[0m\n' }),
        event(3, { type: 'text_chunk', stream: 'stderr', chunk: '\u001b[33msecond\u001b[0m\n' }),
      ),
    );

    expect(parts).toHaveLength(1);
    expect(parts[0]).toMatchObject({
      type: 'system',
      label: 'diagnostics',
      tone: 'diagnostic',
      code: true,
      text: 'first\nsecond\n',
    });
    expect(parts[0].type === 'system' ? parts[0].fullText : '').toContain('\u001b[31m');
  });

  it('injects the opening prompt and folds lifecycle markers and composer sends', () => {
    const parts = normalizeTranscriptParts(
      source(
        {
          seq: 1,
          kind: 'lifecycle',
          event: {
            phase: 'run_meta',
            driver_config: { prompt_bundle_text: 'line 1\nline 2\nline 3\nline 4\nline 5\nline 6\nline 7' },
          },
        },
        { seq: 2, kind: 'lifecycle', event: { phase: 'acquire', task_id: 'TASK-ONE', worker_id: 'codex' } },
        { seq: 3, kind: 'lifecycle', event: { phase: 'composer_send', text: 'Continue' } },
        {
          seq: 4,
          kind: 'lifecycle',
          event: { phase: 'release', outcome: 'completed', reason: 'driver terminal event' },
        },
      ),
    );

    expect(parts[0]).toMatchObject({ type: 'text', role: 'user', label: 'prompt' });
    expect(parts[0].type === 'text' ? parts[0].text : '').not.toContain('line 7');
    expect(parts[0].type === 'text' ? parts[0].fullText : '').toContain('line 7');
    expect(parts[1]).toMatchObject({ type: 'system', label: 'run started' });
    expect(parts[2]).toMatchObject({ type: 'text', role: 'user', text: 'Continue' });
    expect(parts[3]).toMatchObject({ type: 'system', label: 'run ended', tone: 'info' });
  });

  it('does not duplicate a canonical dispatch prompt delivered through send_input', () => {
    const prompt = 'orgasmic compiled prompt\nwork on TASK-ONE';
    const parts = normalizeTranscriptParts(
      source(
        {
          seq: 1,
          kind: 'lifecycle',
          event: { phase: 'run_meta', driver_config: { prompt_bundle_text: prompt } },
        },
        event(2, { type: 'text_chunk', stream: 'user', chunk: prompt }),
        event(3, {
          type: 'provider_runtime',
          event: {
            eventId: 'event-1',
            provider: 'claude',
            threadId: 'thread-1',
            createdAt: '2026-08-15T10:00:00Z',
            type: 'session.started',
            payload: {},
          },
        }),
      ),
    );

    expect(parts.filter((part) => part.type === 'text' && part.role === 'user')).toHaveLength(1);
  });
});

describe('hasResponseAfterPending', () => {
  it('resolves when an assistant part or terminal event occurs after the send', () => {
    const assistantSource = source(
      event(1, { type: 'text_chunk', stream: 'assistant', chunk: 'answer' }, '2026-07-16T10:01:00Z'),
    );
    expect(
      hasResponseAfterPending(
        normalizeTranscriptParts(assistantSource),
        assistantSource,
        '2026-07-16T10:00:00Z',
      ),
    ).toBe(true);

    const completeSource = source(
      event(2, { type: 'run_complete' }, '2026-07-16T10:02:00Z'),
    );
    expect(
      hasResponseAfterPending(
        normalizeTranscriptParts(completeSource),
        completeSource,
        '2026-07-16T10:00:00Z',
      ),
    ).toBe(true);
  });

  it('resolves when a canonical turn terminal event occurs after the send', () => {
    const completeSource = source(
      event(
        1,
        {
          type: 'provider_runtime',
          event: {
            eventId: 'turn-completed',
            provider: 'claude',
            threadId: 'thread-1',
            turnId: 'turn-1',
            createdAt: '2026-07-16T10:01:00Z',
            type: 'turn.completed',
            payload: { state: 'completed' },
          },
        },
        '2026-07-16T10:01:00Z',
      ),
    );

    expect(
      hasResponseAfterPending(
        normalizeTranscriptParts(completeSource),
        completeSource,
        '2026-07-16T10:00:00Z',
      ),
    ).toBe(true);
  });
});

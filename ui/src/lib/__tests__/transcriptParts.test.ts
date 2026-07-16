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

describe('normalizeTranscriptParts', () => {
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
});

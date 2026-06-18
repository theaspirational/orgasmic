// orgasmic:task_GKTWY
import { describe, it, expect } from 'vitest';
import {
  coalesceTextChunks,
  extractPromptBundle,
  stripAnsi,
  groupToolEntries,
  selectGroupSummary,
  type TranscriptEntry,
  type ToolEntry,
} from '../transcriptUtils';

// Real-shape fixtures derived from dispatch-TASK-145-implementer-20260611T114019.jsonl

const LIFECYCLE_WITH_PROMPT = {
  seq: 1,
  kind: 'lifecycle' as const,
  event: {
    driver_config: {
      prompt_bundle_text:
        'orgasmic compiled prompt\ndispatch_kind: implementer\ntask: TASK-145\nworker: implementer-codex-acp\nprompt_spec: implementer\n\n# Prompt Spec: implementer\n',
    },
  },
};

const LIFECYCLE_ACQUIRE = {
  seq: 0,
  kind: 'lifecycle' as const,
  event: {
    kind: 'implementer',
    phase: 'acquire',
    task_id: 'TASK-145',
    worker_id: 'implementer-codex-acp',
  },
};

const DRIVER_EVENT = {
  seq: 5,
  kind: 'driver_event' as const,
  event: { type: 'text_chunk', stream: 'assistant', chunk: 'hello' },
};

// ANSI-laden stderr from real session (seq=52)
const STDERR_ANSI =
  '\x1b[2m2026-06-11T11:40:49.490623Z\x1b[0m \x1b[31mERROR\x1b[0m \x1b[2mcodex_api::endpoint::responses_websocket\x1b[0m\x1b[2m:\x1b[0m failed to connect to websocket: HTTP error: 500 Internal Server Error, url: wss://chatgpt.com/backend-api/codex/responses';

// --- extractPromptBundle ---

describe('extractPromptBundle', () => {
  it('returns null for empty array', () => {
    expect(extractPromptBundle([])).toBeNull();
  });

  it('returns null when no lifecycle event has driver_config', () => {
    expect(extractPromptBundle([LIFECYCLE_ACQUIRE, DRIVER_EVENT])).toBeNull();
  });

  it('returns null when lifecycle has empty prompt_bundle_text', () => {
    const env = { kind: 'lifecycle' as const, event: { driver_config: { prompt_bundle_text: '' } } };
    expect(extractPromptBundle([env])).toBeNull();
  });

  it('returns prompt_bundle_text from real-shape lifecycle seq=1', () => {
    const result = extractPromptBundle([LIFECYCLE_ACQUIRE, LIFECYCLE_WITH_PROMPT]);
    expect(result).not.toBeNull();
    expect(result).toContain('orgasmic compiled prompt');
    expect(result).toContain('dispatch_kind: implementer');
  });

  it('returns first match when multiple lifecycle events have prompt', () => {
    const second = { kind: 'lifecycle' as const, event: { driver_config: { prompt_bundle_text: 'second' } } };
    const result = extractPromptBundle([LIFECYCLE_WITH_PROMPT, second]);
    expect(result).toContain('orgasmic compiled prompt');
  });

  it('ignores non-lifecycle envelopes', () => {
    expect(extractPromptBundle([DRIVER_EVENT])).toBeNull();
  });
});

// --- stripAnsi ---

describe('stripAnsi', () => {
  it('returns text unchanged when no ANSI codes present', () => {
    expect(stripAnsi('hello world')).toBe('hello world');
  });

  it('strips SGR reset code \\x1b[0m', () => {
    expect(stripAnsi('text\x1b[0m')).toBe('text');
  });

  it('strips color code \\x1b[31m', () => {
    expect(stripAnsi('\x1b[31mERROR\x1b[0m')).toBe('ERROR');
  });

  it('strips dim code \\x1b[2m from real stderr fixture', () => {
    const stripped = stripAnsi(STDERR_ANSI);
    expect(stripped).not.toContain('\x1b[');
    expect(stripped).toContain('ERROR');
    expect(stripped).toContain('failed to connect to websocket');
    expect(stripped).toContain('2026-06-11T11:40:49.490623Z');
  });

  it('handles multiple codes in one string', () => {
    expect(stripAnsi('\x1b[1m\x1b[32mgreen bold\x1b[0m')).toBe('green bold');
  });
});

// --- groupToolEntries ---

// Real-shape tool entries derived from TASK-145 session
const EXEC_ENTRY: ToolEntry = {
  id: '34',
  label: 'command request',
  callId: 'call_P9L6CnPRgvPt6pfceQWM4W4b',
  activity: {
    summary: "run cat ~/.claude/plugins/karpathy-skills/SKILL.md",
    meta: [['wait', '30000ms']],
    raw: '{"cmd":"cat ...","workdir":"/tmp/orgasmic-worktrees/task-145"}',
  },
};

const CMD_EXEC_ENTRY: ToolEntry = {
  id: '36',
  label: 'command started',
  callId: 'call_P9L6CnPRgvPt6pfceQWM4W4b',
  activity: {
    summary: 'started cat ~/.claude/plugins/karpathy-skills/SKILL.md',
    meta: [['pid', '79108']],
    raw: '{"command":"/bin/zsh -lc...","processId":79108}',
  },
};

const RESULT_ENTRY: ToolEntry = {
  id: '37',
  label: 'command result',
  callId: 'call_P9L6CnPRgvPt6pfceQWM4W4b',
  activity: {
    summary: 'exit 0 · 630 tokens · 0.0000 seconds',
    meta: [['chunk', 'dc5911']],
    raw: 'Chunk ID: dc5911\nWall time: 0.0000 seconds\nProcess exited with code 0\nOutput:\n...',
  },
};

const WRITE_STDIN_ENTRY: ToolEntry = {
  id: '100',
  label: 'terminal input',
  callId: 'call_WRITESTDIN',
  activity: { summary: 'send 5 chars to terminal', meta: [], raw: '{}' },
};

describe('groupToolEntries', () => {
  it('returns empty array for empty input', () => {
    expect(groupToolEntries([])).toEqual([]);
  });

  it('collapses exec_command triplet into one paired item', () => {
    const items = groupToolEntries([EXEC_ENTRY, CMD_EXEC_ENTRY, RESULT_ENTRY]);
    expect(items).toHaveLength(1);
    expect(items[0].type).toBe('paired');
  });

  it('paired command carries exec_command summary', () => {
    const items = groupToolEntries([EXEC_ENTRY, CMD_EXEC_ENTRY, RESULT_ENTRY]);
    const paired = items[0];
    expect(paired.type).toBe('paired');
    if (paired.type === 'paired') {
      expect(paired.command.summary).toContain('run cat');
      expect(paired.command.callId).toBe('call_P9L6CnPRgvPt6pfceQWM4W4b');
    }
  });

  it('paired status is "exit 0" when result has exit code', () => {
    const items = groupToolEntries([EXEC_ENTRY, CMD_EXEC_ENTRY, RESULT_ENTRY]);
    if (items[0].type === 'paired') {
      expect(items[0].command.status).toBe('exit 0');
    }
  });

  it('paired status is "running" when no result present', () => {
    const items = groupToolEntries([EXEC_ENTRY, CMD_EXEC_ENTRY]);
    if (items[0].type === 'paired') {
      expect(items[0].command.status).toBe('running');
    }
  });

  it('non-command entries remain as single items', () => {
    const items = groupToolEntries([EXEC_ENTRY, CMD_EXEC_ENTRY, RESULT_ENTRY, WRITE_STDIN_ENTRY]);
    expect(items).toHaveLength(2);
    const single = items[1];
    expect(single.type).toBe('single');
    if (single.type === 'single') {
      expect(single.entry.label).toBe('terminal input');
    }
  });

  it('non-exec entry without callId emits as single', () => {
    const standalone: ToolEntry = { id: '99', label: 'patch', activity: { summary: 'apply patch' } };
    const items = groupToolEntries([standalone]);
    expect(items).toHaveLength(1);
    expect(items[0].type).toBe('single');
  });

  it('drops orphaned command result with callId but no request/start pair', () => {
    const items = groupToolEntries([RESULT_ENTRY]);
    expect(items).toEqual([]);
  });

  it('result parsed as exit 1 for non-zero exit code', () => {
    const failResult: ToolEntry = {
      ...RESULT_ENTRY,
      label: 'command error',
      activity: { summary: 'exit 1 · 0 tokens', meta: [], raw: '' },
    };
    const items = groupToolEntries([EXEC_ENTRY, CMD_EXEC_ENTRY, failResult]);
    if (items[0].type === 'paired') {
      expect(items[0].command.status).toBe('exit 1');
    }
  });

  it('multiple commands produce multiple paired items in order', () => {
    const exec2: ToolEntry = { id: '38', label: 'command request', callId: 'call_2', activity: { summary: 'run ls' } };
    const started2: ToolEntry = { id: '40', label: 'command started', callId: 'call_2', activity: { summary: 'started ls' } };
    const result2: ToolEntry = { id: '41', label: 'command result', callId: 'call_2', activity: { summary: 'exit 0 · 10 tokens' } };
    const items = groupToolEntries([EXEC_ENTRY, CMD_EXEC_ENTRY, RESULT_ENTRY, exec2, started2, result2]);
    expect(items).toHaveLength(2);
    expect(items[0].type).toBe('paired');
    expect(items[1].type).toBe('paired');
    if (items[0].type === 'paired') expect(items[0].command.callId).toBe('call_P9L6CnPRgvPt6pfceQWM4W4b');
    if (items[1].type === 'paired') expect(items[1].command.callId).toBe('call_2');
  });
});

// --- coalesceTextChunks ---

describe('coalesceTextChunks', () => {
  it('appends stderr diagnostics text, visible preview, and raw disclosure content', () => {
    const first: TranscriptEntry = {
      id: '1',
      role: 'tool',
      label: 'diagnostics',
      text: 'first clean\n',
      mergeKey: 'tool:stderr-diagnostics',
      activity: {
        summary: 'first clean',
        preview: 'first clean\n',
        raw: '\x1b[31mfirst raw\x1b[0m\n',
      },
    };
    const second: TranscriptEntry = {
      id: '2',
      role: 'tool',
      label: 'diagnostics',
      text: 'second clean\n',
      mergeKey: 'tool:stderr-diagnostics',
      activity: {
        summary: 'second clean',
        preview: 'second clean\n',
        raw: '\x1b[33msecond raw\x1b[0m\n',
      },
    };
    const third: TranscriptEntry = {
      id: '3',
      role: 'tool',
      label: 'diagnostics',
      text: 'third clean\n',
      mergeKey: 'tool:stderr-diagnostics',
      activity: {
        summary: 'third clean',
        preview: 'third clean\n',
        raw: '\x1b[34mthird raw\x1b[0m\n',
      },
    };

    const items = coalesceTextChunks([first, second, third]);

    expect(items).toHaveLength(1);
    expect(items[0].text).toBe('first clean\nsecond clean\nthird clean\n');
    expect(items[0].activity?.preview).toBe('first clean\nsecond clean\nthird clean\n');
    expect(items[0].activity?.raw).toBe(
      '\x1b[31mfirst raw\x1b[0m\n\x1b[33msecond raw\x1b[0m\n\x1b[34mthird raw\x1b[0m\n',
    );
    expect(items[0]).not.toHaveProperty('mergeKey');
  });
});

// --- selectGroupSummary ---

describe('selectGroupSummary', () => {
  it('returns "tool calls" for empty entries', () => {
    expect(selectGroupSummary([])).toBe('tool calls');
  });

  it('prefers last command request summary', () => {
    const entries: ToolEntry[] = [
      { id: '1', label: 'command request', activity: { summary: 'run cat file1' } },
      { id: '2', label: 'command result', activity: { summary: 'exit 0 · 100 tokens' } },
      { id: '3', label: 'command request', activity: { summary: 'run sed file2' } },
      { id: '4', label: 'command result', activity: { summary: 'exit 0 · 200 tokens' } },
    ];
    expect(selectGroupSummary(entries)).toBe('run sed file2');
  });

  it('falls back to command started if no command request', () => {
    const entries: ToolEntry[] = [
      { id: '1', label: 'command started', activity: { summary: 'started grep pattern' } },
      { id: '2', label: 'command result', activity: { summary: 'exit 0 · 50 tokens' } },
    ];
    expect(selectGroupSummary(entries)).toBe('started grep pattern');
  });

  it('falls back to any summary when no command entries', () => {
    const entries: ToolEntry[] = [
      { id: '1', label: 'terminal input', activity: { summary: 'send 10 chars to terminal' } },
    ];
    expect(selectGroupSummary(entries)).toBe('send 10 chars to terminal');
  });

  it('shows in-flight command (no result) as the summary for live trailing group', () => {
    const entries: ToolEntry[] = [
      { id: '1', label: 'command request', activity: { summary: 'run sed -n 1,220p .orgasmic/project.org' } },
      { id: '2', label: 'command started', activity: { summary: 'started sed -n ...' } },
      // no result yet — live run
    ];
    // selectGroupSummary should show the most recent command request
    expect(selectGroupSummary(entries)).toBe('run sed -n 1,220p .orgasmic/project.org');
  });
});

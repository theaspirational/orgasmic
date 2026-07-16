export type SessionEnvelope = {
  seq?: number;
  time?: string;
  kind?: string;
  event?: Record<string, unknown>;
};

export type TranscriptTextPart = {
  id: string;
  type: 'text';
  role: 'assistant' | 'user';
  label: string;
  text: string;
  fullText?: string;
  time?: string;
};

export type TranscriptReasoningPart = {
  id: string;
  type: 'reasoning';
  label: string;
  text: string;
  state: 'streaming' | 'completed';
  time?: string;
};

export type TranscriptToolState = 'streaming' | 'running' | 'completed' | 'error';

export type TranscriptToolPart = {
  id: string;
  type: 'tool';
  callId?: string;
  name: string;
  label: string;
  state: TranscriptToolState;
  input: unknown;
  output: unknown;
  ok: boolean | null;
  summary?: string;
  meta: Array<[string, string]>;
  time?: string;
};

export type TranscriptSystemPart = {
  id: string;
  type: 'system';
  label: string;
  text: string;
  tone: 'info' | 'diagnostic' | 'error';
  code?: boolean;
  fullText?: string;
  time?: string;
};

export type TranscriptPart =
  | TranscriptTextPart
  | TranscriptReasoningPart
  | TranscriptToolPart
  | TranscriptSystemPart;

type PartDraft = TranscriptPart & { mergeKey?: string };

type ToolSummary = {
  label?: string;
  summary?: string;
  meta: Array<[string, string]>;
};

export function parseSessionSource(source: string): SessionEnvelope[] {
  return source
    .split('\n')
    .map((line) => line.trim())
    .filter(Boolean)
    .flatMap((line) => {
      try {
        return [JSON.parse(line) as SessionEnvelope];
      } catch {
        return [];
      }
    });
}

export function extractPromptBundle(envelopes: SessionEnvelope[]): string | null {
  for (const envelope of envelopes) {
    if (envelope.kind !== 'lifecycle') continue;
    const driverConfig = envelope.event?.driver_config;
    if (!isRecord(driverConfig)) continue;
    const prompt = driverConfig.prompt_bundle_text;
    if (typeof prompt === 'string' && prompt.length > 0) return prompt;
  }
  return null;
}

export function normalizeTranscriptParts(
  source: string,
  options: { promptOverride?: string | null } = {},
): TranscriptPart[] {
  const envelopes = parseSessionSource(source);
  const parts: PartDraft[] = [];
  const toolsByCallId = new Map<string, TranscriptToolPart>();
  const pendingResultsByCallId = new Map<string, TranscriptToolPart>();
  let terminalToolState: Extract<TranscriptToolState, 'completed' | 'error'> | null = null;
  const promptBundle = options.promptOverride ?? extractPromptBundle(envelopes);

  if (promptBundle) {
    parts.push({
      id: 'prompt-bundle',
      type: 'text',
      role: 'user',
      label: 'prompt',
      text: promptBundle.split('\n').slice(0, 6).join('\n'),
      fullText: promptBundle,
    });
  }

  for (const [index, envelope] of envelopes.entries()) {
    const event = envelope.event ?? {};
    const eventType = stringValue(event.type);
    const id = String(envelope.seq ?? index);

    if (eventType === 'text_chunk') {
      const stream = stringValue(event.stream);
      const chunk = stringValue(event.chunk);
      if (!chunk.trim()) continue;

      if (stream === 'system' && !isProviderWarning(chunk)) {
        pushPart(
          parts,
          {
            id,
            type: 'reasoning',
            label: 'thinking',
            text: chunk,
            state: 'streaming',
            time: envelope.time,
          },
          'text:system:reasoning',
        );
        continue;
      }

      if (stream === 'stderr') {
        if (isIgnoredStderr(chunk)) continue;
        closeStreamingReasoning(parts);
        const clean = stripAnsi(chunk);
        pushPart(
          parts,
          {
            id,
            type: 'system',
            label: 'diagnostics',
            text: clean,
            fullText: chunk,
            tone: 'diagnostic',
            code: true,
            time: envelope.time,
          },
          'text:stderr:diagnostics',
        );
        continue;
      }

      closeStreamingReasoning(parts);
      if (stream === 'assistant' || stream === 'user') {
        pushPart(
          parts,
          {
            id,
            type: 'text',
            role: stream,
            label: stream,
            text: chunk,
            time: envelope.time,
          },
          `text:${stream}`,
        );
        continue;
      }

      if (stream === 'stdout') {
        pushPart(
          parts,
          {
            id,
            type: 'system',
            label: 'stdout',
            text: chunk,
            tone: 'diagnostic',
            code: true,
            time: envelope.time,
          },
          'text:stdout',
        );
        continue;
      }

      pushPart(
        parts,
        {
          id,
          type: 'system',
          label: isProviderWarning(chunk) ? 'provider warning' : stream || 'system',
          text: chunk,
          tone: isProviderWarning(chunk) ? 'error' : 'info',
          time: envelope.time,
        },
        `text:${stream || 'system'}`,
      );
      continue;
    }

    if (eventType === 'ready' || eventType === 'heartbeat') continue;

    closeStreamingReasoning(parts);

    if (eventType === 'tool_call') {
      const callId = stringValue(event.call_id) || undefined;
      const name = compactLabel(event.name);
      const existing = callId ? toolsByCallId.get(callId) : undefined;
      const pendingResult = callId ? pendingResultsByCallId.get(callId) : undefined;
      const summary = summarizeToolCall(name, event.args);
      if (existing) {
        existing.state = toolCallState(event.args);
        existing.summary ??= summary.summary;
        if (existing.meta.length === 0) existing.meta = summary.meta;
        continue;
      }

      if (pendingResult) {
        pendingResult.name = name;
        pendingResult.label = summary.label ?? `tool ${name}`;
        pendingResult.input = event.args ?? null;
        pendingResult.state = pendingResult.ok === false ? 'error' : 'completed';
        pendingResult.summary = summary.summary ?? pendingResult.summary;
        pendingResult.meta = mergeMeta(summary.meta, pendingResult.meta);
        pendingResult.time = envelope.time ?? pendingResult.time;
        toolsByCallId.set(callId!, pendingResult);
        pendingResultsByCallId.delete(callId!);
        continue;
      }

      const part: TranscriptToolPart = {
        id,
        type: 'tool',
        callId,
        name,
        label: summary.label ?? `tool ${name}`,
        state: toolCallState(event.args),
        input: event.args ?? null,
        output: null,
        ok: null,
        summary: summary.summary,
        meta: summary.meta,
        time: envelope.time,
      };
      parts.push(part);
      if (callId) toolsByCallId.set(callId, part);
      continue;
    }

    if (eventType === 'tool_result') {
      const callId = stringValue(event.call_id) || undefined;
      const ok = booleanValue(event.ok);
      const paired = callId ? toolsByCallId.get(callId) : undefined;
      const resultSummary = summarizeToolResult(ok, event.output);
      if (paired) {
        paired.output = event.output ?? null;
        paired.ok = ok;
        paired.state = ok === false ? 'error' : 'completed';
        paired.summary ??= resultSummary.summary;
        paired.meta = mergeMeta(paired.meta, resultSummary.meta);
        continue;
      }

      const part: TranscriptToolPart = {
        id,
        type: 'tool',
        callId,
        name: 'tool result',
        label: resultSummary.label ?? 'tool result',
        state: ok === false ? 'error' : 'running',
        input: null,
        output: event.output ?? null,
        ok,
        summary: resultSummary.summary,
        meta: resultSummary.meta,
        time: envelope.time,
      };
      parts.push(part);
      if (callId) pendingResultsByCallId.set(callId, part);
      continue;
    }

    if (eventType === 'transition_state') {
      const from = compactLabel(event.from);
      const to = compactLabel(event.to);
      const reason = stringValue(event.reason).trim();
      parts.push({
        id,
        type: 'system',
        label: 'state transition',
        text: reason ? `${from} -> ${to}\n${reason}` : `${from} -> ${to}`,
        tone: 'info',
        time: envelope.time,
      });
      continue;
    }

    if (eventType === 'run_complete') {
      terminalToolState ??= 'completed';
      const summary = stringValue(event.summary).trim();
      if (summary) {
        parts.push({
          id,
          type: 'system',
          label: 'run complete',
          text: summary,
          tone: 'info',
          time: envelope.time,
        });
      }
      continue;
    }

    if (eventType === 'run_fail' || eventType === 'driver_error') {
      terminalToolState = 'error';
      parts.push({
        id,
        type: 'system',
        label: eventType.replace('_', ' '),
        text: stringValue(event.error_markdown || event.message),
        tone: 'error',
        time: envelope.time,
      });
      continue;
    }

    if (envelope.kind === 'lifecycle') {
      if (stringValue(event.phase) === 'release') {
        terminalToolState =
          stringValue(event.outcome) === 'failed' ? 'error' : (terminalToolState ?? 'completed');
      }
      const lifecyclePart = normalizeLifecyclePart(id, envelope.time, event);
      if (lifecyclePart) parts.push(lifecyclePart);
      continue;
    }

    if (envelope.kind === 'note') {
      parts.push({
        id,
        type: 'system',
        label: 'note',
        text: stringValue(event),
        tone: 'info',
        time: envelope.time,
      });
    }
  }

  if (terminalToolState) closeRunningTools(parts, terminalToolState);
  return parts.map(({ mergeKey: _mergeKey, ...part }) => part);
}

export function hasResponseAfterPending(
  parts: TranscriptPart[],
  source: string,
  pendingSince: string | null | undefined,
): boolean {
  if (!pendingSince) return false;
  const pendingTime = Date.parse(pendingSince);
  if (!Number.isFinite(pendingTime)) return false;

  const hasVisibleResponse = parts.some((part) => {
    if (!part.time) return false;
    if (part.type !== 'text' || part.role !== 'assistant') {
      if (part.type !== 'system' || part.tone !== 'error') return false;
    }
    const partTime = Date.parse(part.time);
    return Number.isFinite(partTime) && partTime >= pendingTime;
  });
  if (hasVisibleResponse) return true;

  return parseSessionSource(source).some((envelope) => {
    if (!envelope.time) return false;
    const type = stringValue(envelope.event?.type);
    if (type !== 'run_complete' && type !== 'run_fail' && type !== 'driver_error') return false;
    const eventTime = Date.parse(envelope.time);
    return Number.isFinite(eventTime) && eventTime >= pendingTime;
  });
}

function normalizeLifecyclePart(
  id: string,
  time: string | undefined,
  event: Record<string, unknown>,
): TranscriptPart | null {
  const phase = stringValue(event.phase);
  if (phase === 'composer_send') {
    const text = stringValue(event.text);
    if (!text.trim()) return null;
    return { id, type: 'text', role: 'user', label: 'user', text, time };
  }
  if (phase === 'acquire') {
    const task = stringValue(event.task_id);
    const worker = stringValue(event.worker_id);
    return {
      id,
      type: 'system',
      label: 'run started',
      text: [task, worker].filter(Boolean).join(' · ') || 'run acquired',
      tone: 'info',
      time,
    };
  }
  if (phase === 'attach') {
    return { id, type: 'system', label: 'attached', text: 'session attached', tone: 'info', time };
  }
  if (phase === 'reattach') {
    const transport = stringValue(event.transport);
    return {
      id,
      type: 'system',
      label: 'reattached',
      text: transport ? `session reattached via ${transport}` : 'session reattached',
      tone: 'info',
      time,
    };
  }
  if (phase === 'continuation') {
    const previousRun = stringValue(event.previous_run);
    return {
      id,
      type: 'system',
      label: 'continuation',
      text: previousRun ? `continued from ${previousRun}` : 'continued previous run',
      tone: 'info',
      time,
    };
  }
  if (phase === 'release') {
    const outcome = stringValue(event.outcome);
    const reason = stringValue(event.reason);
    return {
      id,
      type: 'system',
      label: 'run ended',
      text: [outcome, reason].filter(Boolean).join(' · ') || 'run released',
      tone: outcome === 'failed' ? 'error' : 'info',
      time,
    };
  }
  return null;
}

function pushPart(parts: PartDraft[], part: TranscriptPart, mergeKey: string): void {
  const previous = parts[parts.length - 1];
  if (previous?.mergeKey === mergeKey && mergePart(previous, part)) {
    previous.time = part.time ?? previous.time;
    return;
  }
  parts.push({ ...part, mergeKey });
}

function mergePart(previous: PartDraft, next: TranscriptPart): boolean {
  if (previous.type === 'text' && next.type === 'text' && previous.role === next.role) {
    previous.text += next.text;
    return true;
  }
  if (previous.type === 'reasoning' && next.type === 'reasoning') {
    previous.text += next.text;
    previous.state = next.state;
    return true;
  }
  if (
    previous.type === 'system' &&
    next.type === 'system' &&
    previous.label === next.label &&
    previous.tone === next.tone
  ) {
    previous.text += next.text;
    previous.fullText = `${previous.fullText ?? ''}${next.fullText ?? ''}` || undefined;
    return true;
  }
  return false;
}

function closeStreamingReasoning(parts: PartDraft[]): void {
  const last = parts[parts.length - 1];
  if (last?.type === 'reasoning') last.state = 'completed';
}

function closeRunningTools(
  parts: PartDraft[],
  state: Extract<TranscriptToolState, 'completed' | 'error'>,
): void {
  for (const part of parts) {
    if (part.type === 'tool' && part.state === 'running') part.state = state;
  }
}

function toolCallState(args: unknown): TranscriptToolState {
  if (!isRecord(args)) return 'running';
  const status = stringValue(args.status).toLowerCase().replaceAll('_', '-');
  return status === 'streaming' || status === 'input-streaming' || status === 'pending'
    ? 'streaming'
    : 'running';
}

function summarizeToolCall(name: string, args: unknown): ToolSummary {
  if (!isRecord(args)) return { summary: name ? `use ${name}` : 'tool call', meta: [] };
  const meta: Array<[string, string]> = [];
  const cwd = args.workdir ?? args.cwd;
  if (cwd) meta.push(['cwd', trimMiddle(stringValue(cwd), 96)]);

  if (name === 'exec_command') {
    const command = stringValue(args.cmd);
    if (args.yield_time_ms !== undefined) meta.push(['wait', `${stringValue(args.yield_time_ms)}ms`]);
    if (args.max_output_tokens !== undefined) {
      meta.push(['limit', `${stringValue(args.max_output_tokens)} tokens`]);
    }
    return {
      label: 'command request',
      summary: command ? `run ${trimMiddle(command, 140)}` : 'run command',
      meta,
    };
  }

  if (name === 'command_execution') {
    const actions = Array.isArray(args.commandActions) ? args.commandActions : [];
    const actionSummary = actions.map((action) => commandActionSummary(action, args.cwd)).find(Boolean);
    const command = stringValue(args.command);
    const status = stringValue(args.status);
    const processId = stringValue(args.processId);
    if (status) meta.push(['status', status]);
    if (processId) meta.push(['pid', processId]);
    return {
      label: 'command started',
      summary: actionSummary ?? (command ? `started ${trimMiddle(command, 140)}` : 'started command'),
      meta,
    };
  }

  if (name === 'write_stdin') {
    const sessionId = stringValue(args.session_id);
    const chars = stringValue(args.chars);
    if (sessionId) meta.push(['session', sessionId]);
    return {
      label: 'terminal input',
      summary: chars ? `send ${chars.length} chars to terminal` : 'poll terminal',
      meta,
    };
  }

  if (name === 'apply_patch') return { label: 'patch', summary: 'apply patch', meta };
  return { summary: name ? `use ${name}` : 'tool call', meta };
}

function summarizeToolResult(ok: boolean | null, output: unknown): ToolSummary {
  if (typeof output !== 'string') {
    return {
      label: ok === false ? 'tool error' : 'tool result',
      summary: ok === false ? 'tool failed' : 'tool finished',
      meta: [],
    };
  }

  const lines = output.split('\n');
  const chunkId = /^Chunk ID:\s*(.+)$/.exec(lines[0] ?? '')?.[1]?.trim();
  const wallTime = /^Wall time:\s*(.+)$/.exec(lines[1] ?? '')?.[1]?.trim();
  const exitCode = lines
    .map((line) => /Process exited with code\s+(-?\d+)/.exec(line)?.[1])
    .find(Boolean);
  const tokenCount = lines
    .map((line) => /Original token count:\s*(\d+)/.exec(line)?.[1])
    .find(Boolean);
  const summaryParts = [
    exitCode !== undefined ? `exit ${exitCode}` : '',
    tokenCount ? `${tokenCount} tokens` : '',
    wallTime ?? '',
  ].filter(Boolean);
  return {
    label: ok === false ? 'command error' : 'command result',
    summary: summaryParts.length ? summaryParts.join(' · ') : firstLine(output) || 'tool finished',
    meta: chunkId ? [['chunk', chunkId]] : [],
  };
}

function commandActionSummary(action: unknown, cwd: unknown): string | null {
  if (!isRecord(action)) return null;
  const type = stringValue(action.type);
  const command = stringValue(action.command);
  const name = stringValue(action.name);
  const path = relativePath(action.path, cwd);
  if (type === 'read') return `read ${name || path || command}`;
  if (type === 'search') {
    const query = stringValue(action.query);
    return `search ${name || path}${query ? ` for ${query}` : ''}`;
  }
  if (type && type !== 'unknown') return `${type} ${name || path || command}`;
  return command ? `run ${command}` : null;
}

function mergeMeta(
  left: Array<[string, string]>,
  right: Array<[string, string]>,
): Array<[string, string]> {
  const result = [...left];
  const seen = new Set(left.map(([key, value]) => `${key}\u0000${value}`));
  for (const entry of right) {
    const key = `${entry[0]}\u0000${entry[1]}`;
    if (!seen.has(key)) {
      result.push(entry);
      seen.add(key);
    }
  }
  return result;
}

function isIgnoredStderr(text: string): boolean {
  if (text.includes('codex_core_skills::loader') && text.includes("icon path must not contain '..'")) {
    return true;
  }
  const isInfoOrDebug = text.includes(' [INFO] ') || text.includes(' [DEBUG] ');
  return (
    isInfoOrDebug &&
    /^\d{4}-\d{2}-\d{2}\s+\d{2}:\d{2}:\d{2}\s+\[(INFO|DEBUG)\]\s+[\w.:_-]+:/.test(text)
  );
}

function isProviderWarning(text: string): boolean {
  return /warning/i.test(text.trim().slice(0, 96));
}

function stripAnsi(text: string): string {
  return text.replace(/\x1b\[[0-9;]*[a-zA-Z]/g, '');
}

function booleanValue(value: unknown): boolean | null {
  if (typeof value === 'boolean') return value;
  if (value === 'true') return true;
  if (value === 'false') return false;
  return null;
}

function compactLabel(value: unknown): string {
  return stringValue(value).trim() || 'unknown';
}

function firstLine(text: string): string {
  return text.split('\n').find((line) => line.trim())?.trim() ?? '';
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

function relativePath(path: unknown, cwd: unknown): string {
  const text = stringValue(path);
  const base = stringValue(cwd);
  return base && text.startsWith(`${base}/`) ? text.slice(base.length + 1) : text;
}

function stringValue(value: unknown): string {
  if (typeof value === 'string') return value;
  if (value === null || value === undefined) return '';
  return JSON.stringify(value, null, 2);
}

function trimMiddle(text: string, max: number): string {
  if (text.length <= max) return text;
  const head = Math.max(0, Math.floor((max - 3) * 0.62));
  const tail = Math.max(0, max - 3 - head);
  return `${text.slice(0, head)}...${text.slice(text.length - tail)}`;
}

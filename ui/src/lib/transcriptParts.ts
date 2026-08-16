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
  const canonicalInputDeltas = new Map<string, string>();
  let terminalToolState: Extract<TranscriptToolState, 'completed' | 'error'> | null = null;
  const promptBundle = options.promptOverride ?? extractPromptBundle(envelopes);
  const canonicalChat = envelopes.some(
    (envelope) => stringValue(envelope.event?.type) === 'provider_runtime',
  );
  const unmatchedComposerSends = canonicalChat
    ? composerSendCounts(envelopes)
    : new Map<string, number>();

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

    if (eventType === 'provider_runtime') {
      const runtime = isRecord(event.event) ? event.event : null;
      if (!runtime) continue;
      const runtimeType = stringValue(runtime.type);
      const payload = isRecord(runtime.payload) ? runtime.payload : {};
      const runtimeId = stringValue(runtime.itemId || runtime.requestId) || id;
      const runtimeTime = stringValue(runtime.createdAt) || envelope.time;

      if (runtimeType === 'content.delta') {
        const streamKind = stringValue(payload.streamKind);
        const delta = stringValue(payload.delta);
        if (!delta) continue;
        if (streamKind === 'assistant_text') {
          closeStreamingReasoning(parts);
          const contentId = canonicalContentPartId(runtime, runtimeId, streamKind);
          pushPart(
            parts,
            {
              id: contentId,
              type: 'text',
              role: 'assistant',
              label: 'assistant',
              text: delta,
              time: runtimeTime,
            },
            `provider:assistant:${contentId}`,
          );
          continue;
        }
        if (streamKind === 'reasoning_text') {
          const contentId = canonicalContentPartId(runtime, runtimeId, streamKind);
          pushPart(
            parts,
            {
              id: contentId,
              type: 'reasoning',
              label: 'thinking',
              text: delta,
              state: 'streaming',
              time: runtimeTime,
            },
            `provider:reasoning:${contentId}`,
          );
          continue;
        }
        const tool = toolsByCallId.get(runtimeId);
        if (tool) {
          const previous = typeof tool.output === 'string' ? tool.output : '';
          tool.output = `${previous}${delta}`;
        }
        continue;
      }

      if (
        runtimeType === 'item.started' ||
        runtimeType === 'item.updated' ||
        runtimeType === 'item.completed'
      ) {
        const itemType = stringValue(payload.itemType);
        if (itemType === 'assistant_message') {
          if (runtimeType === 'item.completed') closeStreamingReasoning(parts);
          continue;
        }
        closeStreamingReasoning(parts);
        const data = isRecord(payload.data) ? payload.data : {};
        const providerState = isRecord(data.state) ? data.state : {};
        const existing = toolsByCallId.get(runtimeId);
        const name = canonicalProviderToolName(itemType, data, payload, existing?.name);
        const pairedCommand =
          !existing && name === 'command_execution'
            ? immediatelyPrecedingRunningCanonicalExec(parts)
            : undefined;
        const target = existing ?? pairedCommand;
        const status = stringValue(payload.status);
        const state: TranscriptToolState =
          status === 'failed'
            ? 'error'
            : runtimeType === 'item.completed' || status === 'completed'
              ? 'completed'
              : runtimeType === 'item.started'
                ? 'running'
                : 'streaming';
        const inputDelta = stringValue(data.inputDelta);
        if (inputDelta) {
          canonicalInputDeltas.set(
            runtimeId,
            `${canonicalInputDeltas.get(runtimeId) ?? ''}${inputDelta}`,
          );
        }
        const accumulatedInput = parseJsonValue(canonicalInputDeltas.get(runtimeId));
        const input = accumulatedInput ?? data.input ?? providerState.input ?? null;
        const output =
          data.output ??
          data.result ??
          providerState.output ??
          providerState.error ??
          (runtimeType === 'item.completed' ? stringValue(payload.detail) || null : null);
        const summary = summarizeCanonicalProviderTool(name, input, payload, target);
        if (target) {
          if (!existing) toolsByCallId.set(runtimeId, target);
          target.name = name;
          target.label = summary.label ?? target.label;
          target.state = state;
          if (input !== null) target.input = input;
          if (output !== null) target.output = output;
          target.ok = state === 'error' ? false : state === 'completed' ? true : null;
          target.summary = summary.summary ?? target.summary;
          target.meta = mergeMeta(target.meta, summary.meta);
          if (runtimeType === 'item.completed') canonicalInputDeltas.delete(runtimeId);
          continue;
        }
        const part: TranscriptToolPart = {
          id: runtimeId,
          type: 'tool',
          callId: runtimeId,
          name,
          label: summary.label ?? (compactLabel(payload.title) || `tool ${name}`),
          state,
          input,
          output,
          ok: state === 'error' ? false : state === 'completed' ? true : null,
          summary: summary.summary,
          meta: summary.meta,
          time: runtimeTime,
        };
        parts.push(part);
        toolsByCallId.set(runtimeId, part);
        if (runtimeType === 'item.completed') canonicalInputDeltas.delete(runtimeId);
        continue;
      }

      if (runtimeType === 'turn.completed' || runtimeType === 'turn.aborted') {
        closeStreamingReasoning(parts);
        const failed =
          runtimeType === 'turn.aborted' ||
          ['failed', 'cancelled', 'interrupted'].includes(stringValue(payload.state));
        closeRunningTools(parts, failed ? 'error' : 'completed');
        const message = stringValue(payload.errorMessage || payload.reason);
        if (failed && message) {
          parts.push({
            id,
            type: 'system',
            label: runtimeType === 'turn.aborted' ? 'turn aborted' : 'provider error',
            text: message,
            tone: 'error',
            time: runtimeTime,
          });
        }
        continue;
      }

      if (runtimeType === 'runtime.error' || runtimeType === 'runtime.warning') {
        const message = stringValue(payload.message);
        if (!message) continue;
        parts.push({
          id,
          type: 'system',
          label: runtimeType === 'runtime.error' ? 'provider error' : 'provider warning',
          text: message,
          tone: runtimeType === 'runtime.error' ? 'error' : 'diagnostic',
          time: runtimeTime,
        });
        continue;
      }

      if (runtimeType === 'request.opened' || runtimeType === 'user-input.requested') {
        parts.push({
          id,
          type: 'system',
          label: runtimeType === 'request.opened' ? 'approval required' : 'input required',
          text:
            stringValue(payload.detail) ||
            (runtimeType === 'request.opened'
              ? 'The provider is waiting for approval.'
              : 'The provider is waiting for an answer.'),
          tone: 'info',
          time: runtimeTime,
        });
      }
      // Session metadata, token usage, rate limits, and resolved requests are
      // canonical state updates, not transcript prose.
      continue;
    }

    if (eventType === 'text_chunk') {
      const stream = stringValue(event.stream);
      const chunk = stringValue(event.chunk);
      if (!chunk.trim()) continue;
      if (canonicalChat && stream === 'user' && promptBundle && chunk === promptBundle) continue;
      if (canonicalChat && stream === 'user' && consumeComposerSend(unmatchedComposerSends, chunk)) {
        continue;
      }

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
        if (canonicalChat || isIgnoredStderr(chunk)) continue;
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

    // 'pane_activity' is a content-free pane liveness signal,
    // rendered nowhere for the same reason heartbeats are not.
    if (eventType === 'ready' || eventType === 'heartbeat' || eventType === 'pane_activity')
      continue;

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
      if (
        canonicalChat &&
        stringValue(event.phase) === 'composer_send' &&
        promptBundle &&
        stringValue(event.text) === promptBundle
      ) {
        continue;
      }
      if (stringValue(event.phase) === 'release') {
        terminalToolState =
          stringValue(event.outcome) === 'failed' ? 'error' : (terminalToolState ?? 'completed');
      }
      const lifecyclePart = normalizeLifecyclePart(id, envelope.time, event, canonicalChat);
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
    const type = stringValue(envelope.event?.type);
    const runtime = type === 'provider_runtime' && isRecord(envelope.event?.event)
      ? envelope.event.event
      : null;
    const runtimeType = runtime ? stringValue(runtime.type) : '';
    const isTerminal =
      type === 'run_complete' ||
      type === 'run_fail' ||
      type === 'driver_error' ||
      runtimeType === 'turn.completed' ||
      runtimeType === 'turn.aborted';
    if (!isTerminal) return false;
    const eventTime = Date.parse(stringValue(runtime?.createdAt) || envelope.time || '');
    return Number.isFinite(eventTime) && eventTime >= pendingTime;
  });
}

function normalizeLifecyclePart(
  id: string,
  time: string | undefined,
  event: Record<string, unknown>,
  canonicalChat = false,
): TranscriptPart | null {
  const phase = stringValue(event.phase);
  if (phase === 'composer_send') {
    const text = stringValue(event.text);
    if (!text.trim()) return null;
    return { id, type: 'text', role: 'user', label: 'user', text, time };
  }
  if (phase === 'acquire') {
    if (canonicalChat) return null;
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
    if (canonicalChat && outcome !== 'failed') return null;
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

function composerSendKey(text: string): string {
  return text.trim();
}

function composerSendCounts(envelopes: SessionEnvelope[]): Map<string, number> {
  const counts = new Map<string, number>();
  for (const envelope of envelopes) {
    if (envelope.kind !== 'lifecycle') continue;
    const event = envelope.event ?? {};
    if (stringValue(event.phase) !== 'composer_send') continue;
    const key = composerSendKey(stringValue(event.text));
    if (!key) continue;
    counts.set(key, (counts.get(key) ?? 0) + 1);
  }
  return counts;
}

function consumeComposerSend(counts: Map<string, number>, text: string): boolean {
  const key = composerSendKey(text);
  const count = counts.get(key) ?? 0;
  if (!key || count === 0) return false;
  if (count === 1) counts.delete(key);
  else counts.set(key, count - 1);
  return true;
}

function canonicalContentPartId(
  runtime: Record<string, unknown>,
  fallbackId: string,
  streamKind: string,
): string {
  const itemId = stringValue(runtime.itemId || runtime.requestId).trim();
  if (itemId) return `${streamKind}:${itemId}`;

  const threadId = stringValue(runtime.threadId).trim();
  const turnId = stringValue(runtime.turnId).trim();
  const payload = isRecord(runtime.payload) ? runtime.payload : {};
  const contentIndex = payload.contentIndex;
  if (threadId && turnId && typeof contentIndex === 'number') {
    return `${streamKind}:${threadId}:${turnId}:${contentIndex}`;
  }
  if (threadId && turnId) return `${streamKind}:${threadId}:${turnId}`;
  return `${streamKind}:${fallbackId}`;
}

function parseJsonValue(value: string | undefined): unknown | undefined {
  if (!value) return undefined;
  try {
    return JSON.parse(value) as unknown;
  } catch {
    return undefined;
  }
}

function immediatelyPrecedingRunningCanonicalExec(
  parts: PartDraft[],
): TranscriptToolPart | undefined {
  const previous = parts.at(-1);
  return previous?.type === 'tool' && previous.name === 'exec' && previous.state === 'running'
    ? previous
    : undefined;
}

function canonicalProviderToolName(
  itemType: string,
  data: Record<string, unknown>,
  payload: Record<string, unknown>,
  existingName?: string,
): string {
  const semanticName = stringValue(itemType).trim();
  if (semanticName && semanticName !== 'tool_call') return semanticName;
  const explicitName = stringValue(data.toolName || data.tool).trim();
  if (explicitName) return explicitName;
  if (existingName && existingName !== 'tool_call') return existingName;
  return compactLabel(payload.title || semanticName || 'tool');
}

function summarizeCanonicalProviderTool(
  name: string,
  input: unknown,
  payload: Record<string, unknown>,
  existing?: TranscriptToolPart,
): ToolSummary {
  const base = summarizeToolCall(name, input);
  if (name === 'command_execution') {
    return {
      label: 'Ran command',
      summary:
        providerCommand(input) ??
        (existing?.label === 'Ran command' ? existing.summary : undefined),
      meta: base.meta,
    };
  }

  const inputRecord = isRecord(input) ? input : {};
  if (name === 'web_search') {
    const url = stringValue(inputRecord.url).trim();
    const query = stringValue(inputRecord.query || inputRecord.searchTerm).trim();
    return {
      label: url ? 'Fetched page' : 'Searched web',
      summary: trimMiddle(url || query, 180) || existing?.summary,
      meta: base.meta,
    };
  }
  if (name === 'file_read') {
    const path = providerPath(inputRecord);
    return {
      label: 'Read file',
      summary: path ? trimMiddle(path, 180) : existing?.summary,
      meta: base.meta,
    };
  }
  if (name === 'file_change') {
    const path = providerPath(inputRecord);
    return {
      label: 'Changed files',
      summary: path ? trimMiddle(path, 180) : existing?.summary,
      meta: base.meta,
    };
  }
  if (name === 'agent_task') {
    const task = stringValue(
      inputRecord.description || inputRecord.prompt || inputRecord.task || inputRecord.message,
    ).trim();
    return {
      label: 'Delegated task',
      summary: task ? trimMiddle(firstLine(task), 180) : existing?.summary,
      meta: base.meta,
    };
  }

  const title = stringValue(payload.title).trim();
  const semanticTitle = title.toLowerCase() === 'tool' ? '' : title;
  return {
    label: semanticTitle ? trimMiddle(semanticTitle, 96) : existing?.label ?? base.label,
    summary: existing?.summary,
    meta: base.meta,
  };
}

function providerPath(input: Record<string, unknown>): string {
  return stringValue(
    input.path || input.filePath || input.filename || input.newPath || input.oldPath,
  ).trim();
}

function providerCommand(input: unknown): string | undefined {
  if (!isRecord(input)) return undefined;
  const actions = Array.isArray(input.commandActions) ? input.commandActions : [];
  for (const action of actions) {
    if (!isRecord(action)) continue;
    const command = stringValue(action.command).trim();
    if (command) return command;
  }
  const command = stringValue(input.command || input.cmd).trim();
  return command || undefined;
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
    if (part.type === 'tool' && (part.state === 'running' || part.state === 'streaming')) {
      part.state = state;
    }
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

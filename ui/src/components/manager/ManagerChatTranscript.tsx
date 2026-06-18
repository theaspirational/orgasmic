// @arch arch_MK2Q2.5
import { useCallback, useEffect, useMemo, useRef } from 'react';

import { ScrollArea } from '@/components/ui/scroll-area';
import { useEventStream } from '@/hooks/useEventStream';
import { fetchRun } from '@/lib/api';
import type { DaemonEvent } from '@/lib/types';
import {
  coalesceTextChunks,
  extractPromptBundle,
  groupToolEntries,
  selectGroupSummary,
  stripAnsi,
  transcriptBlocks,
  type ActivityDetail,
  type GroupDisplayItem,
  type PairedCommand,
  type TranscriptBlock,
  type TranscriptEntry,
  type TranscriptRole,
} from '@/lib/transcriptUtils';
import { useResource } from '@/lib/useResource';

type SessionEnvelope = {
  seq?: number;
  time?: string;
  kind?: string;
  event?: Record<string, unknown>;
};

function stringifyValue(value: unknown): string {
  if (typeof value === 'string') return value;
  if (value === null || value === undefined) return '';
  return JSON.stringify(value, null, 2);
}

function compactLabel(value: unknown): string {
  const text = stringifyValue(value).trim();
  return text || 'unknown';
}

function usefulJson(value: unknown): string {
  if (value === null || value === undefined) return '';
  if (typeof value === 'string') return value;
  if (typeof value === 'object' && Object.keys(value).length === 0) return '';
  return stringifyValue(value);
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

function trimMiddle(text: string, max = 160): string {
  if (text.length <= max) return text;
  const head = Math.max(0, Math.floor((max - 3) * 0.62));
  const tail = Math.max(0, max - 3 - head);
  return `${text.slice(0, head)}...${text.slice(text.length - tail)}`;
}

function relativePath(path: unknown, cwd: unknown): string {
  const text = stringifyValue(path);
  const base = stringifyValue(cwd);
  if (base && text.startsWith(`${base}/`)) return text.slice(base.length + 1);
  return text;
}

function firstLine(text: string): string {
  return text.split('\n').find((line) => line.trim())?.trim() ?? '';
}

function sameLocalDate(left: Date, right: Date): boolean {
  return (
    left.getFullYear() === right.getFullYear() &&
    left.getMonth() === right.getMonth() &&
    left.getDate() === right.getDate()
  );
}

function formatTranscriptTime(value: string): string {
  const date = new Date(value);
  if (!Number.isFinite(date.getTime())) return value;

  const now = new Date();
  if (sameLocalDate(date, now)) {
    return new Intl.DateTimeFormat(undefined, {
      hour: '2-digit',
      minute: '2-digit',
    }).format(date);
  }

  const options: Intl.DateTimeFormatOptions = {
    month: 'short',
    day: 'numeric',
    hour: '2-digit',
    minute: '2-digit',
  };
  if (date.getFullYear() !== now.getFullYear()) options.year = 'numeric';
  return new Intl.DateTimeFormat(undefined, options).format(date);
}

function parseJsonl(source: string): SessionEnvelope[] {
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

function isCodexIconLoaderWarning(text: string): boolean {
  return text.includes('codex_core_skills::loader') && text.includes("icon path must not contain '..'");
}

function isDiagnosticInfoLogLine(text: string): boolean {
  const isInfoOrDebug = text.includes(' [INFO] ') || text.includes(' [DEBUG] ');
  if (!isInfoOrDebug) return false;
  return /^\d{4}-\d{2}-\d{2}\s+\d{2}:\d{2}:\d{2}\s+\[(INFO|DEBUG)\]\s+[\w.:_-]+:/.test(text);
}

function textChunkEntry(
  id: string,
  stream: string,
  text: string,
  time?: string,
): TranscriptEntry | null {
  if (!text.trim()) return null;
  if (stream === 'stderr' && isCodexIconLoaderWarning(text)) return null;
  if (stream === 'stderr' && isDiagnosticInfoLogLine(text)) return null;
  // stderr is never a chat bubble: route to a collapsed diagnostics activity row
  if (stream === 'stderr') {
    const clean = stripAnsi(text);
    return {
      id,
      role: 'tool',
      label: 'diagnostics',
      text: clean,
      time,
      mergeKey: 'tool:stderr-diagnostics',
      activity: {
        summary: firstLine(clean),
        preview: clean,
        raw: text,
      },
    };
  }
  if (stream === 'system') {
    const textPrefix = text.trim().slice(0, 96);
    const isProviderWarning = /warning/i.test(textPrefix);
    return {
      id,
      role: isProviderWarning ? 'system' : 'work',
      label: isProviderWarning ? 'provider warning' : 'thinking',
      text: isProviderWarning ? text : '',
      time,
      mergeKey: isProviderWarning ? 'system:provider-warning' : 'work:thinking',
    };
  }
  if (stream === 'stdout') {
    return {
      id,
      role: 'tool',
      label: 'stdout',
      text,
      time,
      mergeKey: 'tool:stdout',
    };
  }
  const role: TranscriptRole =
    stream === 'assistant' ? 'assistant' : stream === 'user' ? 'user' : 'system';
  return {
    id,
    role,
    label: stream || 'text',
    text,
    time,
    mergeKey: stream || 'text',
  };
}

function commandActionSummary(action: unknown, cwd: unknown): string | null {
  if (!isRecord(action)) return null;
  const type = stringifyValue(action.type);
  const command = stringifyValue(action.command);
  const name = stringifyValue(action.name);
  const path = relativePath(action.path, cwd);

  if (type === 'read') {
    return `read ${name || path || command}`;
  }
  if (type === 'search') {
    const query = stringifyValue(action.query);
    return `search ${name || path}${query ? ` for ${query}` : ''}`;
  }
  if (type && type !== 'unknown') {
    return `${type} ${name || path || command}`;
  }
  return command ? `run ${command}` : null;
}

function summarizeToolCall(name: string, args: unknown): ActivityDetail {
  const raw = usefulJson(args);
  if (!isRecord(args)) return { raw, preview: raw };

  const meta: Array<[string, string]> = [];
  const cwd = args.workdir ?? args.cwd;
  if (cwd) meta.push(['cwd', trimMiddle(stringifyValue(cwd), 96)]);

  if (name === 'exec_command') {
    const cmd = stringifyValue(args.cmd);
    const summary = cmd ? `run ${trimMiddle(cmd, 140)}` : 'run command';
    if (args.yield_time_ms !== undefined) meta.push(['wait', `${stringifyValue(args.yield_time_ms)}ms`]);
    if (args.max_output_tokens !== undefined) meta.push(['limit', `${stringifyValue(args.max_output_tokens)} tokens`]);
    return { label: 'command request', summary, meta, raw };
  }

  if (name === 'command_execution') {
    const actions = Array.isArray(args.commandActions) ? args.commandActions : [];
    const actionSummary = actions.map((action) => commandActionSummary(action, args.cwd)).find(Boolean);
    const command = stringifyValue(args.command);
    const status = stringifyValue(args.status);
    const processId = stringifyValue(args.processId);
    if (status) meta.push(['status', status]);
    if (processId) meta.push(['pid', processId]);
    return {
      label: 'command started',
      summary: actionSummary ?? (command ? `started ${trimMiddle(command, 140)}` : 'started command'),
      meta,
      raw,
    };
  }

  if (name === 'write_stdin') {
    const sessionId = stringifyValue(args.session_id);
    const chars = stringifyValue(args.chars);
    if (sessionId) meta.push(['session', sessionId]);
    return {
      label: 'terminal input',
      summary: chars ? `send ${chars.length} chars to terminal` : 'poll terminal',
      meta,
      raw,
    };
  }

  if (name === 'apply_patch') {
    return { label: 'patch', summary: 'apply patch', meta, raw };
  }

  return {
    summary: name ? `use ${name}` : 'tool call',
    meta,
    preview: raw,
    raw,
  };
}

function parseToolResultOutput(output: string): ActivityDetail {
  const lines = output.split('\n');
  const chunkId = /^Chunk ID:\s*(.+)$/.exec(lines[0] ?? '')?.[1]?.trim();
  const wallTime = /^Wall time:\s*(.+)$/.exec(lines[1] ?? '')?.[1]?.trim();
  const exitCode = lines
    .map((line) => /Process exited with code\s+(-?\d+)/.exec(line)?.[1])
    .find(Boolean);
  const tokenCount = lines
    .map((line) => /Original token count:\s*(\d+)/.exec(line)?.[1])
    .find(Boolean);
  const outputIndex = lines.findIndex((line) => line.trim() === 'Output:');
  const body = outputIndex >= 0 ? lines.slice(outputIndex + 1).join('\n').trim() : output.trim();
  const previewLines = body.split('\n').slice(0, 8).join('\n').trim();
  const summaryParts = [
    exitCode !== undefined ? `exit ${exitCode}` : '',
    tokenCount ? `${tokenCount} tokens` : '',
    wallTime ?? '',
  ].filter(Boolean);
  const meta: Array<[string, string]> = [];
  if (chunkId) meta.push(['chunk', chunkId]);

  return {
    summary: summaryParts.length ? summaryParts.join(' · ') : firstLine(output) || 'tool finished',
    meta,
    preview: previewLines,
    raw: output,
  };
}

function summarizeToolResult(ok: unknown, output: unknown): ActivityDetail {
  const raw = usefulJson(output);
  if (typeof output === 'string') {
    return {
      label: stringifyValue(ok) === 'false' ? 'command error' : 'command result',
      ...parseToolResultOutput(output),
    };
  }
  return {
    label: stringifyValue(ok) === 'false' ? 'tool error' : 'tool result',
    summary: stringifyValue(ok) === 'false' ? 'tool failed' : 'tool finished',
    preview: raw,
    raw,
  };
}

function transcriptEntries(source: string, promptOverride?: string | null): TranscriptEntry[] {
  const envelopes = parseJsonl(source);

  // Fix 1: inject opening user entry from driver_config.prompt_bundle_text.
  // Prefer the sticky override (retained across refreshes) so the prompt never
  // disappears when a live source refresh momentarily lacks the early
  // lifecycle envelope that carries it.
  const promptBundle = promptOverride ?? extractPromptBundle(envelopes);
  const promptEntry: TranscriptEntry | null = promptBundle
    ? {
        id: 'prompt-bundle',
        role: 'user',
        label: 'prompt',
        text: promptBundle.split('\n').slice(0, 6).join('\n'),
        activity: {
          preview: promptBundle.split('\n').slice(0, 6).join('\n'),
          raw: promptBundle,
        },
      }
    : null;

  const entries = envelopes.flatMap((envelope, index): TranscriptEntry[] => {
    const event = envelope.event ?? {};
    const id = `${envelope.seq ?? index}`;
    const type = stringifyValue(event.type);

    if (type === 'text_chunk') {
      const stream = stringifyValue(event.stream);
      const entry = textChunkEntry(id, stream, stringifyValue(event.chunk), envelope.time);
      return entry ? [entry] : [];
    }

    if (type === 'tool_call') {
      const name = compactLabel(event.name);
      const activity = summarizeToolCall(name, event.args);
      const callId = stringifyValue(event.call_id) || undefined;
      return [{
        id,
        role: 'tool',
        label: activity.label ?? `tool ${name}`,
        text: activity.preview ?? activity.summary ?? activity.raw ?? '',
        activity,
        time: envelope.time,
        callId,
      }];
    }

    if (type === 'tool_result') {
      const activity = summarizeToolResult(event.ok, event.output);
      const callId = stringifyValue(event.call_id) || undefined;
      return [{
        id,
        role: 'tool',
        label: activity.label ?? (stringifyValue(event.ok) === 'true' ? 'tool result' : 'tool error'),
        text: activity.preview ?? activity.summary ?? activity.raw ?? '',
        activity,
        time: envelope.time,
        callId,
      }];
    }

    if (type === 'transition_state') {
      const from = compactLabel(event.from);
      const to = compactLabel(event.to);
      const reason = stringifyValue(event.reason).trim();
      return [{
        id,
        role: 'work',
        label: 'state transition',
        text: reason ? `${from} -> ${to}\n${reason}` : `${from} -> ${to}`,
        time: envelope.time,
      }];
    }

    if (type === 'run_complete') return [];

    if (type === 'run_fail' || type === 'driver_error') {
      return [{
        id,
        role: 'system',
        label: type.replace('_', ' '),
        text: stringifyValue(event.error_markdown || event.message),
        time: envelope.time,
      }];
    }

    if (envelope.kind === 'lifecycle') return [];

    if (envelope.kind === 'note') {
      return [{
        id,
        role: 'system',
        label: 'note',
        text: stringifyValue(event),
        time: envelope.time,
      }];
    }

    return [];
  });
  const allEntries = promptEntry ? [promptEntry, ...entries] : entries;
  return coalesceTextChunks(allEntries);
}

function shouldRefreshTranscript(event: DaemonEvent, runId: string): boolean {
  if (event.topic === 'manager') return true;
  if (event.topic !== 'run') return false;
  const payloadRunId = event.payload.run_id;
  return typeof payloadRunId !== 'string' || payloadRunId === runId;
}

export function ManagerChatTranscript({
  runId,
  initialSource,
  pendingSince,
  onPendingResolved,
}: {
  runId: string;
  initialSource?: string | null;
  pendingSince?: string | null;
  onPendingResolved?: () => void;
}) {
  const bottomRef = useRef<HTMLDivElement | null>(null);
  const detail = useResource(`manager-chat:${runId}`, () => fetchRun(runId));

  useEventStream(
    useCallback(
      (event: DaemonEvent) => {
        if (shouldRefreshTranscript(event, runId)) void detail.refresh();
      },
      [detail, runId],
    ),
  );

  const source = detail.data?.source ?? initialSource ?? '';
  // Sticky prompt: extract from the live source, falling back to initialSource,
  // and retain the last non-null value so the opening prompt bubble can never
  // vanish on a later refresh whose source lacks the early lifecycle envelope.
  const promptRef = useRef<string | null>(null);
  const stickyPrompt = useMemo(() => {
    const found = extractPromptBundle(parseJsonl(source)) ?? extractPromptBundle(parseJsonl(initialSource ?? ''));
    if (found) promptRef.current = found;
    return promptRef.current;
  }, [source, initialSource]);
  const entries = useMemo(() => transcriptEntries(source, stickyPrompt), [source, stickyPrompt]);
  const blocks = useMemo(() => transcriptBlocks(entries), [entries]);
  const pendingResolved = useMemo(
    () => hasAssistantResponseAfterPending(entries, pendingSince) || hasTerminalEventAfterPending(source, pendingSince),
    [entries, pendingSince, source],
  );
  const showPending = Boolean(pendingSince && !pendingResolved);

  useEffect(() => {
    if (pendingResolved) onPendingResolved?.();
  }, [onPendingResolved, pendingResolved]);

  useEffect(() => {
    bottomRef.current?.scrollIntoView({ block: 'end' });
  }, [entries.length, showPending]);

  return (
    <ScrollArea className="h-full">
      <div className="flex min-h-full flex-col gap-3 p-4">
        {detail.error ? (
          <div className="rounded-md border border-destructive/40 bg-destructive/10 p-3 text-sm text-destructive">
            {detail.error instanceof Error ? detail.error.message : String(detail.error)}
          </div>
        ) : null}
        {entries.length === 0 ? (
          <div className="m-auto max-w-sm rounded-lg border bg-card p-4 text-center text-sm text-muted-foreground">
            No transcript events for this run yet.
          </div>
        ) : (
          blocks.map((block) => <TranscriptBlockView key={block.id} block={block} />)
        )}
        {showPending ? <ThinkingBubble /> : null}
        <div ref={bottomRef} />
      </div>
    </ScrollArea>
  );
}

function hasAssistantResponseAfterPending(
  entries: TranscriptEntry[],
  pendingSince: string | null | undefined,
): boolean {
  if (!pendingSince) return false;
  const pendingTime = Date.parse(pendingSince);
  if (!Number.isFinite(pendingTime)) return false;
  return entries.some((entry) => {
    if (!entry.time) return false;
    if (
      entry.role !== 'assistant' &&
      entry.label !== 'run fail' &&
      entry.label !== 'driver error'
    ) {
      return false;
    }
    const entryTime = Date.parse(entry.time);
    return Number.isFinite(entryTime) && entryTime >= pendingTime;
  });
}

function hasTerminalEventAfterPending(
  source: string,
  pendingSince: string | null | undefined,
): boolean {
  if (!pendingSince) return false;
  const pendingTime = Date.parse(pendingSince);
  if (!Number.isFinite(pendingTime)) return false;
  return parseJsonl(source).some((envelope) => {
    if (!envelope.time) return false;
    const type = stringifyValue(envelope.event?.type);
    if (type !== 'run_complete' && type !== 'run_fail' && type !== 'driver_error') return false;
    const eventTime = Date.parse(envelope.time);
    return Number.isFinite(eventTime) && eventTime >= pendingTime;
  });
}

function TranscriptBlockView({ block }: { block: TranscriptBlock }) {
  if (block.type === 'tool-group') {
    return <ToolGroupBlock entries={block.entries} />;
  }
  return <TranscriptEntryView entry={block.entry} />;
}

function TranscriptEntryView({ entry }: { entry: TranscriptEntry }) {
  if (entry.role === 'work' || entry.role === 'tool') {
    return <ActivityRow entry={entry} />;
  }
  return <TranscriptBubble entry={entry} />;
}

function ToolGroupBlock({ entries }: { entries: TranscriptEntry[] }) {
  const firstTime = entries[0]?.time;
  const commandCount = entries.filter((e) => e.label === 'command request').length;
  const resultCount = entries.filter((e) => e.label === 'command result' || e.label === 'command error').length;
  const isLive = commandCount > resultCount;
  const recentSummary = selectGroupSummary(entries);
  const items: GroupDisplayItem[] = groupToolEntries(entries);
  const countLabel = [
    commandCount ? `${commandCount} cmd${commandCount === 1 ? '' : 's'}` : '',
    isLive ? 'running' : '',
  ].filter(Boolean).join(' · ');

  return (
    <details className="max-w-[min(680px,90%)] self-start rounded-md border border-border/70 bg-muted/25 px-2.5 py-1.5 text-muted-foreground">
      <summary className="flex cursor-pointer select-none items-center gap-2 text-xs marker:text-muted-foreground">
        <span className={`size-1.5 shrink-0 rounded-full ${isLive ? 'bg-amber-500/70' : 'bg-amber-500/40'}`} aria-hidden="true" />
        <span className="font-medium text-foreground/80">Tool calls</span>
        <span className="min-w-0 truncate text-foreground/70">{recentSummary}</span>
        {countLabel ? <span className="shrink-0 text-muted-foreground/60">{countLabel}</span> : null}
        {firstTime ? <TranscriptTime className="ml-auto shrink-0 font-mono text-[11px]" value={firstTime} /> : null}
      </summary>
      <div className="mt-2 flex flex-col gap-1.5">
        {items.map((item) =>
          item.type === 'paired' ? (
            <PairedCommandRow key={item.command.callId} command={item.command} />
          ) : (
            <ActivityRow key={item.entry.id} entry={item.entry as TranscriptEntry} />
          ),
        )}
      </div>
    </details>
  );
}

function PairedCommandRow({ command }: { command: PairedCommand }) {
  const isRunning = command.status === 'running';
  const isError = !isRunning && command.status !== 'exit 0';
  const dotColor = isRunning ? 'bg-amber-500/70' : isError ? 'bg-red-500/70' : 'bg-green-500/70';
  const statusColor = isRunning
    ? 'text-amber-600 dark:text-amber-400'
    : isError
      ? 'text-red-600 dark:text-red-400'
      : 'text-green-700 dark:text-green-400';

  return (
    <article className="rounded-md border border-border/70 bg-muted/35 px-2.5 py-1.5 text-muted-foreground">
      <div className="flex min-w-0 items-center gap-2 text-xs">
        <span className={`size-1.5 shrink-0 rounded-full ${dotColor}`} aria-hidden="true" />
        <span className="min-w-0 flex-1 truncate font-medium text-foreground/80">{command.summary}</span>
        <span className={`shrink-0 font-mono text-[11px] ${statusColor}`}>{command.status}</span>
      </div>
      {command.meta.length > 0 ? (
        <dl className="mt-1 flex flex-wrap gap-1 text-[11px]">
          {command.meta.map(([key, value]) => (
            <div key={`${key}:${value}`} className="flex min-w-0 items-center gap-1 rounded border border-border/70 bg-background/40 px-1.5 py-0.5">
              <dt className="shrink-0 text-muted-foreground">{key}</dt>
              <dd className="min-w-0 truncate font-mono text-foreground/75">{value}</dd>
            </div>
          ))}
        </dl>
      ) : null}
      {command.raw ? (
        <details className="mt-1 text-xs">
          <summary className="cursor-pointer select-none text-muted-foreground hover:text-foreground">Raw details</summary>
          <pre className="mt-1 max-h-48 overflow-auto whitespace-pre-wrap break-words rounded border border-border/60 bg-background/35 p-2 font-mono text-[11px] leading-relaxed text-foreground/75">
            {command.raw}
          </pre>
        </details>
      ) : null}
    </article>
  );
}

function TranscriptBubble({ entry }: { entry: TranscriptEntry }) {
  const align = entry.role === 'assistant' ? 'self-start' : entry.role === 'user' ? 'self-end' : 'self-center';
  const tone =
    entry.role === 'assistant'
      ? 'border-teal-500/25 bg-teal-500/5'
      : entry.role === 'system'
        ? 'border-border bg-muted/50'
        : 'border-blue-500/25 bg-blue-500/5';
  const fullText = entry.activity?.raw;
  const isCollapsible = Boolean(fullText && fullText !== entry.text);

  return (
    <article className={`max-w-[min(720px,85%)] rounded-lg border p-3 ${align} ${tone}`}>
      <div className="mb-1 flex items-center gap-2 text-[11px] uppercase text-muted-foreground">
        <span>{entry.label}</span>
        {entry.time ? <TranscriptTime className="font-mono normal-case" value={entry.time} /> : null}
      </div>
      <pre className="whitespace-pre-wrap break-words font-sans text-sm leading-relaxed">{entry.text}</pre>
      {isCollapsible ? (
        <details className="mt-2 text-xs">
          <summary className="cursor-pointer select-none text-muted-foreground hover:text-foreground">
            Full content ({Math.ceil(fullText!.length / 1024)} KB)
          </summary>
          <pre className="mt-1 max-h-96 overflow-auto whitespace-pre-wrap break-words rounded border border-border/60 bg-background/35 p-2 font-sans text-xs leading-relaxed text-foreground/80">
            {fullText}
          </pre>
        </details>
      ) : null}
    </article>
  );
}

function ActivityRow({ entry }: { entry: TranscriptEntry }) {
  const isTool = entry.role === 'tool';
  const activity = entry.activity;
  const summary = activity?.summary?.trim();
  const meta = activity?.meta?.filter(([, value]) => value.trim()) ?? [];
  const preview = (activity?.preview ?? (!activity ? entry.text : '')).trim();
  const raw = activity?.raw?.trim();
  const showRaw = Boolean(raw && raw !== preview && raw !== summary);
  return (
    <article className="max-w-[min(760px,92%)] self-start rounded-md border border-border/70 bg-muted/35 px-3 py-2 text-muted-foreground">
      <div className="flex min-w-0 items-center gap-2 text-xs">
        <span
          className={`size-1.5 shrink-0 rounded-full ${isTool ? 'bg-amber-500/70' : 'bg-teal-500/70'}`}
          aria-hidden="true"
        />
        <span className="min-w-0 truncate font-medium text-foreground/80">{entry.label}</span>
        {entry.time ? <TranscriptTime className="ml-auto shrink-0 font-mono text-[11px]" value={entry.time} /> : null}
      </div>
      {summary ? (
        <p className="mt-1.5 break-words text-sm leading-snug text-foreground">{summary}</p>
      ) : null}
      {meta.length ? (
        <dl className="mt-1.5 flex flex-wrap gap-1.5 text-[11px]">
          {meta.map(([key, value]) => (
            <div key={`${key}:${value}`} className="flex min-w-0 max-w-full items-center gap-1 rounded border border-border/70 bg-background/40 px-1.5 py-0.5">
              <dt className="shrink-0 text-muted-foreground">{key}</dt>
              <dd className="min-w-0 truncate font-mono text-foreground/75">{value}</dd>
            </div>
          ))}
        </dl>
      ) : null}
      {preview ? (
        <pre className="mt-2 max-h-36 overflow-auto whitespace-pre-wrap break-words rounded border border-border/60 bg-background/35 p-2 font-sans text-xs leading-relaxed text-foreground/80">
          {preview}
        </pre>
      ) : null}
      {showRaw ? (
        <details className="mt-2 text-xs">
          <summary className="text-muted-foreground hover:text-foreground">Raw details</summary>
          <pre className="mt-1 max-h-48 overflow-auto whitespace-pre-wrap break-words rounded border border-border/60 bg-background/35 p-2 font-mono text-[11px] leading-relaxed text-foreground/75">
            {raw}
          </pre>
        </details>
      ) : null}
    </article>
  );
}

function TranscriptTime({ value, className }: { value: string; className?: string }) {
  return (
    <time className={className} dateTime={value} title={value}>
      {formatTranscriptTime(value)}
    </time>
  );
}

function ThinkingBubble() {
  return (
    <article
      className="max-w-[min(360px,85%)] self-start rounded-lg border border-teal-500/25 bg-teal-500/5 p-3"
      aria-live="polite"
      aria-label="Agent is thinking"
    >
      <div className="mb-1 text-[11px] uppercase text-muted-foreground">assistant</div>
      <div className="inline-flex items-center gap-2 text-sm leading-relaxed text-muted-foreground">
        <span>Thinking</span>
        <span className="inline-flex items-end gap-1" aria-hidden="true">
          <span className="manager-thinking-dot" />
          <span className="manager-thinking-dot" />
          <span className="manager-thinking-dot" />
        </span>
      </div>
    </article>
  );
}

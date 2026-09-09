import { useEffect, useState } from 'react';

import {
  Conversation,
  ConversationContent,
  ConversationEmptyState,
  ConversationScrollButton,
} from '@/components/ai-elements/conversation';
import { CodeBlock } from '@/components/ai-elements/code-block';
import {
  Message,
  MessageContent,
  MessageResponse,
} from '@/components/ai-elements/message';
import {
  Reasoning,
  ReasoningContent,
  ReasoningTrigger,
} from '@/components/ai-elements/reasoning';
import {
  Tool,
  ToolContent,
  ToolHeader,
  ToolInput,
  ToolOutput,
} from '@/components/ai-elements/tool';
import { useTranscriptStream } from '@/hooks/useTranscriptStream';
import { parseContextBlock } from '@/lib/conversations';
import {
  type TranscriptPart,
  type TranscriptReasoningPart,
  type TranscriptSystemPart,
  type TranscriptTextPart,
  type TranscriptToolPart,
} from '@/lib/transcriptParts';

import { ContextChips } from './ContextChips';

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
  const detail = useTranscriptStream(runId, initialSource);
  const parts = detail.parts;
  const pendingResolved = Boolean(
    pendingSince && detail.responseAt >= Date.parse(pendingSince),
  );
  const showPending = Boolean(pendingSince && !pendingResolved);

  useEffect(() => {
    if (pendingResolved) onPendingResolved?.();
  }, [onPendingResolved, pendingResolved]);

  return (
    <Conversation className="h-full min-h-0">
      <ConversationContent className="mx-auto min-h-full w-full max-w-3xl gap-4 px-4 py-6 sm:px-6">
        {detail.error ? (
          <div
            className="rounded-md border border-destructive/40 bg-destructive/10 p-3 text-sm text-destructive"
            role="alert"
          >
            {detail.error}
          </div>
        ) : null}
        {parts.length === 0 && !showPending ? (
          <ConversationEmptyState
            className="min-h-48"
            description={
              detail.loading
                ? 'Loading the live session and its latest events.'
                : 'Send a message below to guide the agent. Its work will appear here as it happens.'
            }
            title={
              detail.loading
                ? 'Connecting to transcript…'
                : 'Ready for direction'
            }
          />
        ) : (
          <TranscriptPartsView parts={parts} />
        )}
        {showPending ? <ThinkingPlaceholder /> : null}
      </ConversationContent>
      <ConversationScrollButton
        aria-label="Scroll to latest transcript event"
        title="Scroll to latest transcript event"
      />
    </Conversation>
  );
}

/** Activity stays compact between prose messages; expanded order is unchanged. */
type ActivityPart = TranscriptToolPart | TranscriptReasoningPart;
export function TranscriptPartsView({ parts }: { parts: TranscriptPart[] }) {
  const groups: Array<TranscriptPart | ActivityPart[]> = [];
  for (const part of parts) {
    const previous = groups.at(-1);
    if (part.type === 'tool' || part.type === 'reasoning') {
      if (Array.isArray(previous)) previous.push(part);
      else groups.push([part]);
    } else groups.push(part);
  }
  return groups.map((group) =>
    Array.isArray(group) ? (
      <TranscriptActivity key={group[0].id} activity={group} />
    ) : (
      <TranscriptPartView key={group.id} part={group} />
    ),
  );
}

function TranscriptActivity({ activity }: { activity: ActivityPart[] }) {
  const tools = activity.filter(
    (part): part is TranscriptToolPart => part.type === 'tool',
  );
  const [open, setOpen] = useState(false);
  const errors = tools.filter((tool) => tool.state === 'error').length;
  const running = activity.some(
    (part) => part.state === 'running' || part.state === 'streaming',
  );
  if (!tools.length)
    return activity.map((part) => (
      <TranscriptPartView key={part.id} part={part} />
    ));
  return (
    <div className="w-full min-w-0 text-sm" data-testid="transcript-activity">
      <details
        className="peer"
        onToggle={(event) => setOpen(event.currentTarget.open)}
      >
        <summary className="cursor-pointer rounded-sm text-muted-foreground outline-offset-4 focus-visible:outline-2 focus-visible:outline-ring">
          {running ? 'Working' : 'Activity'} · {tools.length}{' '}
          {tools.length === 1 ? 'tool' : 'tools'}
          {activity.length > tools.length && ' · reasoning'}
          {errors > 0 && (
            <span className="ml-2 text-destructive">{errors} failed</span>
          )}
        </summary>
        <div className="mt-3 space-y-2">
          {open &&
            activity.map((part) => (
              <TranscriptPartView key={part.id} part={part} />
            ))}
        </div>
      </details>
      <div className="mt-2 space-y-2 peer-open:hidden" aria-hidden="true">
        {tools.slice(-3).map((tool) => (
          <p
            key={tool.id}
            className="truncate text-muted-foreground"
            title={`${tool.label} ${tool.summary ?? ''}`}
          >
            {tool.label} {tool.summary}
            {tool.state === 'error' ? ' — failed' : ''}
          </p>
        ))}
      </div>
    </div>
  );
}

function TranscriptPartView({ part }: { part: TranscriptPart }) {
  if (part.type === 'text') return <TranscriptMessage part={part} />;
  if (part.type === 'reasoning') return <TranscriptReasoning part={part} />;
  if (part.type === 'tool') return <TranscriptToolCard part={part} />;
  return <TranscriptSystemEvent part={part} />;
}

function TranscriptMessage({ part }: { part: TranscriptTextPart }) {
  // A conversation send carries its chips in a delimited block (CHAT-SCOPE
  // C2): show them as chips and only the operator's text as the message. The
  // scope prompt or tail the daemon put before the block stays under "Full
  // content".
  const block = part.role === 'user' ? parseContextBlock(part.text) : null;
  const text = block ? block.message : part.text;
  const fullText = part.fullText ?? (block?.prefix ? part.text : undefined);
  const showFullText = Boolean(fullText && fullText !== text);
  return (
    <Message className="max-w-[min(720px,95%)]" from={part.role}>
      <MessageContent
        className={part.role === 'assistant' ? 'w-full' : undefined}
      >
        <TranscriptMeta label={part.label} time={part.time} />
        {block?.chips.length ? <ContextChips chips={block.chips} className="mb-1" /> : null}
        {part.role === 'assistant' ? (
          <MessageResponse>{text}</MessageResponse>
        ) : (
          <p className="whitespace-pre-wrap break-words leading-relaxed">
            {text}
          </p>
        )}
        {showFullText ? (
          <details className="text-xs">
            <summary className="cursor-pointer select-none text-muted-foreground hover:text-foreground">
              Full content ({Math.ceil(fullText!.length / 1024)} KB)
            </summary>
            <pre className="mt-2 max-h-96 overflow-auto whitespace-pre-wrap break-words rounded-md border bg-background/60 p-3 font-sans text-xs leading-relaxed text-foreground/80">
              {fullText}
            </pre>
          </details>
        ) : null}
      </MessageContent>
    </Message>
  );
}

function TranscriptReasoning({ part }: { part: TranscriptReasoningPart }) {
  const isStreaming = part.state === 'streaming';
  return (
    <div className="w-full max-w-[min(720px,95%)] self-start">
      <TranscriptMeta label={part.label} time={part.time} />
      <Reasoning defaultOpen={false} isStreaming={isStreaming}>
        <ReasoningTrigger />
        <ReasoningContent>{part.text}</ReasoningContent>
      </Reasoning>
    </div>
  );
}

export function TranscriptToolCard({ part }: { part: TranscriptToolPart }) {
  const hasInput = hasDisplayValue(part.input);
  const hasOutput = hasDisplayValue(part.output);
  const isCanonicalActivity = [
    'command_execution',
    'file_read',
    'file_change',
    'web_search',
    'agent_task',
  ].includes(part.name);
  const title = part.summary
    ? isCanonicalActivity
      ? `${part.label} ${part.summary}`
      : `${part.label}: ${part.summary}`
    : part.label || part.name;
  return (
    <Tool
      className="mb-0 w-full max-w-[min(760px,95%)] self-start bg-card"
      data-testid={`transcript-tool-${part.id}`}
      defaultOpen={part.state === 'error'}
    >
      <ToolHeader name={part.name} state={part.state} title={title} />
      <ToolContent>
        <TranscriptMeta label={part.name} time={part.time} />
        {part.meta.length > 0 ? <ToolMeta meta={part.meta} /> : null}
        {hasInput ? <ToolInput input={part.input} /> : null}
        {hasOutput || part.state === 'error' ? (
          <ToolOutput
            errorText={
              part.state === 'error' ? 'Tool returned an error.' : undefined
            }
            output={part.output}
          />
        ) : null}
      </ToolContent>
    </Tool>
  );
}

function ToolMeta({ meta }: { meta: Array<[string, string]> }) {
  return (
    <dl className="flex flex-wrap gap-1.5 text-[11px]">
      {meta.map(([key, value]) => (
        <div
          key={`${key}:${value}`}
          className="flex min-w-0 max-w-full items-center gap-1 rounded border bg-background/60 px-1.5 py-0.5"
        >
          <dt className="shrink-0 text-muted-foreground">{key}</dt>
          <dd
            className="min-w-0 truncate font-mono text-foreground/80"
            title={value}
          >
            {value}
          </dd>
        </div>
      ))}
    </dl>
  );
}

function TranscriptSystemEvent({ part }: { part: TranscriptSystemPart }) {
  const isError = part.tone === 'error';
  return (
    <article
      className={`w-full max-w-[min(760px,95%)] self-center rounded-md border p-3 text-sm ${
        isError
          ? 'border-destructive/40 bg-destructive/10 text-destructive'
          : 'border-border bg-muted/40 text-foreground'
      }`}
    >
      <TranscriptMeta label={part.label} time={part.time} />
      {part.code ? (
        <CodeBlock
          className="mt-2 max-h-64 overflow-auto"
          code={part.text}
          language="console"
        />
      ) : (
        <p className="mt-1 whitespace-pre-wrap break-words leading-relaxed">
          {part.text}
        </p>
      )}
      {part.fullText && part.fullText !== part.text ? (
        <details className="mt-2 text-xs">
          <summary className="cursor-pointer select-none text-muted-foreground hover:text-foreground">
            Raw details
          </summary>
          <pre className="mt-2 max-h-48 overflow-auto whitespace-pre-wrap break-words rounded-md border bg-background/60 p-3 font-mono text-[11px] leading-relaxed text-foreground/80">
            {part.fullText}
          </pre>
        </details>
      ) : null}
    </article>
  );
}

function TranscriptMeta({ label, time }: { label: string; time?: string }) {
  return (
    <div className="flex items-center gap-2 text-[11px] uppercase text-muted-foreground">
      <span>{label}</span>
      {time ? (
        <TranscriptTime className="font-mono normal-case" value={time} />
      ) : null}
    </div>
  );
}

function TranscriptTime({
  value,
  className,
}: {
  value: string;
  className?: string;
}) {
  return (
    <time className={className} dateTime={value} title={value}>
      {formatTranscriptTime(value)}
    </time>
  );
}

function ThinkingPlaceholder() {
  return (
    <Message aria-label="Agent is thinking" aria-live="polite" from="assistant">
      <MessageContent>
        <TranscriptMeta label="assistant" />
        <Reasoning defaultOpen={false} isStreaming>
          <ReasoningTrigger />
        </Reasoning>
      </MessageContent>
    </Message>
  );
}

function hasDisplayValue(value: unknown): boolean {
  if (value === null || value === undefined || value === '') return false;
  if (Array.isArray(value)) return value.length > 0;
  if (typeof value === 'object') return Object.keys(value).length > 0;
  return true;
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

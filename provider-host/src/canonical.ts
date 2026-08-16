import { randomUUID } from "node:crypto";

export type ProviderId = "claude" | "opencode";

export type ContentStreamKind =
  | "assistant_text"
  | "reasoning_text"
  | "command_output"
  | "tool_output";

export type ItemLifecyclePayload = {
  itemType: string;
  status?: "pending" | "inProgress" | "completed" | "failed";
  title?: string;
  detail?: string;
  data?: unknown;
};

export type ProviderRuntimeEvent = {
  eventId: string;
  provider: ProviderId;
  threadId: string;
  createdAt: string;
  turnId?: string;
  itemId?: string;
  requestId?: string;
  providerRefs?: Record<string, unknown>;
  raw?: unknown;
} & (
  | { type: "session.started"; payload: { message?: string; resume?: unknown } }
  | {
      type: "session.exited";
      payload: { reason?: string; recoverable?: boolean; exitKind?: string };
    }
  | { type: "thread.metadata.updated"; payload: { name?: string; metadata?: unknown } }
  | { type: "thread.token-usage.updated"; payload: { usage: unknown } }
  | { type: "turn.started"; payload: { model?: string; effort?: string } }
  | {
      type: "turn.completed";
      payload: {
        state: string;
        stopReason?: string;
        usage?: unknown;
        modelUsage?: unknown;
        totalCostUsd?: number;
        errorMessage?: string;
      };
    }
  | { type: "turn.aborted"; payload: { reason: string } }
  | { type: "item.started" | "item.updated" | "item.completed"; payload: ItemLifecyclePayload }
  | {
      type: "content.delta";
      payload: {
        streamKind: ContentStreamKind;
        delta: string;
        contentIndex?: number;
        summaryIndex?: number;
      };
    }
  | {
      type: "request.opened" | "request.resolved";
      payload: {
        requestType?: string;
        detail?: string;
        decision?: string;
        args?: unknown;
        resolution?: unknown;
      };
    }
  | {
      type: "user-input.requested" | "user-input.resolved";
      payload: { questions?: unknown; answers?: unknown };
    }
  | {
      type: "account.rate-limits.updated";
      payload: { rateLimits?: unknown; detail?: unknown };
    }
  | {
      type: "runtime.warning" | "runtime.error";
      payload: { message?: string; class?: string; detail?: unknown };
    }
);

export type EventContext = {
  provider: ProviderId;
  threadId: string;
  turnId?: string;
  itemId?: string;
  requestId?: string;
  providerRefs?: Record<string, unknown>;
  raw?: unknown;
};

type EventBody = ProviderRuntimeEvent extends infer Event
  ? Event extends ProviderRuntimeEvent
    ? Pick<Event, "type" | "payload">
    : never
  : never;

export function canonicalEvent(
  context: EventContext,
  body: EventBody,
): ProviderRuntimeEvent {
  return {
    eventId: randomUUID(),
    provider: context.provider,
    threadId: context.threadId,
    createdAt: new Date().toISOString(),
    ...(context.turnId ? { turnId: context.turnId } : {}),
    ...(context.itemId ? { itemId: context.itemId } : {}),
    ...(context.requestId ? { requestId: context.requestId } : {}),
    ...(context.providerRefs ? { providerRefs: context.providerRefs } : {}),
    ...(context.raw !== undefined ? { raw: context.raw } : {}),
    ...body,
  } as ProviderRuntimeEvent;
}

export function errorMessage(cause: unknown): string {
  if (cause instanceof Error && cause.message.trim()) return cause.message.trim();
  if (cause && typeof cause === "object") {
    try {
      return JSON.stringify(cause);
    } catch {
      return String(cause);
    }
  }
  return String(cause);
}

export function itemTypeFromToolName(toolName: string): string {
  const normalized = toolName.toLowerCase();
  if (normalized.includes("bash") || normalized.includes("command")) return "command_execution";
  if (
    normalized.includes("edit") ||
    normalized.includes("write") ||
    normalized.includes("patch")
  ) {
    return "file_change";
  }
  if (normalized.includes("read") || normalized.includes("glob") || normalized.includes("grep")) {
    return "file_read";
  }
  if (normalized.includes("web")) return "web_search";
  if (normalized.includes("task") || normalized.includes("agent")) return "agent_task";
  return "tool_call";
}

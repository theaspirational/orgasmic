import { randomUUID } from "node:crypto";
import { execFile } from "node:child_process";
import { promisify } from "node:util";

import {
  query,
  type Options as ClaudeQueryOptions,
  type PermissionResult,
  type SDKUserMessage,
} from "@anthropic-ai/claude-agent-sdk";

import {
  canonicalEvent,
  errorMessage,
  itemTypeFromToolName,
  type ProviderRuntimeEvent,
} from "./canonical.ts";
import type {
  ChatRuntime,
  Emit,
  ProviderCatalog,
  RuntimeOptionPatch,
  RuntimeOptions,
  SandboxPermissions,
} from "./runtime.ts";

const execFileAsync = promisify(execFile);

type JsonRecord = Record<string, unknown>;

type ClaudeState = {
  threadId: string;
  turnId?: string;
  blockItems: Map<number, string>;
  tools: Map<number, { id: string; name: string; itemType: string; input: unknown }>;
};

class PromptQueue implements AsyncIterable<SDKUserMessage> {
  readonly #values: SDKUserMessage[] = [];
  readonly #waiters: Array<(value: IteratorResult<SDKUserMessage>) => void> = [];
  #closed = false;

  push(value: SDKUserMessage): void {
    if (this.#closed) throw new Error("Claude prompt queue is closed");
    const waiter = this.#waiters.shift();
    if (waiter) waiter({ value, done: false });
    else this.#values.push(value);
  }

  close(): void {
    if (this.#closed) return;
    this.#closed = true;
    for (const waiter of this.#waiters.splice(0)) waiter({ value: undefined, done: true });
  }

  [Symbol.asyncIterator](): AsyncIterator<SDKUserMessage> {
    return {
      next: () => {
        const value = this.#values.shift();
        if (value) return Promise.resolve({ value, done: false });
        if (this.#closed) return Promise.resolve({ value: undefined, done: true });
        return new Promise((resolve) => this.#waiters.push(resolve));
      },
    };
  }
}

function record(value: unknown): JsonRecord | undefined {
  return value && typeof value === "object" ? (value as JsonRecord) : undefined;
}

function string(value: unknown): string | undefined {
  return typeof value === "string" && value.length > 0 ? value : undefined;
}

function contentBlocks(message: unknown): JsonRecord[] {
  const body = record(record(message)?.message);
  return Array.isArray(body?.content)
    ? body.content.flatMap((value) => (record(value) ? [record(value)!] : []))
    : [];
}

function toolResultText(block: JsonRecord): string {
  const content = block.content;
  if (typeof content === "string") return content;
  if (!Array.isArray(content)) return "";
  return content
    .flatMap((entry) => {
      if (typeof entry === "string") return [entry];
      const value = record(entry);
      return value?.type === "text" && typeof value.text === "string" ? [value.text] : [];
    })
    .join("");
}

export function normalizeClaudeMessage(
  state: ClaudeState,
  message: unknown,
): ProviderRuntimeEvent[] {
  const sdk = record(message);
  if (!sdk) return [];
  const rawContext = { provider: "claude" as const, threadId: state.threadId, raw: sdk };

  if (sdk.type === "rate_limit_event") {
    return [
      canonicalEvent(rawContext, {
        type: "account.rate-limits.updated",
        payload: { rateLimits: sdk },
      }),
    ];
  }

  if (sdk.type === "system") {
    if (sdk.subtype === "rate_limit_event") {
      return [
        canonicalEvent(rawContext, {
          type: "account.rate-limits.updated",
          payload: { rateLimits: sdk },
        }),
      ];
    }
    // Status, token counters, retries, and CLI bookkeeping are deliberately
    // not transcript content. Terminal errors arrive through result/runtime.error.
    return [];
  }

  if (sdk.type === "stream_event") {
    const event = record(sdk.event);
    if (!event || !state.turnId) return [];
    const index = typeof event.index === "number" ? event.index : -1;
    const context = { ...rawContext, turnId: state.turnId };

    if (event.type === "message_delta") {
      return event.usage === undefined
        ? []
        : [
            canonicalEvent(context, {
              type: "thread.token-usage.updated",
              payload: { usage: event.usage },
            }),
          ];
    }

    if (event.type === "content_block_start") {
      const block = record(event.content_block);
      if (!block) return [];
      if (block.type === "text" || block.type === "thinking") {
        state.blockItems.set(index, string(block.id) ?? randomUUID());
        return [];
      }
      if (block.type !== "tool_use" && block.type !== "server_tool_use" && block.type !== "mcp_tool_use") {
        return [];
      }
      const id = string(block.id) ?? randomUUID();
      const name = string(block.name) ?? "tool";
      const itemType = itemTypeFromToolName(name);
      const input = block.input ?? {};
      state.tools.set(index, { id, name, itemType, input });
      return [
        canonicalEvent(
          { ...context, itemId: id, providerRefs: { providerItemId: id } },
          {
            type: "item.started",
            payload: {
              itemType,
              status: "inProgress",
              title: name,
              data: { toolName: name, input },
            },
          },
        ),
      ];
    }

    if (event.type === "content_block_delta") {
      const delta = record(event.delta);
      if (!delta) return [];
      if (delta.type === "text_delta" || delta.type === "thinking_delta") {
        const text =
          delta.type === "text_delta" ? string(delta.text) : string(delta.thinking);
        if (!text) return [];
        return [
          canonicalEvent(
            { ...context, itemId: state.blockItems.get(index) },
            {
              type: "content.delta",
              payload: {
                streamKind: delta.type === "thinking_delta" ? "reasoning_text" : "assistant_text",
                delta: text,
                contentIndex: index,
              },
            },
          ),
        ];
      }
      if (delta.type === "input_json_delta") {
        const tool = state.tools.get(index);
        const partial = string(delta.partial_json);
        if (!tool || !partial) return [];
        return [
          canonicalEvent(
            { ...context, itemId: tool.id, providerRefs: { providerItemId: tool.id } },
            {
              type: "item.updated",
              payload: {
                itemType: tool.itemType,
                status: "inProgress",
                title: tool.name,
                data: { toolName: tool.name, inputDelta: partial },
              },
            },
          ),
        ];
      }
      return [];
    }

    if (event.type === "content_block_stop") {
      const itemId = state.blockItems.get(index);
      if (!itemId) return [];
      state.blockItems.delete(index);
      return [
        canonicalEvent(
          { ...context, itemId },
          {
            type: "item.completed",
            payload: {
              itemType: "assistant_message",
              status: "completed",
              title: "Assistant message",
            },
          },
        ),
      ];
    }
    return [];
  }

  if (sdk.type === "user" && state.turnId) {
    const events: ProviderRuntimeEvent[] = [];
    for (const block of contentBlocks(sdk)) {
      if (block.type !== "tool_result") continue;
      const toolUseId = string(block.tool_use_id);
      if (!toolUseId) continue;
      const toolEntry = [...state.tools.entries()].find(([, tool]) => tool.id === toolUseId);
      const tool = toolEntry?.[1];
      if (toolEntry) state.tools.delete(toolEntry[0]);
      const failed = block.is_error === true;
      events.push(
        canonicalEvent(
          {
            ...rawContext,
            turnId: state.turnId,
            itemId: toolUseId,
            providerRefs: { providerItemId: toolUseId },
          },
          {
            type: "item.completed",
            payload: {
              itemType: tool?.itemType ?? "tool_call",
              status: failed ? "failed" : "completed",
              title: tool?.name ?? "tool",
              data: {
                toolName: tool?.name,
                input: tool?.input,
                result: block,
                text: toolResultText(block),
              },
            },
          },
        ),
      );
    }
    return events;
  }

  if (sdk.type === "result" && state.turnId) {
    const turnId = state.turnId;
    state.turnId = undefined;
    state.blockItems.clear();
    state.tools.clear();
    const subtype = string(sdk.subtype) ?? "failed";
    const success = subtype === "success";
    const errors = Array.isArray(sdk.errors)
      ? sdk.errors.filter((value): value is string => typeof value === "string").join("\n")
      : undefined;
    const events: ProviderRuntimeEvent[] = [];
    if (sdk.usage !== undefined) {
      events.push(
        canonicalEvent(rawContext, {
          type: "thread.token-usage.updated",
          payload: { usage: sdk.usage },
        }),
      );
    }
    events.push(
      canonicalEvent(
        { ...rawContext, turnId },
        {
          type: "turn.completed",
          payload: {
            state: success ? "completed" : "failed",
            ...(string(sdk.stop_reason) ? { stopReason: string(sdk.stop_reason) } : {}),
            ...(sdk.usage !== undefined ? { usage: sdk.usage } : {}),
            ...(sdk.modelUsage !== undefined ? { modelUsage: sdk.modelUsage } : {}),
            ...(typeof sdk.total_cost_usd === "number"
              ? { totalCostUsd: sdk.total_cost_usd }
              : {}),
            ...(errors ? { errorMessage: errors } : {}),
          },
        },
      ),
    );
    return events;
  }

  return [];
}

function userMessage(text: string): SDKUserMessage {
  return {
    type: "user",
    parent_tool_use_id: null,
    session_id: "",
    message: { role: "user", content: [{ type: "text", text }] },
  } as SDKUserMessage;
}

function permissionMode(access: string | undefined): ClaudeQueryOptions["permissionMode"] {
  if (access === "auto-accept-edits") return "acceptEdits";
  if (access === "auto") return "auto";
  if (access === "full-access") return "bypassPermissions";
  return "default";
}

function sandboxIsUnrestricted(sandbox: SandboxPermissions | undefined): boolean {
  return Boolean(
    sandbox?.allowExec &&
      sandbox.allowPatch &&
      sandbox.allowNetwork &&
      sandbox.allowWritesOutsideCwd,
  );
}

export function claudeToolPermission(
  toolName: string,
  blockedPath: string | undefined,
  sandbox: SandboxPermissions,
): PermissionResult {
  const deny = (message: string): PermissionResult => ({ behavior: "deny", message });
  if (blockedPath && !sandbox.allowWritesOutsideCwd) {
    return deny(`Writes outside the dispatch worktree are disabled: ${blockedPath}`);
  }
  const tool = toolName.toLowerCase();
  if (tool.includes("webfetch") || tool.includes("websearch")) {
    return sandbox.allowNetwork
      ? { behavior: "allow" }
      : deny("Network access is disabled for this dispatch");
  }
  if (tool.includes("bash") || tool.includes("shell") || tool.includes("terminal")) {
    if (!sandbox.allowExec) return deny("Command execution is disabled for this dispatch");
    if (!sandbox.allowNetwork) {
      return deny("Shell execution is disabled while network access is restricted");
    }
    return { behavior: "allow" };
  }
  if (tool.includes("edit") || tool.includes("write") || tool.includes("notebook")) {
    return sandbox.allowPatch
      ? { behavior: "allow" }
      : deny("File edits are disabled for this dispatch");
  }
  if (tool === "read" || tool === "glob" || tool === "grep") {
    return { behavior: "allow" };
  }
  return deny(`Tool ${toolName} is not allowed by the dispatch sandbox`);
}

export function normalizeClaudeEffort(
  effort: string | undefined,
  model: string | undefined,
): NonNullable<ClaudeQueryOptions["effort"]> | undefined {
  if (!effort || effort === "ultrathink") return undefined;
  if (effort === "ultracode") return "xhigh";
  if (
    effort === "xhigh" &&
    model !== "claude-fable-5" &&
    model !== "claude-opus-5" &&
    model !== "claude-opus-4-8" &&
    model !== "claude-sonnet-5"
  ) {
    return "max";
  }
  if (effort === "max" && model === "claude-sonnet-4-6") return "high";
  return ["low", "medium", "high", "xhigh", "max"].includes(effort)
    ? (effort as NonNullable<ClaudeQueryOptions["effort"]>)
    : undefined;
}

export function applyClaudePromptEffortPrefix(text: string, effort: string | undefined): string {
  return effort === "ultrathink" ? `Ultrathink: ${text}` : text;
}

export async function startClaudeRuntime(options: RuntimeOptions, emit: Emit): Promise<ChatRuntime> {
  const abortController = new AbortController();
  const prompts = new PromptQueue();
  const state: ClaudeState = {
    threadId: options.threadId,
    blockItems: new Map(),
    tools: new Map(),
  };
  let current = { ...options };
  const mode = current.sandbox && !sandboxIsUnrestricted(current.sandbox)
    ? "default"
    : permissionMode(current.access);
  const effort = normalizeClaudeEffort(current.effort, current.model);
  const canUseTool: ClaudeQueryOptions["canUseTool"] = async (
    toolName,
    _input,
    details,
  ) => claudeToolPermission(toolName, details.blockedPath, current.sandbox!);
  const q = query({
    prompt: prompts,
    options: {
      abortController,
      cwd: current.cwd,
      ...(current.model ? { model: current.model } : {}),
      ...(effort ? { effort } : {}),
      // Match T3 Code's deployment boundary: the SDK provides the typed
      // transport and normalization while the authenticated, user-installed
      // Claude Code executable remains the runtime. This also keeps the
      // Orgasmic bundle free of Anthropic's platform-specific 300 MB binary.
      pathToClaudeCodeExecutable: process.env.CLAUDE_BIN || "claude",
      permissionMode: mode,
      ...(mode === "bypassPermissions" ? { allowDangerouslySkipPermissions: true } : {}),
      ...(current.sandbox && !sandboxIsUnrestricted(current.sandbox)
        ? { canUseTool }
        : {}),
      systemPrompt: { type: "preset", preset: "claude_code" },
      settingSources: ["user", "project", "local"],
      ...((current.serviceTier === "fast" || current.effort === "ultracode")
        ? {
            settings: {
              ...(current.serviceTier === "fast" ? { fastMode: true } : {}),
              ...(current.effort === "ultracode" ? { ultracode: true } : {}),
            },
          }
        : {}),
      includePartialMessages: true,
    },
  });

  await q.initializationResult();
  emit(
    canonicalEvent(
      { provider: "claude", threadId: options.threadId },
      {
        type: "session.started",
        payload: {
          message: "Claude Agent SDK session started",
        },
      },
    ),
  );

  void (async () => {
    try {
      for await (const message of q) {
        for (const event of normalizeClaudeMessage(state, message)) emit(event);
      }
    } catch (cause) {
      if (abortController.signal.aborted) return;
      if (state.turnId) {
        const turnId = state.turnId;
        state.turnId = undefined;
        emit(
          canonicalEvent(
            { provider: "claude", threadId: options.threadId, turnId },
            {
              type: "turn.completed",
              payload: { state: "failed", errorMessage: errorMessage(cause) },
            },
          ),
        );
      }
      emit(
        canonicalEvent(
          { provider: "claude", threadId: options.threadId },
          {
            type: "runtime.error",
            payload: { message: errorMessage(cause), class: "transport_error", detail: cause },
          },
        ),
      );
    }
  })();

  return {
    async send(text) {
      const trimmed = text.trim();
      if (!trimmed) throw new Error("Claude turn requires non-empty text");
      if (!state.turnId) {
        state.turnId = `claude-turn-${randomUUID()}`;
        emit(
          canonicalEvent(
            { provider: "claude", threadId: options.threadId, turnId: state.turnId },
            {
              type: "turn.started",
              payload: {
                ...(current.model ? { model: current.model } : {}),
                ...(current.effort ? { effort: current.effort } : {}),
              },
            },
          ),
        );
      }
      prompts.push(userMessage(applyClaudePromptEffortPrefix(trimmed, current.effort)));
    },
    async setOptions(patch: RuntimeOptionPatch) {
      current = { ...current, ...patch };
      if (patch.model) await q.setModel(patch.model);
      if (patch.access) await q.setPermissionMode(permissionMode(patch.access) ?? "default");
      if (patch.model !== undefined || patch.effort !== undefined || patch.serviceTier !== undefined) {
        await q.applyFlagSettings({
          effortLevel: normalizeClaudeEffort(current.effort, current.model) ?? null,
          ultracode: current.effort === "ultracode",
          fastMode: current.serviceTier === "fast",
        });
      }
    },
    async stop(reason = "Session stopped") {
      prompts.close();
      abortController.abort();
      q.close();
      emit(
        canonicalEvent(
          { provider: "claude", threadId: options.threadId },
          {
            type: "session.exited",
            payload: { reason, recoverable: true, exitKind: "graceful" },
          },
        ),
      );
    },
  };
}

const CLAUDE_MODELS = [
  ["claude-fable-5", "Claude Fable 5", false, ["low", "medium", "high", "xhigh", "max", "ultracode", "ultrathink"]],
  ["claude-opus-5", "Claude Opus 5", false, ["low", "medium", "high", "xhigh", "max", "ultracode", "ultrathink"]],
  ["claude-sonnet-5", "Claude Sonnet 5", false, ["low", "medium", "high", "xhigh", "max", "ultrathink"]],
  ["claude-opus-4-8", "Claude Opus 4.8", true, ["low", "medium", "high", "xhigh", "max", "ultracode", "ultrathink"]],
  ["claude-opus-4-7", "Claude Opus 4.7", true, ["low", "medium", "high", "xhigh", "max", "ultrathink"]],
  ["claude-opus-4-6", "Claude Opus 4.6", true, ["low", "medium", "high", "max", "ultrathink"]],
  ["claude-opus-4-5", "Claude Opus 4.5", true, ["low", "medium", "high", "max"]],
  ["claude-sonnet-4-6", "Claude Sonnet 4.6", true, ["low", "medium", "high", "max", "ultrathink"]],
  ["claude-haiku-4-5", "Claude Haiku 4.5", true, []],
] as const;

function atLeast(version: string, minimum: readonly [number, number, number]): boolean {
  const parts = version.split(".").map((part) => Number.parseInt(part, 10) || 0);
  const parsed = [parts[0] ?? 0, parts[1] ?? 0, parts[2] ?? 0] as const;
  for (let index = 0; index < 3; index += 1) {
    if (parsed[index] !== minimum[index]) return parsed[index] > minimum[index];
  }
  return true;
}

export async function claudeCatalog(): Promise<ProviderCatalog> {
  // Importing and using this module proves the Agent SDK dependency is
  // present. The model availability gates intentionally mirror T3 Code's
  // ClaudeProvider, which keys its built-ins off the Claude Code version.
  const { stdout } = await execFileAsync(process.env.CLAUDE_BIN || "claude", ["--version"]);
  const version = stdout.trim().split(/\s+/)[0] ?? "0.0.0";
  const models = CLAUDE_MODELS.filter(([id]) => {
    if (id === "claude-fable-5") return atLeast(version, [2, 1, 169]);
    if (id === "claude-opus-5") return atLeast(version, [2, 1, 219]);
    if (id === "claude-opus-4-8") return atLeast(version, [2, 1, 154]);
    if (id === "claude-opus-4-7") return atLeast(version, [2, 1, 111]);
    return true;
  }).map(([id, label, legacy, reasoningEfforts]) => ({
    id,
    label,
    legacy,
    reasoningEfforts: [...reasoningEfforts],
  }));
  return { id: "claude", source: `claude-agent-sdk:${version}`, models };
}

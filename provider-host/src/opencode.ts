import { randomUUID } from "node:crypto";
import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";
import { createServer } from "node:net";
import { createInterface } from "node:readline";

import { createOpencodeClient } from "@opencode-ai/sdk/v2";

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

type JsonRecord = Record<string, unknown>;
type OpenCodeClient = ReturnType<typeof createOpencodeClient>;

type OpenCodeState = {
  threadId: string;
  sessionId: string;
  turnId?: string;
  messageRoles: Map<string, string>;
  partText: Map<string, string>;
};

function record(value: unknown): JsonRecord | undefined {
  return value && typeof value === "object" ? (value as JsonRecord) : undefined;
}

function string(value: unknown): string | undefined {
  return typeof value === "string" && value.length > 0 ? value : undefined;
}

function responseData<T>(response: unknown): T | undefined {
  return record(response)?.data as T | undefined;
}

async function availablePort(): Promise<number> {
  return await new Promise((resolve, reject) => {
    const server = createServer();
    server.once("error", reject);
    server.listen(0, "127.0.0.1", () => {
      const address = server.address();
      const port = address && typeof address === "object" ? address.port : 0;
      server.close((cause) => (cause ? reject(cause) : resolve(port)));
    });
  });
}

async function startServer(): Promise<{
  child: ChildProcessWithoutNullStreams;
  url: string;
}> {
  const port = await availablePort();
  const child = spawn(
    process.env.OPENCODE_BIN || "opencode",
    ["serve", "--hostname=127.0.0.1", `--port=${port}`],
    {
      env: { ...process.env, OPENCODE_CONFIG_CONTENT: "{}" },
      stdio: ["pipe", "pipe", "pipe"],
      detached: process.platform !== "win32",
    },
  );

  const url = await new Promise<string>((resolve, reject) => {
    const timeout = setTimeout(() => {
      killServer(child);
      reject(new Error("Timed out waiting for OpenCode server startup"));
    }, 30_000);
    const inspect = (line: string) => {
      const match = line.match(/opencode server listening.*?(https?:\/\/\S+)/i);
      if (!match?.[1]) return;
      clearTimeout(timeout);
      resolve(match[1]);
    };
    createInterface({ input: child.stdout }).on("line", inspect);
    createInterface({ input: child.stderr }).on("line", (line) => {
      inspect(line);
      if (!line.toLowerCase().includes("server listening"))
        process.stderr.write(`${line}\n`);
    });
    child.once("error", (cause) => {
      clearTimeout(timeout);
      reject(cause);
    });
    child.once("exit", (code) => {
      clearTimeout(timeout);
      reject(
        new Error(
          `OpenCode server exited before startup (code ${code ?? "?"})`,
        ),
      );
    });
  });
  return { child, url };
}

function killServer(child: ChildProcessWithoutNullStreams): void {
  if (child.exitCode !== null) return;
  if (process.platform !== "win32" && child.pid) {
    try {
      process.kill(-child.pid, "SIGTERM");
      return;
    } catch {
      // Fall through to the direct child.
    }
  }
  child.kill("SIGTERM");
}

function partText(part: JsonRecord): string | undefined {
  return part.type === "text" || part.type === "reasoning"
    ? string(part.text)
    : undefined;
}

function streamKind(part: JsonRecord): "reasoning_text" | "assistant_text" {
  return part.type === "reasoning" ? "reasoning_text" : "assistant_text";
}

function toolPayload(part: JsonRecord) {
  const tool = string(part.tool) ?? "tool";
  const state = record(part.state) ?? {};
  const status = string(state.status) ?? "pending";
  return {
    itemType: itemTypeFromToolName(tool),
    status:
      status === "completed"
        ? ("completed" as const)
        : status === "error"
          ? ("failed" as const)
          : ("inProgress" as const),
    title: string(state.title) ?? tool,
    ...(string(state.output) ? { detail: string(state.output) } : {}),
    data: { tool, state },
  };
}

export function normalizeOpenCodeEvent(
  state: OpenCodeState,
  event: unknown,
): ProviderRuntimeEvent[] {
  const native = record(event);
  const properties = record(native?.properties);
  if (!native || !properties) return [];
  const eventSessionId =
    string(properties.sessionID) ??
    string(record(properties.info)?.sessionID) ??
    string(record(properties.part)?.sessionID);
  if (eventSessionId && eventSessionId !== state.sessionId) return [];
  const context = {
    provider: "opencode" as const,
    threadId: state.threadId,
    ...(state.turnId ? { turnId: state.turnId } : {}),
    raw: native,
  };

  if (native.type === "session.updated") {
    const info = record(properties.info);
    const title = string(info?.title);
    return title
      ? [
          canonicalEvent(context, {
            type: "thread.metadata.updated",
            payload: { name: title, metadata: { sessionID: state.sessionId } },
          }),
        ]
      : [];
  }

  if (native.type === "message.updated") {
    const info = record(properties.info);
    const id = string(info?.id);
    const role = string(info?.role);
    if (id && role) state.messageRoles.set(id, role);
    return [];
  }

  if (native.type === "message.removed") {
    const id = string(properties.messageID);
    if (id) state.messageRoles.delete(id);
    return [];
  }

  if (native.type === "message.part.delta") {
    const partId = string(properties.partID);
    const delta = string(properties.delta);
    if (!partId || !delta || !state.turnId) return [];
    const next = `${state.partText.get(partId) ?? ""}${delta}`;
    state.partText.set(partId, next);
    const kind =
      string(properties.field) === "reasoning"
        ? "reasoning_text"
        : "assistant_text";
    return [
      canonicalEvent(
        { ...context, itemId: partId },
        { type: "content.delta", payload: { streamKind: kind, delta } },
      ),
    ];
  }

  if (native.type === "message.part.updated") {
    const part = record(properties.part);
    if (!part) return [];
    const partId = string(part.id);
    const messageId = string(part.messageID);
    const role = messageId ? state.messageRoles.get(messageId) : undefined;
    const events: ProviderRuntimeEvent[] = [];
    if (role === "assistant" && partId) {
      const text = partText(part);
      const previous = state.partText.get(partId) ?? "";
      if (text && text.length > previous.length) {
        const delta = text.startsWith(previous)
          ? text.slice(previous.length)
          : text;
        state.partText.set(partId, text);
        events.push(
          canonicalEvent(
            { ...context, itemId: partId },
            {
              type: "content.delta",
              payload: { streamKind: streamKind(part), delta },
            },
          ),
        );
      }
      if (part.type === "text" && record(part.time)?.end !== undefined) {
        events.push(
          canonicalEvent(
            { ...context, itemId: partId },
            {
              type: "item.completed",
              payload: {
                itemType: "assistant_message",
                status: "completed",
                title: "Assistant message",
              },
            },
          ),
        );
      }
    }
    if (part.type === "tool") {
      const callId = string(part.callID) ?? partId ?? randomUUID();
      const status = string(record(part.state)?.status) ?? "pending";
      events.push(
        canonicalEvent(
          { ...context, itemId: callId },
          {
            type:
              status === "pending"
                ? "item.started"
                : status === "completed" || status === "error"
                  ? "item.completed"
                  : "item.updated",
            payload: toolPayload(part),
          },
        ),
      );
    }
    return events;
  }

  if (native.type === "session.status") {
    const status = record(properties.status);
    if (status?.type === "retry") {
      return [
        canonicalEvent(context, {
          type: "runtime.warning",
          payload: {
            message: string(status.message) ?? "OpenCode is retrying",
            detail: status,
          },
        }),
      ];
    }
    if (status?.type === "idle" && state.turnId) {
      const turnId = state.turnId;
      state.turnId = undefined;
      return [
        canonicalEvent(
          { ...context, turnId },
          { type: "turn.completed", payload: { state: "completed" } },
        ),
      ];
    }
    return [];
  }

  if (native.type === "session.error") {
    const message = errorMessage(properties.error ?? properties);
    const events: ProviderRuntimeEvent[] = [];
    if (state.turnId) {
      const turnId = state.turnId;
      state.turnId = undefined;
      events.push(
        canonicalEvent(
          { ...context, turnId },
          {
            type: "turn.completed",
            payload: { state: "failed", errorMessage: message },
          },
        ),
      );
    }
    events.push(
      canonicalEvent(context, {
        type: "runtime.error",
        payload: { message, class: "provider_error", detail: properties.error },
      }),
    );
    return events;
  }

  if (native.type === "permission.asked") {
    const requestId = string(properties.id);
    return requestId
      ? [
          canonicalEvent(
            { ...context, requestId },
            {
              type: "request.opened",
              payload: {
                requestType: string(properties.permission) ?? "unknown",
                detail: Array.isArray(properties.patterns)
                  ? properties.patterns.join("\n")
                  : string(properties.permission),
                args: properties.metadata,
              },
            },
          ),
        ]
      : [];
  }

  return [];
}

export function permissionRules(
  access: string | undefined,
  sandbox?: SandboxPermissions,
) {
  if (
    (!sandbox ||
      (sandbox.allowExec &&
        sandbox.allowPatch &&
        sandbox.allowNetwork &&
        sandbox.allowWritesOutsideCwd)) &&
    (!access || access === "full-access")
  ) {
    return [{ permission: "*", pattern: "*", action: "allow" as const }];
  }
  if (sandbox) {
    const rules = [
      { permission: "*", pattern: "*", action: "deny" as const },
      { permission: "read", pattern: "*", action: "allow" as const },
      { permission: "glob", pattern: "*", action: "allow" as const },
      { permission: "grep", pattern: "*", action: "allow" as const },
      { permission: "list", pattern: "*", action: "allow" as const },
      { permission: "lsp", pattern: "*", action: "allow" as const },
      { permission: "skill", pattern: "*", action: "allow" as const },
      { permission: "question", pattern: "*", action: "allow" as const },
      {
        permission: "edit",
        pattern: "*",
        action: sandbox.allowPatch ? ("allow" as const) : ("deny" as const),
      },
      {
        permission: "bash",
        pattern: "*",
        action:
          sandbox.allowExec && sandbox.allowNetwork
            ? ("allow" as const)
            : ("deny" as const),
      },
      {
        permission: "external_directory",
        pattern: "*",
        action: sandbox.allowWritesOutsideCwd
          ? ("allow" as const)
          : ("deny" as const),
      },
      {
        permission: "webfetch",
        pattern: "*",
        action: sandbox.allowNetwork ? ("allow" as const) : ("deny" as const),
      },
      {
        permission: "websearch",
        pattern: "*",
        action: sandbox.allowNetwork ? ("allow" as const) : ("deny" as const),
      },
    ];
    return rules;
  }
  return [
    { permission: "*", pattern: "*", action: "ask" as const },
    { permission: "question", pattern: "*", action: "allow" as const },
  ];
}

function parseModel(
  model: string | undefined,
): { providerID: string; modelID: string } | undefined {
  if (!model) return undefined;
  const separator = model.indexOf("/");
  if (separator <= 0 || separator >= model.length - 1) return undefined;
  return {
    providerID: model.slice(0, separator),
    modelID: model.slice(separator + 1),
  };
}

async function consumeEvents(
  stream: AsyncIterable<unknown>,
  state: OpenCodeState,
  emit: Emit,
): Promise<void> {
  for await (const event of stream) {
    for (const normalized of normalizeOpenCodeEvent(state, event))
      emit(normalized);
  }
}

export async function startOpenCodeRuntime(
  options: RuntimeOptions,
  emit: Emit,
): Promise<ChatRuntime> {
  const server = await startServer();
  try {
    const client = createOpencodeClient({
      baseUrl: server.url,
      directory: options.cwd,
      throwOnError: true,
    });
    const created = await client.session.create({
      permission: permissionRules(options.access, options.sandbox),
    });
    const session = responseData<{ id: string }>(created);
    if (!session?.id) {
      killServer(server.child);
      throw new Error("OpenCode session.create returned no session payload");
    }
    const state: OpenCodeState = {
      threadId: options.threadId,
      sessionId: session.id,
      messageRoles: new Map(),
      partText: new Map(),
    };
    let current = { ...options };
    if (!current.model) {
      const providers = responseData<{
        default?: Record<string, string>;
        connected?: string[];
      }>(await client.provider.list());
      const defaults = providers?.default ?? {};
      const preferredProvider = (providers?.connected ?? []).find(
        (id) => defaults[id],
      );
      if (preferredProvider)
        current.model = `${preferredProvider}/${defaults[preferredProvider]}`;
    }
    const abortController = new AbortController();
    // Establish the SSE subscription before advertising Ready. The first user
    // turn may arrive immediately after launch; starting promptAsync before the
    // subscription exists can lose its earliest text/tool deltas.
    const subscription = await client.event.subscribe(undefined, {
      signal: abortController.signal,
    });

    const failTransport = (cause: unknown) => {
      if (abortController.signal.aborted) return;
      killServer(server.child);
      if (state.turnId) {
        const turnId = state.turnId;
        state.turnId = undefined;
        emit(
          canonicalEvent(
            { provider: "opencode", threadId: options.threadId, turnId },
            {
              type: "turn.completed",
              payload: { state: "failed", errorMessage: errorMessage(cause) },
            },
          ),
        );
      }
      emit(
        canonicalEvent(
          { provider: "opencode", threadId: options.threadId },
          {
            type: "runtime.error",
            payload: {
              message: errorMessage(cause),
              class: "transport_error",
              detail: cause,
            },
          },
        ),
      );
    };
    void consumeEvents(subscription.stream, state, emit).then(
      () => failTransport(new Error("OpenCode event stream ended")),
      failTransport,
    );

    emit(
      canonicalEvent(
        { provider: "opencode", threadId: options.threadId },
        {
          type: "session.started",
          payload: {
            message: "OpenCode SDK session started",
            resume: { sessionId: session.id },
          },
        },
      ),
    );

    return {
      async send(text) {
        const trimmed = text.trim();
        if (!trimmed) throw new Error("OpenCode turn requires non-empty text");
        const model = parseModel(current.model);
        if (!model)
          throw new Error("OpenCode model must use the provider/model format");
        if (!state.turnId) {
          state.turnId = `opencode-turn-${randomUUID()}`;
          emit(
            canonicalEvent(
              {
                provider: "opencode",
                threadId: options.threadId,
                turnId: state.turnId,
              },
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
        const turnId = state.turnId;
        try {
          await client.session.promptAsync({
            sessionID: session.id,
            model,
            ...(current.effort ? { variant: current.effort } : {}),
            parts: [{ type: "text", text: trimmed }],
          });
        } catch (cause) {
          if (state.turnId === turnId) state.turnId = undefined;
          emit(
            canonicalEvent(
              { provider: "opencode", threadId: options.threadId, turnId },
              {
                type: "turn.completed",
                payload: { state: "failed", errorMessage: errorMessage(cause) },
              },
            ),
          );
          throw cause;
        }
      },
      async setOptions(patch: RuntimeOptionPatch) {
        current = { ...current, ...patch };
        if (patch.access) {
          await client.session.update({
            sessionID: session.id,
            permission: permissionRules(patch.access, current.sandbox),
          });
        }
      },
      async stop(reason = "Session stopped") {
        abortController.abort();
        await client.session
          .abort({ sessionID: session.id })
          .catch(() => undefined);
        killServer(server.child);
        emit(
          canonicalEvent(
            { provider: "opencode", threadId: options.threadId },
            {
              type: "session.exited",
              payload: { reason, recoverable: true, exitKind: "graceful" },
            },
          ),
        );
      },
    };
  } catch (cause) {
    killServer(server.child);
    throw cause;
  }
}

export async function openCodeCatalog(cwd: string): Promise<ProviderCatalog> {
  const server = await startServer();
  try {
    const client = createOpencodeClient({
      baseUrl: server.url,
      directory: cwd,
      throwOnError: true,
    });
    const response = await client.provider.list();
    const catalog = responseData<{
      all: Array<{
        id: string;
        name: string;
        models: Record<
          string,
          { id: string; name?: string; variants?: Record<string, unknown> }
        >;
      }>;
      connected: string[];
    }>(response);
    const connected = new Set(catalog?.connected ?? []);
    const models = (catalog?.all ?? [])
      .filter((provider) => connected.has(provider.id))
      .flatMap((provider) =>
        Object.values(provider.models).map((model) => ({
          id: `${provider.id}/${model.id}`,
          label: model.name?.trim() || model.id,
          reasoningEfforts: Object.keys(model.variants ?? {}),
        })),
      )
      .sort((left, right) => left.label.localeCompare(right.label));
    return {
      id: "opencode",
      source: "opencode-sdk:provider.list",
      models,
      ...(models.length === 0
        ? { message: "OpenCode reported no connected provider models" }
        : {}),
    };
  } finally {
    killServer(server.child);
  }
}

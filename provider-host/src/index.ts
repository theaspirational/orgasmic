#!/usr/bin/env node

import { createInterface } from "node:readline";

import { claudeCatalog, startClaudeRuntime } from "./claude.ts";
import { canonicalEvent, errorMessage, type ProviderId } from "./canonical.ts";
import { openCodeCatalog, startOpenCodeRuntime } from "./opencode.ts";
import {
  parseSandboxPermissions,
  type ChatRuntime,
  type RuntimeOptions,
} from "./runtime.ts";

function argument(name: string): string | undefined {
  const index = process.argv.indexOf(`--${name}`);
  return index >= 0 ? process.argv[index + 1] : undefined;
}

function provider(): ProviderId {
  const value = argument("provider");
  if (value === "claude" || value === "opencode") return value;
  throw new Error("--provider must be claude or opencode");
}

function write(value: unknown): void {
  process.stdout.write(`${JSON.stringify(value)}\n`);
}

async function runCatalog(): Promise<void> {
  const selected = provider();
  const catalog =
    selected === "claude"
      ? await claudeCatalog()
      : await openCodeCatalog(argument("cwd") || process.cwd());
  write(catalog);
}

async function createRuntime(options: RuntimeOptions): Promise<ChatRuntime> {
  const emit = (event: unknown) => write(event);
  return options.provider === "claude"
    ? await startClaudeRuntime(options, emit)
    : await startOpenCodeRuntime(options, emit);
}

async function runSession(): Promise<void> {
  const options: RuntimeOptions = {
    provider: provider(),
    threadId: argument("thread-id") || `orgasmic-chat-${crypto.randomUUID()}`,
    cwd: argument("cwd") || process.cwd(),
    ...(argument("model") ? { model: argument("model") } : {}),
    ...(argument("effort") ? { effort: argument("effort") } : {}),
    access: argument("access") || "full-access",
    ...(argument("service-tier") ? { serviceTier: argument("service-tier") } : {}),
    ...(argument("sandbox-permissions")
      ? { sandbox: parseSandboxPermissions(argument("sandbox-permissions")) }
      : {}),
  };
  let runtime: ChatRuntime;
  try {
    runtime = await createRuntime(options);
  } catch (cause) {
    write(
      canonicalEvent(
        { provider: options.provider, threadId: options.threadId },
        {
          type: "runtime.error",
          payload: { message: errorMessage(cause), class: "startup_error", detail: cause },
        },
      ),
    );
    process.exitCode = 1;
    return;
  }

  let stopping = false;
  const stop = async (reason?: string) => {
    if (stopping) return;
    stopping = true;
    await runtime.stop(reason);
  };

  const lines = createInterface({ input: process.stdin });
  lines.on("line", (line) => {
    void (async () => {
      try {
        const command = JSON.parse(line) as {
          type?: string;
          text?: string;
          reason?: string;
          model?: string;
          effort?: string;
          access?: string;
          serviceTier?: string;
        };
        if (command.type === "user_input") {
          await runtime.send(command.text ?? "");
          return;
        }
        if (command.type === "set_options") {
          await runtime.setOptions({
            ...(command.model !== undefined ? { model: command.model } : {}),
            ...(command.effort !== undefined ? { effort: command.effort } : {}),
            ...(command.access !== undefined ? { access: command.access } : {}),
            ...(command.serviceTier !== undefined ? { serviceTier: command.serviceTier } : {}),
          });
          return;
        }
        if (command.type === "stop") {
          await stop(command.reason);
          lines.close();
          return;
        }
        throw new Error(`Unknown provider-host command: ${command.type ?? "missing type"}`);
      } catch (cause) {
        write(
          canonicalEvent(
            { provider: options.provider, threadId: options.threadId },
            {
              type: "runtime.error",
              payload: { message: errorMessage(cause), class: "request_error", detail: cause },
            },
          ),
        );
      }
    })();
  });
  lines.once("close", () => void stop("Input closed"));
  process.once("SIGTERM", () => void stop("Process terminated"));
  process.once("SIGINT", () => void stop("Process interrupted"));
}

const command = process.argv[2];
try {
  if (command === "catalog") await runCatalog();
  else if (command === "session") await runSession();
  else throw new Error("usage: index.ts <catalog|session> --provider <claude|opencode>");
} catch (cause) {
  process.stderr.write(`${errorMessage(cause)}\n`);
  process.exitCode = 1;
}

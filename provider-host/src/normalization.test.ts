import assert from "node:assert/strict";
import test from "node:test";

import {
  applyClaudePromptEffortPrefix,
  claudeToolPermission,
  normalizeClaudeEffort,
  normalizeClaudeMessage,
} from "./claude.ts";
import { normalizeOpenCodeEvent, permissionRules } from "./opencode.ts";
import { parseSandboxPermissions } from "./runtime.ts";

test("Claude telemetry never becomes reasoning text", () => {
  const state = {
    threadId: "thread-1",
    turnId: "turn-1",
    blockItems: new Map<number, string>(),
    tools: new Map<number, { id: string; name: string; itemType: string; input: unknown }>(),
  };
  const rateLimit = normalizeClaudeMessage(state, {
    type: "rate_limit_event",
    rate_limit_info: { rateLimitType: "five_hour" },
  });
  assert.equal(rateLimit.length, 1);
  assert.equal(rateLimit[0]?.type, "account.rate-limits.updated");
  assert.deepEqual(
    normalizeClaudeMessage(state, {
      type: "system",
      subtype: "thinking_tokens",
      estimated_tokens: 50,
    }),
    [],
  );
});

test("Claude result completes a turn without duplicating assistant text", () => {
  const state = {
    threadId: "thread-1",
    turnId: "turn-1",
    blockItems: new Map<number, string>(),
    tools: new Map<number, { id: string; name: string; itemType: string; input: unknown }>(),
  };
  const events = normalizeClaudeMessage(state, {
    type: "result",
    subtype: "success",
    result: "This must not be emitted as another assistant message",
    usage: { input_tokens: 4, output_tokens: 8 },
  });
  assert.deepEqual(
    events.map((event) => event.type),
    ["thread.token-usage.updated", "turn.completed"],
  );
});

test("Claude effort aliases follow T3 Code's SDK normalization", () => {
  assert.equal(normalizeClaudeEffort("ultracode", "claude-opus-5"), "xhigh");
  assert.equal(normalizeClaudeEffort("ultrathink", "claude-opus-5"), undefined);
  assert.equal(normalizeClaudeEffort("max", "claude-sonnet-4-6"), "high");
  assert.equal(
    applyClaudePromptEffortPrefix("inspect this", "ultrathink"),
    "Ultrathink: inspect this",
  );
});

test("OpenCode normalizes reasoning and assistant deltas separately", () => {
  const state = {
    threadId: "thread-1",
    sessionId: "session-1",
    turnId: "turn-1",
    messageRoles: new Map<string, string>(),
    partText: new Map<string, string>(),
  };
  const events = normalizeOpenCodeEvent(state, {
    type: "message.part.delta",
    properties: {
      sessionID: "session-1",
      partID: "part-1",
      field: "reasoning",
      delta: "checking",
    },
  });
  assert.equal(events[0]?.type, "content.delta");
  assert.equal(
    events[0]?.type === "content.delta" ? events[0].payload.streamKind : undefined,
    "reasoning_text",
  );
});

test("sandbox permissions remain structured across the SDK host boundary", () => {
  const sandbox = parseSandboxPermissions(
    "allow_exec=true,allow_patch=true,allow_network=false,allow_writes_outside_cwd=false",
  );
  assert.deepEqual(sandbox, {
    allowExec: true,
    allowPatch: true,
    allowNetwork: false,
    allowWritesOutsideCwd: false,
  });
  assert.deepEqual(claudeToolPermission("Edit", undefined, sandbox!), {
    behavior: "allow",
  });
  assert.equal(claudeToolPermission("Bash", undefined, sandbox!).behavior, "deny");

  const rules = permissionRules("supervised", sandbox);
  assert.equal(
    rules.find((rule) => rule.permission === "edit")?.action,
    "allow",
  );
  assert.equal(
    rules.find((rule) => rule.permission === "bash")?.action,
    "deny",
  );
  assert.equal(
    rules.find((rule) => rule.permission === "external_directory")?.action,
    "deny",
  );
});

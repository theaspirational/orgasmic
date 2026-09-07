import { createInterface } from "node:readline";
import { mkdirSync } from "node:fs";
import { join } from "node:path";
import { createSdkMcpServer, query, tool, type Options } from "@anthropic-ai/claude-agent-sdk";
import { z } from "zod";

export type Broker = (request: Record<string, unknown>) => Promise<unknown>;

/** No built-in tools, user/project settings, external MCP servers, hooks or plugins. */
export function retroTools(broker: Broker) {
  const call = async (request: Record<string, unknown>) => {
    try {
      return { content: [{ type: "text" as const, text: JSON.stringify(await broker(request)) }] };
    } catch (error) {
      return { isError: true, content: [{ type: "text" as const, text: String(error) }] };
    }
  };
  const citation = z.object({ run: z.string(), source_sha256: z.string(), source_line: z.number().int().positive() });
  return [
    tool("catalog", "Start here: immutable ordered scope and catalog summaries, never raw transcripts.", {}, () => call({op:"catalog"})),
    tool("materialize", "Explicitly convert one scoped run if needed. Returns summary and reference.", {run:z.string()}, args => call({op:"materialize",...args})),
    tool("read", "Read at most 10 retained evidence records, capped at 32 KiB. Cite source_line and digest.", {run:z.string(),offset:z.number().int().nonnegative(),limit:z.number().int().min(1).max(10)}, args => call({op:"read",...args})),
    tool("submit", "Terminal declaration: submit the evidence-linked report once.", {
      findings:z.array(z.object({category:z.string(),finding:z.string(),evidence:z.array(citation).min(1)})).max(50),
      coverage:z.string().min(1),limitations:z.string().min(1),
    }, args => call({op:"submit",...args})),
  ];
}

export function retroOptions(directory: string, model: string, broker: Broker): Options {
  const server = createSdkMcpServer({name: "retro", tools: retroTools(broker)});
  const allowedTools = ["catalog","materialize","read","submit"].map(name => `mcp__retro__${name}`);
  const home = join(directory,"runtime","home");
  const config = join(directory,"runtime","claude");
  const tmp = join(directory,"runtime","tmp");
  return {
    cwd:directory, model, tools:[], mcpServers:{retro:server}, strictMcpConfig:true,
    allowedTools, settingSources:[], plugins:[], settings:{disableAllHooks:true},
    permissionMode:"dontAsk", persistSession:false, maxTurns:40,
    pathToClaudeCodeExecutable:process.env.CLAUDE_BIN || "claude",
    env:{
      PATH:process.env.PATH, SYSTEMROOT:process.env.SYSTEMROOT,
      HOME:home, USERPROFILE:home, CLAUDE_CONFIG_DIR:config, TMPDIR:tmp, TMP:tmp, TEMP:tmp,
      XDG_CONFIG_HOME:config, XDG_CACHE_HOME:join(directory,"runtime","cache"),
      ANTHROPIC_API_KEY:process.env.ANTHROPIC_API_KEY,
      CLAUDE_CODE_OAUTH_TOKEN:process.env.CLAUDE_CODE_OAUTH_TOKEN,
      CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC:"1",
    },
    canUseTool:async (name,input) => allowedTools.includes(name)
      ? {behavior:"allow",updatedInput:input}
      : {behavior:"deny",message:"Only scoped retrospective tools are available"},
  };
}

export async function runRetro(): Promise<void> {
  if (!process.env.ANTHROPIC_API_KEY && !process.env.CLAUDE_CODE_OAUTH_TOKEN) {
    throw new Error("Read-only retro requires ANTHROPIC_API_KEY or CLAUDE_CODE_OAUTH_TOKEN; ambient credentials/config are not read");
  }
  const lines = createInterface({input:process.stdin});
  const iterator = lines[Symbol.asyncIterator]();
  const first = await iterator.next();
  if (first.done || first.value.length > 256 * 1024) throw new Error("Missing or oversized retro configuration");
  const config = z.object({prompt:z.string().min(1),model:z.string().min(1),directory:z.string().min(1)}).strict().parse(JSON.parse(first.value));
  let nextId = 0;
  let submitted = false;
  // MCP may issue parallel calls. One promise chain serializes the single stdio reply stream.
  let pending = Promise.resolve<unknown>(undefined);
  const broker: Broker = request => {
    const result = pending.then(async () => {
      if (submitted) throw new Error("Report already submitted");
      const id = ++nextId;
      const wire = JSON.stringify({id,request});
      if (wire.length > 128 * 1024) throw new Error("Tool request exceeds byte budget");
      process.stdout.write(`${wire}\n`);
      const next = await iterator.next();
      if (next.done) throw new Error("Diagnostic broker disconnected");
      const response = JSON.parse(next.value);
      if (response.id !== id) throw new Error("Diagnostic broker response mismatch");
      if (response.error) throw new Error(response.error);
      if (request.op === "submit" && response.result?.status === "report_submitted") submitted = true;
      return response.result;
    });
    pending = result.catch(() => undefined);
    return result;
  };
  const options = retroOptions(config.directory,config.model,broker);
  for (const path of [options.env!.HOME,options.env!.CLAUDE_CONFIG_DIR,options.env!.TMPDIR]) mkdirSync(path!,{recursive:true});
  const q = query({prompt:config.prompt,options});
  lines.once("close", () => q.close());
  try {
    for await (const message of q) {
      if (submitted) break;
      if (message.type === "result" && message.is_error) throw new Error("Retrospector provider failed before submission");
    }
    if (!submitted) throw new Error("Provider ended without report submission");
  } finally {
    q.close();
    lines.close();
  }
}

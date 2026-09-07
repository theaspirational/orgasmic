import test from "node:test";
import assert from "node:assert/strict";
import { retroOptions, retroTools } from "./retro.ts";

test("retro exposes only scoped tools, disables ambient execution and isolates vendor writes", async () => {
  const options = retroOptions("/derived/retro","fixture-model",async () => ({}));
  assert.deepEqual(options.tools,[]);
  assert.deepEqual(options.settingSources,[]);
  assert.deepEqual(options.plugins,[]);
  assert.equal(options.strictMcpConfig,true);
  assert.equal(options.persistSession,false);
  assert.equal((options.settings as {disableAllHooks:boolean}).disableAllHooks,true);
  assert.equal(options.env!.HOME,"/derived/retro/runtime/home");
  assert.equal(options.env!.CLAUDE_CONFIG_DIR,"/derived/retro/runtime/claude");
  assert.equal(options.env!.ORGASMIC_HOME,undefined);
  assert.equal(options.env!.NODE_OPTIONS,undefined);
  const details = {signal:new AbortController().signal,toolUseID:"fixture",requestId:"fixture"};
  for (const name of ["Bash","Write","Read","Glob","WebFetch","Task","Agent","Skill","mcp__daemon__release","mcp__retro__release"]) {
    assert.equal((await options.canUseTool!(name,{},details))?.behavior,"deny",name);
  }
  for (const name of options.allowedTools!) {
    assert.equal((await options.canUseTool!(name,{},details))?.behavior,"allow");
  }
});

test("SDK tools broker catalog first and lazy evidence before explicit cross-run submission", async () => {
  const calls: Record<string,unknown>[] = [];
  const tools = retroTools(async request => {
    calls.push(request);
    if (request.op === "catalog") return {runs:[{run_id:"implementer"},{run_id:"reviewer"},{run_id:"recovery"}]};
    if (request.op === "materialize") return {summary:{events:{tool_call:1}},source_sha256:"digest"};
    if (request.op === "read") return {source_sha256:"digest",records:[{source_line:1,events:[{type:"tool_call",name:"Read",input:{path:"lib.rs"}}]}]};
    if (request.op === "submit") return {status:"report_submitted"};
    throw new Error("unsupported operation");
  });
  const call = async (name:string,args:unknown) => {
    const tool = tools.find(tool => tool.name === name)!;
    const response = await tool.handler(args as never,{});
    assert.equal(response.isError,undefined);
    return JSON.parse((response.content[0] as {text:string}).text);
  };
  assert.equal((await call("catalog",{})).runs.length,3);
  for (const run of ["implementer","recovery"]) {
    await call("materialize",{run});
    await call("read",{run,offset:0,limit:1});
  }
  const report = await call("submit",{findings:[{category:"repeated calls",finding:"Both runs read lib.rs",evidence:["implementer","recovery"].map(run => ({run,source_sha256:"digest",source_line:1}))}],coverage:"Two runs examined; reviewer not needed",limitations:"fixture"});
  assert.equal(report.status,"report_submitted");
  assert.deepEqual(calls.map(call => call.op),["catalog","materialize","read","materialize","read","submit"]);
  assert.equal(calls.some(call => call.run === "reviewer"),false);
  const failure = retroTools(async () => { throw new Error("source changed"); });
  const response = await failure[1].handler({run:"implementer"} as never,{});
  assert.equal(response.isError,true);
});

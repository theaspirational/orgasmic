"""Local ACP peer for dispatch lifecycle/wire tests. No tools, network, or exec."""
import json
import os
import sys

if os.path.basename(sys.argv[0]) == "npx":
    assert sys.argv[1:] in (
        ["--yes", "--package", "@agentclientprotocol/codex-acp@1.10.0", "codex-acp"],
        ["--yes", "--package", "@agentclientprotocol/claude-agent-acp@0.75.1", "claude-agent-acp"],
    ), "unexpected ACP launcher arguments"
else:
    assert sys.argv[1:] == ["acp"], "unexpected provider launch"

catalog = [
    {"id": category, "category": category, "name": category, "type": "select",
     "currentValue": values[0],
     "options": [{"value": value, "name": value} for value in values]}
    for category, values in [
        ("model", ["fixture-model", "  Composer-2.5-FAST  ", "gpt-99", "   "]),
        ("thought_level", ["medium", " XHIGH ", "\t"]),
        ("mode", ["agent", "agent-full-access", "read-only", "bypassPermissions",
                  "auto", "acceptEdits", "default", "dont_ask", "build"]),
    ]
]

for line in sys.stdin:
    request = json.loads(line)
    if "id" not in request:
        continue
    method = request["method"]
    if method == "initialize":
        result = {"protocolVersion": 1, "agentCapabilities": {}}
    elif method == "session/new":
        result = {"sessionId": "dispatch-fixture", "configOptions": catalog}
    elif method == "session/set_config_option":
        params = request["params"]
        option = next(o for o in catalog if o["id"] == params["configId"])
        assert params["value"] in [o["value"] for o in option["options"]]
        option["currentValue"] = params["value"]
        result = {"configOptions": catalog}
    elif method == "session/prompt":
        result = {"stopReason": "end_turn"}
    else:
        raise AssertionError("unexpected ACP method: " + method)
    print(json.dumps({"jsonrpc": "2.0", "id": request["id"], "result": result}), flush=True)

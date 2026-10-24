"""最小的 stdio MCP 服务：一个 echo 工具"""
import json, sys

sys.stdin.reconfigure(encoding="utf-8")
sys.stdout.reconfigure(encoding="utf-8")
for line in sys.stdin:
    msg = json.loads(line)
    if "id" not in msg:
        continue
    method = msg.get("method")
    if method == "initialize":
        result = {"protocolVersion": "2025-06-18", "capabilities": {"tools": {}}, "serverInfo": {"name": "demo", "version": "1"}}
    elif method == "tools/list":
        result = {"tools": [{"name": "echo", "description": "原样返回 text", "annotations": {"readOnlyHint": True}, "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]}}]}
    elif method == "tools/call":
        result = {"content": [{"type": "text", "text": "四肢回声：" + msg["params"]["arguments"].get("text", "")}]}
    else:
        print(json.dumps({"jsonrpc": "2.0", "id": msg["id"], "error": {"code": -32601, "message": "no"}}), flush=True)
        continue
    print(json.dumps({"jsonrpc": "2.0", "id": msg["id"], "result": result}, ensure_ascii=False), flush=True)

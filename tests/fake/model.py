"""假的 OpenAI 兼容流式模型：按历史里已有几条工具结果决定下一步（写文件 → 跑命令核对 → 交付），把收到的请求记到 requests.jsonl"""
import json
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

LOG = sys.argv[2] if len(sys.argv) > 2 else "requests.jsonl"


def chunk(delta, finish=None):
    return {"choices": [{"index": 0, "delta": delta, "finish_reason": finish}]}


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        with open(LOG, "a", encoding="utf-8") as f:
            f.write(json.dumps(body, ensure_ascii=False) + "\n")
        messages = body["messages"]
        tools = [m for m in messages if m["role"] == "tool"]
        last_user = [m for m in messages if m["role"] == "user"][-1]["content"]
        events = []
        if "只聊天" in last_user:
            events.append(chunk({"role": "assistant", "content": "你好，"}))
            events.append(chunk({"content": "我是 EDITH。"}, "stop"))
        elif "四肢" in last_user and len(tools) == 0:
            names = [t["function"]["name"] for t in body.get("tools", [])]
            args = json.dumps({"text": "hi"})
            events.append(chunk({"role": "assistant", "tool_calls": [{"index": 0, "id": "call_l", "type": "function", "function": {"name": "demo__echo", "arguments": args}}]}, "tool_calls"))
        elif "四肢" in last_user:
            events.append(chunk({"role": "assistant", "content": "四肢说：" + tools[-1]["content"]}, "stop"))
        elif "慢" in last_user:
            import time
            for i in range(30):
                events.append(chunk({"content": f"{i} "}))
            self.send_response(200); self.send_header("Content-Type", "text/event-stream"); self.end_headers()
            for event in events:
                self.wfile.write(f"data: {json.dumps(event, ensure_ascii=False)}\n\n".encode()); self.wfile.flush(); time.sleep(0.2)
            self.wfile.write(b"data: [DONE]\n\n")
            return
        elif len(tools) == 0:
            events.append(chunk({"role": "assistant", "content": "先写文件。"}))
            args = json.dumps({"path": "hello.txt", "content": "你好 EDITH\n"}, ensure_ascii=False)
            events.append(chunk({"tool_calls": [{"index": 0, "id": "call_1", "type": "function", "function": {"name": "write_file", "arguments": args[:10]}}]}))
            events.append(chunk({"tool_calls": [{"index": 0, "function": {"arguments": args[10:]}}]}, "tool_calls"))
        elif len(tools) == 1:
            args = json.dumps({"command": "cat hello.txt"})
            events.append(chunk({"role": "assistant", "content": "", "reasoning_content": "核对一下"}))
            events.append(chunk({"tool_calls": [{"index": 0, "id": "call_2", "type": "function", "function": {"name": "run_shell", "arguments": args}}]}, "tool_calls"))
        else:
            seen = tools[-1]["content"].splitlines()
            events.append(chunk({"role": "assistant", "content": "做完了：hello.txt 已写好，"}))
            events.append(chunk({"content": "核对结果：" + " / ".join(seen[:3])}, "stop"))
        events.append({"choices": [], "usage": {"prompt_tokens": 1200, "completion_tokens": 30}})
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.end_headers()
        for event in events:
            self.wfile.write(f"data: {json.dumps(event, ensure_ascii=False)}\n\n".encode())
            self.wfile.flush()
        self.wfile.write(b"data: [DONE]\n\n")


server = ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1])), Handler)
# 端口给 0 时由系统挑一个空的，第一行打出来（测试读它）
print(server.server_address[1], flush=True)
server.serve_forever()

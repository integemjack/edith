"""假的 OpenAI 兼容流式模型：按历史里已有几条工具结果决定下一步（写文件 → 跑命令核对 → 交付），把收到的请求记到 requests.jsonl。
GET 是假的网页：/bing、/ddg、/baidu 是三家搜索的结果页（web_search 的测试），/page 是一个 GBK 编码的中文网页（fetch_url）"""
import json
import re
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

LOG = sys.argv[2] if len(sys.argv) > 2 else "requests.jsonl"


def chunk(delta, finish=None):
    return {"choices": [{"index": 0, "delta": delta, "finish_reason": finish}]}


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def do_GET(self):
        path = self.path.split("?")[0]
        pages = {
            "/bing": '<li class="b_algo"><h2><a href="https://bing.example/a">Bing 的结果</a></h2><p>必应摘要</p></li>'
                     '<li class="b_algo"><h2><a href="https://same.example/">重复的</a></h2></li>',
            "/ddg": '<a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fddg.example%2Fb">DDG 的结果</a>'
                    '<a class="result__snippet" href="x">鸭子摘要</a>'
                    '<a class="result__a" href="https://same.example">重复的</a>',
            "/baidu": '<div class="result c-container" mu="https://baidu.example/c"><h3><a href="x">百度的结果</a></h3>'
                      '<span class="content-right_x">百度摘要</span></div>',
        }
        if path == "/page":
            body = '<html><head><meta charset="gbk"><title>中文页</title></head><body><h1>标题一</h1><p>正文段落，<a href="/next">下一页</a></p></body></html>'.encode("gbk")
            content_type = "text/html"
        elif path in pages:
            body = pages[path].encode("utf-8")
            content_type = "text/html; charset=utf-8"
        else:
            self.send_response(404); self.end_headers(); return
        self.send_response(200)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        with open(LOG, "a", encoding="utf-8") as f:
            f.write(json.dumps(body, ensure_ascii=False) + "\n")
        messages = body["messages"]
        tools = [m for m in messages if m["role"] == "tool"]
        last_user = [m for m in messages if m["role"] == "user"][-1]["content"]
        events = []
        called = re.search(r"调用工具 (\S+)(?: 参数 (\{.*\}))?", last_user)
        last_user_index = max(index for index, m in enumerate(messages) if m["role"] == "user")
        tools_since = [m for m in messages[last_user_index + 1:] if m["role"] == "tool"]
        if called and not tools_since:
            # 「调用工具 X」：调 X（没参数），再把结果原样答出来
            events.append(chunk({"role": "assistant", "tool_calls": [{"index": 0, "id": "call_t", "type": "function", "function": {"name": called.group(1), "arguments": called.group(2) or "{}"}}]}, "tool_calls"))
        elif called:
            events.append(chunk({"role": "assistant", "content": "结果：" + tools_since[-1]["content"]}, "stop"))
        elif "只聊天" in last_user:
            events.append(chunk({"role": "assistant", "content": "你好，"}))
            events.append(chunk({"content": "我是 EDITH。"}, "stop"))
        elif "能力" in last_user and "记" in last_user and len(tools) == 0:
            args = json.dumps({"text": "hi"})
            events.append(chunk({"role": "assistant", "tool_calls": [{"index": 0, "id": "call_c", "type": "function", "function": {"name": "cap__remember", "arguments": args}}]}, "tool_calls"))
        elif "能力" in last_user and "记" in last_user:
            events.append(chunk({"role": "assistant", "content": "能力说：" + tools[-1]["content"]}, "stop"))
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

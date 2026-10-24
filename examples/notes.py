"""示例能力：笔记本（只有动作，演示 act）。只用 Python 标准库，照着改就能写自己的能力。

接上：edith capabilities add notes python3 -u examples/notes.py
之后对 EDITH 说「记一下：周五交报告」「我记过什么」，它会用 notes__save / notes__list。
笔记存在 ~/.edith/notes.json（EDITH_HOME 换了就跟着换）。"""
import json
import os
import sys
import time

# 协议一律 UTF-8、一行一条；日志写标准错误（EDITH 收进 ~/.edith/logs/notes.log）
sys.stdin.reconfigure(encoding="utf-8")
sys.stdout.reconfigure(encoding="utf-8")
HOME = os.environ.get("EDITH_HOME") or os.path.join(os.path.expanduser("~"), ".edith")
NOTES = os.path.join(HOME, "notes.json")


def send(message):
    sys.stdout.write(json.dumps(message, ensure_ascii=False) + "\n")
    sys.stdout.flush()


def reply(request, result=None, error=None):
    message = {"jsonrpc": "2.0", "id": request["id"]}
    if error:
        message["error"] = {"code": error[0], "message": error[1]}
    else:
        message["result"] = result
    send(message)


def load():
    try:
        with open(NOTES, encoding="utf-8") as f:
            return json.load(f)
    except (OSError, ValueError):
        return []


ACTIONS = [
    {
        "name": "save",
        "description": "记一条笔记（用户说「记一下」「别忘了」时用）",
        "parameters": {"type": "object", "properties": {"text": {"type": "string", "description": "要记的内容"}}, "required": ["text"]},
    },
    {
        "name": "list",
        "description": "列出记过的笔记（最近的在后面）",
        "parameters": {"type": "object", "properties": {"keyword": {"type": "string", "description": "只列含这个词的；不填列全部"}}},
    },
]


def act(action, arguments):
    notes = load()
    if action == "save":
        notes.append({"text": arguments["text"], "time": time.strftime("%Y-%m-%d %H:%M")})
        os.makedirs(HOME, exist_ok=True)
        with open(NOTES, "w", encoding="utf-8") as f:
            json.dump(notes, f, ensure_ascii=False, indent=2)
        return {"text": f"记好了（共 {len(notes)} 条）"}
    if action == "list":
        keyword = arguments.get("keyword") or ""
        found = [f"{note['time']}  {note['text']}" for note in notes if keyword in note["text"]]
        return {"text": "\n".join(found) if found else "没有记过"}
    raise KeyError(action)


for line in sys.stdin:
    message = json.loads(line)
    method = message.get("method")
    if method == "initialize":
        reply(message, {
            "protocolVersion": "1",
            "capability": {"title": "笔记本", "version": "1.0", "description": "记笔记、查笔记"},
            "senses": [],
            "actions": ACTIONS,
            "receives": [],
        })
    elif method == "act":
        params = message["params"]
        try:
            reply(message, act(params["action"], params.get("arguments") or {}))
        except KeyError as missing:
            reply(message, error=(-32602, f"缺参数或没有这个动作：{missing}"))
    elif method == "ping" and "id" in message:
        reply(message, {})
    elif method == "shutdown":
        break
    elif "id" in message and method:
        reply(message, error=(-32601, f"没有 {method}"))

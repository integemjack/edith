"""示例能力：收件箱（会主动说话，演示 sense + event）。只用 Python 标准库。

盯着一个文件夹（默认 ~/edith-inbox，可以用参数换）：放进去一个 .txt，它就把内容当成用户的话推给 EDITH；
EDITH 干完，它把回答写到同名的 .answer.md。文件名以 urgent- 开头的按「打断」推，note- 开头的「只记下」，其余「排队」。

接上：edith capabilities add inbox python3 -u examples/inbox.py ~/edith-inbox
然后 edith run（一直开着），往文件夹里丢文件试试。"""
import json
import os
import sys
import threading
import time

sys.stdin.reconfigure(encoding="utf-8")
sys.stdout.reconfigure(encoding="utf-8")
FOLDER = os.path.expanduser(sys.argv[1] if len(sys.argv) > 1 else "~/edith-inbox")
lock = threading.Lock()
next_id = 0
# 我们发出的 sense 请求编号 → 文件；EDITH 回话里有这一轮的编号（turn）
pending = {}
# 轮 → 文件（done 事件来了写回答）
turns = {}


def send(message):
    with lock:
        sys.stdout.write(json.dumps(message, ensure_ascii=False) + "\n")
        sys.stdout.flush()


def log(text):
    print(text, file=sys.stderr, flush=True)


def urgency(name):
    if name.startswith("urgent-"):
        return "interrupt"
    if name.startswith("note-"):
        return "note"
    return "queue"


def watch():
    global next_id
    os.makedirs(FOLDER, exist_ok=True)
    seen = set(os.listdir(FOLDER))
    while True:
        time.sleep(1)
        for name in sorted(os.listdir(FOLDER)):
            if name in seen or not name.endswith(".txt") or name.endswith(".answer.md"):
                continue
            seen.add(name)
            path = os.path.join(FOLDER, name)
            with open(path, encoding="utf-8") as f:
                text = f.read().strip()
            if not text:
                continue
            with lock:
                next_id += 1
                request = next_id
                pending[request] = path
            send({"jsonrpc": "2.0", "id": request, "method": "sense",
                  "params": {"text": text, "kind": "file", "from": name, "urgency": urgency(name)}})
            log(f"交上去了：{name}")


for line in sys.stdin:
    message = json.loads(line)
    method = message.get("method")
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": message["id"], "result": {
            "protocolVersion": "1",
            "capability": {"title": "收件箱", "version": "1.0", "description": f"把 {FOLDER} 里新放的 .txt 交给 EDITH"},
            "senses": [{"kind": "file", "description": "文件夹里新放进来的文字"}],
            "actions": [],
            "receives": ["done"],
        }})
    elif method == "initialized":
        threading.Thread(target=watch, daemon=True).start()
    elif method == "event":
        event = message["params"]
        path = turns.pop(event.get("turn"), None)
        if path and event.get("type") == "done":
            data = event["data"]
            answer = data.get("answer") or data.get("error") or "（没有回答）"
            with open(path[:-4] + ".answer.md", "w", encoding="utf-8") as f:
                f.write(answer + "\n")
    elif method is None and "id" in message:
        # 我们发的 sense 的回话：记下它算在哪一轮
        path = pending.pop(message["id"], None)
        result = message.get("result") or {}
        if path and result.get("turn"):
            turns[result["turn"]] = path
        elif path and "error" in message:
            log(f"没交上：{message['error'].get('message')}")
    elif method == "ping" and "id" in message:
        send({"jsonrpc": "2.0", "id": message["id"], "result": {}})
    elif method == "shutdown":
        break

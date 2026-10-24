"""测试用的能力（讲 EDITH 能力协议，只用标准库）：大脑起它，它连进大脑的套接字（EDITH_ACCESS，先报令牌）；
- 回 initialize：一个动作 remember、一种感知 speech；订哪些事件看环境变量 CAP_RECEIVES（逗号分开，默认 done,turn.started）
- 回 act：remember → 「记住了：<text>」（先等 CAP_ACT_DELAY 秒）
- 握手后按 CAP_SCRIPT（JSON 数组：[{"delay": 秒, "send": 一条消息}]）按时发消息给 EDITH
- 收到的每一条都记进 CAP_LOG（一行一条）；大脑断开或发 shutdown 就退出"""
import json
import os
import socket
import sys
import threading
import time

LOG = os.environ.get("CAP_LOG")
lock = threading.Lock()
host, port = os.environ["EDITH_ACCESS"].rsplit(":", 1)
connection = socket.create_connection((host, int(port)))
reader = connection.makefile("r", encoding="utf-8")
writer = connection.makefile("w", encoding="utf-8")


def send(message):
    with lock:
        writer.write(json.dumps(message, ensure_ascii=False) + "\n")
        writer.flush()


send({"jsonrpc": "2.0", "method": "auth", "params": {"token": os.environ["EDITH_ACCESS_TOKEN"]}})


def log(message):
    if LOG:
        with open(LOG, "a", encoding="utf-8") as f:
            f.write(json.dumps(message, ensure_ascii=False) + "\n")


def script():
    for step in json.loads(os.environ.get("CAP_SCRIPT", "[]")):
        time.sleep(step.get("delay", 0))
        send(step["send"])


receives = [kind for kind in os.environ.get("CAP_RECEIVES", "done,turn.started").split(",") if kind]
for line in reader:
    message = json.loads(line)
    log(message)
    method = message.get("method")
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": message["id"], "result": {
            "protocolVersion": "1",
            "name": os.environ.get("EDITH_CAPABILITY", "cap"),
            "capability": {"title": "测试能力", "version": "1"},
            "senses": [{"kind": "speech", "description": "假装听到的话"}],
            "actions": [{"name": "remember", "description": "记住一句话",
                         "parameters": {"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]}}],
            "receives": receives,
        }})
    elif method == "initialized":
        threading.Thread(target=script, daemon=True).start()
    elif method == "act":
        params = message["params"]
        send({"jsonrpc": "2.0", "method": "act.progress", "params": {"call": params["call"], "text": "在记", "fraction": 0.5}})
        time.sleep(float(os.environ.get("CAP_ACT_DELAY", "0")))
        send({"jsonrpc": "2.0", "id": message["id"], "result": {"text": "记住了：" + params["arguments"].get("text", "")}})
    elif method == "ping" and "id" in message:
        send({"jsonrpc": "2.0", "id": message["id"], "result": {}})
    elif method == "shutdown":
        break

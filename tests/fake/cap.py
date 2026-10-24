"""测试用的能力（讲 EDITH 能力协议，只用标准库）：大脑起它，它连进大脑的套接字（EDITH_ACCESS，先报令牌）；
- 回 initialize：一个动作 remember、一种感知 speech；订哪些事件看环境变量 CAP_RECEIVES（逗号分开，默认 done,turn.started）
- 回 act：remember → 「记住了：<text>」（先等 CAP_ACT_DELAY 秒）
- 握手后按 CAP_SCRIPT（JSON 数组：[{"delay": 秒, "send": 一条消息}]）按时发消息给 EDITH
- CAP_APPROVE=allow|always|deny：声明能批准（approves），大脑问 approve 时这样答（deny 带原因「先别动」）
- CAP_RELAY=<名字>：握手后再开一条中转连接（auth 带 as），它收到的记进 CAP_LOG 时包在 {"relay": …} 里
- CAP_MODEL_URL：报 models ["power://"]，model.endpoint 回这个地址、body 带 {"x_friday": {"test": 1}}
- CAP_HOOKS（逗号分开的时机）：报 hooks；CAP_HOOK_MODE 是 PreToolUse 怎么回——change（默认：参数改成「改过」）、deny（拦下：不许记）、
  need（替动作说批准要求：summary「要记东西」）；PostToolUse 补「钩子看过了」、UserPromptSubmit 补「钩子补的」、
  Stop 第一次不让停（让它「只聊天：再看一遍」）
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


def relay(name):
    """替别人开的连接：同一个令牌加 as，握手后什么也不报"""
    link = socket.create_connection((host, int(port)))
    link_reader = link.makefile("r", encoding="utf-8")
    link_writer = link.makefile("w", encoding="utf-8")
    def link_send(message):
        link_writer.write(json.dumps(message, ensure_ascii=False) + "\n")
        link_writer.flush()
    link_send({"jsonrpc": "2.0", "method": "auth", "params": {"token": os.environ["EDITH_ACCESS_TOKEN"], "as": name}})
    for line in link_reader:
        message = json.loads(line)
        log({"relay": message})
        if message.get("method") == "initialize":
            link_send({"jsonrpc": "2.0", "id": message["id"], "result": {"protocolVersion": "1", "name": "ignored"}})
        elif message.get("method") == "initialized":
            link_send({"jsonrpc": "2.0", "id": 900, "method": "sessions.create", "params": {}})
            link_send({"jsonrpc": "2.0", "id": 901, "method": "sessions.list", "params": {}})


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
            "approves": bool(os.environ.get("CAP_APPROVE")),
            "models": ["power://"] if os.environ.get("CAP_MODEL_URL") else [],
            "hooks": [event for event in os.environ.get("CAP_HOOKS", "").split(",") if event],
        }})
    elif method == "initialized":
        threading.Thread(target=script, daemon=True).start()
        if os.environ.get("CAP_RELAY"):
            threading.Thread(target=relay, args=(os.environ["CAP_RELAY"],), daemon=True).start()
    elif method == "act":
        params = message["params"]
        send({"jsonrpc": "2.0", "method": "act.progress", "params": {"call": params["call"], "text": "在记", "fraction": 0.5}})
        time.sleep(float(os.environ.get("CAP_ACT_DELAY", "0")))
        send({"jsonrpc": "2.0", "id": message["id"], "result": {"text": "记住了：" + params["arguments"].get("text", "")}})
    elif method == "approve":
        send({"jsonrpc": "2.0", "id": message["id"], "result": {"decision": os.environ["CAP_APPROVE"], "reason": "先别动"}})
    elif method == "model.endpoint":
        send({"jsonrpc": "2.0", "id": message["id"], "result": {"baseURL": os.environ["CAP_MODEL_URL"], "body": {"x_friday": {"test": 1}}}})
    elif method == "hook":
        params = message["params"]
        event = params["hook_event_name"]
        mode = os.environ.get("CAP_HOOK_MODE", "change")
        result = {}
        if event == "PreToolUse" and params.get("tool_name") == "cap__remember":
            if mode == "deny":
                result = {"permissionDecision": "deny", "permissionDecisionReason": "不许记"}
            elif mode == "need":
                result = {"need": {"target": "hi", "summary": "要记东西", "rule": "cap__remember(hi)"}}
            else:
                result = {"updatedInput": {"text": "改过"}}
        elif event == "PostToolUse":
            result = {"additionalContext": "钩子看过了"}
        elif event == "UserPromptSubmit":
            result = {"additionalContext": "钩子补的"}
        elif event == "Stop" and not params.get("stop_hook_active"):
            result = {"decision": "block", "reason": "只聊天：再看一遍"}
        send({"jsonrpc": "2.0", "id": message["id"], "result": result})
    elif method == "ping" and "id" in message:
        send({"jsonrpc": "2.0", "id": message["id"], "result": {}})
    elif method == "shutdown":
        break

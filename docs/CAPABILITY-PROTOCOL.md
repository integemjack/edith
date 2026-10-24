# EDITH 能力协议（第 1 版）

EDITH 是大脑；**能力**是它的四肢：耳朵、嘴、手、聊天渠道、桌面客户端、WebSocket 桥、机器人硬件……
能力和大脑之间是一条**双向、实时**的连接：

- 能力可以**主动**把内容推给大脑（听到一句话、收到一条消息、看到一个画面、一个文件变了），也可以交给大脑一件事等它的回答；
- 大脑让能力**做动作**，并把**干活的过程实时**推给关心的能力（边想边说的语音、边写边显示的界面）。

这和 MCP 不同：MCP 只能由大脑去调用工具，服务端没法主动对大脑说话。只会被调用的 MCP 服务在 EDITH 里也能接，算一种「不会说话的能力」（[§9](#9-mcp-能力)）。

本文档是开放的：任何语言、任何人都可以照着写一个能力。示例见 [`examples/`](../examples)（Python 标准库，几十行）、
[`tests/fake/cap.py`](../tests/fake/cap.py)；WebSocket 能力 [`edith-websocket`](WEBSOCKET.md) 也是照这份协议写的、不用大脑的代码。

---

## 1. 传输

**大脑不碰网络**：WebSocket、HTTP、MQTT、串口、蓝牙……都由能力自己去接（比如 [WebSocket 能力](WEBSOCKET.md)）。
协议本身和怎么连无关，EDITH 认两种本机的接法，消息一模一样：

1. **stdio**（主要的）：EDITH 按 `capabilities.json`（[§8](#8-配置capabilitiesjson)）起能力的进程，经它的标准输入 / 输出讲；
2. **本机接入**：自己打开的程序（终端 `edith attach`、桌面端……）连到在跑的大脑上。每个大脑在 `127.0.0.1` 的随机端口上等，
   地址和令牌写在 `~/.edith/instances/<目录的键>-<进程号>.json`（只有本用户能读，[§8.2](#82-在跑的大脑instances)）。
   连上后**第一行**先报令牌：`{"jsonrpc": "2.0", "method": "auth", "params": {"token": "…"}}`，不对就断开；
   之后和 stdio 一样（EDITH 发 `initialize`……）。一条连接就是一个能力，断开就摘掉、不重启。

下面说 stdio，本机接入只是把「标准输入 / 输出」换成这条 TCP 连接（日志用 `log` 通知）。

- EDITH 按配置起能力的进程。
- **标准输入 / 标准输出**：协议消息，**一行一条 JSON**（UTF-8，以 `\n` 结尾；JSON 里的换行要转义成 `\n`，不能有裸换行）。
  空行忽略。不是 JSON 的行 EDITH 回 `-32700` 并记日志。
- **标准错误**：日志，随便写；EDITH 逐行记进 `~/.edith/logs/<能力名>.log`（`edith -v` 时也打到终端）。
- 编码一律 UTF-8。Windows 上的 Python 记得 `sys.stdin.reconfigure(encoding="utf-8")`、`sys.stdout.reconfigure(encoding="utf-8")`。
- 进程起来时的环境变量：EDITH 自己的环境 + 配置里的 `env` + `EDITH_CAPABILITY=<能力名>`。

## 2. 消息格式：JSON-RPC 2.0

两个方向都可以发**请求**（带 `id`，要回话）和**通知**（不带 `id`，不回）。各自编号，互不相干。

```json
{"jsonrpc": "2.0", "id": 7, "method": "sense", "params": {"text": "明天几点开会？"}}
{"jsonrpc": "2.0", "id": 7, "result": {"session": "…", "disposition": "started", "turn": "a1b2c3d4e5f6"}}
{"jsonrpc": "2.0", "id": 8, "error": {"code": -32602, "message": "要给 text"}}
{"jsonrpc": "2.0", "method": "event", "params": {"session": "…", "turn": "…", "type": "text", "data": {"text": "好"}}}
```

- `id`：数字或字符串，原样带回。EDITH 发出的 `id` 是数字。
- 回话的顺序不一定和请求的顺序一样（EDITH 同时处理多个请求；`ask` 要等一轮做完才回）。
- 不认识的通知：忽略。不认识的请求：回 `-32601`。

### 错误码

| 码 | 意思 |
|---|---|
| -32700 | 不是 JSON |
| -32600 | 不是合法的 JSON-RPC 消息（或者能力发了 `initialize`——握手由 EDITH 发起） |
| -32601 | 没有这个方法 |
| -32602 | 参数不对（缺 `text`、`urgency` 取值不认识……） |
| -32603 | 内部错误（写不了存档、能力已经断开……） |
| -32001 | 还没握手：EDITH 的 `initialize` 回话之前发来的请求（`ping` 除外） |
| -32002 | 没有这个会话 |
| -32003 | 大脑用哪个模型还没设（`~/.edith/config.json`） |
| -32004 | 被停下了（动作被取消、等的那一轮被停下） |

## 3. 生命周期

```
EDITH                                  能力
  │ ── 起进程 ───────────────────────────▶ │
  │ ── initialize（请求）──────────────────▶ │
  │ ◀───────────── 回话：我能感知 / 能做什么 ── │
  │ ── initialized（通知）─────────────────▶ │
  │ ◀═══════════ 双向自由收发 ═══════════════▶ │
  │ ── shutdown（通知）、关 stdin ──────────▶ │  3 秒内自己退出，否则被结束
```

### 3.1 `initialize`（EDITH → 能力，请求）

```json
{"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
  "protocolVersion": "1",
  "brain": {"name": "EDITH", "version": "0.1.0", "model": "qwen-max", "id": "3f9a0c1b2d4e5f60", "workspace": "/Users/me/project"},
  "name": "ear",
  "focus": "5d0c…（焦点会话的 id，可能是空字符串）"
}}
```

- `brain.id`：这个大脑的 id（工作目录的键，同一个目录重启后不变）；`brain.workspace`：它管的目录。连着几个大脑的能力（共享设备）靠它分清是谁。
- `name`：EDITH 给这个能力起的名字（配置里的 `name`；本机接入连进来的是空字符串，用它回的 `name`）。它的动作在模型那里叫 `<name>__<动作名>`。
- `focus`：现在的焦点会话（[§5](#5-会话与焦点)）。

能力回话（30 秒内，否则 EDITH 当它没起来）：

```json
{"jsonrpc": "2.0", "id": 1, "result": {
  "protocolVersion": "1",
  "capability": {"title": "耳朵", "version": "1.0", "description": "本机麦克风，听到一句就交给大脑"},
  "senses": [{"kind": "speech", "description": "用户说的话（已经转成文字）"}],
  "actions": [
    {"name": "listen", "description": "开始 / 停止听", "parameters": {"type": "object", "properties": {"on": {"type": "boolean"}}, "required": ["on"]},
     "timeoutSeconds": 30}
  ],
  "receives": ["text", "done"],
  "sessions": "all"
}}
```

| 字段 | 必填 | 说明 |
|---|---|---|
| `protocolVersion` | 是 | `"1"` |
| `capability.title` / `version` / `description` | 否 | 给人和模型看的名字、版本、一句话 |
| `senses` | 否 | 会主动传来什么：`[{kind, description}]`。`kind` 是自己定的词（`speech`、`message`、`vision`、`file`、`sensor`……），写进模型看到的来源里 |
| `actions` | 否 | 能做的动作：`[{name, description, parameters, timeoutSeconds?}]`。`name` 只能是字母、数字、`-`、`_`（不合规的忽略）；`parameters` 是 JSON Schema（同 OpenAI 的工具参数）；`timeoutSeconds` 默认 600 |
| `receives` | 否 | 要收哪些事件（[§7](#7-事件event)）；`["*"]` 是全部，不写是都不要 |
| `sessions` | 否 | `"all"`（默认）：所有会话的事件；`"focus"`：只要焦点会话的 |
| `name` | 否 | 只对本机接入连进来的：想叫什么（重名的 EDITH 加 `-2`、`-3`……） |
| `endpoints` | 否 | 它对外开的入口：`[{kind, url}]`（比如 `{"kind": "websocket", "url": "ws://127.0.0.1:52731"}`），EDITH 登记进 `instances/`，`edith instances` 列得出来 |

模型的系统提示里会列出每个能力：名字、标题、有哪些动作、会主动传来什么。

### 3.2 `initialized`（EDITH → 能力，通知）

握手完了。之后两边都可以随便发。**在它之前能力发来的请求（`ping` 除外）回 `-32001`，通知不理。**

### 3.3 结束

- EDITH 退出、或这个能力被 `edith capabilities remove`：先发 `shutdown` 通知，再关 stdin；3 秒内没退就结束进程。
  **能力读到 stdin 结束（EOF）就该自己退出。**
- 能力自己退出（崩了、出错退出）：EDITH 摘掉它的动作，等着它回话的请求立刻失败；配置里 `restart` 不是 `false` 时
  过一会儿重启它（刚起来不到一分钟就退出的等 5 秒，跑了一分钟以上的等 1 秒；重启不成功等的时间每次翻倍，最多 60 秒）。
  启动时就接不上的，2 秒后开始同样地重试。

## 4. 能力 → EDITH

### 4.1 `sense`：主动推内容（请求或通知）

能力感知到了什么，推给大脑。做成请求能拿到「怎么处理的」，做成通知就不管了（出错只记日志）。

```json
{"jsonrpc": "2.0", "id": 5, "method": "sense", "params": {
  "text": "帮我把今天的会议纪要整理一下",
  "kind": "speech",
  "from": "小明",
  "urgency": "queue",
  "session": "focus"
}}
```

| 参数 | 必填 | 说明 |
|---|---|---|
| `text` | 是 | 内容（文字） |
| `kind` | 否 | 哪种感知（同 `senses` 里的 `kind`） |
| `from` | 否 | 说话的是谁（聊天渠道里的用户名、哪台设备……） |
| `urgency` | 否 | 急缓（[§4.3](#43-急缓urgency)），默认 `queue` |
| `session` | 否 | 交给哪个会话：会话 id（写开头几位也行，要唯一）、`"focus"`（默认）、`"new"`（在焦点会话的工作目录新开一个） |

回话：

```json
{"session": "5d0c…", "disposition": "started", "turn": "a1b2c3d4e5f6"}
```

- `disposition`：`started`（会话闲着，马上开始一轮）、`interrupted`、`interjected`、`queued`、`noted`（见 [§4.3](#43-急缓urgency)）。
- `turn`：这段话算在哪一轮（`noted` 没有，是 `null`）。拿它对上之后的事件。

模型看到的不是原话，而是标了来源的一段（能力能借此区分「谁在说」）：

```
（下面这段由能力「ear」（speech）传来，说话的是 小明）
帮我把今天的会议纪要整理一下
```

（措辞在提示词 `edith.input` / `edith.interjection` / `edith.note`，`~/.edith/Prompts/edith.md` 里可以改。）

### 4.2 `ask`：交一件事、等回答（请求）

参数同 `sense`（`urgency` 不能是 `note`）。**这一轮做完才回话**：

```json
{"session": "5d0c…", "turn": "a1b2c3d4e5f6", "disposition": "started",
 "answer": "整理好了，纪要在 notes/2026-10-24.md", "stopped": false, "error": null}
```

- `answer`：这一轮的最终回答；`stopped`：被停下（打断、`sessions.stop`）；`error`：出错的原因（模型连不上、额度用完……）。
- 这一轮的**所有事件**（[§7](#7-事件event)）都会推给发起 `ask` 的能力，不管它 `receives` 里订没订。
- 补充进了正在干的那一轮（`interjected`）时，等的是那一轮，`answer` 是那一轮的回答。

### 4.3 急缓（`urgency`）

一个会话同一时间只干一轮。会话闲着时，除了 `note`，都是马上开始一轮（`started`）。正在干的时候：

| 取值 | 名字 | 怎么处理 | `disposition` |
|---|---|---|---|
| `interrupt` | 打断 | 停下手上这一轮（停下前做了的留在历史里），马上带着新内容重来；排着的不受影响 | `interrupted` |
| `interject` | 补充 | 在下一个步骤之间（工具跑完、下一段请求之前）塞进这一轮；这一轮没有下一步了（直接答完）就紧接着为它再干一轮 | `interjected` |
| `queue` | 排队（默认） | 这一轮干完再干，先来先干 | `queued` |
| `note` | 只记下 | 记进会话（模型以后能看到），不回复、不打断；正在干时这一轮存档时记进去 | `noted` |

典型用法：语音里用户插话 → `interrupt`；用户说「顺便……」、聊天里追发一条 → `interject`；
新来一封邮件、定时任务 → `queue`；传感器读数、背景信息 → `note`。

### 4.4 会话（请求）

| 方法 | 参数 | 回话 |
|---|---|---|
| `sessions.list` | 无 | `[{id, title, cwd, created, updated, working}]`，新的在前 |
| `sessions.create` | `{cwd?, focus?}` | 新会话 `{id, title, cwd, created, updated}`；`cwd` 默认焦点会话的工作目录；`focus: true` 顺便设成焦点 |
| `sessions.get` | `{id}` | 会话全文：上面的字段 + `history`（OpenAI 格式的历史）+ `working`（在干的那一轮的 id 或 `null`）+ `focus` |
| `sessions.focus` | `{id}` | `{focus}`：设焦点 |
| `sessions.stop` | `{id?}` | `{stopping}`：停下正在干的一轮（排着的也不干了）；不给 `id` 是焦点会话 |

### 4.5 `act.progress`：动作的进度（通知）

```json
{"jsonrpc": "2.0", "method": "act.progress", "params": {"call": "c12", "text": "下载中", "fraction": 0.4}}
```

`call` 是 `act` 里给的那个。EDITH 把它变成那一轮的 `notice` 事件（「能力「x」：下载中（40%）」）。

### 4.6 `log`（通知）、`ping`（请求）

- `log {level?, text}`：记进这个能力的日志（和写标准错误一样）。
- `ping`：回 `{}`。两边都可以发，用来探活。

## 5. 会话与焦点

- **按目录工作**：一个大脑管一个目录（工作目录），会话存在 `<工作目录>/.edith/sessions/<id>.json`。工具的相对路径按会话的工作目录算。
- **焦点会话**：没指明会话的话交给它。`edith "…"` / `edith` 时是这次新开的会话；`edith run` 时是这个目录的**主会话**
  （id 记在 `<工作目录>/.edith/main-session`）。能力可以用 `sessions.focus` 换焦点。

## 6. EDITH → 能力

### 6.1 `act`：做一个动作（请求）

模型调了 `<能力名>__<动作名>` 这个工具时：

```json
{"jsonrpc": "2.0", "id": 12, "method": "act", "params": {
  "call": "c12", "action": "listen", "arguments": {"on": true}, "session": "5d0c…", "turn": "a1b2c3d4e5f6"
}}
```

回话：

```json
{"text": "开始听了", "ok": true, "media": [{"path": "/tmp/shot.png", "mime": "image/png"}]}
```

- `text`：给模型看的结果（不是字符串的会转成 JSON 文字；太长的留头尾截掉）。
- `ok`：默认 `true`；`false` 表示没做成（模型会看到失败）。
- `media`：产出的文件（`path` 或 `url`），列在结果后面。
- 回 JSON-RPC 错误也算没做成，`error.message` 交给模型。
- 超过 `timeoutSeconds`（默认 600）没回话：算失败。
- 做动作时可以随时发 `act.progress`。

### 6.2 `act.cancel`（通知）

```json
{"jsonrpc": "2.0", "method": "act.cancel", "params": {"call": "c12"}}
```

这一轮被停下了，EDITH 不再等这个动作。能力应该尽快停手；之后再回的话 EDITH 不理。

### 6.3 `event`（通知）

见 [§7](#7-事件event)。只推 `receives` 里订了的类型（以及它自己 `ask` 的那几轮的全部事件）。

### 6.4 `shutdown`（通知）

EDITH 要它退出（[§3.3](#33-结束)）。

## 7. 事件（`event`）

```json
{"jsonrpc": "2.0", "method": "event", "params": {
  "session": "5d0c…", "turn": "a1b2c3d4e5f6", "type": "text", "data": {"text": "好的，"}
}}
```

一轮从 `turn.started` 开始、到 `done` 结束，中间的按发生的顺序：

| `type` | `data` | 什么时候 |
|---|---|---|
| `turn.started` | `{text, origin: {capability, kind, from}}` | 开始一轮：交来的原话、从哪来（`capability` 为空是终端里打的） |
| `text` | `{text}` | 回答的文字，一块块到（拼起来是这一段回答） |
| `thinking` | `{text}` | 模型的思考（模型给了才有） |
| `tool` | `{id, name, arguments}` | 开始用一个工具（自带的、能力的动作、MCP 的） |
| `result` | `{id, name, ok, text}` | 用完 |
| `stats` | `{promptTokens, generatedTokens, cachedTokens, promptTime, generateTime, contextParts, contextLimit, contextCompactAt}` | 每段请求的用量（时间是秒；`contextParts` 是上下文各部分的 token：system / tools / user / replies / toolUse） |
| `notice` | `{text, call?}` | 顺带说一句：重试、压了上下文、收到补充、动作进度（带 `call`） |
| `done` | `{answer, stopped, error}` | 这一轮结束 |

想边生成边说话的语音能力订 `text` 和 `done`；界面订 `*`；只关心结果的订 `done`。

## 8. 配置：`capabilities.json`

两层，合起来用：**全局** `~/.edith/capabilities.json`（`EDITH_HOME` 换了就跟着换；所有目录都接）+ **目录**
`<工作目录>/.edith/capabilities.json`（只有这个目录接）。同名的以目录为准；目录里只写 `{"name": "x", "disabled": true}` 就是在这个目录关掉全局的 x。

```json
{"capabilities": [
  {"name": "ear", "command": "python3", "args": ["-u", "/opt/ear/ear.py"], "env": {"EAR_DEVICE": "0"}, "cwd": "/opt/ear"},
  {"name": "ws", "command": "edith-websocket", "args": ["--listen", "0.0.0.0:7789", "--token", "…"], "restart": true},
  {"name": "github", "protocol": "mcp", "command": "npx", "args": ["-y", "@modelcontextprotocol/server-github"], "env": {"GITHUB_TOKEN": "…"}},
  {"name": "search", "protocol": "mcp", "url": "https://example.com/mcp", "headers": {"Authorization": "Bearer …"}},
  {"name": "old", "command": "…", "disabled": true}
]}
```

| 字段 | 说明 |
|---|---|
| `name` | 能力名：只留字母、数字、`-`、`_`，最长 24 个字符 |
| `command` / `args` / `env` / `cwd` | 怎么起它（`command` 按 PATH 找） |
| `protocol` | `"edith"`（默认，本协议）或 `"mcp"` |
| `url` / `headers` | 只有 MCP：HTTP 的 MCP 服务（Streamable HTTP） |
| `restart` | 崩了要不要重启，默认 `true`（只对本协议的能力） |
| `disabled` | `true`：不接 |

能力进程的工作目录默认是大脑的工作目录，环境变量里有 `EDITH_WORKSPACE`（工作目录）、`EDITH_CAPABILITY`（能力名）。

命令行（默认改这个目录那层，加 `--global` 改全局那层）：

```
edith capabilities                                  列出能力（全局的 + 这个目录的；会先接上它们，看得到动作、感知、订的事件）
edith capabilities add <名字> <命令> [参数…]         接一个讲本协议的程序（接得上才记进配置）
edith capabilities add-mcp <名字> <命令> [参数…]     接一个本机的 MCP 服务
edith capabilities add-mcp-url <名字> <地址>          接一个 HTTP 的 MCP 服务
edith capabilities remove <名字>                     去掉
edith run                                           一直开着，只听能力的（主会话是焦点）
```

这个目录有大脑在跑时，`edith capabilities` 不另起一份（独占的设备会撞），`add` / `remove` 只改配置，那个大脑重启后生效。

### 8.2 在跑的大脑（`instances/`）

每个在跑的大脑写一份 `~/.edith/instances/<目录的键>-<进程号>.json`（只有本用户能读）：

```json
{"id": "3f9a0c1b2d4e5f60", "workspace": "/Users/me/project", "pid": 4242, "version": "0.1.0",
 "address": "127.0.0.1:52710", "token": "…", "started": "2026-10-24T15:02:11+08:00",
 "capabilities": ["ear", "ws"], "endpoints": [{"capability": "ws", "kind": "websocket", "url": "ws://127.0.0.1:52731"}]}
```

`address` / `token` 是本机接入（[§1](#1-传输)）；`endpoints` 是能力报的入口。进程没了的登记在 `edith instances` 列出时清掉。
想找某个目录的大脑、它的 WebSocket 地址，读这里。

## 9. MCP 能力

`"protocol": "mcp"` 的能力是 [MCP](https://modelcontextprotocol.io) 服务：它的工具当成动作（`<能力名>__<工具名>`），
大脑调、它回。它不会主动说话、不收事件——想要双向，就写本协议的能力（或者写一个小能力把 MCP 服务包起来）。

## 10. 安全

能力是**受信的本机程序**：它能以用户的名义让大脑做任何事（大脑能读写文件、跑命令）。只接你信得过的能力。
把大脑开到网络上的能力（比如 [WebSocket 能力](WEBSOCKET.md)）要自己管好认证：不是只听本机就必须设令牌。

## 11. 最小的能力（Python）

```python
import json, sys
sys.stdin.reconfigure(encoding="utf-8"); sys.stdout.reconfigure(encoding="utf-8")

def send(message):
    print(json.dumps(message, ensure_ascii=False), flush=True)

for line in sys.stdin:
    message = json.loads(line)
    method = message.get("method")
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": message["id"], "result": {
            "protocolVersion": "1",
            "capability": {"title": "问候"},
            "actions": [{"name": "greet", "description": "跟人打招呼",
                         "parameters": {"type": "object", "properties": {"who": {"type": "string"}}, "required": ["who"]}}],
            "receives": ["done"]}})
    elif method == "initialized":
        # 主动说一句：大脑会在焦点会话里开始一轮
        send({"jsonrpc": "2.0", "method": "sense", "params": {"text": "我上线了，有什么要我做的？", "kind": "event"}})
    elif method == "act":
        who = message["params"]["arguments"]["who"]
        send({"jsonrpc": "2.0", "id": message["id"], "result": {"text": f"已经跟 {who} 打过招呼"}})
    elif method == "event" and message["params"]["type"] == "done":
        print("大脑说：", message["params"]["data"]["answer"], file=sys.stderr, flush=True)
    elif method == "shutdown":
        break
```

接上：`edith capabilities add hello python3 -u hello.py`，然后 `edith run`。

## 12. 版本

`protocolVersion` 是 `"1"`。以后加的字段、方法、事件类型都是可选的：**不认识的字段忽略、不认识的通知忽略**，老的能力照样能用。
要改动已有字段的意思时才升版本号。

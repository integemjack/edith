# EDITH 能力协议（第 1 版）

EDITH 是大脑；**能力**是它的四肢：耳朵、嘴、手、聊天渠道、桌面客户端、远程设备、机器人硬件……
大脑只有**一个接口：本机套接字**。所有能力都从它连进来、都是**一级能力**，和大脑之间是一条**双向、实时**的连接：

- 能力可以**主动**把内容推给大脑（听到一句话、收到一条消息、看到一个画面、一个文件变了），也可以交给大脑一件事等它的回答；
- 大脑让能力**做动作**，并把**干活的过程实时**推给关心的能力（边想边说的语音、边写边显示的界面）。

这和 MCP 不同：MCP 只能由大脑去调用工具，服务端没法主动对大脑说话。MCP 服务不走这个协议：大脑直接读 `.mcp.json` 连（[§9](#9-mcp-服务)）。

本文档是开放的：任何语言、任何人都可以照着写一个能力。示例见 [`examples/`](../examples)（Python 标准库，几十行）、
[`tests/fake/cap.py`](../tests/fake/cap.py)。共用的工具——WebSocket 中转、共享设备——在
[edith.tools](https://github.com/integemjack/edith.tools)，也是照这份协议写的、不用大脑的代码，大脑要用时自己下载（[§8.4](#84-共用的工具edithtools)）。

---

## 1. 传输：大脑的套接字

**大脑只有这一个接口**，网络（WebSocket、HTTP、MQTT、串口、蓝牙……）都由能力自己去接（比如 edith.tools 的 WebSocket 中转
替每个远程客户端各开一条连接进来）。

- 每个在跑的大脑在 `127.0.0.1` 的随机端口上等。连上后**第一行**先报令牌：
  `{"jsonrpc": "2.0", "method": "auth", "params": {"token": "…", "as": "…（可选）"}}`，认不出就断开。之后 EDITH 发 `initialize`（[§3.1](#31-initializeedith--能力请求)）。
- **令牌定了你是谁、能做什么**（[§8.1](#81-权限permissions)）：
  - **专用令牌**：大脑起配置里的能力程序（[§3](#3-生命周期)）时每个发一个，在环境变量 `EDITH_ACCESS`（`127.0.0.1:端口`）、`EDITH_ACCESS_TOKEN` 里。
    拿它连进来的**名字就是配置里的名字**（别人抢不走；已经连着时再连回 `-32006`），权限按配置的 `permissions`；
  - **中转**：能力替别人开的连接（edith-websocket 替每个远程客户端）用自己的专用令牌加 `as`：名字是 `<能力名>-<as>`，
    权限是 `permissions.relay`（默认收窄，而且不会比替它中转的能力大）；
  - **主令牌**：写在 `~/.edith/instances/<目录的键>-<进程号>.json`（只有本用户能读，[§8.2](#82-在跑的大脑instances)），给本用户自己开的程序
    （终端 `edith attach`、桌面端……）：什么都能做，名字用它报的，但不能用配置里能力的名字（重了加 `-2`）。
- **一条连接就是一个能力**。一个程序可以开好几条（每条一个能力）：中转、设备服务就是这样替别人连进来的。
- 消息**一行一条 JSON**（UTF-8，以 `\n` 结尾；JSON 里的换行要转义成 `\n`，不能有裸换行）。空行忽略；不是 JSON 的行 EDITH 回 `-32700` 并记日志。
- 日志：大脑起的程序，标准输出、标准错误都记进 `~/.edith/projects/<工作目录>/logs/<能力名>.log`（`edith -v` 时也打到终端）；也可以发 `log` 通知。

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
| -32005 | 设备忙：共享设备正被别的大脑占着，`error.data.holder` 是占着它的大脑 `{id, workspace}`（[共享设备](https://github.com/integemjack/edith.tools/blob/main/docs/DEVICES.md)） |
| -32006 | 没有权限：这条连接的令牌不许做这件事（[§8.1](#81-权限permissions)）；或这个名字已经连着 |

## 3. 生命周期

```
EDITH                                  能力
  │ ── 起程序（给 EDITH_ACCESS / _TOKEN / EDITH_CAPABILITY）▶ │   配置里的才有这一步
  │ ◀──────────── 连进套接字、第一行 auth ── │
  │ ── initialize（请求）──────────────────▶ │
  │ ◀───────────── 回话：我能感知 / 能做什么 ── │
  │ ── initialized（通知）─────────────────▶ │
  │ ◀═══════════ 双向自由收发 ═══════════════▶ │
  │ ── shutdown（通知）、关连接 ─────────────▶ │  大脑起的程序 3 秒内自己退出，否则被结束
```

**配置里的能力**（[§8](#8-配置capabilitiesjson)）：大脑起它的程序，环境变量里给 `EDITH_ACCESS`、`EDITH_ACCESS_TOKEN`、
`EDITH_CAPABILITY`（它的名字）、`EDITH_WORKSPACE`（工作目录），等它用这个令牌连进来（最多 30 秒；程序起不来、退出码不是 0 都算没接上）。
程序可以一直开着（它就是那个能力），也可以让别人替它连进来后自己退出、退出码 0（比如共享设备的启动器）。

### 3.1 `initialize`（EDITH → 能力，请求）

```json
{"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
  "protocolVersion": "1",
  "brain": {"name": "EDITH", "version": "0.1.0", "model": "qwen-max", "id": "3f9a0c1b2d4e5f60", "workspace": "/Users/me/project"},
  "name": "ear",
  "focus": "5d0c…（焦点会话的 id，可能是空字符串）",
  "grant": {"sense": true, "ask": true, "sessions": "all", "events": true, "tools": "all"}
}}
```

- `brain.id`：这个大脑的 id（工作目录的键，同一个目录重启后不变）；`brain.workspace`：它管的目录。连着几个大脑的能力（共享设备）靠它分清是谁。
- `name`：大脑给它定的名字（专用令牌：配置里的名字；中转：`<能力名>-<as>`）；空字符串是主令牌的连接，用回话里报的。它的动作在模型那里叫 `<名字>__<动作名>`。
- `grant`：这条连接的权限（[§8.1](#81-权限permissions)）。越权的请求回 `-32006`。
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
| `actions` | 否 | 能做的动作：`[{name, description, parameters, timeoutSeconds?, readOnly?, exclusive?, holds?, releases?}]`。`readOnly: true`：不改东西（只读权限的一轮也能用）；`name` 只能是字母、数字、`-`、`_`（不合规的忽略）；`parameters` 是 JSON Schema（同 OpenAI 的工具参数）；`timeoutSeconds` 默认 600；后三个标记给[共享设备](https://github.com/integemjack/edith.tools/blob/main/docs/DEVICES.md)的仲裁用 |
| `receives` | 否 | 要收哪些事件（[§7](#7-事件event)）；`["*"]` 是全部，不写是都不要 |
| `sessions` | 否 | `"all"`（默认）：所有会话的事件；`"focus"`：只要焦点会话的 |
| `name` | 否 | 叫什么：只对主令牌的连接有用（不写是 `client`；重名的、和配置里能力重名的 EDITH 加 `-2`、`-3`……）；专用令牌、中转的名字由大脑定，这里写的不管 |
| `approves` | 否 | `true`：它能替用户批准（[§6.5](#65-approve请求)）。只有全权的连接会被问 |
| `endpoints` | 否 | 它对外开的入口：`[{kind, url}]`（比如 `{"kind": "websocket", "url": "ws://127.0.0.1:52731"}`），EDITH 登记进 `instances/`，`edith instances` 列得出来 |

模型的系统提示里会列出每个能力：名字、标题、有哪些动作、会主动传来什么。

### 3.2 `initialized`（EDITH → 能力，通知）

握手完了。之后两边都可以随便发。**在它之前能力发来的请求（`ping` 除外）回 `-32001`，通知不理。**

### 3.3 结束

- EDITH 退出、或这个能力被 `edith capabilities remove`：先发 `shutdown` 通知，再关连接；大脑起的程序 3 秒内没退就结束它。
  **连接断了就该自己退出。**
- 连接断了（程序崩了、退出了、网络中转那头走了）：EDITH 摘掉它的动作，等着它回话的请求立刻失败；配置里有它、`restart` 不是 `false` 时，
  1 秒后重起它的程序（还开着的先结束掉）；重起不成功等的时间每次翻倍，最多 60 秒。启动时就接不上的，2 秒后开始同样地重试。

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

权限（[§8.1](#81-权限permissions)）：一轮能用哪些工具看**交来这句话的连接**的 `tools`。所以补充只有和正在干的这一轮权限一样时才塞进去，
不一样的改成排队（不借别人的工具）；打断要全部工具的权限（`tools: "all"`），不然也改成排队（不许停下别人的活）。

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

- **按目录工作**：一个大脑管一个目录（工作目录），会话存在 `~/.edith/projects/<工作目录的路径，不是字母数字的换成 ->-<路径哈希>/sessions/<id>.jsonl`（只追加，一行一条）
  （不放进项目目录：同一个目录开几个大脑各写各的文件，worktree 删了对话也不丢）。工具的相对路径按会话的工作目录算。
- **客人只用自己名下的会话**：权限不全的连接（中转的远程设备、`permissions` 收窄过的能力，[§8.1](#81-权限permissions)）是客人。
  它交来的话进**它自己名下**的会话（会话的 `owner` 是它的名字；没指明会话、`focus` 都是它名下最近的一个，没有就新开），
  `sessions.list` / `sessions.get` 只看得到自己名下的，事件也只收自己会话的，不能换焦点、不能停下别人的会话。
  不然它的话进了主人的会话历史，之后全权的那一轮照样看得到（借权注入），它也能看主人的全部对话。
- **焦点会话**：没指明会话的话交给它。`edith "…"` / `edith` / `edith run` 时是这次新开的会话；`-c` 接着这个目录最近的会话，
  `-r <id>` 接着指定的。能力可以用 `sessions.focus` 换焦点。

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
- 回 JSON-RPC 错误也算没做成，`error.message` 交给模型；`-32005`（设备忙）时 EDITH 说清被哪个目录的大脑占着。
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

### 6.5 `approve`（请求）

模型要做要批准的事（改文件、跑命令、打开新网站、读工作目录外或敏感的文件、访问本机 / 内网、没说自己只读的动作和 MCP 工具）时，
EDITH 问能批准的人：这一轮是这个能力交来的、它声明了 `approves`、又是全权的，问它；不然问终端或别的能批准的全权能力。

```json
{"jsonrpc": "2.0", "id": 30, "method": "approve", "params": {
  "session": "…", "turn": "…", "tool": "run_shell", "target": "rm -rf build", "summary": "跑命令：rm -rf build（在 /w）",
  "arguments": {"command": "rm -rf build"}, "origin": "phone"
}}
```

回话：`{"decision": "allow" | "always" | "deny", "reason"?: "…"}`。`always`：这个会话里同类的都行；`deny` 的 `reason` 交给模型。
最多等 10 分钟，这一轮被停下就不等了。没人能批的就不做（`--auto`、`~/.edith/config.json` 的 `permissions` 见 [EDITH.md](EDITH.md#信任与审批)）。

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
| `tool` | `{id, name, arguments}` | 开始用一个工具（自带的、能力的动作、MCP 服务的） |
| `result` | `{id, name, ok, text}` | 用完 |
| `stats` | `{promptTokens, generatedTokens, cachedTokens, promptTime, generateTime, contextParts, contextLimit, contextCompactAt}` | 每段请求的用量（时间是秒；`contextParts` 是上下文各部分的 token：system / tools / user / replies / toolUse） |
| `notice` | `{text, call?}` | 顺带说一句：重试、压了上下文、收到补充、动作进度（带 `call`） |
| `done` | `{answer, stopped, error}` | 这一轮结束 |

想边生成边说话的语音能力订 `text` 和 `done`；界面订 `*`；只关心结果的订 `done`。

## 8. 配置：`capabilities.json`

三层，合起来用（同 Claude Code）：**全局** `~/.edith/capabilities.json`（`EDITH_HOME` 换了就跟着换；所有目录都接）→ **项目**
`<工作目录>/.edith/capabilities.json`（提交进仓库，大家共用）→ **本地** `<工作目录>/.edith/capabilities.local.json`（只给自己，不提交）。
同名的后面盖前面；只写 `{"name": "x", "disabled": true}`（没有 `command`）就是改前面那层的 x，别的照旧。

每一项是「起哪个程序」，程序起来后自己连进大脑的套接字（[§3](#3-生命周期)）：

```json
{"capabilities": [
  {"name": "ear", "command": "python3", "args": ["-u", "/opt/ear/ear.py"], "env": {"EAR_DEVICE": "0"}, "cwd": "/opt/ear"},
  {"name": "ws", "command": "edith-websocket", "args": ["--listen", "0.0.0.0:7789", "--token", "…"]},
  {"name": "mic", "command": "edith-connect", "args": ["mic"]},
  {"name": "phone", "command": "edith-websocket", "args": ["--listen", "0.0.0.0:7790", "--token", "…"],
   "permissions": {"sessions": "read", "tools": "read", "relay": {"ask": true, "tools": ["web_search", "fetch_url"]}}},
  {"name": "old", "command": "…", "disabled": true}
]}
```

| 字段 | 说明 |
|---|---|
| `name` | 能力名：只留字母、数字、`-`、`_`，最长 24 个字符 |
| `command` / `args` / `env` / `cwd` | 怎么起它。`command` 是光秃秃的名字时先找装好的工具（`~/.edith/tools/`），再找 PATH，都没有而 edith.tools 里有就下载（[§8.4](#84-共用的工具edithtools)）。`cwd` 默认是工作目录 |
| `restart` | 连接断了要不要重起，默认 `true` |
| `permissions` | 它能做什么（[§8.1](#81-权限permissions)），默认全开 |
| `disabled` | `true`：不接 |

命令行（默认改本地那层，`--project` 改项目那层，`--global` 改全局那层）：

```
edith capabilities                                  列出能力（三层合起来的；会先接上它们，看得到动作、感知、订的事件）
edith capabilities add <名字> <命令> [参数…]         接一个能力（起它、它连得进来才记进配置）
edith capabilities remove <名字>                     去掉
edith run                                           一直开着，只听能力的
```

这个目录有大脑在跑时，`edith capabilities` 不另起一份（独占的设备会撞），`add` / `remove` 只改配置，那个大脑重启后生效。

### 8.1 权限（`permissions`）

大脑只有套接字这一个接口，权限都在这里定（令牌定身份，[§1](#1-传输大脑的套接字)）。配置里每个能力可以写：

| 字段 | 取值 | 默认 | 管什么 |
|---|---|---|---|
| `sense` | `true` / `false` | `true` | 能不能 `sense` |
| `ask` | `true` / `false` | `true` | 能不能 `ask` |
| `sessions` | `"none"` / `"read"` / `"all"` | `"all"` | `read`：`sessions.list`、`sessions.get`；`all` 再加 `sessions.create`、`sessions.focus`、`sessions.stop` |
| `events` | `true` / `false` | `true` | 收不收订阅的事件（自己 `ask` 的那几轮总能收到） |
| `tools` | `"all"` / `"read"` / `"none"` / `["名字", "前缀*", …]` | `"all"` | 它交来的话，那一轮能用哪些工具。`read`：只读的——自带的看、找、搜、搜网页、看网页，能力写了 `readOnly` 的动作，MCP 写了 `readOnlyHint` 的工具；不许的工具不交给模型，硬调也拒绝。许了的照样要过审批（[§6.5](#65-approve请求)）；不是 `all` 的那一轮不能访问本机 / 内网 |
| `relay` | 上面几项 | `{sense, ask, events: true, sessions: "read", tools: "read"}` | 它替别人开的中转连接（`as`）的权限；不会比它自己的大 |

主令牌的连接（终端、桌面端）全部都能。越权的请求回 `-32006`。`tools` 不是 `all` 或 `sessions` 不是 `all` 的是**客人**，只用自己名下的会话（[§5](#5-会话与焦点)）。

项目、本地两层的配置（这里的和 [§9](#9-mcp-服务) 的）会起程序，要先**信任这个目录**才读：终端里起 edith 时列出要起的命令问一句，
或 `edith trust` 看过再信任；按内容的哈希记在 `~/.edith/trust.json`，内容变了（git pull 拉来新命令）要再信任。没信任时只用全局那层。

### 8.2 在跑的大脑（`instances/`）

每个在跑的大脑写一份 `~/.edith/instances/<目录的键>-<进程号>.json`（只有本用户能读）：

```json
{"id": "3f9a0c1b2d4e5f60", "workspace": "/Users/me/project", "pid": 4242, "version": "0.1.0",
 "address": "127.0.0.1:52710", "token": "…", "started": "2026-10-24T15:02:11+08:00",
 "capabilities": ["ear", "ws"], "endpoints": [{"capability": "ws", "kind": "websocket", "url": "ws://127.0.0.1:52731"}]}
```

`address` / `token` 是大脑的套接字和主令牌（[§1](#1-传输大脑的套接字)）；`endpoints` 是能力报的入口。进程没了的登记在 `edith instances` 列出时清掉。
想找某个目录的大脑、它的 WebSocket 地址，读这里。

### 8.3 共享设备

同一时间只能被一个进程打开的设备（麦克风、摄像头、串口……），几个目录的大脑经 `edith-connect` 共用一个设备服务：
设备服务连到每个大脑的套接字，当前大脑、设备忙（`-32005`）、动作带上是哪个大脑（`act.params.brain`）。
见 edith.tools 的 [DEVICES.md](https://github.com/integemjack/edith.tools/blob/main/docs/DEVICES.md)。

### 8.4 共用的工具（edith.tools）

[integemjack/edith.tools](https://github.com/integemjack/edith.tools) 放大家共用的能力程序：`edith-websocket`（远程能力的中转）、
`edith-connect`（共享设备）。每次发布在 Release 里放每个工具每个平台一个包和一份 `index.json`：

```
edith tools                    可装的和装了的（版本、有没有新的）
edith tools install <工具>…     下载、核对 SHA-256、装到 ~/.edith/tools/<工具>/
edith tools remove <工具>
```

能力配置里写了 `edith-connect` 这样的名字、本机又没有时，大脑当场下载。`EDITH_TOOLS_INDEX` 可以换索引（网址或本地文件）。

## 9. MCP 服务

[MCP](https://modelcontextprotocol.io) 服务只会被调用、不会主动说话，所以不做成能力：大脑**直接读配置**连（stdio 或 Streamable HTTP），
和 Claude Code 一样的写法、一样的三层——全局 `~/.edith/mcp.json` → 项目 `<工作目录>/.mcp.json`（提交进仓库）→ 本地 `<工作目录>/.edith/mcp.local.json`：

```json
{"mcpServers": {
  "github": {"command": "npx", "args": ["-y", "@modelcontextprotocol/server-github"], "env": {"GITHUB_TOKEN": "${GITHUB_TOKEN}"}},
  "docs": {"type": "http", "url": "https://example.com/mcp", "headers": {"Authorization": "Bearer ${DOCS_TOKEN}"}},
  "old": {"disabled": true}
}}
```

字符串里的 `${VAR}`、`${VAR:-默认}` 换成环境变量。工具在模型那里叫 `<服务名>__<工具名>`；写了 `readOnlyHint` 的算只读（[§8.1](#81-权限permissions)）。

```
edith mcp                                           列出 MCP 服务和它们的工具（会试连一下）
edith mcp add <名字> <命令> [参数…]                  加一个本机的（连得上才记；默认本地那层，--project / --global 同上）
edith mcp add-url <名字> <地址> [--header "名字: 值"]  加一个 HTTP 的
edith mcp remove <名字>
```

想要双向（MCP 服务也能主动说话），就照本协议写能力。

## 10. 安全

配置里的能力默认是**受信的本机程序**：权限全开时它能以用户的名义让大脑做任何事（大脑能读写文件、跑命令）。
只接你信得过的能力，不全信的用 `permissions` 收窄（[§8.1](#81-权限permissions)）。
大脑的套接字只听 `127.0.0.1`；专用令牌只交给大脑起的那个程序，主令牌写在只有本用户能读的登记里。
把大脑开到网络上的能力（比如 WebSocket 中转）要自己管好认证（不是只听本机就必须设令牌），它替远程设备开的连接用 `as`，
权限按 `permissions.relay`（默认只读的工具、只看会话）。

## 11. 最小的能力（Python）

```python
import json, os, socket

# 大脑起它时给地址、令牌、名字：连进大脑的套接字，第一行报令牌
host, port = os.environ["EDITH_ACCESS"].rsplit(":", 1)
connection = socket.create_connection((host, int(port)))
reader, writer = connection.makefile("r", encoding="utf-8"), connection.makefile("w", encoding="utf-8")

def send(message):
    writer.write(json.dumps(message, ensure_ascii=False) + "\n")
    writer.flush()

send({"jsonrpc": "2.0", "method": "auth", "params": {"token": os.environ["EDITH_ACCESS_TOKEN"]}})
for line in reader:
    message = json.loads(line)
    method = message.get("method")
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": message["id"], "result": {
            "protocolVersion": "1",
            "name": os.environ.get("EDITH_CAPABILITY", "hello"),
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
        print("大脑说：", message["params"]["data"]["answer"], flush=True)
    elif method == "shutdown":
        break
```

接上：`edith capabilities add hello python3 -u hello.py`，然后 `edith run`。

## 12. 版本

`protocolVersion` 是 `"1"`。以后加的字段、方法、事件类型都是可选的：**不认识的字段忽略、不认识的通知忽略**，老的能力照样能用。
要改动已有字段的意思时才升版本号。

# EDITH：下一代的大脑（原型）

用户 2026-10-24 定：服务端不再集成桌面端程序。把它想成一个**能自己干活的大脑**，开放对外接口；助手、聊天渠道、浏览器、
本地模型、通话、桌面客户端这些都是**能力**（四肢）——没有能力也能干活，接上能力更强。名字叫 **EDITH**。

能力和大脑之间是**双向、实时**的：能力能主动把内容推给大脑（这是它和 MCP 的根本区别：MCP 只能被大脑调用），
大脑能让能力做动作、把干活的过程实时推给它。**大脑只认一种接法——本机进程 + stdio**；WebSocket、HTTP 这些网络入口本身也是能力。

## 结构

```
            ┌───────────────────────── EDITH（edith）────────────────────────┐
 终端 ─────▶│ 调度 mind：会话、急缓（打断 / 补充 / 排队 / 记下）、事件         │
            │   │                                                           │
            │ 大脑 brain：调模型（流式）、用工具，直到交付                      │
            │   ├─ 自带工具 tools：读写改文件、找、搜、抓网页、跑命令            │
            │   ├─ 能力 capabilities：本协议（stdio，双向）◀──────────────────┼──▶ 能力进程：耳朵、渠道、edith-websocket……
            │   └─ MCP mcp：只会被调用的                ◀──────────────────┼──▶ MCP 服务
            └──────────────────────────────────────────────────────────────┘
```

- **大脑**（`src/brain.rs`）：一轮对话。决定都在 friday.core（子模块 `core/`，Friday 的 Swift / Qt 两端也用它）：
  对话循环 `engine_loop`、请求体 `chat_request`、流式解析 `chat_stream`、复读 / 截断 / 上下文压缩；HTTP（ureq）和起进程 EDITH 自己做。
- **调度**（`src/mind.rs`）：一个会话同一时间只干一轮；正在干的时候新来的话按急缓处理；干活过程推给收听者（终端、能力）。
- **自带工具**（`src/tools.rs`）：工具定义和说明来自 friday-core 的 `tool_defs` + `prompts/prompts-tools.md`，执行用 `tool_files`、`file_edit`、`file_search`、`shell_rules`。
- **能力**（`src/capabilities.rs`、`src/protocol.rs`）：[能力协议](CAPABILITY-PROTOCOL.md)，开放接口。
- **MCP**（`src/mcp.rs`）：`"protocol": "mcp"` 的能力，客户端是 friday-core 的 `mcp_client`。
- **WebSocket 能力**（`capabilities/websocket/`，程序 `edith-websocket`）：[说明](WEBSOCKET.md)。

## 用法

```
edith "把 src 里的 TODO 列成清单"        在当前目录干一件事（能力中途补充的、排着的也干完再退出）
edith                                   一句一句聊；干活时再打字算补充；Ctrl+C 停下这一轮
edith -c <会话 id> "接着把第二条做了"   接着某个会话
edith run                               一直开着，只听能力的（主会话是焦点）
edith sessions                          列出会话
edith capabilities [add | add-mcp | add-mcp-url | remove]   管能力
```

大脑用哪个模型：`~/.edith/config.json`（`{"baseURL", "apiKey", "model", "thinking"?, "temperature"?, "enableThinking"?, "supportsVision"?, "extraBody"?}`），
环境变量 `EDITH_BASE_URL` / `EDITH_API_KEY` / `EDITH_MODEL` 盖掉同名的。

数据目录 `~/.edith`（`EDITH_HOME` 换地方）：

| 文件 | 内容 |
|---|---|
| `config.json` | 模型 |
| `capabilities.json` | 接哪些能力（[协议 §8](CAPABILITY-PROTOCOL.md#8-配置capabilitiesjson)） |
| `sessions/<id>.json` | 会话（OpenAI 格式的历史 + 标题、工作目录、时间） |
| `main-session` | `edith run` 的主会话 id |
| `logs/<能力名>.log` | 能力的标准错误、协议出错 |
| `Prompts/` | 提示词（`edith.md` 是 EDITH 自己的；改了下一轮生效，改坏的段自动用自带的；friday-core 的 prompts 管合并） |

## 原型之后

- 子智能体 / 工作流（规则已在 friday-core：`agents`、`team`、`workflow`、`liveness`、`delivery`）；交付检查（`delivery`、`turnCheck`）；
- 记忆、任务状态、审批（`approval`）、密钥只以名字流转（`secret_guard`）；
- 能力层的「本事」：能力声明自己能做「听录音」「生图」这类固定的本事，大脑挑由谁来做（Friday 的 Capability / Provider 规则）；
- 桌面端（Swift / Qt）改成 EDITH 的能力；远程机器只装 EDITH。

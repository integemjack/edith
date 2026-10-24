# EDITH：下一代的大脑（第一个原型）

用户 2026-10-24 定：服务端不再集成桌面端程序。把它想成一个**能自己干活的大脑**，开放对外接口；助手、聊天渠道、浏览器、
本地模型、通话、桌面客户端这些都是**四肢**——没有四肢也能干活，挂上四肢能力更强。名字叫 **EDITH**。

## 它是什么

一个独立的 Rust 程序 `edith`（仓库 `integemjack/edith`），直接用 friday.core（子模块 `core/`，Friday 的 Swift / Qt 两端也用它）里的规则：

- **大脑**：给它用户的话，它自己调模型（OpenAI 兼容接口，流式）、用内置工具把事做完，存好会话。
  对话循环、请求体、流式解析、复读 / 截断处理、上下文压缩都是 `friday-core`（`engine_loop`、`chat_request`、`chat_stream`、
  `context`）；HTTP 和起进程 EDITH 自己做，不链 Qt、不带任何桌面端代码。
- **内置工具**（不靠四肢就有的）：读 / 写 / 改文件、列目录、按名字找文件、按内容搜、跑命令。工具定义和说明来自
  `friday-core` 的 `tool_defs` 和 `prompts/prompts-tools.md`（从 Friday 抄来的那份），执行用 `tool_files`、`file_edit`、`file_search`、`shell_rules`。
- **四肢**：
  - **能力四肢**＝MCP 服务（stdio 或 Streamable HTTP）：在 `~/.edith/limbs.json` 里写好、或运行中经接口挂上 / 摘下；
    挂上后它的工具和内置工具一起交给模型（名字前加 `<四肢名>__`）。MCP 客户端是 `friday-core` 的 `mcp_client`。
  - **身体四肢**＝用 EDITH 对外接口的程序：桌面客户端、聊天渠道、语音、网页……它们把用户的话交进来、看过程、拿结果。

## 用法

```
edith "把 src 里的 TODO 列成清单"        # 在当前目录干一件事，过程和结果打到终端
edith                                   # 一句一句聊（同一个会话）
edith -c <会话 id> "接着把第二条做了"   # 接着某个会话
edith sessions                          # 列出会话
edith limbs                             # 列出四肢和它们的工具
edith serve [--listen 127.0.0.1:7788]   # 开对外接口
```

大脑用哪个模型：`~/.edith/config.json`（`{"baseURL", "apiKey", "model", "thinking"?, "temperature"?}`），环境变量
`EDITH_BASE_URL` / `EDITH_API_KEY` / `EDITH_MODEL` 盖掉同名的。数据目录 `~/.edith`（`EDITH_HOME` 换地方）：
`config.json`、`limbs.json`、`sessions/<id>.json`、`Prompts/`（提示词，改了下一轮生效，`friday-core` 的 prompts 管合并）。

## 对外接口（`edith serve`）

HTTP + JSON，默认只听本机；`--token` 或 `EDITH_TOKEN` 设了就要 `Authorization: Bearer <token>`。

| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/v1/health` | `{name: "EDITH", version, model}` |
| GET | `/v1/sessions` | 会话列表 `[{id, title, updated, cwd}]` |
| POST | `/v1/sessions` | `{cwd?}` → 新会话 `{id}` |
| GET | `/v1/sessions/{id}` | 会话全文（OpenAI 格式的历史 + 元数据） |
| POST | `/v1/sessions/{id}/messages` | `{text}`：交给大脑干活，回 SSE 事件流（见下），干完关流 |
| POST | `/v1/sessions/{id}/stop` | 停下正在干的 |
| GET | `/v1/limbs` | 挂着的能力四肢和它们的工具 |
| POST | `/v1/limbs` | `{name, command, args?, env?, cwd?}` 或 `{name, url, headers?}`：挂上一个 MCP 四肢 |
| DELETE | `/v1/limbs/{name}` | 摘下 |

SSE 事件（`event:` 名 + `data:` JSON）：`text {text}`（回答的文字，一块块）、`thinking {text}`、
`tool {id, name, arguments}`（开始用一个工具）、`result {id, name, ok, text}`（用完）、`stats {...}`（每段请求的用量）、
`done {text}`（这一轮的最终回答）、`error {message}`。

## 原型之后

- 子智能体 / 工作流（规则已在 `friday-core`：`agents`、`team`、`workflow`、`liveness`、`delivery`）；
- 记忆、任务状态、审批（`approval`）、密钥只以名字流转（`secret_guard`）；
- 桌面端（Swift / Qt）改成连 EDITH 的客户端；远程机器只装 EDITH；
- 四肢反向接口：身体四肢也能给大脑提供工具（不只 MCP）。

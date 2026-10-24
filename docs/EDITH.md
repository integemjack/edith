# EDITH：下一代的大脑（原型）

用户 2026-10-24 定：服务端不再集成桌面端程序。把它想成一个**能自己干活的大脑**，开放对外接口；助手、聊天渠道、浏览器、
本地模型、通话、桌面客户端这些都是**能力**（四肢）——没有能力也能干活，接上能力更强。名字叫 **EDITH**。

能力和大脑之间是**双向、实时**的：能力能主动把内容推给大脑（这是它和 MCP 的根本区别：MCP 只能被大脑调用），
大脑能让能力做动作、把干活的过程实时推给它。**大脑只有一个接口——本机套接字**：所有能力（配置里大脑起的程序、终端、
远程设备经中转、共享设备、MCP 服务经桥）都从它连进来，都是一级能力。网络、设备、MCP 这些都不在大脑里。

## 结构

```
            ┌──────────────────── EDITH（edith）─────────────────────┐
 终端 ─────▶│ 调度 mind：会话、急缓（打断 / 补充 / 排队 / 记下）、事件  │
            │ 大脑 brain：调模型（流式）、用工具，直到交付               │
            │   ├─ 自带工具 tools：读写改文件、找、搜、抓网页、跑命令     │
            │   └─ 能力 capabilities：能力协议，双向 ◀── 套接字 instance ◀──┼── 每条连接一个能力：
            └──────────────────────────────────────────────────────┘      配置里的程序（起它、它连进来）、edith attach、
                                                                           edith.tools：edith-websocket（替远程客户端各开一条）、
                                                                           edith-connect（设备服务连各个大脑）、edith-mcp（MCP 桥）
```

- **大脑**（`src/brain.rs`）：一轮对话。决定都在 friday.core（子模块 `core/`，Friday 的 Swift / Qt 两端也用它）：
  对话循环 `engine_loop`、请求体 `chat_request`、流式解析 `chat_stream`、复读 / 截断 / 上下文压缩；调模型的 HTTP（ureq）EDITH 自己做。
- **调度**（`src/mind.rs`）：一个会话同一时间只干一轮；正在干的时候新来的话按急缓处理；干活过程推给收听者（终端、能力）。
- **自带工具**（`src/tools.rs`）：工具定义和说明来自 friday-core 的 `tool_defs` + `prompts/prompts-tools.md`，执行用 `tool_files`、`file_edit`、`file_search`、`shell_rules`。
- **能力**（`src/capabilities.rs`、`src/protocol.rs`）：[能力协议](CAPABILITY-PROTOCOL.md)，开放接口；配置里的程序由大脑起、等它连进来，断了重起。
- **套接字**（`src/instance.rs`）：127.0.0.1 随机端口 + 令牌；在跑的大脑登记在 `~/.edith/instances/`。
- **工具下载**（`src/downloads.rs`）：共用的工具在 [edith.tools](https://github.com/integemjack/edith.tools)，`edith tools` 或要用时自己下。

## 按目录工作

`edith .`：一个大脑管一个目录（工作目录），会话、能力、日志都在 `<目录>/.edith`。工作目录是从当前目录往上找到的第一个
有 `.edith/` 的目录，**到 git 仓库的根为止**（git worktree 的根也算，所以每个 worktree 自成一个工作目录）；都没有就是当前目录。

目录**不加锁**。这个目录已经有大脑在干活（`~/.edith/instances/` 里登记着）时，新起的大脑自动开一个 git worktree
（`<仓库根>/.edith/worktrees/<编号>`，分支 `edith/<编号>`，从 HEAD 开）去那里干，互不干扰；在子目录里起的对应到 worktree
的同一个子目录。`.edith/` 自动写进仓库的 `info/exclude`。worktree 和分支干完留着，由你合并（`git merge edith/<编号>`）或删掉。
不是 git 仓库就在原目录一起干；`--here` 强制在原目录。想接到正在跑的那个大脑上（共用它的会话和能力）用 `edith attach`。

## 用法

```
edith "把 src 里的 TODO 列成清单"        在这个目录干一件事（新会话；能力中途补充的、排着的也干完再退出）
edith [目录]                            一句一句聊（新会话）；干活时再打字算补充；Ctrl+C 停下这一轮
edith -c <会话 id> "接着把第二条做了"   接着某个会话
edith run [目录]                        一直开着，只听能力的（这个目录的主会话是焦点）
edith attach [目录] ["…"]               接到这个目录正在跑的大脑上（经套接字，自己当一个叫 terminal 的能力）
edith sessions                          列出这个目录的会话
edith instances                         列出这台机器上在跑的大脑、它们的能力和入口地址
edith capabilities [add | add-mcp | add-mcp-url | remove] [--global]   管能力（默认这个目录那层）
edith tools [install | remove]          共用的工具（edith.tools）
```

大脑用哪个模型：`~/.edith/config.json`（`{"baseURL", "apiKey", "model", "thinking"?, "temperature"?, "enableThinking"?, "supportsVision"?, "extraBody"?}`），
环境变量 `EDITH_BASE_URL` / `EDITH_API_KEY` / `EDITH_MODEL` 盖掉同名的。

数据分两层。全局 `~/.edith`（`EDITH_HOME` 换地方）：

| 文件 | 内容 |
|---|---|
| `config.json` | 模型 |
| `capabilities.json` | 所有目录都接的能力（[协议 §8](CAPABILITY-PROTOCOL.md#8-配置capabilitiesjson)） |
| `Prompts/` | 提示词（`edith.md` 是 EDITH 自己的；改了下一轮生效，改坏的段自动用自带的；friday-core 的 prompts 管合并） |
| `instances/<目录的键>-<进程号>.json` | 在跑的大脑：目录、进程、套接字的地址和令牌、能力、入口（[协议 §8.2](CAPABILITY-PROTOCOL.md#82-在跑的大脑instances)） |
| `tools/<工具>/` | 从 edith.tools 下的工具；`tools/installed.json` 记版本 |
| `devices.json`、`devices/` | 共享设备的定义、设备服务的登记和日志（edith-connect） |

目录 `<工作目录>/.edith`：

| 文件 | 内容 |
|---|---|
| `capabilities.json` | 只有这个目录接的能力（同名的盖掉全局的） |
| `sessions/<id>.json` | 会话（OpenAI 格式的历史 + 标题、工作目录、时间） |
| `main-session` | `edith run` 的主会话 id |
| `logs/<能力名>.log` | 能力的标准错误、协议出错 |
| `worktrees/<编号>/` | 这个目录有大脑在干活时，新起的大脑开的 git worktree |

## 原型之后

- 子智能体 / 工作流（规则已在 friday-core：`agents`、`team`、`workflow`、`liveness`、`delivery`）；交付检查（`delivery`、`turnCheck`）；
- 记忆、任务状态、审批（`approval`）、密钥只以名字流转（`secret_guard`）；
- 能力层的「本事」：能力声明自己能做「听录音」「生图」这类固定的本事，大脑挑由谁来做（Friday 的 Capability / Provider 规则）；
- 桌面端（Swift / Qt）改成 EDITH 的能力；远程机器只装 EDITH。

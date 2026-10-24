# EDITH：下一代的大脑（原型）

用户 2026-10-24 定：服务端不再集成桌面端程序。把它想成一个**能自己干活的大脑**，开放对外接口；助手、聊天渠道、浏览器、
本地模型、通话、桌面客户端这些都是**能力**（四肢）——没有能力也能干活，接上能力更强。名字叫 **EDITH**。

能力和大脑之间是**双向、实时**的：能力能主动把内容推给大脑（这是它和 MCP 的根本区别：MCP 只能被大脑调用），
大脑能让能力做动作、把干活的过程实时推给它。**大脑只有一个接口——本机套接字**：所有能力（配置里大脑起的程序、终端、
远程设备经中转、共享设备）都从它连进来，都是一级能力，令牌定了每条连接能做什么。MCP 服务只会被调用，不做成能力：
大脑直接读 `.mcp.json` 连。大脑自己会搜网页、看网页。

同一个目录怎么一起干、会话放哪、配置分几层，照 Claude Code 的做法（2026-10-24 定）：会话在项目外面、不加锁、
改文件前要读过且没被别人改过、worktree 要你说才开、配置分全局 / 项目 / 本地三层。

## 结构

```
            ┌──────────────────── EDITH（edith）─────────────────────┐
 终端 ─────▶│ 调度 mind：会话、急缓（打断 / 补充 / 排队 / 记下）、事件  │
            │ 大脑 brain：调模型（流式）、用工具，直到交付               │
            │   ├─ 自带工具 tools：读写改文件、找、搜、跑命令；web：搜网页、看网页 │
            │   ├─ MCP 服务 mcp：读 .mcp.json 直接连                     │
            │   └─ 能力 capabilities：能力协议，双向 ◀── 套接字 instance ◀──┼── 每条连接一个能力（令牌定权限 permissions）：
            └──────────────────────────────────────────────────────┘      配置里的程序（起它、它连进来）、edith attach、
                                                                           edith.tools：edith-websocket（替远程客户端各开一条，as）、
                                                                           edith-connect（设备服务连各个大脑）
```

- **大脑**（`src/brain.rs`）：一轮对话。决定都在 friday.core（子模块 `core/`，Friday 的 Swift / Qt 两端也用它）：
  对话循环 `engine_loop`、请求体 `chat_request`、流式解析 `chat_stream`、复读 / 截断 / 上下文压缩；调模型的 HTTP（ureq）EDITH 自己做。
- **调度**（`src/mind.rs`）：一个会话同一时间只干一轮；正在干的时候新来的话按急缓处理；干活过程推给收听者（终端、能力）。
- **自带工具**（`src/tools.rs`）：工具定义和说明来自 friday-core 的 `tool_defs` + `prompts/prompts-tools.md`，执行用 `tool_files`、`file_edit`、`file_search`、`shell_rules`。
  改文件（`write_file`、`edit_file`）前，这个会话要读过它、读完之后没被改过（别的大脑、命令、用户），不然让模型先重读——不靠锁。
- **网页**（`src/web.rs`）：`web_search` 同时查 Bing、DuckDuckGo、百度（照 Friday 的做法：按浏览器的样子请求、各自解析、轮流合并去重；
  `config.json` 写了 `"searxng"` 就只问它）；`fetch_url` 按页面编码解码，取标题、正文（标题层级、列表）和编号的链接，
  要 JS 才有内容的页面、回 403 / 429 / 503 的挡爬虫页（Cloudflare「请稍候…」）用本机的 Chrome / Edge 无头渲染后再读
  （渲染时用普通浏览器的 UA、关掉自动化标记，不然也会被挡）。
- **MCP**（`src/mcp.rs`）：读 `~/.edith/mcp.json`、`.mcp.json`、`.edith/mcp.local.json` 直接连，客户端是 friday-core 的 `mcp_client`。
- **权限**（`src/permissions.rs`）：主令牌（登记里，给自己开的程序）全权；配置里的能力各发专用令牌，名字由大脑定，权限按 `permissions`；
  中转的子连接（`as`）权限收窄。一轮能用哪些工具看交来这句话的连接。
- **能力**（`src/capabilities.rs`、`src/protocol.rs`）：[能力协议](CAPABILITY-PROTOCOL.md)，开放接口；配置里的程序由大脑起、等它连进来，断了重起。
- **套接字**（`src/instance.rs`）：127.0.0.1 随机端口 + 令牌；在跑的大脑登记在 `~/.edith/instances/`。
- **worktree**（`src/worktree.rs`）：`-w` 时开，退出时没改东西就删掉。
- **工具下载**（`src/downloads.rs`）：共用的工具在 [edith.tools](https://github.com/integemjack/edith.tools)，`edith tools` 或要用时自己下。

## 按目录工作

`edith .`：一个大脑管一个目录（工作目录）。工作目录是从当前目录往上找到的第一个有 `.edith/` 的目录，**到 git 仓库的根为止**
（git worktree 的根也算，所以每个 worktree 自成一个工作目录）；都没有就是当前目录。

目录**不加锁**，同一个目录可以开几个大脑一起干：会话各是各的文件（在 `~/.edith/projects/` 下，不在项目里），
改文件前要读过、读完之后没变过（被别人改了就重读再改），不会互相盖掉。

要并行又互不干扰时，**自己选** `-w`（`--worktree`）：开一个 git worktree（`<仓库根>/.edith/worktrees/<编号>`，分支 `edith/<编号>`，
从 HEAD 开）去那里干；在子目录里起的对应到 worktree 的同一个子目录。退出时没改东西（没有没提交的改动、分支上没有新提交）
就把 worktree 和分支删掉；改了的留着，告诉你怎么合并。`.edith/worktrees/`、`.edith/*.local.json` 自动写进仓库的 `info/exclude`。
worktree 里的会话记在仓库名下，回到仓库 `-c`、`edith sessions` 都找得到，worktree 删了也还在。被强行结束（没机会清）留下的，
`edith worktree` 看、`edith worktree prune` 清掉没改东西的。想接到正在跑的那个大脑上（共用它的会话和能力）用 `edith attach`。

## 信任与审批

照 Claude Code 的做法：

- **信任目录**（`src/trust.rs`）：项目、本地两层的能力和 MCP 配置会起程序，别人的仓库里写什么命令一跑就以你的身份执行。
  所以先信任才读：终端里起的会列出要起的命令问一句，不在终端里的只提醒；`edith trust` 看、`edith trust --yes` 信任、
  `edith trust revoke` 收回。按这几个文件内容的哈希记在 `~/.edith/trust.json`，内容变了要再信任；自己用 `edith capabilities add`、
  `edith mcp add` 改的照旧信任。
- **审批**（`src/approval.rs`）：写文件、改文件、跑命令、没说自己只读的能力动作和 MCP 工具；读工作目录外的、敏感的
  （`~/.ssh`、`.env`、密钥、`~/.edith`）；打开新网站（网址本身就能带数据出去）；访问本机 / 内网（防 SSRF，每一跳都看）——都先问。
  终端里打 `y` 这次行、`a` 这个会话里同类的都行、`n 原因` 不行；能力交来的那一轮问声明了 `approves` 的全权能力
  （[协议 §6.5](CAPABILITY-PROTOCOL.md#65-approve请求)），`edith attach` 接上来的终端也能批。没人能批的就不做（`edith run` 一直开着的大脑等能批准的连上来，最多 10 分钟）。
  `--auto`（或 `EDITH_PERMISSION_MODE=auto`）不问；`~/.edith/config.json`：
  `{"permissions": {"mode": "ask"|"auto", "allow": ["run_shell(git status*)", "fetch_url(github.com)"], "deny": ["run_shell(rm *)"], "additionalDirectories": ["~/notes"]}}`，
  deny 什么时候都挡。只认全局的 config.json：项目里的配置不能替你放开权限。
- **客人的会话**：权限不全的连接只用自己名下的会话，话不进主人的会话（防借权注入），也看不到主人的对话。

## 按需加载（工具、MCP、技能）

照 Claude Code（`src/deferred.rs`、`src/skills.rs`）：工具定义很占地方——说一句「你好」原来 4.8k token 里八成是工具定义，接一个 14 个工具的
MCP 服务每次请求再多 2.1k，而大多数用不上（改完实测：「你好」3.9k，接着那个 MCP 服务 4.1k，只多 0.2k）。
- **一直给的**：自带工具、`delegate_task`、`load_tools`（有技能时还有 `use_skill`）。
- **按需的**：能力的动作、MCP 服务的工具、派子智能体的其余工具（`run_parallel_tasks`、`run_workflow`、打回、看记录、追加指示、停掉）。
  系统提示里只列「名字（一句用途）」；模型用 `load_tools` 取回（写工具名、能力 / MCP 服务的名字取回它的全部、或者带 `*` 的前缀），
  取回后这个会话里一直给；派过子智能体的会话自动给管理用的那几个；直接调了没取回的也照样做。取回过的从历史里算，按第一次的先后接在后面，
  前面那一大段不变，服务商的提示词缓存不用重算。
- **MCP 服务用到时再起**：起来时不连；工具清单按配置的哈希记在 `~/.edith/mcp-cache.json`，取回它的工具、或者调用时才连上，连上后刷新缓存；
  没连过的服务清单里写「还没连过」，`load_tools` 写它的名字就连上。
- **技能**：`<名字>/SKILL.md`（开头 YAML 头 name / description，和 Claude Code 的 Agent Skills 通用），从项目的 `.edith/skills/`、`.claude/skills/`
  （要信任这个目录）、`~/.edith/skills/`、`~/.claude/skills/`、Friday 的 Skills 目录找。系统提示里只列名字和用途，用到时 `use_skill` 读全文；
  技能文件夹里的脚本、参考资料读的时候不算工作目录外。`edith skills` 列出来。

## 交付检查

照 Friday（`src/checks.rs`，规则在 friday-core 的 `delivery`）：一轮快结束时便宜地「起疑」，追一条提醒让智能体自己对照用户的原话判断，每种一轮最多一次：
- 写了代码、配置（`.py`、`.rs`、`.json`……），写完之后没有运行或读回确认 → 先验证再汇报（用户说了「不用运行」的不催）；
- 用户交代的是要做的事，交回来的却是方案、建议、「要不要我…」，这一轮又没真正动手 → 对照原话分析，该做就做完再交付；
- 只说了一句下一步（「接下来改 a.rs」）就结束了 → 接着做；
- 发现了问题却问用户要不要修 → 是自己做出来的、修法明确就修完。

## 子智能体与工作流

照 Friday（`src/team.rs`，规则在 friday-core 的 `agents`、`delivery`、`liveness`）：
- `delegate_task` / `run_parallel_tasks`：派临时子 Agent。主智能体派的**在后台跑**，立刻返回 id，这一轮可以先结束；做完的结果作为一段话交回
  这个会话（正在干活就补充进这一轮，闲着就开始新的一轮去验收）；一次 `run_parallel_tasks` 派出的全部做完一起交回，排着的几段交回也并成一轮
  ——一个回来开一轮验收，每轮都要把整段上下文再发一遍（真模型测：6 次请求 3.2 万 token → 4 次 2.1 万）。子 Agent 也能往下派（最多 3 层），等下级做完再返回。
- `run_workflow`：主智能体给出目标和步骤（任务、验收标准 `check`、依赖 `after`），依赖都做完的同时开始，写了 after 的带着前面的结果再开始，
  前置没做成的不做；全部结束后各步结果连同验收标准一起交回验收。任务里说要用前面的结果（「根据上一步…」）却没写 after 的退回去让它补上
  （真模型测时 glm-5.3-flash 三步都没写 after，结果同时开始）；没起名字的步骤用它的 id 当名字。
- `rework_agent`：验收不合格打回，原来那个子 Agent 带着它的上下文接着改；同一轮打回的几个都改完一起交回。
- `read_agent_log`、`message_agent`、`stop_agent`：看运行记录、追加指示、停掉（停掉工作流的 id 就停整个工作流）。停下这一轮（Ctrl+C、`sessions.stop`）时它派出去的也停。
- 子 Agent 看不到对话：系统提示（`edith.sub`）写明它是临时子 Agent、交回结果的格式（结论 / 依据 / 改动 / 未完成与风险），任务后面附用户的原话。
  交回前自己查一遍（没写结果、只说下一步、只交方案、写完没验证 → 原地打回），交给上级的结果后面附验收提示；结果太长的全文存进 `~/.edith/projects/<目录>/agents/`。
- 同时跑几个有上限（`EDITH_MAX_AGENTS`，默认 4），多的排队。工具权限、审批照派它的那一轮。子 Agent 只在这个进程里活着（打回要它原来的上下文）。
- 给界面看的（Friday 的 Agent 树、卡片里的运行记录）：推 `agent`（谁、谁派的、哪个调用派的、状态、在做什么）和 `agent.event`（它的每一步）事件，
  状态一变把运行记录存进 `projects/<目录>/agents/<会话>/<id>.json`；`agents.list` / `agents.stop` / `agents.send` 列出、停掉、追加指示（协议 §4.4）。

## 会话改动

同 Friday 的 SessionChanges（`src/changes.rs`，存的格式一样）：这段会话（含它派的子 Agent）第一次动一个文件之前的样子是基线，
存在 `projects/<目录>/changes/<会话>/`。write_file / edit_file 写之前记；run_shell 执行前后各看一遍（friday-core 的 `file_changes`），
命令改的、删的拿执行前的行指纹到 git 的 HEAD / 暂存区找原文，找不到只知道改过。`changes.list` / `changes.diff` / `changes.revert`
列出净改动、看改动、撤销（协议 §4.4）；Friday 的文件卡片「本次会话改动」经 edith-friday 用的就是它们。
- `edith "…"` 干完一件事等派出去的都交回、验收完再退出。

## 升级

起来时看一眼老的数据、配置（`src/migrate.rs`）：v0.1.9 及以前放在项目里的会话（`<目录>/.edith/sessions/*.json`）挪到
`~/.edith/projects/` 下换成 `.jsonl`，`main-session` 删掉；能力配置里经 `edith-mcp` 接的 MCP 服务（edith.tools 已经没有它了）、
更早的 `"protocol": "mcp"` 项，挪进同一层的 MCP 配置。

## 用法

```
edith "把 src 里的 TODO 列成清单"        在这个目录干一件事（新会话；能力中途补充的、排着的也干完再退出）
edith [目录]                            一句一句聊（新会话）；干活时再打字算补充；Ctrl+C 停下这一轮
edith -c "接着把第二条做了"             接着这个目录最近的会话（-r <会话 id> 接着指定的）
edith -w "…"                            开一个 git worktree 去那里干（没改东西退出时删掉）
edith worktree [list | prune | remove <编号> [--force]]   看 -w 开的 worktree、清掉没改东西的（被强行结束留下的）、删掉一个
edith run [目录]                        一直开着，只听能力的（新会话是焦点；-c / -r 同上；--idle 不新开会话；--linger <秒> 没有连接、闲这么久就退）
edith attach [目录] ["…"]               接到这个目录正在跑的大脑上（经套接字，自己当一个叫 terminal 的能力）
edith sessions                          列出这个目录的会话
edith instances                         列出这台机器上在跑的大脑、它们的能力和入口地址
edith capabilities [add | remove]       管能力（默认本地那层；--project 项目那层、--global 全局那层）
edith mcp [add | add-url | remove]      管 MCP 服务（同上）
edith tools [install | remove]          共用的工具（edith.tools）
```

管道送进来的也算要做的事（`cat 报错.log | edith "看看怎么回事"`）：给了参数时最多等 1.5 秒（从脚本、定时任务、别的智能体里起的 edith，
标准输入常常是一直不关的管子），没给参数就读到底。

大脑用哪个模型：`~/.edith/config.json`（`{"baseURL", "apiKey", "model", "thinking"?, "temperature"?, "enableThinking"?, "supportsVision"?, "extraBody"?}`），
环境变量 `EDITH_BASE_URL` / `EDITH_API_KEY` / `EDITH_MODEL` 盖掉同名的。

全局 `~/.edith`（`EDITH_HOME` 换地方）：

| 文件 | 内容 |
|---|---|
| `config.json` | 模型；`searxng`（可选，搜网页用自己的 SearXNG）；`permissions`（审批：mode、allow、deny、additionalDirectories） |
| `trust.json` | 信任了哪些目录的项目 / 本地配置（按内容哈希） |
| `capabilities.json`、`mcp.json` | 全局那层的能力、MCP 服务（[协议 §8](CAPABILITY-PROTOCOL.md#8-配置capabilitiesjson)、[§9](CAPABILITY-PROTOCOL.md#9-mcp-服务)） |
| `projects/<工作目录的路径，不是字母数字的换成 ->-<路径哈希前 8 位>/` | 这个目录的数据：`sessions/<id>.jsonl` 会话（只追加，一行一条；同一个会话几个大脑接着写也不互相盖掉）、`reads/<id>.json` 会话读过的文件（先读后改用）、`changes/<id>/` 会话改动的基线、`agents/<会话>/<id>.json` 子 Agent 的运行记录、`logs/<能力名>.log` 能力的日志 |
| `Prompts/` | 提示词（`edith.md` 是 EDITH 自己的；改了下一轮生效，改坏的段自动用自带的；friday-core 的 prompts 管合并） |
| `instances/<目录的键>-<进程号>.json` | 在跑的大脑：目录、进程、套接字的地址和主令牌、能力、入口（[协议 §8.2](CAPABILITY-PROTOCOL.md#82-在跑的大脑instances)） |
| `tools/<工具>/` | 从 edith.tools 下的工具；`tools/installed.json` 记版本 |
| `devices.json`、`devices/` | 共享设备的定义、设备服务的登记和日志（edith-connect） |

项目目录里只放**配置**：

| 文件 | 内容 |
|---|---|
| `.edith/capabilities.json`、`.mcp.json` | 项目那层（提交进仓库，大家共用） |
| `.edith/capabilities.local.json`、`.edith/mcp.local.json` | 本地那层（只给自己，不提交） |
| `.edith/worktrees/<编号>/` | `-w` 开的 git worktree |

## 原型之后

- 子智能体按活挑模型（`work`、`thinking`、`engine` 现在收下不用）、看门狗（`liveness`）、往下派的放后台（`wait_agents`）、`report_to_parent`；
- 记忆、任务状态、密钥只以名字流转（`secret_guard`）；
- 能力层的「本事」：能力声明自己能做「听录音」「生图」这类固定的本事，大脑挑由谁来做（Friday 的 Capability / Provider 规则）；
- 桌面端（Swift / Qt）改成 EDITH 的能力；远程机器只装 EDITH。

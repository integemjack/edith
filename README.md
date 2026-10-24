# EDITH

下一代的大脑（原型）。给它用户的话，它自己调模型、用内置工具（读写改文件、找文件、搜内容、搜网页、看网页、跑命令）和
MCP 服务（直接读 `.mcp.json`）把事做完、存好会话；活大时派子智能体、按工作流并联 / 串联去做，验收合格才交付；交付前查一遍
（写完没验证、交的是方案、只说了下一步的，自己接着做完）。
助手、聊天渠道、浏览器、本地模型、通话、桌面客户端都是**能力**（四肢）。大脑只有一个接口——本机套接字，所有能力都从它连进来、
都是一级能力（令牌定了每条连接能做什么），和大脑**双向、实时**地通信：能力能主动把内容推给大脑（打断、补充、排队、只记下），大脑能让能力做动作、
把干活的过程实时推给它。没有能力也能干活，接上了更强。

- 设计：[docs/EDITH.md](docs/EDITH.md)
- **能力协议（开放接口）**：[docs/CAPABILITY-PROTOCOL.md](docs/CAPABILITY-PROTOCOL.md)——任何语言照着写一个能力就能接上
- 共用的工具（WebSocket 中转、共享设备、Friday 客户端的网关）：[edith.tools](https://github.com/integemjack/edith.tools)，`edith tools` 下载，要用时也会自己下
- Friday 客户端（Mac / Windows / Linux）连 EDITH：那台机器装 `edith-friday` 当 `friday-server`（[edith.tools 的 FRIDAY.md](https://github.com/integemjack/edith.tools/blob/main/docs/FRIDAY.md)）
- 示例能力：[examples/notes.py](examples/notes.py)（动作）、[examples/inbox.py](examples/inbox.py)（主动推内容 + 收事件）

```bash
edith "把 src 里的 TODO 列成清单"      # 在这个目录干一件事（会话在 ~/.edith/projects/ 下；目录不加锁，可以开几个）
edith -c "接着做第二条"                # 接着最近的会话（-r <id> 接着指定的）
edith -w "重构登录"                    # 开一个 git worktree 去那里干，没改东西退出时删掉（edith worktree 看、prune 清）
edith .                               # 一句一句聊（干活时再打字算补充；改文件、跑命令前问你批不批）
edith trust                           # 看这个目录的配置要起哪些程序、信任它（别人的仓库先看再信）
edith attach                          # 接到这个目录正在跑的大脑上
edith instances                       # 这台机器上在跑的大脑
edith capabilities add inbox python3 -u examples/inbox.py ~/edith-inbox   # 接一个能力
edith capabilities add ws edith-websocket          # 让网页、手机连进来（edith-websocket 本机没有就自己下）
edith mcp add fs npx -y @modelcontextprotocol/server-filesystem ~/Documents   # 接一个 MCP 服务（也可以直接写 .mcp.json；用到时才起）
edith skills                          # 列出技能（~/.edith/skills、~/.claude/skills 都认；只列名字和用途，用到时读全文）
edith run                             # 一直开着，只听能力的
```

模型：`~/.edith/config.json` 写 `{"baseURL": "https://…/v1", "apiKey": "…", "model": "…"}`，或设环境变量
`EDITH_BASE_URL` / `EDITH_API_KEY` / `EDITH_MODEL`（任何 OpenAI 兼容的接口，本机的 llama-server、Ollama 也行）。

## 目录

```
src/            main（命令行）、mind（调度：会话、急缓、事件）、brain（一轮对话）、tools（自带工具、先读后改）、web（搜网页、看网页）、
                checks（交付检查）、team（子智能体、工作流）、approval（审批）、trust（信任目录）、migrate（升级）、
                mcp（读 .mcp.json 连 MCP 服务）、capabilities + protocol（能力协议）、permissions（令牌和权限）、
                instance（套接字、登记）、attach（接上去的终端）、downloads（edith tools）、
                worktree（-w 开 git worktree）、terminal（终端上的样子）、sessions、http、home
examples/       示例能力（Python 标准库）
prompts/        提示词：edith.md 是 EDITH 自己的；prompts-tools.md、prompts-framework.md 抄自 Friday（工具说明、循环里的提醒）
core/           子模块 integemjack/friday.core（Friday 的 Swift / Qt 两端也用它）：对话循环、请求体、流式解析、文件工具、MCP 客户端等规则
tests/          端到端测试：tests/fake/ 里的假模型（OpenAI 流式，兼做假的搜索结果页、网页）、假能力（cap.py，连进套接字）、假 MCP 服务（limb.py）
```

## 开发

```bash
git clone --recursive https://github.com/integemjack/edith.git
cargo test            # 要 python3（假模型、假能力）
cargo build --release
```

每次推送 GitHub Actions 编全平台、全架构的包（只有 edith 一个程序）：macOS（arm64、x64）、Windows（x64、x86、arm64）、Linux（x64、x86、arm64、armv7、
armv6、riscv64、loong64、ppc64le、s390x；zig 编成 musl 静态程序，不挑发行版），能在运行器上原生跑的同时跑端到端测试；
推到 `main` 的发一个 Release（`v<版本前两位>.<序号>`，只留最新一个）。
改 friday.core：在 `core/` 里拉新提交，提交子模块指针。

## 许可证

MIT 或 Apache-2.0 任选（[LICENSE-MIT](LICENSE-MIT)、[LICENSE-APACHE](LICENSE-APACHE)），`core/`（friday.core）同样。除非另有说明，提交进来的贡献按同样的双许可证授权。

# EDITH

下一代的大脑（第一个原型）。给它用户的话，它自己调模型、用内置工具（读写改文件、找文件、搜内容、抓网页、跑命令）把事做完、存好会话；
助手、聊天渠道、浏览器、本地模型、通话、桌面客户端都是**四肢**——经对外接口挂上来，没有四肢也能干活，挂上了能力更强。
设计见 [docs/EDITH.md](docs/EDITH.md)。

```bash
edith "把 src 里的 TODO 列成清单"      # 在当前目录干一件事
edith                                 # 一句一句聊
edith limbs add fs npx -y @modelcontextprotocol/server-filesystem ~/Documents   # 挂一个 MCP 四肢
edith serve --listen 127.0.0.1:7788   # 开对外接口（HTTP + SSE）
```

模型：`~/.edith/config.json` 写 `{"baseURL": "https://…/v1", "apiKey": "…", "model": "…"}`，或设环境变量
`EDITH_BASE_URL` / `EDITH_API_KEY` / `EDITH_MODEL`（任何 OpenAI 兼容的接口，本机的 llama-server、Ollama 也行）。

## 目录

```
src/            main（命令行）、brain（一轮对话）、tools（自带工具）、limbs（MCP 四肢）、server（对外接口）、sessions、http、home
prompts/        提示词：edith.md 是 EDITH 自己的；prompts-tools.md、prompts-framework.md 抄自 Friday（工具说明、循环里的提醒）
core/           子模块 integemjack/friday.core：对话循环、请求体、流式解析、文件工具、MCP 客户端等规则
tests/          端到端冒烟测试：tests/fake/ 里的假模型（OpenAI 流式）和假四肢（stdio MCP）
```

## 开发

```bash
git clone --recursive https://github.com/integemjack/edith.git
cargo test            # 要 python3（假模型、假四肢）
cargo build --release
```

每次推送 GitHub Actions 编全平台、全架构的包：macOS（arm64、x64）、Windows（x64、x86、arm64）、Linux（x64、x86、arm64、armv7、
armv6、riscv64、loong64、ppc64le、s390x；zig 编成 musl 静态程序，不挑发行版），能在运行器上原生跑的同时跑端到端测试；
推到 `main` 的发一个 Release（`v<版本前两位>.<序号>`，只留最新一个）。

`core/` 是私有仓库 integemjack/friday.core：下载 Release 里的包直接能用；从源码编要有它的读权限。改 friday.core：在 `core/` 里拉新提交，提交子模块指针。

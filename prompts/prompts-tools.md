@// 格式：「@@ 段名」开一段，到下一个「@@」为止；「@//」开头的行是注释；两层花括号括起来的名字是代码给的参数，前面带「>」的是引用另一段——都照原样留着。
@// 这是 Friday 数据目录里的那一份（<数据目录>/Prompts），谁都可以改：保存后下一轮起生效；某一段改坏了（用了没有的参数、引用了不存在的段）自动用自带的那段，
@// 设置 › 智能体 里能看到原因。装了 git 时每次改动自动记进历史，设置里能回滚到任一版本、恢复成自带的。
@// 内置工具的说明和参数说明（发给模型的工具定义里的 description）。格式见 qt/docs/PROMPTS.md
@// 段名：tool.<工具名> 是工具说明，tool.<工具名>.<参数>[.<子参数>] 是参数说明；ToolDescriptor.makeSpec 生成工具定义时有这一段就用它。
@// 参数结构（类型、必填）在代码里；MCP 工具、脚本工具、提供方带来的新能力用它们自己的说明，不在这里。
@// 平台参数（Prompts 自动带上）：trash、shell、shellHint、speakResult、toolCodeCwd、scriptCwd，取值见文件末尾的 platform.*

@// ———— 文件、命令、网页（AgentTools、TerminalTools） ————

@@ tool.read_file
读取文本文件内容。不给 offset / limit、整个文件不超过 8000 字符时原样返回；文件更长或给了 offset / limit 时按行返回，每行前面带行号，最多 8000 字符，结尾写明共几行、接着从哪行读。大文件先用 search_files 找到行号，再只读那一段。图片文件会作为附件展示给用户，理解图片内容请用 analyze_image。

@@ tool.read_file.limit
最多读几行；不填读到 8000 字符为止

@@ tool.read_file.offset
从第几行开始读（1 起）；不填从头读

@@ tool.read_file.path
文件路径

@@ tool.write_file
把文本写入文件，文件存在则整份覆盖。新建文件或整份重写时用；改已有文件里的几处用 edit_file。

@@ tool.write_file.content
要写入的完整内容

@@ tool.write_file.path
文件路径

@@ tool.edit_file
改已有文件：把 old_text 原样换成 new_text，返回改完的那几行（带行号）。old_text 要和原文一字不差（含缩进、换行）且只出现一次：不唯一就多带几行上下文，或设 replace_all。同一文件改几处用 edits 一次交（有一处对不上就都不改）。只换要改的几处，不要整份重写。

@@ tool.edit_file.edits
同一文件改几处：每项 {old_text, new_text, replace_all?}，按顺序改；给了它就不用上面的 old_text / new_text

@@ tool.edit_file.new_text
新内容；空字符串 = 删掉

@@ tool.edit_file.old_text
要换掉的原文（不带 read_file 的行号）

@@ tool.edit_file.path
文件路径

@@ tool.edit_file.replace_all
true = 全部都换

@@ tool.edit_file.edits.new_text
新内容；空字符串 = 删掉

@@ tool.edit_file.edits.old_text
要换掉的原文（不带 read_file 的行号）

@@ tool.edit_file.edits.replace_all
true = 全部都换

@@ tool.list_directory
列出目录下的文件和子目录。相对路径基于工作目录。

@@ tool.list_directory.path
目录路径，如 ~/Desktop 或 .

@@ tool.find_files
按文件名找文件（含子目录），返回路径（在工作目录下的写相对路径）；不给 pattern 就列出 path 这一层的文件和子目录。隐藏目录和 node_modules、build、dist、third_party 这类依赖 / 构建目录不进去找，要找就把 path 直接指过去。

@@ tool.find_files.path
在哪个目录下找；不填是工作目录

@@ tool.find_files.pattern
文件名通配，不分大小写：*.qml、*Settings*、ChatView.*、*.{cpp,h}；带 / 时按相对 path 的路径匹配，如 src/**/*.swift

@@ tool.search_files
搜文件内容（含子目录）。给 keywords：按关键词找，按字面、不用转义、默认不分大小写，几个词都在一个文件里（不必同一行，文件名里也算）就算，按文件列出、最相关的排前面，每个文件给最相关的几行——只记得几个词（功能名、界面文字、报错）时用它。给 pattern：按正则逐处列出「路径:行号: 那一行」，最多 200 处——要正则或每一处匹配时用它。两个给一个。找到后用 read_file 的 offset / limit 读相关几段。不搜二进制和超过 2 MB 的文件；隐藏目录、依赖 / 构建目录不进，要搜就把 path 指过去。

@@ tool.search_files.glob
只搜文件名匹配这个通配的文件，如 *.qml、*.{cpp,h}

@@ tool.search_files.ignore_case
true = 不分大小写

@@ tool.search_files.path
在哪个目录或文件里搜；不填是工作目录

@@ tool.search_files.pattern
正则，如 openSettings|Key_Comma；按字面找时转义 . ( ) [ ] + * ? 这类字符

@@ tool.search_keywords
按关键词搜文件内容（含子目录）：按字面、不用转义、默认不分大小写；几个词都在一个文件里（不必同一行，文件名里也算）就算。按文件列出，同一行有几个词的、文件名里有的排前面，每个文件给最相关的几行「行号: 那一行」。只记得几个词（功能名、界面文字、报错）时先用它；要正则或每一处匹配用 search_files。跳过的同 search_files。

@@ tool.search_keywords.glob
只搜文件名匹配这个通配的文件，如 *.qml、*.{cpp,h}

@@ tool.search_keywords.keywords
要找的词，空格分开，如 openSettings Key_Comma 或 下载 进度；带空格的整句加双引号，如 "no such file" open

@@ tool.search_keywords.match_any
true = 有任一个词就算（默认要都有）

@@ tool.search_keywords.match_case
true = 分大小写（默认不分）

@@ tool.search_keywords.path
在哪个目录或文件里搜；不填是工作目录

@@ tool.run_shell
@// shell：在哪个 shell 里跑（Windows 的 PowerShell / macOS 的 zsh / Linux 的 bash）；shellHint：Windows 上的 platform.shell-hint.powershell
在 {{shell}} 中执行 shell 命令并返回 stdout/stderr。要跑好几分钟、说不准多久的（远端的测试脚本、加载大模型、训练、下载）有 Friday 的终端动作（friday__run_in_terminal）时用它：它跑完或打出你要的话时会主动告诉你，不用一直等着、也不会到点被掐掉。默认 60 秒超时；编译、跑测试这类要更久的用 timeout_seconds 放宽（最多 600）。{{shellHint}}

@@ tool.run_shell.command
要执行的命令

@@ tool.run_shell.timeout_seconds
最多等几秒，默认 60，最多 600

@@ tool.run_in_terminal
在这段对话的内置终端里新开一个标签运行命令，马上返回、不等它结束（返回开头几秒的输出）。开发服务器、监听编译、要一直跑或要用户看着的命令用它；一会儿就跑完、要拿到结果的命令用 run_shell。终端在用户界面上看得见，用户随时能停；之后用 read_terminal 看它的输出。命令跑完终端就退出，这一轮结束时自动关掉；还在跑的用完了用 close_terminal 关掉。

@@ tool.run_in_terminal.command
要运行的命令（在默认的交互式 shell 里，工作目录是这段对话的）

@@ tool.close_terminal
关掉 run_in_terminal 开的一个终端（命令还在跑就停掉）。验证完的开发服务器、不再需要的监听编译这类用它收掉；用户要接着用的留着。用户自己开的终端关不了。

@@ tool.close_terminal.terminal
关哪个终端：编号（结果里给的 8 位）或标题里的字

@@ tool.read_terminal
读这段对话的内置终端（用户在界面上开的，或 run_in_terminal 开的）最近的输出。用户说「看下终端」「终端里的报错」时用它，不要让用户复制粘贴。不指定 terminal 时读最近有输出的那个；有几个终端时结果开头会列出来。

@@ tool.read_terminal.lines
读最近几行，默认 200，最多 2000

@@ tool.read_terminal.terminal
读哪个终端：编号（结果里给的 8 位）或标题里的字；不填读最近有输出的

@@ tool.read_terminal.wait_for
等到输出里出现这段字再读（可以是正则，不分大小写，如 Build succeeded|error）；命令结束了也不再等。等构建、测试、服务起来时用它，不要 sleep 干等

@@ tool.read_terminal.wait_seconds
最多等几秒，默认 300，最多 600；只给它不给 wait_for 就等到命令结束

@// ———— 内置浏览器（BrowserTools，v2.12） ————

@@ tool.browser_navigate
在这段对话的内置浏览器里打开网页（用户在浏览器卡片里看得见、能一起操作，登录状态一直留着），等加载完返回标题、网址和页面开头的文字。要看动态网页、要登录才能看、要点按钮填表时用它；只是读一篇公开文章用 fetch_url 更快。不给 url 给 action 时是后退 / 前进 / 刷新。你开的标签这一轮结束时自动关掉；用户让你打开给他看、要接着用的带上 keep。

@@ tool.browser_navigate.url
要打开的网址

@@ tool.browser_navigate.new_tab
在新标签里打开；不填在当前标签里打开（还没有标签时自动开一个）

@@ tool.browser_navigate.action
back / forward / reload（不给 url 时用）

@@ tool.browser_navigate.tab
在哪个标签：编号（结果里给的 8 位）、标题里的字或第几个；不填是当前标签

@@ tool.browser_navigate.keep
true：这个标签留给用户，这一轮结束时不关（用户让你打开给他看、要接着用的）；只给 tab 和 keep 是把已经开着的标签留下

@@ tool.browser_read
读当前网页：mode=text 是页面上的文字；mode=elements 列出可交互的元素（链接、按钮、输入框、下拉框…），每个一行带编号如 [e12]，之后用 browser_act 的 ref 指它。要点、要填之前先读 elements。

@@ tool.browser_read.mode
text（默认，页面文字）或 elements（可交互的元素和编号）

@@ tool.browser_read.tab
读哪个标签；不填是当前标签

@@ tool.browser_act
在网页上做一个动作（真的鼠标键盘事件，网页当成用户在操作）：click / double_click / right_click 点 ref 指的元素或 x、y；type 往 ref 指的输入框（或焦点处）输入 text，默认先清空，submit=true 输完按回车；key 按一个键或组合键；scroll 往 direction 滚 amount 格，给 ref 时滚到它；hover 把鼠标移上去；select 在下拉框 ref 里选 text 那一项；dialog 回答网页弹的对话框（accept、text）。返回动作后网址、标题、焦点有没有变。用户正在卡片里操作时会让开。要用户亲手做的（登录、验证码、付款确认）用 browser_hand_over，不要硬闯。

@@ tool.browser_act.action
click / double_click / right_click / type / key / scroll / hover / select / dialog

@@ tool.browser_act.ref
元素编号（browser_read elements 列出的，如 e12）；优先用它，比坐标可靠

@@ tool.browser_act.x
视口里的横坐标（CSS 像素，截图上的位置）；没有 ref 时用

@@ tool.browser_act.y
视口里的纵坐标（CSS 像素）

@@ tool.browser_act.text
type 时要输入的文字；select 时选项的文字或值；dialog 时 prompt 对话框要填的字

@@ tool.browser_act.submit
type 输完按回车

@@ tool.browser_act.append
type 时接在原有内容后面，不先清空

@@ tool.browser_act.key
key 时按的键：Enter、Tab、Escape、Backspace、ArrowDown、PageDown、Ctrl+A 这类

@@ tool.browser_act.direction
scroll 的方向：up / down / left / right，默认 down

@@ tool.browser_act.amount
scroll 滚几格，默认 5（大约半屏到一屏）

@@ tool.browser_act.accept
dialog 时：true 确定、false 取消，默认 true

@@ tool.browser_act.tab
在哪个标签；不填是当前标签

@@ tool.browser_screenshot
截当前网页的视口给你看（坐标就是 browser_act 的 x、y）。读文字、找元素先用 browser_read，看布局、图表、验证码这类只有图才看得出的再截图；截图也会显示给用户。

@@ tool.browser_screenshot.tab
截哪个标签；不填是当前标签

@@ tool.browser_resize
改网页的视口大小（网页认为的窗口大小，如 375×812 看手机版），用户的卡片大小不变、按比例缩小显示；fit=true 回到跟随卡片。

@@ tool.browser_resize.width
视口宽（CSS 像素，320–3840）

@@ tool.browser_resize.height
视口高（CSS 像素，240–2160）

@@ tool.browser_resize.fit
true：回到跟随卡片的大小

@@ tool.browser_resize.tab
改哪个标签；不填是当前标签

@@ tool.browser_tabs
列出这段对话的浏览器标签（编号、标题、网址），或切换、关闭一个。开新标签用 browser_navigate 的 new_tab。你开的标签这一轮结束时会自动关掉，不用一个个关。

@@ tool.browser_tabs.action
list（默认）/ select / close

@@ tool.browser_tabs.tab
select、close 的标签：编号、标题里的字或第几个

@@ tool.browser_hand_over
请用户在浏览器里亲手做一件事（登录、扫码、人机验证、两步验证、确认付款）：打开浏览器卡片、停在这个标签，顶上显示 reason 和「好了」按钮，马上返回。之后结束这一轮、告诉用户要做什么；用户点「好了」会发来消息，再接着做。

@@ tool.browser_hand_over.reason
请用户做什么，一句话（例如「登录 GitHub」「完成滑块验证」）

@@ tool.browser_hand_over.tab
哪个标签；不填是当前标签

@@ tool.browser_run_js
在当前网页里运行一段 JavaScript（网页自己的环境），返回最后一个表达式的值（JSON）。读不到、点不到的情况再用它；能用 browser_read / browser_act 的别用它。

@@ tool.browser_run_js.script
要运行的脚本

@@ tool.browser_run_js.tab
在哪个标签；不填是当前标签

@@ tool.web_search
搜索网页（同时查 Bing、DuckDuckGo、百度，合并去重），返回标题、网址和摘要。需要最新信息、查报错、查文档用法、核实没把握的事实时先搜；摘要不可靠，重要结论再用 fetch_url 打开原文核实。

@@ tool.web_search.count
返回几条，默认 8，最多 20

@@ tool.web_search.language
结果语言，如 zh-CN、en；查技术资料用 en 往往更全。不填由搜索服务决定

@@ tool.web_search.query
搜索词，像在搜索框里输入的那样简短；查报错就贴关键的一行

@@ tool.web_search.time_range
只看最近的结果：day、week、month、year；不填不限

@@ tool.fetch_url
打开网页看：返回标题、正文（保留标题层级、列表、段落）和页面里的链接（正文里写成「文字[编号]」，编号和网址列在最后，要跟进就用那个网址再 fetch_url），每次最多 8000 字符；正文更长时结尾写明共多少字、接着用 offset 从哪读（长文档分几次读，不要只看开头就下结论）。要 JS 才显示内容的页面，本机有 Chrome / Edge 时会自动渲染后再读。

@@ tool.fetch_url.offset
从正文第几个字开始读（0 起）；不填从头读

@@ tool.fetch_url.url
完整的 http(s) 地址

@@ tool.get_current_time
获取当前的本地日期、时间、星期和时区。

@// ———— 扩展自己的能力（MetaTools） ————

@@ tool.create_tool
当现有工具无法完成任务时，编写一个新的脚本工具并注册。创建后会自动用 test_arguments 自测，通过就启用，马上就能调用。

@@ tool.create_tool.code
@// toolCodeCwd：platform.tool-code-cwd（只有 Qt 的脚本工具拿得到 FRIDAY_CWD）
完整脚本源码：从标准输入读取 JSON 参数，把结果打印到标准输出；出错时写标准错误并以非零状态码退出{{toolCodeCwd}}

@@ tool.create_tool.description
工具做什么、什么时候该用它，写清楚参数含义

@@ tool.create_tool.language
python | bash | node

@@ tool.create_tool.name
工具名，小写字母开头，只含小写字母、数字、下划线，如 get_weather

@@ tool.create_tool.parameters
参数的 JSON Schema，例如 {"type":"object","properties":{"city":{"type":"string","description":"城市"}},"required":["city"]}

@@ tool.create_tool.permission
readOnly（只读）| network（需要联网）| writeFiles（需要写文件）| execute（需要执行任意命令，不受沙盒限制）

@@ tool.create_tool.secrets
脚本需要的密钥名数组（如 ["DASHSCOPE_API_KEY"]），运行时以同名环境变量注入；没有的密钥先用 request_secret 向用户申请

@@ tool.create_tool.test_arguments
用于自测的一组真实参数

@@ tool.create_tool.title
给用户看的中文名称

@@ tool.update_tool
修改已有的自定义工具（例如修复自测失败的代码）。只需提供要改的字段，旧版本会自动保留以便回滚。

@@ tool.update_tool.code
新的完整源码

@@ tool.update_tool.description
新的描述

@@ tool.update_tool.language
python | bash | node

@@ tool.update_tool.name
要修改的工具名

@@ tool.update_tool.parameters
新的参数 JSON Schema

@@ tool.update_tool.permission
readOnly（只读）| network（需要联网）| writeFiles（需要写文件）| execute（需要执行任意命令，不受沙盒限制）

@@ tool.update_tool.secrets
新的密钥名数组

@@ tool.update_tool.test_arguments
新的自测参数

@@ tool.delete_tool
删除一个自定义工具（会移到{{trash}}）。

@@ tool.delete_tool.name
要删除的工具名

@@ tool.list_tools
列出当前所有工具（内置、自定义脚本、MCP）及其状态。

@@ tool.read_tool
查看某个自定义工具的清单和源码，修改前先读一遍。

@@ tool.read_tool.name
工具名

@@ tool.add_mcp_server
接入一个 MCP（Model Context Protocol）server，获得它提供的全部工具。本地 server 给出 command 和 args，远程 server 给出 url（需要令牌时放在 headers 里）。用已存在的名字再次调用会覆盖原配置并重新连接，可用来修正配置或重试。

@@ tool.add_mcp_server.args
传给 command 的参数数组，如 ["-y", "@modelcontextprotocol/server-filesystem", "/path/to/dir"]

@@ tool.add_mcp_server.command
可执行文件名或路径，只能是一个词，如 npx、uvx、node、python3；参数不要写在这里

@@ tool.add_mcp_server.env
需要的环境变量，如 API Key；密钥写成 ${secret:名字}

@@ tool.add_mcp_server.headers
远程 server 的 HTTP 请求头，如 {"Authorization": "Bearer ${secret:GITHUB_TOKEN}"}

@@ tool.add_mcp_server.name
server 名称，字母数字下划线连字符，如 filesystem

@@ tool.add_mcp_server.url
远程 server 的 HTTP 地址（与 command 二选一）

@@ tool.remove_mcp_server
移除一个 MCP server 的配置并断开连接。

@@ tool.remove_mcp_server.name
server 名称

@@ tool.search_mcp_servers
按关键词查找可用的 MCP server 以及现成的启动配置（command/args 或 url）。接入之前先用它查，不要猜包名或命令。

@@ tool.search_mcp_servers.query
关键词，如 memory、filesystem、github、sqlite、fetch

@@ tool.get_diagnostics
排查问题时使用：返回最近的错误记录、MCP 服务的状态 / 配置 / 错误输出、自定义工具的自测结果、运行环境（PATH、可用的解释器）。

@@ tool.list_secrets
列出密钥库里已有的密钥名（不含值）。

@@ tool.request_secret
需要 API Key、令牌等密钥时，向用户申请。用户会在安全输入框里填写，值直接进入密钥库，你看不到明文；之后脚本工具通过 secrets 声明、MCP 配置通过 ${secret:名字} 引用。同名密钥已存在时会直接返回。

@@ tool.request_secret.name
密钥名，大写字母、数字、下划线，如 DASHSCOPE_API_KEY

@@ tool.request_secret.reason
告诉用户这个密钥用来做什么、去哪里获取

@@ tool.create_provider
写一个脚本提供方：给现有能力（看图、生图、说话等）多一种实现，或带来一项新能力（听写、OCR、抠图……）。新能力会成为一个工具，出现在设置 › 工具里由用户开关；同一项能力可以有几家提供方，调用时由 provider 参数选。同 id 再调用就是更新（旧版本自动保留）。给了 test 会自测，不通过就停用。

@@ tool.create_provider.capabilities
提供哪些能力：内置能力 vision | videoUnderstanding | imageGeneration | videoGeneration | transcription | speech，或 definitions 里新能力的 id

@@ tool.create_provider.check
脚本支持 action=check（检查依赖、模型是否就绪）时为 true；设置页和每次调用前会问它

@@ tool.create_provider.code
完整脚本：从 stdin 读一个 JSON 请求 {action: run | poll | check, capability, arguments（工具参数原样）, prompt / inputs / options（内置能力解析好的提示词、输入文件路径、尺寸等）, settings（用户设置）, output_dir, working_directory, job}。run 输出 {"text": …} 或 {"files": [写在 output_dir 里的文件]}（可同时带 text），要很久的可以返回 {"job": 任务 id}，之后用 action=poll 查询并输出 {"status": pending | running | succeeded | failed, "files": …}；check 输出 {"ready": true} 或 {"ready": false, "reason": 缺什么、怎么装}。出错写 stderr 并以非零状态退出

@@ tool.create_provider.definitions
新能力的定义（内置能力不要写）。每项 {"id": "transcribe", "title": "听写", "tool": "transcribe_audio", "description": "写给模型的工具说明", "parameters": 参数的 JSON Schema, "symbol": "SF Symbol 名", "detail": "界面上的一句话", "paid": 是否花钱}

@@ tool.create_provider.detail
一句话说明：用什么实现、有什么特点

@@ tool.create_provider.id
提供方 id：小写字母开头，只含小写字母、数字、下划线、连字符，如 whisper_local

@@ tool.create_provider.language
python | bash | node

@@ tool.create_provider.online
调用在线服务（不是本机离线）时为 true

@@ tool.create_provider.options
用户可以在设置里改的选项，每项 {"key", "title", "default", "hint", "control": text | choice | toggle | number | slider | folder | file | audio | secret, "choices": [值或 {value, title}], "min", "max", "capability": 只和某项能力有关时写它的 id, "advanced": true 表示收进「高级」}；脚本从请求的 settings 里读

@@ tool.create_provider.permission
readOnly（只读）| network（需要联网）| writeFiles（需要写文件）| execute（需要执行任意命令，不受沙盒限制）。要下载模型、写缓存或调用本机程序的选 execute

@@ tool.create_provider.secrets
需要的密钥名，运行时以同名环境变量注入；没有的先用 request_secret 申请

@@ tool.create_provider.test
自测用的一次调用 {"capability": 能力 id, "arguments": 工具参数}；会花钱或很慢的可以不给

@@ tool.create_provider.timeout
单次运行的超时秒数，默认 600，最长 3600

@@ tool.create_provider.title
给用户看的中文名

@@ tool.read_provider
查看一个脚本提供方的清单、源码和上次自测结果，修改前先读一遍。

@@ tool.read_provider.id
提供方 id

@@ tool.delete_provider
删除一个脚本提供方（移到{{trash}}）。只有它定义的新能力会一起消失。

@@ tool.delete_provider.id
提供方 id

@@ tool.configure_capability
启用 / 停用某个提供方或它的某项能力（看图、看视频、生成图片、生成视频、说话），改提供方的选项，或新建一个提供方。有允许的提供方提供某项能力时，对应工具（analyze_image、generate_image 等）才会出现；每次调用用哪个提供方由你在工具的 provider 参数里决定。先用 get_diagnostics 查看现状。

@@ tool.configure_capability.enabled
打开（true，默认）还是关闭（false）

@@ tool.configure_capability.kind
要开关的能力：vision（看图）| videoUnderstanding（看视频）| imageGeneration（生图）| videoGeneration（生视频）| transcription（听录音）| speech（说话）。和 provider 一起给只开关这个提供方的这项能力；只给 kind 则开关所有提供方的这项能力

@@ tool.configure_capability.options
提供方选项，如 {"baseURL": "https://api.deepseek.com/v1", "visionModel": "…"}

@@ tool.configure_capability.provider
要开关或修改的提供方 id（get_diagnostics 里能看到）；不给 kind 时开关整个提供方

@@ tool.configure_capability.title
新建提供方的显示名

@@ tool.configure_capability.type
新建提供方的类型：dashscope | openAICompatible | volcengineArk | volcengineSpeech（豆包语音）。已知服务只要用 request_secret 申请到密钥就会自动接入，一般不需要手动新建

@// ———— 多 Agent 协作（AgentTeamTools） ————

@@ tool.delegate_task
@// engineHint：按活自动挑时是 tool-part.engine-pool（列出可选的大脑，前面空一行），否则空（模型由设置定，系统提示和 work 参数里说了）
派一个临时子 Agent 独立完成一个子任务，返回它的最终结果（主 Agent 派的在后台跑：立刻返回 id，做完结果自动送回来）。子 Agent 再往下派时，交给下级的必须是自己任务里的一部分，不能原样往下传（主 Agent 可以把用户的整个任务交给一个子 Agent）。它交回结果后先不销毁，等你验收：不合格用 rework_agent 打回。{{engineHint}}

@@ tool.delegate_task.background
子 Agent 往下派时用（主 Agent 派的一律在后台）：true = 后台运行，立刻返回它的 id，你接着做别的；它中途的汇报和最终结果会自动送到你这里。不填 = 等它做完再返回

@@ tool.delegate_task.deadline_seconds
可选，最多给它多少秒：到点先提醒它交付已有结果，再过 60 秒还没交就停掉、交回做到一半的结果。不会超过你自己剩下的时间。不填就不限时

@@ tool.delegate_task.engine
@// 只有按活自动挑时才有这个参数
子 Agent 的大脑：在线引擎 id（可选的见工具说明）；不填跟随你自己的大脑（填了 work 按活选档）

@@ tool.delegate_task.name
给用户看的短名字，如 研究员、审稿人

@@ tool.delegate_task.role
这个子 Agent 的角色：一两句话写清职责和方法，如「你是研究员，用 web_search 搜索、fetch_url 读原文查证，给出来源网址」；回报的格式和长度由 Friday 规定，不用写。不填则是通才

@@ tool.delegate_task.subagent_type
系统提示「现成的子智能体」里列着的名字：照它写好的角色和工具派（这时 role 可以不填）。没有列出现成的子智能体时不填

@@ tool.delegate_task.task
完整、自包含的任务：子 Agent 看不到这段对话，写全背景、输入（文件路径等）、要交付的成品和怎样算做完

@@ tool.delegate_task.thinking
子 Agent 的深度思考：off / low / medium / high；不填按 work 定（look 低、change 中、hard 高，没填 work 按中），不会超过这段对话设的档位

@@ tool.delegate_task.work
@// workTail：tool-part.work.choose / tool-part.work.fixed
这是什么活：look（只读的查看、搜索、盘点、总结）/ change（改东西、跑命令、装软件）/ hard（设计、疑难排查、审查这类要深想的）；{{workTail}}

@@ tool.run_parallel_tasks
@// engineHint：同 delegate_task
同时派发多个互不依赖的子任务给各自的临时子 Agent，等全部完成后一起返回（主 Agent 派的在后台跑：立刻返回各自的 id，结果陆续自动送回来）。派几个由你按任务需要决定，不限数量；同时跑几个由系统决定（按接口限流自动调整），其余自动排队。{{engineHint}}

@@ tool.run_parallel_tasks.background
子 Agent 往下派时用（主 Agent 派的一律在后台）：true = 全部在后台运行，立刻返回各自的 id，你接着做别的；它们中途的汇报和最终结果会陆续送到你这里。不填 = 等全部做完再返回

@@ tool.run_parallel_tasks.deadline_seconds
可选，每个子 Agent 最多多少秒：到点先提醒交付，再过 60 秒还没交就停掉、交回做到一半的结果。不会超过你自己剩下的时间。不填就不限时

@@ tool.run_parallel_tasks.tasks
@// engineField：按活自动挑时是「engine?, 」，否则空
任务数组，每项 {task, role?, subagent_type?, name?, {{engineField}}work?, thinking?}

@@ tool.run_parallel_tasks.tasks.engine
@// 只有按活自动挑时才有这个参数
在线引擎 id（可选的见工具说明）；不填跟随你自己的大脑（填了 work 按活选档）

@@ tool.run_parallel_tasks.tasks.name
短名字

@@ tool.run_parallel_tasks.tasks.role
角色说明：职责和方法

@@ tool.run_parallel_tasks.tasks.subagent_type
现成子智能体的名字（系统提示里列着的；没有就不填）

@@ tool.run_parallel_tasks.tasks.task
完整、自包含的任务（子 Agent 看不到这段对话）

@@ tool.run_parallel_tasks.tasks.thinking
深度思考 off / low / medium / high；不填按 work 定

@@ tool.run_parallel_tasks.tasks.work
@// workTail：tool-part.item-work.choose / tool-part.item-work.fixed
什么活 look / change / hard；{{workTail}}

@@ tool.run_workflow
@// v2.75 只给任务模式是「工作流」的主 Agent。engineHint：同 delegate_task
按你定制的完整流程派子 Agent：每一步一个子 Agent，依赖都做完的步骤同时开始（并联），写了 after 的等前面的做完、自动拿到它们的结果再开始（串联）；前置步骤没做成的不做。在后台跑：立刻返回，全部步骤结束后各步的结果连同验收标准一起交给你验收，不合格用 rework_agent 打回。{{engineHint}}

@@ tool.run_workflow.goal
用户要的最终成品、怎样算完成（验收时对照它）

@@ tool.run_workflow.steps
@// v2.84 真模型实测：原来只写「每项 {id, task, check, …}」，glm-5.3-flash 头一次调用 15 次里 6 次整份不带 check、被退回重发；写明三个必填后 0/9
步骤数组（按大致的先后写）。每一项都必须有 id、task、check 三个字段（check 是这一步的验收标准，哪一步缺了整个工作流会被退回），其余可选：{id, task, check, after?, name?, role?, subagent_type?, {{engineField}}work?, thinking?}

@@ tool.run_workflow.steps.id
步骤 id，短而不重复，如 research、a1（after 里用它）

@@ tool.run_workflow.steps.task
这一步完整、自包含的任务（子 Agent 看不到这段对话）：背景、输入、要交付的成品

@@ tool.run_workflow.steps.check
验收标准：具体、能检查的合格条件（如「npm test 全过」「文件里有 X 和 Y 两节」）

@@ tool.run_workflow.steps.after
依赖哪几步（步骤 id 的数组）：它们做完、结果附进来之后这一步才开始。不写 = 一开始就做

@@ tool.run_workflow.steps.name
短名字

@@ tool.run_workflow.steps.role
角色说明：职责和方法

@@ tool.run_workflow.steps.subagent_type
现成子智能体的名字（系统提示里列着的；没有就不填）

@@ tool.run_workflow.steps.engine
@// 只有按活自动挑时才有这个参数
在线引擎 id（可选的见工具说明）；不填跟随你自己的大脑（填了 work 按活选档）

@@ tool.run_workflow.steps.work
@// workTail：tool-part.item-work.choose / tool-part.item-work.fixed
什么活 look / change / hard；{{workTail}}

@@ tool.run_workflow.steps.thinking
深度思考 off / low / medium / high；不填按 work 定

@@ tool.read_agent_log
查看一个子 Agent 的完整运行过程：调用了哪些工具、拿到了什么结果、中间写了什么。子 Agent 的结果末尾有运行记录 id；它给的依据要核对、或者结果和你掌握的情况对不上时用。记录太长时只给开头和结尾，中间的用 offset 分段读。

@@ tool.read_agent_log.id
运行记录 id（子 Agent 结果末尾写的那串，前 8 位即可）

@@ tool.read_agent_log.offset
从记录的第几个字开始读（0 起）；不填给开头和结尾

@@ tool.rework_agent
验收不合格时打回：写清哪里不合格、要交付的成品是什么，原来那个子 Agent 带着它已有的上下文（读过的文件、试过的路）接着做，做完返回新的结果。只能打回由你派出、已经交回结果（或因为模型服务出错失败、结果里写着「会话还留着」）的子 Agent（id 是它结果末尾运行记录 id 的前 8 位）：会保留到下一轮（用户对结果追问、纠正时直接打回它接着改）。打回两三次还不行，就换提示词重新派发（重写角色、换做法、拆得更细或换个大脑），框架会把之前走不通的路附给新的子 Agent。

@@ tool.rework_agent.feedback
验收意见：哪里不合格、缺什么、要交付的成品和怎样算做完

@@ tool.rework_agent.id
子 Agent 的运行记录 id

@@ tool.message_agent
给一个还在运行的子 Agent 追加指示（比如从别的子 Agent 那里拿到了新信息、要求变了），它做完当前这一步就会看到，接着做。id 是派发时返回的那串（前 8 位即可）。

@@ tool.message_agent.id
子 Agent 的 id

@@ tool.message_agent.message
追加的指示

@@ tool.stop_agent
停掉一个还在运行的子 Agent（方向错了、已经不需要了），其他子 Agent 照常。它的结束通知会送到你这里。

@@ tool.stop_agent.id
子 Agent 的 id

@@ tool.wait_agents
手头没有别的事、只想等后台子 Agent 时用：等到它们有中途汇报或结果（默认最多 120 秒），返回收到的内容。不用它也行：汇报和结果会在你做完当前一步时自动送到。

@@ tool.wait_agents.seconds
最多等几秒，默认 120

@@ tool.report_to_parent
做到一半有了阶段性结果、或者发现会影响全局的问题时，先交给派你的父级（它做完当前一步就会看到），然后你接着做。最终结果照常写在最后的回答里，不要用它交。

@@ tool.report_to_parent.message
要先交给父级的内容

@// ———— 任务状态 ————

@@ tool.update_task_state
记下任务状态（目标、用户要求、已定的决定、进度、之后要用的信息），之后每条用户消息后面附最新状态（何时记见系统提示「任务状态」）。只传要改的字段：goal、steps 整体替换；constraints、decisions、facts 只追加，过时的写进 remove。

@@ tool.update_task_state.constraints
新增约束：用户明确提的要求、偏好、不能做的事（路径、命名、语言、风格等）

@@ tool.update_task_state.decisions
新增已确认的决定

@@ tool.update_task_state.facts
新增之后要用的信息：路径、版本、网址、命令、名字等

@@ tool.update_task_state.goal
要达成什么，尽量用用户原话

@@ tool.update_task_state.remove
要删的过时条目（原文或其中一段）

@@ tool.update_task_state.steps
完整步骤列表（整体替换），每步 {text, status}，status：todo / doing / done

@@ tool.manage_tools
管理自定义脚本工具（新建、修改用 create_tool）。action：list 列出全部工具（内置、自定义脚本、MCP）和状态；read 看某个自定义工具的清单和源码，改之前先读；delete 删掉它（移到{{trash}}）。

@@ tool.manage_tools.action
list / read / delete

@@ tool.mcp_servers
查找、接入、移除 MCP 服务（Model Context Protocol）。action：search 按关键词查可用的 server 和现成的启动配置（接入前先查，不要猜包名或命令）；add 接入一个 server，拿到它的全部工具（本地的给 command / args，远程的给 url），同名的就是改配置；remove 移除并断开。

@@ tool.mcp_servers.action
search / add / remove

@@ tool.manage_providers
管理能力提供方（新建脚本提供方用 create_provider）。action：read 看一个脚本提供方的清单、源码和上次自测结果，改之前先读；delete 删掉脚本提供方（移到{{trash}}，它定义的新能力一起消失）；configure 启用 / 停用某个提供方或它的某项能力，改提供方的选项，或新建一个内置类型的提供方。

@@ tool.manage_providers.action
read / delete / configure

@@ tool.manage_commands
新建、改写或删除命令和现成的子智能体（同 Claude Code 的 slash commands、subagents 格式；用命令用 use_command）。kind：command 是命令（用户打「/名字 参数」时把正文当提示词发给你），subagent 是现成的子智能体（派活时用 subagent_type 点它）。action：save 新建或整份改写，delete 删掉（文件移到{{trash}}，只在用户要删或它已经过时时用）。

@@ tool.manage_commands.action
save / delete

@@ tool.manage_commands.kind
command（默认）/ subagent

@// ———— 问用户（v2.13 AskUser；主 Agent 和助手共用） ————

@@ tool.ask_user
拿不准又查不清时问用户：给 2–4 个选项，用户点一下就能答（也能自己写）。
只在几种做法会明显改变结果（方案、范围、命名、位置、动不动用户的东西），且文件、配置、记录里都查不到时问；无关紧要的按最合理的理解做，把假设写进结果。
一题一句话，每个选项用 detail 写清意味着什么，推荐的放第一个并标 recommended；几件事一次问完（最多 3 题），不要在回复里写问题等用户打字。
问完照回答接着做，不再确认；用户跳过就按推荐的做。

@@ tool.ask_user.questions
1–3 题，每题 {question, options, recommended, multiple}

@@ tool.ask_user.questions.question
一句话

@@ tool.ask_user.questions.options
2–4 个，每个 {label, detail}；不用加「其他」

@@ tool.ask_user.questions.options.label
几个字（按钮上的字）

@@ tool.ask_user.questions.options.detail
一行：选它意味着什么（可不填）

@@ tool.ask_user.questions.recommended
推荐第几个（1 起），没有就不填

@@ tool.ask_user.questions.multiple
能多选时填 true

@@ tool.answer_question
用户跟你说话时回答了一张在等的问题卡片（「[等用户回答的问题]」里列着的）：把回答交过去。用户说的是编号或选项（「第一个」「卡片式」）就放进 answers（每题一条，按题的顺序）；说了别的、或者一句话答了几题，原话放进 reply。

@@ tool.answer_question.card
卡片 id（「卡片 3f2a9c1b」里的那串）；只有一张在等时可以不填

@@ tool.answer_question.answers
每题的回答：选项名或用户的原话，按题的顺序；没答的那题写空串

@@ tool.answer_question.reply
用户的原话（拆不到每一题时用）

@// ———— 技能（v2.14 Skills；主 Agent 和子 Agent 都有） ————

@@ tool.use_skill
读一个技能的全文（SKILL.md），并列出它带的其他文件。有哪些技能用 search_skills 找（会话第一条消息后面的目录里也带了几个）：手上的活和某个技能对得上时，先用它读出来，再照里面写的做法做。技能带的脚本、模板、参考资料用 read_file 读、run_shell 跑（结果里给了完整路径）。

@@ tool.use_skill.name
技能名（search_skills 或目录里列的名字）

@@ tool.search_skills
按关键词找技能（做某一类事的现成做法），返回名字和用途；对得上的再用 use_skill 读全文。接到一类可能有现成做法的活（发版、写某种文档、某个项目的固定流程、用某个服务）时先找。

@@ tool.search_skills.query
关键词，空格分开，如「发版 打包」「周报」

@@ tool.search_skills.count
返回几个，默认 8，最多 20

@@ tool.search_memory
按关键词查这台机器的记忆库：用户说过的偏好、事实、决定、纠正，和在这台机器、这个项目上踩过的坑。按用户习惯做事、用户提到以前的事、要项目约定、遇到报错时先查；没查到换关键词再查。

@@ tool.search_memory.query
关键词，空格分开、简短，如「提交 署名」「构建 DerivedData」

@@ tool.search_memory.kind
只看一种：preference / fact / decision / correction / experience / habit；不填看全部

@@ tool.search_memory.scope
project（默认）：这个项目的和通用的；all：所有项目

@@ tool.search_memory.count
默认 8，最多 20

@@ tool.save_memory
用户明说要记住的当场记进记忆库：用户说「记住…」「以后都…」「别再…」「从现在起…」这类一直有效的要求、偏好、事实、决定、纠正时，先记下再接着做，不要只口头答应。记过的错了、变了用 replaces 改掉那条（编号在 search_memory 结果和目录里每条后的方括号里），不另记新的。用户没明说的、只管这一次的不用记（一轮结束后会自动整理）。

@@ tool.save_memory.text
要记的一句话，写成以后能照着做的样子，如「提交一律用 integemjack 身份，不加 AI 署名」

@@ tool.save_memory.kind
preference（默认）/ fact / decision / correction

@@ tool.save_memory.scope
general（默认）：到哪都适用；project：明显只是这个项目的约定、路径、构建方法

@@ tool.save_memory.replaces
要改掉的那条的编号（如 m1a2b）；新记不填

@@ tool.save_skill
新建或改写一个技能：写进数据目录的 Skills/<name>/SKILL.md，之后的对话用 search_skills 都能找到它。
什么时候用：用户教了你一套做某类事的做法、让你「记成技能」，或者你摸索出了一类以后还会反复做的活的做法（步骤、要注意的坑、用到的命令和脚本）。
写什么：能照着做的做法，不是这一次的具体内容；一次性的要求记进任务状态，用户的偏好不用做成技能。改已有的技能先用 use_skill 读出来，在原文上改好，整份交回。

@@ tool.save_skill.name
技能名，也是文件夹名：几个字或英文短横线连起来（如 写周报、release-notes）；不能有 / \ : * ? " < > |

@@ tool.save_skill.description
一两句：这个技能做什么、什么情况下该用它。search_skills 按它找，以后靠它判断要不要用，写清触发的场景和关键词

@@ tool.save_skill.content
SKILL.md 的正文（Markdown）：步骤、要点、例子。开头的 name / description 不用写，Friday 会加上

@@ tool.save_skill.files
可选：技能带的其他文件，{相对路径: 文本内容}，如 {"scripts/check.py": "…", "template.md": "…"}；给了的写进去（同名的覆盖），没给的已有文件不动

@@ tool.save_skill.delete
true = 删掉这个技能（只要 name）

@@ tool.delete_skill
删除一个技能（整个文件夹移到{{trash}}）。只在用户要删，或者这个技能已经过时、照着做会出错时用。

@@ tool.delete_skill.name
技能名

@// ———— 命令和子智能体（v2.61 CommandTools；docs/COMMANDS.md §5） ————

@@ tool.use_command
用一个命令（用户或项目配好的提示词，同 Claude Code 的 slash commands）：交回展开后的命令内容，照着做。系统提示的「## 命令」里列着能用的。

@@ tool.use_command.command
命令名，如 review、frontend:test（前面的 / 可写可不写）

@@ tool.use_command.arguments
参数（换进命令里的 $ARGUMENTS、$1…），没有就不填

@@ tool.save_command
新建或改写一个命令（同 Claude Code 的 slash commands 格式）：用户打「/名字 参数」时把正文当提示词发给你。用户让你「做成命令」「以后打 /xx 就…」，或者一类会反复交代的活值得做成一句话就能触发时用。已有的原地改（插件带的不能改）；改已有的先用 read_file 读出原文。

@@ tool.save_command.name
命令名，也是文件名：几个字或英文短横线连起来（如 review、写周报）；要分组用「:」，如 frontend:test

@@ tool.save_command.description
一句话：这个命令做什么（输入框的菜单里显示它，你判断要不要用 use_command 调也看它）

@@ tool.save_command.content
正文（提示词，Markdown）：$ARGUMENTS 换成用户打的全部参数，$1、$2… 换成第几个；!`命令` 会在用的时候先运行、换成输出（如 !`git status`）。开头的 YAML 头不用写，Friday 按参数加上

@@ tool.save_command.argument_hint
可选：参数提示，菜单里名字后面显示，如「<文件> [说明]」

@@ tool.save_command.model
可选：用这个命令的那一轮换成哪个模型：模型名 / id，或 haiku（快）、sonnet（均衡）、opus（强）；不填跟随这段对话

@@ tool.save_command.allowed_tools
可选：用户用这个命令的那一轮免确认的工具，逗号分开，如「Bash(git status:*), Bash(git add:*), Read」（只在用户明确要这样时写）

@@ tool.save_command.disable_model_invocation
可选：true = 只给用户用，你不能用 use_command 自己调

@@ tool.save_command.scope
可选：user（默认，我的：这台机器上都能用）或 project（只在这个项目里：写进工作目录的 .friday/commands 或 .claude/commands）

@@ tool.delete_command
删除一个命令（文件移到{{trash}}）。只在用户要删，或者它已经过时、照着做会出错时用。

@@ tool.delete_command.name
命令名

@@ tool.save_subagent
新建或改写一个现成的子智能体（同 Claude Code 的 subagents 格式）：以后派活时用 subagent_type 点它的名字，角色、能用的工具、档位就照它写的来。用户让你「做一个专门做 xx 的子智能体」，或者同一种角色反复现场写时用。已有的原地改（插件带的不能改）。

@@ tool.save_subagent.name
名字，也是文件名：英文短横线连起来或几个字（如 code-reviewer）

@@ tool.save_subagent.description
一两句：什么时候派它（主智能体按它挑）

@@ tool.save_subagent.content
角色说明（它的系统提示词）：它是谁、怎么做、交回什么。开头的 YAML 头不用写

@@ tool.save_subagent.tools
可选：它能用的工具，逗号分开（Friday 的名字或 Claude Code 的叫法：read_file、Read、Grep、Bash…）；不填和主智能体一样

@@ tool.save_subagent.model
可选：档位 haiku（快）、sonnet（均衡）、opus（强）；不填跟随

@@ tool.save_subagent.scope
可选：user（默认，我的）或 project（只在这个项目里）

@@ tool.delete_subagent
删除一个现成的子智能体（文件移到{{trash}}）。只在用户要删，或者它已经过时时用。

@@ tool.delete_subagent.name
名字

@// ———— 能力（CapabilityRegistry；provider 参数见 tool-part.provider） ————

@@ tool.analyze_image
看一张图片并回答问题：描述内容、读取截图里的文字、识别物体等。图片可以是本地路径（用户附件的路径在消息末尾）或 http(s) 地址。

@@ tool.analyze_image.image
图片的本地路径或 URL

@@ tool.analyze_image.question
想知道什么，如「这张截图里的报错是什么」；要完整描述就写「详细描述这张图」

@@ tool.analyze_video
看一段视频并回答问题：概括内容、找出某个片段发生了什么。视频是本地路径或 URL；会按时间均匀抽帧分析。

@@ tool.analyze_video.question
想知道什么

@@ tool.analyze_video.video
视频的本地路径或 URL

@@ tool.generate_image
按文字描述生成图片。生成的图片会直接展示给用户并返回文件路径。提示词写具体：主体、风格、构图、光线。

@@ tool.generate_image.count
张数，默认 1，最多 4

@@ tool.generate_image.negative_prompt
不希望出现的内容

@@ tool.generate_image.prompt
画面描述（中英文皆可）

@@ tool.generate_image.reference_image
参考图路径（图生图 / 编辑）；不填则纯文生图

@@ tool.generate_image.size
尺寸，如 1024x1024、1280x720，或比例 16:9、9:16、1:1；不填用默认

@@ tool.generate_video
按文字（可附一张参考图）生成一段短视频。这是异步任务：调用后立刻返回任务 ID，通常要等 1–5 分钟，之后用 check_job 查询并取回视频。

@@ tool.generate_video.duration
时长（秒），默认 5

@@ tool.generate_video.image
首帧参考图的路径或 URL（图生视频）；不填则文生视频

@@ tool.generate_video.prompt
画面与动作描述

@@ tool.generate_video.size
分辨率，如 1280x720、720x1280；不填用默认

@@ tool.transcribe_audio
听一段录音（语音消息、会议录音、视频里的声音）并转成文字。录音是本地路径或 http(s) 地址，常见格式（m4a、mp3、wav、ogg、aac）都行。

@@ tool.transcribe_audio.audio
录音的本地路径或 URL

@@ tool.transcribe_audio.language
语言：zh、en、ja 等；不填自动判断

@@ tool.speak
@// speakResult：platform.speak-result.*（Qt 返回文件路径，Swift 自动播放）
把一段话说出来：合成语音，{{speakResult}}。用户让你「说」「念」「读出来」「用语音回答」时用它。text 写成要说的原话，不要带 Markdown 符号、代码和表格。

@@ tool.speak.language
语言：Chinese、English、Japanese、Korean 等；不填自动判断

@@ tool.speak.reference_audio
克隆音色用的参考录音路径（wav / m4a / mp3），当前模型支持克隆时才有用；一般不填

@@ tool.speak.reference_text
参考录音里说的原话，配合 reference_audio 用

@@ tool.speak.style
语气或情绪，一句话描述，如「开心，语速稍快」「低沉严肃」「温柔地安慰」；不填为自然语气（Qwen3-TTS 支持，其他模型多数不支持）

@@ tool.speak.text
要说的话，原样朗读（中英文等皆可）

@@ tool.speak.voice
音色，名字取决于提供方（百炼 Qwen3-TTS：Cherry、Ethan、Dylan…；豆包：zh_female_… 这样的音色 ID；OpenAI：alloy、nova…）；不填用设置里的默认音色

@@ tool.check_job
查询 generate_video 等异步任务的进度。完成后会把结果文件下载下来展示给用户并返回路径；未完成就告诉用户稍后再查（间隔至少 30 秒，不要连续轮询）。

@@ tool.check_job.job_id
任务 ID

@// ———— 助手（AssistantTools） ————

@@ tool.list_machines
列出能管的机器：在线不在线、默认目录。

@@ tool.list_sessions
列出会话（比状态板全：空闲的也逐条列）。可以按机器、状态、关键词（标题、目录）筛。

@@ tool.list_sessions.machine
机器名；不填是全部

@@ tool.list_sessions.query
标题或目录里的字

@@ tool.list_sessions.status
只要这种状态的

@@ tool.read_session
看一个会话最近几条说了什么（压缩过的，只用于这次回答）。

@@ tool.read_session.count
几条，默认 4，最多 12

@@ tool.read_session.session
会话：短号（#3f2a）、机器 + 短号（mac-mini #3f2a），或完整 id

@@ tool.create_session
在某台机器的某个目录开一个新会话，可以直接带上第一句话（派活）。立即返回会话的短号，结果以后作为回执回来。

@@ tool.create_session.attachments
一起交给它的附件：用户消息「附件」里列的路径，原样带上

@@ tool.create_session.directory
工作目录；不填用那台机器的默认目录

@@ tool.create_session.engine
模型名或 id（按名字找）；不填跟新对话一样

@@ tool.create_session.machine
机器名；不填是本机

@@ tool.create_session.permission_mode
权限模式；不填用默认

@@ tool.create_session.prompt
交给它的第一句话：把用户要做的事说完整。用户的原话用「」原样引出来；你的理解、拆解、建议写明是建议，不能写成用户已经定了

@@ tool.create_session.title
会话标题，十个字以内

@@ tool.send_to_session
给一个会话发话（替用户转达）。它在忙时默认排到下一轮（这一轮做完才看到，可能要很久）；用户在纠正、追问、叫停它正在做的事，或说「马上告诉它」时用 interject 插进当前这一轮；要它先停下用 manage_session stop。立即返回。

@@ tool.send_to_session.attachments
一起交给它的附件：用户消息「附件」里列的路径，原样带上

@@ tool.send_to_session.interject
它在忙时插进当前这一轮（默认否）

@@ tool.send_to_session.session
会话：短号（#3f2a）、机器 + 短号（mac-mini #3f2a），或完整 id

@@ tool.send_to_session.text
要转达的话，说完整。用户的原话用「」原样引出来；你的理解、拆解、建议写明是建议，不能写成用户已经定了

@@ tool.manage_session
管理一个会话：stop 停下、retry 重试出错的、rename 改名（value 是新标题）、pin / unpin 置顶、watch / unwatch 有结果马上告诉用户、set_engine 换模型（value 是模型名或 id，按名字找；对不上会列出可选的）、set_permission 改权限（value：auto / plan / full）、delete 删除（要用户确认：第一次调用拿到确认码，问过用户、用户同意后再带 confirm_token 调用）。

@@ tool.manage_session.action
做什么

@@ tool.manage_session.confirm_token
用户同意后带上的确认码

@@ tool.manage_session.session
会话：短号（#3f2a）、机器 + 短号（mac-mini #3f2a），或完整 id

@@ tool.manage_session.value
rename / set_engine / set_permission 的值

@@ tool.resolve_approval
批准或拒绝某个会话里等确认的工具调用。只有用户这次明确说了批准 / 拒绝才能调用。

@@ tool.resolve_approval.approve
批准为真，拒绝为假

@@ tool.resolve_approval.event_id
有几个在等时指定哪一个；不填是第一个

@@ tool.resolve_approval.session
会话：短号（#3f2a）、机器 + 短号（mac-mini #3f2a），或完整 id

@@ tool.post_detail
把长内容（列表、细节、链接）作为「给看的」发给用户，语音时不念。正文只留一两句结论。去处同回复：用户在聊天工具里说的跟着回复发回那个聊天；你主动说的跟着消息推到开着通知的聊天工具；用户在桌面端说的只显示在桌面端这条回复下面的「给看的内容」里（折叠着，点开看），要发到手机 / 聊天工具（SChat 等）上带 to_chat。结果里写明发到了哪，照它说，别说成发到了别处。

@@ tool.post_detail.text
内容（可以用 Markdown）

@@ tool.post_detail.to_chat
也发到聊天工具里（发给绑定的主人）：用户在桌面端说话、却要你发到手机 / SChat 上时带上（默认否）

@@ tool.send_file
把这台主机上的一个文件发给用户：在聊天工具里发成图片、视频、语音或文件（按文件类型），桌面端显示在回复下面。用户要「发给我」「把图 / 文件给我」时用；文件要在这台主机上（附件、会话在本机生成的文件）。用户在聊天工具里说的发回那个聊天；你主动说的（没人问你时，比如会话回执之后）跟着消息发到开着通知的聊天工具；用户在桌面端说的只显示在桌面端，要发到手机 / 聊天工具（SChat 等）上带 to_chat。结果里写明发到了哪、多大，照它说，别说成发到了别处。

@@ tool.send_file.path
文件的完整路径

@@ tool.send_file.to_chat
也发到聊天工具里（发给绑定的主人）：用户在桌面端说话、却要你发到手机 / SChat 上时带上（默认否）

@@ tool.send_voice
在聊天工具（SChat 等）里给用户发一条语音：把一段话用你的声音念出来，发成语音消息。用户要你「发条语音」「用语音跟我说」「念给我听」时用；发到哪同 send_file（在桌面端说的发给绑定的主人）。视频、录音、文档这类现成的文件用 send_file。

@@ tool.send_voice.text
要念的话：口语、短（几句话，太长的只念前 600 字），不要 Markdown；正文里别再写一遍

@@ tool.look_at
自己看一眼一个文件（不用开会话）：图片看内容和上面的字、视频概括、录音听写、文本文件读出来。用户发来图片、视频、语音、文件让你看看、分析时用它；要改、要跑、要深入处理的再交给会话。

@@ tool.look_at.path
文件路径：用户消息「附件」里列的，原样带上

@@ tool.look_at.question
想知道什么；不填是整体描述

@@ tool.call_user
给用户打电话（聊天工具里的语音 / 视频通话）：接通后先念 reason，然后跟用户对话。用户让你「打电话给我」「到时候打给我」时用；没让你打的，只在要紧的事（会话要用户批准、出错停了）才打，而且用户这会儿不在桌面端。打不打得通会作为一条说明回来。

@@ tool.call_user.reason
接通后先说的那句话：为什么打来，一两句、口语

@@ tool.call_user.video
视频通话（默认语音）

@@ tool.loop
建、列、停定时任务和循环任务。command 是一条「/loop …」命令，和用户在输入框里打的一样：
- /loop 内容：节奏你自己定（每次做完在回复最后写 /loop next 20m 定下次，办完了写 /loop done）
- /loop 10m 内容、/loop 每2小时 内容：固定间隔
- /loop 每天9:00 内容、/loop 工作日9:00 内容、/loop 每周一10:00 内容：定点
- /loop 明天8:00 内容、/loop 30分钟后 内容：只做一次
- /loop 列出来；/loop stop 3、/loop stop 全部 停掉（只能停智能体建的；用户自己建的只有用户能停）
用户让你「每隔一会儿看看」「每天早上…」「明天提醒我…」「盯着…直到…」时用，不要只口头答应。到点时内容作为一条消息交回给你（在会话里建的发给这段会话，在助手里建的交给助手）。你建的标成智能体建的，最多 5 个，每个一天最多跑 24 次。

@@ tool.loop.command
一条完整的「/loop …」命令，比如「/loop 每天9:00 看一下昨晚的 CI，挂了就修」

@@ tool.set_quiet_hours
设免打扰时段：每天这几个钟头（本地时间）不主动给用户打电话、改发消息，用户这次明说让你打的照打。用户说「每天 0 到 10 点别打电话给我」「晚上别打」这类话时用（不要只用 remember 记下）；start 和 end 一样是取消。

@@ tool.set_quiet_hours.end
到几点结束（0–23，不含这一点；可以比 start 小，表示跨过半夜，如 22 到 8）

@@ tool.set_quiet_hours.start
从几点开始（0–23）

@@ tool.remember
记下用户的偏好、事实或决定，以后一直记得。记过的一条错了、变了：填 replaces 改掉那条（别再记一条新的）。

@@ tool.remember.kind
偏好 / 事实 / 决定

@@ tool.remember.pinned
很重要、每次都要想着（默认否）

@@ tool.remember.replaces
改掉哪条记忆：它后面方括号里的编号（如 m1a2b）；新记一条不填

@@ tool.remember.text
要记的一句话，写完整

@@ tool.recall
翻以前的对话和记下的事：按关键词找（空格分开的词都要出现）。系统提示词里只放了常驻的和这段对话开始时挑的目录，用户提到以前的事、偏好、约定而目录里没有时，先用它查。

@@ tool.recall.query
关键词

@@ tool.forget
忘掉记下的事：记错了、用户说不对 / 让忘掉的。按编号删（记忆后面方括号里的，如 m1a2b），不知道编号时按关键词找那条。

@@ tool.forget.id
那条记忆的编号（如 m1a2b）；几条用逗号隔开

@@ tool.forget.query
不知道编号时：那条记忆里的字

@// ———— 工具说明里的可变部分（代码按设置挑一段，当参数传进去）————

@@ tool-part.provider
@// 能力工具（内置的和提供方带来的）的 provider 参数
用哪个提供方（id 见系统提示的「能力提供方」）；不填自动选：本地优先，本地不可用再用在线

@@ tool-part.engine-pool
@// 前面空一行（代码拼）。lines：按档分组的可用大脑，每档一行「- 强：id（标题 · 模型，计费…）、…」
可用的大脑（engine 只能填这些 id）：
{{lines}}
engine、work 都不填时跟随你自己的大脑（它歇着、不支持工具时换同一档里能用的）。用 work 说是什么活时按它选档：look（只读的查看、搜索、盘点、总结）用快的，change（改东西、跑命令、装软件）用均衡的，hard（设计、疑难排查、审查这类要深想的）用强的；同一档里先用你自己的大脑，再按套餐内、按量、免费的顺序。额度用完、余额不足的会自动换掉。thinking 不填时按 work 定（look 低、change 中、hard 高，没填 work 按中），不会超过这段对话设的档位。

@@ tool-part.work.choose
没填 engine 时按它选大脑的档位，也按它定深度思考。拿不准就不填：跟随你自己的大脑

@@ tool-part.work.fixed
按它定深度思考，不换模型。拿不准就不填

@@ tool-part.item-work.choose
按它选档位和深度思考，不填跟随你自己的大脑

@@ tool-part.item-work.fixed
按它定深度思考，不换模型

@// ———— 平台参数的取值（Prompts 按平台挑，自动带上）————

@@ platform.trash.mac
@// Swift 版的 trash；Qt 版用 Platform::trashName()（Windows / Linux 是「回收站」）
废纸篓

@@ platform.shell-hint.powershell
用 PowerShell 的写法（Get-ChildItem、Get-Content -Tail、Select-String、Remove-Item -Recurse 等），不要用 rm -rf、grep、head、tail 这类 Unix 命令（这台电脑上不一定有）。

@@ platform.speak-result.file
返回音频文件的路径（会显示在对话里，可以播放）

@@ platform.speak-result.autoplay
自动播放给用户，并显示在对话里可以重播

@@ platform.tool-code-cwd
@// 只有 Qt 版：接在 create_tool 的 code 说明后面
。脚本的当前目录是工具自己的工作文件夹，参数里的相对路径要按环境变量 FRIDAY_CWD（对话的工作目录）解析

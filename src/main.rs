//! EDITH：下一代的大脑（docs/EDITH.md）。给它用户的话，它自己调模型、用工具把事做完；
//! 能力（四肢）讲能力协议（docs/CAPABILITY-PROTOCOL.md），双向实时：没有能力也能干活，接上了更强。
//! 大脑只有一个接口——本机套接字，所有能力都从它连进来、都是一级能力；共用的工具在 integemjack/edith.tools。
//! 按目录工作（`edith .`）：数据在 <目录>/.edith；目录不加锁——这个目录已经有大脑在干活时，新起的大脑自动开一个
//! git worktree 去那里干（worktree.rs，`--here` 不开）；想接到一个正在跑的大脑上用 `edith attach`。

mod attach;
mod brain;
mod capabilities;
mod downloads;
mod home;
mod http;
mod instance;
mod mcp;
mod mind;
mod permissions;
mod protocol;
mod sessions;
mod terminal;
mod tools;
mod web;
mod worktree;

use std::io::{BufRead, IsTerminal, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use mind::{Origin, Urgency};
use sessions::Session;
use terminal::Printer;

const HELP: &str = "EDITH——能自己把事做完的大脑。按目录工作：工作目录是往上找到的第一个有 .edith/ 的目录（到 git 仓库根为止；
都没有就是当前目录）。会话、日志存在 ~/.edith/projects/<目录> 下，项目里的 .edith/ 只放配置。同一个目录可以同时开几个大脑，各写各的会话。

用法：
  edith \"要做的事\"                 在这个目录干一件事（新开一个会话）
  edith [目录]                     一句一句聊（新开一个会话；干活时再打字算补充；空行或 Ctrl+D 结束）
  edith -c [\"…\"]                   接着这个目录最近的一个会话
  edith -r <会话 id> [\"…\"]         接着某个会话（id 写开头几位就行）
  edith run [目录]                 一直开着，只听能力的（新开一个会话；-c 接最近的；Ctrl+C 退出）
  edith attach [目录] [\"…\"]        接到这个目录正在跑的大脑上（一句一句聊，或干一件事就退）
  edith sessions                   列出这个目录的会话
  edith instances                  列出这台机器上在跑的大脑
  edith capabilities               列出能力（全局 → 项目 → 本地，同名的后面盖前面）
  edith capabilities add <名字> <命令> [参数…]   接一个能力（记进本地；--project 记进项目、--global 记进全局）
  edith capabilities remove <名字>                去掉（同上选哪一层）
  edith mcp                        列出 MCP 服务（.mcp.json、~/.edith/mcp.json）和它们的工具
  edith mcp add <名字> <命令> [参数…] | add-url <名字> <地址>   加一个 MCP 服务（记进本地；--project、--global 同上）
  edith mcp remove <名字>          去掉
  edith tools                      列出 edith.tools 里的工具（装了哪个版本、有没有新的）
  edith tools install <工具>       下载、装上（edith-websocket、edith-connect……；能力要用时也会自己下）
  edith tools remove <工具>        去掉

选项：
  -C, --cwd <目录>    工作目录（默认当前目录）
  -w, --worktree      开一个 git worktree 去那里干（.edith/worktrees/，分支 edith/<编号>）；退出时没改东西就自动清掉
  -q, --quiet         只打最终回答，不打过程
  -v, --verbose       也打思考、每段请求的用量、能力的日志

模型：~/.edith/config.json 的 {\"baseURL\", \"apiKey\", \"model\"}，或环境变量 EDITH_BASE_URL / EDITH_API_KEY / EDITH_MODEL
能力协议：docs/CAPABILITY-PROTOCOL.md";

/// Ctrl+C：干活时第一次让这一轮停下，第二次（或闲着时）退出
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

#[cfg(unix)]
fn install_interrupt() {
    extern "C" fn on_interrupt(_: libc::c_int) {
        if INTERRUPTED.swap(true, Ordering::SeqCst) {
            unsafe { libc::_exit(130) };
        }
    }
    unsafe {
        libc::signal(libc::SIGINT, on_interrupt as *const () as libc::sighandler_t);
    }
}

#[cfg(not(unix))]
fn install_interrupt() {}

/// 收拾好退出：关能力、删登记
fn shutdown(code: i32) -> ! {
    capabilities::stop_all();
    instance::release();
    finish_worktree();
    std::process::exit(code);
}

/// 收 Ctrl+C：在干活就停下这一轮，闲着（或 exit_anyway）就收拾好退出
fn watch_interrupt(exit_anyway: bool) {
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(std::time::Duration::from_millis(100));
            if !INTERRUPTED.load(Ordering::SeqCst) {
                continue;
            }
            let focus = mind::focus();
            if !exit_anyway && mind::stop(&focus) {
                mind::wait_idle(&focus);
                INTERRUPTED.store(false, Ordering::SeqCst);
                continue;
            }
            shutdown(130);
        }
    });
}

struct Options {
    /// 当前目录（新会话的工作目录）
    cwd: PathBuf,
    /// 明说了的目录（edith <目录>、edith run <目录>、-C）：就用它当工作目录，不往上找
    explicit: bool,
    quiet: bool,
    verbose: bool,
    /// -r：接着哪个会话
    resume: Option<String>,
    /// -c：接着最近的会话
    continue_latest: bool,
    /// 能力、MCP 配置改哪一层
    layer: capabilities::Layer,
    worktree: bool,
    words: Vec<String>,
}

/// --worktree 开的（退出时没改东西就清掉）
static WORKTREE: std::sync::OnceLock<worktree::Worktree> = std::sync::OnceLock::new();

/// 退出前看一眼 worktree：没改就清掉，改了告诉用户在哪
fn finish_worktree() {
    if let Some(tree) = WORKTREE.get() {
        eprintln!("\x1b[2m（{}）\x1b[0m", worktree::finish(tree));
    }
}

fn main() {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    if matches!(arguments.first().map(String::as_str), Some("-h" | "--help" | "help")) {
        println!("{HELP}");
        return;
    }
    if matches!(arguments.first().map(String::as_str), Some("-V" | "--version")) {
        println!("edith {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    let mut options = match parse(&arguments) {
        Ok(options) => options,
        Err(message) => return exit_on(Err(message)),
    };
    let command = options.words.first().cloned().unwrap_or_default();
    // edith run <目录>、edith attach <目录> […]、edith <目录>：明说的工作目录
    let directory_argument = match command.as_str() {
        "run" | "attach" => options.words.get(1).filter(|word| std::path::Path::new(word).is_dir()).cloned(),
        _ if options.words.len() == 1 && std::path::Path::new(&command).is_dir() => Some(command.clone()),
        _ => None,
    };
    if let Some(directory) = directory_argument {
        match std::fs::canonicalize(&directory) {
            Ok(path) => {
                options.cwd = path;
                options.explicit = true;
            }
            Err(_) => return exit_on(Err(format!("目录不存在：{directory}"))),
        }
        if matches!(command.as_str(), "run" | "attach") {
            options.words.remove(1);
        } else {
            options.words.clear();
        }
    }
    let mut workspace = if options.explicit { options.cwd.clone() } else { home::find_workspace(&options.cwd) };
    // --worktree：开一个 git worktree 去那里干（干活、聊天、run 才开）
    let starts_brain = !matches!(command.as_str(), "sessions" | "instances" | "capabilities" | "caps" | "attach" | "tools" | "mcp");
    if starts_brain && options.worktree {
        match worktree::create(&workspace) {
            Ok(tree) => {
                eprintln!("\x1b[2m（在 git worktree {} 里干，分支 {}）\x1b[0m", tree.path.display(), tree.branch);
                options.cwd = worktree::map_cwd(&options.cwd, &workspace, &tree.path);
                workspace = tree.path.clone();
                let _ = WORKTREE.set(tree);
            }
            Err(reason) => return exit_on(Err(format!("开不了 worktree：{reason}"))),
        }
    }
    home::set_workspace(workspace);
    match command.as_str() {
        "sessions" if options.words.len() == 1 => list_sessions(),
        "instances" if options.words.len() == 1 => list_instances(),
        "capabilities" | "caps" => {
            brain::setup_prompts();
            let rest: Vec<String> = options.words[1..].to_vec();
            let result = capabilities_command(&rest, options.layer);
            capabilities::stop_all();
            instance::release();
            exit_on(result);
        }
        "tools" => exit_on(tools_command(&options.words[1..])),
        "mcp" => {
            let rest: Vec<String> = options.words[1..].to_vec();
            exit_on(mcp_command(&rest, options.layer));
        }
        "run" if options.words.len() == 1 => exit_on(run(options)),
        "attach" => exit_on(attach_to(options)),
        _ => exit_on(work(options)),
    }
}

fn exit_on(result: Result<(), String>) {
    if let Err(message) = result {
        eprintln!("edith：{message}");
        instance::release();
        std::process::exit(1);
    }
}

fn parse(arguments: &[String]) -> Result<Options, String> {
    let mut options = Options {
        cwd: std::env::current_dir().map_err(|error| error.to_string())?,
        explicit: false,
        quiet: false,
        verbose: false,
        resume: None,
        continue_latest: false,
        layer: capabilities::Layer::Local,
        worktree: false,
        words: Vec::new(),
    };
    let mut rest = arguments.iter();
    while let Some(argument) = rest.next() {
        // edith capabilities add <名字> <命令> 之后的都是那个命令的参数（-c 之类不是 edith 的选项）
        let literal =
            matches!(options.words.first().map(String::as_str), Some("capabilities" | "caps" | "mcp")) && options.words.len() >= 3;
        if literal {
            options.words.push(argument.clone());
            continue;
        }
        match argument.as_str() {
            "-C" | "--cwd" => {
                options.cwd = PathBuf::from(rest.next().ok_or("-C 后面要跟目录")?);
                options.explicit = true;
            }
            "-q" | "--quiet" => options.quiet = true,
            "-v" | "--verbose" => options.verbose = true,
            "--global" => options.layer = capabilities::Layer::Global,
            "--project" => options.layer = capabilities::Layer::Project,
            "--local" => options.layer = capabilities::Layer::Local,
            "-w" | "--worktree" => options.worktree = true,
            "-c" | "--continue" => options.continue_latest = true,
            "-r" | "--resume" => options.resume = Some(rest.next().ok_or("-r 后面要跟会话 id")?.clone()),
            "--" => options.words.extend(rest.by_ref().cloned()),
            other if other.starts_with('-') && options.words.is_empty() => return Err(format!("不认识的选项 {other}（edith --help）")),
            other => options.words.push(other.to_string()),
        }
    }
    options.cwd = std::fs::canonicalize(&options.cwd).map_err(|_| format!("工作目录不存在：{}", options.cwd.display()))?;
    if options.verbose {
        // capabilities.rs 看它决定要不要把能力的日志也打到终端
        unsafe { std::env::set_var("EDITH_VERBOSE", "1") };
    }
    Ok(options)
}

/// 起这个目录的大脑：接能力、开本机接入
fn prepare() -> Result<(), String> {
    home::Model::load()?;
    brain::setup_prompts();
    install_interrupt();
    instance::serve()?;
    capabilities::start_all(&mut |message| eprintln!("\x1b[33m{message}\x1b[0m"));
    mcp::start_all(&mut |message| eprintln!("\x1b[33m{message}\x1b[0m"));
    Ok(())
}

/// edith run：一直开着，干活的事都由能力交来（这个目录的主会话是焦点）
fn run(options: Options) -> Result<(), String> {
    home::Model::load()?;
    // 先定好焦点会话再起能力：能力一连上就可能交话（没指明会话的交给焦点）
    let session = match options.continue_latest.then(sessions::latest).flatten() {
        Some(session) => session,
        None => {
            let mut session = Session::new(&home::workspace().to_string_lossy());
            session.save()?;
            session
        }
    };
    mind::set_focus(&session.id);
    prepare()?;
    let printer = Arc::new(Mutex::new(Printer::new(options.quiet, options.verbose)));
    mind::listen(move |envelope| printer.lock().unwrap().show(envelope.kind, &envelope.session, &envelope.data, true));
    watch_interrupt(true);
    eprintln!(
        "EDITH 在 {} 开着（会话 {}，{} 个能力）。Ctrl+C 退出。",
        home::workspace().display(),
        &session.id[..8],
        capabilities::names().len()
    );
    loop {
        std::thread::park();
    }
}

/// 要做的事：参数 + 管道送进来的
fn task(options: &Options) -> Result<Option<String>, String> {
    let mut task = options.words.join(" ");
    let piped = !std::io::stdin().is_terminal();
    if piped {
        let mut input = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut input).map_err(|error| error.to_string())?;
        if !input.trim().is_empty() {
            task = if task.is_empty() { input } else { format!("{task}\n\n{input}") };
        }
        if task.trim().is_empty() {
            return Err("没有要做的事".into());
        }
    }
    Ok(if task.trim().is_empty() { None } else { Some(task) })
}

/// edith attach：接到这个目录正在跑的大脑上（几个的话接最新起的那个）
fn attach_to(mut options: Options) -> Result<(), String> {
    options.words.remove(0);
    let task = task(&options)?;
    let running = instance::running_in(&home::workspace());
    let entry = running.first().ok_or_else(|| format!("{} 没有在跑的大脑（edith instances 看有哪些）", home::workspace().display()))?;
    install_interrupt();
    attach::run(entry, task, options.quiet, options.verbose, &INTERRUPTED)
}

fn work(options: Options) -> Result<(), String> {
    let task = task(&options)?;
    let model = home::Model::load()?;
    let mut session = match (&options.resume, options.continue_latest) {
        (Some(id), _) => Session::find(id).ok_or_else(|| format!("没有会话 {id}（edith sessions 看有哪些）"))?,
        (None, true) => sessions::latest().ok_or("这个目录还没有会话")?,
        (None, false) => Session::new(&options.cwd.to_string_lossy()),
    };
    if !std::path::Path::new(&session.cwd).is_dir() {
        session.cwd = options.cwd.to_string_lossy().into_owned();
    }
    session.save()?;
    // 先定好焦点会话再起能力（同 run）
    mind::set_focus(&session.id);
    prepare()?;
    let printer = Arc::new(Mutex::new(Printer::new(options.quiet, options.verbose)));
    let failure: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let failed = failure.clone();
    let session_id = session.id.clone();
    mind::listen(move |envelope| {
        if envelope.session != session_id {
            return;
        }
        if envelope.kind == "done" {
            *failed.lock().unwrap() = envelope.data["error"].as_str().map(str::to_string);
        }
        printer.lock().unwrap().show(envelope.kind, &envelope.session, &envelope.data, false);
    });
    watch_interrupt(false);
    if let Some(task) = task {
        mind::submit(&session.id, &task, Origin::terminal(), Urgency::Queue, None)?;
        // 干完（中途能力补充、排着的也干完）再退出
        std::thread::sleep(std::time::Duration::from_millis(50));
        mind::wait_idle(&session.id);
        capabilities::stop_all();
        instance::release();
        finish_worktree();
        if !options.quiet {
            eprintln!("\x1b[2m（会话 {}，接着做：edith -r {} \"…\"）\x1b[0m", &session.id[..8], &session.id[..8]);
        }
        return failure.lock().unwrap().take().map_or(Ok(()), Err);
    }
    // 一句一句聊：闲着时打的开始一轮，干活时打的补充进这一轮
    eprintln!("EDITH（{}）在 {}。说要做的事；干活时再打字算补充；空行或 Ctrl+D 结束。", model.model, home::workspace().display());
    let stdin = std::io::stdin();
    loop {
        if mind::working(&session.id).is_none() {
            eprint!("\x1b[1m> \x1b[0m");
            let _ = std::io::stderr().flush();
        }
        let mut line = String::new();
        if stdin.lock().read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
            break;
        }
        let urgency = if mind::working(&session.id).is_some() { Urgency::Interject } else { Urgency::Queue };
        match mind::submit(&session.id, line.trim(), Origin::terminal(), urgency, None) {
            Ok((mind::Disposition::Interjected, _)) => eprintln!("\x1b[2m（补充进了正在做的这一轮）\x1b[0m"),
            Ok(_) => {}
            Err(message) => eprintln!("\x1b[31m{message}\x1b[0m"),
        }
        // 刚开始的这一轮先打一会儿，再回来收下一句
        std::thread::sleep(std::time::Duration::from_millis(50));
        while mind::working(&session.id).is_some() && !stdin_ready() {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
    mind::wait_idle(&session.id);
    capabilities::stop_all();
    instance::release();
    finish_worktree();
    eprintln!("\x1b[2m（会话 {}）\x1b[0m", &session.id[..8]);
    Ok(())
}

/// 终端里已经有打好的一行在等着读（干活时用户打字补充）
#[cfg(unix)]
fn stdin_ready() -> bool {
    let mut poll = libc::pollfd { fd: 0, events: libc::POLLIN, revents: 0 };
    unsafe { libc::poll(&mut poll, 1, 0) > 0 }
}

#[cfg(not(unix))]
fn stdin_ready() -> bool {
    true
}

fn list_sessions() {
    let all = sessions::list();
    if all.is_empty() {
        println!("{} 还没有会话", home::workspace().display());
    }
    for session in all {
        println!("{}  {}  {}  {}", &session.id[..8], session.updated.get(..16).unwrap_or(&session.updated), session.title, session.cwd);
    }
}

fn list_instances() {
    let running = instance::list();
    if running.is_empty() {
        println!("没有在跑的大脑");
    }
    for entry in running {
        let names: Vec<&str> = entry["capabilities"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
        println!(
            "{}  进程 {}  {}",
            entry["workspace"].as_str().unwrap_or(""),
            entry["pid"],
            if names.is_empty() { "（没接能力）".into() } else { names.join("、") }
        );
        for endpoint in entry["endpoints"].as_array().into_iter().flatten() {
            println!("  {}：{}", endpoint["capability"].as_str().unwrap_or(""), endpoint["url"].as_str().unwrap_or(""));
        }
    }
}

fn capabilities_command(arguments: &[String], layer: capabilities::Layer) -> Result<(), String> {
    // 这个目录有大脑在跑：只看配置、不另起一份（独占的设备会撞）；改了配置要它重启才生效
    let running = instance::running_in(&home::workspace()).into_iter().next();
    let added = |value: Value| {
        let name = value["name"].as_str().unwrap_or("").to_string();
        let actions = value["actions"].as_array().map_or(0, Vec::len);
        println!("接上了「{name}」：{actions} 个动作");
    };
    let layer_title = layer.title();
    match arguments.first().map(String::as_str) {
        None | Some("list") => {
            if running.is_none() {
                // 能力的程序起来后要连进套接字：先开
                instance::serve()?;
                capabilities::start_all(&mut |message| eprintln!("{message}"));
            }
            let configs = capabilities::configs();
            if configs.is_empty() {
                println!("还没有能力（edith capabilities add <名字> <命令> [参数…]）");
            }
            let live: Vec<String> = match &running {
                Some(entry) => {
                    entry["capabilities"].as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_string).collect()
                }
                None => capabilities::names(),
            };
            for config in configs {
                let name = capabilities::clean_name(config["name"].as_str().unwrap_or(""));
                let scope = match config["scope"].as_str() {
                    Some("global") => "全局",
                    Some("project") => "项目",
                    _ => "本地",
                };
                if config["disabled"].as_bool() == Some(true) {
                    println!("{name}（{scope}）：关掉了");
                    continue;
                }
                match capabilities::describe(&name) {
                    Some(info) => {
                        let list = |key: &str| {
                            info[key]
                                .as_array()
                                .map(|items| {
                                    items.iter().filter_map(|item| item.as_str().or(item["kind"].as_str())).collect::<Vec<_>>().join("、")
                                })
                                .unwrap_or_default()
                        };
                        let title = info["info"]["title"].as_str().map(|title| format!(" {title}")).unwrap_or_default();
                        let or_none = |text: String| if text.is_empty() { "（没有）".to_string() } else { text };
                        println!("{name}（{scope}）{title}");
                        println!("  动作：{}", or_none(list("actions")));
                        println!("  感知：{}", or_none(list("senses")));
                        println!("  订的事件：{}", or_none(list("receives")));
                    }
                    None if live.contains(&name) => println!("{name}（{scope}）：接着（在正在跑的大脑上）"),
                    None => println!("{name}（{scope}）：没接上（日志在 {}）", capabilities::log_file(&name).display()),
                }
            }
            Ok(())
        }
        Some("add") if arguments.len() >= 3 => {
            let mut config = json!({ "name": arguments[1], "command": arguments[2], "args": arguments[3..] });
            if running.is_some() {
                // 不在这里起它（正在跑的大脑那边会起）：只记配置
                config["name"] = json!(capabilities::clean_name(&arguments[1]));
                capabilities::save(config, layer)?;
                println!("记进了{layer_title}的配置；这个目录的大脑在跑，重启它后生效");
                return Ok(());
            }
            instance::serve()?;
            added(capabilities::add(config, layer)?);
            Ok(())
        }
        Some("remove") if arguments.len() == 2 => {
            if capabilities::remove(&arguments[1], layer) {
                if running.is_some() {
                    println!("从{layer_title}的配置去掉了；这个目录的大脑在跑，重启它后生效");
                }
                Ok(())
            } else {
                Err(format!("{layer_title}的配置里没有能力「{}」", arguments[1]))
            }
        }
        _ => Err("用法：edith capabilities [add <名字> <命令> [参数…] | remove <名字>] [--project | --global]".into()),
    }
}

fn tools_command(arguments: &[String]) -> Result<(), String> {
    match arguments.first().map(String::as_str) {
        None | Some("list") => {
            println!("edith.tools（这个平台：{}）", downloads::platform());
            for (name, current, available, description) in downloads::list()? {
                let state = match (current.is_empty(), available.is_empty() || current == available) {
                    (true, _) => format!("没装，可装 {available}"),
                    (false, true) => format!("装了 {current}"),
                    (false, false) => format!("装了 {current}，有新的 {available}（edith tools install {name}）"),
                };
                println!("{name}  {state}");
                if !description.is_empty() {
                    println!("  {description}");
                }
            }
            Ok(())
        }
        Some("install" | "update") if arguments.len() >= 2 => {
            for name in &arguments[1..] {
                let version = downloads::install(name)?;
                println!("装好了 {name} {version}");
            }
            Ok(())
        }
        Some("remove") if arguments.len() == 2 => {
            if downloads::remove(&arguments[1]) {
                Ok(())
            } else {
                Err(format!("没装 {}", arguments[1]))
            }
        }
        _ => Err("用法：edith tools [install <工具>… | remove <工具>]".into()),
    }
}

fn mcp_command(arguments: &[String], layer: capabilities::Layer) -> Result<(), String> {
    let running = instance::running_in(&home::workspace()).into_iter().next();
    let restart_hint = |done: &str| {
        if running.is_some() {
            println!("{done}；这个目录的大脑在跑，重启它后生效");
        } else {
            println!("{done}");
        }
    };
    let layer_title = layer.title();
    match arguments.first().map(String::as_str) {
        None | Some("list") => {
            let configs = mcp::configs();
            if configs.is_empty() {
                println!("还没有 MCP 服务（edith mcp add <名字> <命令> [参数…]，或写 .mcp.json）");
            }
            for (name, config) in configs {
                let scope = config["scope"].as_str().unwrap_or("");
                let target = config["command"].as_str().or(config["url"].as_str()).unwrap_or("");
                if config["disabled"].as_bool() == Some(true) {
                    println!("{name}（{scope}）：关掉了");
                    continue;
                }
                // 试连一下，列出工具
                match mcp::attach(&name, config.clone()) {
                    Ok(limb) => {
                        let tools: Vec<&str> = limb.tools.iter().filter_map(|tool| tool["name"].as_str()).collect();
                        println!("{name}（{scope}）{target}");
                        println!("  工具：{}", if tools.is_empty() { "（没有）".to_string() } else { tools.join("、") });
                    }
                    Err(error) => println!("{name}（{scope}）{target}：连不上（{error}）"),
                }
            }
            mcp::stop_all();
            Ok(())
        }
        Some("add") if arguments.len() >= 3 => {
            let config = json!({ "command": arguments[2], "args": arguments[3..] });
            mcp::attach(&capabilities::clean_name(&arguments[1]), config.clone()).map_err(|error| format!("连不上：{error}"))?;
            mcp::stop_all();
            mcp::save(&arguments[1], config, layer)?;
            restart_hint(&format!("记进了{layer_title}的 MCP 配置"));
            Ok(())
        }
        Some("add-url") if arguments.len() >= 3 => {
            let mut headers = serde_json::Map::new();
            let mut rest = arguments[3..].iter();
            while let Some(argument) = rest.next() {
                match argument.as_str() {
                    "-H" | "--header" => {
                        let header = rest.next().ok_or("--header 后面要跟「名字: 值」")?;
                        let (key, value) = header.split_once(':').ok_or("--header 要写成「名字: 值」")?;
                        headers.insert(key.trim().to_string(), json!(value.trim()));
                    }
                    other => return Err(format!("不认识的参数 {other}")),
                }
            }
            let mut config = json!({ "type": "http", "url": arguments[2] });
            if !headers.is_empty() {
                config["headers"] = Value::Object(headers);
            }
            mcp::attach(&capabilities::clean_name(&arguments[1]), config.clone()).map_err(|error| format!("连不上：{error}"))?;
            mcp::stop_all();
            mcp::save(&arguments[1], config, layer)?;
            restart_hint(&format!("记进了{layer_title}的 MCP 配置"));
            Ok(())
        }
        Some("remove") if arguments.len() == 2 => {
            if mcp::remove(&arguments[1], layer) {
                restart_hint(&format!("从{layer_title}的 MCP 配置去掉了"));
                Ok(())
            } else {
                Err(format!("{layer_title}的 MCP 配置里没有「{}」", arguments[1]))
            }
        }
        _ => Err("用法：edith mcp [add <名字> <命令> [参数…] | add-url <名字> <地址> [--header \"名字: 值\"] | remove <名字>] [--project | --global]".into()),
    }
}

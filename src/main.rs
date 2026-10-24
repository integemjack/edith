//! EDITH：下一代的大脑（docs/EDITH.md）。给它用户的话，它自己调模型、用工具把事做完；
//! 能力（四肢）是本机起的进程，经 stdio 讲能力协议（docs/CAPABILITY-PROTOCOL.md），双向实时：没有能力也能干活，接上了更强。

mod brain;
mod capabilities;
mod home;
mod http;
mod mcp;
mod mind;
mod protocol;
mod sessions;
mod tools;

use std::io::{BufRead, IsTerminal, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use mind::{Envelope, Origin, Urgency};
use sessions::Session;

const HELP: &str = "EDITH——能自己把事做完的大脑

用法：
  edith \"要做的事\"                 在当前目录干一件事
  edith                            一句一句聊（同一个会话；干活时再打字算补充；空行或 Ctrl+D 结束）
  edith -c <会话 id> \"…\"           接着某个会话（id 写开头几位就行）
  edith run                        一直开着，只听能力的（主会话；Ctrl+C 退出）
  edith sessions                   列出会话
  edith capabilities               列出能力（会先接上它们）
  edith capabilities add <名字> <命令> [参数…]       接一个讲能力协议的程序（记进 ~/.edith/capabilities.json）
  edith capabilities add-mcp <名字> <命令> [参数…]   接一个本机的 MCP 服务
  edith capabilities add-mcp-url <名字> <地址>        接一个 HTTP 的 MCP 服务
  edith capabilities remove <名字>                    去掉

选项：
  -C, --cwd <目录>    工作目录（默认当前目录）
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
            capabilities::stop_all();
            std::process::exit(130);
        }
    });
}

struct Options {
    cwd: PathBuf,
    quiet: bool,
    verbose: bool,
    session: Option<String>,
    words: Vec<String>,
}

fn main() {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    match arguments.first().map(String::as_str) {
        Some("-h" | "--help" | "help") => println!("{HELP}"),
        Some("-V" | "--version") => println!("edith {}", env!("CARGO_PKG_VERSION")),
        Some("sessions") => list_sessions(),
        Some("capabilities" | "caps") => {
            brain::setup_prompts();
            let result = capabilities_command(&arguments[1..]);
            capabilities::stop_all();
            exit_on(result);
        }
        _ => match parse(&arguments) {
            Ok(options) if options.words.first().map(String::as_str) == Some("run") && options.words.len() == 1 => exit_on(run(options)),
            Ok(options) => exit_on(work(options)),
            Err(message) => exit_on(Err(message)),
        },
    }
}

fn exit_on(result: Result<(), String>) {
    if let Err(message) = result {
        eprintln!("edith：{message}");
        std::process::exit(1);
    }
}

fn parse(arguments: &[String]) -> Result<Options, String> {
    let mut options = Options {
        cwd: std::env::current_dir().map_err(|error| error.to_string())?,
        quiet: false,
        verbose: false,
        session: None,
        words: Vec::new(),
    };
    let mut rest = arguments.iter();
    while let Some(argument) = rest.next() {
        match argument.as_str() {
            "-C" | "--cwd" => options.cwd = PathBuf::from(rest.next().ok_or("-C 后面要跟目录")?),
            "-q" | "--quiet" => options.quiet = true,
            "-v" | "--verbose" => options.verbose = true,
            "-c" | "--continue" => options.session = Some(rest.next().ok_or("-c 后面要跟会话 id")?.clone()),
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

fn prepare() -> Result<(), String> {
    home::Model::load()?;
    brain::setup_prompts();
    capabilities::start_all(&mut |message| eprintln!("\x1b[33m{message}\x1b[0m"));
    install_interrupt();
    Ok(())
}

/// edith run：一直开着，干活的事都由能力交来（主会话是焦点）
fn run(options: Options) -> Result<(), String> {
    prepare()?;
    let session = sessions::main_session(&options.cwd.to_string_lossy())?;
    mind::set_focus(&session.id);
    let printer = Arc::new(Mutex::new(Printer::new(&options)));
    let shown = printer.clone();
    mind::listen(move |envelope| shown.lock().unwrap().show(envelope, true));
    watch_interrupt(true);
    eprintln!("EDITH 开着（主会话 {}，{} 个能力）。Ctrl+C 退出。", &session.id[..8], capability_count());
    loop {
        std::thread::park();
    }
}

fn capability_count() -> usize {
    capabilities::configs().iter().filter(|config| config.get("disabled").and_then(Value::as_bool) != Some(true)).count()
}

fn work(options: Options) -> Result<(), String> {
    let model = home::Model::load()?;
    prepare()?;
    let mut session = match &options.session {
        Some(id) => Session::find(id).ok_or_else(|| format!("没有会话 {id}（edith sessions 看有哪些）"))?,
        None => Session::new(&options.cwd.to_string_lossy()),
    };
    if options.session.is_none() || !std::path::Path::new(&session.cwd).is_dir() {
        session.cwd = options.cwd.to_string_lossy().into_owned();
    }
    session.save()?;
    mind::set_focus(&session.id);
    let printer = Arc::new(Mutex::new(Printer::new(&options)));
    let shown = printer.clone();
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
        shown.lock().unwrap().show(envelope, false);
    });
    let mut task = options.words.join(" ");
    // 管道送进来的整段当成要做的事（前面有参数的接在后面）
    let piped = !std::io::stdin().is_terminal();
    if piped {
        let mut input = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut input).map_err(|error| error.to_string())?;
        if !input.trim().is_empty() {
            task = if task.is_empty() { input } else { format!("{task}\n\n{input}") };
        }
    }
    if !task.trim().is_empty() || piped {
        if task.trim().is_empty() {
            return Err("没有要做的事".into());
        }
        watch_interrupt(false);
        mind::submit(&session.id, &task, Origin::terminal(), Urgency::Queue, None)?;
        // 干完（中途能力补充、排着的也干完）再退出
        std::thread::sleep(std::time::Duration::from_millis(50));
        mind::wait_idle(&session.id);
        capabilities::stop_all();
        if !options.quiet {
            eprintln!("\x1b[2m（会话 {}，接着做：edith -c {} \"…\"）\x1b[0m", &session.id[..8], &session.id[..8]);
        }
        return failure.lock().unwrap().take().map_or(Ok(()), Err);
    }
    // 一句一句聊：闲着时打的开始一轮，干活时打的补充进这一轮
    watch_interrupt(false);
    eprintln!("EDITH（{}）在 {}。说要做的事；干活时再打字算补充；空行或 Ctrl+D 结束。", model.model, session.cwd);
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

/// 终端上的样子：回答打到标准输出，过程（工具、提示、能力交来的话）打到标准错误
struct Printer {
    quiet: bool,
    verbose: bool,
    thinking: bool,
    line_start: bool,
}

impl Printer {
    fn new(options: &Options) -> Printer {
        Printer { quiet: options.quiet, verbose: options.verbose, thinking: false, line_start: true }
    }

    fn out(&mut self, text: &str) {
        print!("{text}");
        let _ = std::io::stdout().flush();
        self.line_start = text.ends_with('\n');
    }

    fn side(&mut self, text: &str) {
        if self.thinking {
            self.thinking = false;
            eprint!("\x1b[0m");
        }
        if !self.line_start {
            println!();
            self.line_start = true;
        }
        eprintln!("{text}");
    }

    /// all_sessions：edith run 打所有会话的（前面标上会话）
    fn show(&mut self, envelope: &Envelope, all_sessions: bool) {
        let data = &envelope.data;
        if self.quiet {
            if envelope.kind == "done" {
                println!("{}", data["answer"].as_str().unwrap_or("").trim());
            }
            return;
        }
        let tag = if all_sessions { format!("[{}] ", &envelope.session[..8.min(envelope.session.len())]) } else { String::new() };
        match envelope.kind {
            "turn.started" => {
                let origin = &data["origin"];
                let capability = origin["capability"].as_str().unwrap_or("");
                if !capability.is_empty() || all_sessions {
                    let from = origin["from"].as_str().filter(|from| !from.is_empty()).map(|from| format!(" {from}")).unwrap_or_default();
                    let source = if capability.is_empty() { "终端".to_string() } else { format!("能力「{capability}」") };
                    let text = data["text"].as_str().unwrap_or("").chars().take(200).collect::<String>();
                    self.side(&format!("\x1b[35m{tag}◂ {source}{from}：{text}\x1b[0m"));
                }
            }
            "text" => {
                if self.thinking {
                    self.thinking = false;
                    eprintln!("\x1b[0m");
                }
                self.out(data["text"].as_str().unwrap_or(""));
            }
            "thinking" if self.verbose => {
                if !self.thinking {
                    self.thinking = true;
                    eprint!("\x1b[2m");
                }
                eprint!("{}", data["text"].as_str().unwrap_or(""));
            }
            "tool" => {
                let brief = brief(&data["arguments"]);
                self.side(&format!("\x1b[36m{tag}▸ {}\x1b[0m {brief}", data["name"].as_str().unwrap_or("")));
            }
            "result" => {
                let text = data["text"].as_str().unwrap_or("");
                let first = text.lines().find(|line| !line.trim().is_empty()).unwrap_or("").chars().take(100).collect::<String>();
                let mark = if data["ok"].as_bool() == Some(true) { "\x1b[32m✓\x1b[0m" } else { "\x1b[31m✗\x1b[0m" };
                self.side(&format!("  {mark} \x1b[2m{first}\x1b[0m"));
            }
            "stats" if self.verbose => {
                let line = format!(
                    "  · 输入 {} token、输出 {} token",
                    data["promptTokens"].as_i64().unwrap_or(0),
                    data["generatedTokens"].as_i64().unwrap_or(0)
                );
                self.side(&format!("\x1b[2m{line}\x1b[0m"));
            }
            "notice" => self.side(&format!("\x1b[33m{tag}{}\x1b[0m", data["text"].as_str().unwrap_or(""))),
            "done" => {
                if !self.line_start {
                    println!();
                    self.line_start = true;
                }
                if data["stopped"].as_bool() == Some(true) {
                    self.side("\x1b[33m（停下了）\x1b[0m");
                }
                if let Some(error) = data["error"].as_str() {
                    self.side(&format!("\x1b[31m{error}\x1b[0m"));
                }
            }
            _ => {}
        }
    }
}

/// 工具参数的一行简述（路径、命令、关键词……）
fn brief(arguments: &Value) -> String {
    for key in ["command", "path", "pattern", "keywords", "url", "query", "text"] {
        if let Some(text) = arguments.get(key).and_then(Value::as_str) {
            let line = text.lines().next().unwrap_or("");
            let clipped: String = line.chars().take(90).collect();
            return if clipped.len() < text.len() { format!("{clipped}…") } else { clipped };
        }
    }
    let text = arguments.to_string();
    if text == "{}" { String::new() } else { text.chars().take(90).collect() }
}

fn list_sessions() {
    let all = sessions::list();
    if all.is_empty() {
        println!("还没有会话");
    }
    for session in all {
        println!("{}  {}  {}  {}", &session.id[..8], session.updated.get(..16).unwrap_or(&session.updated), session.title, session.cwd);
    }
}

fn capabilities_command(arguments: &[String]) -> Result<(), String> {
    let added = |value: Value| {
        let name = value["name"].as_str().unwrap_or("").to_string();
        let actions = value["actions"].as_array().map_or(0, Vec::len);
        println!("接上了「{name}」：{actions} 个动作");
    };
    match arguments.first().map(String::as_str) {
        None | Some("list") => {
            let reported = Arc::new(AtomicBool::new(false));
            let flag = reported.clone();
            capabilities::start_all(&mut |message| {
                flag.store(true, Ordering::Relaxed);
                eprintln!("{message}");
            });
            let configs = capabilities::configs();
            if configs.is_empty() {
                println!("还没有能力（edith capabilities add <名字> <命令> [参数…]）");
            }
            for config in configs {
                let name = capabilities::clean_name(config["name"].as_str().unwrap_or(""));
                let protocol = config["protocol"].as_str().unwrap_or("edith");
                if config["disabled"].as_bool() == Some(true) {
                    println!("{name}（{protocol}）：关掉了");
                    continue;
                }
                match capabilities::describe(&name) {
                    Some(info) => {
                        let list = |key: &str| {
                            info[key].as_array().map(|items| items.iter().filter_map(|item| item.as_str().or(item["kind"].as_str())).collect::<Vec<_>>().join("、")).unwrap_or_default()
                        };
                        let title = info["info"]["title"].as_str().map(|title| format!(" {title}")).unwrap_or_default();
                        println!("{name}（{protocol}）{title}");
                        println!("  动作：{}", if list("actions").is_empty() { "（没有）".into() } else { list("actions") });
                        if protocol != "mcp" {
                            println!("  感知：{}", if list("senses").is_empty() { "（没有）".into() } else { list("senses") });
                            println!("  订的事件：{}", if list("receives").is_empty() { "（没有）".into() } else { list("receives") });
                        }
                    }
                    None => println!("{name}（{protocol}）：没接上（日志在 ~/.edith/logs/{name}.log）"),
                }
            }
            Ok(())
        }
        Some("add") if arguments.len() >= 3 => {
            added(capabilities::add(json!({ "name": arguments[1], "command": arguments[2], "args": arguments[3..] }))?);
            Ok(())
        }
        Some("add-mcp") if arguments.len() >= 3 => {
            added(capabilities::add(json!({ "name": arguments[1], "protocol": "mcp", "command": arguments[2], "args": arguments[3..] }))?);
            Ok(())
        }
        Some("add-mcp-url") if arguments.len() == 3 => {
            added(capabilities::add(json!({ "name": arguments[1], "protocol": "mcp", "url": arguments[2] }))?);
            Ok(())
        }
        Some("remove") if arguments.len() == 2 => {
            if capabilities::remove(&arguments[1]) { Ok(()) } else { Err(format!("没有能力「{}」", arguments[1])) }
        }
        _ => Err("用法：edith capabilities [add <名字> <命令> [参数…] | add-mcp <名字> <命令> [参数…] | add-mcp-url <名字> <地址> | remove <名字>]".into()),
    }
}

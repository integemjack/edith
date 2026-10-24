//! EDITH：下一代的大脑（第一个原型，docs/EDITH.md）。给它用户的话，它自己调模型、用工具把事做完；
//! 四肢（MCP 能力、客户端、渠道）经对外接口挂上来，没有四肢也能干活。

mod brain;
mod home;
mod http;
mod limbs;
mod server;
mod sessions;
mod tools;

use std::io::{BufRead, IsTerminal, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::{Value, json};

use brain::Event;
use sessions::Session;

const HELP: &str = "EDITH——能自己把事做完的大脑

用法：
  edith \"要做的事\"                 在当前目录干一件事
  edith                            一句一句聊（同一个会话；空行或 Ctrl+D 结束）
  edith -c <会话 id> \"…\"           接着某个会话（id 写开头几位就行）
  edith sessions                   列出会话
  edith limbs                      列出挂着的四肢和它们的工具
  edith limbs add <名字> <命令> [参数…]   挂上一个本机的 MCP 服务（记进 ~/.edith/limbs.json）
  edith limbs add-url <名字> <地址>        挂上一个 HTTP 的 MCP 服务
  edith limbs remove <名字>                摘下
  edith serve [--listen 127.0.0.1:7788] [--token …]   开对外接口

选项：
  -C, --cwd <目录>    工作目录（默认当前目录）
  -q, --quiet         只打最终回答，不打过程
  -v, --verbose       也打思考和每段请求的用量

模型：~/.edith/config.json 的 {\"baseURL\", \"apiKey\", \"model\"}，或环境变量 EDITH_BASE_URL / EDITH_API_KEY / EDITH_MODEL";

/// Ctrl+C：第一次让这一轮停下，第二次直接退出
static CANCEL: AtomicBool = AtomicBool::new(false);

#[cfg(unix)]
fn install_interrupt() {
    extern "C" fn on_interrupt(_: libc::c_int) {
        if CANCEL.swap(true, Ordering::SeqCst) {
            unsafe { libc::_exit(130) };
        }
    }
    unsafe {
        libc::signal(libc::SIGINT, on_interrupt as *const () as libc::sighandler_t);
    }
}

#[cfg(not(unix))]
fn install_interrupt() {}

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
        Some("limbs") => {
            brain::setup_prompts();
            exit_on(limbs_command(&arguments[1..]));
        }
        Some("serve") => {
            brain::setup_prompts();
            limbs::attach_saved(&mut |message| eprintln!("{message}"));
            let mut listen = "127.0.0.1:7788".to_string();
            let mut token = std::env::var("EDITH_TOKEN").ok().filter(|token| !token.is_empty());
            let mut rest = arguments[1..].iter();
            while let Some(argument) = rest.next() {
                match argument.as_str() {
                    "--listen" => listen = rest.next().cloned().unwrap_or(listen),
                    "--token" => token = rest.next().cloned(),
                    other => exit_on(Err(format!("serve 不认识 {other}"))),
                }
            }
            exit_on(server::serve(&listen, token));
        }
        _ => match parse(&arguments) {
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
    Ok(options)
}

fn work(options: Options) -> Result<(), String> {
    let model = home::Model::load()?;
    brain::setup_prompts();
    limbs::attach_saved(&mut |message| eprintln!("{message}"));
    install_interrupt();
    let mut session = match &options.session {
        Some(id) => Session::find(id).ok_or_else(|| format!("没有会话 {id}（edith sessions 看有哪些）"))?,
        None => Session::new(&options.cwd.to_string_lossy()),
    };
    let cwd = if options.session.is_some() && std::path::Path::new(&session.cwd).is_dir() {
        PathBuf::from(&session.cwd)
    } else {
        options.cwd.clone()
    };
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
        let failed = turn(&model, &mut session, &task, &cwd, &options);
        if !options.quiet {
            eprintln!("\x1b[2m（会话 {}，接着做：edith -c {} \"…\"）\x1b[0m", &session.id[..8], &session.id[..8]);
        }
        return failed.map_or(Ok(()), Err);
    }
    // 一句一句聊
    eprintln!("EDITH（{}）在 {}。说要做的事，空行或 Ctrl+D 结束。", model.model, cwd.display());
    let stdin = std::io::stdin();
    loop {
        eprint!("\x1b[1m> \x1b[0m");
        let _ = std::io::stderr().flush();
        let mut line = String::new();
        if stdin.lock().read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
            break;
        }
        CANCEL.store(false, Ordering::SeqCst);
        if let Some(message) = turn(&model, &mut session, line.trim(), &cwd, &options) {
            eprintln!("\x1b[31m{message}\x1b[0m");
        }
    }
    if !session.history.is_empty() {
        eprintln!("\x1b[2m（会话 {}）\x1b[0m", &session.id[..8]);
    }
    Ok(())
}

/// 干一轮、存会话；出错返回原因
fn turn(model: &home::Model, session: &mut Session, task: &str, cwd: &std::path::Path, options: &Options) -> Option<String> {
    let mut printer = Printer { quiet: options.quiet, verbose: options.verbose, thinking: false, line_start: true };
    let result = brain::run(model, std::mem::take(&mut session.history), task, cwd, &CANCEL, &mut |event| printer.show(event));
    printer.finish();
    session.history = result.history;
    let saved = session.save().err();
    if options.quiet {
        println!("{}", result.answer.trim());
    }
    if result.stopped {
        eprintln!("\x1b[33m（停下了）\x1b[0m");
    }
    result.error.or(saved)
}

/// 终端上的样子：回答打到标准输出，过程（工具、提示）打到标准错误
struct Printer {
    quiet: bool,
    verbose: bool,
    thinking: bool,
    line_start: bool,
}

impl Printer {
    fn out(&mut self, text: &str) {
        print!("{text}");
        let _ = std::io::stdout().flush();
        self.line_start = text.ends_with('\n');
    }

    fn side(&mut self, text: &str) {
        if !self.line_start {
            println!();
            self.line_start = true;
        }
        eprintln!("{text}");
    }

    fn show(&mut self, event: Event) {
        if self.quiet {
            return;
        }
        match event {
            Event::Text(text) => {
                if self.thinking {
                    self.thinking = false;
                    eprintln!("\x1b[0m");
                }
                self.out(&text);
            }
            Event::Thinking(text) if self.verbose => {
                if !self.thinking {
                    self.thinking = true;
                    eprint!("\x1b[2m");
                }
                eprint!("{text}");
            }
            Event::Thinking(_) => {}
            Event::Tool { name, arguments, .. } => {
                let brief = brief(&arguments);
                self.side(&format!("\x1b[36m▸ {name}\x1b[0m {brief}"));
            }
            Event::Result { ok, text, .. } => {
                let first = text.lines().find(|line| !line.trim().is_empty()).unwrap_or("").chars().take(100).collect::<String>();
                let mark = if ok { "\x1b[32m✓\x1b[0m" } else { "\x1b[31m✗\x1b[0m" };
                self.side(&format!("  {mark} \x1b[2m{first}\x1b[0m"));
            }
            Event::Stats(stats) if self.verbose => {
                let line = format!(
                    "  · 输入 {} token、输出 {} token",
                    stats["promptTokens"].as_i64().unwrap_or(0),
                    stats["generatedTokens"].as_i64().unwrap_or(0)
                );
                self.side(&format!("\x1b[2m{line}\x1b[0m"));
            }
            Event::Stats(_) => {}
            Event::Notice(text) => self.side(&format!("\x1b[33m{text}\x1b[0m")),
        }
    }

    fn finish(&mut self) {
        if self.thinking {
            eprint!("\x1b[0m");
        }
        if !self.line_start {
            println!();
            self.line_start = true;
        }
    }
}

/// 工具参数的一行简述（路径、命令、关键词……）
fn brief(arguments: &Value) -> String {
    for key in ["command", "path", "pattern", "keywords", "url", "query"] {
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

fn limbs_command(arguments: &[String]) -> Result<(), String> {
    match arguments.first().map(String::as_str) {
        None | Some("list") => {
            limbs::attach_saved(&mut |message| eprintln!("{message}"));
            let all = limbs::all();
            if all.is_empty() {
                println!("还没有挂四肢（edith limbs add <名字> <命令> [参数…]）");
            }
            for limb in all {
                let names: Vec<&str> = limb.tools.iter().filter_map(|tool| tool.get("name").and_then(Value::as_str)).collect();
                println!("{}（{} 个工具）：{}", limb.name, names.len(), names.join("、"));
            }
            Ok(())
        }
        Some("add") if arguments.len() >= 3 => {
            limbs::attach_saved(&mut |_| {});
            let config = json!({ "name": arguments[1], "command": arguments[2], "args": arguments[3..] });
            let limb = limbs::attach(config, true)?;
            println!("挂上了「{}」：{} 个工具", limb.name, limb.tools.len());
            Ok(())
        }
        Some("add-url") if arguments.len() == 3 => {
            limbs::attach_saved(&mut |_| {});
            let limb = limbs::attach(json!({ "name": arguments[1], "url": arguments[2] }), true)?;
            println!("挂上了「{}」：{} 个工具", limb.name, limb.tools.len());
            Ok(())
        }
        Some("remove") if arguments.len() == 2 => {
            // 先挂上才能摘（摘时顺带从 limbs.json 去掉）
            limbs::attach_saved(&mut |_| {});
            if limbs::detach(&limbs::clean_name(&arguments[1]), true) { Ok(()) } else { Err(format!("没有挂「{}」", arguments[1])) }
        }
        _ => Err("用法：edith limbs [add <名字> <命令> [参数…] | add-url <名字> <地址> | remove <名字>]".into()),
    }
}

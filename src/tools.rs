//! 大脑自带的工具（不靠能力就有的）：时间、列目录、读 / 写 / 改文件、按名字找、按内容搜、按关键词搜、抓网页、跑命令。
//! 工具定义和说明来自 friday-core 的 tool_defs + prompts-tools.md，执行用 tool_files / file_edit / file_search / shell_rules。

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use friday_core::file_edit::Edit;
use friday_core::tool_files::{self, Change, ToolError};
use friday_core::{file_search, shell_rules, tool_defs};
use serde_json::{Map, Value};

use crate::home;

/// 大脑自带的工具（顺序就是给模型的顺序）
pub const BUILTINS: &[&str] = &[
    "get_current_time",
    "list_directory",
    "read_file",
    "find_files",
    "search_files",
    "search_keywords",
    "write_file",
    "edit_file",
    "run_shell",
    "fetch_url",
];

/// 自带工具的定义（发给模型的 tools 里的项）
pub fn specs() -> Vec<Value> {
    let builtins = tool_defs::builtins(&Default::default(), None);
    BUILTINS.iter().filter_map(|name| builtins.iter().find(|tool| tool.name == *name).map(|tool| tool.spec.clone())).collect()
}

/// 一次工具调用的结果
pub struct Outcome {
    pub ok: bool,
    pub text: String,
}

impl Outcome {
    fn ok(text: impl Into<String>) -> Self {
        Outcome { ok: true, text: text.into() }
    }

    fn failed(text: impl Into<String>) -> Self {
        Outcome { ok: false, text: text.into() }
    }
}

impl From<Result<String, ToolError>> for Outcome {
    fn from(result: Result<String, ToolError>) -> Self {
        match result {
            Ok(text) => Outcome::ok(text),
            Err(error) => Outcome::failed(error.message()),
        }
    }
}

/// 相对路径按工作目录、~ 按家目录
pub fn resolve(path: &str, cwd: &Path) -> PathBuf {
    let path = path.trim();
    if path == "~" {
        return home::user_home();
    }
    if let Some(rest) = path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")) {
        return home::user_home().join(rest);
    }
    let given = Path::new(path);
    if given.is_absolute() { given.to_path_buf() } else { cwd.join(given) }
}

fn text<'a>(arguments: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    arguments.get(key).and_then(Value::as_str)
}

fn int(arguments: &Map<String, Value>, key: &str) -> Option<i64> {
    // 有的模型把数字写成字符串
    arguments.get(key).and_then(|value| value.as_i64().or_else(|| value.as_str().and_then(|text| text.trim().parse().ok())))
}

fn flag(arguments: &Map<String, Value>, key: &str) -> bool {
    arguments.get(key).and_then(|value| value.as_bool().or_else(|| value.as_str().map(|text| text == "true"))).unwrap_or(false)
}

/// 跑一个自带工具（不是自带的返回 None）
pub fn run(name: &str, arguments: &Map<String, Value>, cwd: &Path, cancel: &AtomicBool) -> Option<Outcome> {
    if !BUILTINS.contains(&name) {
        return None;
    }
    // 必填参数缺了：照 Friday 的说法告诉模型
    let spec = specs().into_iter().find(|spec| spec["function"]["name"] == name)?;
    let missing = tool_defs::missing_required(&spec, arguments);
    if !missing.is_empty() {
        return Some(Outcome::failed(format!("缺少参数：{}", missing.join("、"))));
    }
    let working = tool_files::slash(cwd);
    let path = |key: &str| tool_files::slash(&resolve(text(arguments, key).unwrap_or("."), cwd));
    let output = |result: Result<tool_files::Output, ToolError>| match result {
        Ok(output) => Outcome::ok(output.text),
        Err(error) => Outcome::failed(error.message()),
    };
    Some(match name {
        "get_current_time" => {
            let now = chrono::Local::now();
            Outcome::ok(format!("{}（{}，UTC{}）", now.format("%Y-%m-%d %H:%M:%S"), weekday(&now), now.format("%:z")))
        }
        "list_directory" => output(tool_files::list_directory(&path("path"), &working)),
        "read_file" => output(tool_files::read_file(&path("path"), int(arguments, "offset"), int(arguments, "limit"), &working)),
        "write_file" => output(tool_files::write_file(&path("path"), text(arguments, "content").unwrap_or(""))),
        "edit_file" => output(tool_files::edit_file(&path("path"), &change(arguments), &working)),
        "find_files" => file_search::find_files(text(arguments, "pattern").unwrap_or(""), &path("path"), &working).into(),
        "search_files" => file_search::search_files(
            text(arguments, "pattern").unwrap_or(""),
            &path("path"),
            text(arguments, "glob").filter(|glob| !glob.is_empty()),
            flag(arguments, "ignore_case"),
            &working,
        )
        .into(),
        "search_keywords" => file_search::search_keywords(
            text(arguments, "keywords").unwrap_or(""),
            &path("path"),
            text(arguments, "glob").filter(|glob| !glob.is_empty()),
            flag(arguments, "match_case"),
            flag(arguments, "match_any"),
            &working,
        )
        .into(),
        "fetch_url" => fetch_url(text(arguments, "url").unwrap_or(""), int(arguments, "offset")),
        "run_shell" => run_shell(text(arguments, "command").unwrap_or(""), int(arguments, "timeout_seconds"), cwd, cancel),
        _ => return None,
    })
}

fn change(arguments: &Map<String, Value>) -> Change {
    let edits: Vec<Edit> = arguments
        .get("edits")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_object)
                .map(|item| Edit {
                    old_text: text(item, "old_text").unwrap_or("").to_string(),
                    new_text: text(item, "new_text").unwrap_or("").to_string(),
                    replace_all: flag(item, "replace_all"),
                })
                .collect()
        })
        .unwrap_or_default();
    if !edits.is_empty() {
        return Change::Several(edits);
    }
    Change::One {
        old_text: text(arguments, "old_text").unwrap_or("").to_string(),
        new_text: text(arguments, "new_text").unwrap_or("").to_string(),
        replace_all: flag(arguments, "replace_all"),
    }
}

fn fetch_url(url: &str, offset: Option<i64>) -> Outcome {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Outcome::failed("url 要以 http:// 或 https:// 开头");
    }
    let mut response = match crate::http::agent().get(url).header("User-Agent", "Mozilla/5.0 (EDITH)").call() {
        Ok(response) => response,
        Err(error) => return Outcome::failed(format!("抓不到：{error}")),
    };
    let status = response.status().as_u16();
    let html = response.headers().get("content-type").and_then(|value| value.to_str().ok()).is_some_and(|kind| kind.contains("html"));
    let body = match response.body_mut().with_config().limit(10 * 1024 * 1024).read_to_string() {
        Ok(body) => body,
        Err(error) => return Outcome::failed(format!("读不完：{error}")),
    };
    if !(200..300).contains(&status) {
        return Outcome::failed(format!("HTTP {status}\n{}", tool_files::truncate(&body)));
    }
    let text = if html { tool_files::strip_html(&body) } else { body };
    Outcome::ok(tool_files::page(&text, offset))
}

/// 跑命令：Unix 用登录时的 shell（$SHELL，没有用 /bin/sh），Windows 用 PowerShell；标准输出和错误按到达顺序合在一起；
/// 超时或被停下时整组进程一起结束
fn run_shell(command: &str, timeout: Option<i64>, cwd: &Path, cancel: &AtomicBool) -> Outcome {
    if command.trim().is_empty() {
        return Outcome::failed("command 是空的");
    }
    if let Some(refusal) = shell_rules::self_kill_refusal(command, "edith", std::process::id() as i64) {
        return Outcome::failed(refusal);
    }
    let timeout = Duration::from_secs(shell_rules::timeout_seconds(timeout) as u64);
    let mut process = shell_command(command);
    process.current_dir(cwd).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).env("EDITH", "1");
    #[cfg(unix)]
    unsafe {
        use std::os::unix::process::CommandExt;
        // 自己一个进程组：超时时连它起的子进程一起结束
        process.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        process.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = match process.spawn() {
        Ok(child) => child,
        Err(error) => return Outcome::failed(format!("起不了命令行：{error}")),
    };
    let output = Arc::new(Mutex::new(Vec::<u8>::new()));
    let mut readers = Vec::new();
    let pipes: Vec<Box<dyn Read + Send>> = [
        child.stdout.take().map(|pipe| Box::new(pipe) as Box<dyn Read + Send>),
        child.stderr.take().map(|pipe| Box::new(pipe) as Box<dyn Read + Send>),
    ]
    .into_iter()
    .flatten()
    .collect();
    for mut pipe in pipes {
        let output = output.clone();
        readers.push(std::thread::spawn(move || {
            let mut buffer = [0u8; 8192];
            while let Ok(count) = pipe.read(&mut buffer) {
                if count == 0 {
                    break;
                }
                output.lock().unwrap().extend_from_slice(&buffer[..count]);
            }
        }));
    }
    let started = Instant::now();
    let code = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.code().map(i64::from),
            Ok(None) => {}
            Err(_) => break None,
        }
        if started.elapsed() >= timeout || cancel.load(Ordering::Relaxed) {
            kill_tree(&mut child);
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(30));
    };
    for reader in readers {
        let _ = reader.join();
    }
    let text = String::from_utf8_lossy(&output.lock().unwrap()).into_owned();
    let mut result = shell_rules::result(code, &text);
    result.push_str(&shell_rules::hint(command, "edith"));
    Outcome { ok: code == Some(0), text: result }
}

fn shell_command(command: &str) -> Command {
    if cfg!(windows) {
        let mut process = Command::new("powershell.exe");
        process.args(["-NoProfile", "-NonInteractive", "-Command", command]);
        process
    } else {
        let shell = std::env::var("SHELL").ok().filter(|shell| !shell.is_empty()).unwrap_or_else(|| "/bin/sh".into());
        let mut process = Command::new(shell);
        process.arg("-c").arg(command);
        process
    }
}

fn kill_tree(child: &mut std::process::Child) {
    #[cfg(unix)]
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill").args(["/T", "/F", "/PID", &child.id().to_string()]).output();
    }
    let _ = child.kill();
}

/// 星期几（中文）
pub fn weekday(time: &chrono::DateTime<chrono::Local>) -> &'static str {
    use chrono::Datelike;
    ["星期一", "星期二", "星期三", "星期四", "星期五", "星期六", "星期日"][time.weekday().num_days_from_monday() as usize]
}

/// 平台的说法（提示词里的平台参数）
pub fn shell_title() -> String {
    if cfg!(windows) {
        return "PowerShell".into();
    }
    let shell = std::env::var("SHELL").unwrap_or_default();
    Path::new(&shell)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "sh".into())
}

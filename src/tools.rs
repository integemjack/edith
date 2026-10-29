//! 大脑自带的工具（不靠能力就有的）：时间、列目录、读 / 写 / 改文件、按名字找、按内容搜、按关键词搜、跑命令、搜网页、看网页（web.rs）。
//! 工具定义和说明来自 friday-core 的 tool_defs + prompts-tools.md，执行用 tool_files / file_edit / file_search / shell_rules。

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
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
    "web_search",
    "fetch_url",
];

/// 同 Friday v2.92：按需给的自带工具（搜网页、看网页：.51 上约四分之一的对话用到；在按需清单的「网页」组里）
pub const DEFERRED: &[&str] = &["web_search", "fetch_url"];
/// 同 Friday v2.92：不再给模型的（时间附在交来的话后面：mind::now_note）；旧名照样能调
pub const RETIRED: &[&str] = &["get_current_time"];

/// 自带工具的定义（全部的：设置里列的、执行时核对参数用）
pub fn specs() -> Vec<Value> {
    let builtins = tool_defs::builtins(&Default::default(), None);
    BUILTINS.iter().filter_map(|name| builtins.iter().find(|tool| tool.name == *name).map(|tool| tool.spec.clone())).collect()
}

/// 一直给模型的自带工具
pub fn given_specs() -> Vec<Value> {
    specs()
        .into_iter()
        .filter(|spec| spec["function"]["name"].as_str().is_some_and(|name| !DEFERRED.contains(&name) && !RETIRED.contains(&name)))
        .collect()
}

/// 按需给的自带工具
pub fn deferred_specs() -> Vec<Value> {
    specs().into_iter().filter(|spec| spec["function"]["name"].as_str().is_some_and(|name| DEFERRED.contains(&name))).collect()
}

/// 一次工具调用的结果
pub struct Outcome {
    pub ok: bool,
    pub text: String,
}

thread_local! {
    /// 这一步（write_file / edit_file / run_shell）加了几行、删了几行：brain 在发 result 事件时取走（卡片上的「+N −M」）
    static LINES: std::cell::Cell<Option<[usize; 2]>> = const { std::cell::Cell::new(None) };
}

thread_local! {
    /// 这一步交给模型看的图片（browser_screenshot）：brain 跟在工具结果后面发给能看图的模型
    static IMAGES: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// 这一步有张图要给模型看（这台上的路径）
pub fn note_image(path: &str) {
    IMAGES.with(|images| images.borrow_mut().push(path.to_string()));
}

/// 取走这个线程上记下的图片
pub fn take_images() -> Vec<String> {
    IMAGES.with(|images| std::mem::take(&mut *images.borrow_mut()))
}

/// 取走这个线程上一步记下的增删行数
pub fn take_lines() -> Option<[usize; 2]> {
    LINES.with(|lines| lines.take())
}

fn note_lines(added: usize, removed: usize) {
    LINES.with(|lines| lines.set(Some([added, removed])));
}

/// 去掉 Windows 的 \\?\ 前缀（UNC 的不动）
fn strip_verbatim(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    match text.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC\\") => PathBuf::from(rest),
        _ => path.to_path_buf(),
    }
}

/// 一个文本文件现在的全文（没有、太大、不是文字的当空）
fn text_of(path: &str) -> String {
    let file = Path::new(path);
    if std::fs::metadata(file).is_ok_and(|meta| meta.len() <= 1 << 20) {
        std::fs::read_to_string(file).unwrap_or_default()
    } else {
        String::new()
    }
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

/// 读过的文件：(会话, 路径) → 读的时候文件内容的 SHA-256。改已有的文件之前要读过、而且读完之后没被别人动过
/// （同 Claude Code：不加锁，撞上了才要它重读）
/// 会话读过的文件：会话 → {路径: 读到 / 写完时的 SHA-256}；也存在 `<项目数据>/reads/<会话>.json`，-c / -r 接着时还认
static SEEN: LazyLock<Mutex<HashMap<String, HashMap<String, String>>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

fn digest(path: &str) -> Option<String> {
    use sha2::Digest;
    let bytes = std::fs::read(path).ok()?;
    Some(sha2::Sha256::digest(&bytes).iter().map(|byte| format!("{byte:02x}")).collect())
}

fn reads_path(session: &str) -> PathBuf {
    home::project_dir().join("reads").join(format!("{session}.json"))
}

/// 这个会话读过的（内存里没有就从文件读）
fn seen(session: &str, path: &str) -> Option<String> {
    let mut all = SEEN.lock().unwrap();
    let entry = all
        .entry(session.to_string())
        .or_insert_with(|| home::read_json(&reads_path(session)).and_then(|value| serde_json::from_value(value).ok()).unwrap_or_default());
    entry.get(path).cloned()
}

/// 记下这个会话看到的样子（读完、自己写完之后）
fn remember(session: &str, path: &str) {
    let Some(hash) = digest(path) else { return };
    let _ = seen(session, path);
    let mut all = SEEN.lock().unwrap();
    let entry = all.entry(session.to_string()).or_default();
    entry.insert(path.to_string(), hash);
    if !session.is_empty() {
        let _ = home::write_json(&reads_path(session), &serde_json::json!(entry));
    }
}

/// 改之前：文件已经存在的，这个会话要读过、读完之后没变过；不行返回说明
fn stale(session: &str, path: &str) -> Option<String> {
    let current = digest(path)?;
    match seen(session, path) {
        None => Some(format!("{path} 已经存在，还没读过：先 read_file 看一遍再改")),
        Some(seen) if seen != current => {
            Some(format!("{path} 在你上次读过之后被改过（可能是别的大脑、命令或用户）：先 read_file 重读，再在新内容上改"))
        }
        Some(_) => None,
    }
}

/// 跑一个自带工具（不是自带的返回 None）。session：这一轮在哪个会话（记它读过哪些文件）
/// 路径写成正斜杠的文字（Windows 也是）
pub fn slash_path(path: &Path) -> String {
    tool_files::slash(path)
}

pub fn run(
    name: &str,
    arguments: &Map<String, Value>,
    cwd: &Path,
    cancel: &AtomicBool,
    session: &str,
    private_ok: bool,
) -> Option<Outcome> {
    if !BUILTINS.contains(&name) {
        return None;
    }
    // 必填参数缺了：照 Friday 的说法告诉模型
    let spec = specs().into_iter().find(|spec| spec["function"]["name"] == name)?;
    let missing = tool_defs::missing_required(&spec, arguments);
    if !missing.is_empty() {
        return Some(Outcome::failed(format!("缺少参数：{}", missing.join("、"))));
    }
    // Windows 上的工作目录可能带 \\?\ 前缀（canonicalize 出来的）：工具给模型看的、会话改动记的路径、命令前后的扫描都用不带前缀的
    let plain = strip_verbatim(cwd);
    let cwd = plain.as_path();
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
        "read_file" => {
            let target = path("path");
            let outcome = output(tool_files::read_file(&target, int(arguments, "offset"), int(arguments, "limit"), &working));
            if outcome.ok {
                remember(session, &target);
            }
            outcome
        }
        "write_file" | "edit_file" => {
            let target = path("path");
            if let Some(refusal) = stale(session, &target) {
                return Some(Outcome::failed(refusal));
            }
            // 会话改动：第一次动它之前的样子记成基线（changes.rs）
            crate::changes::before_write(session, &target);
            let before = text_of(&target);
            let outcome = if name == "write_file" {
                output(tool_files::write_file(&target, text(arguments, "content").unwrap_or("")))
            } else {
                output(tool_files::edit_file(&target, &change(arguments), &working))
            };
            if outcome.ok {
                remember(session, &target);
                // 同时跑着的命令（并行的子智能体）不把它算成命令改的
                friday_core::file_changes::note_written(&target, &tool_files::slash(&home::user_home()));
                let counts = friday_core::line_diff::count_text(&before, &text_of(&target));
                note_lines(counts.added, counts.removed);
            }
            outcome
        }
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
        "web_search" => crate::web::search(
            text(arguments, "query").unwrap_or(""),
            text(arguments, "time_range").unwrap_or(""),
            text(arguments, "language").unwrap_or(""),
            int(arguments, "count").unwrap_or(8).max(1) as usize,
        ),
        "fetch_url" => crate::web::fetch(text(arguments, "url").unwrap_or(""), int(arguments, "offset"), private_ok),
        "run_shell" => {
            // 会话改动：执行前后各看一遍，命令动了的文件记基线（changes.rs）
            let command = text(arguments, "command").unwrap_or("");
            let home_dir = tool_files::slash(&home::user_home());
            let snapshot = friday_core::file_changes::before(command, &working, &home_dir, &working);
            let outcome = run_shell(command, int(arguments, "timeout_seconds"), cwd, cancel);
            let (counts, changed) = friday_core::file_changes::after(&snapshot);
            crate::changes::note_shell(session, &changed);
            if let Some(counts) = counts {
                note_lines(counts.added, counts.removed);
            }
            outcome
        }
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

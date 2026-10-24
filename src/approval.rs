//! 审批（同 Claude Code 的「要不要让它做」）：改东西、往外发东西、碰敏感的东西之前问一声。
//!
//! 要批准的：
//! - 写文件、改文件、跑命令；能力的动作、MCP 的工具（没说自己只读的）；
//! - 读工作目录外的、或敏感的文件（`~/.ssh`、`.env`、密钥、`~/.edith` 等）——「只读」也能把这些带出去；
//! - 打开新网站（fetch_url；同一个会话里批过的网站不再问）——网址本身就能把数据带出去；
//! - 访问本机 / 内网地址（防 SSRF）：权限不全的连接交来的那一轮直接不许。
//!
//! 谁来批：这一轮是终端里打的，问终端；是能力交来的、它声明了 `approves` 又是全权的，问它；不然问终端或别的能批准的全权能力
//! （`edith attach` 接上来的终端就是一个）。权限不全的连接（中转的远程设备）自己不能批自己的。没人能批就不做。
//!
//! 不问：`--auto`（或环境变量 `EDITH_PERMISSION_MODE=auto`、`~/.edith/config.json` 的 `permissions.mode: "auto"`）；
//! `permissions.allow` 里的规则；这个会话里答过「都行」的。`permissions.deny` 的规则什么时候都挡。
//! 规则写法：`工具名` 或 `工具名(目标)`，目标末尾 `*` 按前缀：`run_shell(git status*)`、`fetch_url(github.com)`、`write_file`。
//! 只认全局的 config.json：项目里的配置不能替你放开权限。

use std::collections::HashMap;
use std::io::Write;
use std::net::{IpAddr, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};

use crate::brain::Context;
use crate::home;
use crate::permissions::Tools;

/// 等批准最多多久
const WAIT: Duration = Duration::from_secs(600);

/// 怎么答的
#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    /// 这次行
    Allow,
    /// 这个会话里同类的都行
    Always,
    /// 不行（原因交给模型）
    Deny(String),
}

impl Decision {
    /// 回给大脑的（approve 的回话）
    pub fn to_json(&self) -> Value {
        match self {
            Decision::Allow => json!({ "decision": "allow" }),
            Decision::Always => json!({ "decision": "always" }),
            Decision::Deny(reason) => json!({ "decision": "deny", "reason": reason }),
        }
    }

    pub fn parse(value: &Value) -> Decision {
        let reason = value["reason"].as_str().unwrap_or("").to_string();
        match value["decision"].as_str() {
            Some("allow") => Decision::Allow,
            Some("always") => Decision::Always,
            _ => Decision::Deny(reason),
        }
    }

    /// 终端里打的：y 这次、a 都行、别的不行（n 后面可以接原因）
    pub fn from_line(line: &str) -> Decision {
        let line = line.trim();
        let lower = line.to_lowercase();
        if matches!(lower.as_str(), "y" | "yes" | "是" | "行" | "好") {
            Decision::Allow
        } else if matches!(lower.as_str(), "a" | "always" | "都行") {
            Decision::Always
        } else {
            let reason = line.trim_start_matches(['n', 'N']).trim().to_string();
            Decision::Deny(reason)
        }
    }
}

/// 一件要批准的事
#[derive(Debug, Clone)]
pub struct Need {
    pub tool: String,
    /// 目标（命令、路径、网站、动作）
    pub target: String,
    /// 给人看的一句
    pub summary: String,
    /// 答「都行」时记下的规则
    always: String,
    /// 访问本机 / 内网
    pub private_network: bool,
}

/// 0 按配置、1 问、2 不问（--auto）
static MODE: AtomicU8 = AtomicU8::new(0);
/// 会话里答过「都行」的规则
static SESSION_RULES: LazyLock<Mutex<HashMap<String, Vec<String>>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

pub fn set_auto(auto: bool) {
    MODE.store(if auto { 2 } else { 1 }, Ordering::Relaxed);
}

fn settings() -> Value {
    home::read_json(&home::dir().join("config.json")).map(|config| config["permissions"].clone()).unwrap_or(Value::Null)
}

fn auto() -> bool {
    match MODE.load(Ordering::Relaxed) {
        2 => true,
        1 => false,
        _ => std::env::var("EDITH_PERMISSION_MODE").ok().as_deref() == Some("auto") || settings()["mode"].as_str() == Some("auto"),
    }
}

fn rules(kind: &str) -> Vec<String> {
    settings()[kind].as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_string).collect()
}

/// 规则对得上这件事
fn matches(rule: &str, need: &Need) -> bool {
    let (tool, pattern) = match rule.split_once('(') {
        Some((tool, rest)) => (tool.trim(), Some(rest.trim_end_matches(')').trim())),
        None => (rule.trim(), None),
    };
    if tool != need.tool {
        return false;
    }
    match pattern {
        None | Some("*") | Some("") => true,
        Some(pattern) => {
            let pattern = expand_home(pattern);
            match pattern.strip_suffix('*') {
                Some(prefix) => need.target.starts_with(prefix),
                None => need.target == pattern,
            }
        }
    }
}

fn expand_home(text: &str) -> String {
    match text.strip_prefix("~/") {
        Some(rest) => crate::tools::slash_path(&home::user_home().join(rest)),
        None => text.to_string(),
    }
}

// MARK: - 要不要批准

const READ_TOOLS: &[&str] = &["list_directory", "read_file", "find_files", "search_files", "search_keywords"];

/// 敏感的：密钥、凭据、EDITH 自己的数据（主令牌、模型的 apiKey）
pub fn sensitive(path: &Path) -> bool {
    let text = crate::tools::slash_path(path);
    let edith = crate::tools::slash_path(&home::dir());
    let skills = format!("{edith}/skills");
    if (text == edith || text.starts_with(&format!("{edith}/"))) && text != skills && !text.starts_with(&format!("{skills}/")) {
        return true;
    }
    path.components().any(|component| {
        let name = component.as_os_str().to_string_lossy().to_lowercase();
        matches!(
            name.as_str(),
            ".ssh"
                | ".gnupg"
                | ".aws"
                | ".azure"
                | ".kube"
                | ".netrc"
                | ".git-credentials"
                | ".npmrc"
                | ".pypirc"
                | ".docker"
                | "keychains"
        ) || name == ".env"
            || name.starts_with(".env.")
            || name.starts_with("id_rsa")
            || name.starts_with("id_ed25519")
            || name.starts_with("id_ecdsa")
            || name.ends_with(".pem")
            || name.ends_with(".key")
            || name.ends_with(".p12")
            || name == "credentials"
            || name == "login data"
            || name == "cookies"
    })
}

/// 读的时候不用问的地方：工作目录、这个会话的目录、临时目录、permissions.additionalDirectories
fn inside_roots(path: &Path, cwd: &Path) -> bool {
    let canonical = |path: &Path| crate::tools::slash_path(&std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()));
    let target = canonical(path);
    // 技能的文件夹也算：use_skill 读了之后要用里面的脚本、参考资料
    let mut roots = vec![home::workspace(), cwd.to_path_buf(), std::env::temp_dir()];
    roots.extend(crate::skills::dirs());
    roots.extend(rules("additionalDirectories").iter().map(|dir| PathBuf::from(expand_home(dir))));
    roots.iter().any(|root| {
        let root = canonical(root);
        target == root || target.starts_with(&format!("{}/", root.trim_end_matches('/')))
    })
}

/// 网址的主机、端口
fn host_of(url: &str) -> Option<(String, u16)> {
    let (scheme, rest) = url.split_once("://")?;
    let authority = rest.split(['/', '?', '#']).next()?;
    let authority = authority.rsplit_once('@').map_or(authority, |(_, host)| host);
    let default = if scheme.eq_ignore_ascii_case("https") { 443 } else { 80 };
    if let Some(rest) = authority.strip_prefix('[') {
        let (host, port) = rest.split_once(']')?;
        return Some((host.to_string(), port.trim_start_matches(':').parse().unwrap_or(default)));
    }
    match authority.rsplit_once(':') {
        Some((host, port)) => Some((host.to_lowercase(), port.parse().unwrap_or(default))),
        None => Some((authority.to_lowercase(), default)),
    }
}

/// 本机、内网、链路本地（云的元数据 169.254.169.254）……
pub fn private_address(address: &IpAddr) -> bool {
    match address {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.octets()[0] == 100 && (64..128).contains(&v4.octets()[1])
                || v4.octets()[0] == 0
        }
        IpAddr::V6(v6) => {
            let first = v6.segments()[0];
            v6.is_loopback()
                || v6.is_unspecified()
                || (first & 0xfe00) == 0xfc00
                || (first & 0xffc0) == 0xfe80
                || v6.to_ipv4_mapped().is_some_and(|v4| private_address(&IpAddr::V4(v4)))
        }
    }
}

/// 这个网址指向本机 / 内网（主机名解析出来的任何一个地址是就算；解析不了不算）
pub fn private_url(url: &str) -> bool {
    let Some((host, port)) = host_of(url) else { return false };
    if host == "localhost" || host.ends_with(".localhost") || host.ends_with(".local") || host.ends_with(".internal") {
        return true;
    }
    (host.as_str(), port).to_socket_addrs().map(|mut addresses| addresses.any(|address| private_address(&address.ip()))).unwrap_or(false)
}

fn text(arguments: &Map<String, Value>, key: &str) -> String {
    arguments.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

fn clip(text: &str, limit: usize) -> String {
    if text.chars().count() > limit { format!("{}…", text.chars().take(limit).collect::<String>()) } else { text.to_string() }
}

/// 这个工具调用要不要批准；要的话是什么事
pub fn classify(name: &str, arguments: &Map<String, Value>, cwd: &Path) -> Option<Need> {
    let need =
        |target: String, summary: String, always: String| Need { tool: name.to_string(), target, summary, always, private_network: false };
    let path = |key: &str| crate::tools::resolve(arguments.get(key).and_then(Value::as_str).unwrap_or("."), cwd);
    if READ_TOOLS.contains(&name) {
        let target = path("path");
        let shown = crate::tools::slash_path(&target);
        if sensitive(&target) {
            return Some(need(shown.clone(), format!("读敏感的文件 / 目录：{shown}"), format!("{name}({shown})")));
        }
        if !inside_roots(&target, cwd) {
            let folder = if target.is_dir() { shown.clone() } else { target.parent().map(crate::tools::slash_path).unwrap_or_default() };
            return Some(need(shown.clone(), format!("读工作目录外的：{shown}"), format!("{name}({}/*)", folder.trim_end_matches('/'))));
        }
        return None;
    }
    match name {
        "write_file" | "edit_file" => {
            let shown = crate::tools::slash_path(&path("path"));
            let verb = if name == "write_file" { "写文件" } else { "改文件" };
            Some(need(shown.clone(), format!("{verb}：{shown}"), name.to_string()))
        }
        "run_shell" => {
            let command = text(arguments, "command");
            Some(need(command.clone(), format!("跑命令：{}（在 {}）", clip(&command, 300), cwd.display()), format!("run_shell({command})")))
        }
        "fetch_url" => {
            let url = text(arguments, "url");
            let host = host_of(&url).map(|(host, _)| host).unwrap_or_default();
            let mut need = need(host.clone(), format!("打开网页：{}", clip(&url, 300)), format!("fetch_url({host})"));
            if private_url(&url) {
                need.private_network = true;
                need.summary = format!("访问本机 / 内网地址：{}", clip(&url, 300));
            }
            Some(need)
        }
        _ => {
            let shown = clip(&Value::Object(arguments.clone()).to_string(), 300);
            if crate::capabilities::is_action(name) && !crate::capabilities::is_read_only(name) {
                return Some(need(name.to_string(), format!("能力的动作 {name}：{shown}"), name.to_string()));
            }
            if crate::mcp::is_tool(name) && !crate::mcp::is_read_only(name) {
                return Some(need(name.to_string(), format!("MCP 工具 {name}：{shown}"), name.to_string()));
            }
            None
        }
    }
}

/// 用工具之前：要批准的去问；不许做返回原因（交给模型）。Ok 里是许不许访问本机 / 内网（fetch_url 每一跳都看）
pub fn check(name: &str, arguments: &Map<String, Value>, cwd: &Path, context: &Context, cancel: &AtomicBool) -> Result<bool, String> {
    let guest = context.tools != Tools::All;
    // 主人的这一轮：问过、批了的（或 --auto）可以访问本机 / 内网；客人的从来不行
    let Some(need) = classify(name, arguments, cwd) else { return Ok(!guest && auto()) };
    if need.private_network && guest {
        return Err(format!("不许：{}（权限不全的连接交来的这一轮不能访问本机 / 内网）", need.summary));
    }
    if let Some(rule) = rules("deny").into_iter().find(|rule| matches(rule, &need)) {
        return Err(format!("不许：{}（~/.edith/config.json 的 permissions.deny：{rule}）", need.summary));
    }
    if auto() {
        return Ok(!guest);
    }
    let session_rules = SESSION_RULES.lock().unwrap().get(&context.session).cloned().unwrap_or_default();
    if rules("allow").iter().chain(session_rules.iter()).any(|rule| matches(rule, &need)) {
        return Ok(!guest && need.private_network);
    }
    let request = json!({
        "session": context.session,
        "turn": context.turn,
        "tool": need.tool,
        "target": need.target,
        "summary": need.summary,
        "arguments": arguments,
        "origin": context.origin,
    });
    let decision = ask(&request, &context.origin, guest, cancel).ok_or_else(|| {
        format!(
            "没做：{}——要批准，可是没人能批（不在终端里，也没有能批准的能力）。让用户在终端里跑、用 edith --auto，或在 ~/.edith/config.json 的 permissions.allow 里放开",
            need.summary
        )
    })?;
    match decision {
        Decision::Allow => Ok(need.private_network),
        Decision::Always => {
            SESSION_RULES.lock().unwrap().entry(context.session.clone()).or_default().push(need.always.clone());
            Ok(need.private_network)
        }
        Decision::Deny(reason) => {
            let reason = if reason.is_empty() { String::new() } else { format!("，用户说：{reason}") };
            Err(format!("用户没批准：{}{reason}。别换个办法硬做，按用户的意思来（不清楚就问）", need.summary))
        }
    }
}

/// 问谁：这一轮的来源能批就问它；不然终端；不然别的能批准的全权能力。都没有返回 None
fn ask(request: &Value, origin: &str, guest: bool, cancel: &AtomicBool) -> Option<Decision> {
    if origin.is_empty() && terminal_ready() {
        return terminal(request, cancel);
    }
    if !guest
        && !origin.is_empty()
        && let Some(decision) = crate::capabilities::ask_approval(Some(origin), request, cancel)
    {
        return Some(decision);
    }
    if terminal_ready() {
        return terminal(request, cancel);
    }
    // 别的能批准的全权能力（客人自己不能批自己的）
    crate::capabilities::ask_approval(None, request, cancel)
}

// MARK: - 终端里批

/// 这个进程的终端能批（stdin、stderr 都是终端时 main 打开）
static TERMINAL: AtomicBool = AtomicBool::new(false);
/// 等终端答的那一件（一次只问一件）
static PENDING: Mutex<Option<Sender<Decision>>> = Mutex::new(None);
static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

pub fn enable_terminal() {
    TERMINAL.store(true, Ordering::Relaxed);
}

fn terminal_ready() -> bool {
    TERMINAL.load(Ordering::Relaxed)
}

/// 终端打的一行：交给在等的那一件；没在等返回 false
pub fn answer_terminal(line: &str) -> bool {
    match PENDING.lock().unwrap().take() {
        Some(sender) => {
            let _ = sender.send(Decision::from_line(line));
            true
        }
        None => false,
    }
}

pub fn prompt_text(summary: &str) -> String {
    format!("\x1b[33m要批准吗：{summary}\x1b[0m\n  y 这次行 · a 这个会话里同类的都行 · n 不行（n 后面可以写原因）> ")
}

fn terminal(request: &Value, cancel: &AtomicBool) -> Option<Decision> {
    let _turn = ONE_AT_A_TIME.lock().unwrap();
    let (sender, receiver) = mpsc::channel();
    *PENDING.lock().unwrap() = Some(sender);
    eprint!("\n{}", prompt_text(request["summary"].as_str().unwrap_or("")));
    let _ = std::io::stderr().flush();
    let deadline = Instant::now() + WAIT;
    loop {
        match receiver.recv_timeout(Duration::from_millis(100)) {
            Ok(decision) => return Some(decision),
            Err(RecvTimeoutError::Disconnected) => return Some(Decision::Deny(String::new())),
            Err(RecvTimeoutError::Timeout) => {}
        }
        if cancel.load(Ordering::Relaxed) || Instant::now() >= deadline {
            PENDING.lock().unwrap().take();
            eprintln!();
            return Some(Decision::Deny("没等到回答".into()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rules_and_addresses() {
        let need = Need {
            tool: "run_shell".into(),
            target: "git status -s".into(),
            summary: String::new(),
            always: String::new(),
            private_network: false,
        };
        assert!(matches("run_shell(git status*)", &need) && matches("run_shell", &need) && !matches("run_shell(git push*)", &need));
        assert!(!matches("write_file", &need));
        assert!(private_url("http://127.0.0.1:8080/x") && private_url("http://[::1]/") && private_url("http://169.254.169.254/latest"));
        assert!(private_url("http://localhost/") && private_url("http://10.1.2.3/") && private_url("http://192.168.20.239:22/"));
        assert!(!private_url("https://93.184.216.34/"));
        assert!(
            sensitive(Path::new("/Users/x/.ssh/id_rsa"))
                && sensitive(Path::new("/w/.env.local"))
                && !sensitive(Path::new("/w/src/main.rs"))
        );
        assert_eq!(Decision::from_line("n 别删"), Decision::Deny("别删".into()));
        assert_eq!(Decision::from_line("a"), Decision::Always);
    }
}

//! 能力：大脑的四肢，讲能力协议（docs/CAPABILITY-PROTOCOL.md）：双向、实时，能力能主动推内容（sense）、交事（ask），
//! 大脑能让它做动作（act）、把干活的过程推给它（event）。两种接法，协议一样：
//! - 大脑按配置起能力的进程，经 stdin / stdout 讲（WebSocket、聊天渠道、语音……都这样接）；
//! - 自己打开的程序（终端、桌面端）经这个大脑的本机套接字连进来（instance.rs），断开就摘掉、不重启。
//!
//! 只会被调用的 MCP 服务也算一种能力（`"protocol": "mcp"`，见 mcp.rs）。
//!
//! 配置两层：全局 `~/.edith/capabilities.json` + 目录 `<工作目录>/.edith/capabilities.json`（同名的以目录为准）：
//! `{"capabilities": [{name, command, args?, env?, cwd?, protocol?, url?, headers?, disabled?, restart?}]}`。

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, LazyLock, Mutex, RwLock, Weak};
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};

use crate::brain::Context;
use crate::mind::{self, Disposition, Origin, Urgency};
use crate::protocol::{self, Message};
use crate::sessions::{self, Session};
use crate::tools::Outcome;
use crate::{home, mcp};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_ACTION_TIMEOUT: u64 = 600;
/// 能被订阅的事件
pub const EVENT_TYPES: &[&str] = &["turn.started", "text", "thinking", "tool", "result", "stats", "notice", "done"];

/// 一个接上了的能力（讲 EDITH 协议的）
pub struct Capability {
    pub name: String,
    pub config: Value,
    /// initialize 回的 capability（title、version、description）
    pub info: Value,
    pub senses: Vec<Value>,
    /// {name, description, parameters, timeoutSeconds?}
    pub actions: Vec<Value>,
    pub receives: Vec<String>,
    /// 只要焦点会话的事件（sessions: "focus"）
    focus_only: bool,
    /// 握手做完了（之前它发来的请求一律回 NOT_INITIALIZED）
    ready: bool,
    /// 它对外开的入口（initialize 回的 endpoints：WebSocket 地址等），登记进 instances/ 让别人找得到
    pub endpoints: Vec<Value>,
    /// 经本机套接字连进来的（不是大脑起的进程）
    pub connected: bool,
    writer: Mutex<Option<Box<dyn Write + Send>>>,
    pending: Mutex<HashMap<u64, mpsc::Sender<Result<Value, Value>>>>,
    next_id: AtomicU64,
    alive: AtomicBool,
    /// 它 ask 的那几轮：这几轮的事件不管订没订都推给它
    asked: Mutex<HashSet<String>>,
    /// 正在做的动作：call → (会话, 轮)（进度推到那一轮）
    acting: Mutex<HashMap<String, (String, String)>>,
    listener: Mutex<Option<u64>>,
    child: Mutex<Option<Child>>,
}

static CAPABILITIES: LazyLock<RwLock<BTreeMap<String, Arc<Capability>>>> = LazyLock::new(|| RwLock::new(BTreeMap::new()));
static STOPPING: AtomicBool = AtomicBool::new(false);

/// 能力名只留字母、数字、-、_（动作的工具名要合 OpenAI 的规矩）
pub fn clean_name(name: &str) -> String {
    let cleaned: String = name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
    cleaned.trim_matches('_').chars().take(24).collect()
}

fn layer_path(global: bool) -> PathBuf {
    if global { home::dir() } else { home::workspace_data() }.join("capabilities.json")
}

/// 两层是不是同一个文件（工作目录就是家目录时 .edith 就是全局的那个）
fn single_layer() -> bool {
    let canonical = |path: PathBuf| std::fs::canonicalize(&path).unwrap_or(path);
    canonical(home::dir()) == canonical(home::workspace_data())
}

fn read_layer(global: bool) -> Vec<Value> {
    home::read_json(&layer_path(global)).and_then(|value| value.get("capabilities").and_then(Value::as_array).cloned()).unwrap_or_default()
}

fn name_of(config: &Value) -> String {
    clean_name(config.get("name").and_then(Value::as_str).unwrap_or(""))
}

/// 合并后的配置：全局的 + 目录的（同名的以目录为准：目录里写 `{"name": "x", "disabled": true}` 就关掉全局的 x）；
/// 每项多一个 "scope"：global / workspace
pub fn configs() -> Vec<Value> {
    let mut merged: Vec<Value> = Vec::new();
    let layers: &[bool] = if single_layer() { &[false] } else { &[true, false] };
    for &global in layers {
        for mut item in read_layer(global) {
            item["scope"] = json!(if global { "global" } else { "workspace" });
            let name = name_of(&item);
            match merged.iter_mut().find(|existing| name_of(existing) == name) {
                // 只写了 disabled 的：关掉全局那个，别的照旧
                Some(existing) if item.get("command").is_none() && item.get("url").is_none() => {
                    if let Some(disabled) = item.get("disabled") {
                        existing["disabled"] = disabled.clone();
                    }
                    existing["scope"] = item["scope"].clone();
                }
                Some(existing) => *existing = item,
                None => merged.push(item),
            }
        }
    }
    merged
}

fn save_layer(global: bool, items: &[Value]) -> Result<(), String> {
    home::write_json(&layer_path(global), &json!({ "capabilities": items }))
}

fn is_mcp(config: &Value) -> bool {
    config.get("protocol").and_then(Value::as_str) == Some("mcp")
}

fn log_line(name: &str, text: &str) {
    let dir = home::workspace_data().join("logs");
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join(format!("{name}.log"))) {
        let _ = writeln!(file, "{} {text}", chrono::Local::now().format("%Y-%m-%d %H:%M:%S"));
    }
    if std::env::var_os("EDITH_VERBOSE").is_some() {
        eprintln!("\x1b[2m[{name}] {text}\x1b[0m");
    }
}

/// 启动时把 capabilities.json 里没关掉的都接上（接不上的报原因、跳过；会自己重启的过一会儿再试）
pub fn start_all(report: &mut dyn FnMut(String)) {
    STOPPING.store(false, Ordering::Relaxed);
    for config in configs() {
        if config.get("disabled").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        let name = clean_name(config.get("name").and_then(Value::as_str).unwrap_or(""));
        if let Err(error) = start(&name, &config) {
            report(format!("能力「{name}」接不上：{error}"));
            if !is_mcp(&config) && restarts(&config) {
                schedule_restart(name, Duration::from_secs(2));
            }
        }
    }
}

fn start(name: &str, config: &Value) -> Result<(), String> {
    if name.is_empty() {
        return Err("能力要有名字（name：字母、数字、-、_）".into());
    }
    if is_mcp(config) { mcp::attach(name, config.clone()).map(|_| ()) } else { start_native(name, config).map(|_| ()) }
}

/// 加一个能力：先接上试试，接得上才记进配置（global：全局那层，否则这个目录那层；同名的换掉）。返回接上后的说明
pub fn add(config: Value, global: bool) -> Result<Value, String> {
    let name = name_of(&config);
    let mut stored = config.clone();
    stored["name"] = json!(name);
    stop(&name);
    start(&name, &stored)?;
    let global = global && !single_layer();
    let mut items = read_layer(global);
    items.retain(|item| name_of(item) != name);
    items.push(stored);
    save_layer(global, &items)?;
    Ok(describe(&name).unwrap_or(Value::Null))
}

/// 只记进配置、不接（这个目录的大脑在别的进程里跑着时用；它重启后生效）
pub fn save(config: Value, global: bool) -> Result<(), String> {
    let name = name_of(&config);
    if name.is_empty() {
        return Err("能力要有名字（name：字母、数字、-、_）".into());
    }
    let global = global && !single_layer();
    let mut items = read_layer(global);
    items.retain(|item| name_of(item) != name);
    items.push(config);
    save_layer(global, &items)
}

/// 去掉一个能力（断开并从那一层的配置删掉）；没有这个返回 false
pub fn remove(name: &str, global: bool) -> bool {
    let name = clean_name(name);
    let global = global && !single_layer();
    let mut items = read_layer(global);
    let before = items.len();
    items.retain(|item| name_of(item) != name);
    let removed = items.len() != before;
    if removed {
        let _ = save_layer(global, &items);
    }
    stop(&name) || removed
}

/// 断开一个（不改配置）
fn stop(name: &str) -> bool {
    let native = CAPABILITIES.write().unwrap().remove(name);
    if let Some(capability) = &native {
        capability.shutdown();
    }
    mcp::detach(name) || native.is_some()
}

/// 退出前：通知所有能力、关掉它们
pub fn stop_all() {
    STOPPING.store(true, Ordering::Relaxed);
    let all: Vec<Arc<Capability>> = std::mem::take(&mut *CAPABILITIES.write().unwrap()).into_values().collect();
    for capability in &all {
        capability.shutdown();
    }
    for limb in mcp::all() {
        mcp::detach(&limb.name);
    }
}

fn restarts(config: &Value) -> bool {
    config.get("restart").and_then(Value::as_bool).unwrap_or(true)
}

fn schedule_restart(name: String, delay: Duration) {
    std::thread::spawn(move || {
        let mut delay = delay;
        loop {
            std::thread::sleep(delay);
            if STOPPING.load(Ordering::Relaxed) || CAPABILITIES.read().unwrap().contains_key(&name) {
                return;
            }
            // 按配置里现在的样子起；已经删掉、关掉的不再起
            let current = configs().into_iter().find(|item| clean_name(item.get("name").and_then(Value::as_str).unwrap_or("")) == name);
            let config = match current {
                Some(item) if item.get("disabled").and_then(Value::as_bool) != Some(true) => item,
                _ => return,
            };
            match start_native(&name, &config) {
                Ok(_) => {
                    log_line(&name, "重启好了");
                    return;
                }
                Err(error) => log_line(&name, &format!("重启没成功：{error}")),
            }
            delay = (delay * 2).min(Duration::from_secs(60));
        }
    });
}

fn start_native(name: &str, config: &Value) -> Result<Arc<Capability>, String> {
    let workspace = home::workspace();
    let command =
        config.get("command").and_then(Value::as_str).filter(|command| !command.is_empty()).ok_or("能力要给 command（怎么起它）")?;
    let arguments: Vec<String> = config
        .get("args")
        .and_then(Value::as_array)
        .map(|args| args.iter().filter_map(|arg| arg.as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    let mut process = Command::new(command);
    process.args(&arguments).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).env("EDITH_CAPABILITY", name);
    if let Some(extra) = config.get("env").and_then(Value::as_object) {
        for (key, value) in extra {
            process.env(key, value.as_str().map(str::to_string).unwrap_or_else(|| value.to_string()));
        }
    }
    // 没写 cwd 的在工作目录里跑
    match config.get("cwd").and_then(Value::as_str).filter(|cwd| !cwd.is_empty()) {
        Some(cwd) => process.current_dir(cwd),
        None => process.current_dir(&workspace),
    };
    process.env("EDITH_WORKSPACE", &workspace);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        process.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = process.spawn().map_err(|error| format!("起不了 {command}：{error}"))?;
    let stdin = child.stdin.take().ok_or("拿不到它的标准输入")?;
    let stdout = child.stdout.take().ok_or("拿不到它的标准输出")?;
    if let Some(stderr) = child.stderr.take() {
        let name = name.to_string();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                log_line(&name, &line);
            }
        });
    }
    handshake(name, config, Box::new(stdin), Box::new(stdout), Some(child))
}

/// 经本机套接字连进来的一个（instance.rs 接到连接后调）：握手，名字用它回的 name（重名的加编号）
pub fn accept(reader: Box<dyn Read + Send>, writer: Box<dyn Write + Send>) -> Result<Arc<Capability>, String> {
    handshake("", &json!({ "restart": false }), writer, reader, None)
}

/// 用一个不重名的名字：想要的已经被占了就加 -2、-3……
fn unique_name(wanted: &str) -> String {
    let base = {
        let cleaned = clean_name(wanted);
        if cleaned.is_empty() { "client".to_string() } else { cleaned }
    };
    let taken = |name: &str| CAPABILITIES.read().unwrap().contains_key(name) || mcp::all().iter().any(|limb| limb.name == name);
    if !taken(&base) {
        return base;
    }
    (2..).map(|index| format!("{base}-{index}")).find(|name| !taken(name)).unwrap()
}

/// 握手（EDITH 发 initialize、它回话、EDITH 发 initialized）并登记。name 为空：连进来的，名字按它回的
fn handshake(
    name: &str,
    config: &Value,
    writer: Box<dyn Write + Send>,
    stdout: Box<dyn Read + Send>,
    child: Option<Child>,
) -> Result<Arc<Capability>, String> {
    let connected = child.is_none();
    // 握手前先用一个临时的壳收发（还不知道它报了什么）
    let shell = Arc::new(Capability {
        name: if connected { "connecting".into() } else { name.to_string() },
        config: config.clone(),
        info: Value::Null,
        senses: Vec::new(),
        actions: Vec::new(),
        receives: Vec::new(),
        focus_only: false,
        ready: false,
        endpoints: Vec::new(),
        connected,
        writer: Mutex::new(Some(writer)),
        pending: Mutex::new(HashMap::new()),
        next_id: AtomicU64::new(1),
        alive: AtomicBool::new(true),
        asked: Mutex::new(HashSet::new()),
        acting: Mutex::new(HashMap::new()),
        listener: Mutex::new(None),
        child: Mutex::new(child),
    });
    let current: Arc<Mutex<Weak<Capability>>> = Arc::new(Mutex::new(Arc::downgrade(&shell)));
    {
        let current = current.clone();
        let name = name.to_string();
        std::thread::spawn(move || read_loop(&name, stdout, current));
    }
    let model = crate::home::Model::load().map(|model| model.model).unwrap_or_default();
    let params = json!({
        "protocolVersion": protocol::VERSION,
        "brain": {
            "name": "EDITH",
            "version": env!("CARGO_PKG_VERSION"),
            "model": model,
            "id": crate::instance::brain_id(),
            "workspace": home::workspace().to_string_lossy(),
        },
        "name": name,
        "focus": mind::focus(),
    });
    let reply = match shell.request("initialize", params, HANDSHAKE_TIMEOUT, None) {
        Ok(reply) => reply,
        Err(error) => {
            shell.kill();
            return Err(format!("握手没成功：{}", error_text(&error)));
        }
    };
    let name = if connected { unique_name(reply.get("name").and_then(Value::as_str).unwrap_or("client")) } else { name.to_string() };
    let name = name.as_str();
    let actions: Vec<Value> = reply
        .get("actions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|action| action.get("name").and_then(Value::as_str).is_some_and(|name| !name.is_empty() && clean_name(name) == name))
        .cloned()
        .collect();
    let receives: Vec<String> = reply
        .get("receives")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .flat_map(|kind| if kind == "*" { EVENT_TYPES.iter().map(|kind| kind.to_string()).collect() } else { vec![kind.to_string()] })
        .collect();
    let capability = Arc::new(Capability {
        name: name.to_string(),
        config: config.clone(),
        info: reply.get("capability").cloned().unwrap_or(Value::Null),
        senses: reply.get("senses").and_then(Value::as_array).cloned().unwrap_or_default(),
        actions,
        receives,
        focus_only: reply.get("sessions").and_then(Value::as_str) == Some("focus"),
        ready: true,
        endpoints: reply.get("endpoints").and_then(Value::as_array).cloned().unwrap_or_default(),
        connected,
        writer: Mutex::new(shell.writer.lock().unwrap().take()),
        pending: Mutex::new(std::mem::take(&mut *shell.pending.lock().unwrap())),
        next_id: AtomicU64::new(shell.next_id.load(Ordering::Relaxed)),
        alive: AtomicBool::new(true),
        asked: Mutex::new(HashSet::new()),
        acting: Mutex::new(HashMap::new()),
        listener: Mutex::new(None),
        child: Mutex::new(shell.child.lock().unwrap().take()),
    });
    *current.lock().unwrap() = Arc::downgrade(&capability);
    // 事件：订了的类型、它自己 ask 的那几轮
    let weak = Arc::downgrade(&capability);
    let listener = mind::listen(move |envelope| {
        let Some(capability) = weak.upgrade() else { return };
        capability.deliver(envelope);
    });
    *capability.listener.lock().unwrap() = Some(listener);
    if let Some(old) = CAPABILITIES.write().unwrap().insert(name.to_string(), capability.clone()) {
        old.shutdown();
    }
    capability.notify("initialized", json!({}));
    // 进程结束：摘掉、按配置重启（连进来的在 read_loop 读到断开时摘掉）
    if !connected {
        let capability = capability.clone();
        std::thread::spawn(move || capability.watch());
    }
    crate::instance::refresh();
    log_line(
        name,
        &format!(
            "接上了：{} 个动作，感知 {} 种，订了 {} 种事件",
            capability.actions.len(),
            capability.senses.len(),
            capability.receives.len()
        ),
    );
    Ok(capability)
}

fn error_text(error: &Value) -> String {
    error.get("message").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| error.to_string())
}

/// 读它的标准输出：一行一条
fn read_loop(name: &str, stdout: impl Read, current: Arc<Mutex<Weak<Capability>>>) {
    for line in BufReader::new(stdout).lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let Some(capability) = current.lock().unwrap().upgrade() else { break };
        let message: Value = match serde_json::from_str(&line) {
            Ok(message) => message,
            Err(_) => {
                capability.send(&protocol::error(&Value::Null, protocol::PARSE_ERROR, "不是一行 JSON"));
                log_line(name, &format!("读不懂：{}", line.chars().take(200).collect::<String>()));
                continue;
            }
        };
        match protocol::classify(&message) {
            Message::Response { id, outcome } => {
                let sender = id.as_u64().and_then(|id| capability.pending.lock().unwrap().remove(&id));
                if let Some(sender) = sender {
                    let _ = sender.send(outcome.cloned().map_err(Clone::clone));
                }
            }
            Message::Request { id, method, params } => {
                let (id, method, params) = (id.clone(), method.to_string(), params.clone());
                std::thread::spawn(move || {
                    let reply = match capability.handle(&method, &params) {
                        Ok(result) => protocol::result(&id, result),
                        Err((code, message)) => protocol::error(&id, code, &message),
                    };
                    capability.send(&reply);
                });
            }
            Message::Notification { method, params } => capability.handle_notification(method, params),
            Message::Invalid => {
                capability.send(&protocol::error(&Value::Null, protocol::INVALID_REQUEST, "不是 JSON-RPC 2.0 消息"));
            }
        }
    }
    // 连进来的断开了：摘掉（大脑起的进程由 watch 等它退出）
    let capability = current.lock().unwrap().upgrade();
    if let Some(capability) = capability.filter(|capability| capability.connected) {
        capability.ended();
    }
}

type Failure = (i64, String);

fn invalid(message: impl Into<String>) -> Failure {
    (protocol::INVALID_PARAMS, message.into())
}

impl Capability {
    fn send(&self, message: &Value) -> bool {
        let mut writer = self.writer.lock().unwrap();
        let Some(pipe) = writer.as_mut() else { return false };
        let ok = writeln!(pipe, "{message}").and_then(|_| pipe.flush()).is_ok();
        if !ok {
            *writer = None;
        }
        ok
    }

    fn notify(&self, method: &str, params: Value) {
        self.send(&protocol::notification(method, params));
    }

    /// 发请求等回话（cancel：等的时候被停下就不等了）
    fn request(&self, method: &str, params: Value, timeout: Duration, cancel: Option<&AtomicBool>) -> Result<Value, Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = mpsc::channel();
        self.pending.lock().unwrap().insert(id, sender);
        if !self.send(&protocol::request(id, method, params)) {
            self.pending.lock().unwrap().remove(&id);
            return Err(json!({ "code": protocol::INTERNAL_ERROR, "message": "能力已经断开" }));
        }
        let deadline = Instant::now() + timeout;
        loop {
            match receiver.recv_timeout(Duration::from_millis(100)) {
                Ok(outcome) => return outcome,
                Err(RecvTimeoutError::Disconnected) => return Err(json!({ "code": protocol::INTERNAL_ERROR, "message": "能力已经断开" })),
                Err(RecvTimeoutError::Timeout) => {}
            }
            if cancel.is_some_and(|cancel| cancel.load(Ordering::Relaxed)) {
                self.pending.lock().unwrap().remove(&id);
                return Err(json!({ "code": protocol::CANCELLED, "message": "已停下" }));
            }
            if Instant::now() >= deadline {
                self.pending.lock().unwrap().remove(&id);
                return Err(json!({ "code": protocol::INTERNAL_ERROR, "message": format!("等了 {} 秒没回话", timeout.as_secs()) }));
            }
        }
    }

    fn deliver(&self, envelope: &mind::Envelope) {
        let asked = self.asked.lock().unwrap().contains(&envelope.turn);
        let subscribed = self.receives.iter().any(|kind| kind == envelope.kind) && (!self.focus_only || envelope.session == mind::focus());
        if asked || subscribed {
            self.notify("event", envelope.to_json());
        }
        if asked && envelope.kind == "done" {
            self.asked.lock().unwrap().remove(&envelope.turn);
        }
    }

    fn origin(&self, params: &Value) -> Origin {
        Origin {
            capability: self.name.clone(),
            kind: params.get("kind").and_then(Value::as_str).unwrap_or("").to_string(),
            from: params.get("from").and_then(Value::as_str).unwrap_or("").to_string(),
        }
    }

    fn handle(&self, method: &str, params: &Value) -> Result<Value, Failure> {
        if !self.ready && method != "ping" {
            return Err((protocol::NOT_INITIALIZED, "还没握手：等 EDITH 的 initialize、回了话再发".into()));
        }
        match method {
            "ping" => Ok(json!({})),
            "sense" => self.sense(params),
            "ask" => self.ask(params),
            "sessions.list" => Ok(Value::Array(
                sessions::list()
                    .iter()
                    .map(|session| {
                        let mut summary = session.summary();
                        summary["working"] = json!(mind::working(&session.id).is_some());
                        summary
                    })
                    .collect(),
            )),
            "sessions.create" => {
                let cwd = match params.get("cwd").and_then(Value::as_str) {
                    Some(cwd) => cwd.to_string(),
                    None => focus_cwd(),
                };
                if !std::path::Path::new(&cwd).is_dir() {
                    return Err(invalid(format!("工作目录不存在：{cwd}")));
                }
                let mut session = Session::new(&cwd);
                session.save().map_err(|message| (protocol::INTERNAL_ERROR, message))?;
                if params.get("focus").and_then(Value::as_bool) == Some(true) {
                    mind::set_focus(&session.id);
                }
                Ok(session.summary())
            }
            "sessions.get" => {
                let id = params.get("id").and_then(Value::as_str).ok_or_else(|| invalid("要给 id"))?;
                let session = Session::find(id).ok_or((protocol::NO_SESSION, format!("没有会话 {id}")))?;
                let mut value = session.to_json();
                value["working"] = json!(mind::working(&session.id));
                value["focus"] = json!(session.id == mind::focus());
                Ok(value)
            }
            "sessions.focus" => {
                let id = params.get("id").and_then(Value::as_str).ok_or_else(|| invalid("要给 id"))?;
                let session = Session::find(id).ok_or((protocol::NO_SESSION, format!("没有会话 {id}")))?;
                mind::set_focus(&session.id);
                Ok(json!({ "focus": session.id }))
            }
            "sessions.stop" => {
                let id = params.get("id").and_then(Value::as_str).map(str::to_string).unwrap_or_else(mind::focus);
                Ok(json!({ "stopping": mind::stop(&id) }))
            }
            "initialize" => Err((protocol::INVALID_REQUEST, "握手由 EDITH 发起：等 EDITH 的 initialize，回话就行".into())),
            other => Err((protocol::METHOD_NOT_FOUND, format!("没有这个方法：{other}"))),
        }
    }

    /// sense / ask 的共同部分：会话、急缓、文字
    fn prepare(&self, params: &Value) -> Result<(String, String, Urgency), Failure> {
        home::Model::load().map_err(|message| (protocol::NO_MODEL, message))?;
        let text = params
            .get("text")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .ok_or_else(|| invalid("要给 text"))?;
        let urgency = match params.get("urgency").and_then(Value::as_str) {
            None => Urgency::Queue,
            Some(value) => Urgency::parse(value)
                .ok_or_else(|| invalid(format!("urgency 只能是 interrupt / interject / queue / note，不是 {value}")))?,
        };
        let session = match params.get("session").and_then(Value::as_str) {
            None | Some("focus") => {
                let focus = mind::focus();
                if focus.is_empty() {
                    return Err((protocol::NO_SESSION, "还没有焦点会话：给 session，或先 sessions.create".into()));
                }
                focus
            }
            Some("new") => {
                let mut session = Session::new(&focus_cwd());
                session.save().map_err(|message| (protocol::INTERNAL_ERROR, message))?;
                session.id
            }
            Some(id) => Session::find(id).map(|session| session.id).ok_or((protocol::NO_SESSION, format!("没有会话 {id}")))?,
        };
        Ok((session, text.to_string(), urgency))
    }

    fn sense(&self, params: &Value) -> Result<Value, Failure> {
        let (session, text, urgency) = self.prepare(params)?;
        let (disposition, turn) = mind::submit(&session, &text, self.origin(params), urgency, None).map_err(submit_failure)?;
        Ok(json!({ "session": session, "disposition": disposition.name(), "turn": turn }))
    }

    fn ask(&self, params: &Value) -> Result<Value, Failure> {
        let (session, text, urgency) = self.prepare(params)?;
        if urgency == Urgency::Note {
            return Err(invalid("ask 要等回答：urgency 不能是 note（只记下用 sense）"));
        }
        let turn = mind::new_turn_id();
        self.asked.lock().unwrap().insert(turn.clone());
        let outcome = mind::ask(&session, &text, self.origin(params), urgency, Some(turn.clone()));
        let (disposition, actual, done) = match outcome {
            Ok(outcome) => outcome,
            Err(message) => {
                self.asked.lock().unwrap().remove(&turn);
                return Err(submit_failure(message));
            }
        };
        if let Some(actual) = actual.as_ref().filter(|actual| **actual != turn) {
            // 补充进了正在干的那一轮：那轮的事件也推给它
            self.asked.lock().unwrap().insert(actual.clone());
        }
        self.asked.lock().unwrap().remove(&turn);
        if disposition == Disposition::Interjected {
            // 等的那一轮结束后它的事件不用再推
            if let Some(actual) = &actual {
                self.asked.lock().unwrap().remove(actual);
            }
        }
        Ok(json!({
            "session": session,
            "turn": actual,
            "disposition": disposition.name(),
            "answer": done["answer"],
            "stopped": done["stopped"],
            "error": done["error"],
        }))
    }

    fn handle_notification(&self, method: &str, params: &Value) {
        if !self.ready {
            log_line(&self.name, &format!("握手前的通知不理：{method}"));
            return;
        }
        match method {
            "sense" => {
                if let Err((_, message)) = self.sense(params) {
                    log_line(&self.name, &format!("sense 没交上：{message}"));
                }
            }
            "act.progress" => {
                let call = params.get("call").and_then(Value::as_str).unwrap_or("");
                let Some((session, turn)) = self.acting.lock().unwrap().get(call).cloned() else { return };
                let mut text = params.get("text").and_then(Value::as_str).unwrap_or("").to_string();
                if let Some(fraction) = params.get("fraction").and_then(Value::as_f64) {
                    text = format!("{text}（{:.0}%）", (fraction * 100.0).clamp(0.0, 100.0));
                }
                mind::emit(&session, &turn, "notice", json!({ "text": format!("能力「{}」：{}", self.name, text.trim()), "call": call }));
            }
            "log" => {
                let level = params.get("level").and_then(Value::as_str).unwrap_or("info");
                log_line(&self.name, &format!("[{level}] {}", params.get("text").and_then(Value::as_str).unwrap_or("")));
            }
            _ => log_line(&self.name, &format!("不认识的通知：{method}")),
        }
    }

    /// 让它做一个动作
    fn act(&self, action: &Value, arguments: &Map<String, Value>, context: &Context, cancel: &AtomicBool) -> Outcome {
        let name = action.get("name").and_then(Value::as_str).unwrap_or("");
        let timeout = action.get("timeoutSeconds").and_then(Value::as_u64).unwrap_or(DEFAULT_ACTION_TIMEOUT);
        let call = format!("c{}", self.next_id.fetch_add(1, Ordering::Relaxed));
        self.acting.lock().unwrap().insert(call.clone(), (context.session.clone(), context.turn.clone()));
        let params = json!({ "call": call, "action": name, "arguments": arguments, "session": context.session, "turn": context.turn });
        let outcome = self.request("act", params, Duration::from_secs(timeout), Some(cancel));
        self.acting.lock().unwrap().remove(&call);
        match outcome {
            Ok(result) => {
                let ok = result.get("ok").and_then(Value::as_bool).unwrap_or(true);
                let mut text = match result.get("text") {
                    Some(Value::String(text)) => text.clone(),
                    Some(other) => other.to_string(),
                    None => String::new(),
                };
                for item in result.get("media").and_then(Value::as_array).into_iter().flatten() {
                    if let Some(path) = item.get("path").or(item.get("url")).and_then(Value::as_str) {
                        text.push_str(&format!("\n文件：{path}"));
                    }
                }
                let text = if text.trim().is_empty() {
                    "（完成，没有内容）".to_string()
                } else {
                    friday_core::tool_files::truncate(&text)
                };
                Outcome { ok, text }
            }
            Err(error) => {
                let code = error.get("code").and_then(Value::as_i64);
                if code == Some(protocol::CANCELLED) {
                    self.notify("act.cancel", json!({ "call": call }));
                }
                if code == Some(protocol::BUSY) {
                    // 共享设备被别的大脑占着：告诉模型是谁、怎么办
                    let holder = error["data"]["holder"]["workspace"].as_str().unwrap_or("另一个目录");
                    return Outcome {
                        ok: false,
                        text: format!(
                            "能力「{}」设备忙：正被 {holder} 的大脑占着（{}）。等它用完再试，或者先做别的、换个办法。",
                            self.name,
                            error_text(&error)
                        ),
                    };
                }
                Outcome { ok: false, text: format!("能力「{}」：{}", self.name, error_text(&error)) }
            }
        }
    }

    fn kill(&self) {
        if let Some(mut child) = self.child.lock().unwrap().take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    /// 让它退出：发 shutdown、关 stdin，3 秒没退就结束进程
    fn shutdown(&self) {
        self.alive.store(false, Ordering::Relaxed);
        if let Some(id) = self.listener.lock().unwrap().take() {
            mind::unlisten(id);
        }
        self.notify("shutdown", json!({}));
        // 关掉写的一头：进程读到 EOF；连进来的连接整个关掉（SocketWriter 丢掉时关）
        *self.writer.lock().unwrap() = None;
        let child = self.child.lock().unwrap().take();
        if let Some(mut child) = child {
            let deadline = Instant::now() + Duration::from_secs(3);
            while Instant::now() < deadline {
                if let Ok(Some(_)) = child.try_wait() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    /// 不在了（进程退出、连接断开）：摘掉它的动作和事件，等着的请求立刻失败
    fn ended(self: Arc<Self>) {
        let was_alive = self.alive.swap(false, Ordering::Relaxed);
        if let Some(id) = self.listener.lock().unwrap().take() {
            mind::unlisten(id);
        }
        self.pending.lock().unwrap().clear();
        *self.writer.lock().unwrap() = None;
        let mut all = CAPABILITIES.write().unwrap();
        if all.get(&self.name).is_some_and(|current| Arc::ptr_eq(current, &self)) {
            all.remove(&self.name);
        }
        drop(all);
        if was_alive && self.connected {
            log_line(&self.name, "断开了");
        }
        crate::instance::refresh();
    }

    /// 等进程结束：摘掉它，等着的请求立刻失败；按配置重启
    fn watch(self: Arc<Self>) {
        let started = Instant::now();
        loop {
            std::thread::sleep(Duration::from_millis(200));
            let exited = {
                let mut child = self.child.lock().unwrap();
                match child.as_mut() {
                    None => true,
                    Some(process) => matches!(process.try_wait(), Ok(Some(_)) | Err(_)),
                }
            };
            if exited {
                break;
            }
        }
        let was_alive = self.alive.load(Ordering::Relaxed);
        self.clone().ended();
        if !was_alive || STOPPING.load(Ordering::Relaxed) {
            return;
        }
        log_line(&self.name, "进程退出了");
        if restarts(&self.config) {
            let delay = if started.elapsed() > Duration::from_secs(60) { Duration::from_secs(1) } else { Duration::from_secs(5) };
            schedule_restart(self.name.clone(), delay);
        }
    }
}

fn submit_failure(message: String) -> Failure {
    let code = if message.starts_with("没有会话") { protocol::NO_SESSION } else { protocol::INVALID_PARAMS };
    (code, message)
}

/// 新会话用焦点会话的工作目录（没有焦点用家目录）
fn focus_cwd() -> String {
    Session::load(&mind::focus()).map(|session| session.cwd).unwrap_or_else(|| home::user_home().to_string_lossy().into_owned())
}

fn native() -> Vec<Arc<Capability>> {
    CAPABILITIES.read().unwrap().values().cloned().collect()
}

/// 能力的动作交给模型的工具定义：名字「<能力名>__<动作名>」
pub fn specs() -> Vec<Value> {
    let mut specs = Vec::new();
    for capability in native() {
        for action in &capability.actions {
            let name = action["name"].as_str().unwrap_or("");
            let description = action.get("description").and_then(Value::as_str).unwrap_or("");
            specs.push(json!({
                "type": "function",
                "function": {
                    "name": format!("{}{}{name}", capability.name, mcp::SEPARATOR).chars().take(64).collect::<String>(),
                    "description": format!("（能力「{}」）{description}", capability.name),
                    "parameters": action.get("parameters").cloned().unwrap_or_else(|| json!({ "type": "object", "properties": {} })),
                },
            }));
        }
    }
    specs
}

/// 给提示词看的：接着哪些能力、能做什么、能感知什么
pub fn summary() -> String {
    native()
        .iter()
        .map(|capability| {
            let actions: Vec<&str> = capability.actions.iter().filter_map(|action| action["name"].as_str()).collect();
            let senses: Vec<&str> = capability.senses.iter().filter_map(|sense| sense.get("kind").and_then(Value::as_str)).collect();
            let title = capability.info.get("title").and_then(Value::as_str).map(|title| format!("（{title}）")).unwrap_or_default();
            let mut parts = Vec::new();
            if !actions.is_empty() {
                parts.push(format!("动作 {}", actions.join("、")));
            }
            if !senses.is_empty() {
                parts.push(format!("会主动传来 {}", senses.join("、")));
            }
            format!("- {}{title}：{}", capability.name, if parts.is_empty() { "只看事件".into() } else { parts.join("；") })
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// 调能力的动作（名字不是它们的返回 None）
pub fn call(full_name: &str, arguments: &Map<String, Value>, context: &Context, cancel: &AtomicBool) -> Option<Outcome> {
    let (name, rest) = full_name.split_once(mcp::SEPARATOR)?;
    let capability = CAPABILITIES.read().unwrap().get(name).cloned()?;
    let action = capability
        .actions
        .iter()
        .find(|action| {
            action["name"].as_str().is_some_and(|action| action == rest || (full_name.chars().count() == 64 && action.starts_with(rest)))
        })?
        .clone();
    Some(capability.act(&action, arguments, context, cancel))
}

/// 一个能力现在的样子（命令行 edith capabilities 列出来的）
pub fn describe(name: &str) -> Option<Value> {
    if let Some(capability) = CAPABILITIES.read().unwrap().get(name) {
        return Some(json!({
            "name": capability.name,
            "protocol": "edith",
            "info": capability.info,
            "senses": capability.senses,
            "actions": capability.actions.iter().map(|action| action["name"].clone()).collect::<Vec<_>>(),
            "receives": capability.receives,
        }));
    }
    mcp::all().into_iter().find(|limb| limb.name == name).map(|limb| {
        json!({
            "name": limb.name,
            "protocol": "mcp",
            "actions": limb.tools.iter().map(|tool| tool["name"].clone()).collect::<Vec<_>>(),
        })
    })
}

/// 各个能力对外开的入口（登记进 instances/）：[{capability, kind, url, …}]
pub fn endpoints() -> Vec<Value> {
    let mut all = Vec::new();
    for capability in native() {
        for endpoint in &capability.endpoints {
            let mut entry = endpoint.clone();
            if entry.is_object() {
                entry["capability"] = json!(capability.name);
                all.push(entry);
            }
        }
    }
    all
}

/// 接着的能力名（登记用）
pub fn names() -> Vec<String> {
    let mut names: Vec<String> = native().iter().map(|capability| capability.name.clone()).collect();
    names.extend(mcp::all().iter().map(|limb| limb.name.clone()));
    names
}

//! 能力：大脑的四肢，讲能力协议（docs/CAPABILITY-PROTOCOL.md）：双向、实时，能力能主动推内容（sense）、交事（ask），
//! 大脑能让它做动作（act）、把干活的过程推给它（event）。
//!
//! 大脑只有一个接口：本机套接字（instance.rs）。**所有能力都从它连进来，都是一级能力**：
//! - 配置里的：大脑起它的程序（给 EDITH_ACCESS / 它专用的 EDITH_ACCESS_TOKEN / EDITH_CAPABILITY），程序自己连进来，
//!   名字就是配置里的（令牌定的，别人抢不走）；它那条连接断了，过一会儿再起它（restart 不是 false 时）；
//! - 它替别人开的（edith-websocket 替远程设备）：同一个令牌加 `as`，名字 `<能力>-<as>`，权限收窄（permissions.relay）；
//! - 自己打开的：终端（edith attach）、桌面端……拿登记里的主令牌，用它回的名字（配置里的名字不给），断开就摘掉。
//!
//! 每条连接能做什么由令牌定（permissions.rs），越权的回 NOT_PERMITTED；它交来的话那一轮只能用它许可的工具。
//!
//! 共用的工具（WebSocket 中转、共享设备、MCP 桥）在 integemjack/edith.tools，本机没有就下载（downloads.rs）。
//!
//! 配置三层（同名的后面盖前面）：全局 `~/.edith/capabilities.json` → 项目 `<工作目录>/.edith/capabilities.json`（提交进仓库）
//! → 本地 `<工作目录>/.edith/capabilities.local.json`（只给自己）：
//! `{"capabilities": [{name, command, args?, env?, cwd?, disabled?, restart?, permissions?}]}`，
//! permissions：`{sense?, ask?, sessions?: "none"|"read"|"all", events?, tools?: "all"|"read"|"none"|[名字…], relay?: {同样几项}}`。

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
use crate::home;
use crate::mind::{self, Disposition, Origin, Urgency};
use crate::permissions::{self, Authority, Grant, Sessions};
use crate::protocol::{self, Message};
use crate::sessions::{self, Session};
use crate::tools::Outcome;

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
/// 起了程序后等它连进来多久
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_ACTION_TIMEOUT: u64 = 600;
/// 动作的工具名：<能力名>__<动作名>
pub const SEPARATOR: &str = "__";
/// 能被订阅的事件
pub const EVENT_TYPES: &[&str] =
    &["turn.started", "input", "text", "thinking", "tool", "result", "stats", "notice", "done", "agent", "agent.event"];

/// 一个连着的能力
pub struct Capability {
    pub name: String,
    /// initialize 回的 capability（title、version、description）
    pub info: Value,
    pub senses: Vec<Value>,
    /// {name, description, parameters, timeoutSeconds?, exclusive?, holds?, releases?}
    pub actions: Vec<Value>,
    pub receives: Vec<String>,
    /// 这条连接能做什么（令牌定的）
    pub grant: Grant,
    /// 它能批准（initialize 回了 approves: true；只有全权的才问它）
    approves: bool,
    /// 只要焦点会话的事件（sessions: "focus"）
    focus_only: bool,
    /// 握手做完了（之前它发来的请求一律回 NOT_INITIALIZED）
    ready: bool,
    /// 它对外开的入口（initialize 回的 endpoints：WebSocket 地址等），登记进 instances/ 让别人找得到
    pub endpoints: Vec<Value>,
    writer: Mutex<Option<Box<dyn Write + Send>>>,
    pending: Mutex<HashMap<u64, mpsc::Sender<Result<Value, Value>>>>,
    next_id: AtomicU64,
    alive: AtomicBool,
    /// 它 ask 的那几轮：这几轮的事件不管订没订都推给它
    asked: Mutex<HashSet<String>>,
    /// 正在做的动作：call → (会话, 轮)（进度推到那一轮）
    acting: Mutex<HashMap<String, (String, String)>>,
    listener: Mutex<Option<u64>>,
}

static CAPABILITIES: LazyLock<RwLock<BTreeMap<String, Arc<Capability>>>> = LazyLock::new(|| RwLock::new(BTreeMap::new()));
/// 大脑按配置起的程序（能力名 → 进程）：退出、去掉能力时结束它
static LAUNCHED: LazyLock<Mutex<HashMap<String, Child>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
static STOPPING: AtomicBool = AtomicBool::new(false);
/// 连接断了时请求的错误说法（批准时据此改问别人 / 等下一个连上来）
const GONE: &str = "能力已经断开";

/// 能力名只留字母、数字、-、_（动作的工具名要合 OpenAI 的规矩）
pub fn clean_name(name: &str) -> String {
    let cleaned: String = name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
    cleaned.trim_matches('_').chars().take(24).collect()
}

/// 配置在哪一层
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layer {
    Global,
    Project,
    Local,
}

impl Layer {
    pub fn title(self) -> &'static str {
        match self {
            Layer::Global => "全局",
            Layer::Project => "项目",
            Layer::Local => "本地",
        }
    }

    fn key(self) -> &'static str {
        match self {
            Layer::Global => "global",
            Layer::Project => "project",
            Layer::Local => "local",
        }
    }
}

fn layer_path(layer: Layer) -> PathBuf {
    match layer {
        Layer::Global => home::dir().join("capabilities.json"),
        Layer::Project => home::workspace_data().join("capabilities.json"),
        Layer::Local => home::workspace_data().join("capabilities.local.json"),
    }
}

/// 用到的层（工作目录就是家目录时项目层就是全局那个文件，只算一次）；项目、本地两层要这个目录的配置被信任（trust.rs）
fn layers() -> Vec<Layer> {
    let canonical = |path: PathBuf| std::fs::canonicalize(&path).unwrap_or(path);
    let mut layers = vec![Layer::Global];
    if crate::trust::allowed() {
        if canonical(home::dir()) != canonical(home::workspace_data()) {
            layers.push(Layer::Project);
        }
        layers.push(Layer::Local);
    }
    layers
}

fn read_layer(layer: Layer) -> Vec<Value> {
    home::read_json(&layer_path(layer)).and_then(|value| value.get("capabilities").and_then(Value::as_array).cloned()).unwrap_or_default()
}

fn name_of(config: &Value) -> String {
    clean_name(config.get("name").and_then(Value::as_str).unwrap_or(""))
}

/// 合并后的配置：全局 → 项目 → 本地，同名的后面盖前面（只写 `{"name": "x", "disabled": true}` 就是关掉前面那层的 x）；
/// 每项多一个 "scope"：global / project / local
pub fn configs() -> Vec<Value> {
    let mut merged: Vec<Value> = Vec::new();
    for layer in layers() {
        for mut item in read_layer(layer) {
            item["scope"] = json!(layer.key());
            let name = name_of(&item);
            match merged.iter_mut().find(|existing| name_of(existing) == name) {
                // 只写了 disabled 之类的：改前面那层的，别的照旧
                Some(existing) if item.get("command").is_none() => {
                    for (key, value) in item.as_object().unwrap() {
                        existing[key] = value.clone();
                    }
                }
                Some(existing) => *existing = item,
                None => merged.push(item),
            }
        }
    }
    merged
}

/// 配置里开着的这一项
fn enabled_config(name: &str) -> Option<Value> {
    configs().into_iter().find(|item| name_of(item) == name && item.get("disabled").and_then(Value::as_bool) != Some(true))
}

fn save_layer(layer: Layer, items: &[Value]) -> Result<(), String> {
    home::write_json(&layer_path(layer), &json!({ "capabilities": items }))
}

/// 一层原样的配置（不管信任；升级迁移用）
pub fn raw_layer(layer: Layer) -> Vec<Value> {
    read_layer(layer)
}

/// 原样写回一层（升级迁移用）
pub fn write_raw_layer(layer: Layer, items: &[Value]) -> Result<(), String> {
    save_layer(layer, items)
}

/// 能力的日志：`<项目数据>/logs/<名字>.log`
pub fn log_file(name: &str) -> PathBuf {
    home::project_dir().join("logs").join(format!("{name}.log"))
}

fn log_line(name: &str, text: &str) {
    let path = log_file(name);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(file, "{} {text}", chrono::Local::now().format("%Y-%m-%d %H:%M:%S"));
    }
    if std::env::var_os("EDITH_VERBOSE").is_some() {
        eprintln!("\x1b[2m[{name}] {text}\x1b[0m");
    }
}

/// 启动时把配置里没关掉的都起来、等它们连进来（接不上的报原因、跳过；会自己重启的过一会儿再试）
pub fn start_all(report: &mut dyn FnMut(String)) {
    STOPPING.store(false, Ordering::Relaxed);
    let wanted: Vec<Value> = configs().into_iter().filter(|config| config.get("disabled").and_then(Value::as_bool) != Some(true)).collect();
    // 一起起、一起等（每个最多 30 秒）
    let (sender, results) = mpsc::channel();
    for config in &wanted {
        let sender = sender.clone();
        let config = config.clone();
        std::thread::spawn(move || {
            let name = name_of(&config);
            let _ = sender.send((name.clone(), config.clone(), launch(&name, &config)));
        });
    }
    drop(sender);
    for (name, config, outcome) in results {
        if let Err(error) = outcome {
            report(format!("能力「{name}」接不上：{error}"));
            if restarts(&config) {
                schedule_restart(name, Duration::from_secs(2));
            }
        }
    }
}

/// 加一个能力：先起来试试，连得进来才记进那一层的配置（同名的换掉）。返回接上后的说明
pub fn add(config: Value, layer: Layer) -> Result<Value, String> {
    let name = name_of(&config);
    let mut stored = config.clone();
    stored["name"] = json!(name);
    stop(&name);
    launch(&name, &stored)?;
    save(stored, layer)?;
    Ok(describe(&name).unwrap_or(Value::Null))
}

/// 只记进配置、不起（这个目录的大脑在别的进程里跑着时用；它重启后生效）
pub fn save(config: Value, layer: Layer) -> Result<(), String> {
    let name = name_of(&config);
    if name.is_empty() {
        return Err("能力要有名字（name：字母、数字、-、_）".into());
    }
    let was_trusted = crate::trust::trusted();
    let mut items = read_layer(layer);
    items.retain(|item| name_of(item) != name);
    items.push(config);
    save_layer(layer, &items)?;
    crate::trust::after_own_write(was_trusted);
    Ok(())
}

/// 去掉一个能力（断开、结束它的程序，并从那一层的配置删掉）；没有这个返回 false
pub fn remove(name: &str, layer: Layer) -> bool {
    let name = clean_name(name);
    let mut items = read_layer(layer);
    let before = items.len();
    items.retain(|item| name_of(item) != name);
    let removed = items.len() != before;
    if removed {
        let was_trusted = crate::trust::trusted();
        let _ = save_layer(layer, &items);
        crate::trust::after_own_write(was_trusted);
    }
    let stopped = stop(&name);
    if enabled_config(&name).is_none() {
        permissions::revoke(&name);
    }
    stopped || removed
}

/// 断开一个、结束大脑起的那个程序（不改配置）
fn stop(name: &str) -> bool {
    let connected = CAPABILITIES.write().unwrap().remove(name);
    if let Some(capability) = &connected {
        capability.shutdown();
    }
    let launched = LAUNCHED.lock().unwrap().remove(name);
    if let Some(child) = launched {
        finish(vec![child]);
    }
    connected.is_some()
}

/// 等进程自己退（最多 3 秒），不退的结束掉
fn finish(mut children: Vec<Child>) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline && children.iter_mut().any(|child| matches!(child.try_wait(), Ok(None))) {
        std::thread::sleep(Duration::from_millis(50));
    }
    for mut child in children {
        if matches!(child.try_wait(), Ok(None)) {
            let _ = child.kill();
        }
        let _ = child.wait();
    }
}

/// 退出前：通知所有能力、断开，结束大脑起的程序
pub fn stop_all() {
    STOPPING.store(true, Ordering::Relaxed);
    let all: Vec<Arc<Capability>> = std::mem::take(&mut *CAPABILITIES.write().unwrap()).into_values().collect();
    for capability in &all {
        capability.shutdown();
    }
    let children: Vec<Child> = LAUNCHED.lock().unwrap().drain().map(|(_, child)| child).collect();
    finish(children);
    crate::mcp::stop_all();
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
            let Some(config) = enabled_config(&name) else { return };
            match launch(&name, &config) {
                Ok(()) => {
                    log_line(&name, "重新连上了");
                    return;
                }
                Err(error) => log_line(&name, &format!("重起没成功：{error}")),
            }
            delay = (delay * 2).min(Duration::from_secs(60));
        }
    });
}

/// 起配置里的程序，等它用这个名字连进来（最多 30 秒）。程序可以一直开着（它就是那个能力），也可以让别人替它连进来后自己退出
fn launch(name: &str, config: &Value) -> Result<(), String> {
    if name.is_empty() {
        return Err("能力要有名字（name：字母、数字、-、_）".into());
    }
    let (address, _) = crate::instance::access().ok_or("大脑的套接字还没开")?;
    let token = permissions::token_for(name, config);
    let command =
        config.get("command").and_then(Value::as_str).filter(|command| !command.is_empty()).ok_or("能力要给 command（怎么起它）")?;
    let program = crate::downloads::resolve(command)?;
    let arguments: Vec<String> = config
        .get("args")
        .and_then(Value::as_array)
        .map(|args| args.iter().filter_map(|arg| arg.as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    let workspace = home::workspace();
    let mut process = Command::new(&program);
    process
        .args(&arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("EDITH_ACCESS", &address)
        .env("EDITH_ACCESS_TOKEN", &token)
        .env("EDITH_CAPABILITY", name)
        .env("EDITH_WORKSPACE", &workspace);
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
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        process.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = process.spawn().map_err(|error| format!("起不了 {command}：{error}"))?;
    // 它的输出都记进日志
    for pipe in [
        child.stdout.take().map(|pipe| Box::new(pipe) as Box<dyn Read + Send>),
        child.stderr.take().map(|pipe| Box::new(pipe) as Box<dyn Read + Send>),
    ]
    .into_iter()
    .flatten()
    {
        let name = name.to_string();
        std::thread::spawn(move || {
            for line in BufReader::new(pipe).lines().map_while(Result::ok) {
                log_line(&name, &line);
            }
        });
    }
    let deadline = Instant::now() + LAUNCH_TIMEOUT;
    let mut exited: Option<std::process::ExitStatus> = None;
    loop {
        if CAPABILITIES.read().unwrap().contains_key(name) {
            break;
        }
        if exited.is_none()
            && let Ok(Some(status)) = child.try_wait()
        {
            // 退出码不是 0：起不来；是 0：多半是让别人替它连（启动器），接着等
            if !status.success() {
                return Err(format!("{command} 退出了（{status}），没连进来（日志在 {}）", log_file(name).display()));
            }
            exited = Some(status);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!(
                "{command} 起来了，但 {} 秒内没用「{name}」这个名字连进来（日志在 {}）",
                LAUNCH_TIMEOUT.as_secs(),
                log_file(name).display()
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    if exited.is_none() {
        let old = LAUNCHED.lock().unwrap().insert(name.to_string(), child);
        if let Some(old) = old {
            finish(vec![old]);
        }
    }
    Ok(())
}

/// 连进来的一个（instance.rs 接到、认出令牌后调）：握手。名字令牌定了的用它（配置里的能力已经连着就不收），
/// 主令牌的用它回的 name（重名的、和配置里的能力重名的加编号）
pub fn accept(reader: Box<dyn Read + Send>, mut writer: Box<dyn Write + Send>, authority: Authority) -> Result<Arc<Capability>, String> {
    if let Some(name) = &authority.name
        && !authority.relayed
        && CAPABILITIES.read().unwrap().contains_key(name)
    {
        let refusal = protocol::error(
            &Value::Null,
            protocol::NOT_PERMITTED,
            &format!("「{name}」已经连着：一个令牌只能有一条自己的连接（替别人连用 auth 的 as）"),
        );
        let _ = writeln!(writer, "{refusal}");
        return Err(format!("「{name}」已经连着"));
    }
    handshake(writer, reader, authority)
}

/// 用一个不重名的名字：想要的已经被占了（连着的、主令牌的连接还不能用配置里的名字）就加 -2、-3……
fn unique_name(wanted: &str, reserve_configured: bool) -> String {
    let base = {
        let cleaned = clean_name(wanted);
        if cleaned.is_empty() { "client".to_string() } else { cleaned }
    };
    let configured: HashSet<String> = if reserve_configured { configs().iter().map(name_of).collect() } else { HashSet::new() };
    let taken = |name: &str| CAPABILITIES.read().unwrap().contains_key(name) || configured.contains(name);
    if !taken(&base) {
        return base;
    }
    (2..).map(|index| format!("{base}-{index}")).find(|name| !taken(name)).unwrap()
}

/// 握手（EDITH 发 initialize、它回话、EDITH 发 initialized）并登记
fn handshake(writer: Box<dyn Write + Send>, reader: Box<dyn Read + Send>, authority: Authority) -> Result<Arc<Capability>, String> {
    // 握手前先用一个临时的壳收发（还不知道它报了什么）
    let shell = Arc::new(Capability {
        name: "connecting".into(),
        info: Value::Null,
        senses: Vec::new(),
        actions: Vec::new(),
        receives: Vec::new(),
        grant: authority.grant.clone(),
        approves: false,
        focus_only: false,
        ready: false,
        endpoints: Vec::new(),
        writer: Mutex::new(Some(writer)),
        pending: Mutex::new(HashMap::new()),
        next_id: AtomicU64::new(1),
        alive: AtomicBool::new(true),
        asked: Mutex::new(HashSet::new()),
        acting: Mutex::new(HashMap::new()),
        listener: Mutex::new(None),
    });
    let current: Arc<Mutex<Weak<Capability>>> = Arc::new(Mutex::new(Arc::downgrade(&shell)));
    {
        let current = current.clone();
        std::thread::spawn(move || read_loop(reader, current));
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
        "name": authority.name.clone().unwrap_or_default(),
        "focus": mind::focus(),
        "grant": authority.grant.to_json(),
    });
    let reply = match shell.request("initialize", params, HANDSHAKE_TIMEOUT, None) {
        Ok(reply) => reply,
        Err(error) => {
            *shell.writer.lock().unwrap() = None;
            return Err(format!("握手没成功：{}", error_text(&error)));
        }
    };
    let name = match &authority.name {
        // 中转的子连接可能重名（同一台设备连了两次）：加编号
        Some(fixed) if authority.relayed => unique_name(fixed, false),
        Some(fixed) => fixed.clone(),
        None => unique_name(reply.get("name").and_then(Value::as_str).unwrap_or("client"), true),
    };
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
        info: reply.get("capability").cloned().unwrap_or(Value::Null),
        senses: reply.get("senses").and_then(Value::as_array).cloned().unwrap_or_default(),
        actions,
        receives,
        grant: authority.grant,
        approves: reply.get("approves").and_then(Value::as_bool) == Some(true),
        focus_only: reply.get("sessions").and_then(Value::as_str) == Some("focus"),
        ready: true,
        endpoints: reply.get("endpoints").and_then(Value::as_array).cloned().unwrap_or_default(),
        writer: Mutex::new(shell.writer.lock().unwrap().take()),
        pending: Mutex::new(std::mem::take(&mut *shell.pending.lock().unwrap())),
        next_id: AtomicU64::new(shell.next_id.load(Ordering::Relaxed)),
        alive: AtomicBool::new(true),
        asked: Mutex::new(HashSet::new()),
        acting: Mutex::new(HashMap::new()),
        listener: Mutex::new(None),
    });
    *current.lock().unwrap() = Arc::downgrade(&capability);
    // 事件：订了的类型、它自己 ask 的那几轮
    let weak = Arc::downgrade(&capability);
    let listener = mind::listen(move |envelope| {
        let Some(capability) = weak.upgrade() else { return };
        capability.deliver(envelope);
    });
    *capability.listener.lock().unwrap() = Some(listener);
    CAPABILITIES.write().unwrap().insert(name.to_string(), capability.clone());
    capability.notify("initialized", json!({}));
    crate::instance::refresh();
    log_line(
        name,
        &format!(
            "连上了：{} 个动作，感知 {} 种，订了 {} 种事件",
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
fn read_loop(reader: Box<dyn Read + Send>, current: Arc<Mutex<Weak<Capability>>>) {
    for line in BufReader::new(reader).lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let Some(capability) = current.lock().unwrap().upgrade() else { break };
        let message: Value = match serde_json::from_str(&line) {
            Ok(message) => message,
            Err(_) => {
                capability.send(&protocol::error(&Value::Null, protocol::PARSE_ERROR, "不是一行 JSON"));
                log_line(&capability.name, &format!("读不懂：{}", line.chars().take(200).collect::<String>()));
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
    // 断开了：摘掉
    let capability = current.lock().unwrap().upgrade();
    if let Some(capability) = capability {
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
            return Err(json!({ "code": protocol::INTERNAL_ERROR, "message": GONE }));
        }
        let deadline = Instant::now() + timeout;
        loop {
            match receiver.recv_timeout(Duration::from_millis(100)) {
                Ok(outcome) => return outcome,
                Err(RecvTimeoutError::Disconnected) => return Err(json!({ "code": protocol::INTERNAL_ERROR, "message": GONE })),
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
        let subscribed = self.grant.events
            && self.receives.iter().any(|kind| kind == envelope.kind)
            && (!self.focus_only || envelope.session == mind::focus())
            && self.owns(&envelope.session);
        if asked || subscribed {
            self.notify("event", envelope.to_json());
        }
        if asked && envelope.kind == "done" {
            self.asked.lock().unwrap().remove(&envelope.turn);
        }
    }

    fn origin(&self, params: &Value) -> Origin {
        // user：用户自己打的字（Friday 客户端、edith attach），交给模型时不包「由能力传来」——只有全权的连接能这样说，
        // 客人说 user 照样当能力传来的；agent 是子智能体交回的专用说法，能力不能冒用
        let kind = match params.get("kind").and_then(Value::as_str).unwrap_or("") {
            "user" if !self.full() => "message",
            "agent" => "message",
            kind => kind,
        };
        Origin {
            capability: self.name.clone(),
            kind: kind.to_string(),
            from: params.get("from").and_then(Value::as_str).unwrap_or("").to_string(),
            tools: self.grant.tools.clone(),
        }
    }

    /// 权限全开（主人、没收窄的能力）：和终端共用会话。不全的是「客人」：只能用自己名下的会话、只收自己会话的事件——
    /// 不然它交来的话进了主人的会话历史，之后全权的那一轮照样看得到（借权注入），它也能看主人的全部对话
    fn full(&self) -> bool {
        self.grant.tools == permissions::Tools::All && self.grant.sessions == Sessions::All
    }

    /// 客人能不能碰这个会话
    fn owns(&self, session: &str) -> bool {
        self.full() || sessions::owner_of(session) == self.name
    }

    /// 客人的新会话（记在它名下）
    fn guest_session(&self) -> Result<String, Failure> {
        let mut session = Session::new(&focus_cwd());
        session.owner = self.name.clone();
        session.save().map_err(|message| (protocol::INTERNAL_ERROR, message))?;
        Ok(session.id)
    }

    /// 这条连接能不能做这件事
    fn permitted(&self, method: &str) -> Result<(), Failure> {
        let allowed = match method {
            "sense" => self.grant.sense,
            "ask" => self.grant.ask,
            "sessions.list" | "sessions.get" | "agents.list" | "changes.list" | "changes.diff" => self.grant.sessions >= Sessions::Read,
            "sessions.create" | "sessions.focus" | "sessions.stop" | "sessions.remove" | "sessions.dequeue" | "agents.stop"
            | "agents.send" | "agents.dequeue" | "changes.revert" => self.grant.sessions == Sessions::All,
            _ => true,
        };
        if allowed {
            Ok(())
        } else {
            Err((protocol::NOT_PERMITTED, format!("没有权限：「{}」不能 {method}（权限在能力配置的 permissions 里）", self.name)))
        }
    }

    fn handle(&self, method: &str, params: &Value) -> Result<Value, Failure> {
        if !self.ready && method != "ping" {
            return Err((protocol::NOT_INITIALIZED, "还没握手：等 EDITH 的 initialize、回了话再发".into()));
        }
        self.permitted(method)?;
        match method {
            "ping" => Ok(json!({})),
            "sense" => self.sense(params),
            "ask" => self.ask(params),
            "sessions.list" => Ok(Value::Array(
                sessions::list()
                    .iter()
                    .filter(|session| self.full() || session.owner == self.name)
                    .map(|session| {
                        let mut summary = session.summary();
                        let current = mind::current(&session.id);
                        summary["working"] = json!(current.is_some());
                        // 在干的那一轮：哪一轮、交来的原话（后连上来的客户端据此接着显示）
                        if let Some((turn, text)) = current {
                            summary["turn"] = json!({ "id": turn, "text": text });
                        }
                        let queue = mind::queue_of(&session.id);
                        if !queue.is_empty() {
                            summary["queue"] =
                                json!(queue.iter().map(|(turn, text)| json!({ "turn": turn, "text": text })).collect::<Vec<_>>());
                        }
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
                // 用对方给的 id（edith-friday：Friday 客户端自己定会话的 UUID）；已经有这个会话就照旧回它
                if let Some(id) = params.get("id").and_then(Value::as_str) {
                    let id = id.to_lowercase();
                    if uuid::Uuid::parse_str(&id).is_err() {
                        return Err(invalid(format!("id 要是 UUID：{id}")));
                    }
                    if let Some(existing) = Session::load(&id).filter(|existing| self.owns(&existing.id)) {
                        return Ok(existing.summary());
                    }
                    session.id = id;
                }
                if !self.full() {
                    session.owner = self.name.clone();
                }
                session.save().map_err(|message| (protocol::INTERNAL_ERROR, message))?;
                if params.get("focus").and_then(Value::as_bool) == Some(true) && self.full() {
                    mind::set_focus(&session.id);
                }
                Ok(session.summary())
            }
            "sessions.get" => {
                let id = params.get("id").and_then(Value::as_str).ok_or_else(|| invalid("要给 id"))?;
                let session =
                    Session::find(id).filter(|session| self.owns(&session.id)).ok_or((protocol::NO_SESSION, format!("没有会话 {id}")))?;
                let mut value = session.to_json();
                value["working"] = json!(mind::working(&session.id));
                value["focus"] = json!(session.id == mind::focus());
                Ok(value)
            }
            "sessions.focus" => {
                let id = params.get("id").and_then(Value::as_str).ok_or_else(|| invalid("要给 id"))?;
                let session = Session::find(id).ok_or((protocol::NO_SESSION, format!("没有会话 {id}")))?;
                if !self.full() {
                    return Err((protocol::NOT_PERMITTED, "没有权限：焦点是主人的，权限不全的连接不能换".into()));
                }
                mind::set_focus(&session.id);
                Ok(json!({ "focus": session.id }))
            }
            "sessions.stop" => {
                let id = params.get("id").and_then(Value::as_str).map(str::to_string).unwrap_or_else(mind::focus);
                if !self.owns(&id) {
                    return Err((protocol::NOT_PERMITTED, "没有权限：只能停下自己名下的会话".into()));
                }
                Ok(json!({ "stopping": mind::stop(&id) }))
            }
            "sessions.dequeue" => {
                // 拿掉一段排着的（sense 排队时回的 turn）
                let id = params.get("id").and_then(Value::as_str).ok_or_else(|| invalid("要给 id"))?;
                let turn = params.get("turn").and_then(Value::as_str).ok_or_else(|| invalid("要给 turn"))?;
                let session =
                    Session::find(id).filter(|session| self.owns(&session.id)).ok_or((protocol::NO_SESSION, format!("没有会话 {id}")))?;
                Ok(json!({ "removed": mind::dequeue(&session.id, turn) }))
            }
            "sessions.remove" => {
                // 删掉一段会话（在干活的先停下）：存档、改动基线、子 Agent 的运行记录一起删
                let id = params.get("id").and_then(Value::as_str).ok_or_else(|| invalid("要给 id"))?;
                let session =
                    Session::find(id).filter(|session| self.owns(&session.id)).ok_or((protocol::NO_SESSION, format!("没有会话 {id}")))?;
                // 停下后等这一轮收完（它结束时会存档，删早了又被写回来）；不等派出去的子 Agent（停了，交回时会话已经没了）
                if mind::stop(&session.id) {
                    let deadline = std::time::Instant::now() + Duration::from_secs(10);
                    while mind::working(&session.id).is_some() && std::time::Instant::now() < deadline {
                        std::thread::sleep(Duration::from_millis(50));
                    }
                }
                Ok(json!({ "removed": sessions::remove(&session.id) }))
            }
            // 子 Agent（team.rs）：在跑的、它们的运行记录；停掉、追加指示
            "agents.list" => {
                let session = params.get("session").and_then(Value::as_str);
                if session.is_some_and(|session| !self.owns(session)) {
                    return Err((protocol::NOT_PERMITTED, "没有权限：只能看自己名下会话的子 Agent".into()));
                }
                let agents = crate::team::list(session);
                Ok(Value::Array(agents.into_iter().filter(|agent| self.owns(agent["session"].as_str().unwrap_or(""))).collect()))
            }
            "agents.stop" | "agents.send" | "agents.dequeue" => {
                let id = params.get("id").and_then(Value::as_str).ok_or_else(|| invalid("要给 id"))?;
                let session = crate::team::session_of(id).ok_or((protocol::NO_SESSION, format!("没有子 Agent {id}")))?;
                if !self.owns(&session) {
                    return Err((protocol::NOT_PERMITTED, "没有权限：只能管自己名下会话的子 Agent".into()));
                }
                if method == "agents.stop" {
                    return Ok(json!({ "name": crate::team::stop(id) }));
                }
                if method == "agents.dequeue" {
                    let item = params.get("item").and_then(Value::as_str).ok_or_else(|| invalid("要给 item"))?;
                    return Ok(json!({ "removed": crate::team::dequeue(id, item) }));
                }
                let text = params
                    .get("text")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|text| !text.is_empty())
                    .ok_or_else(|| invalid("要给 text"))?;
                crate::team::send(id, text)
                    .map(|(name, item)| json!({ "name": name, "item": item }))
                    .map_err(|message| (protocol::INVALID_PARAMS, message))
            }
            // 会话改动（changes.rs）：改了哪些文件、改动、撤销
            "changes.list" | "changes.diff" | "changes.revert" => {
                let id = params.get("session").and_then(Value::as_str).ok_or_else(|| invalid("要给 session"))?;
                let session =
                    Session::find(id).filter(|session| self.owns(&session.id)).ok_or((protocol::NO_SESSION, format!("没有会话 {id}")))?;
                let path = params.get("path").and_then(Value::as_str).filter(|path| !path.is_empty());
                match method {
                    "changes.list" => Ok(crate::changes::list(&session.id)),
                    "changes.diff" => crate::changes::diff(&session.id, path.ok_or_else(|| invalid("要给 path"))?)
                        .map_err(|message| (protocol::NO_SESSION, message)),
                    _ => Ok(crate::changes::revert(&session.id, path)),
                }
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
            // 客人：没指明的、focus 都是它自己名下最近的会话（没有就新开）；指明的要是它名下的
            None | Some("focus") if !self.full() => match sessions::list().into_iter().find(|session| session.owner == self.name) {
                Some(session) => session.id,
                None => self.guest_session()?,
            },
            Some("new") if !self.full() => self.guest_session()?,
            Some(id) if !self.full() => Session::find(id)
                .filter(|session| session.owner == self.name)
                .map(|session| session.id)
                .ok_or((protocol::NO_SESSION, format!("没有会话 {id}（权限不全的连接只能用自己名下的会话）")))?,
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
                if let Err((_, message)) = self.permitted("sense").and_then(|_| self.sense(params)) {
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

    /// 让它断开：发 shutdown、关连接（大脑起的程序由 stop / stop_all 等它退、不退的结束）
    fn shutdown(&self) {
        self.alive.store(false, Ordering::Relaxed);
        if let Some(id) = self.listener.lock().unwrap().take() {
            mind::unlisten(id);
        }
        self.notify("shutdown", json!({}));
        // 丢掉写的一头：整条连接关掉（SocketWriter 丢掉时关）
        *self.writer.lock().unwrap() = None;
    }

    /// 连接断了：摘掉它的动作和事件，等着的请求立刻失败；配置里有它、又不是大脑让它断的，过一会儿重起它的程序
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
        crate::instance::refresh();
        if !was_alive || STOPPING.load(Ordering::Relaxed) {
            return;
        }
        log_line(&self.name, "断开了");
        if let Some(config) = enabled_config(&self.name)
            && restarts(&config)
        {
            let name = self.name.clone();
            std::thread::spawn(move || {
                // 它的程序还开着就先结束掉，再重起
                let child = LAUNCHED.lock().unwrap().remove(&name);
                if let Some(child) = child {
                    finish(vec![child]);
                }
                schedule_restart(name, Duration::from_secs(1));
            });
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

/// 能力的动作交给模型的工具定义：名字「<能力名>__<动作名>」（tools：这一轮许可的）
pub fn specs(tools: &permissions::Tools) -> Vec<Value> {
    let mut specs = Vec::new();
    for capability in native() {
        for action in &capability.actions {
            let name = action["name"].as_str().unwrap_or("");
            if !tools.allows(&format!("{}{SEPARATOR}{name}", capability.name), read_only(action)) {
                continue;
            }
            let description = action.get("description").and_then(Value::as_str).unwrap_or("");
            specs.push(json!({
                "type": "function",
                "function": {
                    "name": format!("{}{}{name}", capability.name, SEPARATOR).chars().take(64).collect::<String>(),
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

/// 动作说自己只读（不改东西）
fn read_only(action: &Value) -> bool {
    action.get("readOnly").and_then(Value::as_bool) == Some(true)
}

/// 这个工具名是能力的动作
pub fn is_action(full_name: &str) -> bool {
    let Some((name, rest)) = full_name.split_once(SEPARATOR) else { return false };
    let Some(capability) = CAPABILITIES.read().unwrap().get(name).cloned() else { return false };
    capability.actions.iter().any(|action| action["name"].as_str() == Some(rest))
}

/// 这个能力能批准（声明了 approves、是全权的）
pub fn can_approve(name: &str) -> bool {
    native().iter().any(|capability| capability.name == name && capability.approves && capability.full())
}

/// 请能力批准（approve 请求）。who：只问这一个（交话的那个能力）；None：问全部能批准的全权能力（几个 Friday 客户端都显示待确认），
/// 谁先答算谁的，其余的不再等、收一条 approve.cancel（界面把待确认收掉）。没人能批、问着的都断了返回 None
pub fn ask_approval(who: Option<&str>, request: &Value, cancel: &AtomicBool) -> Option<crate::approval::Decision> {
    let approvers: Vec<Arc<Capability>> = native()
        .into_iter()
        .filter(|capability| capability.approves && capability.full() && who.is_none_or(|who| capability.name == who))
        .collect();
    if approvers.is_empty() {
        return None;
    }
    let settled = Arc::new(AtomicBool::new(false));
    let (sender, receiver) = mpsc::channel::<(String, Result<Value, Value>)>();
    for approver in &approvers {
        let (approver, request, sender, settled) = (approver.clone(), request.clone(), sender.clone(), settled.clone());
        std::thread::spawn(move || {
            let reply = approver.request("approve", request, Duration::from_secs(600), Some(&settled));
            let _ = sender.send((approver.name.clone(), reply));
        });
    }
    drop(sender);
    let mut outcome = None;
    let mut waiting = approvers.len();
    while waiting > 0 && !cancel.load(Ordering::Relaxed) {
        match receiver.recv_timeout(Duration::from_millis(100)) {
            Ok((name, reply)) => {
                waiting -= 1;
                match reply {
                    Ok(result) => {
                        outcome = Some(crate::approval::Decision::parse(&result));
                        break;
                    }
                    // 问着问着断了（Friday 客户端关了）：等别的答；都断了当没人能批，等着的会改问下一个连上来的
                    Err(error) if error_text(&error) == GONE => {}
                    Err(error) => {
                        outcome = Some(crate::approval::Decision::Deny(format!("「{name}」没答上：{}", error_text(&error))));
                        break;
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    settled.store(true, Ordering::Relaxed);
    let done = json!({ "session": request["session"], "call": request["call"], "agent": request["agent"] });
    for approver in &approvers {
        approver.notify("approve.cancel", done.clone());
    }
    outcome
}

/// 这个工具名是能力的只读动作
pub fn is_read_only(full_name: &str) -> bool {
    let Some((name, rest)) = full_name.split_once(SEPARATOR) else { return false };
    let Some(capability) = CAPABILITIES.read().unwrap().get(name).cloned() else { return false };
    capability.actions.iter().any(|action| action["name"].as_str() == Some(rest) && read_only(action))
}

/// 调能力的动作（名字不是它们的返回 None）
pub fn call(full_name: &str, arguments: &Map<String, Value>, context: &Context, cancel: &AtomicBool) -> Option<Outcome> {
    let (name, rest) = full_name.split_once(SEPARATOR)?;
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
    let capability = CAPABILITIES.read().unwrap().get(name).cloned()?;
    Some(json!({
        "name": capability.name,
        "info": capability.info,
        "senses": capability.senses,
        "actions": capability.actions.iter().map(|action| action["name"].clone()).collect::<Vec<_>>(),
        "receives": capability.receives,
        "grant": capability.grant.to_json(),
    }))
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
/// 连着的、不是按配置起的连接有几条（Friday 客户端、attach、中转进来的远程设备……）：run --linger 按它算闲着没有
pub fn visitors() -> usize {
    let launched = LAUNCHED.lock().unwrap();
    native().iter().filter(|capability| !launched.contains_key(&capability.name)).count()
}

pub fn names() -> Vec<String> {
    native().iter().map(|capability| capability.name.clone()).collect()
}

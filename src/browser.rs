//! 内置浏览器（Friday 的 browser.*，Friday 仓库的 qt/docs/BROWSER.md §9；照 Friday Qt 版没有界面时用的 CdpBrowser）：大脑起这台装着的
//! Chrome / Chromium / Edge 的无头模式，经 DevTools 协议开标签、推画面（Page.startScreencast）、转鼠标键盘。标签在大脑里：
//! 客户端断了照常在，接上来接着看；大脑重起后按记下的网址重新打开（`<项目数据目录>/browser-tabs.json`）。
//! Linux / macOS 走 `--remote-debugging-pipe`（fd 3 进、fd 4 出，一条一条以 \0 分隔，不开端口）；Windows 走本机随机端口 + WebSocket。
//! 看着一个标签的连接（browser.watch）收它的 `browser.frame`；标签开了、关了、标题网址加载状态、对话框变了推 `browser`（全部标签）给全权的连接。

use std::collections::HashMap;
#[cfg(unix)]
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

const DEFAULT_VIEWPORT: (u32, u32) = (1280, 800);
/// 等浏览器回话最多多久
const CALL_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, Default)]
struct Tab {
    id: String,
    session: String,
    target: String,
    /// CDP 的会话（attachToTarget 回的）：空 = 还没接上
    cdp: String,
    frame: String,
    url: String,
    title: String,
    loading: bool,
    navigating: bool,
    /// 接上了要打开的
    pending: Option<String>,
    container: (u32, u32),
    fixed: Option<(u32, u32)>,
    dialog: Option<Value>,
    /// 看着它的连接 → 画面最大多大
    watchers: HashMap<String, (u32, u32)>,
}

impl Tab {
    fn viewport(&self) -> (u32, u32) {
        self.fixed.unwrap_or(if self.container.0 >= 10 && self.container.1 >= 10 { self.container } else { DEFAULT_VIEWPORT })
    }

    /// 画面最大多大：看着的里面最大的
    fn frame_size(&self) -> Option<(u32, u32)> {
        self.watchers.values().copied().reduce(|a, b| (a.0.max(b.0), a.1.max(b.1)))
    }
}

type Reply = Sender<Result<Value, String>>;

/// 往浏览器写的那一头
enum Writer {
    #[cfg(unix)]
    Pipe(std::io::PipeWriter),
    #[cfg(windows)]
    Socket(socket::Socket),
}

impl Writer {
    fn send(&mut self, text: &str) -> std::io::Result<()> {
        match self {
            #[cfg(unix)]
            Writer::Pipe(pipe) => {
                pipe.write_all(text.as_bytes())?;
                pipe.write_all(&[0])?;
                pipe.flush()
            }
            #[cfg(windows)]
            Writer::Socket(socket) => socket.send_text(text),
        }
    }
}

#[derive(Default)]
struct Inner {
    tabs: Vec<Tab>,
    /// 会话 → 选中的标签
    current: HashMap<String, String>,
    loaded: bool,
    start_error: String,
    downloads: HashMap<String, (String, String)>,
}

struct Browser {
    inner: Mutex<Inner>,
    writer: Mutex<Option<Writer>>,
    process: Mutex<Option<Child>>,
    /// 一次只起一个
    starting: Mutex<()>,
    next: AtomicU64,
    pending: Mutex<HashMap<u64, Reply>>,
    dirty: AtomicBool,
    flusher: AtomicBool,
}

static BROWSER: LazyLock<Browser> = LazyLock::new(|| Browser {
    inner: Mutex::new(Inner::default()),
    writer: Mutex::new(None),
    process: Mutex::new(None),
    starting: Mutex::new(()),
    next: AtomicU64::new(1),
    pending: Mutex::new(HashMap::new()),
    dirty: AtomicBool::new(false),
    flusher: AtomicBool::new(false),
});

// MARK: - 找浏览器、起浏览器

/// 这台装着的 Chrome / Chromium / Edge（EDITH_BROWSER 指定的优先）
fn executable() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("EDITH_BROWSER").filter(|path| !path.is_empty()).map(PathBuf::from) {
        return path.is_file().then_some(path);
    }
    let mut candidates: Vec<PathBuf> = Vec::new();
    if cfg!(windows) {
        for base in ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"].iter().filter_map(std::env::var_os) {
            let base = PathBuf::from(base);
            candidates.push(base.join("Google/Chrome/Application/chrome.exe"));
            candidates.push(base.join("Microsoft/Edge/Application/msedge.exe"));
            candidates.push(base.join("Chromium/Application/chrome.exe"));
        }
    } else if cfg!(target_os = "macos") {
        for app in [
            "Google Chrome.app/Contents/MacOS/Google Chrome",
            "Chromium.app/Contents/MacOS/Chromium",
            "Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
        ] {
            candidates.push(Path::new("/Applications").join(app));
            candidates.push(crate::home::user_home().join("Applications").join(app));
        }
    } else {
        for name in ["google-chrome", "google-chrome-stable", "chromium", "chromium-browser", "microsoft-edge", "microsoft-edge-stable"] {
            if let Some(found) = crate::downloads::find_in_path(name) {
                candidates.push(found);
            }
        }
        candidates.push(PathBuf::from("/snap/bin/chromium"));
    }
    candidates.into_iter().find(|path| path.is_file())
}

fn unavailable_reason() -> String {
    if cfg!(windows) {
        "这台机器上没有 Chrome 或 Edge：装一个后再试".into()
    } else if cfg!(target_os = "macos") {
        "这台 Mac 上没有 Chrome、Chromium 或 Edge：装一个后再试".into()
    } else {
        "这台机器上没有 Chrome / Chromium：装一个后再试（Ubuntu / Debian：sudo apt install chromium-browser 或 chromium）".into()
    }
}

fn data_dir() -> PathBuf {
    crate::home::project_dir().join("browser")
}

fn downloads_dir() -> PathBuf {
    crate::home::dir().join("downloads")
}

fn running() -> bool {
    BROWSER.writer.lock().unwrap().is_some()
}

/// 起浏览器（起着的不再起）
fn ensure_started() -> Result<(), String> {
    let _once = BROWSER.starting.lock().unwrap();
    if running() {
        return Ok(());
    }
    let program = executable().ok_or_else(unavailable_reason)?;
    let profile = data_dir().join("profile");
    std::fs::create_dir_all(&profile).map_err(|error| format!("建不了 {}：{error}", profile.display()))?;
    let _ = std::fs::create_dir_all(downloads_dir());
    let mut args: Vec<String> = [
        "--headless=new",
        "--no-first-run",
        "--no-default-browser-check",
        "--mute-audio",
        "--hide-scrollbars",
        "--window-size=1280,800",
        "--lang=zh-CN",
    ]
    .iter()
    .map(|arg| arg.to_string())
    .collect();
    args.push(format!("--user-data-dir={}", profile.display()));
    if cfg!(target_os = "linux") {
        // 容器里 /dev/shm 很小；root、AppArmor 限制了非特权用户命名空间时沙箱起不来
        args.push("--disable-dev-shm-usage".into());
        let flag = |path: &str| std::fs::read_to_string(path).map(|text| text.trim().to_string()).unwrap_or_default();
        #[cfg(unix)]
        let root = unsafe { libc::geteuid() } == 0;
        #[cfg(not(unix))]
        let root = false;
        if root
            || flag("/proc/sys/kernel/apparmor_restrict_unprivileged_userns") == "1"
            || flag("/proc/sys/kernel/unprivileged_userns_clone") == "0"
        {
            args.push("--no-sandbox".into());
        }
    }
    let started = start_transport(&program, args, &profile);
    let mut inner = BROWSER.inner.lock().unwrap();
    match started {
        Ok(()) => {
            inner.start_error.clear();
            drop(inner);
            post("Target.setDiscoverTargets", json!({ "discover": true }), "");
            post(
                "Browser.setDownloadBehavior",
                json!({ "behavior": "allow", "downloadPath": downloads_dir().to_string_lossy(), "eventsEnabled": true }),
                "",
            );
            Ok(())
        }
        Err(error) => {
            inner.start_error = error.clone();
            Err(error)
        }
    }
}

#[cfg(unix)]
fn start_transport(program: &Path, mut args: Vec<String>, _profile: &Path) -> Result<(), String> {
    use std::os::fd::AsRawFd;
    use std::os::unix::process::CommandExt;
    args.push("--remote-debugging-pipe".into());
    args.push("about:blank".into());
    let (child_read, to_browser) = std::io::pipe().map_err(|error| format!("建不了管道：{error}"))?;
    let (from_browser, child_write) = std::io::pipe().map_err(|error| format!("建不了管道：{error}"))?;
    let (read_fd, write_fd) = (child_read.as_raw_fd(), child_write.as_raw_fd());
    let mut command = std::process::Command::new(program);
    command.args(&args).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    unsafe {
        // 子进程里把两头接到 fd 3、4（dup2 出来的不带 CLOEXEC）
        command.pre_exec(move || {
            if libc::dup2(read_fd, 3) < 0 || libc::dup2(write_fd, 4) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = command.spawn().map_err(|error| format!("起不来浏览器（{}）：{error}", program.display()))?;
    drop(child_read);
    drop(child_write);
    *BROWSER.process.lock().unwrap() = Some(child);
    *BROWSER.writer.lock().unwrap() = Some(Writer::Pipe(to_browser));
    std::thread::spawn(move || {
        let mut reader = std::io::BufReader::new(from_browser);
        let mut message = Vec::new();
        while std::io::BufRead::read_until(&mut reader, 0, &mut message).is_ok_and(|count| count > 0) {
            if message.last() == Some(&0) {
                message.pop();
            }
            if let Ok(value) = serde_json::from_slice::<Value>(&message) {
                on_message(value);
            }
            message.clear();
        }
        closed();
    });
    Ok(())
}

#[cfg(windows)]
fn start_transport(program: &Path, mut args: Vec<String>, profile: &Path) -> Result<(), String> {
    let port_file = profile.join("DevToolsActivePort");
    let _ = std::fs::remove_file(&port_file);
    args.push("--remote-debugging-port=0".into());
    args.push("--remote-debugging-address=127.0.0.1".into());
    args.push("about:blank".into());
    let child = std::process::Command::new(program)
        .args(&args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|error| format!("起不来浏览器（{}）：{error}", program.display()))?;
    *BROWSER.process.lock().unwrap() = Some(child);
    // 等端口文件出来（最多 15 秒）再连
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    let lines = loop {
        let lines: Vec<String> = std::fs::read_to_string(&port_file)
            .unwrap_or_default()
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect();
        if lines.len() >= 2 {
            break lines;
        }
        if std::time::Instant::now() > deadline {
            stop();
            return Err("浏览器起来了但连不上（没有 DevToolsActivePort）".into());
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let (writer, mut reader) = socket::connect(&format!("127.0.0.1:{}", lines[0]), &lines[1])?;
    *BROWSER.writer.lock().unwrap() = Some(Writer::Socket(writer));
    std::thread::spawn(move || {
        while let Some(text) = reader.next_text() {
            if let Ok(value) = serde_json::from_str::<Value>(&text) {
                on_message(value);
            }
        }
        closed();
    });
    Ok(())
}

/// 浏览器退出了：标签都当没接上（下次用时重起、按网址重新打开），等着的回话都失败
fn closed() {
    *BROWSER.writer.lock().unwrap() = None;
    if let Some(mut child) = BROWSER.process.lock().unwrap().take() {
        let _ = child.wait();
    }
    {
        let mut inner = BROWSER.inner.lock().unwrap();
        for tab in inner.tabs.iter_mut() {
            tab.cdp.clear();
            tab.target.clear();
            if tab.pending.is_none() && !tab.url.is_empty() {
                tab.pending = Some(tab.url.clone());
            }
        }
    }
    let pending: Vec<Reply> = BROWSER.pending.lock().unwrap().drain().map(|(_, reply)| reply).collect();
    for reply in pending {
        let _ = reply.send(Err("浏览器退出了".into()));
    }
    changed();
}

/// 大脑退出时：关掉浏览器（Browser.close 让它自己退，Cookie、登录才写到硬盘）
pub fn stop() {
    if running() {
        post("Browser.close", json!({}), "");
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while running() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    if let Some(mut child) = BROWSER.process.lock().unwrap().take() {
        let _ = child.kill();
        let _ = child.wait();
    }
    *BROWSER.writer.lock().unwrap() = None;
}

// MARK: - 收发

fn send_raw(method: &str, params: Value, cdp: &str, id: u64) -> bool {
    let mut message = json!({ "id": id, "method": method, "params": params });
    if !cdp.is_empty() {
        message["sessionId"] = json!(cdp);
    }
    let mut writer = BROWSER.writer.lock().unwrap();
    let Some(pipe) = writer.as_mut() else { return false };
    pipe.send(&message.to_string()).is_ok()
}

/// 发了不等回话
fn post(method: &str, params: Value, cdp: &str) {
    let id = BROWSER.next.fetch_add(1, Ordering::Relaxed);
    send_raw(method, params, cdp, id);
}

/// 发了等回话（别在读的那个线程里调）
fn call(method: &str, params: Value, cdp: &str) -> Result<Value, String> {
    let id = BROWSER.next.fetch_add(1, Ordering::Relaxed);
    let (sender, receiver) = mpsc::channel();
    BROWSER.pending.lock().unwrap().insert(id, sender);
    if !send_raw(method, params, cdp, id) {
        BROWSER.pending.lock().unwrap().remove(&id);
        return Err("浏览器没在跑".into());
    }
    match receiver.recv_timeout(CALL_TIMEOUT) {
        Ok(outcome) => outcome,
        Err(_) => {
            BROWSER.pending.lock().unwrap().remove(&id);
            Err(format!("{method} 等了 {} 秒没回话", CALL_TIMEOUT.as_secs()))
        }
    }
}

fn on_message(message: Value) {
    if let Some(id) = message.get("id").and_then(Value::as_u64) {
        if let Some(reply) = BROWSER.pending.lock().unwrap().remove(&id) {
            let outcome = match message.get("error") {
                Some(error) => Err(error["message"].as_str().unwrap_or("浏览器回了错误").to_string()),
                None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
            };
            let _ = reply.send(outcome);
        }
        return;
    }
    let method = message["method"].as_str().unwrap_or("");
    let params = &message["params"];
    let cdp = message["sessionId"].as_str().unwrap_or("");
    on_event(method, params, cdp);
}

/// 网页没有标题时 Chrome 报网址（去掉协议）：当作没有
fn display_title(title: &str, url: &str) -> String {
    let title = title.trim();
    let bare = url.split_once("://").map_or(url, |(_, rest)| rest);
    if title.is_empty() || title == url || title == bare { String::new() } else { title.to_string() }
}

fn on_event(method: &str, params: &Value, cdp: &str) {
    let text = |value: &Value| value.as_str().unwrap_or("").to_string();
    let mut inner = BROWSER.inner.lock().unwrap();
    match method {
        "Target.targetCreated" => {
            // 网页开的新窗口：同一段会话的新标签
            let info = &params["targetInfo"];
            let opener = text(&info["openerId"]);
            let target = text(&info["targetId"]);
            if info["type"] != "page" || opener.is_empty() || inner.tabs.iter().any(|tab| tab.target == target) {
                return;
            }
            let Some(parent) = inner.tabs.iter().find(|tab| tab.target == opener).cloned() else { return };
            let tab = Tab {
                id: uuid::Uuid::new_v4().to_string(),
                session: parent.session.clone(),
                target,
                url: text(&info["url"]),
                container: parent.container,
                ..Default::default()
            };
            let id = tab.id.clone();
            inner.current.insert(tab.session.clone(), id.clone());
            inner.tabs.push(tab);
            drop(inner);
            std::thread::spawn(move || {
                let _ = attach(&id);
            });
            save();
            changed();
            return;
        }
        "Target.targetInfoChanged" => {
            let info = &params["targetInfo"];
            let target = text(&info["targetId"]);
            if let Some(tab) = inner.tabs.iter_mut().find(|tab| tab.target == target) {
                let url = text(&info["url"]);
                let title = display_title(info["title"].as_str().unwrap_or(""), &url);
                if url != tab.url || title != tab.title {
                    tab.url = url;
                    tab.title = title;
                    drop(inner);
                    changed();
                    save();
                }
            }
            return;
        }
        "Target.targetDestroyed" => {
            let target = text(&params["targetId"]);
            if let Some(index) = inner.tabs.iter().position(|tab| tab.target == target) {
                let tab = inner.tabs.remove(index);
                if inner.current.get(&tab.session) == Some(&tab.id) {
                    inner.current.remove(&tab.session);
                }
                drop(inner);
                changed();
                save();
            }
            return;
        }
        "Target.detachedFromTarget" => {
            let session = text(&params["sessionId"]);
            if let Some(tab) = inner.tabs.iter_mut().find(|tab| tab.cdp == session) {
                tab.cdp.clear();
            }
            return;
        }
        "Browser.downloadWillBegin" | "Browser.downloadProgress" => {
            let guid = text(&params["guid"]);
            let (tab, name) = if method == "Browser.downloadWillBegin" {
                let frame = text(&params["frameId"]);
                let Some(owner) = inner.tabs.iter().find(|tab| tab.frame == frame).map(|tab| tab.id.clone()) else { return };
                let name = text(&params["suggestedFilename"]);
                inner.downloads.insert(guid.clone(), (owner.clone(), name.clone()));
                (owner, name)
            } else {
                let Some(known) = inner.downloads.get(&guid).cloned() else { return };
                known
            };
            let state = match params["state"].as_str() {
                Some("completed") => "done",
                Some("canceled") => "failed",
                _ => "progress",
            };
            if state != "progress" {
                inner.downloads.remove(&guid);
            }
            let total = params["totalBytes"].as_f64().unwrap_or(0.0);
            let fraction = if total > 0.0 { params["receivedBytes"].as_f64().unwrap_or(0.0) / total } else { 0.0 };
            let path = downloads_dir().join(&name);
            drop(inner);
            crate::capabilities::notify_full(
                "browser.download",
                json!({ "tab": tab, "name": name, "path": path.to_string_lossy(), "state": state, "fraction": fraction }),
            );
            return;
        }
        _ => {}
    }
    let Some(tab) = inner.tabs.iter_mut().find(|tab| !cdp.is_empty() && tab.cdp == cdp) else { return };
    match method {
        "Page.frameNavigated" => {
            let frame = &params["frame"];
            if frame["parentId"].as_str().unwrap_or("").is_empty() {
                tab.frame = text(&frame["id"]);
                tab.url = text(&frame["url"]);
                let cdp = tab.cdp.clone();
                drop(inner);
                // 后退 / 前进从缓存拿回来的页面不触发加载事件：标题自己读一次
                refresh_title(&cdp);
                changed();
            }
        }
        "Page.frameStartedLoading" | "Page.frameStoppedLoading" => {
            if text(&params["frameId"]) == tab.frame {
                tab.loading = method == "Page.frameStartedLoading";
                drop(inner);
                changed();
            }
        }
        "Page.loadEventFired" | "Page.domContentEventFired" => {
            let cdp = tab.cdp.clone();
            drop(inner);
            refresh_title(&cdp);
        }
        "Runtime.bindingCalled" => {
            if params["name"] == "__edithTitle" {
                let title = display_title(params["payload"].as_str().unwrap_or(""), &tab.url);
                if title != tab.title {
                    tab.title = title;
                    drop(inner);
                    changed();
                    save();
                }
            }
        }
        "Page.javascriptDialogOpening" => {
            tab.dialog = Some(json!({ "type": params["type"], "message": params["message"], "defaultText": params["defaultPrompt"] }));
            drop(inner);
            changed();
        }
        "Page.javascriptDialogClosed" => {
            tab.dialog = None;
            drop(inner);
            changed();
        }
        "Page.screencastFrame" => {
            let (id, viewport, watchers) = (tab.id.clone(), tab.viewport(), tab.watchers.keys().cloned().collect::<Vec<_>>());
            let cdp = tab.cdp.clone();
            drop(inner);
            post("Page.screencastFrameAck", json!({ "sessionId": params["sessionId"] }), &cdp);
            if !watchers.is_empty() {
                crate::capabilities::notify_some(
                    &watchers,
                    "browser.frame",
                    json!({ "tab": id, "data": params["data"], "viewport": [viewport.0, viewport.1] }),
                );
            }
        }
        _ => {}
    }
}

/// 读一次标题、网址（targetInfoChanged 有时不报）
fn refresh_title(cdp: &str) {
    let cdp = cdp.to_string();
    std::thread::spawn(move || {
        let Ok(result) = call("Runtime.evaluate", json!({ "expression": "[document.title, location.href]", "returnByValue": true }), &cdp)
        else {
            return;
        };
        let value = &result["result"]["value"];
        let url = value[1].as_str().unwrap_or("").to_string();
        let title = display_title(value[0].as_str().unwrap_or(""), &url);
        let mut inner = BROWSER.inner.lock().unwrap();
        if let Some(tab) = inner.tabs.iter_mut().find(|tab| tab.cdp == cdp)
            && (tab.title != title || (!url.is_empty() && tab.url != url))
        {
            tab.title = title;
            if !url.is_empty() {
                tab.url = url;
            }
            drop(inner);
            changed();
            save();
        }
    });
}

/// 网页标题变了（网页用脚本改的）：在 edith 这个隔离世界里盯着 document.title，经绑定报给这边
const TITLE_WATCH: &str = r#"(() => {
  let last = null;
  const send = () => { const title = document.title; if (title === last) return; last = title; try { __edithTitle(title); } catch (e) {} };
  const watch = () => { send(); new MutationObserver(send).observe(document.documentElement || document, { subtree: true, childList: true, characterData: true }); };
  if (document.readyState === 'loading') document.addEventListener('DOMContentLoaded', watch, { once: true }); else watch();
})()"#;

// MARK: - 标签

fn with_tab<T>(id: &str, change: impl FnOnce(&mut Tab) -> T) -> Result<T, String> {
    let mut inner = BROWSER.inner.lock().unwrap();
    load_tabs(&mut inner);
    inner.tabs.iter_mut().find(|tab| tab.id == id).map(change).ok_or_else(|| format!("没有标签 {id}"))
}

/// 接上一个标签（没开的先开；接上了打开等着的网址、要画面的开始推）
fn attach(id: &str) -> Result<(), String> {
    ensure_started()?;
    let (target, cdp) = with_tab(id, |tab| (tab.target.clone(), tab.cdp.clone()))?;
    if !cdp.is_empty() {
        return Ok(());
    }
    let target = if target.is_empty() {
        call("Target.createTarget", json!({ "url": "about:blank" }), "")?["targetId"].as_str().unwrap_or("").to_string()
    } else {
        target
    };
    let cdp =
        call("Target.attachToTarget", json!({ "targetId": target, "flatten": true }), "")?["sessionId"].as_str().unwrap_or("").to_string();
    with_tab(id, |tab| {
        tab.target = target.clone();
        tab.cdp = cdp.clone();
    })?;
    post("Page.enable", json!({}), &cdp);
    post("Runtime.enable", json!({}), &cdp);
    post("Runtime.addBinding", json!({ "name": "__edithTitle", "executionContextName": "edith" }), &cdp);
    post("Page.addScriptToEvaluateOnNewDocument", json!({ "source": TITLE_WATCH, "worldName": "edith" }), &cdp);
    apply_viewport(id);
    let tree = call("Page.getFrameTree", json!({}), &cdp)?;
    let pending = with_tab(id, |tab| {
        tab.frame = tree["frameTree"]["frame"]["id"].as_str().unwrap_or("").to_string();
        tab.pending.take()
    })?;
    if let Some(url) = pending {
        navigate_to(id, &url);
    }
    screencast(id);
    changed();
    Ok(())
}

fn apply_viewport(id: &str) {
    let Ok((cdp, (width, height))) = with_tab(id, |tab| (tab.cdp.clone(), tab.viewport())) else { return };
    if cdp.is_empty() {
        return;
    }
    post("Emulation.setDeviceMetricsOverride", json!({ "width": width, "height": height, "deviceScaleFactor": 1, "mobile": false }), &cdp);
    screencast(id);
}

/// 有人看着就推画面（不超过看着的里面最大的那个），没人看就停
fn screencast(id: &str) {
    let Ok((cdp, size, viewport)) = with_tab(id, |tab| (tab.cdp.clone(), tab.frame_size(), tab.viewport())) else { return };
    if cdp.is_empty() {
        return;
    }
    match size {
        Some((width, height)) => post(
            "Page.startScreencast",
            json!({ "format": "jpeg", "quality": 70, "maxWidth": width.clamp(64, viewport.0.max(64)), "maxHeight": height.clamp(64, viewport.1.max(64)), "everyNthFrame": 1 }),
            &cdp,
        ),
        None => post("Page.stopScreencast", json!({}), &cdp),
    }
}

fn navigate_to(id: &str, url: &str) {
    let Ok(cdp) = with_tab(id, |tab| {
        tab.navigating = true;
        tab.cdp.clone()
    }) else {
        return;
    };
    changed();
    let (id, url) = (id.to_string(), url.to_string());
    // Page.navigate 在新页面提交（或失败）后才回，之后的加载状态由 frameStartedLoading / StoppedLoading 管
    std::thread::spawn(move || {
        let _ = call("Page.navigate", json!({ "url": url }), &cdp);
        let _ = with_tab(&id, |tab| tab.navigating = false);
        changed();
    });
}

/// 地址栏风格的输入 → 网址（像网址的补协议，别的用必应搜；同 Friday 的 normalize）
fn normalize(input: &str) -> String {
    let text = input.trim();
    if text.is_empty() {
        return String::new();
    }
    let lower = text.to_lowercase();
    let has_scheme = text.split_once(':').is_some_and(|(scheme, rest)| {
        !scheme.is_empty()
            && scheme.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
            && scheme.chars().all(|c| c.is_ascii_alphanumeric() || "+.-".contains(c))
            && !rest.split('/').next().is_some_and(|port| !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()))
    }) || lower.starts_with("about:");
    if has_scheme {
        return text.to_string();
    }
    let host = text.split('/').next().unwrap_or("");
    let bare = host.split(':').next().unwrap_or("");
    if ["localhost", "127.0.0.1", "[::1]"].contains(&bare)
        || bare.split('.').count() == 4 && bare.split('.').all(|part| part.parse::<u8>().is_ok())
    {
        return format!("http://{text}");
    }
    let domain =
        !text.contains(' ') && bare.rsplit_once('.').is_some_and(|(_, top)| top.len() >= 2 && top.chars().all(|c| c.is_ascii_alphabetic()));
    if domain {
        return format!("https://{text}");
    }
    let encoded: String = text
        .bytes()
        .map(
            |byte| {
                if byte.is_ascii_alphanumeric() || b"-_.~".contains(&byte) { (byte as char).to_string() } else { format!("%{byte:02X}") }
            },
        )
        .collect();
    format!("https://www.bing.com/search?q={encoded}")
}

fn tabs_file() -> PathBuf {
    data_dir().join("tabs.json")
}

/// 第一次用时读回记下的标签（没接上，用到时按网址重新打开）
fn load_tabs(inner: &mut Inner) {
    if inner.loaded {
        return;
    }
    inner.loaded = true;
    let Some(saved) = crate::home::read_json(&tabs_file()) else { return };
    for item in saved["tabs"].as_array().into_iter().flatten() {
        let text = |key: &str| item[key].as_str().unwrap_or("").to_string();
        if text("id").is_empty() || text("session").is_empty() {
            continue;
        }
        let fixed = item["viewport"]
            .as_array()
            .filter(|size| size.len() == 2)
            .map(|size| (size[0].as_u64().unwrap_or(0) as u32, size[1].as_u64().unwrap_or(0) as u32));
        let url = text("url");
        inner.tabs.push(Tab {
            id: text("id"),
            session: text("session"),
            title: text("title"),
            pending: (!url.is_empty()).then(|| url.clone()),
            url,
            fixed,
            ..Default::default()
        });
    }
    for (session, tab) in saved["current"].as_object().into_iter().flatten() {
        inner.current.insert(session.clone(), tab.as_str().unwrap_or("").to_string());
    }
}

fn save() {
    let inner = BROWSER.inner.lock().unwrap();
    let tabs: Vec<Value> = inner
        .tabs
        .iter()
        .map(|tab| {
            let mut item = json!({ "id": tab.id, "session": tab.session, "url": tab.pending.clone().unwrap_or_else(|| tab.url.clone()), "title": tab.title });
            if let Some((width, height)) = tab.fixed {
                item["viewport"] = json!([width, height]);
            }
            item
        })
        .collect();
    let value = json!({ "tabs": tabs, "current": inner.current });
    drop(inner);
    let _ = crate::home::write_json(&tabs_file(), &value);
}

/// 这段会话选中的标签：选过的（还在的），不然最后一个
fn current_of(inner: &Inner, session: &str) -> String {
    let chosen = inner.current.get(session).filter(|id| inner.tabs.iter().any(|tab| tab.id == **id && tab.session == session));
    chosen.cloned().or_else(|| inner.tabs.iter().rev().find(|tab| tab.session == session).map(|tab| tab.id.clone())).unwrap_or_default()
}

/// BrowserState
pub fn state() -> Value {
    if executable().is_none() {
        return json!({ "available": false, "reason": unavailable_reason(), "tabs": [] });
    }
    let mut inner = BROWSER.inner.lock().unwrap();
    load_tabs(&mut inner);
    let starting = running();
    let tabs: Vec<Value> = inner
        .tabs
        .iter()
        .map(|tab| {
            let host = tab.url.split_once("://").map(|(_, rest)| rest.split('/').next().unwrap_or("")).unwrap_or("");
            let title = if !tab.title.is_empty() {
                tab.title.clone()
            } else if host.is_empty() {
                "新标签".into()
            } else {
                host.to_string()
            };
            let (width, height) = tab.viewport();
            let mut item = json!({
                "id": tab.id, "conversationID": tab.session, "title": title,
                "url": if tab.url == "about:blank" { "" } else { tab.url.as_str() },
                "loading": tab.loading || tab.navigating || (tab.cdp.is_empty() && tab.pending.is_some() && starting),
                "current": current_of(&inner, &tab.session) == tab.id, "viewport": [width, height], "fixed": tab.fixed.is_some(),
            });
            if let Some(dialog) = &tab.dialog {
                item["dialog"] = dialog.clone();
            }
            item
        })
        .collect();
    let mut value = json!({ "available": true, "tabs": tabs });
    if !inner.start_error.is_empty() {
        value["reason"] = json!(inner.start_error);
    }
    value
}

/// 标签变了：80 毫秒内合成一次推给全权的连接
fn changed() {
    BROWSER.dirty.store(true, Ordering::Relaxed);
    if !BROWSER.flusher.swap(true, Ordering::Relaxed) {
        std::thread::spawn(|| {
            loop {
                std::thread::sleep(Duration::from_millis(80));
                if BROWSER.dirty.swap(false, Ordering::Relaxed) {
                    crate::capabilities::notify_full("browser", state());
                }
            }
        });
    }
}

/// 给这段会话开一个标签（选中它）
pub fn open(session: &str, url: &str) -> Result<Value, String> {
    ensure_started()?;
    let target = normalize(url);
    let tab = Tab {
        id: uuid::Uuid::new_v4().to_string(),
        session: session.to_string(),
        pending: Some(if target.is_empty() { "about:blank".into() } else { target.clone() }),
        url: target,
        ..Default::default()
    };
    let id = tab.id.clone();
    {
        let mut inner = BROWSER.inner.lock().unwrap();
        load_tabs(&mut inner);
        inner.current.insert(session.to_string(), id.clone());
        inner.tabs.push(tab);
    }
    changed();
    save();
    let attaching = id.clone();
    std::thread::spawn(move || {
        if let Err(error) = attach(&attaching) {
            crate::capabilities::log_line("browser", &format!("接不上标签：{error}"));
        }
    });
    Ok(json!({ "tab": id }))
}

/// 用户在卡片里的操作、看画面这些都要标签接着：没接上的先接上（大脑重起后第一次看）
fn wake(id: &str) {
    if with_tab(id, |tab| tab.cdp.is_empty()).unwrap_or(false) {
        let id = id.to_string();
        std::thread::spawn(move || {
            let _ = attach(&id);
        });
    }
}

/// Qt 的修饰键 → CDP 的（Alt 1、Ctrl 2、Meta 4、Shift 8）
fn modifiers(qt: u64) -> u64 {
    let mut bits = 0;
    if qt & 0x0800_0000 != 0 {
        bits |= 1;
    }
    if qt & 0x0400_0000 != 0 {
        bits |= 2;
    }
    if qt & 0x1000_0000 != 0 {
        bits |= 4;
    }
    if qt & 0x0200_0000 != 0 {
        bits |= 8;
    }
    bits
}

/// DOM 的 code → Windows 虚拟键码（Chrome 按它做回车、退格、方向键这类的默认动作）
fn virtual_key(code: &str, key: &str) -> u32 {
    let named = [
        ("Enter", 13),
        ("NumpadEnter", 13),
        ("Backspace", 8),
        ("Tab", 9),
        ("Escape", 27),
        ("Space", 32),
        ("PageUp", 33),
        ("PageDown", 34),
        ("End", 35),
        ("Home", 36),
        ("ArrowLeft", 37),
        ("ArrowUp", 38),
        ("ArrowRight", 39),
        ("ArrowDown", 40),
        ("Delete", 46),
        ("Insert", 45),
        ("ShiftLeft", 16),
        ("ShiftRight", 16),
        ("ControlLeft", 17),
        ("ControlRight", 17),
        ("AltLeft", 18),
        ("AltRight", 18),
        ("MetaLeft", 91),
        ("MetaRight", 92),
    ];
    if let Some((_, value)) = named.iter().find(|(name, _)| *name == code) {
        return *value;
    }
    if let Some(letter) = code.strip_prefix("Key").filter(|rest| rest.len() == 1) {
        return letter.to_ascii_uppercase().chars().next().unwrap_or('A') as u32;
    }
    if let Some(digit) = code.strip_prefix("Digit").filter(|rest| rest.len() == 1) {
        return digit.chars().next().unwrap_or('0') as u32;
    }
    if let Some(number) = code.strip_prefix('F').and_then(|rest| rest.parse::<u32>().ok()).filter(|number| *number >= 1) {
        return 111 + number;
    }
    let mut chars = key.chars();
    match (chars.next(), chars.next()) {
        (Some(only), None) => only.to_ascii_uppercase() as u32,
        _ => 0,
    }
}

/// browser.* 里除了 open、state 的（tab：标签 id；watcher：发请求的连接）
pub fn request(method: &str, params: &Value, watcher: &str) -> Result<Value, String> {
    let id = params["tab"].as_str().ok_or("要给 tab")?.to_string();
    let number = |key: &str| params[key].as_f64().unwrap_or(0.0);
    let size = |key: &str, default: u32, top: u32| (params[key].as_u64().unwrap_or(default as u64) as u32).clamp(64, top);
    match method {
        "browser.select" => {
            let session = with_tab(&id, |tab| tab.session.clone())?;
            BROWSER.inner.lock().unwrap().current.insert(session, id);
            changed();
            save();
        }
        "browser.close" => {
            let tab = {
                let mut inner = BROWSER.inner.lock().unwrap();
                let index = inner.tabs.iter().position(|tab| tab.id == id).ok_or_else(|| format!("没有标签 {id}"))?;
                let tab = inner.tabs.remove(index);
                if inner.current.get(&tab.session) == Some(&tab.id) {
                    inner.current.remove(&tab.session);
                }
                tab
            };
            if !tab.target.is_empty() {
                post("Target.closeTarget", json!({ "targetId": tab.target }), "");
            }
            changed();
            save();
        }
        "browser.navigate" => {
            let (cdp, attached) = with_tab(&id, |tab| (tab.cdp.clone(), !tab.cdp.is_empty()))?;
            if let Some(url) = params["url"].as_str().map(normalize).filter(|url| !url.is_empty()) {
                if attached {
                    navigate_to(&id, &url);
                } else {
                    with_tab(&id, |tab| tab.pending = Some(url))?;
                    wake(&id);
                }
                return Ok(json!({}));
            }
            if !attached {
                return Ok(json!({}));
            }
            match params["action"].as_str().unwrap_or("reload") {
                "stop" => post("Page.stopLoading", json!({}), &cdp),
                "back" | "forward" => {
                    let step: i64 = if params["action"] == "back" { -1 } else { 1 };
                    std::thread::spawn(move || {
                        let Ok(history) = call("Page.getNavigationHistory", json!({}), &cdp) else { return };
                        let index = history["currentIndex"].as_i64().unwrap_or(0) + step;
                        if let Some(entry) = history["entries"].as_array().and_then(|entries| entries.get(usize::try_from(index).ok()?)) {
                            post("Page.navigateToHistoryEntry", json!({ "entryId": entry["id"] }), &cdp);
                        }
                    });
                }
                _ => post("Page.reload", json!({}), &cdp),
            }
        }
        "browser.watch" | "browser.unwatch" => {
            // 一个连接同时只看一个标签：换标签时别的标签不再推给它
            let watching = method == "browser.watch";
            {
                let mut inner = BROWSER.inner.lock().unwrap();
                for tab in inner.tabs.iter_mut() {
                    if tab.id != id || !watching {
                        tab.watchers.remove(watcher);
                    }
                }
                if watching && let Some(tab) = inner.tabs.iter_mut().find(|tab| tab.id == id) {
                    tab.watchers.insert(watcher.to_string(), (size("width", 1280, 3840), size("height", 800, 2160)));
                }
            }
            let all: Vec<String> = BROWSER.inner.lock().unwrap().tabs.iter().map(|tab| tab.id.clone()).collect();
            for tab in all {
                screencast(&tab);
            }
            if watching {
                wake(&id);
            }
        }
        "browser.container" => {
            let (width, height) = (number("width") as u32, number("height") as u32);
            if width >= 10
                && height >= 10
                && with_tab(&id, |tab| std::mem::replace(&mut tab.container, (width, height)) != (width, height) && tab.fixed.is_none())?
            {
                apply_viewport(&id);
                changed();
            }
        }
        "browser.fit" => {
            with_tab(&id, |tab| tab.fixed = None)?;
            apply_viewport(&id);
            changed();
            save();
        }
        "browser.dialog" => {
            let cdp = with_tab(&id, |tab| {
                tab.dialog = None;
                tab.cdp.clone()
            })?;
            post(
                "Page.handleJavaScriptDialog",
                json!({ "accept": params["accept"].as_bool().unwrap_or(true), "promptText": params["text"].as_str().unwrap_or("") }),
                &cdp,
            );
            changed();
        }
        // 智能体请用户做事（交给用户）：EDITH 的智能体还不会，点了「好了」照常
        "browser.handOverDone" => {}
        "browser.input" => {
            let cdp = with_tab(&id, |tab| tab.cdp.clone())?;
            if cdp.is_empty() {
                wake(&id);
                return Ok(json!({}));
            }
            let qt = params["modifiers"].as_u64().unwrap_or(0);
            match params["kind"].as_str().unwrap_or("") {
                kind @ ("mouse" | "wheel") => {
                    let kind_type = params["type"].as_str().unwrap_or("move");
                    let (event, pressed) = match (kind, kind_type) {
                        ("wheel", _) => ("mouseWheel", false),
                        (_, "press") => ("mousePressed", true),
                        (_, "release") => ("mouseReleased", true),
                        _ => ("mouseMoved", false),
                    };
                    let button = params["button"].as_str().unwrap_or("none");
                    let mut mouse = json!({
                        "type": event, "x": number("x"), "y": number("y"),
                        "button": if pressed || button != "none" { button } else { "none" },
                        "clickCount": if pressed { params["clicks"].as_u64().unwrap_or(1).max(1) } else { 0 },
                        "modifiers": modifiers(qt),
                    });
                    if event == "mouseWheel" {
                        mouse["deltaX"] = json!(number("dx"));
                        mouse["deltaY"] = json!(number("dy"));
                    }
                    post("Input.dispatchMouseEvent", mouse, &cdp);
                }
                "key" => {
                    let press = params["press"].as_bool().unwrap_or(true);
                    let key = params["key"].as_str().unwrap_or("");
                    let code = params["code"].as_str().unwrap_or("");
                    let text = params["text"].as_str().unwrap_or("");
                    let kind = if !press {
                        "keyUp"
                    } else if text.is_empty() {
                        "rawKeyDown"
                    } else {
                        "keyDown"
                    };
                    let mut event = json!({ "type": kind, "key": key, "code": code, "windowsVirtualKeyCode": virtual_key(code, key), "modifiers": modifiers(qt) });
                    if press && !text.is_empty() {
                        event["text"] = json!(text);
                    }
                    post("Input.dispatchKeyEvent", event, &cdp);
                }
                "text" => post("Input.insertText", json!({ "text": params["text"].as_str().unwrap_or("") }), &cdp),
                _ => {}
            }
        }
        other => return Err(format!("没有 {other}")),
    }
    Ok(json!({}))
}

/// 连接断了：它看着的不再推给它
pub fn forget(watcher: &str) {
    let ids: Vec<String> = {
        let mut inner = BROWSER.inner.lock().unwrap();
        inner.tabs.iter_mut().filter_map(|tab| tab.watchers.remove(watcher).map(|_| tab.id.clone())).collect()
    };
    for id in ids {
        screencast(&id);
    }
}

/// Windows：DevTools 的 WebSocket（只连本机，不用 TLS；够 CDP 用的那一点：文本帧、分片、ping）
#[cfg(windows)]
mod socket {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpStream;

    pub struct Socket(TcpStream);

    pub struct Reader(BufReader<TcpStream>, TcpStream);

    pub fn connect(address: &str, path: &str) -> Result<(Socket, Reader), String> {
        use base64::Engine;
        let mut stream = TcpStream::connect(address).map_err(|error| format!("连不上浏览器 {address}：{error}"))?;
        let key = base64::engine::general_purpose::STANDARD.encode(uuid::Uuid::new_v4().as_bytes());
        let request = format!(
            "GET {path} HTTP/1.1\r\nHost: {address}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
        );
        stream.write_all(request.as_bytes()).map_err(|error| error.to_string())?;
        let mut reader = BufReader::new(stream.try_clone().map_err(|error| error.to_string())?);
        let mut status = String::new();
        reader.read_line(&mut status).map_err(|error| error.to_string())?;
        if !status.contains(" 101 ") {
            return Err(format!("浏览器不肯升级成 WebSocket：{}", status.trim()));
        }
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).map_err(|error| error.to_string())? == 0 || line == "\r\n" {
                break;
            }
        }
        let pong = stream.try_clone().map_err(|error| error.to_string())?;
        Ok((Socket(stream), Reader(reader, pong)))
    }

    fn frame(opcode: u8, payload: &[u8]) -> Vec<u8> {
        let mut out = vec![0x80 | opcode];
        let length = payload.len();
        if length < 126 {
            out.push(0x80 | length as u8);
        } else if length <= u16::MAX as usize {
            out.push(0x80 | 126);
            out.extend_from_slice(&(length as u16).to_be_bytes());
        } else {
            out.push(0x80 | 127);
            out.extend_from_slice(&(length as u64).to_be_bytes());
        }
        let mask = *uuid::Uuid::new_v4().as_bytes();
        out.extend_from_slice(&mask[..4]);
        out.extend(payload.iter().enumerate().map(|(index, byte)| byte ^ mask[index % 4]));
        out
    }

    impl Socket {
        pub fn send_text(&mut self, text: &str) -> std::io::Result<()> {
            self.0.write_all(&frame(0x1, text.as_bytes()))?;
            self.0.flush()
        }
    }

    impl Reader {
        /// 下一条文本消息（分片的拼好）；断了是 None
        pub fn next_text(&mut self) -> Option<String> {
            let mut message = Vec::new();
            loop {
                let mut head = [0u8; 2];
                self.0.read_exact(&mut head).ok()?;
                let (fin, opcode) = (head[0] & 0x80 != 0, head[0] & 0x0f);
                let mut length = (head[1] & 0x7f) as u64;
                if length == 126 {
                    let mut bytes = [0u8; 2];
                    self.0.read_exact(&mut bytes).ok()?;
                    length = u16::from_be_bytes(bytes) as u64;
                } else if length == 127 {
                    let mut bytes = [0u8; 8];
                    self.0.read_exact(&mut bytes).ok()?;
                    length = u64::from_be_bytes(bytes);
                }
                let mask = if head[1] & 0x80 != 0 {
                    let mut bytes = [0u8; 4];
                    self.0.read_exact(&mut bytes).ok()?;
                    Some(bytes)
                } else {
                    None
                };
                let mut payload = vec![0u8; length as usize];
                self.0.read_exact(&mut payload).ok()?;
                if let Some(mask) = mask {
                    for (index, byte) in payload.iter_mut().enumerate() {
                        *byte ^= mask[index % 4];
                    }
                }
                match opcode {
                    0x8 => return None,
                    0x9 => {
                        let _ = self.1.write_all(&frame(0xA, &payload));
                        continue;
                    }
                    0xA => continue,
                    _ => message.extend(payload),
                }
                if fin {
                    return String::from_utf8(message).ok();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_bar_and_keys() {
        assert_eq!(normalize("example.com/a"), "https://example.com/a");
        assert_eq!(normalize("localhost:8080"), "http://localhost:8080");
        assert_eq!(normalize("192.168.1.2/x"), "http://192.168.1.2/x");
        assert_eq!(normalize("https://a.b/c"), "https://a.b/c");
        assert_eq!(normalize("about:blank"), "about:blank");
        assert_eq!(normalize("怎么 煮饭"), "https://www.bing.com/search?q=%E6%80%8E%E4%B9%88%20%E7%85%AE%E9%A5%AD");
        assert_eq!((virtual_key("Enter", "Enter"), virtual_key("KeyA", "a"), virtual_key("F5", "F5")), (13, 65, 116));
        assert_eq!(modifiers(0x0400_0000 | 0x0200_0000), 2 | 8);
        assert_eq!(display_title("example.com/", "https://example.com/"), "");
    }
}

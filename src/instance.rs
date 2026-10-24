//! 在跑的大脑实例（按目录工作：`edith .`；目录不加锁——要并行又互不干扰就用 git worktree 各开一个目录）：
//! - 本机接入：127.0.0.1 上随机端口，自己打开的程序（终端 `edith attach`、桌面端……）连上来先报令牌（`auth`），
//!   之后讲能力协议，一条连接就是一个能力（capabilities::accept）；
//! - 登记：`~/.edith/instances/<目录的键>-<进程号>.json`：哪个目录、哪个进程、接入地址和令牌、接着哪些能力、
//!   能力对外开的入口（WebSocket 地址等）。只有本用户能读；进程没了的登记在列出时清掉。

use std::io::{BufRead, BufReader, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use serde_json::{Value, json};

use crate::{capabilities, home};

/// 接入地址和令牌（serve 之后才有）
static SERVING: Mutex<Option<(String, String)>> = Mutex::new(None);

/// 这个大脑的 id：工作目录的键（同一个目录重启后不变，能力拿它认出是同一个大脑）
pub fn brain_id() -> String {
    home::workspace_key(&home::workspace())
}

fn registry_dir() -> PathBuf {
    home::dir().join("instances")
}

fn registry_path() -> PathBuf {
    registry_dir().join(format!("{}-{}.json", brain_id(), std::process::id()))
}

/// 开本机接入、写登记
pub fn serve() -> Result<(), String> {
    let listener = TcpListener::bind("127.0.0.1:0").map_err(|error| format!("开不了本机接入：{error}"))?;
    let address = listener.local_addr().map_err(|error| error.to_string())?.to_string();
    let token = uuid::Uuid::new_v4().simple().to_string();
    *SERVING.lock().unwrap() = Some((address, token.clone()));
    refresh();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let token = token.clone();
            std::thread::spawn(move || accept(stream, &token));
        }
    });
    Ok(())
}

/// 连进来的一条：第一行要是 auth（令牌对），之后交给 capabilities::accept 握手
fn accept(stream: TcpStream, token: &str) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let Ok(reading) = stream.try_clone() else { return };
    let mut reader = BufReader::new(reading);
    let mut line = String::new();
    let authorized = reader.read_line(&mut line).is_ok()
        && serde_json::from_str::<Value>(&line).is_ok_and(|message| message["method"] == "auth" && message["params"]["token"] == token);
    if !authorized {
        let refusal =
            crate::protocol::error(&Value::Null, crate::protocol::INVALID_REQUEST, "先发 auth（令牌在 ~/.edith/instances/ 的登记里）");
        let _ = writeln!(&stream, "{refusal}");
        let _ = stream.shutdown(Shutdown::Both);
        return;
    }
    let _ = stream.set_read_timeout(None);
    if let Err(error) = capabilities::accept(Box::new(reader), Box::new(SocketWriter(stream))) {
        eprintln!("\x1b[33m连进来的没握上手：{error}\x1b[0m");
    }
}

/// 写的一头：丢掉时把整条连接关掉（读的那头随之读到结束）
pub struct SocketWriter(pub TcpStream);

impl Write for SocketWriter {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.0.write(buffer)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

impl Drop for SocketWriter {
    fn drop(&mut self) {
        let _ = self.0.shutdown(Shutdown::Both);
    }
}

/// 重写登记（能力接上、断开时调）
pub fn refresh() {
    let Some((address, token)) = SERVING.lock().unwrap().clone() else { return };
    let entry = json!({
        "id": brain_id(),
        "workspace": home::workspace().to_string_lossy(),
        "pid": std::process::id(),
        "version": env!("CARGO_PKG_VERSION"),
        "address": address,
        "token": token,
        "started": started(),
        "capabilities": capabilities::names(),
        "endpoints": capabilities::endpoints(),
    });
    let dir = registry_dir();
    let _ = std::fs::create_dir_all(&dir);
    restrict(&dir, 0o700);
    let path = registry_path();
    if home::write_json(&path, &entry).is_ok() {
        restrict(&path, 0o600);
    }
}

fn started() -> String {
    static STARTED: OnceLock<String> = OnceLock::new();
    STARTED.get_or_init(|| chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, false)).clone()
}

#[cfg(unix)]
fn restrict(path: &std::path::Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
}

#[cfg(not(unix))]
fn restrict(_: &std::path::Path, _: u32) {}

/// 退出前：删登记
pub fn release() {
    if SERVING.lock().unwrap().take().is_some() {
        let _ = std::fs::remove_file(registry_path());
    }
}

/// 登记的那个还活着（接入地址连得上）
fn alive(entry: &Value) -> bool {
    let Some(address) = entry["address"].as_str().and_then(|address| address.parse().ok()) else { return false };
    TcpStream::connect_timeout(&address, Duration::from_millis(500)).is_ok()
}

/// 在 workspace 这个目录跑着的大脑（新起的在前）
pub fn running_in(workspace: &std::path::Path) -> Vec<Value> {
    let wanted = workspace.to_string_lossy();
    let mut found: Vec<Value> = list().into_iter().filter(|entry| entry["workspace"].as_str() == Some(wanted.as_ref())).collect();
    found.sort_by(|a, b| b["started"].as_str().cmp(&a["started"].as_str()));
    found
}

/// 这台机器上在跑的大脑（顺便清掉进程已经没了的登记）
pub fn list() -> Vec<Value> {
    let mut running = Vec::new();
    for entry in std::fs::read_dir(registry_dir()).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|extension| extension != "json") {
            continue;
        }
        match home::read_json(&path) {
            Some(value) if alive(&value) => running.push(value),
            _ => {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
    running.sort_by(|a, b| a["workspace"].as_str().cmp(&b["workspace"].as_str()));
    running
}

/// 连到一个在跑的大脑的本机接入（报好令牌）
pub fn connect(entry: &Value) -> Result<TcpStream, String> {
    let address = entry["address"].as_str().ok_or("登记里没有接入地址")?;
    let mut stream = TcpStream::connect(address).map_err(|error| format!("连不上 {address}：{error}"))?;
    let auth = json!({ "jsonrpc": "2.0", "method": "auth", "params": { "token": entry["token"] } });
    writeln!(stream, "{auth}").map_err(|error| error.to_string())?;
    Ok(stream)
}

//! edith-websocket：WebSocket 能力（docs/WEBSOCKET.md）。EDITH 经 stdio 起它（能力协议，docs/CAPABILITY-PROTOCOL.md），
//! 它对外开一个 WebSocket 端口：远程的客户端（网页、手机、别的机器上的程序）连上来交事（ask / sense）、管会话、
//! 实时收干活的过程（event）。它只讲协议、不用大脑的代码——别人照着文档也能写一个一样的。
//!
//! 用法：`edith capabilities add ws edith-websocket --listen 127.0.0.1:7789 --token <令牌>`
//! 不是只听本机（127.0.0.1 / ::1 / localhost）时必须给令牌（或环境变量 EDITH_WEBSOCKET_TOKEN）。

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tungstenite::{Message, WebSocket};

const VERSION: &str = env!("CARGO_PKG_VERSION");
/// 客户端能用的方法（原样转给 EDITH）
const FORWARDED: &[&str] = &["ask", "sense", "sessions.list", "sessions.create", "sessions.get", "sessions.focus", "sessions.stop", "ping"];

struct Client {
    name: String,
    outgoing: Sender<String>,
    /// None：所有会话
    sessions: Option<Vec<String>>,
    /// None：所有事件
    types: Option<Vec<String>>,
}

static CLIENTS: LazyLock<Mutex<HashMap<u64, Client>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
/// 转给 EDITH 的请求：我们的编号 → (客户端, 客户端原来的编号)
static FORWARDS: LazyLock<Mutex<HashMap<u64, (u64, Value)>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
static NEXT_CLIENT: AtomicU64 = AtomicU64::new(1);
static NEXT_REQUEST: AtomicU64 = AtomicU64::new(1);
static STDOUT: Mutex<()> = Mutex::new(());

/// 日志写标准错误（EDITH 收进 ~/.edith/logs/<能力名>.log）
fn log(text: &str) {
    eprintln!("{text}");
}

/// 发给 EDITH（一行一条）
fn to_brain(message: &Value) {
    let _guard = STDOUT.lock().unwrap();
    let mut stdout = std::io::stdout().lock();
    let _ = writeln!(stdout, "{message}");
    let _ = stdout.flush();
}

fn to_client(client: u64, message: &Value) {
    if let Some(entry) = CLIENTS.lock().unwrap().get(&client) {
        let _ = entry.outgoing.send(message.to_string());
    }
}

fn rpc_error(id: &Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

struct Settings {
    listen: String,
    token: Option<String>,
}

fn settings() -> Result<Settings, String> {
    let mut listen = "127.0.0.1:7789".to_string();
    let mut token = std::env::var("EDITH_WEBSOCKET_TOKEN").ok().filter(|token| !token.is_empty());
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--listen" => listen = arguments.next().ok_or("--listen 后面要跟地址")?,
            "--token" => token = Some(arguments.next().ok_or("--token 后面要跟令牌")?),
            "-V" | "--version" => {
                println!("edith-websocket {VERSION}");
                std::process::exit(0);
            }
            other => return Err(format!("不认识 {other}（--listen 地址:端口、--token 令牌）")),
        }
    }
    let host = listen.rsplit_once(':').map(|(host, _)| host.trim_matches(['[', ']'])).unwrap_or("");
    let local = matches!(host, "127.0.0.1" | "::1" | "localhost");
    if !local && token.is_none() {
        return Err(format!("{listen} 不只是本机能连：要给 --token（或环境变量 EDITH_WEBSOCKET_TOKEN）"));
    }
    Ok(Settings { listen, token })
}

fn main() {
    let settings = match settings() {
        Ok(settings) => settings,
        Err(message) => {
            log(&message);
            std::process::exit(2);
        }
    };
    let listener = match TcpListener::bind(&settings.listen) {
        Ok(listener) => listener,
        Err(error) => {
            log(&format!("开不了 {}：{error}", settings.listen));
            std::process::exit(1);
        }
    };
    let address = listener.local_addr().map(|address| address.to_string()).unwrap_or(settings.listen.clone());
    let token = Arc::new(settings.token);
    // 握手（initialize → initialized）之后才接客户端
    let (ready_sender, ready) = mpsc::channel::<()>();
    let description = format!("在 ws://{address} 接远程客户端：交事（ask / sense）、管会话、实时收干活的过程");
    std::thread::spawn(move || brain_loop(ready_sender, description));
    if ready.recv().is_err() {
        return;
    }
    log(&format!("在 ws://{address} 等客户端"));
    for stream in listener.incoming().flatten() {
        let token = token.clone();
        std::thread::spawn(move || serve_client(stream, &token));
    }
}

/// 读 EDITH 发来的（stdin 一行一条）
fn brain_loop(ready: Sender<()>, description: String) {
    let mut ready = Some(ready);
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        let Ok(message) = serde_json::from_str::<Value>(&line) else { continue };
        let method = message.get("method").and_then(Value::as_str);
        let id = message.get("id").filter(|id| !id.is_null());
        match (method, id) {
            (Some("initialize"), Some(id)) => to_brain(&json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": "1",
                    "capability": { "title": "WebSocket", "version": VERSION, "description": description },
                    "senses": [{ "kind": "message", "description": "远程客户端经 WebSocket 交来的话" }],
                    "actions": [],
                    "receives": ["*"],
                    "sessions": "all",
                },
            })),
            (Some("ping"), Some(id)) => to_brain(&json!({ "jsonrpc": "2.0", "id": id, "result": {} })),
            (Some(other), Some(id)) => to_brain(&rpc_error(id, -32601, &format!("WebSocket 能力没有 {other}"))),
            (Some("initialized"), None) => {
                if let Some(ready) = ready.take() {
                    let _ = ready.send(());
                }
            }
            (Some("event"), None) => broadcast(message.get("params").unwrap_or(&Value::Null)),
            (Some("shutdown"), None) => std::process::exit(0),
            (Some(_), None) => {}
            (None, Some(id)) => {
                // EDITH 回了一个我们转过去的请求：还给那个客户端、换回它原来的编号
                let Some(id) = id.as_u64() else { continue };
                let Some((client, original)) = FORWARDS.lock().unwrap().remove(&id) else { continue };
                let mut reply = message.clone();
                reply["id"] = original;
                to_client(client, &reply);
            }
            (None, None) => {}
        }
    }
    // EDITH 关了 stdin：退出
    std::process::exit(0);
}

fn broadcast(event: &Value) {
    let session = event.get("session").and_then(Value::as_str).unwrap_or("");
    let kind = event.get("type").and_then(Value::as_str).unwrap_or("");
    let message = json!({ "jsonrpc": "2.0", "method": "event", "params": event }).to_string();
    for client in CLIENTS.lock().unwrap().values() {
        let session_ok = client.sessions.as_ref().is_none_or(|sessions| sessions.iter().any(|item| item == session));
        let type_ok = client.types.as_ref().is_none_or(|types| types.iter().any(|item| item == kind));
        if session_ok && type_ok {
            let _ = client.outgoing.send(message.clone());
        }
    }
}

fn query(uri: &str, key: &str) -> Option<String> {
    let query = uri.split_once('?')?.1;
    query.split('&').filter_map(|pair| pair.split_once('=')).find(|(name, _)| *name == key).map(|(_, value)| percent_decode(value))
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte == b'%'
            && index + 2 < bytes.len()
            && let Ok(decoded) = u8::from_str_radix(std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("zz"), 16)
        {
            out.push(decoded);
            index += 3;
            continue;
        }
        out.push(if byte == b'+' { b' ' } else { byte });
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

// 握手回调的返回类型是 tungstenite 定的
#[allow(clippy::result_large_err)]
fn serve_client(stream: TcpStream, token: &Option<String>) {
    let peer = stream.peer_addr().map(|address| address.to_string()).unwrap_or_default();
    let mut name = String::new();
    let check = |request: &Request, response: Response| -> Result<Response, ErrorResponse> {
        let uri = request.uri().to_string();
        name = query(&uri, "name").unwrap_or_default();
        if let Some(token) = token {
            let header = request
                .headers()
                .get("authorization")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.strip_prefix("Bearer "));
            let given = header.map(str::to_string).or_else(|| query(&uri, "token"));
            if given.as_deref().map(str::trim) != Some(token.as_str()) {
                let mut refused = ErrorResponse::new(Some("要令牌：?token=… 或 Authorization: Bearer …".into()));
                *refused.status_mut() = tungstenite::http::StatusCode::UNAUTHORIZED;
                return Err(refused);
            }
        }
        Ok(response)
    };
    let mut socket = match tungstenite::accept_hdr(stream, check) {
        Ok(socket) => socket,
        Err(error) => {
            log(&format!("{peer} 没连上：{error}"));
            return;
        }
    };
    let _ = socket.get_ref().set_read_timeout(Some(Duration::from_millis(30)));
    let id = NEXT_CLIENT.fetch_add(1, Ordering::Relaxed);
    if name.is_empty() {
        name = format!("websocket-{id}");
    }
    let (outgoing, incoming) = mpsc::channel::<String>();
    CLIENTS.lock().unwrap().insert(id, Client { name: name.clone(), outgoing, sessions: None, types: None });
    log(&format!("{name}（{peer}）连上了"));
    client_loop(&mut socket, id, &incoming);
    CLIENTS.lock().unwrap().remove(&id);
    FORWARDS.lock().unwrap().retain(|_, (client, _)| *client != id);
    log(&format!("{name} 断开了"));
}

fn client_loop(socket: &mut WebSocket<TcpStream>, client: u64, incoming: &Receiver<String>) {
    loop {
        match socket.read() {
            Ok(Message::Text(text)) => {
                if let Some(reply) = handle_client(client, text.as_str()) {
                    let _ = socket.send(Message::text(reply.to_string()));
                }
            }
            Ok(Message::Close(_)) => return,
            Ok(_) => {}
            Err(tungstenite::Error::Io(error)) if matches!(error.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {
            }
            Err(_) => return,
        }
        while let Ok(message) = incoming.try_recv() {
            if socket.send(Message::text(message)).is_err() {
                return;
            }
        }
        // 读超时时没写出去的（pong 等）补上
        let _ = socket.flush();
    }
}

/// 客户端发来的一条：要直接回的返回回话；转给 EDITH 的返回 None（EDITH 回了再还给它）
fn handle_client(client: u64, text: &str) -> Option<Value> {
    let Ok(message) = serde_json::from_str::<Value>(text) else { return Some(rpc_error(&Value::Null, -32700, "不是 JSON")) };
    let id = message.get("id").cloned().filter(|id| !id.is_null());
    let Some(method) = message.get("method").and_then(Value::as_str) else {
        return Some(rpc_error(&id.unwrap_or(Value::Null), -32600, "要有 method"));
    };
    let mut params = message.get("params").cloned().unwrap_or_else(|| json!({}));
    if method == "subscribe" {
        let list = |key: &str| -> Option<Vec<String>> {
            match params.get(key) {
                Some(Value::Array(items)) => Some(items.iter().filter_map(Value::as_str).map(str::to_string).collect()),
                _ => None,
            }
        };
        if let Some(entry) = CLIENTS.lock().unwrap().get_mut(&client) {
            entry.sessions = list("sessions");
            entry.types = list("types");
        }
        return id.map(|id| json!({ "jsonrpc": "2.0", "id": id, "result": {} }));
    }
    if !FORWARDED.contains(&method) {
        return id.map(|id| rpc_error(&id, -32601, &format!("没有这个方法：{method}")));
    }
    // 交来的话标上是哪个客户端
    if matches!(method, "ask" | "sense") && params.get("from").is_none() {
        let name = CLIENTS.lock().unwrap().get(&client).map(|entry| entry.name.clone()).unwrap_or_default();
        params["from"] = json!(name);
        if params.get("kind").is_none() {
            params["kind"] = json!("message");
        }
    }
    match id {
        Some(id) => {
            let forwarded = NEXT_REQUEST.fetch_add(1, Ordering::Relaxed);
            FORWARDS.lock().unwrap().insert(forwarded, (client, id));
            to_brain(&json!({ "jsonrpc": "2.0", "id": forwarded, "method": method, "params": params }));
        }
        None => to_brain(&json!({ "jsonrpc": "2.0", "method": method, "params": params })),
    }
    None
}

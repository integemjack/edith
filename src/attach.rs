//! 接到这个目录正在跑的大脑上（同一个目录第二次 `edith .` / `edith "…"`）：经它的本机接入连上，自己当一个能力
//! （名字 terminal、订焦点会话的全部事件），终端里打的字用 ask / sense 交过去，过程照常打出来。

use std::collections::HashMap;
use std::io::{BufRead, BufReader, IsTerminal, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use crate::instance;
use crate::terminal::Printer;

struct Link {
    stream: Mutex<TcpStream>,
    pending: Mutex<HashMap<u64, Sender<Value>>>,
    next: AtomicU64,
    /// 焦点会话在干的那一轮（干活时打的字算补充）
    working: Mutex<Option<String>>,
}

impl Link {
    fn send(&self, message: &Value) {
        let mut stream = self.stream.lock().unwrap();
        let _ = writeln!(stream, "{message}");
        let _ = stream.flush();
    }

    fn request(&self, method: &str, params: Value) -> Value {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = mpsc::channel();
        self.pending.lock().unwrap().insert(id, sender);
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        receiver.recv().unwrap_or_else(|_| json!({ "error": { "message": "和大脑的连接断了" } }))
    }
}

/// 接上去。task：一次干完就退的那件事（None：一句一句聊）。interrupted：Ctrl+C 的标记（main 装的处理）
pub fn run(entry: &Value, task: Option<String>, quiet: bool, verbose: bool, interrupted: &'static AtomicBool) -> Result<(), String> {
    let stream = instance::connect(entry)?;
    let reading = stream.try_clone().map_err(|error| error.to_string())?;
    let link = Arc::new(Link {
        stream: Mutex::new(stream),
        pending: Mutex::new(HashMap::new()),
        next: AtomicU64::new(1),
        working: Mutex::new(None),
    });
    let printer = Arc::new(Mutex::new(Printer::new(quiet, verbose)));
    let (ready_sender, ready) = mpsc::channel::<()>();
    {
        let link = link.clone();
        let printer = printer.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(reading).lines() {
                let Ok(line) = line else { break };
                let Ok(message) = serde_json::from_str::<Value>(&line) else { continue };
                handle(&link, &printer, &message, &ready_sender);
            }
            // 大脑那边关了
            eprintln!("\x1b[33m（和大脑的连接断了）\x1b[0m");
            std::process::exit(1);
        });
    }
    ready.recv_timeout(std::time::Duration::from_secs(30)).map_err(|_| "大脑没有回握手".to_string())?;
    // Ctrl+C：停下焦点会话正在干的一轮；闲着时退出
    {
        let link = link.clone();
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(std::time::Duration::from_millis(100));
                if !interrupted.swap(false, Ordering::SeqCst) {
                    continue;
                }
                if link.working.lock().unwrap().is_some() {
                    link.request("sessions.stop", json!({}));
                } else {
                    std::process::exit(130);
                }
            }
        });
    }
    let workspace = entry["workspace"].as_str().unwrap_or("");
    if let Some(task) = task {
        if !quiet {
            eprintln!("\x1b[2m（接到 {workspace} 正在跑的大脑上）\x1b[0m");
        }
        // 和本机干一件事一样：新开一个会话
        let reply = link.request("ask", json!({ "text": task, "session": "new" }));
        if let Some(error) = reply["error"]["message"].as_str() {
            return Err(error.to_string());
        }
        return reply["result"]["error"].as_str().map_or(Ok(()), |error| Err(error.to_string()));
    }
    eprintln!("接到 {workspace} 正在跑的大脑上（进程 {}）。说要做的事；干活时再打字算补充；空行或 Ctrl+D 结束。", entry["pid"]);
    let stdin = std::io::stdin();
    loop {
        if link.working.lock().unwrap().is_none() && stdin.is_terminal() {
            eprint!("\x1b[1m> \x1b[0m");
            let _ = std::io::stderr().flush();
        }
        let mut line = String::new();
        if stdin.lock().read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
            break;
        }
        let urgency = if link.working.lock().unwrap().is_some() { "interject" } else { "queue" };
        let reply = link.request("sense", json!({ "text": line.trim(), "urgency": urgency }));
        match (reply["result"]["disposition"].as_str(), reply["error"]["message"].as_str()) {
            (Some("interjected"), _) => eprintln!("\x1b[2m（补充进了正在做的这一轮）\x1b[0m"),
            (_, Some(error)) => eprintln!("\x1b[31m{error}\x1b[0m"),
            _ => {}
        }
        // 刚开始的这一轮先打一会儿
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    Ok(())
}

fn handle(link: &Link, printer: &Mutex<Printer>, message: &Value, ready: &Sender<()>) {
    let method = message["method"].as_str();
    let id = message.get("id").filter(|id| !id.is_null());
    match (method, id) {
        (Some("initialize"), Some(id)) => link.send(&json!({ "jsonrpc": "2.0", "id": id, "result": {
            "protocolVersion": "1",
            "name": "terminal",
            "capability": { "title": "终端", "version": env!("CARGO_PKG_VERSION"), "description": "接进来的命令行" },
            "senses": [{ "kind": "terminal", "description": "终端里打的字" }],
            "actions": [],
            "receives": ["*"],
            "sessions": "focus",
        }})),
        (Some("initialized"), None) => {
            let _ = ready.send(());
        }
        (Some("event"), None) => {
            let event = &message["params"];
            let kind = event["type"].as_str().unwrap_or("");
            let turn = event["turn"].as_str().unwrap_or("").to_string();
            match kind {
                "turn.started" => *link.working.lock().unwrap() = Some(turn),
                "done" => {
                    let mut working = link.working.lock().unwrap();
                    if working.as_deref() == Some(turn.as_str()) {
                        *working = None;
                    }
                }
                _ => {}
            }
            // 自己打的字不用再显示一遍
            let own =
                kind == "turn.started" && event["data"]["origin"]["capability"].as_str().is_some_and(|name| name.starts_with("terminal"));
            if !own {
                printer.lock().unwrap().show(kind, event["session"].as_str().unwrap_or(""), &event["data"], false);
            }
        }
        (Some("shutdown"), None) => {
            eprintln!("\x1b[33m（大脑退出了）\x1b[0m");
            std::process::exit(0);
        }
        (Some("ping"), Some(id)) => link.send(&json!({ "jsonrpc": "2.0", "id": id, "result": {} })),
        (Some(other), Some(id)) => {
            link.send(&json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": format!("终端没有 {other}") } }));
        }
        (None, Some(id)) => {
            let sender = id.as_u64().and_then(|id| link.pending.lock().unwrap().remove(&id));
            if let Some(sender) = sender {
                let _ = sender.send(message.clone());
            }
        }
        _ => {}
    }
}

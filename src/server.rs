//! 对外接口（`edith serve`）：HTTP + JSON，干活的过程按 SSE 推。身体四肢（桌面客户端、聊天渠道、语音、网页……）都走这里；
//! 能力四肢（MCP）也能经这里挂上 / 摘下。路径与事件见 docs/EDITH.md「对外接口」。

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

use serde_json::{Value, json};
use tiny_http::{Header, Method, Request, Response, Server};

use crate::brain::{self, Event};
use crate::home::Model;
use crate::limbs;
use crate::sessions::{self, Session};

/// 正在干活的会话 → 停下的开关
static RUNNING: LazyLock<Mutex<HashMap<String, Arc<AtomicBool>>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

pub fn serve(listen: &str, token: Option<String>) -> Result<(), String> {
    let server = Server::http(listen).map_err(|error| format!("开不了 {listen}：{error}"))?;
    eprintln!("EDITH 在 http://{listen} 等着（{}）", if token.is_some() { "要令牌" } else { "不要令牌，只该听本机" });
    let token = Arc::new(token);
    for request in server.incoming_requests() {
        let token = token.clone();
        std::thread::spawn(move || handle(request, &token));
    }
    Ok(())
}

fn json_response(status: u16, value: &Value) -> Response<std::io::Cursor<Vec<u8>>> {
    Response::from_data(value.to_string().into_bytes())
        .with_status_code(status)
        .with_header(Header::from_bytes("Content-Type", "application/json; charset=utf-8").unwrap())
}

fn error(status: u16, message: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    json_response(status, &json!({ "error": message }))
}

fn authorized(request: &Request, token: &Option<String>) -> bool {
    let Some(token) = token else { return true };
    request.headers().iter().any(|header| {
        header.field.equiv("Authorization") && header.value.as_str().strip_prefix("Bearer ").is_some_and(|given| given.trim() == token)
    })
}

fn read_body(request: &mut Request) -> Result<Value, String> {
    let mut text = String::new();
    request.as_reader().take(16 * 1024 * 1024).read_to_string(&mut text).map_err(|error| error.to_string())?;
    if text.trim().is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_str(&text).map_err(|error| format!("请求体不是 JSON：{error}"))
}

fn handle(mut request: Request, token: &Option<String>) {
    if !authorized(&request, token) {
        let _ = request.respond(error(401, "要令牌：Authorization: Bearer <token>"));
        return;
    }
    let method = request.method().clone();
    let path = request.url().split('?').next().unwrap_or("").trim_end_matches('/').to_string();
    let parts: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
    let body = match method {
        Method::Post => match read_body(&mut request) {
            Ok(body) => body,
            Err(message) => {
                let _ = request.respond(error(400, &message));
                return;
            }
        },
        _ => json!({}),
    };
    let response = match (&method, parts.as_slice()) {
        (Method::Get, ["v1", "health"]) => {
            let model = Model::load().map(|model| model.model).unwrap_or_default();
            json_response(200, &json!({ "name": "EDITH", "version": env!("CARGO_PKG_VERSION"), "model": model }))
        }
        (Method::Get, ["v1", "sessions"]) => json_response(200, &Value::Array(sessions::list().iter().map(Session::summary).collect())),
        (Method::Post, ["v1", "sessions"]) => {
            let cwd = body.get("cwd").and_then(Value::as_str).map(str::to_string).unwrap_or_else(default_cwd);
            if !std::path::Path::new(&cwd).is_dir() {
                error(400, &format!("工作目录不存在：{cwd}"))
            } else {
                let mut session = Session::new(&cwd);
                match session.save() {
                    Ok(()) => json_response(201, &session.summary()),
                    Err(message) => error(500, &message),
                }
            }
        }
        (Method::Get, ["v1", "sessions", id]) => match Session::load(id) {
            Some(session) => json_response(200, &session.to_json()),
            None => error(404, "没有这个会话"),
        },
        (Method::Post, ["v1", "sessions", id, "messages"]) => return run_turn(request, id, &body),
        (Method::Post, ["v1", "sessions", id, "stop"]) => match RUNNING.lock().unwrap().get(*id) {
            Some(cancel) => {
                cancel.store(true, Ordering::Relaxed);
                json_response(200, &json!({ "stopping": true }))
            }
            None => json_response(200, &json!({ "stopping": false })),
        },
        (Method::Get, ["v1", "limbs"]) => json_response(200, &limbs_json()),
        (Method::Post, ["v1", "limbs"]) => match limbs::attach(body, true) {
            Ok(limb) => json_response(201, &limb_json(&limb)),
            Err(message) => error(400, &message),
        },
        (Method::Delete, ["v1", "limbs", name]) => {
            if limbs::detach(name, true) {
                json_response(200, &json!({ "removed": name }))
            } else {
                error(404, "没有挂这个四肢")
            }
        }
        _ => error(404, "没有这个接口（见 docs/EDITH.md「对外接口」）"),
    };
    let _ = request.respond(response);
}

fn default_cwd() -> String {
    crate::home::user_home().to_string_lossy().into_owned()
}

fn limb_json(limb: &limbs::Limb) -> Value {
    json!({ "name": limb.name, "config": limb.config, "tools": limb.tools })
}

fn limbs_json() -> Value {
    Value::Array(limbs::all().iter().map(|limb| limb_json(limb)).collect())
}

fn event_json(event: &Event) -> (&'static str, Value) {
    match event {
        Event::Text(text) => ("text", json!({ "text": text })),
        Event::Thinking(text) => ("thinking", json!({ "text": text })),
        Event::Tool { id, name, arguments } => ("tool", json!({ "id": id, "name": name, "arguments": arguments })),
        Event::Result { id, name, ok, text } => ("result", json!({ "id": id, "name": name, "ok": ok, "text": text })),
        Event::Stats(stats) => ("stats", stats.clone()),
        Event::Notice(text) => ("notice", json!({ "text": text })),
    }
}

/// SSE 按 HTTP/1.1 分块写：每个事件一块、写完就推出去；客户端断开了照样干完、存好会话
struct Stream {
    writer: Option<Box<dyn Write + Send>>,
}

impl Stream {
    fn send(&mut self, event: &str, data: &Value) {
        let Some(writer) = self.writer.as_mut() else { return };
        let payload = format!("event: {event}\ndata: {data}\n\n");
        let written = write!(writer, "{:x}\r\n{payload}\r\n", payload.len()).and_then(|_| writer.flush());
        if written.is_err() {
            self.writer = None;
        }
    }

    fn end(&mut self) {
        if let Some(mut writer) = self.writer.take() {
            let _ = writer.write_all(b"0\r\n\r\n").and_then(|_| writer.flush());
        }
    }
}

fn run_turn(request: Request, id: &str, body: &Value) {
    let text = body.get("text").and_then(Value::as_str).unwrap_or("").trim().to_string();
    let Some(mut session) = Session::load(id) else {
        let _ = request.respond(error(404, "没有这个会话"));
        return;
    };
    if text.is_empty() {
        let _ = request.respond(error(400, "text 是空的"));
        return;
    }
    let model = match Model::load() {
        Ok(model) => model,
        Err(message) => {
            let _ = request.respond(error(503, &message));
            return;
        }
    };
    let cancel = Arc::new(AtomicBool::new(false));
    {
        let mut running = RUNNING.lock().unwrap();
        if running.contains_key(id) {
            drop(running);
            let _ = request.respond(error(409, "这个会话正在干活：等它做完，或先 stop"));
            return;
        }
        running.insert(id.to_string(), cancel.clone());
    }
    let streaming = body.get("stream").and_then(Value::as_bool).unwrap_or(true);
    let mut pending = Some(request);
    let mut stream = Stream { writer: None };
    if streaming {
        let mut writer = pending.take().unwrap().into_writer();
        let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream; charset=utf-8\r\nCache-Control: no-cache\r\nTransfer-Encoding: chunked\r\n\r\n";
        if writer.write_all(head.as_bytes()).and_then(|_| writer.flush()).is_ok() {
            stream.writer = Some(writer);
        }
    }
    let cwd = PathBuf::from(&session.cwd);
    let turn = brain::run(&model, std::mem::take(&mut session.history), &text, &cwd, &cancel, &mut |event| {
        let (name, data) = event_json(&event);
        stream.send(name, &data);
    });
    session.history = turn.history;
    let saved = session.save().err();
    RUNNING.lock().unwrap().remove(id);
    let error_text = turn.error.or(saved);
    let result = json!({ "answer": turn.answer, "stopped": turn.stopped, "error": error_text });
    if let Some(request) = pending {
        let _ = request.respond(json_response(if error_text.is_some() { 502 } else { 200 }, &result));
        return;
    }
    if let Some(message) = &error_text {
        stream.send("error", &json!({ "message": message }));
    }
    stream.send("done", &result);
    stream.end();
}

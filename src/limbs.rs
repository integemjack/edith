//! 能力四肢：MCP 服务（stdio 或 Streamable HTTP）。挂上后它的工具和自带工具一起交给模型，名字前加「<四肢名>__」。
//! `~/.edith/limbs.json`：`{"limbs": [{name, command, args?, env?, cwd?} | {name, url, headers?}, …]}`；运行中也能经对外接口挂上 / 摘下。
//! MCP 客户端是 friday-core 的 mcp_client；它的 HTTP 交给宿主发（mcpClient.httpPost 事件），EDITH 就是这个宿主。

use std::collections::{BTreeMap, HashMap};
use std::io::Read;
use std::sync::{Arc, LazyLock, Mutex, RwLock};

use friday_core::{events, mcp_client};
use serde_json::{Map, Value, json};

use crate::{home, tools::Outcome};

const CONNECT_TIMEOUT_MS: u64 = 60_000;
const CALL_TIMEOUT_MS: u64 = 10 * 60_000;
pub const SEPARATOR: &str = "__";

pub struct Limb {
    pub name: String,
    /// 挂它用的设置（limbs.json 里那一项）
    pub config: Value,
    client: Arc<mcp_client::Client>,
    client_id: u64,
    /// 规整过的 MCP 工具（{name, description?, inputSchema, …}）
    pub tools: Vec<Value>,
}

static LIMBS: LazyLock<RwLock<BTreeMap<String, Arc<Limb>>>> = LazyLock::new(|| RwLock::new(BTreeMap::new()));

fn path() -> std::path::PathBuf {
    home::dir().join("limbs.json")
}

/// 四肢名只留字母、数字、-、_（工具名要合 OpenAI 的规矩）
pub fn clean_name(name: &str) -> String {
    let cleaned: String = name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
    cleaned.trim_matches('_').chars().take(24).collect()
}

/// 启动时把 limbs.json 里的都挂上（挂不上的打出原因、跳过）
pub fn attach_saved(report: &mut dyn FnMut(String)) {
    install_http_host();
    let configs = home::read_json(&path()).and_then(|value| value.get("limbs").cloned()).and_then(|limbs| limbs.as_array().cloned());
    for config in configs.unwrap_or_default() {
        let name = config.get("name").and_then(Value::as_str).unwrap_or("").to_string();
        if let Err(error) = attach(config, false) {
            report(format!("四肢「{name}」挂不上：{error}"));
        }
    }
}

fn save() -> Result<(), String> {
    let limbs: Vec<Value> = LIMBS.read().unwrap().values().map(|limb| limb.config.clone()).collect();
    home::write_json(&path(), &json!({ "limbs": limbs }))
}

/// 挂上一个四肢（同名的先摘下）。remember：写进 limbs.json，下次启动自动挂
pub fn attach(config: Value, remember: bool) -> Result<Arc<Limb>, String> {
    install_http_host();
    let object = config.as_object().ok_or("四肢的设置要是 JSON 对象")?;
    let name = clean_name(object.get("name").and_then(Value::as_str).unwrap_or(""));
    if name.is_empty() {
        return Err("四肢要有名字（name：字母、数字、-、_）".into());
    }
    detach(&name, false);
    let client_id = mcp_client::open("EDITH", env!("CARGO_PKG_VERSION"));
    let client = mcp_client::get(client_id).ok_or("开不了 MCP 客户端")?;
    let connected = if let Some(command) = object.get("command").and_then(Value::as_str) {
        let arguments: Vec<String> = object
            .get("args")
            .and_then(Value::as_array)
            .map(|args| args.iter().filter_map(|arg| arg.as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        let mut environment: HashMap<String, String> = std::env::vars().collect();
        if let Some(extra) = object.get("env").and_then(Value::as_object) {
            for (key, value) in extra {
                environment.insert(key.clone(), value.as_str().map(str::to_string).unwrap_or_else(|| value.to_string()));
            }
        }
        let cwd = object.get("cwd").and_then(Value::as_str).unwrap_or("");
        client.connect_stdio(command, &arguments, &environment, cwd, CONNECT_TIMEOUT_MS, None)
    } else if let Some(url) = object.get("url").and_then(Value::as_str) {
        let headers = object
            .get("headers")
            .and_then(Value::as_object)
            .map(|headers| headers.iter().map(|(key, value)| (key.clone(), value.as_str().unwrap_or("").to_string())).collect())
            .unwrap_or_default();
        client.connect_http(url, headers, CONNECT_TIMEOUT_MS, None)
    } else {
        mcp_client::close(client_id);
        return Err("四肢要给 command（本机起的 MCP 服务）或 url（HTTP 的 MCP 服务）".into());
    };
    if let Err(failure) = connected {
        mcp_client::close(client_id);
        return Err(failure.message);
    }
    let mut tools = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let page = match client.list_tools(cursor.as_deref(), CONNECT_TIMEOUT_MS, None) {
            Ok(page) => page,
            Err(failure) => {
                mcp_client::close(client_id);
                return Err(failure.message);
            }
        };
        tools.extend(page.get("tools").and_then(Value::as_array).into_iter().flatten().map(mcp_client::normalize_tool));
        cursor = page.get("nextCursor").and_then(Value::as_str).filter(|next| !next.is_empty()).map(str::to_string);
        if cursor.is_none() {
            break;
        }
    }
    let mut stored = config.clone();
    stored["name"] = json!(name);
    let limb = Arc::new(Limb { name: name.clone(), config: stored, client, client_id, tools });
    LIMBS.write().unwrap().insert(name, limb.clone());
    if remember {
        save()?;
    }
    Ok(limb)
}

/// 摘下（forget：也从 limbs.json 里去掉）；没有这个四肢返回 false
pub fn detach(name: &str, forget: bool) -> bool {
    let removed = LIMBS.write().unwrap().remove(name);
    if let Some(limb) = &removed {
        mcp_client::close(limb.client_id);
    }
    if forget && removed.is_some() {
        let _ = save();
    }
    removed.is_some()
}

pub fn all() -> Vec<Arc<Limb>> {
    LIMBS.read().unwrap().values().cloned().collect()
}

/// 四肢带来的工具定义（OpenAI 格式；名字「<四肢名>__<工具名>」，太长的截到 64 个字符）
pub fn specs() -> Vec<Value> {
    let mut specs = Vec::new();
    for limb in all() {
        for tool in &limb.tools {
            let name = tool.get("name").and_then(Value::as_str).unwrap_or("");
            let full: String = format!("{}{SEPARATOR}{name}", limb.name).chars().take(64).collect();
            let description = tool.get("description").and_then(Value::as_str).unwrap_or("");
            specs.push(json!({
                "type": "function",
                "function": {
                    "name": full,
                    "description": format!("（四肢「{}」）{description}", limb.name),
                    "parameters": tool.get("inputSchema").cloned().unwrap_or_else(|| json!({ "type": "object", "properties": {} })),
                },
            }));
        }
    }
    specs
}

/// 给提示词看的：挂着哪些四肢
pub fn summary() -> String {
    all()
        .iter()
        .map(|limb| {
            let names: Vec<&str> = limb.tools.iter().filter_map(|tool| tool.get("name").and_then(Value::as_str)).collect();
            format!("- {}：{}", limb.name, names.join("、"))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// 调四肢的工具（名字不是四肢的返回 None）
pub fn call(full_name: &str, arguments: &Map<String, Value>) -> Option<Outcome> {
    let (limb_name, _) = full_name.split_once(SEPARATOR)?;
    let limb = LIMBS.read().unwrap().get(limb_name).cloned()?;
    // 名字截过的按前缀找回原名
    let rest = &full_name[limb_name.len() + SEPARATOR.len()..];
    let tool = limb
        .tools
        .iter()
        .filter_map(|tool| tool.get("name").and_then(Value::as_str))
        .find(|name| *name == rest || (full_name.chars().count() == 64 && name.starts_with(rest)))?
        .to_string();
    Some(match limb.client.call_tool(&tool, Value::Object(arguments.clone()), CALL_TIMEOUT_MS, None) {
        Ok(result) => {
            let failed = result.get("isError").and_then(Value::as_bool).unwrap_or(false);
            let text = content_text(&result);
            Outcome { ok: !failed, text: friday_core::tool_files::truncate(if text.is_empty() { "（没有内容）" } else { &text }) }
        }
        Err(failure) => Outcome { ok: false, text: format!("四肢「{}」出错：{}", limb.name, failure.message) },
    })
}

/// 结果里的内容拼成文字（friday-core 解码过的：{kind, text?, uri?, resourceText?, …}；图片、音频只说有一个）
fn content_text(result: &Value) -> String {
    let mut parts = Vec::new();
    for item in result.get("content").and_then(Value::as_array).into_iter().flatten() {
        let field = |key: &str| item.get(key).and_then(Value::as_str).unwrap_or("").to_string();
        match item.get("kind").and_then(Value::as_str) {
            Some("text") => parts.push(field("text")),
            Some("image") => parts.push(format!("［图片 {}］", field("mimeType"))),
            Some("audio") => parts.push(format!("［音频 {}］", field("mimeType"))),
            Some("resource") => parts.push(item.get("resourceText").and_then(Value::as_str).map_or_else(|| field("uri"), str::to_string)),
            Some("resource_link") => parts.push(format!("{}（{}）", field("name"), field("uri"))),
            _ => {}
        }
    }
    parts.join("\n")
}

// MARK: - MCP 的 HTTP（friday-core 推 mcpClient.httpPost，这里发出去、把响应一块块交回）

static ABORTED: LazyLock<Mutex<HashMap<(u64, u64), bool>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

fn install_http_host() {
    if events::has_handler() {
        return;
    }
    events::set_handler(Some(Box::new(|message: &str| {
        let Ok(message) = serde_json::from_str::<Value>(message) else { return };
        let data = message.get("data").cloned().unwrap_or(Value::Null);
        let number = |key: &str| data.get(key).and_then(Value::as_u64).unwrap_or(0);
        match message.get("event").and_then(Value::as_str) {
            Some("mcpClient.httpPost") => {
                let (client, post) = (number("client"), number("post"));
                ABORTED.lock().unwrap().insert((client, post), false);
                std::thread::spawn(move || http_post(client, post, &data));
            }
            Some("mcpClient.httpAbort") => {
                if let Some(flag) = ABORTED.lock().unwrap().get_mut(&(number("client"), number("post"))) {
                    *flag = true;
                }
            }
            _ => {}
        }
    })));
}

fn http_post(client_id: u64, post: u64, data: &Value) {
    let aborted = || ABORTED.lock().unwrap().get(&(client_id, post)).copied().unwrap_or(true);
    let finish = |error: Option<&str>, was_aborted: bool| {
        ABORTED.lock().unwrap().remove(&(client_id, post));
        if let Some(client) = mcp_client::get(client_id) {
            client.http_end(post, error, was_aborted);
        }
    };
    let url = data.get("url").and_then(Value::as_str).unwrap_or("");
    let mut request = crate::http::agent().post(url);
    for pair in data.get("headers").and_then(Value::as_array).into_iter().flatten() {
        if let (Some(key), Some(value)) = (pair.get(0).and_then(Value::as_str), pair.get(1).and_then(Value::as_str)) {
            request = request.header(key, value);
        }
    }
    let body = data.get("body").and_then(Value::as_str).unwrap_or("").to_string();
    let mut response = match request.send(body) {
        Ok(response) => response,
        Err(error) => return finish(Some(&error.to_string()), false),
    };
    let header = |name: &str| response.headers().get(name).and_then(|value| value.to_str().ok()).unwrap_or("").to_string();
    let (content_type, session) = (header("content-type"), header("mcp-session-id"));
    let Some(client) = mcp_client::get(client_id) else { return };
    client.http_head(post, response.status().as_u16() as i64, &content_type, &session);
    let mut reader = response.body_mut().as_reader();
    let mut buffer = [0u8; 8192];
    loop {
        if aborted() {
            return finish(None, true);
        }
        match reader.read(&mut buffer) {
            Ok(0) => return finish(None, false),
            Ok(count) => client.http_data(post, &buffer[..count]),
            Err(error) => return finish(Some(&error.to_string()), false),
        }
    }
}

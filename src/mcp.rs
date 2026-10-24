//! MCP 服务：大脑直接读配置连（不是能力：MCP 只会被调用、不会主动说话，不用走套接字）。
//! 配置三层，和 Claude Code 一样的写法 `{"mcpServers": {"<名字>": {command, args?, env?, cwd?} | {type: "http", url, headers?}}}`：
//! 全局 `~/.edith/mcp.json` → 项目 `<工作目录>/.mcp.json`（提交进仓库）→ 本地 `<工作目录>/.edith/mcp.local.json`；
//! 同名的后面盖前面，`{"disabled": true}` 关掉前面那层的。字符串里的 `${VAR}`、`${VAR:-默认}` 换成环境变量。
//! 工具名「<服务名>__<工具名>」；写了 readOnlyHint 的算只读（权限 tools: "read" 时能用）。
//! **用到时再起**（同 Claude Code 的按需加载）：起来时不连；工具清单按配置的哈希记在 `~/.edith/mcp-cache.json`，系统提示里只列名字
//! （deferred.rs），模型用 load_tools 取回定义、或者调用它的工具时才连上；连上后刷新缓存。没连过的服务清单里写「还不知道有哪些工具」。
//! MCP 客户端是 friday-core 的 mcp_client；它的 HTTP 交给宿主发（mcpClient.httpPost 事件），EDITH 就是这个宿主。

use std::collections::{BTreeMap, HashMap};
use std::io::Read;
use std::sync::{Arc, LazyLock, Mutex, RwLock};

use friday_core::{events, mcp_client};
use serde_json::{Map, Value, json};

use crate::capabilities::Layer;
use crate::home;
use crate::permissions::Tools;
use crate::tools::Outcome;

const CONNECT_TIMEOUT_MS: u64 = 60_000;
const CALL_TIMEOUT_MS: u64 = 10 * 60_000;
pub const SEPARATOR: &str = "__";

/// 一个连着的 MCP 服务
pub struct Limb {
    pub name: String,
    client: Arc<mcp_client::Client>,
    client_id: u64,
    /// 规整过的 MCP 工具（{name, description?, inputSchema, …}）
    pub tools: Vec<Value>,
}

static LIMBS: LazyLock<RwLock<BTreeMap<String, Arc<Limb>>>> = LazyLock::new(|| RwLock::new(BTreeMap::new()));

fn layer_path(layer: Layer) -> std::path::PathBuf {
    match layer {
        Layer::Global => home::dir().join("mcp.json"),
        Layer::Project => home::workspace().join(".mcp.json"),
        Layer::Local => home::workspace_data().join("mcp.local.json"),
    }
}

fn read_layer(layer: Layer) -> Map<String, Value> {
    home::read_json(&layer_path(layer)).and_then(|value| value.get("mcpServers").and_then(Value::as_object).cloned()).unwrap_or_default()
}

/// 合并后的配置（名字 → 设置，多一个 scope）
pub fn configs() -> BTreeMap<String, Value> {
    let mut merged: BTreeMap<String, Value> = BTreeMap::new();
    // 项目、本地两层要这个目录的配置被信任（trust.rs）
    let layers: &[Layer] = if crate::trust::allowed() { &[Layer::Global, Layer::Project, Layer::Local] } else { &[Layer::Global] };
    for &layer in layers {
        for (name, mut item) in read_layer(layer) {
            let name = crate::capabilities::clean_name(&name);
            if name.is_empty() || !item.is_object() {
                continue;
            }
            item["scope"] = json!(layer.title());
            match merged.get_mut(&name) {
                // 只写了 disabled 之类的：改前面那层的
                Some(existing) if item.get("command").is_none() && item.get("url").is_none() => {
                    for (key, value) in item.as_object().unwrap() {
                        existing[key] = value.clone();
                    }
                }
                _ => {
                    merged.insert(name, item);
                }
            }
        }
    }
    merged
}

fn disabled(config: &Value) -> bool {
    config.get("disabled").and_then(Value::as_bool) == Some(true)
}

/// 记进那一层（同名的换掉）
pub fn save(name: &str, config: Value, layer: Layer) -> Result<(), String> {
    let name = crate::capabilities::clean_name(name);
    if name.is_empty() {
        return Err("MCP 服务要有名字（字母、数字、-、_）".into());
    }
    let path = layer_path(layer);
    let mut file = home::read_json(&path).filter(Value::is_object).unwrap_or_else(|| json!({}));
    if !file["mcpServers"].is_object() {
        file["mcpServers"] = json!({});
    }
    file["mcpServers"][&name] = config;
    let was_trusted = crate::trust::trusted();
    home::write_json(&path, &file)?;
    crate::trust::after_own_write(was_trusted);
    Ok(())
}

/// 从那一层去掉；没有返回 false
pub fn remove(name: &str, layer: Layer) -> bool {
    let path = layer_path(layer);
    let Some(mut file) = home::read_json(&path) else { return false };
    let removed = file["mcpServers"].as_object_mut().and_then(|servers| servers.remove(name)).is_some();
    if removed {
        let was_trusted = crate::trust::trusted();
        let _ = home::write_json(&path, &file);
        crate::trust::after_own_write(was_trusted);
    }
    detach(name);
    removed
}

/// `${VAR}`、`${VAR:-默认}` 换成环境变量
fn expand(text: &str) -> String {
    static VARIABLE: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r"\$\{([A-Za-z_][A-Za-z0-9_]*)(?::-([^}]*))?\}").unwrap());
    VARIABLE
        .replace_all(text, |captures: &regex::Captures| {
            std::env::var(&captures[1])
                .ok()
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| captures.get(2).map_or("", |m| m.as_str()).to_string())
        })
        .into_owned()
}

fn expand_all(value: &Value) -> Value {
    match value {
        Value::String(text) => json!(expand(text)),
        Value::Array(items) => Value::Array(items.iter().map(expand_all).collect()),
        Value::Object(object) => Value::Object(object.iter().map(|(key, value)| (key.clone(), expand_all(value))).collect()),
        other => other.clone(),
    }
}

/// 缓存的工具清单：服务名 → {hash（配置的哈希）, tools}
fn cache_path() -> std::path::PathBuf {
    home::dir().join("mcp-cache.json")
}

/// 配置的哈希（只算起服务用的那些：configs() 加的 scope、disabled 不算）
fn config_hash(config: &Value) -> String {
    use sha2::Digest;
    let mut config = config.clone();
    if let Some(object) = config.as_object_mut() {
        object.remove("scope");
        object.remove("disabled");
    }
    sha2::Sha256::digest(config.to_string().as_bytes()).iter().take(8).map(|byte| format!("{byte:02x}")).collect()
}

fn remember_tools(name: &str, config: &Value, tools: &[Value]) {
    let path = cache_path();
    let mut cache = home::read_json(&path).filter(Value::is_object).unwrap_or_else(|| json!({}));
    cache[name] = json!({ "hash": config_hash(config), "tools": tools });
    let _ = home::write_json(&path, &cache);
}

/// 配置里开着的服务：名字 → 配置（不含 scope 等加的字段）
fn enabled() -> Vec<(String, Value)> {
    configs()
        .into_iter()
        .filter(|(_, config)| !disabled(config))
        .map(|(name, mut config)| {
            if let Some(object) = config.as_object_mut() {
                object.remove("scope");
            }
            (name, config)
        })
        .collect()
}

/// 知道工具清单的服务：连着的用实时的，没连的用缓存的（配置变了的缓存不算）
pub fn known() -> Vec<(String, Vec<Value>)> {
    let live: BTreeMap<String, Arc<Limb>> = LIMBS.read().unwrap().clone();
    let cache = home::read_json(&cache_path()).unwrap_or(Value::Null);
    enabled()
        .into_iter()
        .filter_map(|(name, config)| {
            if let Some(limb) = live.get(&name) {
                return Some((name, limb.tools.clone()));
            }
            let cached = &cache[&name];
            (cached["hash"].as_str() == Some(config_hash(&config).as_str()))
                .then(|| (name.clone(), cached["tools"].as_array().cloned().unwrap_or_default()))
        })
        .collect()
}

/// 还不知道有哪些工具的服务（没连过，或者配置改过）
pub fn unknown() -> Vec<String> {
    let known: Vec<String> = known().into_iter().map(|(name, _)| name).collect();
    enabled().into_iter().map(|(name, _)| name).filter(|name| !known.contains(name)).collect()
}

/// 用到时连上（已经连着就不动）
pub fn ensure(name: &str) -> Result<(), String> {
    if LIMBS.read().unwrap().contains_key(name) {
        return Ok(());
    }
    let config =
        enabled().into_iter().find(|(server, _)| server == name).map(|(_, config)| config).ok_or(format!("没有 MCP 服务「{name}」"))?;
    attach(name, config).map(|_| ())
}

/// 退出前：都断开
pub fn stop_all() {
    let all: Vec<Arc<Limb>> = std::mem::take(&mut *LIMBS.write().unwrap()).into_values().collect();
    for limb in all {
        mcp_client::close(limb.client_id);
    }
}

/// 连上一个 MCP 服务（同名的先断开）；name 已经规整过
pub fn attach(name: &str, config: Value) -> Result<Arc<Limb>, String> {
    install_http_host();
    let raw = config.clone();
    let config = expand_all(&config);
    let object = config.as_object().ok_or("MCP 服务的设置要是 JSON 对象")?;
    let name = name.to_string();
    detach(&name);
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
        return Err("MCP 服务要给 command（本机起的）或 url（HTTP 的）".into());
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
        // list_tools 已经规整过（再规整一遍会丢掉 readOnlyHint：它从 annotations 里取）
        tools.extend(page.get("tools").and_then(Value::as_array).into_iter().flatten().cloned());
        cursor = page.get("nextCursor").and_then(Value::as_str).filter(|next| !next.is_empty()).map(str::to_string);
        if cursor.is_none() {
            break;
        }
    }
    remember_tools(&name, &raw, &tools);
    let limb = Arc::new(Limb { name: name.clone(), client, client_id, tools });
    LIMBS.write().unwrap().insert(name, limb.clone());
    Ok(limb)
}

/// 断开；没连着返回 false
pub fn detach(name: &str) -> bool {
    let removed = LIMBS.write().unwrap().remove(name);
    if let Some(limb) = &removed {
        mcp_client::close(limb.client_id);
    }
    removed.is_some()
}

fn read_only(tool: &Value) -> bool {
    tool.get("readOnlyHint").and_then(Value::as_bool) == Some(true)
}

/// MCP 服务带来的工具定义（OpenAI 格式；名字「<服务名>__<工具名>」，太长的截到 64 个字符；只给这一轮许可的）。
/// 没连的服务用缓存的清单（不为了列定义去起服务）
pub fn specs(allowed: &Tools) -> Vec<Value> {
    let mut specs = Vec::new();
    for (server, tools) in known() {
        for tool in &tools {
            let name = tool.get("name").and_then(Value::as_str).unwrap_or("");
            let full: String = format!("{server}{SEPARATOR}{name}").chars().take(64).collect();
            if !allowed.allows(&full, read_only(tool)) {
                continue;
            }
            let description = tool.get("description").and_then(Value::as_str).unwrap_or("");
            specs.push(json!({
                "type": "function",
                "function": {
                    "name": full,
                    "description": format!("（MCP 服务「{server}」）{description}"),
                    "parameters": tool.get("inputSchema").cloned().unwrap_or_else(|| json!({ "type": "object", "properties": {} })),
                },
            }));
        }
    }
    specs
}

/// 工具名 → (服务名, 工具)：连着的、缓存的都找；名字截过的按前缀找回原名
fn find(full_name: &str) -> Option<(String, Value)> {
    let (server, rest) = full_name.split_once(SEPARATOR)?;
    let (_, tools) = known().into_iter().find(|(name, _)| name == server)?;
    let tool = tools.into_iter().find(|tool| {
        tool.get("name")
            .and_then(Value::as_str)
            .is_some_and(|name| name == rest || (full_name.chars().count() == 64 && name.starts_with(rest)))
    })?;
    Some((server.to_string(), tool))
}

/// 这个工具名是 MCP 服务的工具
pub fn is_tool(full_name: &str) -> bool {
    find(full_name).is_some()
}

/// 这个工具名是 MCP 服务的只读工具
pub fn is_read_only(full_name: &str) -> bool {
    find(full_name).is_some_and(|(_, tool)| read_only(&tool))
}

/// 调 MCP 服务的工具（名字不是它们的返回 None）：服务还没起就先起
pub fn call(full_name: &str, arguments: &Map<String, Value>) -> Option<Outcome> {
    let (server, tool) = find(full_name)?;
    if let Err(error) = ensure(&server) {
        return Some(Outcome { ok: false, text: format!("MCP 服务「{server}」起不来：{error}") });
    }
    let limb = LIMBS.read().unwrap().get(&server).cloned()?;
    let tool = tool.get("name").and_then(Value::as_str).unwrap_or("").to_string();
    Some(match limb.client.call_tool(&tool, Value::Object(arguments.clone()), CALL_TIMEOUT_MS, None) {
        Ok(result) => {
            let failed = result.get("isError").and_then(Value::as_bool).unwrap_or(false);
            let text = content_text(&result);
            Outcome { ok: !failed, text: friday_core::tool_files::truncate(if text.is_empty() { "（没有内容）" } else { &text }) }
        }
        Err(failure) => Outcome { ok: false, text: format!("MCP 服务「{}」出错：{}", limb.name, failure.message) },
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

#[cfg(test)]
mod tests {
    #[test]
    fn expands_variables() {
        unsafe { std::env::set_var("EDITH_TEST_TOKEN", "abc") };
        assert_eq!(super::expand("Bearer ${EDITH_TEST_TOKEN}"), "Bearer abc");
        assert_eq!(super::expand("${EDITH_NOPE_X:-def}/x"), "def/x");
    }
}

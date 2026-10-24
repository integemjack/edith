//! 按需加载的工具（同 Claude Code 的 ToolSearch）：工具定义很占地方（说一句「你好」4.8k token 里八成是工具定义，接一个 14 个工具的
//! MCP 服务每次请求再多 2k），而大多数用不上。所以：
//! - 一直给的：自带工具、delegate_task、load_tools（有技能时还有 use_skill）；
//! - 按需的：能力的动作、MCP 服务的工具、派子智能体的其余工具（并联、工作流、打回、看记录、追加指示、停掉）——系统提示里只列名字和一句用途，
//!   模型用 load_tools 取回定义后才给它；派过子智能体的会话自动给打回、看记录这些管理用的；直接调了没取回的也照样做，之后一直给。
//! - 取回过的从历史里算（load_tools 的参数、调用过的工具名），不另存状态：-c 接着的会话照样有；按第一次取回的先后接在后面，
//!   前面那一大段不变（服务商的提示词缓存不用重算）。

use serde_json::{Map, Value, json};

use crate::brain::Context;
use crate::tools::Outcome;

pub const LOAD: &str = "load_tools";

/// 派过子智能体之后自动给的管理工具
const MANAGE: &[&str] = &["rework_agent", "read_agent_log", "message_agent", "stop_agent"];
const DISPATCH: &[&str] = &["delegate_task", "run_parallel_tasks", "run_workflow"];

fn text(key: &str, pairs: &[(&str, String)]) -> String {
    let args: friday_core::prompts::Args = pairs.iter().map(|(name, value)| (name.to_string(), value.clone())).collect();
    friday_core::prompts::text(key, &args, None)
}

fn name_of(spec: &Value) -> &str {
    spec["function"]["name"].as_str().unwrap_or("")
}

/// 这一轮能按需取回的全部工具定义（派子智能体的、能力的、MCP 的；只含许可的）
pub fn available(context: &Context) -> Vec<Value> {
    let allowed = &context.tools;
    let mut specs: Vec<Value> = crate::team::specs(context)
        .into_iter()
        .filter(|spec| !crate::team::ALWAYS.contains(&name_of(spec)) && allowed.allows(name_of(spec), false))
        .collect();
    specs.extend(crate::capabilities::specs(allowed));
    specs.extend(crate::mcp::specs(allowed));
    specs
}

/// 一句用途：说明的第一句（去掉「（MCP 服务「x」）」这类前缀），最多 40 个字
fn summary(spec: &Value) -> String {
    let description = spec["function"]["description"].as_str().unwrap_or("");
    let description = match description.strip_prefix('（').and_then(|rest| rest.split_once('）')) {
        Some((_, rest)) if description.starts_with("（MCP 服务") || description.starts_with("（能力") => rest,
        _ => description,
    };
    let first = description.split(['。', '；', '\n']).next().unwrap_or("").trim();
    let first = first.split(". ").next().unwrap_or(first);
    if first.chars().count() > 40 { format!("{}…", first.chars().take(40).collect::<String>()) } else { first.to_string() }
}

/// 归哪一组（清单里按组列）
fn group(name: &str) -> String {
    if crate::team::TOOLS.contains(&name) {
        return "派子智能体".into();
    }
    let owner = name.split_once(crate::capabilities::SEPARATOR).map_or(name, |(owner, _)| owner);
    if crate::capabilities::is_action(name) { format!("能力「{owner}」") } else { format!("MCP 服务「{owner}」") }
}

/// 系统提示里的清单（没有按需工具时是空）
pub fn prompt(context: &Context) -> String {
    let specs = available(context);
    let unknown = crate::mcp::unknown();
    if specs.is_empty() && unknown.is_empty() {
        return String::new();
    }
    let mut groups: Vec<(String, Vec<String>)> = Vec::new();
    for spec in &specs {
        let name = name_of(spec);
        let entry = format!("{name}（{}）", summary(spec));
        let key = group(name);
        match groups.iter_mut().find(|(existing, _)| *existing == key) {
            Some((_, entries)) => entries.push(entry),
            None => groups.push((key, vec![entry])),
        }
    }
    let mut lines: Vec<String> = groups.into_iter().map(|(key, entries)| format!("- {key}：{}", entries.join("、"))).collect();
    lines.extend(unknown.into_iter().map(|server| text("edith.deferred.unknown", &[("server", server)])));
    text("edith.deferred", &[("list", lines.join("\n"))])
}

/// load_tools 的参数 → 工具名：确切的名字、能力 / MCP 服务的名字（它的全部工具）、带 * 的前缀
fn expand(wanted: &str, specs: &[Value]) -> Vec<String> {
    let wanted = wanted.trim();
    let names = specs.iter().map(name_of);
    if let Some(prefix) = wanted.strip_suffix('*') {
        return names.filter(|name| name.starts_with(prefix)).map(str::to_string).collect();
    }
    if specs.iter().any(|spec| name_of(spec) == wanted) {
        return vec![wanted.to_string()];
    }
    let prefix = format!("{wanted}{}", crate::capabilities::SEPARATOR);
    names.filter(|name| name.starts_with(&prefix)).map(str::to_string).collect()
}

fn requested(arguments: &Value) -> Vec<String> {
    match &arguments["names"] {
        Value::Array(items) => items.iter().filter_map(Value::as_str).map(str::to_string).collect(),
        Value::String(text) => text.split([',', '，', ' ']).filter(|part| !part.is_empty()).map(str::to_string).collect(),
        _ => Vec::new(),
    }
}

/// 历史里取回过、用过的按需工具（按第一次的先后）
pub fn loaded(history: &[Value], specs: &[Value]) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    let add = |name: String, names: &mut Vec<String>| {
        if !names.contains(&name) && specs.iter().any(|spec| name_of(spec) == name) {
            names.push(name);
        }
    };
    for message in history.iter().filter(|message| message["role"] == "assistant") {
        for call in message["tool_calls"].as_array().into_iter().flatten() {
            let name = call["function"]["name"].as_str().unwrap_or("");
            if name == LOAD {
                let arguments: Value = serde_json::from_str(call["function"]["arguments"].as_str().unwrap_or("{}")).unwrap_or(Value::Null);
                for wanted in requested(&arguments) {
                    for name in expand(&wanted, specs) {
                        add(name, &mut names);
                    }
                }
            } else {
                add(name.to_string(), &mut names);
                if DISPATCH.contains(&name) {
                    for manage in MANAGE {
                        add(manage.to_string(), &mut names);
                    }
                }
            }
        }
    }
    names
}

/// 这一轮给模型的按需工具定义（取回过的）
pub fn specs(context: &Context, history: &[Value]) -> Vec<Value> {
    let available = available(context);
    let names = loaded(history, &available);
    names.iter().filter_map(|name| available.iter().find(|spec| name_of(spec) == name).cloned()).collect()
}

/// load_tools 自己的定义
pub fn spec() -> Value {
    json!({ "type": "function", "function": {
        "name": LOAD,
        "description": text("edith.tool.load_tools", &[]),
        "parameters": { "type": "object", "properties": {
            "names": { "type": "array", "items": { "type": "string" }, "description": text("edith.tool.load_tools.names", &[]) },
        }, "required": ["names"] },
    }})
}

/// 跑 load_tools：没连过的 MCP 服务先连上（才知道有哪些工具）
pub fn load(arguments: &Map<String, Value>, context: &Context) -> Outcome {
    let wanted = requested(&Value::Object(arguments.clone()));
    if wanted.is_empty() {
        return Outcome { ok: false, text: "names 要写要取回的工具名（或能力、MCP 服务的名字）".into() };
    }
    let mut notes = Vec::new();
    // 要的是还不知道工具的 MCP 服务，或者它的工具：先连上
    for server in crate::mcp::unknown() {
        let prefix = format!("{server}{}", crate::capabilities::SEPARATOR);
        if wanted.iter().any(|name| name.trim_end_matches('*') == server || name.starts_with(&prefix))
            && let Err(error) = crate::mcp::ensure(&server)
        {
            notes.push(format!("MCP 服务「{server}」连不上：{error}"));
        }
    }
    let available = available(context);
    let mut found = Vec::new();
    let mut missing = Vec::new();
    for name in &wanted {
        let names = expand(name, &available);
        if names.is_empty() {
            missing.push(name.clone());
        }
        for name in names {
            if !found.contains(&name) {
                found.push(name);
            }
        }
    }
    let mut lines = Vec::new();
    if !found.is_empty() {
        lines.push(format!("已取回 {} 个工具，现在可以直接调用：{}", found.len(), found.join("、")));
    }
    if !missing.is_empty() {
        lines.push(format!("没有这些（看系统提示里按需工具的清单）：{}", missing.join("、")));
    }
    lines.extend(notes);
    Outcome { ok: !found.is_empty(), text: lines.join("\n") }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(name: &str) -> Value {
        json!({ "type": "function", "function": { "name": name, "description": "（MCP 服务「fs」）读一个文件。别的说明", "parameters": {} } })
    }

    #[test]
    fn loads_by_name_server_and_prefix() {
        let specs = vec![spec("fs__read_file"), spec("fs__write_file"), spec("run_workflow"), spec("rework_agent"), spec("read_agent_log")];
        assert_eq!(expand("fs", &specs), vec!["fs__read_file", "fs__write_file"]);
        assert_eq!(expand("fs__read*", &specs), vec!["fs__read_file"]);
        assert_eq!(expand("run_workflow", &specs), vec!["run_workflow"]);
        assert!(expand("nope", &specs).is_empty());
        assert_eq!(summary(&specs[0]), "读一个文件");
        // 历史里取回的、调用过的；派过子智能体的自动带上管理工具；按先后
        let history = vec![
            json!({ "role": "assistant", "tool_calls": [{ "function": { "name": "load_tools", "arguments": "{\"names\":[\"fs__read_file\"]}" } }] }),
            json!({ "role": "assistant", "tool_calls": [{ "function": { "name": "delegate_task", "arguments": "{}" } }] }),
        ];
        assert_eq!(loaded(&history, &specs), vec!["fs__read_file", "rework_agent", "read_agent_log"]);
    }
}

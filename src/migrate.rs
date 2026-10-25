//! 升级时把老的数据、配置换成现在的样子（每次起来看一眼，做过的不再做）：
//! - 老的会话 `<工作目录>/.edith/sessions/<id>.json`（v0.1.9 及以前放在项目里）→ `~/.edith/projects/<工作目录>/sessions/<id>.jsonl`；
//!   老的 `main-session`（主会话，已经没有了）删掉；
//! - 能力配置里经 `edith-mcp` 接的 MCP 服务（edith.tools 已经没有这个工具）、更早的 `"protocol": "mcp"` 项
//!   → 同一层的 MCP 配置（全局 `~/.edith/mcp.json`、项目 `.mcp.json`、本地 `.edith/mcp.local.json`）；
//! - 提示词 `~/.edith/Prompts/edith.md`（v0.1.31 及以前的名字）→ `prompts-edith.md`（提示词文件都叫 prompts-*.md，Friday 设置里的历史才有它），
//!   `.upstream/` 里上次自带的那份一起改名，改过的内容照样留着。

use std::path::Path;

use serde_json::{Map, Value, json};

use crate::capabilities::{self, Layer};
use crate::home;
use crate::sessions::Session;

/// 做一遍；做了什么返回给人看的几句
pub fn run() -> Vec<String> {
    let mut done = Vec::new();
    if let Some(note) = sessions() {
        done.push(note);
    }
    done.extend(mcp_capabilities());
    if let Some(note) = prompt_names() {
        done.push(note);
    }
    done
}

/// 提示词 edith.md 改名成 prompts-edith.md
fn prompt_names() -> Option<String> {
    let dir = home::dir().join("Prompts");
    let (old, new) = (dir.join("edith.md"), dir.join("prompts-edith.md"));
    if !old.is_file() || new.exists() {
        return None;
    }
    std::fs::rename(&old, &new).ok()?;
    let upstream = dir.join(".upstream");
    if upstream.join("edith.md").is_file() && !upstream.join("prompts-edith.md").exists() {
        let _ = std::fs::rename(upstream.join("edith.md"), upstream.join("prompts-edith.md"));
    }
    Some("提示词 edith.md 改名成了 prompts-edith.md".into())
}

/// 项目里的老会话挪到全局
fn sessions() -> Option<String> {
    let old = home::workspace_data().join("sessions");
    let target = home::project_dir().join("sessions");
    if !old.is_dir() || canonical(&old) == canonical(&target) {
        return None;
    }
    let mut moved = 0;
    for entry in std::fs::read_dir(&old).ok()?.flatten() {
        let path = entry.path();
        let Some(id) = path.file_name().and_then(|name| name.to_str()).and_then(|name| name.strip_suffix(".json")).map(str::to_string)
        else {
            continue;
        };
        let Some(value) = home::read_json(&path) else { continue };
        if !target.join(format!("{id}.jsonl")).exists() {
            let mut session = Session::from_legacy(&id, &value);
            if session.save().is_err() {
                continue;
            }
        }
        let _ = std::fs::remove_file(&path);
        moved += 1;
    }
    // 挪空了就删掉；主会话已经没有了
    let _ = std::fs::remove_dir(&old);
    let _ = std::fs::remove_file(home::workspace_data().join("main-session"));
    (moved > 0).then(|| format!("把 {moved} 个旧会话从 {} 挪到了 {}", old.display(), target.display()))
}

fn canonical(path: &Path) -> std::path::PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// 能力配置里的一项是不是老的 MCP 接法；是的话换成 MCP 配置的写法
fn as_mcp(item: &Value) -> Option<Value> {
    let mut config = Map::new();
    if item.get("protocol").and_then(Value::as_str) == Some("mcp") {
        for key in ["command", "args", "env", "cwd", "url", "headers"] {
            if let Some(value) = item.get(key) {
                config.insert(key.into(), value.clone());
            }
        }
        if config.contains_key("url") {
            config.insert("type".into(), json!("http"));
        }
        return Some(Value::Object(config));
    }
    let command = item.get("command").and_then(Value::as_str)?;
    let program = Path::new(command).file_stem().and_then(|stem| stem.to_str()).unwrap_or("");
    if program != "edith-mcp" {
        return None;
    }
    let args: Vec<String> =
        item.get("args").and_then(Value::as_array).into_iter().flatten().filter_map(|arg| arg.as_str().map(str::to_string)).collect();
    if args.first().map(String::as_str) == Some("--url") {
        let mut headers = Map::new();
        let mut rest = args.iter().skip(2);
        while let Some(flag) = rest.next() {
            if flag == "--header"
                && let Some((key, value)) = rest.next().and_then(|header| header.split_once(':'))
            {
                headers.insert(key.trim().to_string(), json!(value.trim()));
            }
        }
        config.insert("type".into(), json!("http"));
        config.insert("url".into(), json!(args.get(1).cloned().unwrap_or_default()));
        if !headers.is_empty() {
            config.insert("headers".into(), Value::Object(headers));
        }
    } else {
        let (program, rest) = args.split_first()?;
        config.insert("command".into(), json!(program));
        config.insert("args".into(), json!(rest));
        for key in ["env", "cwd"] {
            if let Some(value) = item.get(key) {
                config.insert(key.into(), value.clone());
            }
        }
    }
    if item.get("disabled").and_then(Value::as_bool) == Some(true) {
        config.insert("disabled".into(), json!(true));
    }
    Some(Value::Object(config))
}

/// 每一层：老的 MCP 接法挪进同一层的 MCP 配置
fn mcp_capabilities() -> Vec<String> {
    let mut done = Vec::new();
    for layer in [Layer::Global, Layer::Project, Layer::Local] {
        let items = capabilities::raw_layer(layer);
        let (old, kept): (Vec<Value>, Vec<Value>) = items.into_iter().partition(|item| as_mcp(item).is_some());
        if old.is_empty() {
            continue;
        }
        let mut names = Vec::new();
        for item in &old {
            let name = item.get("name").and_then(Value::as_str).unwrap_or("").to_string();
            if let Some(config) = as_mcp(item)
                && crate::mcp::save(&name, config, layer).is_ok()
            {
                names.push(name);
            }
        }
        let was_trusted = crate::trust::trusted();
        if capabilities::write_raw_layer(layer, &kept).is_ok() {
            crate::trust::after_own_write(was_trusted);
            done.push(format!("{}的能力配置里 {} 改成直接连的 MCP 服务（edith mcp 看）", layer.title(), names.join("、")));
        }
    }
    done
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_old_mcp_capabilities() {
        let local =
            as_mcp(&json!({ "name": "fs", "command": "edith-mcp", "args": ["npx", "-y", "server-fs", "/tmp"], "env": { "A": "1" } }))
                .unwrap();
        assert_eq!(local, json!({ "command": "npx", "args": ["-y", "server-fs", "/tmp"], "env": { "A": "1" } }));
        let http = as_mcp(&json!({ "name": "s", "command": "/x/edith-mcp.exe", "args": ["--url", "https://e/mcp", "--header", "Authorization: Bearer t"] })).unwrap();
        assert_eq!(http, json!({ "type": "http", "url": "https://e/mcp", "headers": { "Authorization": "Bearer t" } }));
        let older = as_mcp(&json!({ "name": "o", "protocol": "mcp", "url": "https://e/mcp" })).unwrap();
        assert_eq!(older["type"], "http");
        assert!(as_mcp(&json!({ "name": "ws", "command": "edith-websocket" })).is_none());
    }
}

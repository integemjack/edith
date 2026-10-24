//! 信任这个目录的配置吗（同 Claude Code 打开新目录时问的那一句）：项目、本地那两层的能力和 MCP 配置会**起程序**，
//! 别人的仓库里写什么命令，一跑 edith 就会以你的身份执行。所以：
//! - 这两层的配置文件（`.edith/capabilities.json`、`.edith/capabilities.local.json`、`.mcp.json`、`.edith/mcp.local.json`）
//!   按内容算一个哈希，记在 `~/.edith/trust.json`（目录 → 哈希）；内容变了（git pull 拉来新命令）就要再问；
//! - 没信任的目录：这两层不读，只用全局的；终端里起的会先列出要起的命令问一句，不在终端里的（edith run 当服务跑、管道）
//!   只提醒，用 `edith trust` 看过再信任；
//! - 自己用 `edith capabilities add`、`edith mcp add` 改的：改之前信任（或者还没有这些文件），改完照旧信任。

use std::io::{BufRead, IsTerminal, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, Ordering};

use serde_json::{Value, json};

use crate::home;

/// 这次运行定下的：0 还没定（按 trust.json 算）、1 信任、2 不信任
static DECISION: AtomicU8 = AtomicU8::new(0);

fn store() -> PathBuf {
    home::dir().join("trust.json")
}

fn key() -> String {
    home::workspace().to_string_lossy().into_owned()
}

/// 要信任才读的文件（存在的；在全局数据目录里的不算——工作目录就是家目录时 `.edith/capabilities.json` 就是全局那个）
pub fn files() -> Vec<PathBuf> {
    let global = std::fs::canonicalize(home::dir()).unwrap_or_else(|_| home::dir());
    let data = home::workspace_data();
    [data.join("capabilities.json"), data.join("capabilities.local.json"), home::workspace().join(".mcp.json"), data.join("mcp.local.json")]
        .into_iter()
        .filter(|path| path.is_file())
        .filter(|path| std::fs::canonicalize(path).map_or(true, |real| !real.starts_with(&global)))
        .collect()
}

/// 这些文件现在的样子
fn digest() -> String {
    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    for path in files() {
        hasher.update(path.to_string_lossy().as_bytes());
        hasher.update([0]);
        hasher.update(std::fs::read(&path).unwrap_or_default());
        hasher.update([0]);
    }
    hasher.finalize().iter().map(|byte| format!("{byte:02x}")).collect()
}

/// trust.json 里记的和现在的一样（没有这些文件也算）
pub fn trusted() -> bool {
    if files().is_empty() {
        return true;
    }
    home::read_json(&store()).is_some_and(|all| all[key()]["digest"].as_str() == Some(digest().as_str()))
}

/// 读不读项目、本地那两层
pub fn allowed() -> bool {
    match DECISION.load(Ordering::Relaxed) {
        1 => true,
        2 => false,
        _ => trusted(),
    }
}

/// 信任现在的样子
pub fn grant() -> Result<(), String> {
    let mut all = home::read_json(&store()).filter(Value::is_object).unwrap_or_else(|| json!({}));
    all[key()] = json!({ "digest": digest(), "at": chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, false) });
    home::write_json(&store(), &all)?;
    DECISION.store(1, Ordering::Relaxed);
    Ok(())
}

/// 不再信任
pub fn revoke() -> Result<bool, String> {
    let Some(mut all) = home::read_json(&store()) else { return Ok(false) };
    let removed = all.as_object_mut().and_then(|all| all.remove(&key())).is_some();
    if removed {
        home::write_json(&store(), &all)?;
    }
    DECISION.store(0, Ordering::Relaxed);
    Ok(removed)
}

/// 自己改了这两层（edith capabilities / mcp 的 add、remove）：改之前信任，改完照旧信任
pub fn after_own_write(was_trusted: bool) {
    if was_trusted {
        let _ = grant();
    }
}

/// 这两层要起的程序（给人看）
pub fn commands() -> Vec<String> {
    let describe = |config: &Value| {
        let command = config["command"].as_str().unwrap_or("");
        let args: Vec<&str> = config["args"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
        if command.is_empty() {
            config["url"].as_str().unwrap_or("").to_string()
        } else {
            format!("{command} {}", args.join(" ")).trim().to_string()
        }
    };
    let mut lines = Vec::new();
    for path in files() {
        let Some(value) = home::read_json(&path) else {
            lines.push(format!("{}：不是 JSON", path.display()));
            continue;
        };
        let file = path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
        for item in value["capabilities"].as_array().into_iter().flatten() {
            if item.get("command").is_some() {
                lines.push(format!("能力 {}：{}（{file}）", item["name"].as_str().unwrap_or("?"), describe(item)));
            }
        }
        for (name, item) in value["mcpServers"].as_object().into_iter().flatten() {
            if item.get("command").is_some() || item.get("url").is_some() {
                lines.push(format!("MCP {name}：{}（{file}）", describe(item)));
            }
        }
    }
    lines
}

/// 起能力、MCP 之前：没信任的，终端里问一句；不在终端里只提醒。返回信任了没有
pub fn ensure() -> bool {
    if DECISION.load(Ordering::Relaxed) != 0 {
        return allowed();
    }
    if trusted() {
        DECISION.store(1, Ordering::Relaxed);
        return true;
    }
    let commands = commands();
    let workspace = home::workspace();
    if std::io::stdin().is_terminal() && std::io::stderr().is_terminal() {
        eprintln!("\x1b[33m这个目录（{}）的配置要起这些程序，以你的身份运行：\x1b[0m", workspace.display());
        for line in &commands {
            eprintln!("  {line}");
        }
        eprint!("信任这个目录的配置吗？只信任你自己写的或看过的。[y/N] ");
        let _ = std::io::stderr().flush();
        let mut answer = String::new();
        let _ = std::io::stdin().lock().read_line(&mut answer);
        if matches!(answer.trim().to_lowercase().as_str(), "y" | "yes" | "是") {
            let _ = grant();
            return true;
        }
    } else {
        eprintln!(
            "\x1b[33m（{} 的项目 / 本地配置还没信任（或改过了），里面的 {} 项没起：edith trust 看过再信任）\x1b[0m",
            workspace.display(),
            commands.len()
        );
    }
    DECISION.store(2, Ordering::Relaxed);
    false
}

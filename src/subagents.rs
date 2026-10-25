//! 现成的子智能体（同 Claude Code 的 subagents，Friday 的 Subagents）：一个 Markdown 文件一个，YAML 头 name（没有用文件名）、
//! description（什么时候派它）、tools（逗号分开或 YAML 列表，Claude Code 的叫法 Read、Grep、Bash… 换成 EDITH 的；没写 = 和主智能体一样）、
//! model（haiku / sonnet / opus 或 look / change / hard；EDITH 的子智能体都用这一轮的模型，只记下），正文是角色说明。
//! 派活时 subagent_type 点名：角色、能用的工具照文件来；general-purpose 这类通用的名字、一个现成的都没有时当没写。
//! 从这些地方找（同名的先找到的算）：项目的 `.edith/agents/`、`.claude/agents/`、`.friday/agents/`（要这个目录的配置被信任），
//! `~/.edith/agents/`（Friday 插件带的在它下面的 `<插件>/` 里，名字「插件:名字」）、Claude Code 的 `~/.claude/agents/`、Friday 的 Agents 目录。
//! 每次用到时重新列，放进去、改了、删了的下一次派活就生效。

use std::path::{Path, PathBuf};

use crate::home;
use crate::skills::{field, split_header};

/// 系统提示里每个的用途最多几个字
const DESCRIPTION_LIMIT: usize = 300;

#[derive(Debug, Clone)]
pub struct Agent {
    /// 派它时写的名字（插件带的是「插件:名字」）
    pub name: String,
    /// 插件带的去掉「插件:」的名字（不撞时也能这样叫）
    pub short_name: String,
    pub description: String,
    /// 头里的 tools（原样，拆开的）
    pub tools: Vec<String>,
    pub model: String,
    pub file: PathBuf,
    /// project / user / claude / friday / plugin
    pub source: &'static str,
    pub plugin: String,
    /// 插件的文件夹（正文里的 ${CLAUDE_PLUGIN_ROOT} 换成它）
    pub plugin_dir: Option<PathBuf>,
}

/// 派发时用的：角色、能用的工具（None = 和主智能体一样）
#[derive(Debug, Clone)]
pub struct Definition {
    pub agent: Agent,
    pub role: String,
    pub tools: Option<Vec<String>>,
}

/// 找的目录（先找的同名优先）和来源
pub fn roots() -> Vec<(PathBuf, &'static str)> {
    let mut roots = Vec::new();
    if crate::trust::allowed() {
        for name in [".edith", ".claude", ".friday"] {
            roots.push((home::workspace().join(name).join("agents"), "project"));
        }
    }
    roots.push((home::dir().join("agents"), "user"));
    roots.push((crate::skills::claude_dir().join("agents"), "claude"));
    if let Some(dir) = crate::skills::friday_data_dir() {
        roots.push((dir.join("Agents"), "friday"));
    }
    roots
}

/// 文件夹下的 .md（含子文件夹，跳过点开头的）：(相对路径去掉 .md、正斜杠, 完整路径)，按路径排
pub fn markdown_files(dir: &Path) -> Vec<(String, PathBuf)> {
    fn walk(dir: &Path, prefix: &str, out: &mut Vec<(String, PathBuf)>) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            let path = entry.path();
            let relative = if prefix.is_empty() { name.clone() } else { format!("{prefix}/{name}") };
            // 插件带的是链过来的文件夹：跟着链接走
            if path.is_dir() {
                walk(&path, &relative, out);
            } else if let Some(stem) = relative.strip_suffix(".md").or_else(|| relative.strip_suffix(".MD")) {
                out.push((stem.to_string(), path));
            }
        }
    }
    let mut files = Vec::new();
    walk(dir, "", &mut files);
    files.sort();
    files
}

/// tools 头的值拆开：逗号、空白、YAML 列表的「- 」、方括号、引号都认
pub fn split_tools(value: &str) -> Vec<String> {
    let mut tools: Vec<String> = Vec::new();
    for part in value.split(|c: char| c == ',' || c == '，' || c.is_whitespace() || c == '[' || c == ']') {
        let part = part.trim_matches(['"', '\'']);
        if !part.is_empty() && part != "-" && !tools.iter().any(|tool| tool == part) {
            tools.push(part.to_string());
        }
    }
    tools
}

fn clipped(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit { text.to_string() } else { format!("{}…", text.chars().take(limit).collect::<String>()) }
}

fn make(relative: &str, file: &Path, source: &'static str, plugin: &str, plugin_dir: Option<PathBuf>) -> Option<Agent> {
    let text = std::fs::read_to_string(file).ok()?;
    let (header, _) = split_header(&text);
    let name = field(&header, "name").map(|name| name.split_whitespace().collect::<Vec<_>>().join(" ")).unwrap_or_else(|| {
        // 插件文件夹下的子文件夹也是命名空间
        relative.replace('/', ":")
    });
    let description = field(&header, "description").unwrap_or_default().split_whitespace().collect::<Vec<_>>().join(" ");
    let model = field(&header, "model").unwrap_or_default().trim().to_string();
    let tools = split_tools(&field(&header, "tools").unwrap_or_default());
    let full = if plugin.is_empty() { name.clone() } else { format!("{plugin}:{name}") };
    Some(Agent {
        name: full,
        short_name: name,
        description,
        tools,
        model,
        file: file.to_path_buf(),
        source,
        plugin: plugin.to_string(),
        plugin_dir,
    })
}

/// 全部（同名的只留先找到的，按名字排）
pub fn all() -> Vec<Agent> {
    let mut agents: Vec<Agent> = Vec::new();
    for (root, source) in roots() {
        for (relative, file) in markdown_files(&root) {
            // ~/.edith/agents/<插件>/… 是 Friday 插件带的（edith-friday 链过来的文件夹）
            let (plugin, plugin_dir, relative) = match (source, relative.split_once('/')) {
                ("user", Some((folder, rest))) if root.join(folder).join(".friday-plugin").is_file() => {
                    let dir = std::fs::read_to_string(root.join(folder).join(".friday-plugin")).ok().map(|text| PathBuf::from(text.trim()));
                    (folder.to_string(), dir, rest.to_string())
                }
                _ => (String::new(), None, relative.clone()),
            };
            let source = if plugin.is_empty() { source } else { "plugin" };
            let Some(agent) = make(&relative, &file, source, &plugin, plugin_dir) else { continue };
            if agent.name.is_empty() || agents.iter().any(|existing| existing.name.eq_ignore_ascii_case(&agent.name)) {
                continue;
            }
            agents.push(agent);
        }
    }
    agents.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()).then(a.name.cmp(&b.name)));
    agents
}

/// 按名字找（不分大小写；插件带的短名字不撞时也认）
pub fn find(name: &str) -> Option<Agent> {
    let wanted = name.trim();
    if wanted.is_empty() {
        return None;
    }
    let agents = all();
    if let Some(agent) = agents.iter().find(|agent| agent.name.eq_ignore_ascii_case(wanted)) {
        return Some(agent.clone());
    }
    let mut found = agents.iter().filter(|agent| !agent.plugin.is_empty() && agent.short_name.eq_ignore_ascii_case(wanted));
    let first = found.next()?;
    if found.next().is_some() { None } else { Some(first.clone()) }
}

/// 头里的工具名换成 EDITH 的（Read → read_file、Bash → run_shell…），没有的丢掉；一个都对不上时 None（和主智能体一样）
pub fn map_tools(tools: &[String], exists: &dyn Fn(&str) -> bool) -> Option<Vec<String>> {
    let mut mapped: Vec<String> = Vec::new();
    for tool in tools {
        let mut name = tool.clone();
        if !exists(&name) {
            name = friday_core::tool_aliases::canonical_name(tool);
            if !exists(&name) {
                continue;
            }
        }
        if !mapped.contains(&name) {
            mapped.push(name);
        }
    }
    (!mapped.is_empty()).then_some(mapped)
}

/// 派活点名的：找到就照它的；通用的名字、一个都没有当没写；其余回给模型的说明
pub fn resolve(agent_type: Option<&str>, exists: &dyn Fn(&str) -> bool) -> Result<Option<Definition>, String> {
    let Some(wanted) = agent_type.map(str::trim).filter(|name| !name.is_empty()) else { return Ok(None) };
    if let Some(agent) = find(wanted) {
        let text = std::fs::read_to_string(&agent.file).unwrap_or_default();
        let (_, body) = split_header(&text);
        let role = match &agent.plugin_dir {
            Some(dir) => body.replace("${CLAUDE_PLUGIN_ROOT}", &crate::tools::slash_path(dir)),
            None => body,
        };
        let tools = map_tools(&agent.tools, exists);
        return Ok(Some(Definition { agent, role: role.trim().to_string(), tools }));
    }
    let generic = ["general-purpose", "general", "default", "通才", "通用"];
    let names: Vec<String> = all().into_iter().map(|agent| agent.name).collect();
    if generic.contains(&wanted.to_lowercase().as_str()) || names.is_empty() {
        return Ok(None);
    }
    Err(format!("没有叫「{wanted}」的现成子智能体。有的：{}；都不合适就去掉 subagent_type，用 role 写清角色", names.join("、")))
}

/// 角色：现成的正文（空的用默认的），派发时又写了 role 的接在后面
pub fn composed_role(definition: Option<&Definition>, role: &str) -> String {
    match definition {
        None => role.to_string(),
        Some(definition) if role.trim().is_empty() => definition.role.clone(),
        Some(definition) if definition.role.is_empty() => role.to_string(),
        Some(definition) => format!("{}\n\n{}", definition.role, role.trim()),
    }
}

/// 系统提示里「现成的子智能体」一段（能派子智能体的才给；一个都没有时为空）
pub fn prompt() -> String {
    let agents = all();
    if agents.is_empty() {
        return String::new();
    }
    let lines: Vec<String> = agents
        .iter()
        .map(|agent| {
            let description = clipped(&agent.description, DESCRIPTION_LIMIT);
            if description.is_empty() { format!("- {}", agent.name) } else { format!("- {}：{description}", agent.name) }
        })
        .collect();
    friday_core::prompts::text("edith.agents", &[("agents".to_string(), lines.join("\n"))].into_iter().collect(), None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_and_maps_tools() {
        assert_eq!(split_tools("Read, Grep  Bash"), vec!["Read", "Grep", "Bash"]);
        assert_eq!(split_tools("- Read - \"Glob\" [Edit]"), vec!["Read", "Glob", "Edit"]);
        let exists = |name: &str| ["read_file", "run_shell", "find_files"].contains(&name);
        assert_eq!(map_tools(&split_tools("Read, Bash, Nope"), &exists), Some(vec!["read_file".to_string(), "run_shell".to_string()]));
        assert_eq!(map_tools(&split_tools("Nope"), &exists), None);
    }
}

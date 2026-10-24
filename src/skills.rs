//! 技能（同 Claude Code 的 Agent Skills，文件夹通用）：`<目录>/<名字>/SKILL.md`，开头是 YAML 头（`---` name / description `---`），
//! 同一个文件夹里可以带脚本、模板、参考资料。系统提示里只列名字和用途，用到时 use_skill 读全文（按需加载，不占上下文）。
//! 从这些地方找（同名的前面的算）：项目的 `.edith/skills/`、`.claude/skills/`（要这个目录的配置被信任：别人仓库里的技能也是让模型照做的话），
//! `~/.edith/skills/`、Claude Code 的 `~/.claude/skills/`、Friday 的 Skills 目录。每次用到时重新列，放进去、改了、删了的下一轮就生效。

use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use crate::home;
use crate::tools::Outcome;

pub const USE: &str = "use_skill";
/// 清单里每个用途最多几个字
const DESCRIPTION_LIMIT: usize = 250;
/// use_skill 一次最多交回几个字（多的用 read_file 接着读）
const CONTENT_LIMIT: usize = 30000;

#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub file: PathBuf,
    /// 从哪来（项目 / EDITH / Claude Code / Friday）
    pub source: &'static str,
}

impl Skill {
    pub fn dir(&self) -> &Path {
        self.file.parent().unwrap_or(&self.file)
    }
}

/// Friday 的技能目录（各平台的数据目录）
fn friday_dir() -> Option<PathBuf> {
    let home = home::user_home();
    let dir = if cfg!(target_os = "macos") {
        home.join("Library/Application Support/Friday/Skills")
    } else if cfg!(windows) {
        PathBuf::from(std::env::var_os("APPDATA")?).join("Friday").join("Skills")
    } else {
        std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".local/share")).join("Friday").join("Skills")
    };
    Some(dir)
}

/// 找技能的目录（先找的同名优先）
pub fn roots() -> Vec<(PathBuf, &'static str)> {
    let mut roots = Vec::new();
    if crate::trust::allowed() {
        roots.push((home::workspace().join(".edith").join("skills"), "项目"));
        roots.push((home::workspace().join(".claude").join("skills"), "项目"));
    }
    roots.push((home::dir().join("skills"), "EDITH"));
    roots.push((home::user_home().join(".claude").join("skills"), "Claude Code"));
    if let Some(dir) = friday_dir() {
        roots.push((dir, "Friday"));
    }
    roots
}

/// YAML 头里的一项（只认简单的 `key: value`，值可以带引号）
fn field(header: &str, key: &str) -> Option<String> {
    header.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        (name.trim() == key).then(|| value.trim().trim_matches(['"', '\'']).to_string()).filter(|value| !value.is_empty())
    })
}

/// 读一个 SKILL.md：头里的 name / description；没有头时用文件夹名和正文第一句
fn parse(file: &Path, source: &'static str) -> Option<Skill> {
    let text = std::fs::read_to_string(file).ok()?;
    let folder = file.parent()?.file_name()?.to_string_lossy().into_owned();
    let (header, body) = match text.strip_prefix("---") {
        Some(rest) => match rest.split_once("\n---") {
            Some((header, body)) => (header.to_string(), body.to_string()),
            None => (String::new(), text.clone()),
        },
        None => (String::new(), text.clone()),
    };
    let name = field(&header, "name").unwrap_or(folder);
    let description = field(&header, "description").unwrap_or_else(|| {
        body.lines()
            .map(|line| line.trim().trim_start_matches('#').trim())
            .find(|line| !line.is_empty())
            .unwrap_or("")
            .split(['。', '.'])
            .next()
            .unwrap_or("")
            .to_string()
    });
    let description = if description.chars().count() > DESCRIPTION_LIMIT {
        format!("{}…", description.chars().take(DESCRIPTION_LIMIT).collect::<String>())
    } else {
        description
    };
    Some(Skill { name, description, file: file.to_path_buf(), source })
}

/// 现在有的技能（同名的只留先找到的）
pub fn list() -> Vec<Skill> {
    let mut skills: Vec<Skill> = Vec::new();
    for (root, source) in roots() {
        let Ok(entries) = std::fs::read_dir(&root) else { continue };
        let mut folders: Vec<PathBuf> =
            entries.flatten().map(|entry| entry.path()).filter(|path| path.join("SKILL.md").is_file()).collect();
        folders.sort();
        for folder in folders {
            if let Some(skill) = parse(&folder.join("SKILL.md"), source)
                && !skills.iter().any(|existing| existing.name == skill.name)
            {
                skills.push(skill);
            }
        }
    }
    skills
}

/// 技能所在的目录（读它们里面的脚本、参考资料不算「工作目录外」）
pub fn dirs() -> Vec<PathBuf> {
    roots().into_iter().map(|(root, _)| root).filter(|root| root.is_dir()).collect()
}

/// 系统提示里的清单（没有技能时是空）
pub fn prompt() -> String {
    let skills = list();
    if skills.is_empty() {
        return String::new();
    }
    let lines: Vec<String> = skills.iter().map(|skill| format!("- {}：{}", skill.name, skill.description)).collect();
    friday_core::prompts::text("edith.skills", &[("list".to_string(), lines.join("\n"))].into_iter().collect(), None)
}

/// use_skill 的定义（有技能时才给）
pub fn spec() -> Value {
    let text = |key: &str| friday_core::prompts::text(key, &Default::default(), None);
    json!({ "type": "function", "function": {
        "name": USE,
        "description": text("edith.tool.use_skill"),
        "parameters": { "type": "object", "properties": {
            "name": { "type": "string", "description": text("edith.tool.use_skill.name") },
        }, "required": ["name"] },
    }})
}

/// use_skill：读全文（太长的只给开头，写明怎么接着读），附上文件夹路径
pub fn use_skill(arguments: &Map<String, Value>) -> Outcome {
    let wanted = arguments.get("name").and_then(Value::as_str).unwrap_or("").trim();
    let skills = list();
    let Some(skill) =
        skills.iter().find(|skill| skill.name == wanted).or_else(|| skills.iter().find(|skill| skill.name.eq_ignore_ascii_case(wanted)))
    else {
        let names: Vec<&str> = skills.iter().map(|skill| skill.name.as_str()).collect();
        return Outcome {
            ok: false,
            text: format!("没有技能「{wanted}」（有的：{}）", if names.is_empty() { "无".into() } else { names.join("、") }),
        };
    };
    let content = std::fs::read_to_string(&skill.file).unwrap_or_default();
    let total = content.chars().count();
    let shown: String = content.chars().take(CONTENT_LIMIT).collect();
    let more = if total > CONTENT_LIMIT {
        format!("\n…（共 {total} 字，后面的用 read_file 读 {} 从第 {CONTENT_LIMIT} 字接着看）", crate::tools::slash_path(&skill.file))
    } else {
        String::new()
    };
    Outcome {
        ok: true,
        text: format!(
            "技能「{}」（{}，文件夹 {}：里面提到的脚本、模板、参考资料按这个路径找）\n\n{shown}{more}",
            skill.name,
            skill.source,
            crate::tools::slash_path(skill.dir())
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_headers_and_falls_back() {
        let root = std::env::temp_dir().join(format!("edith-skills-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("pdf")).unwrap();
        std::fs::create_dir_all(root.join("plain")).unwrap();
        std::fs::write(root.join("pdf/SKILL.md"), "---\nname: pdf-tools\ndescription: \"处理 PDF：合并、拆分\"\n---\n# 用法\n…").unwrap();
        std::fs::write(root.join("plain/SKILL.md"), "# 发版\n先跑测试。再打标签。").unwrap();
        let pdf = parse(&root.join("pdf/SKILL.md"), "EDITH").unwrap();
        assert_eq!((pdf.name.as_str(), pdf.description.as_str()), ("pdf-tools", "处理 PDF：合并、拆分"));
        let plain = parse(&root.join("plain/SKILL.md"), "EDITH").unwrap();
        assert_eq!((plain.name.as_str(), plain.description.as_str()), ("plain", "发版"));
        let _ = std::fs::remove_dir_all(&root);
    }
}

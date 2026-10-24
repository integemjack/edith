//! 记忆库（同 Friday 的 MEMORY.md，精简）：`~/.edith/memory.json`，用户说过要记住的偏好、事实、决定、纠正。
//! 模型用 save_memory 当场记、search_memory 按关键词查；常驻的每轮放进系统提示。Friday 客户端的 设置 › 记忆 经网关直接改这份文件。
//! 一条：`{id（m + 4 位十六进制）, kind, text, project（空 = 到哪都适用，不然是工作目录）, pinned, source（said = 模型照用户的话记的；
//! user = 用户在设置里写的）, state（active / archived）, uses, created_at, updated_at, last_used_at?, archived_at?, archive_reason?}`。
//! 几个大脑、网关都写它：每次改先读回最新的再改，写临时文件再换名。

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde_json::{Map, Value, json};

use crate::home;
use crate::tools::Outcome;

pub const SAVE: &str = "save_memory";
pub const SEARCH: &str = "search_memory";
/// 这个进程里的读改写一个个来（别的进程写的靠换名不读到半个）
static LOCK: Mutex<()> = Mutex::new(());

/// 记忆库文件（测试用别的）
pub struct Book(pub PathBuf);

pub fn book() -> Book {
    Book(home::dir().join("memory.json"))
}

fn load_from(path: &Path) -> Vec<Value> {
    home::read_json(path).and_then(|value| value.get("items").and_then(Value::as_array).cloned()).unwrap_or_default()
}

fn now() -> String {
    chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, false)
}

pub fn kind_label(kind: &str) -> &'static str {
    match kind {
        "fact" => "事实",
        "decision" => "决定",
        "correction" => "纠正",
        "experience" => "经验",
        "habit" => "习惯",
        "routine" => "惯例",
        _ => "偏好",
    }
}

fn text<'a>(item: &'a Value, key: &str) -> &'a str {
    item.get(key).and_then(Value::as_str).unwrap_or("")
}

fn active(item: &Value) -> bool {
    text(item, "state") != "archived"
}

fn project_key() -> String {
    crate::tools::slash_path(&home::workspace())
}

/// 新编号：m + 4 位十六进制，和已有的不撞
fn new_id(items: &[Value]) -> String {
    loop {
        let id = format!("m{}", &uuid::Uuid::new_v4().simple().to_string()[..4]);
        if !items.iter().any(|item| text(item, "id") == id) {
            return id;
        }
    }
}

/// 一行给模型看的：「- [偏好] 正文 [m1a2b]（2026-10-24，常驻）」
fn line(item: &Value, here: &str) -> String {
    let mut notes = vec![text(item, "updated_at").get(..10).unwrap_or("").to_string()];
    let project = text(item, "project");
    if !project.is_empty() && project != here {
        notes.push(format!("项目 {project}"));
    }
    if item.get("pinned").and_then(Value::as_bool) == Some(true) {
        notes.push("常驻".into());
    }
    notes.retain(|note| !note.is_empty());
    format!("- [{}] {} [{}]（{}）", kind_label(text(item, "kind")), text(item, "text"), text(item, "id"), notes.join("，"))
}

impl Book {
    pub fn load(&self) -> Vec<Value> {
        load_from(&self.0)
    }

    fn store(&self, items: &[Value]) -> Result<(), String> {
        home::write_json(&self.0, &json!({ "items": items }))
    }

    /// 记一条（save_memory；replaces 改掉那条）：同一句话、同一个项目的已有那条就更新它（归档了的恢复）
    pub fn save(&self, text_value: &str, kind: &str, project: &str, replaces: Option<&str>, source: &str) -> Result<Value, String> {
        let _guard = LOCK.lock().unwrap();
        let mut items = self.load();
        let kind = if ["preference", "fact", "decision", "correction", "experience", "habit", "routine"].contains(&kind) {
            kind
        } else {
            "preference"
        };
        let stamp = now();
        let existing = items.iter().position(|item| match replaces {
            Some(id) => text(item, "id") == id,
            None => text(item, "text") == text_value && text(item, "project") == project,
        });
        if replaces.is_some() && existing.is_none() {
            return Err(format!("没有编号 {} 的记忆", replaces.unwrap_or("")));
        }
        let item = match existing {
            Some(index) => {
                let item = &mut items[index];
                item["text"] = json!(text_value);
                item["kind"] = json!(kind);
                item["project"] = json!(project);
                item["state"] = json!("active");
                item["updated_at"] = json!(stamp);
                if let Some(object) = item.as_object_mut() {
                    object.remove("archived_at");
                    object.remove("archive_reason");
                }
                item.clone()
            }
            None => {
                let item = json!({
                    "id": new_id(&items), "kind": kind, "text": text_value, "project": project, "pinned": false, "source": source,
                    "state": "active", "uses": 0, "created_at": stamp, "updated_at": stamp,
                });
                items.push(item.clone());
                item
            }
        };
        self.store(&items)?;
        Ok(item)
    }

    /// 按关键词找（search_memory）：每个关键词对上一次算一分，分高的、新的在前；scope 不是 all 时只看这个项目的和通用的，一条没有再找全部项目
    pub fn search(&self, query: &str, kind: Option<&str>, all_projects: bool, count: usize, project: &str) -> (Vec<Value>, bool) {
        let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
        let items: Vec<Value> =
            self.load().into_iter().filter(active).filter(|item| kind.is_none_or(|kind| text(item, "kind") == kind)).collect();
        let rank = |scoped: bool| -> Vec<Value> {
            let mut scored: Vec<(usize, Value)> = items
                .iter()
                .filter(|item| !scoped || text(item, "project").is_empty() || text(item, "project") == project)
                .filter_map(|item| {
                    let haystack = text(item, "text").to_lowercase();
                    let score = if words.is_empty() { 1 } else { words.iter().filter(|word| haystack.contains(word.as_str())).count() };
                    (score > 0).then(|| (score, item.clone()))
                })
                .collect();
            scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| text(&b.1, "updated_at").cmp(text(&a.1, "updated_at"))));
            scored.into_iter().take(count).map(|(_, item)| item).collect()
        };
        let found = rank(!all_projects);
        if found.is_empty() && !all_projects {
            let wider = rank(false);
            let widened = !wider.is_empty();
            return (wider, widened);
        }
        (found, false)
    }

    /// 用过了：uses + 1、记下时间（模型查到的）
    fn note_used(&self, ids: &[String]) {
        if ids.is_empty() {
            return;
        }
        let _guard = LOCK.lock().unwrap();
        let mut items = self.load();
        let stamp = now();
        for item in items.iter_mut().filter(|item| ids.iter().any(|id| id == text(item, "id"))) {
            item["uses"] = json!(item.get("uses").and_then(Value::as_u64).unwrap_or(0) + 1);
            item["last_used_at"] = json!(stamp);
        }
        let _ = self.store(&items);
    }
}

fn prompt_text(key: &str, pairs: &[(&str, String)]) -> String {
    let args: friday_core::prompts::Args = pairs.iter().map(|(name, value)| (name.to_string(), value.clone())).collect();
    friday_core::prompts::text(key, &args, None)
}

/// 系统提示里的那段：怎么用记忆库，和常驻的那几条（记忆库是空的就只说怎么记）
pub fn prompt() -> String {
    let here = project_key();
    let items = book().load();
    let pinned: Vec<String> = items
        .iter()
        .filter(|item| active(item) && item.get("pinned").and_then(Value::as_bool) == Some(true))
        .filter(|item| text(item, "project").is_empty() || text(item, "project") == here)
        .map(|item| line(item, &here))
        .collect();
    let count = items.iter().filter(|item| active(item)).count();
    prompt_text("edith.memory", &[("count", count.to_string()), ("pinned", pinned.join("\n"))])
}

pub fn specs() -> Vec<Value> {
    let property = |key: &str| json!({ "type": "string", "description": prompt_text(key, &[]) });
    vec![
        json!({ "type": "function", "function": {
            "name": SEARCH,
            "description": prompt_text("edith.tool.search_memory", &[]),
            "parameters": { "type": "object", "properties": {
                "query": property("edith.tool.search_memory.query"),
                "kind": property("edith.tool.search_memory.kind"),
                "scope": property("edith.tool.search_memory.scope"),
                "count": { "type": "integer", "description": prompt_text("edith.tool.search_memory.count", &[]) },
            }, "required": ["query"] },
        }}),
        json!({ "type": "function", "function": {
            "name": SAVE,
            "description": prompt_text("edith.tool.save_memory", &[]),
            "parameters": { "type": "object", "properties": {
                "text": property("edith.tool.save_memory.text"),
                "kind": property("edith.tool.save_memory.kind"),
                "scope": property("edith.tool.save_memory.scope"),
                "replaces": property("edith.tool.save_memory.replaces"),
            }, "required": ["text"] },
        }}),
    ]
}

/// 跑 save_memory / search_memory
pub fn run(name: &str, arguments: &Map<String, Value>) -> Option<Outcome> {
    let argument = |key: &str| arguments.get(key).and_then(Value::as_str).map(str::trim).filter(|value| !value.is_empty());
    let here = project_key();
    Some(match name {
        SAVE => {
            let Some(words) = argument("text") else { return Some(Outcome { ok: false, text: "缺少参数：text".into() }) };
            let project = if argument("scope") == Some("project") { here.clone() } else { String::new() };
            match book().save(words, argument("kind").unwrap_or("preference"), &project, argument("replaces"), "said") {
                Ok(item) => Outcome {
                    ok: true,
                    text: format!("记下了{}：{} [{}]", kind_label(text(&item, "kind")), text(&item, "text"), text(&item, "id")),
                },
                Err(error) => Outcome { ok: false, text: error },
            }
        }
        SEARCH => {
            let count = arguments.get("count").and_then(Value::as_u64).unwrap_or(8).clamp(1, 20) as usize;
            let book = book();
            let (found, widened) =
                book.search(argument("query").unwrap_or(""), argument("kind"), argument("scope") == Some("all"), count, &here);
            book.note_used(&found.iter().map(|item| text(item, "id").to_string()).collect::<Vec<_>>());
            let text = if found.is_empty() {
                "记忆库里没有对得上的（换几个关键词再查）".to_string()
            } else {
                let lines: Vec<String> = found.iter().map(|item| line(item, &here)).collect();
                let head = if widened { "这个项目里没有，下面是别的项目的：\n" } else { "" };
                format!("{head}{}", lines.join("\n"))
            };
            Outcome { ok: true, text }
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_search_and_replace() {
        let home = std::env::temp_dir().join(format!("edith-memory-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let book = Book(home.join("memory.json"));
        let item = book.save("提交一律用 integemjack 身份", "preference", "", None, "said").unwrap();
        let id = text(&item, "id").to_string();
        assert!(id.starts_with('m') && id.len() == 5, "{id}");
        book.save("这个项目用 cargo test 测", "fact", "/w", None, "said").unwrap();
        let (found, widened) = book.search("提交 身份", None, false, 8, "/other");
        assert_eq!((found.len(), widened), (1, false));
        // 别的项目的：这个项目里没有时再找全部
        let (found, widened) = book.search("cargo", None, false, 8, "/other");
        assert_eq!((found.len(), widened), (1, true));
        // 改掉那条，不另记
        book.save("提交用 integemjack 身份，不加署名", "preference", "", Some(&id), "said").unwrap();
        assert_eq!(book.load().len(), 2);
        assert!(book.save("x", "fact", "", Some("m0000"), "said").is_err());
        let _ = std::fs::remove_dir_all(&home);
    }
}

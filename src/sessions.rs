//! 会话：`~/.edith/projects/<工作目录>/sessions/<id>.jsonl`（不放在项目目录里：同一个目录几个大脑各写各的文件），
//! 存 OpenAI 格式的历史（不含 system）和几项元数据。
//!
//! **只追加**（同 Claude Code）：一行一条——`{"type":"meta",…}`（id、cwd、created、owner，后面的盖前面的）、
//! `{"type":"message","message":{…}}`、`{"type":"title","title":…}`、`{"type":"folds","marks":{…}}`（v2.86 较早的部分怎么交给模型，
//! 见 folds.rs；后面的盖前面的，reset 时清掉）、`{"type":"reset"}`（上下文压缩后历史整个换了：
//! 之前的作废，后面跟着新的全部）。两个大脑用 -c 接着同一个会话也不会互相盖掉，写到一半断电只丢最后半行。
//! 老的 `<id>.json`（整个文件重写的那种）读到时换成 `.jsonl`。

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

use serde_json::{Map, Value, json};

use crate::home;

#[derive(Debug, Clone)]
pub struct Session {
    pub id: String,
    pub title: String,
    pub cwd: String,
    pub created: String,
    pub updated: String,
    pub history: Vec<Value>,
    /// 谁的会话：空是主人的（终端、全权的能力）；权限不全的连接（中转的远程设备、收窄过的能力）只能用自己名下的
    pub owner: String,
    /// 这段会话专用的系统提示（接在主智能体的后面；建会话时能力给的，如 Friday 助手的人设）
    pub instructions: String,
    /// v2.86 较早的部分怎么交给模型（folds.rs；历史下标 → 标记）。history 是原样的
    pub folds: Map<String, Value>,
    /// 文件里已经有的（这个进程看到的）：存的时候只追加多出来的
    saved: Saved,
}

#[derive(Debug, Clone, Default)]
struct Saved {
    exists: bool,
    history: Vec<Value>,
    title: String,
    cwd: String,
    owner: String,
    instructions: String,
    folds: Map<String, Value>,
}

/// 分出去的另一支（Friday 改一条重发、切分支时存着的那份历史）记在这个名下：列表里不出现，-c 不接它
pub const ARCHIVED: &str = "#branch";

/// 会话 → 主人（事件按它分给能力，不每次读文件）
static OWNERS: LazyLock<Mutex<HashMap<String, String>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// 这个会话是谁的（空：主人的）
pub fn owner_of(id: &str) -> String {
    if let Some(owner) = OWNERS.lock().unwrap().get(id) {
        return owner.clone();
    }
    Session::load(id).map(|session| session.owner).unwrap_or_default()
}

fn dir() -> PathBuf {
    home::project_dir().join("sessions")
}

fn file(id: &str) -> PathBuf {
    dir().join(format!("{id}.jsonl"))
}

/// 删掉一段会话：存档、读过的文件、会话改动的基线、子 Agent 的运行记录（sessions.remove）
pub fn remove(id: &str) -> bool {
    if !valid(id) {
        return false;
    }
    let removed = std::fs::remove_file(file(id)).is_ok();
    let project = home::project_dir();
    let _ = std::fs::remove_file(project.join("reads").join(format!("{id}.json")));
    let _ = std::fs::remove_dir_all(project.join("agents").join(id));
    crate::changes::remove(id);
    OWNERS.lock().unwrap().remove(id);
    removed
}

/// 会话 id 只认 uuid 的字符（防止拼出别的路径）
fn valid(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

fn now() -> String {
    chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, false)
}

fn modified(path: &Path) -> Option<String> {
    let time = std::fs::metadata(path).ok()?.modified().ok()?;
    Some(chrono::DateTime::<chrono::Local>::from(time).to_rfc3339_opts(chrono::SecondsFormat::Secs, false))
}

impl Session {
    pub fn new(cwd: &str) -> Session {
        let now = now();
        Session {
            id: uuid::Uuid::new_v4().to_string(),
            title: String::new(),
            cwd: cwd.to_string(),
            created: now.clone(),
            updated: now,
            history: Vec::new(),
            owner: String::new(),
            instructions: String::new(),
            folds: Map::new(),
            saved: Saved::default(),
        }
    }

    pub fn load(id: &str) -> Option<Session> {
        if !valid(id) {
            return None;
        }
        let path = file(id);
        let session = match std::fs::read_to_string(&path) {
            Ok(text) => Session::replay(id, &text, modified(&path)),
            Err(_) => Session::convert_legacy(id)?,
        };
        OWNERS.lock().unwrap().insert(session.id.clone(), session.owner.clone());
        Some(session)
    }

    /// 一行行放一遍（坏的行跳过：多半是写到一半断了的最后一行）
    fn replay(id: &str, text: &str, updated: Option<String>) -> Session {
        let mut session = Session::new("");
        session.id = id.to_string();
        session.created.clear();
        for line in text.lines() {
            let Ok(entry) = serde_json::from_str::<Value>(line) else { continue };
            let field = |key: &str| entry.get(key).and_then(Value::as_str).map(str::to_string);
            match entry["type"].as_str() {
                Some("meta") => {
                    if let Some(cwd) = field("cwd") {
                        session.cwd = cwd;
                    }
                    if let Some(created) = field("created").filter(|_| session.created.is_empty()) {
                        session.created = created;
                    }
                    if let Some(owner) = field("owner") {
                        session.owner = owner;
                    }
                    if let Some(instructions) = field("instructions") {
                        session.instructions = instructions;
                    }
                }
                Some("message") => session.history.push(entry["message"].clone()),
                Some("reset") => {
                    session.history.clear();
                    session.folds.clear();
                }
                Some("title") => session.title = field("title").unwrap_or_default(),
                Some("folds") => session.folds = entry["marks"].as_object().cloned().unwrap_or_default(),
                _ => {}
            }
        }
        session.updated = updated.unwrap_or_else(|| session.created.clone());
        session.saved = Saved {
            exists: true,
            history: session.history.clone(),
            title: session.title.clone(),
            cwd: session.cwd.clone(),
            owner: session.owner.clone(),
            instructions: session.instructions.clone(),
            folds: session.folds.clone(),
        };
        session
    }

    /// 老的整个文件那种（`<id>.json`）：读出来、换成只追加的
    fn convert_legacy(id: &str) -> Option<Session> {
        let legacy = dir().join(format!("{id}.json"));
        let value = home::read_json(&legacy)?;
        let mut session = Session::from_legacy(id, &value);
        session.save().ok()?;
        let _ = std::fs::remove_file(&legacy);
        Some(session)
    }

    /// 老格式的一个会话（整个 JSON）
    pub fn from_legacy(id: &str, value: &Value) -> Session {
        let text = |key: &str| value.get(key).and_then(Value::as_str).unwrap_or("").to_string();
        let mut session = Session::new(&text("cwd"));
        session.id = id.to_string();
        session.title = text("title");
        session.created = text("created");
        session.updated = text("updated");
        session.owner = text("owner");
        session.history = value.get("history").and_then(Value::as_array).cloned().unwrap_or_default();
        session
    }

    /// id 可以只给开头几位（命令行里方便）：唯一对得上就是它
    pub fn find(prefix: &str) -> Option<Session> {
        if let Some(session) = Session::load(prefix) {
            return Some(session);
        }
        let matches: Vec<String> = list().into_iter().map(|item| item.id).filter(|id| id.starts_with(prefix)).collect();
        if matches.len() == 1 { Session::load(&matches[0]) } else { None }
    }

    /// 存：只把文件里还没有的追加上（历史整个换了的——上下文压缩——记一个 reset 再写全部）
    pub fn save(&mut self) -> Result<(), String> {
        self.updated = now();
        if self.title.is_empty() {
            self.title = first_user_text(&self.history).chars().take(40).collect();
        }
        let mut lines: Vec<Value> = Vec::new();
        if !self.saved.exists
            || self.saved.cwd != self.cwd
            || self.saved.owner != self.owner
            || self.saved.instructions != self.instructions
        {
            let mut meta = json!({ "type": "meta", "id": self.id, "cwd": self.cwd, "created": self.created });
            if !self.owner.is_empty() || self.saved.exists {
                meta["owner"] = json!(self.owner);
            }
            if !self.instructions.is_empty() || self.saved.instructions != self.instructions {
                meta["instructions"] = json!(self.instructions);
            }
            lines.push(meta);
        }
        if self.saved.title != self.title {
            lines.push(json!({ "type": "title", "title": self.title }));
        }
        let known = self.saved.history.len();
        let appended = self.history.len() >= known && self.history[..known] == self.saved.history[..];
        let fresh: &[Value] = if appended {
            &self.history[known..]
        } else {
            // 历史整个换了（截短、换成另一支）：标记按下标记的，作废（下一轮重新整理）
            lines.push(json!({ "type": "reset" }));
            self.folds.clear();
            &self.history
        };
        lines.extend(fresh.iter().map(|message| json!({ "type": "message", "message": message })));
        if self.folds != self.saved.folds && !(self.folds.is_empty() && !appended) {
            lines.push(json!({ "type": "folds", "marks": self.folds }));
        }
        if !lines.is_empty() {
            let mut text: String = lines.iter().map(|line| format!("{line}\n")).collect();
            // 上次写到一半断了（最后没有换行）：先换行，别接在那半行后面
            if !ends_with_newline(&file(&self.id)) {
                text.insert(0, '\n');
            }
            std::fs::create_dir_all(dir()).map_err(|error| format!("建不了 {}：{error}", dir().display()))?;
            let path = file(&self.id);
            // 一次写完一整段（O_APPEND：几个进程一起追加也不会交错在一行里）
            let mut handle = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .map_err(|error| format!("写不了 {}：{error}", path.display()))?;
            handle.write_all(text.as_bytes()).map_err(|error| format!("写不了 {}：{error}", path.display()))?;
        }
        self.saved = Saved {
            exists: true,
            history: self.history.clone(),
            title: self.title.clone(),
            cwd: self.cwd.clone(),
            owner: self.owner.clone(),
            instructions: self.instructions.clone(),
            folds: self.folds.clone(),
        };
        OWNERS.lock().unwrap().insert(self.id.clone(), self.owner.clone());
        Ok(())
    }

    pub fn summary(&self) -> Value {
        let mut value = json!({ "id": self.id, "title": self.title, "cwd": self.cwd, "created": self.created, "updated": self.updated });
        if !self.owner.is_empty() {
            value["owner"] = json!(self.owner);
        }
        value
    }

    pub fn to_json(&self) -> Value {
        let mut value = self.summary();
        value["history"] = Value::Array(self.history.clone());
        value
    }
}

/// 文件是空的、不存在，或者最后一个字节是换行
fn ends_with_newline(path: &Path) -> bool {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut handle) = std::fs::File::open(path) else { return true };
    if handle.seek(SeekFrom::End(-1)).is_err() {
        return true;
    }
    let mut last = [0u8; 1];
    handle.read_exact(&mut last).is_err() || last[0] == b'\n'
}

fn first_user_text(history: &[Value]) -> String {
    let text = history
        .iter()
        .find(|entry| entry.get("role").and_then(Value::as_str) == Some("user"))
        .and_then(|entry| entry.get("content").and_then(Value::as_str))
        .unwrap_or("");
    // 能力交来、记下、补充的：去掉给模型标来源的那一行（edith.input / note / interjection）
    let text = match text.split_once('\n') {
        Some((first, rest))
            if first.starts_with('（')
                && first.ends_with('）')
                && ["传来", "记下了下面这段", "补充了下面这段"].iter().any(|mark| first.contains(mark)) =>
        {
            rest
        }
        _ => text,
    };
    text.replace('\n', " ")
}

/// 所有会话（新的在前），只有元数据
pub fn list() -> Vec<Session> {
    let mut ids: Vec<String> = std::fs::read_dir(dir())
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            name.strip_suffix(".jsonl").or_else(|| name.strip_suffix(".json")).map(str::to_string)
        })
        .collect();
    ids.sort();
    ids.dedup();
    let mut sessions: Vec<Session> = ids
        .iter()
        .filter_map(|id| {
            let mut session = Session::load(id)?;
            session.history.clear();
            Some(session)
        })
        .collect();
    sessions.sort_by(|a, b| b.updated.cmp(&a.updated));
    sessions
}

/// 这个工作目录最近的一个会话（-c / --continue 接它；只看主人的）
pub fn latest() -> Option<Session> {
    list().iter().find(|session| session.owner.is_empty()).and_then(|session| Session::load(&session.id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appends_and_resets() {
        let home = std::env::temp_dir().join(format!("edith-sessions-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        unsafe { std::env::set_var("EDITH_HOME", &home) };
        home::set_workspace(home.join("work"));
        let mut session = Session::new("/w");
        session.history.push(json!({ "role": "user", "content": "一" }));
        session.save().unwrap();
        // 另一个进程也接着它（各自从同一个样子开始）
        let mut other = Session::load(&session.id).unwrap();
        session.history.push(json!({ "role": "assistant", "content": "A" }));
        session.save().unwrap();
        other.history.push(json!({ "role": "assistant", "content": "B" }));
        other.save().unwrap();
        let both = Session::load(&session.id).unwrap();
        assert_eq!(both.history.len(), 3, "两边写的都留着：{:?}", both.history);
        assert_eq!(both.title, "一");
        // 标来源的那一行不算进标题
        let noted =
            vec![json!({ "role": "user", "content": "（能力「friday」记下了下面这段，不用回复，之后用得着时参考）\n/loop 10m 看 CI" })];
        assert_eq!(first_user_text(&noted), "/loop 10m 看 CI");
        // v2.86 较早的部分怎么交给模型的标记：存进 folds 行，接着追加不丢；历史整个换了就作废
        let mut marked = Session::load(&session.id).unwrap();
        marked.folds.insert("1".into(), json!({ "fold": true }));
        marked.save().unwrap();
        marked.history.push(json!({ "role": "user", "content": "再说一句" }));
        marked.save().unwrap();
        let reloaded = Session::load(&session.id).unwrap();
        assert_eq!(reloaded.folds.get("1"), Some(&json!({ "fold": true })));
        assert_eq!(reloaded.history.len(), 4);
        let mut rewound = reloaded;
        rewound.history.truncate(3);
        rewound.save().unwrap();
        assert!(Session::load(&session.id).unwrap().folds.is_empty(), "截短了：标记作废");
        // 压缩：历史整个换了
        let mut compacted = Session::load(&session.id).unwrap();
        compacted.history = vec![json!({ "role": "user", "content": "摘要" })];
        compacted.save().unwrap();
        assert_eq!(Session::load(&session.id).unwrap().history, compacted.history);
        // 写到一半断了的最后一行不影响
        let mut handle = std::fs::OpenOptions::new().append(true).open(file(&session.id)).unwrap();
        handle.write_all(b"{\"type\":\"message\",\"mess").unwrap();
        let mut after = Session::load(&session.id).unwrap();
        assert_eq!(after.history.len(), 1);
        after.history.push(json!({ "role": "assistant", "content": "接着" }));
        after.save().unwrap();
        assert_eq!(Session::load(&session.id).unwrap().history.len(), 2, "断了的半行后面接着写坏了");
        // 老格式：读到就换
        let legacy = dir().join("0000aaaa-0000-0000-0000-000000000000.json");
        std::fs::write(&legacy, json!({ "title": "老", "cwd": "/old", "history": [{ "role": "user", "content": "x" }] }).to_string())
            .unwrap();
        let old = Session::load("0000aaaa-0000-0000-0000-000000000000").unwrap();
        assert_eq!((old.title.as_str(), old.history.len()), ("老", 1));
        assert!(!legacy.exists() && file(&old.id).exists());
        let _ = std::fs::remove_dir_all(&home);
    }
}

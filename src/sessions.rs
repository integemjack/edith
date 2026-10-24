//! 会话：`~/.edith/projects/<工作目录>/sessions/<id>.json`（不放在项目目录里：同一个目录几个大脑各写各的文件），存 OpenAI 格式的历史（不含 system）和几项元数据

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex};

use serde_json::{Value, json};

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
}

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

/// 会话 id 只认 uuid 的字符（防止拼出别的路径）
fn valid(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

fn now() -> String {
    chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, false)
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
        }
    }

    pub fn load(id: &str) -> Option<Session> {
        if !valid(id) {
            return None;
        }
        let value = home::read_json(&dir().join(format!("{id}.json")))?;
        let text = |key: &str| value.get(key).and_then(Value::as_str).unwrap_or("").to_string();
        Some(Session {
            id: id.to_string(),
            title: text("title"),
            cwd: text("cwd"),
            created: text("created"),
            updated: text("updated"),
            history: value.get("history").and_then(Value::as_array).cloned().unwrap_or_default(),
            owner: text("owner"),
        })
        .inspect(|session| {
            OWNERS.lock().unwrap().insert(session.id.clone(), session.owner.clone());
        })
    }

    /// id 可以只给开头几位（命令行里方便）：唯一对得上就是它
    pub fn find(prefix: &str) -> Option<Session> {
        if let Some(session) = Session::load(prefix) {
            return Some(session);
        }
        let matches: Vec<String> = list().into_iter().map(|item| item.id).filter(|id| id.starts_with(prefix)).collect();
        if matches.len() == 1 { Session::load(&matches[0]) } else { None }
    }

    pub fn save(&mut self) -> Result<(), String> {
        self.updated = now();
        if self.title.is_empty() {
            self.title = first_user_text(&self.history).chars().take(40).collect();
        }
        OWNERS.lock().unwrap().insert(self.id.clone(), self.owner.clone());
        home::write_json(&dir().join(format!("{}.json", self.id)), &self.to_json())
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

fn first_user_text(history: &[Value]) -> String {
    history
        .iter()
        .find(|entry| entry.get("role").and_then(Value::as_str) == Some("user"))
        .and_then(|entry| entry.get("content").and_then(Value::as_str))
        .unwrap_or("")
        .replace('\n', " ")
}

/// 所有会话（新的在前），只有元数据
pub fn list() -> Vec<Session> {
    let mut sessions: Vec<Session> = std::fs::read_dir(dir())
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let id = name.strip_suffix(".json")?;
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
